use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use riverctl::{
    api,
    commands::{debug, invite, member, message, room},
    config, output,
};

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

    /// Optional path to write log output (stdout remains reserved for command output/JSON)
    #[arg(long, global = true, value_name = "PATH", env = "RIVERCTL_LOG_FILE")]
    log_file: Option<PathBuf>,
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

    // Initialize logging (keep stdout clean for user/JSON output)
    let _log_guard = init_logging(cli.debug, cli.log_file.as_deref())?;

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

fn init_logging(debug: bool, log_path: Option<&Path>) -> Result<Option<WorkerGuard>> {
    use std::fs::OpenOptions;
    use tracing_subscriber::{fmt, layer::SubscriberExt, Registry};

    let filter = if debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::from_default_env()
    };

    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(atty::is(atty::Stream::Stderr));

    let registry = Registry::default().with(filter).with(stderr_layer);

    if let Some(path) = log_path {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;
        let (writer, guard) = tracing_appender::non_blocking(file);
        let file_layer = fmt::layer().with_ansi(false).with_writer(writer);
        let subscriber = registry.with(file_layer);
        tracing::subscriber::set_global_default(subscriber)
            .context("failed to install tracing subscriber")?;
        Ok(Some(guard))
    } else {
        tracing::subscriber::set_global_default(registry)
            .context("failed to install tracing subscriber")?;
        Ok(None)
    }
}
