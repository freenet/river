use crate::api::ApiClient;
use crate::deputies::{grant_status_line, party_label, DeputyGrant, RoomDeputies};
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1};
use river_core::room_state::member::{AuthorizedMember, MemberId, MembersV1};
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::ChatRoomStateV1;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Subcommand)]
pub enum DebugCommands {
    /// Perform a raw contract GET operation
    ContractGet {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
    /// Test WebSocket connection
    Websocket,
    /// Show contract key for a room
    ContractKey {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
    /// Show room state summary including bans, members, and configuration
    RoomState {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
    /// Show current ban list for a room
    Bans {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
    /// Show room configuration
    Config {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
}

/// What a ban is currently doing, as opposed to merely being stored
/// (freenet/river#472). Since deputy ban authority (#410) a ban can sit in
/// state without excluding anyone, and the CLI used to render that identically
/// to a live ban.
///
/// Three states rather than a boolean because the honest answer genuinely has
/// three cases: a ban can be provably dead, provably live, or contingent on
/// something that has not happened yet. Collapsing the third into either
/// neighbour is a lie in one direction or the other.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BanEnforcement {
    /// Definitive: this ban is not keeping its target out. Either the target is
    /// a current member the ban fails to exclude (so that person is in the room
    /// right now), or the ban can never apply at all — its signature does not
    /// verify against the banner's current key, or it names the room OWNER, who
    /// `is_ban_authorized` refuses as a target unconditionally.
    Inert,
    /// The ban currently excludes its target: the banner's authority over them
    /// holds against the state as it stands.
    ///
    /// Note exactly what this does and does not promise. Reached with the target
    /// ABSENT, it implies an ABSOLUTE grant (the banner is the owner, or an
    /// owner-appointed global moderator), because only those re-derive without
    /// the target's invite chain — so the ban re-applies wherever they reattach.
    /// Reached with the target still PRESENT, the grant may instead be positional
    /// (a strict ancestor), which is valid now but lapses if that ancestor
    /// leaves. So this always means "excluding them today", and additionally
    /// means "durable across their return" only in the absent case.
    Enforcing,
    /// The target is not in the room, but the banner's authority came from their
    /// POSITION relative to the target (strict ancestor, or deputy of a non-owner
    /// ancestor) or from no grant at all. An absent member has no invite chain to
    /// walk, so that authority cannot be re-derived now and re-derives differently
    /// depending on who re-invites them. Not a promise either way.
    Undetermined,
}

impl BanEnforcement {
    /// Suffix appended to this ban's line in the human listing. `Enforcing` is
    /// unmarked because it is the expected case; the other two are what a
    /// moderator needs to notice.
    fn marker(self) -> &'static str {
        match self {
            BanEnforcement::Enforcing => "",
            BanEnforcement::Inert => "  [NOT ENFORCING]",
            BanEnforcement::Undetermined => "  [UNDETERMINED]",
        }
    }
}

#[derive(Serialize)]
struct BanInfo {
    banned_user_id: String,
    banned_by_id: String,
    banned_at_secs: u64,
    /// See [`BanEnforcement`].
    ///
    /// Do NOT reduce this to a bare `is_ban_authorized`. That predicate returns
    /// false for BOTH "provably unauthorized" and "authority is position-derived
    /// and the target is gone", so folding its false into a single not-enforcing
    /// verdict reports every strict-ancestor and non-owner-deputy ban as dead the
    /// moment it succeeds — and a ban's success is precisely what removes the
    /// target. Note this is NOT because absent targets are unknowable: the two
    /// ABSOLUTE grants in `is_ban_authorized` (banner is the owner; banner is an
    /// owner-appointed global moderator) read nothing about the target's position
    /// and so re-derive fine with the target gone, which is exactly why
    /// `Enforcing` is expressible here at all. Pinned in both directions by
    /// `legitimate_ancestor_ban_becomes_undetermined_once_its_target_is_removed`
    /// and `absolute_grants_stay_enforcing_with_the_target_absent`.
    enforcement: BanEnforcement,
}

/// Classify one ban against the converged `(members + member_info)` state.
///
/// Order matters: the two definitive-dead cases are settled before any authority
/// question, then a positive authority answer wins, and only a NEGATIVE authority
/// answer is split by whether the target is present (definitive) or absent
/// (contingent). That split is the whole point of the three states.
fn classify_ban(
    ban: &AuthorizedUserBan,
    members_by_id: &HashMap<MemberId, &AuthorizedMember>,
    member_info: &MemberInfoV1,
    owner_id: MemberId,
    owner_vk: &VerifyingKey,
) -> BanEnforcement {
    // A ban whose signature does not verify against the banner's CURRENT key
    // never excludes anyone: `banned_member_ids` skips it, and cleanup step 5
    // sweeps it. Settled before the authority question, which would otherwise
    // report a forged ban as enforcing.
    if !BansV1::ban_signature_matches_current_key(ban, members_by_id, owner_id, owner_vk) {
        return BanEnforcement::Inert;
    }
    let target = ban.ban.banned_user;
    // The owner is never a valid ban target (`is_ban_authorized` denies it
    // outright, before any grant). Permanently dead, not contingent: the owner
    // cannot be re-invited into a position that would make it apply, so this
    // must not fall through to `Undetermined` merely because the owner is not
    // in the members list.
    if target == owner_id {
        return BanEnforcement::Inert;
    }
    if MembersV1::is_ban_authorized(ban.banned_by, target, members_by_id, member_info, owner_id) {
        return BanEnforcement::Enforcing;
    }
    if members_by_id.contains_key(&target) {
        // Present AND unauthorized: the actionable #472 case. This person is in
        // the room and the ban is not stopping them.
        BanEnforcement::Inert
    } else {
        BanEnforcement::Undetermined
    }
}

/// Classify every ban in `room_state`, projecting each to a `BanInfo`. Kept as a
/// pure helper (no I/O) so the enforcement wiring is unit testable without a live
/// node — see the tests at the bottom of this file.
fn collect_ban_infos(room_state: &ChatRoomStateV1, owner_vk: &VerifyingKey) -> Vec<BanInfo> {
    let owner_id = MemberId::from(owner_vk);
    let members_by_id = room_state.members.members_by_member_id();
    room_state
        .bans
        .0
        .iter()
        .map(|ban| {
            let banned_at_secs = ban
                .ban
                .banned_at
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            BanInfo {
                banned_user_id: ban.ban.banned_user.to_string(),
                banned_by_id: ban.banned_by.to_string(),
                banned_at_secs,
                enforcement: classify_ban(
                    ban,
                    &members_by_id,
                    &room_state.member_info,
                    owner_id,
                    owner_vk,
                ),
            }
        })
        .collect()
}

/// Render the human-readable `debug bans` output as lines. Pure so the call-outs
/// are unit testable: this listing is the surface a moderator reads to decide
/// whether someone is still kept out, so a ban that is not doing that has to be
/// visibly distinct rather than silently blending in with the live ones.
fn ban_list_lines(bans: &[BanInfo]) -> Vec<String> {
    let count = |state: BanEnforcement| bans.iter().filter(|b| b.enforcement == state).count();
    let inert = count(BanEnforcement::Inert);
    let undetermined = count(BanEnforcement::Undetermined);

    let mut lines = vec![
        format!(
            "Ban List ({} bans: {} enforcing, {} not enforcing, {} undetermined)",
            bans.len(),
            count(BanEnforcement::Enforcing),
            inert,
            undetermined
        ),
        "=========".to_string(),
    ];

    if bans.is_empty() {
        lines.push("No bans.".to_string());
        return lines;
    }

    for ban in bans {
        lines.push(format!(
            "  {} banned by {} at {}{}",
            ban.banned_user_id,
            ban.banned_by_id,
            ban.banned_at_secs,
            ban.enforcement.marker()
        ));
    }

    // Each note is keyed to a state that is actually present, so the output does
    // not explain a case this room does not have. Neither claims where the target
    // is: a state fetched before cleanup (a full-state PUT bypasses it via
    // `verify`) can hold an inert ban whose target is already gone.
    if inert > 0 {
        lines.push(String::new());
        lines.push(format!(
            "Note: {} ban(s) marked NOT ENFORCING are not keeping their target \
             out. A ban lands here when its banner is no longer a member, when \
             the deputy authority it was issued under was revoked, or when the \
             banned user had themselves deputized the banner. Run `riverctl \
             member deputized-by <room> <banned_by_id>` to see whether the \
             banner still holds a grant, and from whom.",
            inert
        ));
    }

    if undetermined > 0 {
        lines.push(String::new());
        lines.push(format!(
            "Note: {} ban(s) marked UNDETERMINED target someone who is not \
             currently in the room. Their banner's authority came from a \
             position in the invite tree relative to that target, which cannot \
             be re-derived while the target is absent, so whether the ban \
             applies on their return depends on who re-invites them.",
            undetermined
        ));
    }

    lines
}

#[derive(Serialize)]
struct RoomStateSummary {
    room_name: String,
    member_count: usize,
    ban_count: usize,
    message_count: usize,
    max_user_bans: usize,
    max_members: usize,
    privacy_mode: String,
    configuration_version: u32,
    /// Number of `deputizer -> deputy` grants in the room (#410). Deputies are
    /// per-deputizer, so this counts grants, not "deputy members".
    deputy_grant_count: usize,
    /// Every grant, read from the CANONICAL `member_info` record per member so
    /// a revoked grant lingering in a duplicate record is not reported.
    deputy_grants: Vec<DeputyGrant>,
}

#[derive(Serialize)]
struct RoomConfig {
    room_name: String,
    /// Room description. `None` when unset; for a private room this is the
    /// sealed bytes rendered lossily (riverctl does not unseal here), mirroring
    /// `riverctl room config`'s no-flags display.
    description: Option<String>,
    privacy_mode: String,
    configuration_version: u32,
    max_recent_messages: usize,
    max_user_bans: usize,
    max_message_size: usize,
    max_nickname_size: usize,
    max_members: usize,
    max_room_name: usize,
    max_room_description: usize,
}

impl RoomConfig {
    /// Build the human/JSON view of a room's configuration. Mirrors the fields
    /// `riverctl room config` prints with no flags — including the description,
    /// which the dedicated `debug config` command previously omitted. The
    /// description is rendered lossily and is NOT unsealed for a private room
    /// (debug-only view), matching the no-flags `room config` path.
    fn from_configuration(config: &river_core::room_state::configuration::Configuration) -> Self {
        Self {
            room_name: config.display.name.to_string_lossy(),
            description: config
                .display
                .description
                .as_ref()
                .map(|d| d.to_string_lossy()),
            privacy_mode: format!("{:?}", config.privacy_mode),
            configuration_version: config.configuration_version,
            max_recent_messages: config.max_recent_messages,
            max_user_bans: config.max_user_bans,
            max_message_size: config.max_message_size,
            max_nickname_size: config.max_nickname_size,
            max_members: config.max_members,
            max_room_name: config.max_room_name,
            max_room_description: config.max_room_description,
        }
    }
}

/// Build the JSON summary emitted by `debug contract-get --format json`.
///
/// Mirrors the fields the human-readable branch prints, so a scripted
/// consumer gets the same information instead of the previous bare
/// `{"status":"success","contract_key":...}` stub.
fn contract_get_summary_json(
    contract_key: &str,
    configuration_version: u32,
    room_name: &str,
    member_count: usize,
    message_count: usize,
) -> serde_json::Value {
    serde_json::json!({
        "status": "success",
        "contract_key": contract_key,
        "configuration_version": configuration_version,
        "room_name": room_name,
        "member_count": member_count,
        "message_count": message_count,
    })
}

/// Build the `{"status":"error", ...}` JSON emitted by debug subcommands on
/// failure. Routing the message through serde keeps the output valid JSON even
/// when the error text contains a quote, backslash, or newline — the old
/// hand-rolled `format!` interpolation could emit malformed JSON.
fn status_error_json(message: &str) -> serde_json::Value {
    serde_json::json!({ "status": "error", "message": message })
}

pub async fn execute(command: DebugCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        DebugCommands::ContractGet { room_owner_key } => {
            // Decode the room owner key from base58
            let decoded = bs58::decode(&room_owner_key)
                .into_vec()
                .map_err(|e| anyhow!("Failed to decode room owner key: {}", e))?;

            if decoded.len() != 32 {
                return Err(anyhow!(
                    "Invalid room owner key length: expected 32 bytes, got {}",
                    decoded.len()
                ));
            }

            let mut key_bytes = [0u8; 32];
            key_bytes.copy_from_slice(&decoded);
            let owner_vk = VerifyingKey::from_bytes(&key_bytes)
                .map_err(|e| anyhow!("Invalid verifying key: {}", e))?;

            let contract_key = api.owner_vk_to_contract_key(&owner_vk);

            if !matches!(format, OutputFormat::Json) {
                eprintln!(
                    "DEBUG: Performing contract GET for room owned by: {}",
                    room_owner_key
                );
                eprintln!("Contract key: {}", contract_key.id());
            }

            match api.get_room(&owner_vk, false).await {
                Ok(room_state) => {
                    match format {
                        OutputFormat::Human => {
                            println!("✓ Successfully retrieved room state");
                            println!(
                                "Configuration version: {}",
                                room_state.configuration.configuration.configuration_version
                            );
                            println!(
                                "Room name: {}",
                                room_state
                                    .configuration
                                    .configuration
                                    .display
                                    .name
                                    .to_string_lossy()
                            );
                            println!("Members: {}", room_state.members.members.len());
                            println!("Messages: {}", room_state.recent_messages.messages.len());
                        }
                        OutputFormat::Json => {
                            let cfg = &room_state.configuration.configuration;
                            let json = contract_get_summary_json(
                                &contract_key.id().to_string(),
                                cfg.configuration_version,
                                &cfg.display.name.to_string_lossy(),
                                room_state.members.members.len(),
                                room_state.recent_messages.messages.len(),
                            );
                            println!("{}", serde_json::to_string_pretty(&json)?);
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    match format {
                        OutputFormat::Human => eprintln!("✗ Contract GET failed: {}", e),
                        OutputFormat::Json => {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&status_error_json(&e.to_string()))?
                            );
                        }
                    }
                    Err(e)
                }
            }
        }
        DebugCommands::Websocket => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("DEBUG: Testing WebSocket connection...");
            }

            match api.test_connection().await {
                Ok(()) => {
                    match format {
                        OutputFormat::Human => println!("✓ WebSocket connection successful"),
                        OutputFormat::Json => {
                            println!(
                                r#"{{"status": "success", "message": "WebSocket connection successful"}}"#
                            );
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    match format {
                        OutputFormat::Human => eprintln!("✗ WebSocket connection failed: {}", e),
                        OutputFormat::Json => {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&status_error_json(&e.to_string()))?
                            );
                        }
                    }
                    Err(e)
                }
            }
        }
        DebugCommands::ContractKey { room_owner_key } => {
            // Decode the room owner key from base58
            let decoded = bs58::decode(&room_owner_key)
                .into_vec()
                .map_err(|e| anyhow!("Failed to decode room owner key: {}", e))?;

            if decoded.len() != 32 {
                return Err(anyhow!(
                    "Invalid room owner key length: expected 32 bytes, got {}",
                    decoded.len()
                ));
            }

            let mut key_bytes = [0u8; 32];
            key_bytes.copy_from_slice(&decoded);
            let owner_vk = VerifyingKey::from_bytes(&key_bytes)
                .map_err(|e| anyhow!("Invalid verifying key: {}", e))?;

            let contract_key = api.owner_vk_to_contract_key(&owner_vk);

            match format {
                OutputFormat::Human => {
                    println!("Room owner key: {}", room_owner_key);
                    println!("Contract key: {}", contract_key.id());
                }
                OutputFormat::Json => {
                    println!(
                        r#"{{"room_owner_key": "{}", "contract_key": "{}"}}"#,
                        room_owner_key,
                        contract_key.id()
                    );
                }
            }
            Ok(())
        }
        DebugCommands::RoomState { room_owner_key } => {
            let owner_vk = parse_owner_key(&room_owner_key)?;
            let mut room_state = api.get_room(&owner_vk, false).await?;
            // Private-room nicknames are sealed; collect the local member's
            // secrets so deputy grants render names rather than ciphertext.
            let secrets = api.room_display_secrets(&owner_vk, &mut room_state);
            let deputy_grants = RoomDeputies::new(&room_state, &owner_vk, &secrets).all_grants();

            let config = &room_state.configuration.configuration;
            let summary = RoomStateSummary {
                room_name: config.display.name.to_string_lossy(),
                member_count: room_state.members.members.len(),
                ban_count: room_state.bans.0.len(),
                message_count: room_state.recent_messages.messages.len(),
                max_user_bans: config.max_user_bans,
                max_members: config.max_members,
                privacy_mode: format!("{:?}", config.privacy_mode),
                configuration_version: config.configuration_version,
                deputy_grant_count: deputy_grants.len(),
                deputy_grants,
            };

            match format {
                OutputFormat::Human => {
                    println!("Room State Summary");
                    println!("==================");
                    println!("Room name: {}", summary.room_name);
                    println!("Privacy mode: {}", summary.privacy_mode);
                    println!("Config version: {}", summary.configuration_version);
                    println!();
                    println!(
                        "Members: {} / {}",
                        summary.member_count, summary.max_members
                    );
                    println!("Bans: {} / {}", summary.ban_count, summary.max_user_bans);
                    println!("Messages: {}", summary.message_count);
                    println!("Deputy grants: {}", summary.deputy_grant_count);
                    for grant in &summary.deputy_grants {
                        println!(
                            "  {} -> {}  {}",
                            party_label(&grant.deputizer),
                            party_label(&grant.deputy),
                            grant_status_line(grant)
                        );
                    }
                }
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&summary)?);
                }
            }
            Ok(())
        }
        DebugCommands::Bans { room_owner_key } => {
            let owner_vk = parse_owner_key(&room_owner_key)?;
            let room_state = api.get_room(&owner_vk, false).await?;

            let bans = collect_ban_infos(&room_state, &owner_vk);

            match format {
                OutputFormat::Human => {
                    for line in ban_list_lines(&bans) {
                        println!("{}", line);
                    }
                }
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&bans)?);
                }
            }
            Ok(())
        }
        DebugCommands::Config { room_owner_key } => {
            let owner_vk = parse_owner_key(&room_owner_key)?;
            let room_state = api.get_room(&owner_vk, false).await?;

            let config = &room_state.configuration.configuration;
            let room_config = RoomConfig::from_configuration(config);

            match format {
                OutputFormat::Human => {
                    println!("Room Configuration");
                    println!("==================");
                    println!("Room name: {}", room_config.room_name);
                    println!(
                        "Description: {}",
                        room_config.description.as_deref().unwrap_or("(none)")
                    );
                    println!("Privacy mode: {}", room_config.privacy_mode);
                    println!("Config version: {}", room_config.configuration_version);
                    println!();
                    println!("Limits:");
                    println!("  max_members: {}", room_config.max_members);
                    println!("  max_user_bans: {}", room_config.max_user_bans);
                    println!("  max_recent_messages: {}", room_config.max_recent_messages);
                    println!("  max_message_size: {}", room_config.max_message_size);
                    println!("  max_nickname_size: {}", room_config.max_nickname_size);
                    println!("  max_room_name: {}", room_config.max_room_name);
                    println!(
                        "  max_room_description: {}",
                        room_config.max_room_description
                    );
                }
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&room_config)?);
                }
            }
            Ok(())
        }
    }
}

