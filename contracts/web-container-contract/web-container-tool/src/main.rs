use clap::{Parser, Subcommand};
use common::crypto_values::CryptoValue;
use common::web_container::WebContainerMetadata;
use ed25519_dalek::{SigningKey, Signer};
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "web-container-tool")]
#[command(about = "Web container key management tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate a new keypair and save to config
    Generate,
    /// Sign a compressed webapp file
    Sign {
        /// Input compressed webapp file (e.g. webapp.tar.xz)
        #[arg(long, short)]
        input: String,
        /// Output file for metadata
        #[arg(long, short)]
        output: String,
        /// Version number for the webapp
        #[arg(long, short)]
        version: u32,
    },
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

fn get_config_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut config_dir = dirs::config_dir()
        .ok_or("Could not find config directory")?;
    config_dir.push("river");
    config_dir.push("web-container-keys.toml");
    Ok(config_dir)
}

fn read_signing_key() -> Result<SigningKey, Box<dyn std::error::Error>> {
    let config_path = get_config_path()?;
    let config_str = fs::read_to_string(config_path)?;
    let config: toml::Table = toml::from_str(&config_str)?;
    
    let signing_key_str = config["keys"]["signing_key"]
        .as_str()
        .ok_or("Could not find signing key in config")?;
    
    let signing_key = signing_key_str.parse::<CryptoValue>()?;
    
    match signing_key {
        CryptoValue::SigningKey(sk) => Ok(sk),
        _ => Err("Invalid key type in config".into()),
    }
}

fn sign_webapp(
    input: String,
    output: String,
    version: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read the signing key
    let signing_key = read_signing_key()?;
    
    // Read the compressed webapp
    let webapp_bytes = fs::read(&input)?;
    
    // Create message to sign (version + webapp)
    let mut message = version.to_be_bytes().to_vec();
    message.extend_from_slice(&webapp_bytes);
    
    // Sign the message
    let signature = signing_key.sign(&message);
    
    // Create metadata
    let metadata = WebContainerMetadata {
        version,
        signature,
    };
    
    // Create output file
    let mut output_file = fs::File::create(&output)?;
    
    // Write metadata
    ciborium::ser::into_writer(&metadata, &mut output_file)?;

    println!("Metadata written to: {}", output);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Generate => generate_keys(),
        Commands::Sign { input, output, version } => sign_webapp(input, output, version),
    }
}
