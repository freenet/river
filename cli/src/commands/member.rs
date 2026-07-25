use crate::api::ApiClient;
use crate::deputies::{
    display_nickname, grant_status_line, party_label, ResolveError, RoomDeputies,
};
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use colored::Colorize;
use river_core::room_state::member::MemberId;

#[derive(Subcommand)]
pub enum MemberCommands {
    /// List members of a room
    List {
        /// Room ID (owner key in base58)
        room_id: String,
    },
    /// Set your nickname in a room
    SetNickname {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Your new nickname
        nickname: String,
    },
    /// Ban a member from a room
    Ban {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member ID to ban (8-character short ID from member list)
        member_id: String,
    },
    /// Deputize a member so they can help moderate (ban) within your invite subtree
    Deputize {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member ID to deputize (8-character short ID from member list)
        member_id: String,
    },
    /// Revoke a member's deputy authority (their prior bans stop enforcing)
    RevokeDeputy {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member ID whose deputy authority to revoke (8-character short ID)
        member_id: String,
    },
    /// Show the deputies a member has appointed ("who has X deputized?")
    ///
    /// Deputies are per-deputizer, not a room-wide flag: this lists only the
    /// members that MEMBER_ID has deputized, who may ban within MEMBER_ID's own
    /// invite subtree. Defaults to your own identity in the room.
    ///
    /// For the opposite question ("is this member anyone's deputy?") use
    /// `member deputized-by`.
    Deputies {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member ID whose deputies to list (8-character short ID from member
        /// list). Defaults to your own identity in this room.
        member_id: Option<String>,
    },
    /// Show who has deputized a member ("is X a deputy of anyone?")
    ///
    /// The reverse of `member deputies`: scans every member's signed deputy
    /// list for MEMBER_ID and reports each member who granted them authority,
    /// including whether that grant is the room owner's (room-wide) or a
    /// regular member's (limited to that member's invite subtree).
    DeputizedBy {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member ID to look up (8-character short ID from member list)
        member_id: String,
    },
}

pub async fn execute(command: MemberCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        MemberCommands::List { room_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Listing members of room: {}", room_id);
            }

            // Parse the room owner key
            let owner_key_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;
            if owner_key_bytes.len() != 32 {
                return Err(anyhow!("Invalid room ID: expected 32 bytes"));
            }
            let mut key_array = [0u8; 32];
            key_array.copy_from_slice(&owner_key_bytes);
            let owner_vk = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;

            // Get the room state
            let mut room_state = api.get_room(&owner_vk, false).await?;

            // For a private room, collect the local member's secrets so member
            // nicknames (which are AES-256-GCM sealed) decrypt instead of
            // rendering as "[Encrypted: N bytes, vN]". Empty / no-op for a
            // public room or a room not in local storage.
            let secrets = api.room_display_secrets(&owner_vk, &mut room_state);

            // Deputy grants are attributed per-deputizer (#410), so a member row
            // carries the members who deputized THEM, never a bare badge (the
            // UI's badge is viewer-scoped and routinely misread as room-wide).
            let deputies = RoomDeputies::new(&room_state, &owner_vk, &secrets);
            let deputized_by = deputies.deputizers_by_deputy();

            // One row per MEMBER, not per `member_info` record: `verify`
            // deliberately accepts several records for the same member
            // (migration-safety), so iterating the raw vector emitted duplicate
            // rows whose nickname came from a losing record while the deputy
            // annotation came from the canonical one. `members_with_info` is
            // deduplicated and `party` reads the canonical record.
            let members: Vec<_> = deputies
                .members_with_info()
                .map(|id| {
                    let party = deputies.party(id);
                    let granted_by: Vec<MemberId> =
                        deputized_by.get(&id).cloned().unwrap_or_default();
                    (party, deputies.deputies_of(id).to_vec(), granted_by)
                })
                .collect();

            match format {
                OutputFormat::Human => {
                    if members.is_empty() {
                        println!("No members found in room.");
                    } else {
                        println!("\n{} member(s) found:\n", members.len());
                        for (party, _own_deputies, granted_by) in &members {
                            // Escaped and quoted: an unescaped nickname can
                            // forge a row that reads as a real deputy grant,
                            // and `colored` drops the colour that would
                            // distinguish it as soon as output is piped.
                            let nickname = party
                                .nickname
                                .as_deref()
                                .map(display_nickname)
                                .unwrap_or_else(|| "(unknown)".to_string());
                            print!("  {} ({})", nickname.green(), party.member_id);
                            if !granted_by.is_empty() {
                                let names: Vec<String> = granted_by
                                    .iter()
                                    .map(|id| party_label(&deputies.party(*id)))
                                    .collect();
                                print!("{}", format!("  deputy of: {}", names.join(", ")).yellow());
                            }
                            println!();
                        }
                        println!();
                    }
                }
                OutputFormat::Json => {
                    let json_members: Vec<_> = members
                        .into_iter()
                        .map(|(party, own_deputies, granted_by)| {
                            serde_json::json!({
                                "member_id": party.member_id,
                                // Unchanged shape: the pre-#470 output rendered
                                // a member with no readable nickname as the
                                // lossy placeholder string, never null, and
                                // `party.nickname` is only None for a member
                                // with no member_info record, which cannot
                                // appear in this listing.
                                "nickname": party.nickname.unwrap_or_default(),
                                // Members this member has deputized.
                                "deputies": ids_to_strings(&own_deputies),
                                // Members who have deputized this member.
                                "deputized_by": ids_to_strings(&granted_by),
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json_members)?);
                }
            }
            Ok(())
        }
        MemberCommands::SetNickname { room_id, nickname } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Setting nickname to '{}' in room: {}", nickname, room_id);
            }

            // Parse the room owner key
            let owner_key_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;
            if owner_key_bytes.len() != 32 {
                return Err(anyhow!("Invalid room ID: expected 32 bytes"));
            }
            let mut key_array = [0u8; 32];
            key_array.copy_from_slice(&owner_key_bytes);
            let owner_vk = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;

            match api.set_nickname(&owner_vk, nickname.clone()).await {
                Ok(()) => match format {
                    OutputFormat::Human => {
                        println!("{}", "Nickname updated successfully!".green());
                    }
                    OutputFormat::Json => {
                        println!(
                            "{}",
                            serde_json::json!({
                                "success": true,
                                "nickname": nickname,
                            })
                        );
                    }
                },
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    return Err(e);
                }
            }
            Ok(())
        }
        MemberCommands::Ban { room_id, member_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Banning member '{}' from room: {}", member_id, room_id);
            }

            // Parse the room owner key
            let owner_key_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;
            if owner_key_bytes.len() != 32 {
                return Err(anyhow!("Invalid room ID: expected 32 bytes"));
            }
            let mut key_array = [0u8; 32];
            key_array.copy_from_slice(&owner_key_bytes);
            let owner_vk = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;

            match api.ban_member(&owner_vk, &member_id).await {
                Ok(()) => match format {
                    OutputFormat::Human => {
                        println!(
                            "{}",
                            format!("Member '{}' has been banned.", member_id).green()
                        );
                    }
                    OutputFormat::Json => {
                        println!(
                            "{}",
                            serde_json::json!({
                                "success": true,
                                "banned_member_id": member_id,
                            })
                        );
                    }
                },
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    return Err(e);
                }
            }
            Ok(())
        }
        MemberCommands::Deputize { room_id, member_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Deputizing member '{}' in room: {}", member_id, room_id);
            }
            let owner_vk = parse_room_id(&room_id)?;
            match api.deputize(&owner_vk, &member_id).await {
                Ok(()) => match format {
                    OutputFormat::Human => println!(
                        "{}",
                        format!(
                            "Member '{}' can now help moderate people you invited.",
                            member_id
                        )
                        .green()
                    ),
                    OutputFormat::Json => println!(
                        "{}",
                        serde_json::json!({ "success": true, "deputized_member_id": member_id })
                    ),
                },
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    return Err(e);
                }
            }
            Ok(())
        }
        MemberCommands::RevokeDeputy { room_id, member_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Revoking deputy '{}' in room: {}", member_id, room_id);
            }
            let owner_vk = parse_room_id(&room_id)?;
            match api.revoke_deputy(&owner_vk, &member_id).await {
                Ok(()) => match format {
                    OutputFormat::Human => println!(
                        "{}",
                        format!("Member '{}' is no longer your deputy.", member_id).green()
                    ),
                    OutputFormat::Json => println!(
                        "{}",
                        serde_json::json!({ "success": true, "revoked_member_id": member_id })
                    ),
                },
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    return Err(e);
                }
            }
            Ok(())
        }
        MemberCommands::Deputies { room_id, member_id } => {
            let owner_vk = parse_room_id(&room_id)?;

            // No MEMBER_ID means the identity this CLI joined the room as.
            // Deputy grants are per-deputizer, so "my deputies" is the only
            // sensible default; there is no room-wide list to fall back to.
            // Resolved BEFORE the fetch so a caller with no local room record
            // gets the error immediately instead of after a full GET.
            let own_id = match &member_id {
                Some(_) => None,
                None => Some(own_member_id(&api, &owner_vk)?),
            };

            if !matches!(format, OutputFormat::Json) {
                eprintln!("Listing deputies in room: {}", room_id);
            }

            let mut room_state = api.get_room(&owner_vk, false).await?;
            let secrets = api.room_display_secrets(&owner_vk, &mut room_state);
            let deputies = RoomDeputies::new(&room_state, &owner_vk, &secrets);

            let subject_id = match member_id.as_deref() {
                Some(short) => resolve_or_explain(&deputies, short)?,
                // `own_id` was resolved above on exactly this branch.
                None => {
                    own_id.ok_or_else(|| anyhow!("no member ID given and no local identity"))?
                }
            };

            let subject = deputies.party(subject_id);
            let grants: Vec<_> = deputies
                .deputies_of(subject_id)
                .iter()
                .map(|deputy| deputies.grant(subject_id, *deputy))
                .collect();

            match format {
                OutputFormat::Human => {
                    if grants.is_empty() {
                        println!(
                            "{} has not deputized anyone in this room.",
                            party_label(&subject)
                        );
                    } else {
                        println!(
                            "\nDeputies appointed by {} ({}):\n",
                            party_label(&subject),
                            count(grants.len(), "grant")
                        );
                        for grant in &grants {
                            println!(
                                "  {}  {}",
                                party_label(&grant.deputy).green(),
                                grant_status_line(grant)
                            );
                        }
                        println!();
                    }
                    // stderr, like every other `member` subcommand's chatter, so
                    // piping the human output does not mix prose into the data.
                    eprintln!(
                        "{}",
                        "Deputy authority is per-deputizer: this lists only the members \
                         the member above deputized."
                            .dimmed()
                    );
                }
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "room_id": room_id,
                            "direction": "deputies-of",
                            "subject": subject,
                            "grants": grants,
                        }))?
                    );
                }
            }
            Ok(())
        }
        MemberCommands::DeputizedBy { room_id, member_id } => {
            let owner_vk = parse_room_id(&room_id)?;
            if !matches!(format, OutputFormat::Json) {
                eprintln!(
                    "Looking up who deputized '{}' in room: {}",
                    member_id, room_id
                );
            }
            let mut room_state = api.get_room(&owner_vk, false).await?;
            let secrets = api.room_display_secrets(&owner_vk, &mut room_state);
            let deputies = RoomDeputies::new(&room_state, &owner_vk, &secrets);

            let subject_id = resolve_or_explain(&deputies, &member_id)?;
            let subject = deputies.party(subject_id);
            let grants: Vec<_> = deputies
                .deputizers_of(subject_id)
                .into_iter()
                .map(|deputizer| deputies.grant(deputizer, subject_id))
                .collect();

            match format {
                OutputFormat::Human => {
                    if grants.is_empty() {
                        println!(
                            "No one has deputized {} in this room.",
                            party_label(&subject)
                        );
                    } else {
                        println!(
                            "\n{} has been deputized by {}:\n",
                            party_label(&subject),
                            count(grants.len(), "member")
                        );
                        for grant in &grants {
                            let owner_tag = if grant.deputizer.is_owner {
                                "  [room owner]"
                            } else {
                                ""
                            };
                            println!(
                                "  {}{}  {}",
                                party_label(&grant.deputizer).green(),
                                owner_tag,
                                grant_status_line(grant)
                            );
                        }
                        println!();
                    }
                }
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "room_id": room_id,
                            "direction": "deputized-by",
                            "subject": subject,
                            "grants": grants,
                        }))?
                    );
                }
            }
            Ok(())
        }
    }
}

