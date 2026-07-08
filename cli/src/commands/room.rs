use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use river_core::room_state::privacy::SealedBytes;

#[derive(Subcommand)]
pub enum RoomCommands {
    /// Create a new room
    Create {
        /// Room name
        #[arg(short, long)]
        name: String,

        /// Your nickname in the room
        #[arg(short = 'N', long)]
        nickname: Option<String>,
    },
    /// List all rooms
    List,
    /// Join a room
    Join {
        /// Room ID
        room_id: String,
    },
    /// Leave a room
    Leave {
        /// Room ID
        room_id: String,
    },
    /// Republish a room to the network
    ///
    /// Re-PUTs the room contract with its current state, making this node
    /// seed it again. Use when the room exists locally but isn't being
    /// served on the network.
    Republish {
        /// Room owner key (base58)
        room_id: String,
    },
    /// Update room configuration (owner only)
    Config {
        /// Room owner key (base58)
        room_id: String,

        /// Set room name (public rooms only)
        #[arg(long)]
        name: Option<String>,

        /// Set room description (public rooms only)
        #[arg(long)]
        description: Option<String>,

        /// Set maximum number of user bans remembered
        #[arg(long)]
        max_bans: Option<usize>,

        /// Set maximum number of recent messages stored
        #[arg(long)]
        max_messages: Option<usize>,

        /// Set maximum number of members
        #[arg(long)]
        max_members: Option<usize>,

        /// Set maximum message size in bytes
        #[arg(long)]
        max_message_size: Option<usize>,

        /// Set maximum nickname size in characters
        #[arg(long)]
        max_nickname_size: Option<usize>,

        /// Set maximum room name length
        #[arg(long)]
        max_room_name: Option<usize>,

        /// Set maximum room description length
        #[arg(long)]
        max_room_description: Option<usize>,
    },
}

/// Build the JSON payload emitted by `room join --format json`.
///
/// `room join` cannot make the caller a member (River requires an
/// invitation chain back to the owner), so it reports an `unsupported`
/// status plus the actionable next step rather than emitting nothing.
fn join_requires_invitation_json(room_id: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "unsupported",
        "room_id": room_id,
        "reason": "joining a room requires an invitation",
        "hint": "riverctl invite accept <invitation-code>",
    })
}

