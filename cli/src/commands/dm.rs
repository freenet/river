//! In-room direct messages (#243 Phase 3).
//!
//! `dm send` / `dm list` / `dm purge` produce the same wire bytes as the
//! River UI does for the same operation: encryption goes through
//! `river_core::room_state::direct_messages::compose_direct_message`, which
//! itself calls `river_core::ecies::seal_dm_for_recipient`. The contract
//! WASM doesn't care which client posted the state — verification rests on
//! the sender + recipient signatures, both produced from helpers that live
//! in the `river-core` crate.

use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use clap::Subcommand;
use ed25519_dalek::VerifyingKey;
use river_core::chat_delegate::OutboundDmEntry;
use river_core::room_state::direct_messages::{
    advance_recipient_purges, compose_direct_message, open_direct_message, pair_message_count,
    PurgeToken, MAX_DM_MESSAGES_PER_PAIR,
};
use river_core::room_state::dm_body::{decode_body, DirectMessageBody};
use river_core::room_state::member::MemberId;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Subcommand)]
pub enum DmCommands {
    /// Send a direct message to a co-member of a room.
    ///
    /// `recipient` accepts a short MemberId prefix (e.g. the first 8 chars
    /// shown by `riverctl member list`) or a full MemberId.
    Send {
        /// Room ID (base58-encoded room owner verifying key)
        room_id: String,
        /// Recipient member ID (short prefix accepted)
        recipient: String,
        /// Message body (plaintext, encrypted on send)
        message: String,
    },
    /// List direct messages addressed to or sent by your local member in a
    /// room. Decrypted on display.
    List {
        /// Room ID
        room_id: String,
        /// Show only DMs exchanged with this counterparty (short prefix accepted)
        #[arg(long)]
        with: Option<String>,
        /// Maximum messages to show per counterparty
        #[arg(short, long, default_value = "50")]
        limit: usize,
        /// Show only messages from the last N minutes
        #[arg(long)]
        since_minutes: Option<u64>,
    },
    /// Purge a direct message addressed to you. Builds a recipient purge
    /// envelope listing the message's token; once accepted by the room
    /// contract, any peer holding the message drops it on merge.
    ///
    /// The token to pass is printed under each inbound DM by `dm list`
    /// (look for `purge token: ...`). The integer-index form was dropped
    /// because `dm list`'s indices change when you pass `--with` or
    /// `--limit`; using the hex token is unambiguous regardless of the
    /// filter you used to find the message.
    Purge {
        /// Room ID
        room_id: String,
        /// 32-character hex PurgeToken (16 bytes) — copy from the
        /// `purge token: ...` line shown beneath each inbound DM in
        /// `dm list` output.
        token: String,
    },
}

pub async fn execute(command: DmCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        DmCommands::Send {
            room_id,
            recipient,
            message,
        } => execute_send(api, format, &room_id, &recipient, &message).await,
        DmCommands::List {
            room_id,
            with,
            limit,
            since_minutes,
        } => execute_list(api, format, &room_id, with.as_deref(), limit, since_minutes).await,
        DmCommands::Purge { room_id, token } => execute_purge(api, format, &room_id, &token).await,
    }
}