/// Render a list of member ids as their short-id strings, for JSON output.
fn ids_to_strings(ids: &[MemberId]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

/// `"1 grant"` / `"3 grants"`, so counted output reads as English rather than
/// as the `N thing(s)` form.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Resolve a short member id, turning the structured failure into the same
/// "use member list" guidance the write paths give — and, for an ambiguous
/// prefix, listing the ids it matched instead of silently picking one.
fn resolve_or_explain(deputies: &RoomDeputies<'_>, short: &str) -> Result<MemberId> {
    deputies.resolve_short_id(short).map_err(|err| match err {
        ResolveError::NotFound => anyhow!(
            "Member '{}' not found in this room. Use 'member list' to see member IDs.",
            short
        ),
        ResolveError::Ambiguous(ids) => anyhow!(
            "Member ID '{}' is ambiguous: it matches {}. Pass the full 8-character ID.",
            short,
            ids.join(", ")
        ),
    })
}

/// The member id this CLI would currently sign as in `room_owner_key`.
///
/// Routes through `Storage::self_identity`, the same derivation `identity
/// whoami` and every send path use, rather than re-deriving from a signing key
/// (see the "do not re-inline" note on `api::author_member_id`). It therefore
/// honours `--signing-key-file` / `RIVER_SIGNING_KEY_FILE`: with an override in
/// effect this is the OVERRIDE identity, not the persisted one.
fn own_member_id(
    api: &ApiClient,
    room_owner_key: &ed25519_dalek::VerifyingKey,
) -> Result<MemberId> {
    api.storage()
        .self_identity(room_owner_key)?
        .map(|identity| identity.member_id)
        .ok_or_else(|| {
            anyhow!(
                "Room not found in local storage, so there is no 'your own' identity to \
                 default to. Pass a member ID explicitly."
            )
        })
}

/// Decode a base58 room id (owner verifying key) into a `VerifyingKey`.
fn parse_room_id(room_id: &str) -> Result<ed25519_dalek::VerifyingKey> {
    let owner_key_bytes = bs58::decode(room_id)
        .into_vec()
        .map_err(|e| anyhow!("Invalid room ID: {}", e))?;
    if owner_key_bytes.len() != 32 {
        return Err(anyhow!("Invalid room ID: expected 32 bytes"));
    }
    let mut key_array = [0u8; 32];
    key_array.copy_from_slice(&owner_key_bytes);
    ed25519_dalek::VerifyingKey::from_bytes(&key_array)
        .map_err(|e| anyhow!("Invalid room ID: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Minimal harness so the `MemberCommands` clap surface can be parsed
    /// without pulling in the whole binary's global flags.
    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: MemberCommands,
    }

    fn parse(args: &[&str]) -> Result<MemberCommands, clap::Error> {
        let mut argv = vec!["member"];
        argv.extend_from_slice(args);
        TestCli::try_parse_from(argv).map(|cli| cli.command)
    }

    #[test]
    fn deputies_accepts_an_explicit_member_id() {
        match parse(&["deputies", "ROOM", "7XSOGJTK"]).expect("must parse") {
            MemberCommands::Deputies { room_id, member_id } => {
                assert_eq!(room_id, "ROOM");
                assert_eq!(member_id.as_deref(), Some("7XSOGJTK"));
            }
            other => panic!("wrong subcommand: {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn deputies_member_id_is_optional_and_defaults_to_self() {
        // The no-MEMBER_ID form is the "what did I deputize?" shorthand; it must
        // stay optional rather than becoming a required positional.
        match parse(&["deputies", "ROOM"]).expect("must parse without a member id") {
            MemberCommands::Deputies { room_id, member_id } => {
                assert_eq!(room_id, "ROOM");
                assert_eq!(member_id, None);
            }
            other => panic!("wrong subcommand: {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn deputized_by_is_spelled_in_kebab_case_and_requires_a_member_id() {
        match parse(&["deputized-by", "ROOM", "PMAQEUP5"]).expect("must parse") {
            MemberCommands::DeputizedBy { room_id, member_id } => {
                assert_eq!(room_id, "ROOM");
                assert_eq!(member_id, "PMAQEUP5");
            }
            other => panic!("wrong subcommand: {:?}", std::mem::discriminant(&other)),
        }

        // The reverse lookup has no sensible "self" default (asking who
        // deputized you is a different question from your own grants), so the
        // member id stays mandatory.
        assert!(
            parse(&["deputized-by", "ROOM"]).is_err(),
            "deputized-by must require a member id"
        );
    }

    #[test]
    fn room_id_is_required_for_both_new_commands() {
        assert!(parse(&["deputies"]).is_err());
        assert!(parse(&["deputized-by"]).is_err());
    }

    #[test]
    fn ids_to_strings_renders_short_ids() {
        use ed25519_dalek::SigningKey;
        let id: MemberId = SigningKey::from_bytes(&[5u8; 32]).verifying_key().into();
        assert_eq!(ids_to_strings(&[id]), vec![id.to_string()]);
        assert!(ids_to_strings(&[]).is_empty());
    }
}