pub async fn execute(command: RoomCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        RoomCommands::Create { name, nickname } => {
            // Ask for nickname if not provided
            let nickname = match nickname {
                Some(n) => n,
                None => {
                    if atty::is(atty::Stream::Stdin) {
                        dialoguer::Input::<String>::new()
                            .with_prompt("Enter your nickname")
                            .default("Anonymous".to_string())
                            .interact_text()?
                    } else {
                        "Anonymous".to_string()
                    }
                }
            };

            if !matches!(format, OutputFormat::Json) {
                eprintln!("Creating room '{}' with nickname '{}'...", name, nickname);
            }

            match api.create_room(name.clone(), nickname).await {
                Ok((owner_key, contract_key)) => {
                    let result = CreateRoomResult {
                        room_name: name,
                        owner_key: bs58::encode(owner_key.as_bytes()).into_string(),
                        contract_key: contract_key.id().to_string(),
                    };

                    match format {
                        OutputFormat::Human => {
                            println!("{}", "Room created successfully!".green());
                            println!("Owner key: {}", result.owner_key);
                            println!("Contract key: {}", result.contract_key);
                            println!("\nTo invite others, use:");
                            println!("  riverctl invite create {}", result.owner_key);
                        }
                        OutputFormat::Json => {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    Err(e)
                }
            }
        }
        RoomCommands::List => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Listing rooms...");
            }

            match api.list_rooms().await {
                Ok(rooms) => {
                    if rooms.is_empty() {
                        match format {
                            OutputFormat::Human => {
                                println!("No rooms found. Use 'riverctl room create' to create a new room.");
                            }
                            OutputFormat::Json => {
                                println!("[]");
                            }
                        }
                    } else {
                        match format {
                            OutputFormat::Human => {
                                println!("\n{} room(s) found:\n", rooms.len());
                                for (owner_key, name, contract_key) in rooms {
                                    println!("Room: {}", name.green());
                                    println!("  Owner key: {}", owner_key);
                                    println!("  Contract key: {}", contract_key);
                                    println!();
                                }
                            }
                            OutputFormat::Json => {
                                let json_rooms: Vec<_> = rooms
                                    .into_iter()
                                    .map(|(owner_key, name, contract_key)| {
                                        serde_json::json!({
                                            "name": name,
                                            "owner_key": owner_key,
                                            "contract_key": contract_key,
                                        })
                                    })
                                    .collect();
                                println!("{}", serde_json::to_string_pretty(&json_rooms)?);
                            }
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    Err(e)
                }
            }
        }
        RoomCommands::Join { room_id } => {
            // River has no "open join": membership requires an invitation
            // chain back to the room owner (see the State Authorization Rule),
            // so there is nothing for this command to execute. Make that
            // explicit in both output modes instead of printing a misleading
            // "Joining room" line (and, in JSON mode, instead of emitting
            // nothing at all).
            match format {
                OutputFormat::Human => {
                    println!("{}", "Joining a room requires an invitation.".yellow());
                    println!("Ask an existing member for an invitation code, then run:");
                    println!("  riverctl invite accept <invitation-code>");
                }
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&join_requires_invitation_json(&room_id))?
                    );
                }
            }
            Ok(())
        }
        RoomCommands::Leave { room_id } => {
            // Parse the room owner key (base58) into a verifying key.
            let owner_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;
            let owner_key = ed25519_dalek::VerifyingKey::from_bytes(
                owner_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Invalid room ID length"))?,
            )
            .map_err(|e| anyhow::anyhow!("Invalid room owner key: {}", e))?;

            // Forget the locally-stored credentials for this room. This is the
            // deliberate-replace escape hatch for the re-accept guard
            // (freenet/river#308): once removed, `riverctl invite accept` can
            // store a fresh identity for the same room. Local-only — the room
            // contract and on-chain membership are untouched.
            let removed = api.storage().remove_room(&owner_key)?;

            match (format, removed) {
                (OutputFormat::Human, true) => {
                    println!("{}", "Left room (local credentials removed).".green());
                    println!("To rejoin, accept a fresh invitation:");
                    println!("  riverctl invite accept <invitation-code>");
                }
                (OutputFormat::Human, false) => {
                    println!("No locally-stored room found for: {}", room_id);
                }
                (OutputFormat::Json, _) => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": "success",
                            "room_id": room_id,
                            "removed": removed,
                        })
                    );
                }
            }
            Ok(())
        }
        RoomCommands::Config {
            room_id,
            name,
            description,
            max_bans,
            max_messages,
            max_members,
            max_message_size,
            max_nickname_size,
            max_room_name,
            max_room_description,
        } => {
            let has_changes = name.is_some()
                || description.is_some()
                || max_bans.is_some()
                || max_messages.is_some()
                || max_members.is_some()
                || max_message_size.is_some()
                || max_nickname_size.is_some()
                || max_room_name.is_some()
                || max_room_description.is_some();

            let owner_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;
            let owner_key = ed25519_dalek::VerifyingKey::from_bytes(
                owner_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Invalid room ID length"))?,
            )
            .map_err(|e| anyhow::anyhow!("Invalid room owner key: {}", e))?;

            if !has_changes {
                // No changes requested, show current config
                let mut room_state = api.get_room(&owner_key, false).await?;
                // For a private room the name/description are AES-256-GCM sealed;
                // decrypt them with the local member's secrets so this shows the
                // real values instead of "[Encrypted: N bytes, vN]". Falls back
                // to that placeholder when the secret is unavailable.
                let secrets = api.room_display_secrets(&owner_key, &mut room_state);
                let unseal = |sealed: &river_core::room_state::privacy::SealedBytes| {
                    river_core::ecies::unseal_bytes_with_secrets(sealed, &secrets)
                        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
                        .unwrap_or_else(|_| sealed.to_string_lossy())
                };
                let cfg = &room_state.configuration.configuration;
                let room_name = unseal(&cfg.display.name);
                let room_desc = cfg
                    .display
                    .description
                    .as_ref()
                    .map(unseal)
                    .unwrap_or_else(|| "(none)".to_string());
                println!("Current configuration:");
                println!("  name: {}", room_name);
                println!("  description: {}", room_desc);
                println!("  max_members: {}", cfg.max_members);
                println!("  max_recent_messages: {}", cfg.max_recent_messages);
                println!("  max_user_bans: {}", cfg.max_user_bans);
                println!("  max_message_size: {}", cfg.max_message_size);
                println!("  max_nickname_size: {}", cfg.max_nickname_size);
                println!("  max_room_name: {}", cfg.max_room_name);
                println!("  max_room_description: {}", cfg.max_room_description);
                return Ok(());
            }

            if !matches!(format, OutputFormat::Json) {
                eprintln!("Updating room configuration...");
            }

            let name_clone = name.clone();
            let description_clone = description.clone();

            // Pre-seal name / description for the room's privacy mode BEFORE the
            // update. In a PRIVATE room these are AES-256-GCM sealed under the
            // room secret — the previous unconditional `SealedBytes::public` got
            // a private-room name REJECTED by the contract's config guard
            // ("Private room must have encrypted display metadata") and silently
            // published a private-room description as PLAINTEXT. Only fetch state
            // when a metadata field is actually changing. `sealed_description`'s
            // outer Option is "was --description passed", the inner is the value
            // (None clears it).
            #[allow(clippy::type_complexity)]
            let (sealed_name, sealed_description): (
                Option<SealedBytes>,
                Option<Option<SealedBytes>>,
            ) = if name_clone.is_some() || description_clone.is_some() {
                let mut state = api.get_room(&owner_key, false).await?;
                let secrets = api.room_display_secrets(&owner_key, &mut state);
                let sn = match &name_clone {
                    Some(n) => Some(
                        crate::private_room::seal_field_for_room(&state, &secrets, n.as_bytes())
                            .map_err(|e| anyhow::anyhow!(e))?,
                    ),
                    None => None,
                };
                let sd = match &description_clone {
                    Some(d) if d.is_empty() => Some(None),
                    Some(d) => Some(Some(
                        crate::private_room::seal_field_for_room(&state, &secrets, d.as_bytes())
                            .map_err(|e| anyhow::anyhow!(e))?,
                    )),
                    None => None,
                };
                (sn, sd)
            } else {
                (None, None)
            };

            match api
                .update_config(&owner_key, |cfg| {
                    if let Some(ref n) = sealed_name {
                        cfg.display.name = n.clone();
                    }
                    if let Some(ref d) = sealed_description {
                        cfg.display.description = d.clone();
                    }
                    if let Some(v) = max_bans {
                        cfg.max_user_bans = v;
                    }
                    if let Some(v) = max_messages {
                        cfg.max_recent_messages = v;
                    }
                    if let Some(v) = max_members {
                        cfg.max_members = v;
                    }
                    if let Some(v) = max_message_size {
                        cfg.max_message_size = v;
                    }
                    if let Some(v) = max_nickname_size {
                        cfg.max_nickname_size = v;
                    }
                    if let Some(v) = max_room_name {
                        cfg.max_room_name = v;
                    }
                    if let Some(v) = max_room_description {
                        cfg.max_room_description = v;
                    }
                })
                .await
            {
                Ok(()) => {
                    match format {
                        OutputFormat::Human => {
                            println!("{}", "Configuration updated successfully!".green());
                            if let Some(v) = name {
                                println!("  name: {}", v);
                            }
                            if let Some(v) = description {
                                println!("  description: {}", v);
                            }
                            if let Some(v) = max_bans {
                                println!("  max_user_bans: {}", v);
                            }
                            if let Some(v) = max_messages {
                                println!("  max_recent_messages: {}", v);
                            }
                            if let Some(v) = max_members {
                                println!("  max_members: {}", v);
                            }
                            if let Some(v) = max_message_size {
                                println!("  max_message_size: {}", v);
                            }
                            if let Some(v) = max_nickname_size {
                                println!("  max_nickname_size: {}", v);
                            }
                            if let Some(v) = max_room_name {
                                println!("  max_room_name: {}", v);
                            }
                            if let Some(v) = max_room_description {
                                println!("  max_room_description: {}", v);
                            }
                        }
                        OutputFormat::Json => {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "status": "success",
                                    "room_id": room_id,
                                })
                            );
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    Err(e)
                }
            }
        }
        RoomCommands::Republish { room_id } => {
            // Parse the room owner key
            let owner_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;
            let owner_key = ed25519_dalek::VerifyingKey::from_bytes(
                owner_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Invalid room ID length"))?,
            )
            .map_err(|e| anyhow::anyhow!("Invalid room owner key: {}", e))?;

            if !matches!(format, OutputFormat::Json) {
                eprintln!("Republishing room: {}", room_id);
            }

            match api.republish_room(&owner_key).await {
                Ok(()) => {
                    match format {
                        OutputFormat::Human => {
                            println!("{}", "Room republished successfully!".green());
                            println!("The room contract is now being seeded on the network.");
                        }
                        OutputFormat::Json => {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "status": "success",
                                    "room_id": room_id,
                                })
                            );
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    Err(e)
                }
            }
        }
    }
}

#[derive(serde::Serialize)]
struct CreateRoomResult {
    room_name: String,
    owner_key: String,
    contract_key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_json_reports_unsupported_with_invitation_hint() {
        // Regression guard: `room join --format json` used to emit nothing
        // (silent exit 0). It must now return a structured, actionable
        // payload naming the room and pointing at `invite accept`.
        let json = join_requires_invitation_json("ROOMOWNERKEY");
        assert_eq!(json["status"], "unsupported");
        assert_eq!(json["room_id"], "ROOMOWNERKEY");
        assert!(json["reason"].as_str().unwrap().contains("invitation"));
        assert_eq!(json["hint"], "riverctl invite accept <invitation-code>");
    }
}
