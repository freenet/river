//! In-room direct messages (#243 Phase 3).
//!
//! `dm send` / `dm list` / `dm purge` produce the same wire bytes as the
//! River UI does for the same operation: encryption goes through
//! `river_core::room_state::direct_messages::compose_direct_message`, which
//! itself calls `river_core::ecies::seal_dm_for_recipient`. The contract
//! WASM doesn't care which client posted the state — verification rests on
//! the sender + recipient signatures, both produced from helpers that live
//! in the `river-core` crate.

use crate::api::{ApiClient, Invitation};
use crate::commands::invite::{print_invitation_accepted, resolve_nickname};
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use clap::Subcommand;
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::chat_delegate::OutboundDmEntry;
use river_core::room_state::direct_messages::{
    advance_recipient_purges, compose_direct_message, open_direct_message, pair_message_count,
    PurgeToken, MAX_DM_MESSAGES_PER_PAIR,
};
use river_core::room_state::dm_body::{decode_body, encode_body, DirectMessageBody, InvitePayload};
use river_core::room_state::member::MemberId;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
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
    /// Send a room invitation AS a direct message, so the recipient's River UI
    /// renders it as a clickable "Invitation" card (with an Accept button)
    /// rather than inert text.
    ///
    /// This is the send-side counterpart of `dm accept` and the CLI equivalent
    /// of the UI's "Share invite via DM" button (#252). Pasting a bare invite
    /// code into `dm send` just shows raw text at the other end — only this
    /// structured invite DM produces the card.
    ///
    /// `room_id` is the CARRIER room the DM travels in — you and `recipient`
    /// must both be members of it. `--room` is the TARGET room you are inviting
    /// them to (any room you are a member of, including `room_id` itself). A
    /// fresh single-use invitee credential is minted, exactly as
    /// `invite create` does.
    Invite {
        /// Carrier room ID (base58 room owner key) — the room whose DM thread
        /// the invitation is delivered in. You and the recipient must both be
        /// members.
        room_id: String,
        /// Recipient member ID (short prefix accepted).
        recipient: String,
        /// Target room ID (base58 room owner key) — the room you are inviting
        /// the recipient to join. You must be a member of it.
        #[arg(long)]
        room: String,
        /// Optional personal note shown above the Accept button.
        #[arg(short = 'm', long)]
        message: Option<String>,
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
    /// Accept a room invitation that arrived as a direct message.
    ///
    /// The River UI can "share an invite via DM" (#252): the invitation is
    /// embedded in a DM rather than handed over as an `?invitation=…` link.
    /// Such DMs show up in `dm list <room_id>` as `[Invitation to room …]`.
    /// This command finds that invite DM, decodes the embedded invitation,
    /// and joins the target room — the CLI counterpart of the UI's "Accept"
    /// button on the invitation card.
    ///
    /// `room_id` is the room the invite DM was *received in* (the carrier
    /// room), NOT the room you are joining. If you have invite DMs to more
    /// than one room, narrow the selection with `--from` (by sender) or
    /// `--room` (by target room).
    Accept {
        /// Carrier room ID (base58 room owner key) — the room whose DM
        /// thread contains the invitation.
        room_id: String,
        /// Only consider invite DMs sent by this member (short MemberId
        /// prefix accepted). Narrows by *sender*; if a single sender invited
        /// you to several rooms, disambiguate with `--room` instead.
        #[arg(long)]
        from: Option<String>,
        /// Only consider invitations to this *target* room (base58 prefix of
        /// the target room's owner key — e.g. the 8-char prefix shown by
        /// `dm list`). Use this to pick one when invitations to several rooms
        /// are present, including several from the same sender.
        #[arg(long)]
        room: Option<String>,
        /// Your nickname in the room you are joining.
        #[arg(short = 'N', long)]
        nickname: Option<String>,
    },
}

pub async fn execute(command: DmCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        DmCommands::Send {
            room_id,
            recipient,
            message,
        } => execute_send(api, format, &room_id, &recipient, &message).await,
        DmCommands::Invite {
            room_id,
            recipient,
            room,
            message,
        } => execute_invite(api, format, &room_id, &recipient, &room, message.as_deref()).await,
        DmCommands::List {
            room_id,
            with,
            limit,
            since_minutes,
        } => execute_list(api, format, &room_id, with.as_deref(), limit, since_minutes).await,
        DmCommands::Purge { room_id, token } => execute_purge(api, format, &room_id, &token).await,
        DmCommands::Accept {
            room_id,
            from,
            room,
            nickname,
        } => {
            execute_accept(
                api,
                format,
                &room_id,
                from.as_deref(),
                room.as_deref(),
                nickname,
            )
            .await
        }
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

    // Text-variant DMs keep the legacy wire shape (raw UTF-8 bytes, no magic
    // byte) for backwards compatibility: pre-#243 clients in the wild decode
    // raw UTF-8 directly via `String::from_utf8_lossy`, and new clients
    // round-trip cleanly because `decode_body`'s legacy fallback hands raw
    // UTF-8 back as `DirectMessageBody::Text`. So `dm send` passes the raw
    // bytes, NOT `encode_body(&Text{..})`. Only NEW variants (`Invite`) opt
    // into the magic-byte + CBOR shape — see `execute_invite`.
    deliver_dm(
        &api,
        format,
        room_owner_key,
        &signing_key,
        &room_state,
        recipient_vk,
        message.as_bytes(),
        message.to_string(),
        DmKind::Text,
    )
    .await
}