async fn execute_send(
    api: ApiClient,
    format: OutputFormat,
    room_id: &str,
    recipient: &str,
    message: &str,
) -> Result<()> {
    let room_owner_key = parse_room_id(room_id)?;

    // Local signing key + cached state for resolving the recipient.
    let (signing_key, _, _) = api
        .storage()
        .get_room(&room_owner_key)?
        .ok_or_else(|| anyhow!("Room not found. You must be a member of the room to send DMs."))?;

    let room_state = api.get_room(&room_owner_key, false).await?;

    let recipient_vk = resolve_recipient_vk(&room_state, &room_owner_key, recipient)?;
    let recipient_id = MemberId::from(&recipient_vk);
    let self_id = MemberId::from(&signing_key.verifying_key());

    if self_id == recipient_id {
        return Err(anyhow!("Cannot send a DM to yourself."));
    }

    // The recipient must already be in the room — the sender does not hold
    // the recipient's owner-signed `AuthorizedMember`, so a pruned recipient
    // cannot be bundled into the DM delta (that side is follow-up
    // territory; see issue #110 for the related "pruned member" gap).
    // Sender membership is handled by the rejoin bundle below — Bug #1.
    let owner_id = MemberId::from(&room_owner_key);
    let is_recipient_member = recipient_id == owner_id
        || room_state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == recipient_id);
    if !is_recipient_member {
        return Err(anyhow!("Recipient is not currently a member of the room."));
    }

    // Bug #1 (Ivvor, Matrix 2026-05-16): an invited-but-inactive sender
    // can be pruned from `members.members` by `post_apply_cleanup`, after
    // which the contract's `DirectMessagesV1::apply_delta` silent-drops
    // any DM whose sender isn't currently in members. Bundle the same
    // rejoin pieces the regular `send_message_with_key` path produces so
    // the DM lands atomically with the member-rejoin. `MembersV1`
    // precedes `DirectMessagesV1` in the macro's field order so the
    // sender is back in members by the time the DM sub-state apply runs.
    // Contract-level pin: `pruned_sender_can_dm_when_bundling_rejoin_delta`
    // in `common/tests/direct_messages_test.rs`.
    let (rejoin_members, rejoin_member_info) =
        api.build_rejoin_delta(&room_state, &room_owner_key, &signing_key);

    // Codex P2 (PR #269 review): if the sender is pruned AND
    // `build_rejoin_delta` returned no credentials (e.g. older stored
    // rooms missing `self_authorized_member`), the contract will
    // silent-drop the DM but the CLI's local pre-flight `apply_delta`
    // returns Ok — meaning we'd `send_state_delta` and print success
    // even though no DM lands. Surface the failure here with a clear
    // diagnostic instead.
    let is_self_member = self_id == owner_id
        || room_state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == self_id);
    if !is_self_member && rejoin_members.is_none() {
        return Err(anyhow!(
            "Your member entry is not in the room and no stored rejoin credentials \
             are available. Re-accept your invitation with `riverctl invite accept` \
             before sending a DM."
        ));
    }

    // Per-pair cap: contract `apply_delta` silently drops overflow so we
    // must surface the cap as a hard error here, otherwise the CLI would
    // print "DM sent" with nothing actually delivered.
    let existing = pair_message_count(&room_state.direct_messages, self_id, recipient_id);
    if existing >= MAX_DM_MESSAGES_PER_PAIR {
        return Err(anyhow!(
            "Per-pair DM cap reached ({}/{}). Ask the recipient to purge older DMs from this thread before sending more.",
            existing,
            MAX_DM_MESSAGES_PER_PAIR
        ));
    }

    let now = unix_now()?;
    // Text-variant DMs keep the legacy wire shape (raw UTF-8 bytes,
    // no magic byte) for backwards compatibility: pre-this-PR clients
    // in the wild decode raw UTF-8 directly via
    // `String::from_utf8_lossy`. A new-format `Text` body would
    // render a stray `\u{0080}` glyph plus garbled CBOR on those
    // older clients, with no way for them to know better. New clients
    // round-trip cleanly because `decode_body`'s legacy fallback path
    // hands raw UTF-8 back as `DirectMessageBody::Text`.
    //
    // Only NEW variants (currently `Invite`) need to opt into the
    // magic-byte + CBOR wire shape — old clients can't render those
    // anyway, so the rendering regression is moot.
    let auth = compose_direct_message(
        &signing_key,
        &recipient_vk,
        &room_owner_key,
        now,
        now,
        message.as_bytes(),
    )
    .map_err(|e| anyhow!("Failed to compose DM: {}", e))?;

    let delta = ChatRoomStateV1Delta {
        members: rejoin_members,
        member_info: rejoin_member_info,
        direct_messages: Some(
            river_core::room_state::direct_messages::DirectMessagesDelta {
                new_messages: vec![auth.clone()],
                advanced_purges: vec![],
            },
        ),
        ..Default::default()
    };

    // Local pre-flight: walk the same apply_delta the contract will run.
    let params = ChatRoomParametersV1 {
        owner: room_owner_key,
    };
    let mut local = room_state.clone();
    {
        use freenet_scaffold::ComposableState;
        local
            .apply_delta(&room_state, &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Local pre-flight apply_delta failed: {:?}", e))?;
    }

    // Codex P2 defence-in-depth (PR #269 review): `DirectMessagesV1::apply_delta`
    // silent-drops DMs whose sender isn't a current member, so the pre-flight
    // above can return Ok even though our DM was dropped. Verify the message
    // we tried to send is actually present in the post-merge state before
    // we report success to the user.
    let landed = local
        .direct_messages
        .messages
        .iter()
        .any(|m| m.sender_signature == auth.sender_signature);
    if !landed {
        return Err(anyhow!(
            "Local pre-flight: the DM was silently dropped by the contract \
             (likely because the sender or recipient is not a current member). \
             Refusing to claim a successful send."
        ));
    }

    api.send_state_delta(&room_owner_key, &delta).await?;

    let token = auth.purge_token();
    let token_hex = hex_token(&token);

    // Persist plaintext in the local outbound-DM cache so a future
    // `dm list` can render the sender's own bubble as plaintext
    // instead of `<sent: ciphertext only>`. See issue freenet/river#256.
    // Failure is logged but not fatal — the DM has already been sent.
    if let Err(e) = persist_outbound_plaintext(
        &api,
        room_owner_key,
        self_id,
        recipient_id,
        token,
        auth.message.timestamp,
        message.to_string(),
    ) {
        tracing::warn!("Failed to persist outbound DM plaintext locally: {}", e);
    }

    match format {
        OutputFormat::Human => {
            println!(
                "DM sent to {} (purge token: {})",
                short_member_id(&recipient_id),
                token_hex
            );
        }
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "status": "success",
                    "recipient": recipient_id.to_string(),
                    "purge_token": token_hex,
                }))?
            );
        }
    }
    Ok(())
}

