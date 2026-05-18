use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use ed25519_dalek::SigningKey;
use std::path::{Path, PathBuf};
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use riverctl::{
    api,
    commands::{debug, dm, identity, invite, member, message, room},
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

    /// Override the signing identity for this command only. Reads a raw
    /// 32-byte Ed25519 secret key from the given file path.
    ///
    /// The override is in-memory: it does NOT modify `rooms.json`. Use
    /// this when you have multiple identities in the same room (e.g.,
    /// room owner + invite bot + alt accounts) and want to pick which
    /// one signs at command time, without the UI's chat-delegate sync
    /// silently rewriting `rooms.json[room].signing_key_bytes` away
    /// from your intended identity. The override key must be a valid
    /// member of the target room or the command will be rejected by
    /// the contract.
    ///
    /// Falls back to the `RIVER_SIGNING_KEY_FILE` env var if the flag
    /// is not passed.
    ///
    /// Distinct from `message send --signing-key`, which takes a
    /// base64-encoded key inline as a single-command override — the
    /// global `--signing-key-file` flag is preferred for non-test use
    /// because the key doesn't appear in shell history.
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        env = "RIVER_SIGNING_KEY_FILE"
    )]
    signing_key_file: Option<PathBuf>,
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
    /// Identity export/import commands
    Identity {
        #[command(subcommand)]
        command: identity::IdentityCommands,
    },
    /// Debug commands for troubleshooting
    Debug {
        #[command(subcommand)]
        command: debug::DebugCommands,
    },
    /// In-room direct message commands
    Dm {
        #[command(subcommand)]
        command: dm::DmCommands,
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

    // Resolve optional --signing-key-file override (or RIVER_SIGNING_KEY_FILE env var).
    let signing_key_override = cli
        .signing_key_file
        .as_deref()
        .map(load_signing_key_from_file)
        .transpose()?;

    // Create API client
    let api_client = api::ApiClient::new_with_signing_key_override(
        &cli.node_url,
        config,
        cli.config_dir.as_deref(),
        signing_key_override,
    )
    .await?;

    // Execute command
    match cli.command {
        Commands::Room { command } => room::execute(command, api_client, cli.format).await?,
        Commands::Message { command } => message::execute(command, api_client, cli.format).await?,
        Commands::Member { command } => member::execute(command, api_client, cli.format).await?,
        Commands::Invite { command } => invite::execute(command, api_client, cli.format).await?,
        Commands::Identity { command } => {
            identity::execute(command, api_client, cli.format).await?
        }
        Commands::Debug { command } => debug::execute(command, api_client, cli.format).await?,
        Commands::Dm { command } => dm::execute(command, api_client, cli.format).await?,
    }

    Ok(())
}

/// Load a raw 32-byte Ed25519 secret key from the given file path.
/// Used by the `--signing-key-file` flag / `RIVER_SIGNING_KEY_FILE` env var.
/// Errors are surfaced with a clear message identifying the bad path
/// and the actual length seen, so the user can tell "I pointed at the
/// wrong file" from "I pointed at a base64-encoded file".
fn load_signing_key_from_file(path: &Path) -> Result<SigningKey> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read signing key file: {}", path.display()))?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "signing key file {} must be exactly 32 raw bytes, got {} bytes \
             (was this file base64- or hex-encoded? the override expects raw bytes, \
             matching the format written by `riverctl identity export` and the keys \
             in ~/.config/freenet-river-official/*.bin)",
            path.display(),
            bytes.len()
        ));
    }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&buf))
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
