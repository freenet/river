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
    /// Send a message to a room.
    ///
    /// Mentions: a bare `@nickname` that unambiguously (case-insensitively)
    /// matches a current member's public nickname is converted into a mention
    /// that links to that member by id and follows their later renames. An
    /// unknown, ambiguous, or private-room `@word` is sent as plain text
    /// (riverctl cannot decrypt private-room nicknames). The desktop UI offers
    /// an @-autocomplete picker for the same result.
    Send {
        /// Room ID (base58-encoded room owner verifying key)
        room_id: String,
        /// Message content. Write `@nickname` to mention a member.
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
        #[arg(allow_hyphen_values = true)]
        message_id: String,
        /// New message content
        new_content: String,
    },
    /// Delete a message you sent
    Delete {
        /// Room ID
        room_id: String,
        /// Message ID (from 'message list --json', use the signature field)
        #[arg(allow_hyphen_values = true)]
        message_id: String,
    },
    /// Add a reaction to a message
    React {
        /// Room ID
        room_id: String,
        /// Message ID (from 'message list --json', use the signature field)
        #[arg(allow_hyphen_values = true)]
        message_id: String,
        /// Emoji to react with (e.g., "👍", "❤️", "😂")
        emoji: String,
    },
    /// Remove a reaction from a message
    Unreact {
        /// Room ID
        room_id: String,
        /// Message ID (from 'message list --json', use the signature field)
        #[arg(allow_hyphen_values = true)]
        message_id: String,
        /// Emoji to remove
        emoji: String,
    },
    /// Reply to a message.
    ///
    /// `@nickname` mentions in the reply text are resolved exactly as in `send`.
    Reply {
        /// Room ID
        room_id: String,
        /// Message ID of the message to reply to
        #[arg(allow_hyphen_values = true)]
        message_id: String,
        /// Reply text. Write `@nickname` to mention a member.
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
            let sent_message_id = if let Some(signing_key_str) = signing_key {
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
                    .await?
            } else {
                // Send using signing key from local storage
                api.send_message(&room_owner_key, message.clone()).await?
            };

            match format {
                OutputFormat::Human => {
                    println!("Message sent successfully (id: {})", sent_message_id.0 .0)
                }
                // Same inner-i64 form `message list` prints and `message delete`
                // accepts, so a caller can retract its own message without a
                // round trip to find it.
                OutputFormat::Json => println!(
                    r#"{{"status":"success","message":"sent","message_id":"{}"}}"#,
                    sent_message_id.0 .0
                ),
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
            let mut room_state = api.get_room(&room_owner_key, false).await?;

            // For a private room, collect the local member's decryption secrets
            // and rebuild the message actions_state (edits/deletes/reactions)
            // from the decrypted private actions. Empty map / no-op for a public
            // room or a room not in local storage — private bodies then still
            // render as "<encrypted>", exactly as before. Must run before
            // display_messages() so a decrypted private *deletion* hides its
            // message.
            let secrets = api.room_display_secrets(&room_owner_key, &mut room_state);

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

                            // Get nickname if available (decrypted for a private room).
                            // `canonical`, not a bare `.find()` (#411 round 8 item A): a
                            // duplicate-holding state could otherwise display a stale
                            // (e.g. revoked) record's nickname.
                            //
                            // Escaped for this HUMAN-only println below — the
                            // JSON branch further down makes its own separate
                            // `unseal_nickname_display` call and stays raw
                            // (attacker-controlled nickname; JSON escaping
                            // already makes it safe there).
                            let nickname = room_state
                                .member_info
                                .canonical(msg.message.author)
                                .map(|info| {
                                    crate::deputies::display_nickname(
                                        &crate::api::unseal_nickname_display(
                                            &info.member_info.preferred_nickname,
                                            &secrets,
                                        ),
                                    )
                                })
                                .unwrap_or(author_short);

                            let datetime: DateTime<Utc> = msg.message.time.into();
                            let local_time: DateTime<Local> = datetime.into();

                            // Get display content (handles edits, non-text
                            // public content like join events, and — via
                            // `secrets` — decrypted private-room bodies; only a
                            // body whose secret is unavailable renders as
                            // "<encrypted>"). The `_for_terminal` variant also
                            // escapes any `@mention` nickname substituted into
                            // the text — a mentioned member's nickname is
                            // attacker-controlled just like the author's
                            // (freenet/river#474). The JSON arm below keeps its
                            // own separate `message_display_text_with_secrets`
                            // call and stays raw.
                            let content = crate::api::message_display_text_for_terminal(
                                &room_state,
                                msg,
                                &secrets,
                            );

                            // Check if message is edited
                            let msg_id = msg.id();
                            let edited = room_state.recent_messages.is_edited(&msg_id);
                            let edited_indicator = if edited { " (edited)" } else { "" };

                            // Check for reply context (shared with the monitor
                            // stream via crate::api::reply_context_display so the
                            // two renderings can't drift, including the truncation
                            // marker appended by truncate_reply_preview).
                            let reply_prefix = crate::api::reply_prefix_display(
                                &crate::api::reply_context_display_with_secrets(
                                    &room_state,
                                    msg,
                                    &secrets,
                                ),
                            );

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

                            // `canonical`, not a bare `.find()` (#411 round 8 item A).
                            let nickname = room_state
                                .member_info
                                .canonical(msg.message.author)
                                .map(|info| {
                                    crate::api::unseal_nickname_display(
                                        &info.member_info.preferred_nickname,
                                        &secrets,
                                    )
                                });

                            let datetime: DateTime<Utc> = msg.message.time.into();

                            // Get display content (handles edits, non-text
                            // public content like join events, and — via
                            // `secrets` — decrypted private-room bodies; only a
                            // body whose secret is unavailable renders as
                            // "<encrypted>")
                            let content = crate::api::message_display_text_with_secrets(
                                &room_state,
                                msg,
                                &secrets,
                            );

                            // Check edited status
                            let edited = room_state.recent_messages.is_edited(&msg_id);

                            // Get reactions
                            let reactions: std::collections::HashMap<String, usize> = room_state
                                .recent_messages
                                .reactions(&msg_id)
                                .map(|r| r.iter().map(|(k, v)| (k.clone(), v.len())).collect())
                                .unwrap_or_default();
                            // Who reacted, alongside the counts. See
                            // `output_reaction_change` for why a count alone is
                            // not enough to act on a reaction.
                            let reactors: std::collections::HashMap<String, Vec<String>> =
                                room_state
                                    .recent_messages
                                    .reactions(&msg_id)
                                    .map(|r| {
                                        r.iter()
                                            .map(|(emoji, ids)| {
                                                (
                                                    emoji.clone(),
                                                    ids.iter().map(|id| id.to_string()).collect(),
                                                )
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();

                            // Encode message ID for use in edit/delete/react commands
                            let message_id_str = msg_id.0 .0.to_string();

                            // Reply context (null for non-replies) — same shape
                            // as the monitor stream's JSON, so a bridge sees
                            // reply_to on both the backfill and the live feed.
                            // Shared helper so the two cannot drift.
                            let reply_to = crate::api::reply_to_json(
                                &crate::api::reply_context_display_with_secrets(
                                    &room_state,
                                    msg,
                                    &secrets,
                                ),
                            );

                            json!({
                                "message_id": message_id_str,
                                "author": author_str,
                                "nickname": nickname,
                                "content": content,
                                "timestamp": datetime.to_rfc3339(),
                                "edited": edited,
                                "reply_to": reply_to,
                                "reactions": reactions,
                                "reactors": reactors,
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

            let reply_message_id = api
                .send_reply(&room_owner_key, target_message_id, message.clone())
                .await?;

            match format {
                OutputFormat::Human => {
                    println!("Reply sent successfully (id: {})", reply_message_id.0 .0)
                }
                // Emit the ID in the SAME form `message list` prints and
                // `message delete` accepts (the inner i64), so a caller can act
                // on its own reply without a second round trip to find it.
                // Automated moderation needs this to delete a reply it sent.
                OutputFormat::Json => println!(
                    r#"{{"status":"success","action":"reply","message_id":"{}"}}"#,
                    reply_message_id.0 .0
                ),
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

#[cfg(test)]
mod tests {
    /// `message list`'s nickname is attacker-controlled (any room member sets
    /// their own) — freenet/river#474. The HUMAN println must escape it via
    /// `display_nickname` (ANSI/CR/bell must not reach the terminal), while
    /// the JSON arm's separate `unseal_nickname_display` call must stay raw:
    /// escaping it there would corrupt the value for a bridge/consumer, since
    /// JSON's own string escaping already makes it safe.
    ///
    /// Source-scrape, not a live round trip: this file has no stdout-capture
    /// harness. Split on the unique `OutputFormat::Json => {` for `message
    /// list` (verified unique in production) so the human/json regions can't
    /// be confused with each other or with the unrelated Human/Json arms of
    /// the other `message` subcommands later in this file.
    ///
    /// The plain `.split_once("mod tests")` cut below (rather than the
    /// `production_source` brace-aware stripper used elsewhere in this repo)
    /// is only equivalent to it because this file has exactly ONE
    /// `#[cfg(test)] mod tests` block, at the very end, with no earlier
    /// `#[cfg(test)]` item. If an earlier `#[cfg(test)]` item is ever added
    /// above this point, this cut would stop excluding it and the pin could
    /// silently start matching against test-only code.
    #[test]
    fn message_list_escapes_nickname_for_human_and_keeps_json_raw() {
        let source = include_str!("message.rs");
        let production = source
            .split_once("mod tests")
            .map(|(before, _)| before)
            .expect("test module marker missing; the cut would scan everything");
        assert_eq!(
            production.matches("OutputFormat::Json => {").count(),
            1,
            "split anchor must be unique or this pin scrapes the wrong region"
        );
        let (human_region, json_and_after) = production
            .split_once("OutputFormat::Json => {")
            .expect("anchor not found; the pin would scan nothing");

        assert!(
            human_region.contains("display_nickname("),
            "the message list HUMAN branch must escape the nickname"
        );
        assert!(
            json_and_after.contains("unseal_nickname_display("),
            "the message list JSON branch must still resolve a nickname; \
             the pin would pass vacuously otherwise"
        );
        assert!(
            !json_and_after.contains("display_nickname("),
            "the message list JSON branch's nickname must stay raw — \
             escaping it here would corrupt the value for a bridge/consumer"
        );
    }

    /// `message reply --format json` must return the new message's ID, and in
    /// the SAME form `message list` prints and `message delete` accepts.
    ///
    /// Without it a caller cannot act on a reply it just sent: the response was
    /// `{"status":"success","action":"reply"}` and nothing else, so identifying
    /// your own reply meant scanning the room and guessing. Automated moderation
    /// needs to delete a reply it posted, which is what motivated this.
    ///
    /// Source-scrape rather than a live round trip, because sending a reply
    /// requires a node and a room. Whitespace is squashed so rustfmt cannot
    /// silently disarm the pin.

    /// Reactions must be attributable. Room state has always held
    /// `HashMap<String, Vec<MemberId>>`, but both JSON boundaries collapsed it
    /// to a count via `v.len()`, so nothing downstream could tell WHO reacted --
    /// which made reactions impossible to moderate rather than merely awkward.
    ///
    /// `reactions` (counts) is retained unchanged so existing consumers keep
    /// working; `reactors` is additive.
    #[test]
    fn reaction_json_exposes_who_reacted_not_just_how_many() {
        let source = include_str!("message.rs");
        // Scan production only: without this cut the needles below match their
        // own literals in this test and the pin passes vacuously.
        let production = source
            .split_once("mod tests")
            .map(|(before, _)| before)
            .expect("test module marker missing; the cut would scan everything");
        let squashed: String = production.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(
            squashed.contains(r#""reactions":reactions,"#),
            "count field not found; the pin would pass vacuously"
        );
        assert!(
            squashed.contains(r#""reactors":reactors,"#),
            "message list must expose who reacted, not only a count"
        );
    }

    /// `message send` must return the new message's ID for the same reason
    /// `message reply` does: a caller that posts a notice needs to retract it
    /// once it stops being relevant, and only the author may delete a message.
    #[test]
    fn send_json_returns_the_new_message_id() {
        let source = include_str!("message.rs");
        let production = source
            .split_once("mod tests")
            .map(|(before, _)| before)
            .expect("test module marker missing; the cut would scan everything");
        let squashed: String = production.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squashed.contains(r#""status":"success","message":"sent""#),
            "send JSON arm not found; the pin would pass vacuously"
        );
        assert!(
            squashed.contains(r#""message":"sent","message_id":"{}""#),
            "send must return message_id"
        );
        assert!(
            squashed.contains("sent_message_id.0.0"),
            "send must emit the inner i64, matching what `message delete` accepts"
        );
    }

    #[test]
    fn reply_json_returns_the_new_message_id() {
        let source = include_str!("message.rs");
        // Scan ONLY production code. Without this cut the assertions below match
        // their OWN string literals in this test, so the pin passes no matter
        // what the reply arm actually prints -- verified by mutation.
        let production = source
            .split_once("mod tests")
            .map(|(before, _)| before)
            .expect("test module marker missing; the cut would scan everything");
        let squashed: String = production.chars().filter(|c| !c.is_whitespace()).collect();

        // Vacuity guard: if the reply arm is ever renamed or moved out of this
        // file, every assertion below would pass without checking anything.
        assert!(
            squashed.contains(r#""status":"success","action":"reply""#),
            "reply JSON arm not found in this file; the pin would pass vacuously"
        );

        assert!(
            squashed.contains(r#""action":"reply","message_id":"{}""#),
            "reply JSON must carry message_id"
        );

        // `message list` emits the inner i64 (`msg_id.0 .0`) and `message
        // delete` parses that same form, so the reply must not print the
        // Display form of MessageId, which is the debug-formatted FastHash.
        assert!(
            squashed.contains("reply_message_id.0.0"),
            "reply must emit the inner i64, matching what `message delete` accepts"
        );
    }
}