async fn execute_list(
    api: ApiClient,
    format: OutputFormat,
    room_id: &str,
    with: Option<&str>,
    limit: usize,
    since_minutes: Option<u64>,
) -> Result<()> {
    let room_owner_key = parse_room_id(room_id)?;
    let (signing_key, _, _) = api
        .storage()
        .get_room(&room_owner_key)?
        .ok_or_else(|| anyhow!("Room not found. You must be a member of the room to read DMs."))?;
    let self_vk = signing_key.verifying_key();
    let self_id = MemberId::from(&self_vk);

    let room_state = api.get_room(&room_owner_key, false).await?;

    let with_filter = with
        .map(|s| resolve_recipient_id(&room_state, &room_owner_key, s))
        .transpose()?;

    let cutoff = since_minutes.map(|m| {
        unix_now()
            .map(|now| now.saturating_sub(m.saturating_mul(60)))
            .unwrap_or(0)
    });

    // Build a nickname lookup so output is human-readable.
    let nicknames: HashMap<MemberId, String> = room_state
        .member_info
        .member_info
        .iter()
        .map(|info| {
            (
                info.member_info.member_id,
                info.member_info.preferred_nickname.to_string_lossy(),
            )
        })
        .collect();

    // Load the local outbound-DM plaintext cache so we can render the
    // sender's own bubbles as plaintext instead of `<sent: ciphertext
    // only>`. See issue freenet/river#256. Missing entries (e.g. DMs
    // sent before this cache shipped, or from another device) still
    // fall back to the legacy placeholder.
    let outbound_lookup: HashMap<(MemberId, PurgeToken), String> = api
        .storage()
        .load_outbound_dms()
        .map(|store| {
            store
                .entries
                .into_iter()
                .filter(|e| e.room_owner_vk == room_owner_key.to_bytes())
                .map(|e| ((e.recipient, e.purge_token), e.plaintext))
                .collect()
        })
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to load outbound DM cache: {}", e);
            HashMap::new()
        });

    // Walk every DM we are party to; decrypt on display.
    let mut decrypted: Vec<DecryptedDm> = Vec::new();
    for msg in &room_state.direct_messages.messages {
        let is_self_sender = msg.message.sender == self_id;
        let is_self_recipient = msg.message.recipient == self_id;
        if !is_self_sender && !is_self_recipient {
            continue;
        }

        let counterparty = if is_self_sender {
            msg.message.recipient
        } else {
            msg.message.sender
        };

        if let Some(filter) = with_filter {
            if counterparty != filter {
                continue;
            }
        }

        if let Some(cut) = cutoff {
            if msg.message.timestamp < cut {
                continue;
            }
        }

        // Render the body as a String. For inbound DMs we decrypt the
        // ECIES envelope, then decode the structured `DirectMessageBody`
        // (which falls back to legacy raw-UTF-8 → `Text` for pre-#XXX
        // peers). Outbound DMs go through the local plaintext cache as
        // before — the cache stores the user-facing string regardless of
        // wire shape.
        let body_str = if is_self_recipient {
            match open_direct_message(&signing_key, msg) {
                Ok(bytes) => match decode_body(&bytes) {
                    Ok(body) => format_dm_body_for_cli(&body, &nicknames),
                    Err(_) => "<unable to decode body>".to_string(),
                },
                Err(_) => "<unable to decrypt>".to_string(),
            }
        } else {
            match outbound_lookup.get(&(msg.message.recipient, msg.purge_token())) {
                Some(plaintext) => plaintext.clone(),
                None => "<sent: ciphertext only>".to_string(),
            }
        };

        decrypted.push(DecryptedDm {
            counterparty,
            outgoing: is_self_sender,
            timestamp: msg.message.timestamp,
            body: body_str,
            token: msg.purge_token(),
        });
    }

    // Best-effort prune: drop cached entries whose ciphertext is gone
    // from this room's state (recipient purged or contract cap
    // evicted). We only touch entries for THIS room — other rooms'
    // entries are left alone since their state isn't in scope here.
    if let Err(e) = prune_outbound_cache_for_room(&api, &room_owner_key, &room_state) {
        tracing::warn!("Failed to prune outbound DM cache: {}", e);
    }

    // Sort by counterparty, then chronological.
    decrypted.sort_by(|a, b| {
        a.counterparty
            .cmp(&b.counterparty)
            .then(a.timestamp.cmp(&b.timestamp))
    });

    // Group + cap per counterparty.
    let mut by_peer: HashMap<MemberId, Vec<DecryptedDm>> = HashMap::new();
    for dm in decrypted {
        by_peer.entry(dm.counterparty).or_default().push(dm);
    }
    for thread in by_peer.values_mut() {
        if thread.len() > limit {
            let take_from = thread.len() - limit;
            *thread = thread.split_off(take_from);
        }
    }

    match format {
        OutputFormat::Human => {
            if by_peer.is_empty() {
                println!("No direct messages found.");
                return Ok(());
            }
            let mut peers: Vec<_> = by_peer.keys().copied().collect();
            peers.sort();
            for peer in peers {
                let nickname = nicknames
                    .get(&peer)
                    .cloned()
                    .unwrap_or_else(|| short_member_id(&peer));
                println!("--- DM thread with {} ({}) ---", nickname, peer);
                let mut idx = 1usize;
                for dm in by_peer.get(&peer).unwrap() {
                    let local_time = format_unix_local(dm.timestamp);
                    let direction = if dm.outgoing { "->" } else { "<-" };
                    println!("[{:>3}] {} [{}] {}", idx, direction, local_time, dm.body);
                    if !dm.outgoing {
                        println!("        purge token: {}", hex_token(&dm.token));
                    }
                    idx += 1;
                }
                println!();
            }
        }
        OutputFormat::Json => {
            let threads: Vec<_> = by_peer
                .into_iter()
                .map(|(peer, dms)| {
                    json!({
                        "counterparty": peer.to_string(),
                        "counterparty_nickname": nicknames.get(&peer).cloned(),
                        "messages": dms
                            .into_iter()
                            .map(|dm| {
                                let datetime: DateTime<Utc> =
                                    SystemTime::UNIX_EPOCH
                                        .checked_add(std::time::Duration::from_secs(dm.timestamp))
                                        .map(DateTime::<Utc>::from)
                                        .unwrap_or_else(Utc::now);
                                json!({
                                    "direction": if dm.outgoing { "outgoing" } else { "incoming" },
                                    "timestamp": datetime.to_rfc3339(),
                                    "timestamp_unix": dm.timestamp,
                                    "body": dm.body,
                                    "purge_token": hex_token(&dm.token),
                                })
                            })
                            .collect::<Vec<_>>(),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&threads)?);
        }
    }
    Ok(())
}

async fn execute_purge(
    api: ApiClient,
    format: OutputFormat,
    room_id: &str,
    token: &str,
) -> Result<()> {
    let room_owner_key = parse_room_id(room_id)?;
    let (signing_key, _, _) = api
        .storage()
        .get_room(&room_owner_key)?
        .ok_or_else(|| anyhow!("Room not found. You must be a member of the room to purge DMs."))?;
    let self_id: MemberId = signing_key.verifying_key().into();

    let room_state = api.get_room(&room_owner_key, false).await?;

    let resolved_token = parse_hex_token(token).ok_or_else(|| {
        anyhow!(
            "Expected a 32-character hex purge token, got: {} (run `dm list` and copy the value after `purge token:`).",
            token
        )
    })?;

    // Sanity: confirm the token names a DM we actually received. The
    // contract `apply_delta` is silent-drop on no-op envelopes; without
    // this guard a typo in the token would print "Purge envelope sent"
    // and tombstone nothing.
    let matched = room_state
        .direct_messages
        .messages
        .iter()
        .any(|m| m.message.recipient == self_id && m.purge_token() == resolved_token);
    if !matched {
        return Err(anyhow!(
            "No inbound DM in this room matches that purge token. Run `dm list` to confirm."
        ));
    }

    // Skip if the recipient already has this token in their current envelope.
    let already_purged = room_state
        .direct_messages
        .purges
        .iter()
        .find(|p| p.recipient_id == self_id)
        .map(|p| p.state.purged.contains(&resolved_token))
        .unwrap_or(false);
    if already_purged {
        return Err(anyhow!(
            "That DM is already in your purge envelope; nothing to do."
        ));
    }

    // Compose the new envelope on top of any existing one for this recipient.
    let previous = room_state
        .direct_messages
        .purges
        .iter()
        .find(|p| p.recipient_id == self_id)
        .cloned();
    let envelope = advance_recipient_purges(
        &signing_key,
        &room_owner_key,
        previous.as_ref(),
        [resolved_token],
    )
    .map_err(|e| anyhow!("Failed to build purge envelope: {}", e))?;

    let delta = ChatRoomStateV1Delta {
        direct_messages: Some(
            river_core::room_state::direct_messages::DirectMessagesDelta {
                new_messages: vec![],
                advanced_purges: vec![envelope.clone()],
            },
        ),
        ..Default::default()
    };

    api.send_state_delta(&room_owner_key, &delta).await?;

    match format {
        OutputFormat::Human => println!(
            "Purge envelope sent (version {}, {} tombstones total).",
            envelope.state.version,
            envelope.state.purged.len()
        ),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "success",
                "version": envelope.state.version,
                "tombstone_count": envelope.state.purged.len(),
                "purge_token": hex_token(&resolved_token),
            }))?
        ),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct DecryptedDm {
    counterparty: MemberId,
    outgoing: bool,
    timestamp: u64,
    body: String,
    token: PurgeToken,
}

