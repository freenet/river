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
use river_core::room_state::direct_messages::{
    advance_recipient_purges, compose_direct_message, open_direct_message, AuthorizedDirectMessage,
    PurgeToken,
};
use river_core::room_state::member::MemberId;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use serde_json::json;
use std::collections::HashMap;
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
    Purge {
        /// Room ID
        room_id: String,
        /// Either a 32-character hex PurgeToken (16 bytes) or a 1-based
        /// index from the most recent `dm list` output (DMs addressed to
        /// you, sorted ascending by timestamp).
        token_or_index: String,
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
        DmCommands::Purge {
            room_id,
            token_or_index,
        } => execute_purge(api, format, &room_id, &token_or_index).await,
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

    // Make sure both ends are current members from the contract's POV; the
    // contract `verify` would otherwise reject the delta and waste a round
    // trip.
    let owner_id = MemberId::from(&room_owner_key);
    let is_self_member = self_id == owner_id
        || room_state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == self_id);
    if !is_self_member {
        return Err(anyhow!(
            "Your member entry is not in the room. Run `riverctl invite accept` first."
        ));
    }
    let is_recipient_member = recipient_id == owner_id
        || room_state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == recipient_id);
    if !is_recipient_member {
        return Err(anyhow!("Recipient is not currently a member of the room."));
    }

    let now = unix_now()?;
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

    api.send_state_delta(&room_owner_key, &delta).await?;

    let token = auth.purge_token();
    let token_hex = hex_token(&token);
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

        // Sent DMs are encrypted to the recipient — we can't decrypt our own
        // outbound messages from the contract state alone.
        let body = if is_self_recipient {
            open_direct_message(&signing_key, msg)
                .unwrap_or_else(|_| b"<unable to decrypt>".to_vec())
        } else {
            b"<sent: ciphertext only>".to_vec()
        };

        decrypted.push(DecryptedDm {
            counterparty,
            outgoing: is_self_sender,
            timestamp: msg.message.timestamp,
            body: String::from_utf8_lossy(&body).into_owned(),
            token: msg.purge_token(),
        });
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
    token_or_index: &str,
) -> Result<()> {
    let room_owner_key = parse_room_id(room_id)?;
    let (signing_key, _, _) = api
        .storage()
        .get_room(&room_owner_key)?
        .ok_or_else(|| anyhow!("Room not found. You must be a member of the room to purge DMs."))?;
    let self_id: MemberId = signing_key.verifying_key().into();

    let room_state = api.get_room(&room_owner_key, false).await?;

    // Resolve the input either as a hex token or an index into our inbound
    // DM list (sorted ascending by timestamp).
    let inbound_for_me: Vec<&AuthorizedDirectMessage> = {
        let mut v: Vec<&AuthorizedDirectMessage> = room_state
            .direct_messages
            .messages
            .iter()
            .filter(|m| m.message.recipient == self_id)
            .collect();
        v.sort_by_key(|m| m.message.timestamp);
        v
    };

    let resolved_token = if let Some(t) = parse_hex_token(token_or_index) {
        t
    } else if let Ok(idx_1based) = token_or_index.parse::<usize>() {
        if idx_1based == 0 || idx_1based > inbound_for_me.len() {
            return Err(anyhow!(
                "Index {} out of range (1..={} based on inbound DMs)",
                idx_1based,
                inbound_for_me.len()
            ));
        }
        inbound_for_me[idx_1based - 1].purge_token()
    } else {
        return Err(anyhow!(
            "Expected a 32-character hex token or a 1-based DM index, got: {}",
            token_or_index
        ));
    };

    // Sanity: confirm the token names a DM we actually received.
    let matched = inbound_for_me
        .iter()
        .any(|m| m.purge_token() == resolved_token);
    if !matched {
        return Err(anyhow!(
            "No inbound DM in this room matches that purge token. Run `dm list` to confirm."
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
    let owner_id = MemberId::from(room_owner_key);
    if let Some(member) = state
        .members
        .members
        .iter()
        .find(|m| m.member.id().to_string().starts_with(needle))
    {
        return Ok(member.member.member_vk);
    }
    if owner_id.to_string().starts_with(needle) {
        return Ok(*room_owner_key);
    }
    Err(anyhow!(
        "No member matched '{}' in this room. Pass a longer prefix (try `riverctl member list`).",
        needle
    ))
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

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("clock before UNIX_EPOCH: {}", e))?
        .as_secs())
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
    fn short_member_id_is_short() {
        use ed25519_dalek::SigningKey;
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let id = MemberId::from(&sk.verifying_key());
        let s = short_member_id(&id);
        assert_eq!(s.chars().count(), 8);
        assert!(id.to_string().starts_with(&s));
    }
}
