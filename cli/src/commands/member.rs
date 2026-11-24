use crate::api::ApiClient;
use crate::deputies::{
    display_nickname, grant_status_line, party_label, DeputyParty, ResolveError, RoomDeputies,
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
        /// Member to ban: an 8-character short ID, or the member's full verifying
        /// key (from `member list`) to name one exactly.
        member_id: String,
        /// Refuse prefix/case-folded resolution; the supplied ID must match exactly.
        #[arg(long)]
        require_exact_member_id: bool,
        /// Refuse if banning this member would also remove invite descendants.
        #[arg(long)]
        require_no_descendants: bool,
        /// Refuse if any current canonical member record deputizes this member.
        #[arg(long)]
        require_not_deputy: bool,
    },
    /// Sponsor a provisional (open-admission) member: make them an ordinary
    /// member, invited by the key you sign with. Their key and messages are kept.
    /// Refused when the target is banned, is not provisional, or the room is at
    /// 90% of its member limit.
    Sponsor {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Provisional member: an 8-character short ID, or the full verifying key
        /// (from `member list`).
        member_id: String,
    },
    /// Deputize a member so they can help moderate (ban) within your invite subtree
    Deputize {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member to deputize: an 8-character short ID, or the member's full verifying
        /// key (from `member list`) to name one exactly.
        member_id: String,
    },
    /// Revoke a member's deputy authority (their prior bans stop enforcing)
    RevokeDeputy {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member whose deputy authority to revoke: an 8-character short ID, or
        /// the member's full verifying key (from `member list`).
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
        /// Member whose deputies to list: an 8-character short ID, or the
        /// member's full verifying key (from `member list`). Defaults to your
        /// own identity in this room.
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
        /// Member to look up: an 8-character short ID, or the member's full verifying
        /// key (from `member list`) to name one exactly.
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
                    // The full verifying key — the collision-proof identity to
                    // compare when trusting a member (the `member_id` label is
                    // only a 40-bit truncation). Every id iterated here holds a
                    // `member_info` record, and `MemberInfoV1::verify` requires
                    // such a record's member to be the owner or a current
                    // member, so on verified state this is always `Some`; the
                    // `None` arm below is defensive.
                    let verifying_key = deputies
                        .verifying_key(id)
                        .map(|vk| bs58::encode(vk.as_bytes()).into_string());
                    (
                        party,
                        deputies.deputies_of(id).to_vec(),
                        granted_by,
                        verifying_key,
                    )
                })
                .collect();

            // Open-admission (provisional) members: appended after the invited
            // members, so everything riverctl printed before is unchanged.
            // Same human row shape as a member (`"nick" (ID)` then `key:`), plus
            // a trailing `  provisional` marker the autoban parser ignores;
            // counted in "N member(s) found" so that figure matches the rows.
            let provisional: Vec<_> = deputies
                .provisional_members()
                .into_iter()
                .map(|id| {
                    let party = deputies.party(id);
                    let verifying_key = deputies
                        .verifying_key(id)
                        .map(|vk| bs58::encode(vk.as_bytes()).into_string());
                    (party, verifying_key)
                })
                .collect();

            match format {
                OutputFormat::Human => {
                    print!("{}", member_list_human(&deputies, &members, &provisional));
                }
                OutputFormat::Json => {
                    let mut json_members: Vec<_> = members
                        .into_iter()
                        .map(|(party, own_deputies, granted_by, verifying_key)| {
                            member_list_json(
                                &party,
                                &own_deputies,
                                &granted_by,
                                verifying_key.as_deref(),
                            )
                        })
                        .collect();
                    json_members.extend(provisional.iter().map(|(party, verifying_key)| {
                        provisional_member_json(party, verifying_key.as_deref())
                    }));
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
        MemberCommands::Ban {
            room_id,
            member_id,
            require_exact_member_id,
            require_no_descendants,
            require_not_deputy,
        } => {
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

            let safety = crate::api::BanSafety {
                require_exact_member_id,
                require_no_descendants,
                require_not_deputy,
            };
            match api
                .ban_member_with_safety(&owner_vk, &member_id, safety)
                .await
            {
                Ok(kind) => match format {
                    OutputFormat::Human => {
                        let list = match kind {
                            crate::api::BanKind::User => "the room's user ban list",
                            crate::api::BanKind::Provisional => "the room's provisional ban list",
                        };
                        println!(
                            "{}",
                            format!("Member '{}' has been banned (on {}).", member_id, list)
                                .green()
                        );
                    }
                    OutputFormat::Json => {
                        println!(
                            "{}",
                            serde_json::json!({
                                "success": true,
                                "banned_member_id": member_id,
                                "ban_kind": kind.as_str(),
                                "ban_held": true,
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
        MemberCommands::Sponsor { room_id, member_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Sponsoring member '{}' in room: {}", member_id, room_id);
            }
            let owner_vk = parse_room_id(&room_id)?;
            match api.sponsor_member(&owner_vk, &member_id).await {
                Ok(id) => match format {
                    OutputFormat::Human => println!(
                        "{}",
                        format!("Member '{}' ({}) is now sponsored.", member_id, id).green()
                    ),
                    OutputFormat::Json => println!(
                        "{}",
                        serde_json::json!({
                            "success": true,
                            "sponsored_member_id": id.to_string(),
                        })
                    ),
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

/// One row of `member list --format json`.
///
/// A published output contract, so it lives in a testable helper rather than
/// inline in the command: `member_id` and `nickname` predate the deputy
/// commands and must not change shape, while `deputies` (who this member
/// deputized) and `deputized_by` (who deputized them) are additive.
///
/// `nickname` is a STRING, never null. Before the deputy commands a member with
/// an unreadable nickname rendered as the lossy placeholder, and `party.nickname`
/// is `None` only for a member with no `member_info` record, who cannot appear
/// in this listing at all.
fn member_list_json(
    party: &DeputyParty,
    own_deputies: &[MemberId],
    granted_by: &[MemberId],
    verifying_key: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "member_id": party.member_id,
        // The base58 ed25519 public key — the collision-proof identity, matching
        // `identity whoami`'s `verifying_key` field. `null` (not "") when no key
        // is in state for this member, since absence of a key is meaningful (a
        // consumer must not treat a missing key as an empty string to compare).
        "verifying_key": verifying_key,
        "nickname": party.nickname.clone().unwrap_or_default(),
        "deputies": ids_to_strings(own_deputies),
        "deputized_by": ids_to_strings(granted_by),
    })
}

/// The human `member list` text. Pure (returns the text) so the exact shape,
/// which the moderation bot and the autoban script parse, is pinned by tests.
#[allow(clippy::type_complexity)]
fn member_list_human(
    deputies: &RoomDeputies<'_>,
    members: &[(DeputyParty, Vec<MemberId>, Vec<MemberId>, Option<String>)],
    provisional: &[(DeputyParty, Option<String>)],
) -> String {
    use std::fmt::Write;
    let total = members.len() + provisional.len();
    let mut out = String::new();
    if total == 0 {
        out.push_str("No members found in room.\n");
        return out;
    }
    let _ = write!(out, "\n{} member(s) found:\n\n", total);
    for (party, _own_deputies, granted_by, verifying_key) in members {
        // Escaped and quoted: an unescaped nickname can forge a row that reads
        // as a real deputy grant, and `colored` drops the colour that would
        // distinguish it as soon as output is piped.
        let nickname = party
            .nickname
            .as_deref()
            .map(display_nickname)
            .unwrap_or_else(|| "(unknown)".to_string());
        let _ = write!(out, "  {} ({})", nickname.green(), party.member_id);
        if !granted_by.is_empty() {
            let names: Vec<String> = granted_by
                .iter()
                .map(|id| party_label(&deputies.party(*id)))
                .collect();
            let _ = write!(
                out,
                "{}",
                format!("  deputy of: {}", names.join(", ")).yellow()
            );
        }
        out.push('\n');
        // Shown for every member by default (deliberately not behind a flag):
        // the point is to make the collision-proof identity visible so people
        // stop trusting the 40-bit short id. Printed plain (no colour) so it
        // copies cleanly into an allow-list. On verified state the key is
        // always present; this guard is defensive.
        if let Some(vk) = verifying_key {
            let _ = writeln!(out, "      key: {}", vk);
        }
    }
    for (party, verifying_key) in provisional {
        let nickname = party
            .nickname
            .as_deref()
            .map(display_nickname)
            .unwrap_or_else(|| "(unknown)".to_string());
        let _ = writeln!(
            out,
            "  {} ({}){}",
            nickname.green(),
            party.member_id,
            "  provisional".yellow()
        );
        if let Some(vk) = verifying_key {
            let _ = writeln!(out, "      key: {}", vk);
        }
    }
    out.push('\n');
    out
}

/// One provisional (open-admission) row of `member list --format json`: the
/// same keys as a member row, so a consumer parsing members parses these, plus
/// `"provisional": true`. A provisional member has no deputy relationships.
/// Member rows do not carry the `provisional` key at all.
fn provisional_member_json(party: &DeputyParty, verifying_key: Option<&str>) -> serde_json::Value {
    let mut row = member_list_json(party, &[], &[], verifying_key);
    row["provisional"] = serde_json::Value::Bool(true);
    row
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
            "Member ID '{}' is ambiguous: it matches {}. Pass the full 8-character ID, \
             or the member's full verifying key (from 'member list') to name one exactly.",
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
    fn ban_safety_flags_are_explicit_and_closed() {
        match parse(&[
            "ban",
            "ROOM",
            "ABCDEFGH",
            "--require-exact-member-id",
            "--require-no-descendants",
            "--require-not-deputy",
        ])
        .expect("must parse")
        {
            MemberCommands::Ban {
                room_id,
                member_id,
                require_exact_member_id,
                require_no_descendants,
                require_not_deputy,
            } => {
                assert_eq!(room_id, "ROOM");
                assert_eq!(member_id, "ABCDEFGH");
                assert!(require_exact_member_id);
                assert!(require_no_descendants);
                assert!(require_not_deputy);
            }
            other => panic!("wrong subcommand: {:?}", std::mem::discriminant(&other)),
        }
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
    fn member_list_json_keeps_the_pre_existing_shape_and_adds_both_directions() {
        // `member_id` and `nickname` are the published shape from before the
        // deputy commands; renaming either silently breaks every consumer.
        // `nickname` must stay a STRING, never null.
        use ed25519_dalek::SigningKey;
        let a: MemberId = SigningKey::from_bytes(&[5u8; 32]).verifying_key().into();
        let b: MemberId = SigningKey::from_bytes(&[6u8; 32]).verifying_key().into();

        let party = DeputyParty {
            member_id: a.to_string(),
            nickname: Some("Alice".to_string()),
            is_owner: false,
            in_room: true,
            retains_ban_authority: false,
        };
        let json = member_list_json(&party, &[b], &[], Some("SoMeBaSe58Key"));
        assert_eq!(json["member_id"], a.to_string());
        assert_eq!(json["nickname"], "Alice");
        assert_eq!(json["deputies"][0], b.to_string());
        assert!(
            json["deputized_by"].as_array().unwrap().is_empty(),
            "an empty list, not null"
        );
        // The collision-proof identity is surfaced as base58, mirroring
        // `whoami`'s `verifying_key` field.
        assert_eq!(json["verifying_key"], "SoMeBaSe58Key");

        // A member with no readable nickname renders as an empty string, not
        // null, matching the pre-deputy-command behaviour.
        let anonymous = DeputyParty {
            member_id: b.to_string(),
            nickname: None,
            is_owner: false,
            in_room: true,
            retains_ban_authority: false,
        };
        let json = member_list_json(&anonymous, &[], &[a], None);
        assert_eq!(json["nickname"], "");
        assert!(!json["nickname"].is_null());
        assert_eq!(json["deputized_by"][0], a.to_string());
        // No key in state for this member => explicit null (a consumer must be
        // able to tell "no key known" from an empty string it might compare).
        assert!(json["verifying_key"].is_null());
    }

    /// A room with the owner, one invited member "Alice" and one provisional
    /// (proof-of-work admitted) member "Spammer".
    fn room_with_provisional() -> (
        ed25519_dalek::SigningKey,
        river_core::room_state::ChatRoomStateV1,
        ed25519_dalek::SigningKey,
        ed25519_dalek::SigningKey,
    ) {
        use ed25519_dalek::SigningKey;
        use river_core::room_state::member::{AuthorizedMember, Member};
        use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
        use river_core::room_state::provisional::{
            AdmissionEvidence, AuthorizedProvisionalMember, ProvisionalMember,
        };
        let owner = SigningKey::from_bytes(&[1u8; 32]);
        let alice = SigningKey::from_bytes(&[2u8; 32]);
        let spam = SigningKey::from_bytes(&[3u8; 32]);
        let oid: MemberId = owner.verifying_key().into();
        let mut state = river_core::room_state::ChatRoomStateV1::default();
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: oid,
                invited_by: oid,
                member_vk: alice.verifying_key(),
            },
            &owner,
        ));
        for (k, name) in [(&alice, "Alice"), (&spam, "Spammer")] {
            state
                .member_info
                .member_info
                .push(AuthorizedMemberInfo::new_with_member_key(
                    MemberInfo::new_public(k.verifying_key().into(), 1, name.to_string()),
                    k,
                ));
        }
        state
            .provisional
            .entries
            .push(AuthorizedProvisionalMember::new(
                &owner.verifying_key(),
                ProvisionalMember {
                    member_vk: spam.verifying_key(),
                    evidence: AdmissionEvidence::Pow { nonce: 0 },
                },
                &spam,
            ));
        (owner, state, alice, spam)
    }

    #[allow(clippy::type_complexity)]
    fn rows_for(
        deputies: &RoomDeputies<'_>,
    ) -> (
        Vec<(DeputyParty, Vec<MemberId>, Vec<MemberId>, Option<String>)>,
        Vec<(DeputyParty, Option<String>)>,
    ) {
        let key = |id: MemberId| {
            deputies
                .verifying_key(id)
                .map(|vk| bs58::encode(vk.as_bytes()).into_string())
        };
        let members = deputies
            .members_with_info()
            .map(|id| (deputies.party(id), vec![], vec![], key(id)))
            .collect();
        let provisional = deputies
            .provisional_members()
            .into_iter()
            .map(|id| (deputies.party(id), key(id)))
            .collect();
        (members, provisional)
    }

    #[test]
    fn human_member_list_adds_provisional_rows_the_autoban_parser_reads() {
        colored::control::set_override(false);
        let (owner, state, _alice, spam) = room_with_provisional();
        let secrets = std::collections::HashMap::new();
        let deputies = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);
        let (members, provisional) = rows_for(&deputies);
        let text = member_list_human(&deputies, &members, &provisional);
        let spam_id: MemberId = spam.verifying_key().into();
        let spam_key = bs58::encode(spam.verifying_key().as_bytes()).into_string();

        // The count covers the provisional row, so "N member(s) found" equals
        // the number of rows the autoban parser extracts.
        assert!(text.contains("\n2 member(s) found:\n"), "{text}");
        // Same `  "nick" (ID)` shape as a member row, then the marker.
        assert!(
            text.contains(&format!("  \"Spammer\" ({spam_id})  provisional\n")),
            "{text}"
        );
        assert!(text.contains(&format!("      key: {spam_key}\n")), "{text}");
        let row_lines = text.lines().filter(|l| l.starts_with("  \"")).count();
        assert_eq!(row_lines, 2, "{text}");
    }

    #[test]
    fn human_member_list_is_unchanged_when_there_are_no_provisional_members() {
        colored::control::set_override(false);
        let (owner, mut state, alice, _spam) = room_with_provisional();
        state.provisional.entries.clear();
        let secrets = std::collections::HashMap::new();
        let deputies = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);
        let (members, provisional) = rows_for(&deputies);
        assert!(provisional.is_empty());
        let text = member_list_human(&deputies, &members, &provisional);
        let alice_id: MemberId = alice.verifying_key().into();
        let alice_key = bs58::encode(alice.verifying_key().as_bytes()).into_string();
        // Exactly the pre-change rendering: the count is the member count, and
        // no marker appears.
        assert!(text.starts_with("\n1 member(s) found:\n\n"), "{text}");
        assert!(!text.contains("provisional"), "{text}");
        assert!(
            text.contains(&format!(
                "  \"Alice\" ({alice_id})\n      key: {alice_key}\n"
            )),
            "{text}"
        );
        assert!(text.ends_with("\n\n"));
    }

    #[test]
    fn json_member_list_marks_provisional_rows_and_leaves_member_rows_alone() {
        let (owner, state, alice, spam) = room_with_provisional();
        let secrets = std::collections::HashMap::new();
        let deputies = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);
        let (members, provisional) = rows_for(&deputies);
        let (party, key) = &provisional[0];
        let row = provisional_member_json(party, key.as_deref());
        let spam_id: MemberId = spam.verifying_key().into();
        assert_eq!(row["member_id"], spam_id.to_string());
        assert_eq!(row["nickname"], "Spammer");
        assert_eq!(row["provisional"], true);
        assert_eq!(
            row["verifying_key"],
            bs58::encode(spam.verifying_key().as_bytes()).into_string()
        );
        assert!(row["deputies"].as_array().unwrap().is_empty());
        assert!(row["deputized_by"].as_array().unwrap().is_empty());
        // Member rows do not carry the key at all.
        let (mparty, _, _, mkey) = members
            .iter()
            .find(|(p, ..)| p.nickname.as_deref() == Some("Alice"))
            .unwrap();
        let mrow = member_list_json(mparty, &[], &[], mkey.as_deref());
        assert!(mrow.get("provisional").is_none());
        let alice_id: MemberId = alice.verifying_key().into();
        assert_eq!(mrow["member_id"], alice_id.to_string());
    }

    #[test]
    fn ids_to_strings_renders_short_ids() {
        use ed25519_dalek::SigningKey;
        let id: MemberId = SigningKey::from_bytes(&[5u8; 32]).verifying_key().into();
        assert_eq!(ids_to_strings(&[id]), vec![id.to_string()]);
        assert!(ids_to_strings(&[]).is_empty());
    }
}