fn parse_room_id(room_id: &str) -> Result<VerifyingKey> {
    let bytes = bs58::decode(room_id)
        .into_vec()
        .map_err(|e| anyhow!("Invalid room ID: {}", e))?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "Invalid room ID: expected 32 bytes, got {}",
            bytes.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&arr).map_err(|e| anyhow!("Invalid room ID: {}", e))
}

fn resolve_recipient_vk(
    state: &river_core::ChatRoomStateV1,
    room_owner_key: &VerifyingKey,
    needle: &str,
) -> Result<VerifyingKey> {
    // Collect every candidate match and require exactly one. Picking the
    // first match silently — the original behaviour — could route a
    // private DM to the wrong recipient if the prefix collides with
    // multiple members or with both a member and the room owner. Found
    // by Codex review of #244.
    //
    // Dedupe by `VerifyingKey`, not by id-prefix-match — the room owner
    // can ALSO be enrolled as an explicit `AuthorizedMember` in some
    // contracts, which would otherwise produce a spurious "ambiguous"
    // error even though both matches resolve to the same destination
    // key (Skeptical-review #4 on pass 3).
    let owner_id = MemberId::from(room_owner_key);
    let mut matches: Vec<(MemberId, VerifyingKey)> = state
        .members
        .members
        .iter()
        .filter(|m| m.member.id().to_string().starts_with(needle))
        .map(|m| (m.member.id(), m.member.member_vk))
        .collect();
    if owner_id.to_string().starts_with(needle) {
        matches.push((owner_id, *room_owner_key));
    }
    // Dedupe by destination key. Sort first so dedup is contiguous.
    matches.sort_by_key(|(_, vk)| vk.to_bytes());
    matches.dedup_by_key(|(_, vk)| *vk);
    match matches.len() {
        0 => Err(anyhow!(
            "No member matched '{}' in this room. Pass a longer prefix (try `riverctl member list`).",
            needle
        )),
        1 => Ok(matches.remove(0).1),
        _ => {
            let listing: Vec<String> = matches.iter().map(|(id, _)| id.to_string()).collect();
            Err(anyhow!(
                "Recipient prefix '{}' is ambiguous — matches {} distinct members: {}. \
                 Pass a longer prefix.",
                needle,
                matches.len(),
                listing.join(", ")
            ))
        }
    }
}

