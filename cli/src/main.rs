use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod api;
mod commands;
mod config;
mod error;
mod output;
mod storage;

use crate::commands::{debug, invite, member, message, room};

#[derive(Parser)]
#[command(name = "river")]
#[command(about = "Command-line interface for River chat on Freenet")]
#[command(version)]
#[command(arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Output format (human, json)
    #[arg(short, long, global = true, default_value = "human")]
    format: output::OutputFormat,

    /// Freenet node WebSocket URL
    #[arg(
        long,
        global = true,
        default_value = "ws://127.0.0.1:7509/v1/contract/command?encodingProtocol=native"
    )]
    node_url: String,

    /// Configuration directory for storing room data
    #[arg(long, global = true)]
    config_dir: Option<String>,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    debug: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Room management commands
    Room {
        #[command(subcommand)]
        command: room::RoomCommands,
    },
    /// Message commands
    Message {
        #[command(subcommand)]
        command: message::MessageCommands,
    },
    /// Member management commands
    Member {
        #[command(subcommand)]
        command: member::MemberCommands,
    },
    /// Invitation commands
    Invite {
        #[command(subcommand)]
        command: invite::InviteCommands,
    },
    /// Debug commands for troubleshooting
    Debug {
        #[command(subcommand)]
        command: debug::DebugCommands,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let filter = if cli.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::from_default_env()
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    info!("River starting...");

    // Load configuration
    let config = config::Config::load()?;

    // Create API client
    let api_client = api::ApiClient::new(&cli.node_url, config, cli.config_dir.as_deref()).await?;

    // Execute command
    match cli.command {
        Commands::Room { command } => room::execute(command, api_client, cli.format).await?,
        Commands::Message { command } => message::execute(command, api_client, cli.format).await?,
        Commands::Member { command } => member::execute(command, api_client, cli.format).await?,
        Commands::Invite { command } => invite::execute(command, api_client, cli.format).await?,
        Commands::Debug { command } => debug::execute(command, api_client, cli.format).await?,
    }

    Ok(())
}
