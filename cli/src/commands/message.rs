use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::Result;
use base64::Engine;
use chrono::{DateTime, Local, Utc};
use clap::Subcommand;
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::message::MessageId;
use serde_json::json;

#[derive(Subcommand)]
pub enum MessageCommands {
    /// Send a message to a room
    Send {
        /// Room ID (base58-encoded room owner verifying key)
        room_id: String,
        /// Message content
        message: String,
        /// Signing key (base64-encoded 32-byte Ed25519 signing key).
        /// If provided, sends without requiring local room storage.
        /// Can also be set via RIVER_SIGNING_KEY environment variable.
        #[arg(long, env = "RIVER_SIGNING_KEY")]
        signing_key: Option<String>,
    },
    /// List recent messages in a room
    List {
        /// Room ID
        room_id: String,
        /// Number of messages to show
        #[arg(short, long, default_value = "20")]
        limit: usize,
        /// Show messages from the last N minutes
        #[arg(long)]
        since_minutes: Option<u64>,
    },
    /// Stream messages from a room in real-time
    Stream {
        /// Room ID
        room_id: String,
        /// Polling interval in milliseconds (only used without --subscribe)
        #[arg(short, long, default_value = "1000")]
        poll_interval: u64,
        /// Auto-exit after N seconds (0 = no timeout)
        #[arg(short, long, default_value = "0")]
        timeout: u64,
        /// Exit after receiving N new messages (0 = no limit)
        #[arg(short = 'n', long, default_value = "0")]
        max_messages: usize,
        /// Show last N messages when starting
        #[arg(short = 'i', long, default_value = "0")]
        initial_messages: usize,
        /// Use Freenet subscription for real-time updates instead of polling
        #[arg(short = 's', long, default_value = "false")]
        subscribe: bool,
    },
    /// Edit a message you sent
    Edit {
        /// Room ID
        room_id: String,
        /// Message ID (from 'message list --json', use the signature field)
        message_id: String,
        /// New message content
        new_content: String,
    },
    /// Delete a message you sent
    Delete {
        /// Room ID
        room_id: String,
        /// Message ID (from 'message list --json', use the signature field)
        message_id: String,
    },
    /// Add a reaction to a message
    React {
        /// Room ID
        room_id: String,
        /// Message ID (from 'message list --json', use the signature field)
        message_id: String,
        /// Emoji to react with (e.g., "👍", "❤️", "😂")
        emoji: String,
    },
    /// Remove a reaction from a message
    Unreact {
        /// Room ID
        room_id: String,
        /// Message ID (from 'message list --json', use the signature field)
        message_id: String,
        /// Emoji to remove
        emoji: String,
    },
    /// Reply to a message
    Reply {
        /// Room ID
        room_id: String,
        /// Message ID of the message to reply to
        message_id: String,
        /// Reply text
        message: String,
    },
}

