use anyhow::Result;
use clap::Subcommand;
use crate::api::ApiClient;
use crate::output::OutputFormat;

#[derive(Subcommand)]
pub enum MemberCommands {
    /// List members of a room
    List {
        /// Room ID
        room_id: String,
    },
}

pub async fn execute(command: MemberCommands, _api: ApiClient, _format: OutputFormat) -> Result<()> {
    match command {
        MemberCommands::List { room_id } => {
            println!("Listing members of room: {}", room_id);
            // TODO: Implement member listing
            Ok(())
        }
    }
}