fn resolve_recipient_id(
    state: &river_core::ChatRoomStateV1,
    room_owner_key: &VerifyingKey,
    needle: &str,
) -> Result<MemberId> {
    resolve_recipient_vk(state, room_owner_key, needle).map(|vk| MemberId::from(&vk))
}

fn parse_hex_token(s: &str) -> Option<PurgeToken> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(PurgeToken(out))
}

fn hex_token(t: &PurgeToken) -> String {
    let mut s = String::with_capacity(32);
    for b in &t.0 {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn short_member_id(id: &MemberId) -> String {
    let s = id.to_string();
    s.chars().take(8).collect()
}

/// Append a new outbound DM entry to the local cache, enforcing the
/// per-pair cap to match the contract's `MAX_DM_MESSAGES_PER_PAIR`
/// eviction policy. Issue freenet/river#256.
fn persist_outbound_plaintext(
    api: &ApiClient,
    room_owner_vk: VerifyingKey,
    sender: MemberId,
    recipient: MemberId,
    purge_token: PurgeToken,
    timestamp: u64,
    plaintext: String,
) -> Result<()> {
    let mut store = api.storage().load_outbound_dms()?;
    let room_bytes = room_owner_vk.to_bytes();
    store.entries.push(OutboundDmEntry {
        room_owner_vk: room_bytes,
        sender,
        recipient,
        purge_token,
        timestamp,
        plaintext,
    });

    apply_per_pair_cap(&mut store);
    api.storage().save_outbound_dms(&store)?;
    Ok(())
}

/// Pure helper: drop the oldest entries from each `(room, sender,
/// recipient)` tuple until the per-pair count is at most
/// `MAX_DM_MESSAGES_PER_PAIR`. Mirrors the contract's eviction policy
/// so the local cache size stays bounded.
fn apply_per_pair_cap(store: &mut river_core::chat_delegate::OutboundDmStore) {
    let mut by_pair: HashMap<([u8; 32], MemberId, MemberId), Vec<usize>> = HashMap::new();
    for (i, entry) in store.entries.iter().enumerate() {
        by_pair
            .entry((entry.room_owner_vk, entry.sender, entry.recipient))
            .or_default()
            .push(i);
    }
    let mut to_drop: HashSet<usize> = HashSet::new();
    for indices in by_pair.into_values() {
        if indices.len() <= MAX_DM_MESSAGES_PER_PAIR {
            continue;
        }
        let mut sorted = indices;
        sorted.sort_by_key(|i| store.entries[*i].timestamp);
        let drop_count = sorted.len() - MAX_DM_MESSAGES_PER_PAIR;
        for i in sorted.into_iter().take(drop_count) {
            to_drop.insert(i);
        }
    }
    if !to_drop.is_empty() {
        let mut kept = Vec::with_capacity(store.entries.len() - to_drop.len());
        for (i, entry) in store.entries.drain(..).enumerate() {
            if !to_drop.contains(&i) {
                kept.push(entry);
            }
        }
        store.entries = kept;
    }
}

/// Drop cached outbound-DM entries for `room_owner_vk` whose token
/// appears in some recipient's purge envelope in the supplied
/// `room_state`. Only entries for this room are considered — other
/// rooms' state isn't in scope so their entries are left alone.
///
/// **Why we ONLY act on purge envelopes (not on "ciphertext missing
/// from `messages`")** — see the UI-side
/// `prune_outbound_dms_for_purges` docs for the cold-start
/// race rationale; the CLI shares the same risk because `dm list`
/// could be invoked against a freshly-republished node where the
/// contract state hasn't fully resynced. Issue freenet/river#256.
fn prune_outbound_cache_for_room(
    api: &ApiClient,
    room_owner_vk: &VerifyingKey,
    room_state: &river_core::ChatRoomStateV1,
) -> Result<()> {
    let mut store = api.storage().load_outbound_dms()?;
    let room_bytes = room_owner_vk.to_bytes();

    let purged: HashSet<(MemberId, PurgeToken)> = room_state
        .direct_messages
        .purges
        .iter()
        .flat_map(|envelope| {
            envelope
                .state
                .purged
                .iter()
                .map(move |token| (envelope.recipient_id, *token))
        })
        .collect();
    if purged.is_empty() {
        return Ok(());
    }

    let before = store.entries.len();
    store.entries.retain(|e| {
        e.room_owner_vk != room_bytes || !purged.contains(&(e.recipient, e.purge_token))
    });
    if store.entries.len() != before {
        api.storage().save_outbound_dms(&store)?;
    }
    Ok(())
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("clock before UNIX_EPOCH: {}", e))?
        .as_secs())
}

