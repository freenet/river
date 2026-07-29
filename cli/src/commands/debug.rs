use crate::api::ApiClient;
use crate::deputies::{grant_status_line, party_label, DeputyGrant, RoomDeputies};
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::ban::BansV1;
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomStateV1;
use serde::Serialize;

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

#[derive(Serialize)]
struct BanInfo {
    banned_user_id: String,
    banned_by_id: String,
    banned_at_secs: u64,
    /// Whether this ban is currently keeping its target out of the room, as
    /// opposed to sitting in state while the target walks around inside. Since
    /// deputy ban authority (freenet/river#410) a ban can go inert without being
    /// removed: the banner left or was pruned, their deputy authority was
    /// revoked, or (once no absolute grant applies) the target has since
    /// deputized the banner.
    ///
    /// `false` is the case worth acting on, and it is unambiguous: the target is
    /// a CURRENT member that this ban fails to exclude, so that person is in the
    /// room right now.
    ///
    /// Scope, deliberately not overclaimed: for a target who is already gone this
    /// reports `true` without re-deriving the banner's authority, because an
    /// absent member has no invite chain to walk and `is_ban_authorized` cannot
    /// be evaluated against a hypothetical future one. So `true` means "not in
    /// the room", not "guaranteed to hold if they are re-invited by someone new".
    /// Do NOT switch this to a bare `is_ban_authorized` to tighten that: every
    /// successfully enforced ban ends with its target removed, so doing so
    /// reports NOT ENFORCING for essentially every ban that is working. Pinned by
    /// `legitimate_ancestor_ban_still_enforcing_after_its_target_is_removed`.
    enforcing: bool,
}

/// Classify every ban in `room_state` as enforcing or inert, projecting each to
/// a `BanInfo`. Kept as a pure helper (no I/O) so the enforcement wiring is unit
/// testable without a live node — see the tests at the bottom of this file.
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
            let enforcing = BansV1::ban_is_enforcing(
                ban,
                &members_by_id,
                &room_state.member_info,
                owner_id,
                owner_vk,
            );
            BanInfo {
                banned_user_id: ban.ban.banned_user.to_string(),
                banned_by_id: ban.banned_by.to_string(),
                banned_at_secs,
                enforcing,
            }
        })
        .collect()
}