pub async fn execute(command: MessageCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        MessageCommands::Send {
            room_id,
            message,
            signing_key,
        } => {
            // Parse room ID (base58-encoded verifying key)
            let room_owner_key_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;

            if room_owner_key_bytes.len() != 32 {
                return Err(anyhow::anyhow!(
                    "Invalid room ID: expected 32 bytes, got {}",
                    room_owner_key_bytes.len()
                ));
            }

            let room_owner_key =
                VerifyingKey::from_bytes(&room_owner_key_bytes.try_into().unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;

            // Send the message - use explicit signing key if provided, otherwise use storage
            if let Some(signing_key_str) = signing_key {
                // Parse signing key (base64-encoded)
                let signing_key_bytes = base64::engine::general_purpose::STANDARD
                    .decode(&signing_key_str)
                    .map_err(|e| {
                        anyhow::anyhow!("Invalid signing key (base64 decode failed): {}", e)
                    })?;

                if signing_key_bytes.len() != 32 {
                    return Err(anyhow::anyhow!(
                        "Invalid signing key: expected 32 bytes, got {}",
                        signing_key_bytes.len()
                    ));
                }

                let signing_key = SigningKey::from_bytes(
                    &signing_key_bytes
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("Invalid signing key length"))?,
                );

                // Send using the provided signing key (fetches room state from network)
                api.send_message_with_key(&room_owner_key, message.clone(), &signing_key)
                    .await?;
            } else {
                // Send using signing key from local storage
                api.send_message(&room_owner_key, message.clone()).await?;
            }

            match format {
                OutputFormat::Human => println!("Message sent successfully"),
                OutputFormat::Json => println!(r#"{{"status":"success","message":"sent"}}"#),
            }
            Ok(())
        }
        MessageCommands::List {
            room_id,
            limit,
            since_minutes,
        } => {
            // Parse room ID
            let room_owner_key_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;

            if room_owner_key_bytes.len() != 32 {
                return Err(anyhow::anyhow!(
                    "Invalid room ID: expected 32 bytes, got {}",
                    room_owner_key_bytes.len()
                ));
            }

            let room_owner_key =
                VerifyingKey::from_bytes(&room_owner_key_bytes.try_into().unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;

            // Get room state
            let room_state = api.get_room(&room_owner_key, false).await?;

            // Get only display messages (non-deleted, non-action)
            let mut messages: Vec<_> = room_state.recent_messages.display_messages().collect();

            // Apply time filter if specified
            if let Some(minutes) = since_minutes {
                let cutoff_time =
                    std::time::SystemTime::now() - std::time::Duration::from_secs(minutes * 60);
                messages.retain(|msg| msg.message.time >= cutoff_time);
            }

            // Sort by time (newest first) and limit
            messages.sort_by(|a, b| b.message.time.cmp(&a.message.time));
            messages.truncate(limit);

            // Reverse to show oldest first (chronological order)
            messages.reverse();

            match format {
                OutputFormat::Human => {
                    if messages.is_empty() {
                        println!("No messages found");
                    } else {
                        for msg in &messages {
                            let author_str = msg.message.author.to_string();
                            let author_short = author_str.chars().take(8).collect::<String>();

                            // Get nickname if available
                            let nickname = room_state
                                .member_info
                                .member_info
                                .iter()
                                .find(|info| info.member_info.member_id == msg.message.author)
                                .map(|info| info.member_info.preferred_nickname.to_string_lossy())
                                .unwrap_or(author_short);

                            let datetime: DateTime<Utc> = msg.message.time.into();
                            let local_time: DateTime<Local> = datetime.into();

                            // Get effective content (handles edits)
                            let content = room_state
                                .recent_messages
                                .effective_text(msg)
                                .unwrap_or_else(|| "<encrypted>".to_string());

                            // Check if message is edited
                            let msg_id = msg.id();
                            let edited = room_state.recent_messages.is_edited(&msg_id);
                            let edited_indicator = if edited { " (edited)" } else { "" };

                            // Check for reply context
                            let reply_prefix = {
                                use river_core::room_state::content::CONTENT_TYPE_REPLY;
                                if msg.message.content.content_type() == CONTENT_TYPE_REPLY {
                                    if let Some(
                                        river_core::room_state::content::DecodedContent::Reply(
                                            reply,
                                        ),
                                    ) = msg.message.content.decode_content()
                                    {
                                        let preview: String =
                                            reply.target_content_preview.chars().take(50).collect();
                                        format!(
                                            "[reply to {}: {}...] ",
                                            reply.target_author_name, preview
                                        )
                                    } else {
                                        String::new()
                                    }
                                } else {
                                    String::new()
                                }
                            };

                            // Get reactions
                            let reactions_str = room_state
                                .recent_messages
                                .reactions(&msg_id)
                                .map(|reactions| {
                                    if reactions.is_empty() {
                                        String::new()
                                    } else {
                                        let parts: Vec<_> = reactions
                                            .iter()
                                            .map(|(emoji, reactors)| {
                                                format!("{}×{}", emoji, reactors.len())
                                            })
                                            .collect();
                                        format!(" [{}]", parts.join(" "))
                                    }
                                })
                                .unwrap_or_default();

                            println!(
                                "[{} - {}]: {}{}{}{}",
                                local_time.format("%H:%M:%S"),
                                nickname,
                                reply_prefix,
                                content,
                                edited_indicator,
                                reactions_str
                            );
                        }
                    }
                }
                OutputFormat::Json => {
                    let json_messages: Vec<_> = messages
                        .iter()
                        .map(|msg| {
                            let author_str = msg.message.author.to_string();
                            let msg_id = msg.id();

                            let nickname = room_state
                                .member_info
                                .member_info
                                .iter()
                                .find(|info| info.member_info.member_id == msg.message.author)
                                .map(|info| info.member_info.preferred_nickname.to_string_lossy());

                            let datetime: DateTime<Utc> = msg.message.time.into();

                            // Get effective content
                            let content = room_state
                                .recent_messages
                                .effective_text(msg)
                                .unwrap_or_else(|| "<encrypted>".to_string());

                            // Check edited status
                            let edited = room_state.recent_messages.is_edited(&msg_id);

                            // Get reactions
                            let reactions: std::collections::HashMap<String, usize> = room_state
                                .recent_messages
                                .reactions(&msg_id)
                                .map(|r| r.iter().map(|(k, v)| (k.clone(), v.len())).collect())
                                .unwrap_or_default();

                            // Encode message ID for use in edit/delete/react commands
                            let message_id_str = msg_id.0 .0.to_string();

                            json!({
                                "message_id": message_id_str,
                                "author": author_str,
                                "nickname": nickname,
                                "content": content,
                                "timestamp": datetime.to_rfc3339(),
                                "edited": edited,
                                "reactions": reactions,
                            })
                        })
                        .collect();

                    println!("{}", serde_json::to_string_pretty(&json_messages)?);
                }
            }
            Ok(())
        }
        MessageCommands::Stream {
            room_id,
            poll_interval,
            timeout,
            max_messages,
            initial_messages,
            subscribe,
        } => {
            // Parse room ID
            let room_owner_key_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;

            if room_owner_key_bytes.len() != 32 {
                return Err(anyhow::anyhow!(
                    "Invalid room ID: expected 32 bytes, got {}",
                    room_owner_key_bytes.len()
                ));
            }

            let room_owner_key =
                VerifyingKey::from_bytes(&room_owner_key_bytes.try_into().unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;

            if subscribe {
                // Use real Freenet subscriptions for updates
                api.subscribe_and_stream(
                    &room_owner_key,
                    timeout,
                    max_messages,
                    initial_messages,
                    format,
                )
                .await?;
            } else {
                // Use polling for updates
                api.stream_messages(
                    &room_owner_key,
                    poll_interval,
                    timeout,
                    max_messages,
                    initial_messages,
                    format,
                )
                .await?;
            }

            Ok(())
        }
        MessageCommands::Edit {
            room_id,
            message_id,
            new_content,
        } => {
            let room_owner_key = parse_room_id(&room_id)?;
            let target_message_id = parse_message_id(&message_id)?;

            api.edit_message(&room_owner_key, target_message_id, new_content.clone())
                .await?;

            match format {
                OutputFormat::Human => println!("Message edited successfully"),
                OutputFormat::Json => println!(r#"{{"status":"success","action":"edit"}}"#),
            }
            Ok(())
        }
        MessageCommands::Delete {
            room_id,
            message_id,
        } => {
            let room_owner_key = parse_room_id(&room_id)?;
            let target_message_id = parse_message_id(&message_id)?;

            api.delete_message(&room_owner_key, target_message_id)
                .await?;

            match format {
                OutputFormat::Human => println!("Message deleted successfully"),
                OutputFormat::Json => println!(r#"{{"status":"success","action":"delete"}}"#),
            }
            Ok(())
        }
        MessageCommands::React {
            room_id,
            message_id,
            emoji,
        } => {
            let room_owner_key = parse_room_id(&room_id)?;
            let target_message_id = parse_message_id(&message_id)?;

            api.add_reaction(&room_owner_key, target_message_id, emoji.clone())
                .await?;

            match format {
                OutputFormat::Human => println!("Reaction '{}' added successfully", emoji),
                OutputFormat::Json => println!(
                    r#"{{"status":"success","action":"react","emoji":"{}"}}"#,
                    emoji
                ),
            }
            Ok(())
        }
        MessageCommands::Unreact {
            room_id,
            message_id,
            emoji,
        } => {
            let room_owner_key = parse_room_id(&room_id)?;
            let target_message_id = parse_message_id(&message_id)?;

            api.remove_reaction(&room_owner_key, target_message_id, emoji.clone())
                .await?;

            match format {
                OutputFormat::Human => println!("Reaction '{}' removed successfully", emoji),
                OutputFormat::Json => println!(
                    r#"{{"status":"success","action":"unreact","emoji":"{}"}}"#,
                    emoji
                ),
            }
            Ok(())
        }
        MessageCommands::Reply {
            room_id,
            message_id,
            message,
        } => {
            let room_owner_key = parse_room_id(&room_id)?;
            let target_message_id = parse_message_id(&message_id)?;

            api.send_reply(&room_owner_key, target_message_id, message.clone())
                .await?;

            match format {
                OutputFormat::Human => println!("Reply sent successfully"),
                OutputFormat::Json => println!(r#"{{"status":"success","action":"reply"}}"#),
            }
            Ok(())
        }
    }
}

/// Helper to parse room ID from base58-encoded string
fn parse_room_id(room_id: &str) -> Result<VerifyingKey> {
    let room_owner_key_bytes = bs58::decode(room_id)
        .into_vec()
        .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;

    if room_owner_key_bytes.len() != 32 {
        return Err(anyhow::anyhow!(
            "Invalid room ID: expected 32 bytes, got {}",
            room_owner_key_bytes.len()
        ));
    }

    VerifyingKey::from_bytes(&room_owner_key_bytes.try_into().unwrap())
        .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))
}

/// Helper to parse message ID from string (i64 hash value)
fn parse_message_id(message_id: &str) -> Result<MessageId> {
    let hash_value: i64 = message_id
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid message ID (expected integer): {}", e))?;

    Ok(MessageId(freenet_scaffold::util::FastHash(hash_value)))
}
