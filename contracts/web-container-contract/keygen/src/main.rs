use clap::{Parser, Subcommand};
use common::crypto_values::CryptoValue;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "keygen")]
#[command(about = "Web container key management tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate a new keypair and save to config
    Generate,
}

fn generate_keys() -> Result<(), Box<dyn std::error::Error>> {
    // Generate keys
    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();
    let signing_key = CryptoValue::SigningKey(signing_key).to_encoded_string();
    let verifying_key = CryptoValue::VerifyingKey(verifying_key).to_encoded_string();

    // Create config structure
    let config = toml::toml! {
        [keys]
        signing_key = signing_key
        verifying_key = verifying_key
    };

    // Get config directory
    let mut config_dir = dirs::config_dir()
        .ok_or("Could not find config directory")?;
    config_dir.push("river");

    // Create directory if it doesn't exist
    fs::create_dir_all(&config_dir)?;

    // Create config file path
    let mut config_path = config_dir;
    config_path.push("web-container-keys.toml");

    // Write config file
    fs::write(&config_path, toml::to_string(&config)?)?;
    println!("Keys written to: {}", config_path.display());

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Generate => generate_keys(),
    }
}