/// Render the human-readable `debug bans` output as lines. Pure so the
/// non-enforcing call-out is unit testable: this listing is the surface a
/// moderator reads to decide whether someone is still kept out, so a ban that
/// no longer enforces has to be visibly distinct rather than silently blending
/// in with the live ones.
fn ban_list_lines(bans: &[BanInfo]) -> Vec<String> {
    let enforcing_count = bans.iter().filter(|b| b.enforcing).count();
    let inert_count = bans.len() - enforcing_count;

    let mut lines = vec![
        format!(
            "Ban List ({} bans, {} enforcing)",
            bans.len(),
            enforcing_count
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
            if ban.enforcing {
                ""
            } else {
                "  [NOT ENFORCING]"
            }
        ));
    }

    if inert_count > 0 {
        lines.push(String::new());
        lines.push(format!(
            "Note: {} ban(s) are stored in room state but are NOT keeping their \
             target out, so those users are in the room. A ban stops enforcing \
             when its banner leaves the room or loses the deputy authority it \
             banned under. Check the banner with `riverctl member deputized-by \
             <room> <banned_by_id>`.",
            inert_count
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
    use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
    use river_core::room_state::member::{AuthorizedMember, Member};
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use river_core::room_state::ChatRoomParametersV1;

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
    fn owner_ban_of_a_present_member_reports_enforcing() {
        // Baseline for the negative cases below: without this, a test asserting
        // `!enforcing` would pass even if `enforcing` were hardcoded to false.
        let owner = key(1);
        let alice = key(2);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_ban(&mut state, &owner, &owner, &alice);

        let bans = collect_ban_infos(&state, &owner.verifying_key());
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].banned_user_id, id(&alice).to_string());
        assert_eq!(bans[0].banned_by_id, id(&owner).to_string());
        assert!(
            bans[0].enforcing,
            "an owner's ban of a current member is authorized, so it must report as enforcing"
        );
    }

    #[test]
    fn deputy_ban_reports_enforcing_while_the_grant_stands() {
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

        let bans = collect_ban_infos(&state, &owner.verifying_key());
        assert!(
            bans[0].enforcing,
            "bob is a current owner-appointed deputy, so his ban enforces"
        );
    }

    #[test]
    fn deputy_ban_reports_not_enforcing_after_the_grant_is_revoked() {
        // The issue's headline case (#472): the owner revokes bob's deputy
        // authority by publishing a later `member_info` record without him.
        // The ban stays in state but the contract stops excluding alice, and
        // `debug bans` used to present it identically to a live ban.
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

        let bans = collect_ban_infos(&state, &owner.verifying_key());
        assert_eq!(bans.len(), 1, "the ban is still stored in state");
        assert!(
            !bans[0].enforcing,
            "bob's authority was revoked, so his ban no longer keeps alice out"
        );
    }

    #[test]
    fn ban_reports_not_enforcing_once_its_banner_is_no_longer_a_member() {
        // Second inert path from #472: the banner left or was pruned, so the
        // deputy-derived grants no longer apply. Alice must remain a member,
        // otherwise `ban_is_enforcing` short-circuits to true on an absent
        // target and the test would pass for the wrong reason.
        let owner = key(1);
        let alice = key(2);
        let carol = key(4);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &owner, &carol);
        push_info(&mut state, &owner, 0, vec![id(&carol)]);
        push_ban(&mut state, &owner, &carol, &alice);

        // Sanity: enforcing while carol is present.
        assert!(collect_ban_infos(&state, &owner.verifying_key())[0].enforcing);

        // Carol leaves / is pruned.
        state
            .members
            .members
            .retain(|m| m.member.id() != id(&carol));

        let bans = collect_ban_infos(&state, &owner.verifying_key());
        assert!(
            !bans[0].enforcing,
            "a ban whose banner is gone is inert and must not read as active"
        );
    }

    #[test]
    fn legitimate_ancestor_ban_still_enforcing_after_its_target_is_removed() {
        // Regression guard for a wrong "fix" that is tempting on inspection:
        // computing the flag from a bare `is_ban_authorized` instead of
        // `ban_is_enforcing`, on the theory that the latter skips the authority
        // check for an absent target.
        //
        // It skips it for a reason. Once a ban has done its job the target is
        // gone, so there is no invite chain left to walk and the ancestor grant
        // that authorized the ban can no longer be re-derived. A bare
        // `is_ban_authorized` therefore reports NOT ENFORCING for essentially
        // every ban that is working, which is far more misleading than the
        // narrow case it would tighten.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);

        // owner -> bob -> alice, and bob bans alice as her strict ancestor.
        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &bob);
        push_member(&mut state, &owner, &bob, &alice);
        push_ban(&mut state, &owner, &bob, &alice);

        assert!(
            collect_ban_infos(&state, &owner.verifying_key())[0].enforcing,
            "an ancestor's ban of a present member enforces"
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

        assert!(
            collect_ban_infos(&state, &owner.verifying_key())[0].enforcing,
            "the ban is working, so it must not read as NOT ENFORCING once its \
             target is gone"
        );
    }

    #[test]
    fn a_present_target_agrees_with_the_contracts_excluded_set() {
        // `enforcing == false` is the actionable claim: the target is a current
        // member this ban fails to exclude. Tie that to the contract's own
        // excluded set rather than to a predicate that merely looks related.
        // Each state holds exactly one ban, so `banned_member_ids` containing
        // the target is precisely "this ban excludes its target".
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
            let reported = collect_ban_infos(state, &owner.verifying_key())[0].enforcing;
            let target = state.bans.0[0].ban.banned_user;
            assert!(
                state
                    .members
                    .members
                    .iter()
                    .any(|m| m.member.id() == target),
                "this equivalence only holds for a PRESENT target: {label}"
            );

            let actually_excluded = state
                .members
                .banned_member_ids(&state.bans, &state.member_info, &params)
                .contains(&target);

            assert_eq!(
                reported, actually_excluded,
                "`debug bans` disagrees with the contract's excluded set for: {label}"
            );

            saw_enforcing |= reported;
            saw_inert |= !reported;
        }

        // Guard against a vacuous pass if every case landed on one side.
        assert!(saw_enforcing && saw_inert, "cases must cover both outcomes");
    }

    #[test]
    fn ban_json_carries_the_enforcing_flag_alongside_the_pre_existing_shape() {
        // `banned_user_id` / `banned_by_id` / `banned_at_secs` are the published
        // shape; `enforcing` is additive. Renaming or dropping any of them
        // breaks consumers of `debug bans -f json`.
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
        assert_eq!(
            entry["enforcing"], true,
            "`enforcing` must be present and boolean in the JSON output"
        );
    }

    #[test]
    fn human_output_marks_only_the_non_enforcing_bans() {
        // The human branch is the surface the issue is about: an inert ban that
        // renders identically to a live one is the whole bug.
        let enforcing = BanInfo {
            banned_user_id: "LIVEUSER".to_string(),
            banned_by_id: "BANNERA".to_string(),
            banned_at_secs: 100,
            enforcing: true,
        };
        let inert = BanInfo {
            banned_user_id: "INERTUSER".to_string(),
            banned_by_id: "BANNERB".to_string(),
            banned_at_secs: 200,
            enforcing: false,
        };

        let lines = ban_list_lines(&[enforcing, inert]);
        let live_line = lines
            .iter()
            .find(|l| l.contains("LIVEUSER"))
            .expect("the enforcing ban must still be listed");
        let inert_line = lines
            .iter()
            .find(|l| l.contains("INERTUSER"))
            .expect("the inert ban must still be listed");

        assert!(
            !live_line.contains("NOT ENFORCING"),
            "an enforcing ban must not be flagged: {live_line}"
        );
        assert!(
            inert_line.contains("NOT ENFORCING"),
            "an inert ban must be visibly flagged: {inert_line}"
        );

        let rendered = lines.join("\n");
        assert!(
            rendered.contains("2 bans, 1 enforcing"),
            "the header must report how many bans actually enforce: {rendered}"
        );
        assert!(
            rendered.contains("member deputized-by"),
            "the note must point at the command that explains why a ban went inert"
        );
    }

    #[test]
    fn human_output_stays_quiet_when_every_ban_enforces() {
        // No false alarm: a room whose bans are all live must not carry the
        // inert-ban note, or moderators learn to ignore it.
        let lines = ban_list_lines(&[BanInfo {
            banned_user_id: "LIVEUSER".to_string(),
            banned_by_id: "BANNERA".to_string(),
            banned_at_secs: 100,
            enforcing: true,
        }]);
        let rendered = lines.join("\n");

        assert!(rendered.contains("1 bans, 1 enforcing"));
        assert!(!rendered.contains("NOT ENFORCING"));
        assert!(!rendered.contains("Note:"));
    }

    #[test]
    fn human_output_handles_an_empty_ban_list() {
        let lines = ban_list_lines(&[]);
        let rendered = lines.join("\n");
        assert!(rendered.contains("0 bans, 0 enforcing"));
        assert!(rendered.contains("No bans."));
        assert!(!rendered.contains("Note:"));
    }
}