/// What kind of DM `deliver_dm` is delivering — only affects the
/// success-message wording, not the wire bytes (the caller has already
/// encoded those into `body_bytes`).
#[derive(Clone, Copy)]
enum DmKind {
    Text,
    Invite,
}

/// Shared delivery core for `dm send` and `dm invite`.
///
/// Given an ALREADY-ENCODED DM `body_bytes` (raw UTF-8 for a legacy `Text`
/// DM, magic-byte+CBOR for a structured `Invite`), this runs the identical
/// membership / per-pair-cap pre-flight, composes and signs the DM, publishes
/// the delta (bundling a member-rejoin when the sender was pruned), verifies
/// the DM actually lands in a local pre-flight `apply_delta`, caches
/// `cache_label` for the sender's own `dm list` bubble, and prints the result.
///
/// Keeping one body means the anti-silent-drop guards (#269) and the rejoin
/// bundling (#256 / Ivvor Bug #1) are byte-identical for text and invite DMs.
#[allow(clippy::too_many_arguments)]
async fn deliver_dm(
    api: &ApiClient,
    format: OutputFormat,
    room_owner_key: VerifyingKey,
    signing_key: &SigningKey,
    room_state: &ChatRoomStateV1,
    recipient_vk: VerifyingKey,
    body_bytes: &[u8],
    cache_label: String,
    kind: DmKind,
) -> Result<()> {
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
        api.build_rejoin_delta(room_state, &room_owner_key, signing_key);

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
    let auth = compose_direct_message(
        signing_key,
        &recipient_vk,
        &room_owner_key,
        now,
        now,
        body_bytes,
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
            .apply_delta(room_state, &params, &Some(delta.clone()))
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

    // Persist a label in the local outbound-DM cache so a future `dm list`
    // renders the sender's own bubble as text instead of
    // `<sent: ciphertext only>` (the ciphertext is recipient-sealed, so the
    // sender can't re-derive it). For a text DM this is the message; for an
    // invite it's the same `[Invitation to room …]` summary the recipient's
    // list shows. See issue freenet/river#256. Failure is logged but not
    // fatal — the DM has already been sent.
    if let Err(e) = persist_outbound_plaintext(
        api,
        room_owner_key,
        self_id,
        recipient_id,
        token,
        auth.message.timestamp,
        cache_label,
    ) {
        tracing::warn!("Failed to persist outbound DM plaintext locally: {}", e);
    }

    let noun = match kind {
        DmKind::Text => "DM",
        DmKind::Invite => "Invitation DM",
    };
    match format {
        OutputFormat::Human => {
            println!(
                "{} sent to {} (purge token: {})",
                noun,
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

/// Normalize the optional `--message` for an invite DM: trim it, and treat a
/// blank / whitespace-only note as absent so the recipient's UI hides the
/// message box entirely (per the `InvitePayload::personal_message` contract)
/// rather than rendering an empty line.
fn normalize_personal_message(message: Option<&str>) -> Option<String> {
    message
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// `dm invite`: send a structured invitation DM so the recipient's UI shows a
/// clickable Accept card. The CLI counterpart of the UI's "Share invite via
/// DM" (#252) and the send-side counterpart of `dm accept`.
async fn execute_invite(
    api: ApiClient,
    format: OutputFormat,
    carrier_room_id: &str,
    recipient: &str,
    target_room_id: &str,
    message: Option<&str>,
) -> Result<()> {
    let carrier_room_key = parse_room_id(carrier_room_id)?;
    let target_room_key = parse_room_id(target_room_id)?;

    // Signing identity for the carrier room (the DM is signed by, and travels
    // in, the carrier room).
    let (signing_key, _, _) = api.storage().get_room(&carrier_room_key)?.ok_or_else(|| {
        anyhow!("Carrier room not found. You must be a member of it to send an invite DM.")
    })?;

    let room_state = api.get_room(&carrier_room_key, false).await?;
    let recipient_vk = resolve_recipient_vk(&room_state, &carrier_room_key, recipient)?;

    // Build the invitation for the TARGET room (requires the target room in
    // local storage). Byte-identical to what `invite create` produces — same
    // `build_invitation` — so the recipient's `dm accept` / UI Accept card
    // decode it the same way as a base58 `?invitation=` code.
    let invitation = api.build_invitation(&target_room_key).map_err(|e| {
        anyhow!(
            "Could not build an invitation for the target room (are you a member of it?): {}",
            e
        )
    })?;

    let mut invitation_payload = Vec::new();
    ciborium::ser::into_writer(&invitation, &mut invitation_payload)
        .map_err(|e| anyhow!("Failed to serialize invitation: {}", e))?;

    let body = DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: target_room_key,
        invitation_payload,
        personal_message: normalize_personal_message(message),
    }));
    let body_bytes =
        encode_body(&body).map_err(|e| anyhow!("Failed to encode invite DM: {}", e))?;

    // Same summary the recipient's `dm list` shows, cached so the sender's own
    // bubble isn't a bare `<sent: ciphertext only>`.
    let cache_label = format_dm_body_for_cli(&body, &HashMap::new());

    deliver_dm(
        &api,
        format,
        carrier_room_key,
        &signing_key,
        &room_state,
        recipient_vk,
        &body_bytes,
        cache_label,
        DmKind::Invite,
    )
    .await
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

    let mut room_state = api.get_room(&room_owner_key, false).await?;

    // For a private room, collect the local member's secrets so DM
    // counterparty nicknames (AES-256-GCM sealed) decrypt instead of showing
    // "[Encrypted: N bytes, vN]". Empty / no-op for a public room. Must run
    // before the immutable borrows below.
    let secrets = api.room_display_secrets(&room_owner_key, &mut room_state);

    let with_filter = with
        .map(|s| resolve_recipient_id(&room_state, &room_owner_key, s))
        .transpose()?;

    let cutoff = since_minutes.map(|m| {
        unix_now()
            .map(|now| now.saturating_sub(m.saturating_mul(60)))
            .unwrap_or(0)
    });

    // Build a nickname lookup so output is human-readable (decrypted for a
    // private room).
    let nicknames: HashMap<MemberId, String> = room_state
        .member_info
        .member_info
        .iter()
        .map(|info| {
            (
                info.member_info.member_id,
                crate::api::unseal_nickname_display(&info.member_info.preferred_nickname, &secrets),
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
        let (body_str, is_invite) = if is_self_recipient {
            match open_direct_message(&signing_key, msg) {
                Ok(bytes) => match decode_body(&bytes) {
                    Ok(body) => {
                        let invite = matches!(body, DirectMessageBody::Invite(_));
                        (format_dm_body_for_cli(&body, &nicknames), invite)
                    }
                    Err(_) => ("<unable to decode body>".to_string(), false),
                },
                Err(_) => ("<unable to decrypt>".to_string(), false),
            }
        } else {
            let plaintext = match outbound_lookup.get(&(msg.message.recipient, msg.purge_token())) {
                Some(plaintext) => plaintext.clone(),
                None => "<sent: ciphertext only>".to_string(),
            };
            (plaintext, false)
        };

        decrypted.push(DecryptedDm {
            counterparty,
            outgoing: is_self_sender,
            timestamp: msg.message.timestamp,
            body: body_str,
            token: msg.purge_token(),
            is_invite,
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

    // Whether any inbound invite DM exists — computed BEFORE the per-thread
    // cap below so a `dm accept`able invite that scrolled off a chatty thread
    // still surfaces the discoverability tip (`dm accept` scans the full,
    // uncapped state regardless of what `dm list` chose to print).
    let any_invite = decrypted.iter().any(|dm| dm.is_invite);

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
            // Discoverability: invite-via-DM (#252) bodies render as
            // `[Invitation to room …]` but aren't actionable until the user
            // knows the accept command. Point them at it when any is present.
            if any_invite {
                println!(
                    "Tip: accept an invitation that arrived as a DM with \
                     `riverctl dm accept {room_id}` (narrow with `--from <sender>` or \
                     `--room <target>` if you have several)."
                );
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
                                    "is_invitation": dm.is_invite,
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

/// `dm accept` — join a room from an invitation that arrived as a DM.
///
/// The invite-via-DM flow (#252) embeds a CBOR `Invitation` inside a
/// `DirectMessageBody::Invite` whose ECIES envelope only the recipient can
/// open, so the invitation never appears as a base58 `?invitation=…` code the
/// user could paste into `invite accept`. We decrypt every inbound DM in the
/// carrier room, keep the ones that decode to an `Invite`, narrow by sender
/// (`--from`) and/or target room (`--room`), then pick a single valid
/// invitation via [`select_invite_to_accept`] and join through the same
/// `accept_invitation_struct` core the base58 `invite accept` path uses — so
/// the re-accept guard, room GET, secret persistence and join publish are all
/// byte-identical regardless of how the invitation arrived. (In particular the
/// invitation's `room_secrets` flow through `accept_invitation_struct`
/// unchanged, so a DM-borne private-room invite persists its secrets exactly
/// like the URL path.)
async fn execute_accept(
    api: ApiClient,
    format: OutputFormat,
    room_id: &str,
    from: Option<&str>,
    room: Option<&str>,
    nickname: Option<String>,
) -> Result<()> {
    let room_owner_key = parse_room_id(room_id)?;
    let (signing_key, _, _) = api.storage().get_room(&room_owner_key)?.ok_or_else(|| {
        anyhow!("Room not found. You must be a member of the carrier room to read its invite DMs.")
    })?;

    let room_state = api.get_room(&room_owner_key, false).await?;

    let invites = collect_inbound_invites(&room_state, &signing_key, from);
    if invites.is_empty() {
        return Err(match from {
            Some(f) => anyhow!(
                "No invitation DMs from a sender matching '{}' were found in this room. \
                 Run `riverctl dm list {}` to see who sent you invitations.",
                f,
                room_id
            ),
            None => anyhow!(
                "No invitation DMs found in this room. An invitation sent via DM appears in \
                 `riverctl dm list {}` as `[Invitation to room …]`.",
                room_id
            ),
        });
    }

    let chosen = select_invite_to_accept(invites, room)?;
    let nickname = resolve_nickname(nickname)?;

    if !matches!(format, OutputFormat::Json) {
        eprintln!(
            "Accepting invitation to room {} (from {})...",
            short_room_key(&chosen.target),
            short_member_id(&chosen.sender)
        );
    }

    let (room_owner_vk, contract_key) = api
        .accept_invitation_struct(chosen.invitation, &nickname)
        .await?;
    print_invitation_accepted(format, &room_owner_vk, &contract_key);
    Ok(())
}

/// A decoded, room-matched invite candidate — an [`InboundInvite`] whose
/// embedded `Invitation` decoded cleanly and targets the room its payload
/// advertised.
#[derive(Debug)]
struct ValidInvite {
    sender: MemberId,
    timestamp: u64,
    target: VerifyingKey,
    invitation: Invitation,
}

/// Choose the single invitation `dm accept` should act on, or explain why it
/// can't. Pure (no I/O) so the selection rules are unit-testable.
///
/// Steps, in order:
/// 1. **Target filter.** When `room_filter` is set, drop invites whose target
///    room's base58 key doesn't start with the given prefix.
/// 2. **Decode + validate.** Decode each remaining invite's embedded
///    `Invitation` and keep only the well-formed, room-matched ones
///    (`decode_invitation_from_payload`). Malformed invites — undecodable CBOR
///    or a target-room mismatch — are dropped, NOT fatal: a later corrupt or
///    malicious invite to a room must not block accepting an earlier valid one
///    (Codex review of #343). If every candidate is malformed we say so, with
///    a count.
/// 3. **Disambiguate.** If the surviving valid invites target more than one
///    distinct room, refuse and list them, pointing at `--room` / `--from`.
/// 4. **Pick newest.** For the single surviving target, take the highest
///    `timestamp`. Several valid invites to one room are interchangeable
///    bearer credentials, so the choice only needs to be deterministic.
fn select_invite_to_accept(
    invites: Vec<InboundInvite>,
    room_filter: Option<&str>,
) -> Result<ValidInvite> {
    let filtered: Vec<InboundInvite> = match room_filter {
        Some(prefix) => invites
            .into_iter()
            .filter(|i| {
                bs58::encode(i.payload.room_owner_vk.as_bytes())
                    .into_string()
                    .starts_with(prefix)
            })
            .collect(),
        None => invites,
    };
    if filtered.is_empty() {
        return Err(anyhow!(
            "No invitation DMs target a room matching '{}'. Drop or widen `--room`.",
            room_filter.unwrap_or_default()
        ));
    }

    // Decode + validate; drop (don't fail on) malformed invites.
    let total = filtered.len();
    let mut valid: Vec<ValidInvite> = Vec::new();
    for inv in &filtered {
        if let Ok(invitation) = decode_invitation_from_payload(&inv.payload) {
            valid.push(ValidInvite {
                sender: inv.sender,
                timestamp: inv.timestamp,
                target: inv.payload.room_owner_vk,
                invitation,
            });
        }
    }
    if valid.is_empty() {
        return Err(anyhow!(
            "All {} matching invitation DM(s) were malformed (undecodable, or targeting a \
             mismatched room) and were skipped. Nothing to accept.",
            total
        ));
    }

    // Disambiguate to a single target room among the VALID invites.
    let distinct_targets: HashSet<[u8; 32]> = valid.iter().map(|v| v.target.to_bytes()).collect();
    if distinct_targets.len() > 1 {
        let mut lines: Vec<String> = valid
            .iter()
            .map(|v| {
                format!(
                    "  - room {} (from {})",
                    short_room_key(&v.target),
                    short_member_id(&v.sender)
                )
            })
            .collect();
        lines.sort();
        lines.dedup();
        // Tailor the hint: if the user already passed `--room` and it still
        // matched several rooms (a too-short prefix / prefix collision),
        // telling them to "use --room" again is a dead-end — ask for a longer
        // prefix instead.
        let hint = if room_filter.is_some() {
            "That `--room` prefix matches more than one room — pass a longer prefix \
             (or add `--from <sender prefix>`) to pick one."
        } else {
            "Re-run with `--room <target room prefix>` (or `--from <sender prefix>`) to pick one."
        };
        return Err(anyhow!(
            "Found invitations to {} different rooms in this DM thread:\n{}\n{}",
            distinct_targets.len(),
            lines.join("\n"),
            hint
        ));
    }

    // Single target: newest valid invite. `max_by_key` returns the last
    // maximal element on ties, which is fine — interchangeable credentials.
    Ok(valid
        .into_iter()
        .max_by_key(|v| v.timestamp)
        .expect("valid is non-empty (checked above)"))
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
    /// True when the decrypted body decoded to a
    /// `DirectMessageBody::Invite` — drives the `dm list` "accept with…"
    /// footer tip and the `is_invitation` JSON field.
    is_invite: bool,
}

/// One inbound DM that decoded to a `DirectMessageBody::Invite`, surfaced by
/// [`collect_inbound_invites`] for `dm accept`.
struct InboundInvite {
    sender: MemberId,
    timestamp: u64,
    payload: InvitePayload,
}

/// Decrypt every inbound DM in `state` addressed to the local member and
/// return those that decode to a `DirectMessageBody::Invite`. `from_filter`,
/// when set, keeps only invites whose sender's MemberId string starts with
/// the given prefix. This is a plain `starts_with` on the MemberId — unlike
/// `dm list --with` (which routes through `resolve_recipient_vk`) it does NOT
/// consult the member list or error on an ambiguous prefix; over-broad
/// filters simply fall through to the target-room disambiguation in
/// [`select_invite_to_accept`]. Pure (no I/O) so `dm accept`'s selection
/// logic is unit-testable against a hand-built room state. Undecryptable or
/// undecodable DMs are skipped rather than failing the whole command — a
/// single corrupt entry shouldn't block accepting a valid invitation.
fn collect_inbound_invites(
    state: &ChatRoomStateV1,
    signing_key: &SigningKey,
    from_filter: Option<&str>,
) -> Vec<InboundInvite> {
    let self_id = MemberId::from(&signing_key.verifying_key());
    let mut out = Vec::new();
    for msg in &state.direct_messages.messages {
        if msg.message.recipient != self_id {
            continue;
        }
        if let Some(prefix) = from_filter {
            if !msg.message.sender.to_string().starts_with(prefix) {
                continue;
            }
        }
        let Ok(bytes) = open_direct_message(signing_key, msg) else {
            continue;
        };
        let Ok(body) = decode_body(&bytes) else {
            continue;
        };
        if let DirectMessageBody::Invite(payload) = body {
            out.push(InboundInvite {
                sender: msg.message.sender,
                timestamp: msg.message.timestamp,
                payload: *payload,
            });
        }
    }
    out
}

/// Decode the CBOR `Invitation` embedded in an [`InvitePayload`] and
/// cross-check that it targets the room the payload advertises. The target
/// room key is carried twice — once in the cheap-to-inspect `room_owner_vk`
/// field and once inside the `Invitation` — and the `dm_body` docs say a
/// client SHOULD reject a mismatch as malformed (the cleartext "which room"
/// hint disagreeing with the credential we'd act on). This is a field-equality
/// check, NOT cryptographic verification: authorization is enforced by the
/// room contract when the join delta is published (same as `invite accept`),
/// so this only stops us acting on a self-inconsistent / spoofed-hint DM.
fn decode_invitation_from_payload(payload: &InvitePayload) -> Result<Invitation> {
    let invitation: Invitation = ciborium::de::from_reader(&payload.invitation_payload[..])
        .map_err(|e| anyhow!("Invite DM carried an undecodable invitation: {}", e))?;
    if invitation.room != payload.room_owner_vk {
        return Err(anyhow!(
            "Malformed invite DM: the embedded invitation targets a different room than the \
             DM's advertised target. Refusing to act on it."
        ));
    }
    Ok(invitation)
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

/// 8-char prefix of a room owner key's base58 form — the same abbreviation
/// `dm list` shows for invite DMs, reused by `dm accept` diagnostics.
fn short_room_key(vk: &VerifyingKey) -> String {
    bs58::encode(vk.as_bytes())
        .into_string()
        .chars()
        .take(8)
        .collect()
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
    // Single locked load→mutate→save so a concurrent DM-send / room-leave can't
    // clobber this append (issue freenet/river#307).
    let room_bytes = room_owner_vk.to_bytes();
    api.storage().mutate_outbound_dms(|store| {
        store.entries.push(OutboundDmEntry {
            room_owner_vk: room_bytes,
            sender,
            recipient,
            purge_token,
            timestamp,
            plaintext,
        });
        apply_per_pair_cap(store);
        Ok(())
    })
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
    let room_bytes = room_owner_vk.to_bytes();

    // The purge set is derived from `room_state`, not the cache, so compute it
    // (and short-circuit when empty) BEFORE taking the storage lock — no point
    // serializing on an empty prune.
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

    // Single locked load→mutate→save so a concurrent DM-send can't clobber this
    // prune (issue freenet/river#307).
    api.storage().mutate_outbound_dms(|store| {
        store.entries.retain(|e| {
            e.room_owner_vk != room_bytes || !purged.contains(&(e.recipient, e.purge_token))
        });
        Ok(())
    })
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
///   isn't decoded here — we just surface the target room owner key (8-char
///   prefix) so the recipient knows which room it's for. To actually join,
///   run `riverctl dm accept <carrier room id>` (see [`execute_accept`]),
///   which decodes the embedded invitation; `dm list` prints a tip pointing
///   there whenever an invite DM is present. Personal message (when present)
///   is appended.
fn format_dm_body_for_cli(
    body: &DirectMessageBody,
    _nicknames: &HashMap<MemberId, String>,
) -> String {
    match body {
        DirectMessageBody::Text { text } => text.clone(),
        DirectMessageBody::Invite(payload) => {
            let room_short = short_room_key(&payload.room_owner_vk);
            match &payload.personal_message {
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
        use river_core::room_state::dm_body::InvitePayload;
        let owner_vk = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        let body = DirectMessageBody::Invite(Box::new(InvitePayload {
            room_owner_vk: owner_vk,
            invitation_payload: vec![],
            personal_message: Some("come hang out".to_string()),
        }));
        let s = format_dm_body_for_cli(&body, &HashMap::new());
        assert!(s.starts_with("[Invitation to room "), "got: {}", s);
        assert!(s.ends_with("…] come hang out"), "got: {}", s);
    }

    #[test]
    fn format_dm_body_for_cli_invite_no_personal_message() {
        use ed25519_dalek::SigningKey;
        use river_core::room_state::dm_body::InvitePayload;
        let owner_vk = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        let body = DirectMessageBody::Invite(Box::new(InvitePayload {
            room_owner_vk: owner_vk,
            invitation_payload: vec![],
            personal_message: None,
        }));
        let s = format_dm_body_for_cli(&body, &HashMap::new());
        assert!(s.starts_with("[Invitation to room "), "got: {}", s);
        assert!(s.ends_with("…]"), "got: {}", s);
    }

    #[test]
    fn format_dm_body_for_cli_invite_blank_personal_message_treated_as_none() {
        use ed25519_dalek::SigningKey;
        use river_core::room_state::dm_body::InvitePayload;
        let owner_vk = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        let body = DirectMessageBody::Invite(Box::new(InvitePayload {
            room_owner_vk: owner_vk,
            invitation_payload: vec![],
            personal_message: Some("   \t  ".to_string()),
        }));
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

    // -----------------------------------------------------------------
    // `dm accept` selection logic (#252 CLI parity)
    // -----------------------------------------------------------------

    use ed25519_dalek::SigningKey;
    use river_core::room_state::dm_body::encode_body;
    use river_core::room_state::member::{AuthorizedMember, Member};

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// Build a structurally-valid CBOR `Invitation` for `target_owner`,
    /// signed by `target_owner` as the inviter. Content beyond `room` is
    /// inert for these tests — we only exercise decode + room-match.
    fn encode_invitation(target_owner: &SigningKey) -> Vec<u8> {
        let target_vk = target_owner.verifying_key();
        let owner_id = MemberId::from(&target_vk);
        let invitee_sk = key(200);
        let member = Member {
            owner_member_id: owner_id,
            member_vk: invitee_sk.verifying_key(),
            invited_by: owner_id,
        };
        let invitation = Invitation {
            room: target_vk,
            invitee_signing_key: invitee_sk,
            invitee: AuthorizedMember::new(member, target_owner),
            room_secrets: vec![],
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&invitation, &mut bytes).unwrap();
        bytes
    }

    /// Compose an inbound DM addressed to `me` from `sender` carrying the
    /// given body bytes, anchored in carrier room `carrier_owner`.
    fn dm_to_me(
        sender: &SigningKey,
        me: &SigningKey,
        carrier_owner: &VerifyingKey,
        body: &[u8],
        ts: u64,
    ) -> river_core::room_state::direct_messages::AuthorizedDirectMessage {
        compose_direct_message(sender, &me.verifying_key(), carrier_owner, ts, ts, body).unwrap()
    }

    fn invite_body(target_owner_vk: VerifyingKey, payload_bytes: Vec<u8>) -> Vec<u8> {
        encode_body(&DirectMessageBody::Invite(Box::new(InvitePayload {
            room_owner_vk: target_owner_vk,
            invitation_payload: payload_bytes,
            personal_message: None,
        })))
        .unwrap()
    }

    /// `collect_inbound_invites` returns only inbound `Invite` DMs: it skips
    /// plain text DMs, DMs addressed to someone else, and honours the
    /// `--from` sender prefix filter.
    #[test]
    fn collect_inbound_invites_filters_correctly() {
        let me = key(1);
        let alice = key(2);
        let bob = key(3);
        let other = key(4);
        let carrier = key(10).verifying_key();
        let target_a = key(20).verifying_key();
        let target_b = key(21).verifying_key();
        let ts = unix_now().unwrap();

        let mut state = ChatRoomStateV1::default();
        // Invite DM to me from Alice (target room A).
        state.direct_messages.messages.push(dm_to_me(
            &alice,
            &me,
            &carrier,
            &invite_body(target_a, vec![1, 2, 3]),
            ts,
        ));
        // Plain text DM to me from Alice — must be ignored.
        state
            .direct_messages
            .messages
            .push(dm_to_me(&alice, &me, &carrier, b"just chatting", ts));
        // Invite DM to me from Bob (target room B).
        state.direct_messages.messages.push(dm_to_me(
            &bob,
            &me,
            &carrier,
            &invite_body(target_b, vec![4, 5, 6]),
            ts,
        ));
        // Invite DM addressed to someone ELSE — must be ignored.
        state.direct_messages.messages.push(dm_to_me(
            &alice,
            &other,
            &carrier,
            &invite_body(target_a, vec![7, 8, 9]),
            ts,
        ));

        // No filter: both invites addressed to me, none of the noise.
        let all = collect_inbound_invites(&state, &me, None);
        assert_eq!(all.len(), 2, "should find Alice's and Bob's invites only");

        // `--from` Alice's MemberId prefix: just her invite.
        let alice_id = MemberId::from(&alice.verifying_key()).to_string();
        let from_alice = collect_inbound_invites(&state, &me, Some(&alice_id[..8]));
        assert_eq!(from_alice.len(), 1);
        assert_eq!(
            from_alice[0].payload.room_owner_vk, target_a,
            "Alice's invite targets room A"
        );

        // `--from` a prefix matching nobody: empty.
        assert!(collect_inbound_invites(&state, &me, Some("zzzzzzzz")).is_empty());
    }

    /// `decode_invitation_from_payload` round-trips a matching invitation and
    /// rejects one whose embedded `room` disagrees with the payload's
    /// advertised `room_owner_vk` (malformed / spoofed invite DM).
    #[test]
    fn decode_invitation_from_payload_validates_room_match() {
        let target = key(30);
        let target_vk = target.verifying_key();
        let bytes = encode_invitation(&target);

        // Matching room_owner_vk → decodes to an invitation for that room.
        let good = InvitePayload {
            room_owner_vk: target_vk,
            invitation_payload: bytes.clone(),
            personal_message: None,
        };
        let inv = decode_invitation_from_payload(&good).expect("matching invite should decode");
        assert_eq!(inv.room, target_vk);

        // Mismatched room_owner_vk → rejected as malformed.
        let mismatched = InvitePayload {
            room_owner_vk: key(31).verifying_key(),
            invitation_payload: bytes,
            personal_message: None,
        };
        let err = decode_invitation_from_payload(&mismatched)
            .expect_err("room mismatch must be rejected");
        assert!(err.to_string().contains("different room"), "got: {err}");

        // Garbage payload → decode error, not a panic.
        let garbage = InvitePayload {
            room_owner_vk: target_vk,
            invitation_payload: vec![0xff, 0x00, 0x13, 0x37],
            personal_message: None,
        };
        assert!(decode_invitation_from_payload(&garbage).is_err());
    }

    /// The invite DM that `dm invite` builds must decode back — through the
    /// exact `decode_body` + `decode_invitation_from_payload` path `dm accept`
    /// and the UI Accept card use — into the same invitation. This pins the
    /// send/accept wire contract end to end (freenet/river#456).
    #[test]
    fn invite_dm_body_round_trips_through_accept_path() {
        let target = key(30);
        let target_vk = target.verifying_key();
        // Same bytes `execute_invite` puts in `invitation_payload` (CBOR of the
        // `Invitation`, identical to the base58 `invite create` payload).
        let invitation_payload = encode_invitation(&target);

        let body = DirectMessageBody::Invite(Box::new(InvitePayload {
            room_owner_vk: target_vk,
            invitation_payload,
            personal_message: normalize_personal_message(Some("  come join  ")),
        }));

        // Wire round-trip: encode_body → decode_body.
        let wire = encode_body(&body).expect("encode");
        let decoded = decode_body(&wire).expect("decode");
        assert_eq!(decoded, body, "invite DM must survive the wire round-trip");

        let DirectMessageBody::Invite(payload) = decoded else {
            panic!("expected an Invite body");
        };
        // Personal message trimmed, not dropped.
        assert_eq!(payload.personal_message.as_deref(), Some("come join"));

        // The recipient's accept path decodes the embedded invitation and
        // checks it targets the advertised room.
        let invitation = decode_invitation_from_payload(&payload)
            .expect("invite must decode on the accept side");
        assert_eq!(invitation.room, target_vk);

        // And `dm list` labels it as an invitation to that room.
        let label = format_dm_body_for_cli(&body, &HashMap::new());
        assert!(
            label.starts_with("[Invitation to room ") && label.ends_with("come join"),
            "got: {label}"
        );
    }

    /// A blank / whitespace-only `--message` becomes `None` so the recipient's
    /// UI hides the message box rather than rendering an empty line.
    #[test]
    fn normalize_personal_message_blanks_to_none() {
        assert_eq!(normalize_personal_message(None), None);
        assert_eq!(normalize_personal_message(Some("")), None);
        assert_eq!(normalize_personal_message(Some("   ")), None);
        assert_eq!(
            normalize_personal_message(Some("  hi ")).as_deref(),
            Some("hi")
        );
    }

    /// Build an `InboundInvite` for `select_invite_to_accept` tests. A `valid`
    /// invite carries a well-formed, room-matched `Invitation`; otherwise the
    /// payload is garbage CBOR that fails to decode.
    fn inbound(sender: &SigningKey, target: &SigningKey, ts: u64, valid: bool) -> InboundInvite {
        let payload_bytes = if valid {
            encode_invitation(target)
        } else {
            vec![0xde, 0xad, 0xbe, 0xef]
        };
        InboundInvite {
            sender: MemberId::from(&sender.verifying_key()),
            timestamp: ts,
            payload: InvitePayload {
                room_owner_vk: target.verifying_key(),
                invitation_payload: payload_bytes,
                personal_message: None,
            },
        }
    }

    /// Several valid invites to ONE target collapse to that target, and the
    /// newest (highest timestamp) is chosen.
    #[test]
    fn select_picks_newest_valid_for_single_target() {
        let alice = key(2);
        let target_a = key(20);
        let chosen = select_invite_to_accept(
            vec![
                inbound(&alice, &target_a, 100, true),
                inbound(&alice, &target_a, 200, true),
            ],
            None,
        )
        .expect("single target should select");
        assert_eq!(chosen.target, target_a.verifying_key());
        assert_eq!(chosen.timestamp, 200, "newest valid invite wins");
    }

    /// Codex #343 P2: a malformed NEWER invite to a target must not block
    /// accepting an older VALID invite to the same target.
    #[test]
    fn select_skips_malformed_newest_falls_back_to_valid() {
        let alice = key(2);
        let target_a = key(20);
        let chosen = select_invite_to_accept(
            vec![
                inbound(&alice, &target_a, 100, true),  // valid, older
                inbound(&alice, &target_a, 200, false), // malformed, newer
            ],
            None,
        )
        .expect("should fall back to the older valid invite");
        assert_eq!(chosen.target, target_a.verifying_key());
        assert_eq!(chosen.timestamp, 100, "the valid invite is selected");
    }

    /// When every candidate is malformed, selection fails with a clear message
    /// rather than picking one and aborting deep in the accept path.
    #[test]
    fn select_errors_when_all_malformed() {
        let alice = key(2);
        let target_a = key(20);
        let err = select_invite_to_accept(vec![inbound(&alice, &target_a, 100, false)], None)
            .expect_err("all-malformed must error");
        assert!(err.to_string().contains("malformed"), "got: {err}");
    }

    /// Valid invites to DISTINCT target rooms are ambiguous and refused with a
    /// listing pointing at the disambiguating flags.
    #[test]
    fn select_errors_on_distinct_targets() {
        let alice = key(2);
        let bob = key(3);
        let target_a = key(20);
        let target_b = key(21);
        let err = select_invite_to_accept(
            vec![
                inbound(&alice, &target_a, 100, true),
                inbound(&bob, &target_b, 100, true),
            ],
            None,
        )
        .expect_err("distinct targets must be ambiguous");
        assert!(err.to_string().contains("different rooms"), "got: {err}");
    }

    /// Codex #343 P2: `--room` resolves the same-sender-multiple-rooms case
    /// that `--from` cannot.
    #[test]
    fn select_room_filter_disambiguates_same_sender_two_rooms() {
        let alice = key(2);
        let target_a = key(20);
        let target_b = key(21);
        let invites = || {
            vec![
                inbound(&alice, &target_a, 100, true),
                inbound(&alice, &target_b, 100, true),
            ]
        };

        // No room filter → ambiguous even though it's one sender.
        assert!(select_invite_to_accept(invites(), None).is_err());

        // `--room <prefix of B>` → unambiguously picks room B.
        let b_prefix: String = bs58::encode(target_b.verifying_key().as_bytes())
            .into_string()
            .chars()
            .take(12)
            .collect();
        let chosen = select_invite_to_accept(invites(), Some(&b_prefix))
            .expect("room filter should disambiguate");
        assert_eq!(chosen.target, target_b.verifying_key());
    }

    /// A `--room` prefix matching no invite errors rather than silently
    /// selecting an unrelated invitation.
    #[test]
    fn select_room_filter_no_match_errors() {
        let alice = key(2);
        let target_a = key(20);
        let err =
            select_invite_to_accept(vec![inbound(&alice, &target_a, 100, true)], Some("zzzz"))
                .expect_err("no-match room filter must error");
        assert!(err.to_string().contains("matching"), "got: {err}");
    }

    /// A too-broad `--room` that still matches several distinct targets must
    /// fall through to the ambiguity error — never silently pick one. (The
    /// empty prefix matches every target via `starts_with("")`, modelling a
    /// prefix collision.)
    #[test]
    fn select_room_filter_too_broad_still_errors() {
        let alice = key(2);
        let bob = key(3);
        let target_a = key(20);
        let target_b = key(21);
        let err = select_invite_to_accept(
            vec![
                inbound(&alice, &target_a, 100, true),
                inbound(&bob, &target_b, 100, true),
            ],
            Some(""),
        )
        .expect_err("an over-broad --room must still error, not pick");
        let msg = err.to_string();
        assert!(msg.contains("different rooms"), "got: {msg}");
        // The hint adapts: since `--room` was supplied, ask for a longer prefix.
        assert!(msg.contains("longer prefix"), "got: {msg}");
    }

    /// A malformed invite to a DIFFERENT room must be dropped, not counted as a
    /// second distinct target — otherwise it would spuriously block accepting
    /// the one valid invite.
    #[test]
    fn select_drops_malformed_across_targets() {
        let alice = key(2);
        let bob = key(3);
        let target_a = key(20);
        let target_b = key(21);
        let chosen = select_invite_to_accept(
            vec![
                inbound(&alice, &target_a, 100, true), // valid → room A
                inbound(&bob, &target_b, 200, false),  // malformed → room B, dropped
            ],
            None,
        )
        .expect("the lone valid invite should be selected");
        assert_eq!(chosen.target, target_a.verifying_key());
    }
}
