use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use colored::Colorize;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::ContractKey;

#[derive(Subcommand)]
pub enum InviteCommands {
    /// Create an invitation for a room
    Create {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
    /// Accept an invitation
    Accept {
        /// Invitation code
        invitation_code: String,

        /// Your nickname in the room
        #[arg(short = 'N', long)]
        nickname: Option<String>,
    },
}

pub async fn execute(command: InviteCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        InviteCommands::Create { room_owner_key } => {
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

            if !matches!(format, OutputFormat::Json) {
                eprintln!("Creating invitation for room owned by: {}", room_owner_key);
            }

            match api.create_invitation(&owner_vk).await {
                Ok(invitation_code) => {
                    match format {
                        OutputFormat::Human => {
                            println!("{}", "Invitation created successfully!".green());
                            println!("\nInvitation code:");
                            println!("{}", invitation_code.bright_yellow());
                            println!("\nShare this code with someone to invite them to the room.");
                            println!("They can accept it with:");
                            println!("  riverctl invite accept {}", invitation_code);
                        }
                        OutputFormat::Json => {
                            println!(
                                r#"{{"status": "success", "invitation_code": "{}"}}"#,
                                invitation_code
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
        InviteCommands::Accept {
            invitation_code,
            nickname,
        } => {
            let nickname = resolve_nickname(nickname)?;

            if !matches!(format, OutputFormat::Json) {
                eprintln!("Accepting invitation...");
            }

            match api.accept_invitation(&invitation_code, &nickname).await {
                Ok((room_owner_vk, contract_key)) => {
                    print_invitation_accepted(format, &room_owner_vk, &contract_key);
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

/// Resolve the invitee nickname: use the value the user passed, else prompt
/// interactively on a TTY, else fall back to "Anonymous". Shared by
/// `invite accept` and `dm accept` so both entry points behave identically.
pub fn resolve_nickname(nickname: Option<String>) -> Result<String> {
    match nickname {
        Some(n) => Ok(n),
        None => {
            if atty::is(atty::Stream::Stdin) {
                Ok(dialoguer::Input::<String>::new()
                    .with_prompt("Enter your nickname")
                    .default("Anonymous".to_string())
                    .interact_text()?)
            } else {
                Ok("Anonymous".to_string())
            }
        }
    }
}

/// Print the success report after an invitation is accepted. Shared by
/// `invite accept` and `dm accept` so the post-accept guidance stays
/// identical regardless of how the invitation reached the user.
pub fn print_invitation_accepted(
    format: OutputFormat,
    room_owner_vk: &VerifyingKey,
    contract_key: &ContractKey,
) {
    let owner_key_str = bs58::encode(room_owner_vk.as_bytes()).into_string();

    match format {
        OutputFormat::Human => {
            println!("{}", "Invitation accepted successfully!".green());
            println!("Room owner key: {}", owner_key_str);
            println!("Contract key: {}", contract_key.id());
            println!("\nYou can now:");
            println!(
                "  - Send messages: riverctl message send {} \"Hello!\"",
                owner_key_str
            );
            println!("  - List members: riverctl member list {}", owner_key_str);
        }
        OutputFormat::Json => {
            println!(
                r#"{{"status": "success", "room_owner_key": "{}", "contract_key": "{}"}}"#,
                owner_key_str,
                contract_key.id()
            );
        }
    }
}