/// Render a decoded [`DirectMessageBody`] for `dm list` output.
///
/// * `Text` → the text itself.
/// * `Invite` → a compact one-line summary so an inbound invite DM is
///   labelled distinctly from prose. The on-wire `invitation_payload`
///   isn't decoded here — we just surface the room owner key (8-char
///   prefix) so the recipient can `riverctl room accept` against the
///   right target if they want to. Personal message (when present) is
///   appended.
fn format_dm_body_for_cli(
    body: &DirectMessageBody,
    _nicknames: &HashMap<MemberId, String>,
) -> String {
    match body {
        DirectMessageBody::Text { text } => text.clone(),
        DirectMessageBody::Invite {
            room_owner_vk,
            personal_message,
            ..
        } => {
            let room_short: String = bs58::encode(room_owner_vk.as_bytes())
                .into_string()
                .chars()
                .take(8)
                .collect();
            match personal_message {
                Some(msg) if !msg.trim().is_empty() => {
                    format!("[Invitation to room {}…] {}", room_short, msg.trim())
                }
                _ => format!("[Invitation to room {}…]", room_short),
            }
        }
    }
}

fn format_unix_local(unix_secs: u64) -> String {
    SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(unix_secs))
        .map(|st| {
            let dt: DateTime<Utc> = st.into();
            let local: DateTime<Local> = dt.into();
            local.format("%Y-%m-%d %H:%M:%S").to_string()
        })
        .unwrap_or_else(|| "<bad timestamp>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_token_round_trip() {
        let original = PurgeToken([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        let s = hex_token(&original);
        assert_eq!(s, "00112233445566778899aabbccddeeff");
        assert_eq!(parse_hex_token(&s).unwrap(), original);
    }

    #[test]
    fn parse_hex_token_rejects_bad_inputs() {
        assert!(parse_hex_token("").is_none());
        assert!(parse_hex_token("zz").is_none());
        // Wrong length.
        assert!(parse_hex_token("aa").is_none());
        // 32-char string with a non-hex char.
        assert!(parse_hex_token("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_none());
    }

    #[test]
    fn format_dm_body_for_cli_renders_text_verbatim() {
        let body = DirectMessageBody::Text {
            text: "hello peer".to_string(),
        };
        let s = format_dm_body_for_cli(&body, &HashMap::new());
        assert_eq!(s, "hello peer");
    }

    #[test]
    fn format_dm_body_for_cli_invite_has_room_prefix_and_message() {
        use ed25519_dalek::SigningKey;
        let owner_vk = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        let body = DirectMessageBody::Invite {
            room_owner_vk: owner_vk,
            invitation_payload: vec![],
            personal_message: Some("come hang out".to_string()),
        };
        let s = format_dm_body_for_cli(&body, &HashMap::new());
        assert!(s.starts_with("[Invitation to room "), "got: {}", s);
        assert!(s.ends_with("…] come hang out"), "got: {}", s);
    }

    #[test]
    fn format_dm_body_for_cli_invite_no_personal_message() {
        use ed25519_dalek::SigningKey;
        let owner_vk = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        let body = DirectMessageBody::Invite {
            room_owner_vk: owner_vk,
            invitation_payload: vec![],
            personal_message: None,
        };
        let s = format_dm_body_for_cli(&body, &HashMap::new());
        assert!(s.starts_with("[Invitation to room "), "got: {}", s);
        assert!(s.ends_with("…]"), "got: {}", s);
    }

    #[test]
    fn format_dm_body_for_cli_invite_blank_personal_message_treated_as_none() {
        use ed25519_dalek::SigningKey;
        let owner_vk = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        let body = DirectMessageBody::Invite {
            room_owner_vk: owner_vk,
            invitation_payload: vec![],
            personal_message: Some("   \t  ".to_string()),
        };
        let s = format_dm_body_for_cli(&body, &HashMap::new());
        assert!(
            s.ends_with("…]"),
            "expected blank message to omit suffix, got: {}",
            s
        );
    }

    #[test]
    fn short_member_id_is_short() {
        use ed25519_dalek::SigningKey;
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let id = MemberId::from(&sk.verifying_key());
        let s = short_member_id(&id);
        assert_eq!(s.chars().count(), 8);
        assert!(id.to_string().starts_with(&s));
    }

    /// Codex review of #244 found that the previous `resolve_recipient_vk`
    /// silently picked the first prefix match, which could route a private
    /// DM to the wrong recipient on accidental or malicious prefix
    /// collisions. This test pins the new behaviour: zero matches errors
    /// "no member matched", multi matches error "ambiguous", and a unique
    /// match resolves correctly.
    #[test]
    fn resolve_recipient_vk_rejects_ambiguous_prefix() {
        use ed25519_dalek::SigningKey;
        use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
        use river_core::room_state::member::{AuthorizedMember, Member, MembersV1};
        use river_core::ChatRoomStateV1;

        let owner_sk = SigningKey::from_bytes(&[1u8; 32]);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        // Construct two members whose 1-char id prefixes are very likely
        // to differ; we'll resolve by their actual 8-char prefix.
        let alice_sk = SigningKey::from_bytes(&[2u8; 32]);
        let bob_sk = SigningKey::from_bytes(&[3u8; 32]);

        let mk = |sk: &SigningKey| -> AuthorizedMember {
            AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: sk.verifying_key(),
                },
                &owner_sk,
            )
        };
        let alice = mk(&alice_sk);
        let bob = mk(&bob_sk);
        let alice_id_str = alice.member.id().to_string();
        let bob_id_str = bob.member.id().to_string();

        let state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk),
            members: MembersV1 {
                members: vec![alice.clone(), bob.clone()],
            },
            ..Default::default()
        };

        // Unique full-id prefix resolves.
        let resolved = resolve_recipient_vk(&state, &owner_vk, &alice_id_str).unwrap();
        assert_eq!(resolved, alice_sk.verifying_key());

        // Empty prefix matches every member -> ambiguous (3 matches incl owner).
        let err = resolve_recipient_vk(&state, &owner_vk, "")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ambiguous"),
            "expected ambiguity error, got: {err}"
        );

        // Garbage prefix matches nothing.
        let err = resolve_recipient_vk(&state, &owner_vk, "zzzzzzzzzzzzz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("No member matched"), "got: {err}");

        // A prefix that happens to match both Alice and Bob exactly should
        // error. Construct that case by finding a real common prefix; if
        // the ids share no leading char (likely), at minimum the empty
        // prefix case above proves the multi-match path.
        let common_len = alice_id_str
            .chars()
            .zip(bob_id_str.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if common_len > 0 {
            let shared = &alice_id_str[..common_len];
            let err = resolve_recipient_vk(&state, &owner_vk, shared)
                .unwrap_err()
                .to_string();
            assert!(err.contains("ambiguous"), "got: {err}");
        }
    }

    /// `apply_per_pair_cap` must drop the oldest entries for any
    /// `(room, sender, recipient)` tuple that exceeds the contract's
    /// per-pair cap, mirroring the contract's silent eviction. Without
    /// this the local outbound-plaintext cache would grow unbounded
    /// when the same pair exchanges many DMs.
    #[test]
    fn apply_per_pair_cap_drops_oldest_above_cap() {
        use freenet_scaffold::util::FastHash;
        use river_core::chat_delegate::{OutboundDmEntry, OutboundDmStore};

        let room: [u8; 32] = [1; 32];
        let sender = MemberId(FastHash(11));
        let recipient = MemberId(FastHash(22));

        let mut store = OutboundDmStore {
            entries: vec![],
            hidden_threads: vec![],
        };
        let over_cap = MAX_DM_MESSAGES_PER_PAIR + 5;
        for i in 0..over_cap {
            store.entries.push(OutboundDmEntry {
                room_owner_vk: room,
                sender,
                recipient,
                purge_token: PurgeToken([i as u8; 16]),
                timestamp: i as u64,
                plaintext: format!("msg-{}", i),
            });
        }
        apply_per_pair_cap(&mut store);

        assert_eq!(store.entries.len(), MAX_DM_MESSAGES_PER_PAIR);
        // Oldest 5 entries should have been dropped — surviving ones
        // start at timestamp 5.
        let min_ts = store.entries.iter().map(|e| e.timestamp).min().unwrap();
        assert_eq!(min_ts, 5);
    }

    /// Different `(sender, recipient)` pairs must each have their own
    /// independent cap; the cap mustn't be applied globally.
    #[test]
    fn apply_per_pair_cap_isolates_pairs() {
        use freenet_scaffold::util::FastHash;
        use river_core::chat_delegate::{OutboundDmEntry, OutboundDmStore};

        let room: [u8; 32] = [1; 32];
        let me = MemberId(FastHash(99));
        let alice = MemberId(FastHash(11));
        let bob = MemberId(FastHash(22));

        let mut store = OutboundDmStore {
            entries: vec![],
            hidden_threads: vec![],
        };
        // Fill (me -> alice) to over-cap; (me -> bob) only one entry.
        for i in 0..MAX_DM_MESSAGES_PER_PAIR + 3 {
            store.entries.push(OutboundDmEntry {
                room_owner_vk: room,
                sender: me,
                recipient: alice,
                purge_token: PurgeToken([i as u8; 16]),
                timestamp: i as u64,
                plaintext: "to alice".into(),
            });
        }
        store.entries.push(OutboundDmEntry {
            room_owner_vk: room,
            sender: me,
            recipient: bob,
            purge_token: PurgeToken([0xff; 16]),
            timestamp: 1,
            plaintext: "to bob".into(),
        });

        apply_per_pair_cap(&mut store);

        let alice_count = store
            .entries
            .iter()
            .filter(|e| e.recipient == alice)
            .count();
        let bob_count = store.entries.iter().filter(|e| e.recipient == bob).count();
        assert_eq!(alice_count, MAX_DM_MESSAGES_PER_PAIR);
        assert_eq!(bob_count, 1);
    }
}
