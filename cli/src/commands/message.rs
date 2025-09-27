use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use clap::Subcommand;
use ed25519_dalek::VerifyingKey;
use serde_json::json;

#[derive(Subcommand)]
pub enum MessageCommands {
    /// Send a message to a room
    Send {
        /// Room ID
        room_id: String,
        /// Message content
        message: String,
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
        /// Polling interval in milliseconds
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
    },
}

pub async fn execute(command: MessageCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        MessageCommands::Send { room_id, message } => {
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

            // Send the message
            api.send_message(&room_owner_key, message.clone()).await?;

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

            // Filter messages based on criteria
            let mut messages = room_state.recent_messages.messages.clone();

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
                                .map(|info| &info.member_info.preferred_nickname)
                                .unwrap_or(&author_short);

                            let datetime: DateTime<Utc> = msg.message.time.into();
                            let local_time: DateTime<Local> = datetime.into();

                            println!(
                                "[{} - {}]: {}",
                                local_time.format("%H:%M:%S"),
                                nickname,
                                msg.message.content
                            );
                        }
                    }
                }
                OutputFormat::Json => {
                    let json_messages: Vec<_> = messages
                        .iter()
                        .map(|msg| {
                            let author_str = msg.message.author.to_string();

                            let nickname = room_state
                                .member_info
                                .member_info
                                .iter()
                                .find(|info| info.member_info.member_id == msg.message.author)
                                .map(|info| &info.member_info.preferred_nickname);

                            let datetime: DateTime<Utc> = msg.message.time.into();

                            json!({
                                "author": author_str,
                                "nickname": nickname,
                                "content": msg.message.content,
                                "timestamp": datetime.to_rfc3339(),
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

            // Stream messages using the new method
            api.stream_messages(
                &room_owner_key,
                poll_interval,
                timeout,
                max_messages,
                initial_messages,
                format,
            )
            .await?;

            Ok(())
        }
    }
}
