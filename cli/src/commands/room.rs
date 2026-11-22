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

        /// Create an encrypted (private) room: the name, your nickname, and all
        /// messages are AES-256-GCM encrypted with a room secret. Members join
        /// via invitation only.
        #[arg(short = 'p', long)]
        private: bool,
    },
    /// List all rooms
    List {
        /// Signing key (base64-encoded 32-byte Ed25519 signing key), matching
        /// `message send --signing-key` / `RIVER_SIGNING_KEY`.
        ///
        /// Only affects `self_member_id` / `signing_key_source`: report the
        /// identity THIS key would send as, so the value agrees with
        /// `identity whoami` and with the `author` on messages this key signs.
        /// Room names still resolve via each room's stored key, since a
        /// signing identity does not necessarily hold the room secret.
        #[arg(long, env = "RIVER_SIGNING_KEY")]
        signing_key: Option<String>,
    },
    /// Join a room
    ///
    /// Without `--pow` this explains that a room needs an invitation. With
    /// `--pow`, an open public room admits you provisionally: the command
    /// solves the room's proof of work, signs it with your key, and posts it
    /// with your nickname and a first message. It prints the signing key when
    /// it made one; pass it to `message send --signing-key` afterwards.
    Join {
        /// Room ID
        room_id: String,

        /// Join an open public room by proof of work (see `room admission`)
        #[arg(long)]
        pow: bool,

        /// Your nickname in the room (with --pow)
        #[arg(short = 'N', long)]
        nickname: Option<String>,

        /// Your first message (with --pow)
        #[arg(short, long)]
        message: Option<String>,

        /// Signing key (base64 32-byte Ed25519 key) to join as (with --pow);
        /// a new key is made when omitted
        #[arg(long, env = "RIVER_SIGNING_KEY")]
        signing_key: Option<String>,

        /// Solve this many bits beyond the requirement (with --pow), so a
        /// small rise in difficulty does not invalidate the proof
        #[arg(long, default_value_t = 0)]
        extra_bits: u8,

        /// Proof-of-work threads (with --pow; default: one per core)
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Show what a room asks of a newcomer (invitation, or proof of work),
    /// and, with a signing key, where that key stands in the room
    Admission {
        /// Room owner key (base58)
        room_id: String,

        /// Signing key (base64 32-byte Ed25519 key) to report the status of
        #[arg(long, env = "RIVER_SIGNING_KEY")]
        signing_key: Option<String>,
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

        /// Set room name (sealed automatically in a private room)
        #[arg(long)]
        name: Option<String>,

        /// Set room description (sealed automatically in a private room)
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

        /// Open (true) or close (false) admission by proof of work in a
        /// public room. Closing clears the room's provisional members and bans.
        #[arg(long)]
        public_admission: Option<bool>,

        /// Proof-of-work difficulty, in leading zero bits (0 to 32)
        #[arg(long)]
        admission_difficulty: Option<u8>,

        /// Pick a new random proof-of-work epoch: invalidates every proof
        /// solved so far, stockpiles included
        #[arg(long)]
        admission_new_epoch: bool,

        /// Most open (provisional) members the room holds at once (1 to 16)
        #[arg(long)]
        max_provisional: Option<u32>,

        /// Message-window slots open members may fill (1 to 500; the room
        /// also caps it at half its message window)
        #[arg(long)]
        max_provisional_messages: Option<u32>,

        /// Most provisional bans the room holds (0 to 16)
        #[arg(long)]
        max_provisional_bans: Option<u32>,

        /// List the room for discovery (no effect on admission)
        #[arg(long)]
        admission_listed: Option<bool>,
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
        RoomCommands::Create {
            name,
            nickname,
            private,
        } => {
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
                eprintln!(
                    "Creating {} room '{}' with nickname '{}'...",
                    if private { "private" } else { "public" },
                    name,
                    nickname
                );
            }

            match api.create_room(name.clone(), nickname, private).await {
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
        RoomCommands::List { signing_key } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Listing rooms...");
            }

            // Same three-way precedence as `identity whoami`, so the two
            // surfaces never report different ids for the same room
            // (freenet/river#438). Disclosed alongside `self_member_id`
            // because an override replaces the identity for EVERY room at
            // once, including ones it is not a member of.
            let inline_key = signing_key
                .as_deref()
                .map(crate::commands::identity::parse_inline_signing_key)
                .transpose()?;
            let signing_key_source = match (&inline_key, api.storage().has_signing_key_override()) {
                (Some(_), _) => "inline",
                (None, true) => "override",
                (None, false) => "stored",
            };
            match api.list_rooms_as(inline_key.as_ref()).await {
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
                                for room in rooms {
                                    println!("Room: {}", room.name.green());
                                    println!(
                                        "  Owner key: {}",
                                        bs58::encode(room.owner_vk.as_bytes()).into_string()
                                    );
                                    println!("  Contract key: {}", room.contract_key);
                                    println!("  Your member ID: {}", room.self_identity.member_id);
                                    println!();
                                }
                            }
                            OutputFormat::Json => {
                                let json_rooms: Vec<_> = rooms
                                    .into_iter()
                                    .map(|room| {
                                        serde_json::json!({
                                            "name": room.name,
                                            "owner_key": bs58::encode(room.owner_vk.as_bytes())
                                                .into_string(),
                                            "contract_key": room.contract_key,
                                            // freenet/river#438: the `author` a
                                            // message from this identity carries.
                                            "self_member_id":
                                                room.self_identity.member_id.to_string(),
                                            // Which identity that is. Under
                                            // `--signing-key-file` the override
                                            // applies to EVERY room, including
                                            // ones it isn't a member of, so say
                                            // so rather than letting a script
                                            // read the ids as per-room facts.
                                            "signing_key_source": signing_key_source,
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
        RoomCommands::Admission {
            room_id,
            signing_key,
        } => {
            let owner_key = crate::admission::parse_room_id(&room_id)?;
            let room_state = api.get_room(&owner_key, false).await?;
            let req = river_core::room_state::admission::admission_requirements(
                &room_state.configuration.configuration,
            );
            let status = match signing_key.as_deref() {
                Some(k) => {
                    let (sk, _) = crate::admission::key_or_new(Some(k))?;
                    let stored = api
                        .storage()
                        .get_provisional_entry(&owner_key, &sk.verifying_key())?;
                    Some(river_core::room_state::admission::admission_status(
                        &room_state,
                        &river_core::room_state::ChatRoomParametersV1 { owner: owner_key },
                        &sk.verifying_key(),
                        stored.as_ref(),
                        std::time::SystemTime::now(),
                    ))
                }
                None => None,
            };
            match format {
                OutputFormat::Human => {
                    println!("{}", crate::admission::describe_requirements(&req));
                    if let Some(status) = &status {
                        println!("your key: {}", crate::admission::status_json(status));
                    }
                }
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "room_id": room_id,
                            "requirements": crate::admission::requirements_json(&req),
                            "status": status.as_ref().map(crate::admission::status_json),
                        }))?
                    );
                }
            }
            Ok(())
        }
        RoomCommands::Join {
            room_id,
            pow: true,
            nickname,
            message,
            signing_key,
            extra_bits,
            threads,
        } => {
            let owner_key = crate::admission::parse_room_id(&room_id)?;
            let (sk, generated) = crate::admission::key_or_new(signing_key.as_deref())?;
            let room_state = api.get_room(&owner_key, false).await?;
            let threads = threads.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            });
            let quiet = matches!(format, OutputFormat::Json);
            let (delta, outcome) = crate::admission::build_join(
                api.storage(),
                &room_state,
                &owner_key,
                &sk,
                &crate::admission::JoinOptions {
                    nickname: nickname.as_deref().unwrap_or("Anonymous"),
                    text: message.as_deref().unwrap_or("Hello!"),
                    extra_bits,
                    threads,
                },
                |tried, expected| {
                    if !quiet {
                        eprintln!(
                            "solving proof of work: {tried} hashes tried, about {expected:.0} expected"
                        );
                    }
                },
            )?;
            // The apply the contract will run: refuse to report a join the room
            // would drop.
            let mut check = room_state.clone();
            let params = river_core::room_state::ChatRoomParametersV1 { owner: owner_key };
            freenet_scaffold::ComposableState::apply_delta(
                &mut check,
                &room_state,
                &params,
                &Some(delta.clone()),
            )
            .map_err(|e| anyhow::anyhow!("The room would refuse this join: {e}"))?;
            crate::api::refuse_if_dropped(&check, &outcome.message_id)?;
            api.storage()
                .put_provisional_entry(&owner_key, &sk.verifying_key(), &outcome.entry)?;
            api.send_state_delta(&owner_key, &delta).await?;
            let key_b64 = {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(sk.to_bytes())
            };
            match format {
                OutputFormat::Human => {
                    println!("{}", "Joined the room as an open member.".green());
                    println!("member id: {}", outcome.member_id);
                    if generated {
                        println!("signing key (keep it; nothing else stores it): {key_b64}");
                    }
                    println!(
                        "send more messages with: riverctl message send {room_id} \"text\" \\
                         --signing-key <key>"
                    );
                }
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": "success",
                            "room_id": room_id,
                            "member_id": outcome.member_id.to_string(),
                            "signing_key": generated.then_some(key_b64),
                            "solved_bits": outcome.nonce_bits,
                        })
                    );
                }
            }
            Ok(())
        }
        RoomCommands::Join { room_id, .. } => {
            // River has no "open join" without proof of work: membership
            // requires an invitation chain back to the room owner (see the
            // State Authorization Rule), or, in an open public room,
            // `--pow`. Make that explicit in both output modes instead of
            // printing a misleading "Joining room" line (and, in JSON mode,
            // instead of emitting nothing at all).
            match format {
                OutputFormat::Human => {
                    println!("{}", "Joining a room requires an invitation.".yellow());
                    println!("Ask an existing member for an invitation code, then run:");
                    println!("  riverctl invite accept <invitation-code>");
                    println!("An open public room can instead be joined by proof of work:");
                    println!("  riverctl room admission <room-id>");
                    println!("  riverctl room join <room-id> --pow");
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
            public_admission,
            admission_difficulty,
            admission_new_epoch,
            max_provisional,
            max_provisional_messages,
            max_provisional_bans,
            admission_listed,
        } => {
            let policy_edit = crate::admission::PolicyEdit {
                enable: public_admission,
                difficulty_bits: admission_difficulty,
                new_epoch: admission_new_epoch,
                max_provisional,
                max_provisional_messages,
                max_provisional_bans,
                listed: admission_listed,
            };
            let has_changes = !policy_edit.is_empty()
                || name.is_some()
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
                println!(
                    "  admission: {}",
                    crate::admission::describe_requirements(
                        &river_core::room_state::admission::admission_requirements(cfg)
                    )
                );
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
                    policy_edit.apply(cfg);
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