/// Helper to parse a base58-encoded room owner key
fn parse_owner_key(room_owner_key: &str) -> Result<VerifyingKey> {
    let decoded = bs58::decode(room_owner_key)
        .into_vec()
        .map_err(|e| anyhow!("Failed to decode room owner key: {}", e))?;

    if decoded.len() != 32 {
        return Err(anyhow!(
            "Invalid room owner key length: expected 32 bytes, got {}",
            decoded.len()
        ));
    }

    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&decoded);
    VerifyingKey::from_bytes(&key_bytes).map_err(|e| anyhow!("Invalid verifying key: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_get_summary_json_includes_room_fields() {
        // Regression guard: the JSON branch of `debug contract-get` used to
        // emit only `{"status":"success","contract_key":...}` (a TODO stub),
        // dropping every field the human branch prints. Ensure the summary
        // now carries them.
        let json = contract_get_summary_json("CONTRACTKEY123", 7, "My Room", 3, 42);
        assert_eq!(json["status"], "success");
        assert_eq!(json["contract_key"], "CONTRACTKEY123");
        assert_eq!(json["configuration_version"], 7);
        assert_eq!(json["room_name"], "My Room");
        assert_eq!(json["member_count"], 3);
        assert_eq!(json["message_count"], 42);
    }

    #[test]
    fn room_config_includes_description() {
        // Regression guard: `debug config` used to print the room name and
        // every limit but silently drop the description, even though it is the
        // command literally named "Show room configuration". Ensure the
        // description survives both the struct and the JSON projection.
        use river_core::room_state::configuration::Configuration;
        use river_core::room_state::privacy::RoomDisplayMetadata;

        let mut config = Configuration::default();
        config.display =
            RoomDisplayMetadata::public("My Room".to_string(), Some("Hello **world**".to_string()));

        let rc = RoomConfig::from_configuration(&config);
        assert_eq!(rc.room_name, "My Room");
        assert_eq!(rc.description.as_deref(), Some("Hello **world**"));

        let json = serde_json::to_value(&rc).unwrap();
        assert_eq!(json["description"], "Hello **world**");
    }

    #[test]
    fn room_config_description_none_when_unset() {
        // A room with no description must serialize `description: null` (and
        // render "(none)" in the human branch via `unwrap_or`), not panic or
        // omit the field.
        use river_core::room_state::configuration::Configuration;

        let config = Configuration::default();
        let rc = RoomConfig::from_configuration(&config);
        assert_eq!(rc.description, None);

        let json = serde_json::to_value(&rc).unwrap();
        assert!(json["description"].is_null());
    }

    #[test]
    fn status_error_json_stays_valid_with_special_chars() {
        // Regression guard: the JSON error arms used to hand-roll output via
        // `format!`, producing invalid JSON when the error text contained a
        // quote, backslash, or newline. serde must escape it so the payload
        // round-trips through a strict parser.
        let raw = "decode failed for \"weird\\input\"\nsecond line";
        let value = status_error_json(raw);
        let serialized = serde_json::to_string(&value).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["status"], "error");
        assert_eq!(parsed["message"], raw);
    }

    // --- `debug bans` enforcement reporting (#472) ------------------------
    //
    // A ban can sit in room state while the contract no longer enforces it
    // (deputy ban authority, #410). These pin that `debug bans` distinguishes
    // the two, since a moderator reads this list to decide whether someone is
    // actually kept out.

    use ed25519_dalek::SigningKey;
    use river_core::room_state::ban::UserBan;
    use river_core::room_state::member::Member;
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use river_core::room_state::ChatRoomParametersV1;

    /// The single ban's verdict in a state built by these fixtures.
    fn verdict(state: &ChatRoomStateV1, owner: &SigningKey) -> BanEnforcement {
        let infos = collect_ban_infos(state, &owner.verifying_key());
        assert_eq!(infos.len(), 1, "fixture must carry exactly one ban");
        infos[0].enforcement
    }

    /// The cli crate's dalek build does not enable the `rand` `generate`
    /// helper, so keys come from fixed seeds (mirrors `deputies.rs`'s tests).
    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn id(sk: &SigningKey) -> MemberId {
        sk.verifying_key().into()
    }

    /// Add `sk` as a member invited by `inviter`, signed by the inviter as
    /// `AuthorizedMember::new` asserts.
    fn push_member(
        state: &mut ChatRoomStateV1,
        owner: &SigningKey,
        inviter: &SigningKey,
        sk: &SigningKey,
    ) {
        let member = Member {
            owner_member_id: id(owner),
            invited_by: id(inviter),
            member_vk: sk.verifying_key(),
        };
        state
            .members
            .members
            .push(AuthorizedMember::new(member, inviter));
    }

    /// Push a signed `member_info` record for `sk` at `version` granting
    /// `deputies`. A later version supersedes an earlier one via `canonical`,
    /// which is how a revocation is represented.
    fn push_info(
        state: &mut ChatRoomStateV1,
        sk: &SigningKey,
        version: u32,
        deputies: Vec<MemberId>,
    ) {
        let mut info = MemberInfo::new_public(id(sk), version, "member".to_string());
        info.deputies = deputies;
        state
            .member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(info, sk));
    }

    /// A ban of `target` issued and signed by `banner`.
    fn push_ban(
        state: &mut ChatRoomStateV1,
        owner: &SigningKey,
        banner: &SigningKey,
        target: &SigningKey,
    ) {
        let ban = UserBan {
            owner_member_id: id(owner),
            banned_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            banned_user: id(target),
        };
        state
            .bans
            .0
            .push(AuthorizedUserBan::new(ban, id(banner), banner));
    }

    #[test]
    fn owner_ban_of_a_present_member_is_enforcing() {
        // Positive baseline. Without it, the Inert cases below would pass even
        // against a classifier that returned Inert unconditionally.
        let owner = key(1);
        let alice = key(2);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_ban(&mut state, &owner, &owner, &alice);

        let bans = collect_ban_infos(&state, &owner.verifying_key());
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].banned_user_id, id(&alice).to_string());
        assert_eq!(bans[0].banned_by_id, id(&owner).to_string());
        assert_eq!(bans[0].enforcement, BanEnforcement::Enforcing);
    }

    #[test]
    fn deputy_ban_is_enforcing_while_the_grant_stands() {
        // The other half of the revocation pair below: the SAME ban, differing
        // only in whether the owner's deputy grant is still present.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &owner, &bob);
        push_info(&mut state, &owner, 0, vec![id(&bob)]);
        push_ban(&mut state, &owner, &bob, &alice);

        assert_eq!(verdict(&state, &owner), BanEnforcement::Enforcing);
    }

    #[test]
    fn deputy_ban_is_inert_after_the_grant_is_revoked() {
        // The issue's headline case (#472): the owner revokes bob's deputy
        // authority by publishing a later `member_info` record without him. The
        // ban stays in state, alice stays in the room, and `debug bans` used to
        // present this identically to a live ban.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &owner, &bob);
        push_info(&mut state, &owner, 0, vec![id(&bob)]);
        push_ban(&mut state, &owner, &bob, &alice);
        // Revocation: a higher-version record wins in `canonical`.
        push_info(&mut state, &owner, 1, vec![]);

        assert_eq!(state.bans.0.len(), 1, "the ban is still stored in state");
        assert_eq!(verdict(&state, &owner), BanEnforcement::Inert);
    }

    #[test]
    fn ban_is_inert_once_its_banner_is_no_longer_a_member() {
        // Second dead path from #472: the banner left or was pruned, so the
        // deputy-derived grants no longer apply. Alice stays a member, so this
        // is the definitive case rather than the contingent one.
        let owner = key(1);
        let alice = key(2);
        let carol = key(4);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &owner, &carol);
        push_info(&mut state, &owner, 0, vec![id(&carol)]);
        push_ban(&mut state, &owner, &carol, &alice);

        assert_eq!(
            verdict(&state, &owner),
            BanEnforcement::Enforcing,
            "precondition: enforcing while carol is present"
        );

        state
            .members
            .members
            .retain(|m| m.member.id() != id(&carol));

        assert_eq!(verdict(&state, &owner), BanEnforcement::Inert);
    }

    #[test]
    fn legitimate_ancestor_ban_becomes_undetermined_once_its_target_is_removed() {
        // Guards against collapsing the classifier back to a bare
        // `is_ban_authorized` with a two-valued verdict.
        //
        // Bob's authority over alice came from being her strict ancestor. Once
        // the ban does its job alice is gone, there is no chain left to walk,
        // and `is_ban_authorized` returns false — indistinguishable from "never
        // had authority". Reading that false as "this ban is dead" would mark
        // every working strict-ancestor and non-owner-deputy ban NOT ENFORCING,
        // since a ban's success is exactly what removes its target.
        //
        // The honest verdict is Undetermined: bob's grant re-derives only if
        // alice comes back under him. Pair with
        // `absolute_grants_stay_enforcing_with_the_target_absent`, which stops
        // the opposite shortcut of calling every absent target Undetermined.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);

        // owner -> bob -> alice, and bob bans alice as her strict ancestor.
        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &bob);
        push_member(&mut state, &owner, &bob, &alice);
        push_ban(&mut state, &owner, &bob, &alice);

        assert_eq!(
            verdict(&state, &owner),
            BanEnforcement::Enforcing,
            "precondition: an ancestor's ban of a present member enforces"
        );

        // Let the contract enforce it, exactly as `post_apply_cleanup` does.
        let params = ChatRoomParametersV1 {
            owner: owner.verifying_key(),
        };
        let excluded = state
            .members
            .banned_member_ids(&state.bans, &state.member_info, &params);
        state
            .members
            .members
            .retain(|m| !excluded.contains(&m.member.id()));
        assert!(
            !state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == id(&alice)),
            "precondition: the ban removed alice"
        );

        assert_eq!(verdict(&state, &owner), BanEnforcement::Undetermined);
    }

    #[test]
    fn absolute_grants_stay_enforcing_with_the_target_absent() {
        // The counterweight to the test above, and the reason three states are
        // worth building rather than collapsing every absent target into
        // Undetermined. Neither absolute grant reads the target's invite chain:
        // `banner == owner_id` returns before the chain walk, and the
        // owner-appointed global moderator check reads only the owner's deputy
        // list. Both therefore re-derive with the target gone and re-apply
        // wherever they reattach, so both are real promises.
        let owner = key(1);
        let alice = key(2); // absent target
        let gmod = key(4);

        // Owner's own ban of an absent alice.
        let mut by_owner = ChatRoomStateV1::default();
        push_member(&mut by_owner, &owner, &owner, &gmod);
        push_ban(&mut by_owner, &owner, &owner, &alice);
        assert_eq!(
            verdict(&by_owner, &owner),
            BanEnforcement::Enforcing,
            "the owner's grant is absolute and target-independent"
        );

        // Owner-appointed global moderator's ban of an absent alice.
        let mut by_gmod = ChatRoomStateV1::default();
        push_member(&mut by_gmod, &owner, &owner, &gmod);
        push_info(&mut by_gmod, &owner, 0, vec![id(&gmod)]);
        push_ban(&mut by_gmod, &owner, &gmod, &alice);
        assert_eq!(
            verdict(&by_gmod, &owner),
            BanEnforcement::Enforcing,
            "an owner-appointed global moderator's grant is absolute too"
        );
    }

    #[test]
    fn ban_signed_by_someone_other_than_its_attributed_banner_is_inert() {
        // `verify` skips a ban's signature when the banner is absent at
        // bans-apply time (bans apply before members), so a state can carry a
        // ban attributed to a current, fully-authorized member but signed by
        // someone else. `banned_member_ids` re-checks the signature against the
        // banner's CURRENT key and refuses to enforce it (#411 round 4 A).
        //
        // This must be settled BEFORE the authority question: bob here is an
        // owner-appointed global moderator, so `is_ban_authorized` says yes and
        // the classifier would otherwise report a forged ban as Enforcing.
        use river_core::util::sign_struct;

        let owner = key(1);
        let alice = key(2);
        let bob = key(3); // attributed banner: a real member WITH a grant
        let mallory = key(7); // who actually signed it

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &owner, &bob);
        push_info(&mut state, &owner, 0, vec![id(&bob)]);

        let ban = UserBan {
            owner_member_id: id(&owner),
            banned_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            banned_user: id(&alice),
        };
        let forged = sign_struct(&ban, &mallory);
        state
            .bans
            .0
            .push(AuthorizedUserBan::with_signature(ban, id(&bob), forged));

        // The contract does not exclude alice on the strength of this ban.
        let params = ChatRoomParametersV1 {
            owner: owner.verifying_key(),
        };
        assert!(
            !state
                .members
                .banned_member_ids(&state.bans, &state.member_info, &params)
                .contains(&id(&alice)),
            "precondition: the contract refuses to enforce a mis-signed ban"
        );

        assert_eq!(verdict(&state, &owner), BanEnforcement::Inert);
    }

    #[test]
    fn ban_naming_the_room_owner_is_inert_not_undetermined() {
        // `is_ban_authorized` denies `target == owner_id` outright, before any
        // grant, and the owner is not in the members list. Without the explicit
        // guard this would fall through the absent-target branch and render
        // UNDETERMINED forever, implying it might apply if the owner "came
        // back". It never can, so it is definitively dead.
        let owner = key(1);
        let bob = key(3);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &bob);
        push_ban(&mut state, &owner, &bob, &owner);

        assert_eq!(verdict(&state, &owner), BanEnforcement::Inert);
    }

    #[test]
    fn a_present_target_agrees_with_the_contracts_excluded_set() {
        // For a PRESENT target the verdict is definitive, so it can be tied to
        // the contract's own excluded set rather than to a predicate that merely
        // looks related. Each state holds exactly one ban, so `banned_member_ids`
        // containing the target is precisely "this ban excludes its target".
        // Undetermined cannot occur here and is asserted absent.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);

        let mut cases: Vec<(&str, ChatRoomStateV1)> = Vec::new();

        let mut owner_bans = ChatRoomStateV1::default();
        push_member(&mut owner_bans, &owner, &owner, &alice);
        push_ban(&mut owner_bans, &owner, &owner, &alice);
        cases.push(("owner bans a present member", owner_bans));

        let mut deputy_bans = ChatRoomStateV1::default();
        push_member(&mut deputy_bans, &owner, &owner, &alice);
        push_member(&mut deputy_bans, &owner, &owner, &bob);
        push_info(&mut deputy_bans, &owner, 0, vec![id(&bob)]);
        push_ban(&mut deputy_bans, &owner, &bob, &alice);
        cases.push(("current deputy bans a present member", deputy_bans));

        let mut revoked_bans = ChatRoomStateV1::default();
        push_member(&mut revoked_bans, &owner, &owner, &alice);
        push_member(&mut revoked_bans, &owner, &owner, &bob);
        push_info(&mut revoked_bans, &owner, 0, vec![id(&bob)]);
        push_ban(&mut revoked_bans, &owner, &bob, &alice);
        push_info(&mut revoked_bans, &owner, 1, vec![]);
        cases.push(("revoked deputy bans a present member", revoked_bans));

        let mut stranger_bans = ChatRoomStateV1::default();
        push_member(&mut stranger_bans, &owner, &owner, &alice);
        push_member(&mut stranger_bans, &owner, &owner, &bob);
        push_ban(&mut stranger_bans, &owner, &bob, &alice);
        cases.push((
            "member with no authority bans a present member",
            stranger_bans,
        ));

        let params = ChatRoomParametersV1 {
            owner: owner.verifying_key(),
        };

        let mut saw_enforcing = false;
        let mut saw_inert = false;

        for (label, state) in &cases {
            let reported = verdict(state, &owner);
            let target = state.bans.0[0].ban.banned_user;
            assert!(
                state
                    .members
                    .members
                    .iter()
                    .any(|m| m.member.id() == target),
                "this equivalence only holds for a PRESENT target: {label}"
            );
            assert_ne!(
                reported,
                BanEnforcement::Undetermined,
                "a present target is never contingent: {label}"
            );

            let actually_excluded = state
                .members
                .banned_member_ids(&state.bans, &state.member_info, &params)
                .contains(&target);

            assert_eq!(
                reported == BanEnforcement::Enforcing,
                actually_excluded,
                "`debug bans` disagrees with the contract's excluded set for: {label}"
            );

            saw_enforcing |= reported == BanEnforcement::Enforcing;
            saw_inert |= reported == BanEnforcement::Inert;
        }

        // Guard against a vacuous pass if every case landed on one side.
        assert!(saw_enforcing && saw_inert, "cases must cover both outcomes");
    }

    #[test]
    fn each_ban_is_classified_independently() {
        // `collect_ban_infos` maps over the ban list, so a classifier hoisted
        // out of the closure, or one verdict stamped onto every entry, would
        // pass every single-ban test above. A room holding a mix is #472's
        // actual scenario, and the order must line up with the bans as stored.
        let owner = key(1);
        let alice = key(2); // present, banned by a revoked deputy -> Inert
        let bob = key(3); // the revoked deputy
        let dave = key(5); // absent, banned by bob as a plain member -> Undetermined
        let erin = key(6); // present, banned by the owner -> Enforcing

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &owner, &bob);
        push_member(&mut state, &owner, &owner, &erin);
        // bob held a grant when he banned alice; it is revoked below.
        push_info(&mut state, &owner, 0, vec![id(&bob)]);

        push_ban(&mut state, &owner, &bob, &alice); // -> Inert after revocation
        push_ban(&mut state, &owner, &bob, &dave); // -> Undetermined (dave absent)
        push_ban(&mut state, &owner, &owner, &erin); // -> Enforcing

        push_info(&mut state, &owner, 1, vec![]); // revoke bob

        let bans = collect_ban_infos(&state, &owner.verifying_key());
        assert_eq!(bans.len(), 3);

        let by_target: Vec<(String, BanEnforcement)> = bans
            .iter()
            .map(|b| (b.banned_user_id.clone(), b.enforcement))
            .collect();

        assert_eq!(
            by_target,
            vec![
                (id(&alice).to_string(), BanEnforcement::Inert),
                (id(&dave).to_string(), BanEnforcement::Undetermined),
                (id(&erin).to_string(), BanEnforcement::Enforcing),
            ],
            "each ban must carry its OWN verdict, in stored order"
        );
    }

    #[test]
    fn ban_json_keeps_the_pre_existing_fields_and_serializes_state_as_a_string() {
        // `banned_user_id` / `banned_by_id` / `banned_at_secs` are the published
        // shape and must not move. `enforcement` REPLACES the short-lived
        // `enforcing` bool (#472): the key is renamed deliberately so the
        // breaking change is visible to a consumer rather than a bool silently
        // becoming a string under the same name.
        let owner = key(1);
        let alice = key(2);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_ban(&mut state, &owner, &owner, &alice);

        let bans = collect_ban_infos(&state, &owner.verifying_key());
        let json = serde_json::to_value(&bans).unwrap();
        let entry = &json[0];

        assert_eq!(entry["banned_user_id"], id(&alice).to_string());
        assert_eq!(entry["banned_by_id"], id(&owner).to_string());
        assert_eq!(entry["banned_at_secs"], 1_700_000_000u64);
        assert_eq!(entry["enforcement"], "enforcing");
        assert!(
            entry.get("enforcing").is_none(),
            "the old bool key must be gone, not shadowing the new field"
        );
    }

    #[test]
    fn every_enforcement_state_has_a_distinct_json_spelling() {
        // Pins the wire spelling of all three variants. A `rename_all` change or
        // a variant rename is a breaking change for `-f json` consumers and must
        // fail here rather than in someone's script.
        let spell = |e: BanEnforcement| serde_json::to_value(e).unwrap();
        assert_eq!(spell(BanEnforcement::Inert), "inert");
        assert_eq!(spell(BanEnforcement::Enforcing), "enforcing");
        assert_eq!(spell(BanEnforcement::Undetermined), "undetermined");
    }

    fn info(user: &str, state: BanEnforcement) -> BanInfo {
        BanInfo {
            banned_user_id: user.to_string(),
            banned_by_id: "BANNER".to_string(),
            banned_at_secs: 100,
            enforcement: state,
        }
    }

    #[test]
    fn human_output_marks_each_state_distinctly() {
        // The human branch is the surface the issue is about: a ban that is not
        // keeping anyone out must not render identically to one that is, and the
        // contingent case must not render as either.
        let lines = ban_list_lines(&[
            info("LIVEUSER", BanEnforcement::Enforcing),
            info("INERTUSER", BanEnforcement::Inert),
            info("MAYBEUSER", BanEnforcement::Undetermined),
        ]);
        let line_for = |u: &str| {
            lines
                .iter()
                .find(|l| l.contains(u))
                .unwrap_or_else(|| panic!("{u} must still be listed"))
                .clone()
        };

        let live = line_for("LIVEUSER");
        assert!(
            !live.contains("NOT ENFORCING") && !live.contains("UNDETERMINED"),
            "an enforcing ban must be unmarked: {live}"
        );
        assert!(
            line_for("INERTUSER").contains("[NOT ENFORCING]"),
            "an inert ban must be visibly flagged"
        );
        let maybe = line_for("MAYBEUSER");
        assert!(
            maybe.contains("[UNDETERMINED]") && !maybe.contains("NOT ENFORCING"),
            "a contingent ban needs its own marker, not the inert one: {maybe}"
        );

        let rendered = lines.join("\n");
        assert!(
            rendered.contains("3 bans: 1 enforcing, 1 not enforcing, 1 undetermined"),
            "the header must break the count down by state: {rendered}"
        );
        assert!(
            rendered.contains("member deputized-by <room> <banned_by_id>"),
            "the inert note must point at the BANNER's grant, which is the party \
             whose authority decides whether the ban still applies"
        );
        assert!(
            rendered.contains("depends on who re-invites them"),
            "the undetermined note must explain what it is contingent on"
        );
        assert!(
            !rendered.contains("are in the room"),
            "no note may claim where the target is: an uncleaned full-state PUT \
             can hold an inert ban whose target is already gone"
        );
    }

    #[test]
    fn human_output_only_explains_states_that_are_present() {
        // No false alarms. A room whose bans are all live must carry neither
        // note, or moderators learn to ignore both.
        let rendered = ban_list_lines(&[info("LIVEUSER", BanEnforcement::Enforcing)]).join("\n");
        assert!(rendered.contains("1 bans: 1 enforcing, 0 not enforcing, 0 undetermined"));
        assert!(!rendered.contains("NOT ENFORCING"));
        assert!(!rendered.contains("Note:"));

        // A room with only contingent bans gets the undetermined note and NOT
        // the inert one, which would otherwise send the reader chasing a
        // revoked grant that does not exist.
        let only_maybe =
            ban_list_lines(&[info("MAYBEUSER", BanEnforcement::Undetermined)]).join("\n");
        assert!(only_maybe.contains("UNDETERMINED"));
        assert!(only_maybe.contains("depends on who re-invites them"));
        assert!(
            !only_maybe.contains("member deputized-by"),
            "the inert remediation must not appear when no ban is inert"
        );
    }

    #[test]
    fn human_output_handles_an_empty_ban_list() {
        let rendered = ban_list_lines(&[]).join("\n");
        assert!(rendered.contains("0 bans: 0 enforcing, 0 not enforcing, 0 undetermined"));
        assert!(rendered.contains("No bans."));
        assert!(!rendered.contains("Note:"));
    }

    /// Source pin for the `debug bans` CALL SITE.
    ///
    /// Every other test here exercises `collect_ban_infos` and `ban_list_lines`
    /// directly, so all of them stay green if the `Bans` arm stops calling them
    /// — which is exactly how this feature shipped broken once already: the
    /// salvaged first draft added the helper and left the arm building `BanInfo`
    /// inline. A behavioural test would need a live node, so this scrapes the
    /// source, in the idiom already used by `storage.rs` and `identity.rs`.
    ///
    /// The body is cut at `mod tests` so these assertions' own text cannot
    /// satisfy them via `include_str!`.
    #[test]
    fn bans_command_delegates_to_the_shared_helpers() {
        let src = include_str!("debug.rs");
        let body = &src[..src.find("mod tests").unwrap_or(src.len())];

        let arm = body
            .find("DebugCommands::Bans { room_owner_key }")
            .expect("the `debug bans` arm must exist");
        let next_arm = body[arm..]
            .find("DebugCommands::Config")
            .map(|i| arm + i)
            .unwrap_or(body.len());
        let arm_src = &body[arm..next_arm];

        assert!(
            arm_src.contains("collect_ban_infos("),
            "the Bans arm must classify via `collect_ban_infos`. Building \
             `BanInfo` inline here is how the enforcement field shipped \
             unpopulated the first time (freenet/river#472)."
        );
        assert!(
            arm_src.contains("ban_list_lines("),
            "the Bans arm's human branch must render via `ban_list_lines`, or \
             the state markers this feature exists for are tested but never \
             printed."
        );
        assert!(
            arm_src.contains("serde_json::to_string_pretty(&bans)"),
            "the JSON branch must serialize the SAME classified `bans` value, \
             so `-f json` and the human listing cannot disagree."
        );
        assert!(
            !arm_src.contains("BanInfo {"),
            "the Bans arm must not construct `BanInfo` itself — that bypasses \
             `classify_ban` and can reintroduce a hardcoded verdict."
        );
    }
}
