use clap::{Parser, Subcommand};
use river_common::crypto_values::CryptoValue;
use river_common::web_container::WebContainerMetadata;
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
        /// Output file for contract parameters
        #[arg(long)]
        parameters: String,
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
    let config_str = fs::read_to_string(&config_path)?;
    println!("Read config from: {}", config_path.display());
    
    let config: toml::Table = toml::from_str(&config_str)?;
    println!("Parsed TOML config");
    
    let signing_key_str = config["keys"]["signing_key"]
        .as_str()
        .ok_or("Could not find signing key in config")?;
    println!("Found signing key string: {}", signing_key_str);
    
    // Remove the "river:v1:sk:" prefix
    let key_data = signing_key_str.strip_prefix("river:v1:sk:")
        .ok_or("Signing key must start with 'river:v1:sk:'")?;
    println!("Stripped prefix, parsing key data: {}", key_data);
    
    println!("Attempting to parse key data as CryptoValue: {}", key_data);
    let signing_key = key_data.parse::<CryptoValue>()
        .map_err(|e| format!("Failed to parse signing key data: {}", e))?;
    println!("Successfully parsed as CryptoValue: {:?}", signing_key);
    
    match signing_key {
        CryptoValue::SigningKey(sk) => {
            println!("Successfully extracted SigningKey");
            Ok(sk)
        },
        other => Err(format!("Expected SigningKey, got {:?}", other).into()),
    }
}

fn sign_webapp(
    input: String,
    output: String,
    parameters: String,
    version: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read the signing key
    let signing_key = match read_signing_key() {
        Ok(key) => {
            println!("Read signing key successfully");
            key
        },
        Err(e) => return Err(format!("Failed to read signing key: {}", e).into()),
    };
    
    // Read the compressed webapp
    let webapp_bytes = match fs::read(&input) {
        Ok(bytes) => {
            println!("Read {} bytes from webapp file", bytes.len());
            bytes
        },
        Err(e) => return Err(format!("Failed to read webapp file '{}': {}", input, e).into()),
    };
    
    // Create message to sign (version + webapp)
    let mut message = Vec::new();
    message.extend_from_slice(&version.to_be_bytes());
    message.extend_from_slice(&webapp_bytes);
    println!("Created message to sign: {} bytes total ({} bytes version + {} bytes webapp)", 
             message.len(), std::mem::size_of::<u32>(), webapp_bytes.len());
    
    // Output debug info
    let verifying_key = signing_key.verifying_key();
    println!("Verifying key (base58): {}", bs58::encode(verifying_key.to_bytes()).into_string());
    println!("Message length: {} bytes", message.len());
    if message.len() > 20 {
        println!("Message first 10 bytes (base58): {}", bs58::encode(&message[..10]).into_string());
        println!("Message last 10 bytes (base58): {}", bs58::encode(&message[message.len()-10..]).into_string());
    } else {
        println!("Message (base58): {}", bs58::encode(&message).into_string());
    }

    // Sign the message
    let signature = signing_key.sign(&message);
    println!("Generated signature (base58): {}", bs58::encode(signature.to_bytes()).into_string());
    println!("Signature length: {} bytes", signature.to_bytes().len());
    
    // Create metadata
    let metadata = WebContainerMetadata {
        version,
        signature,
    };
    println!("Created metadata struct with version {}", version);
    
    // Create output file
    let mut output_file = match fs::File::create(&output) {
        Ok(file) => {
            println!("Created output file: {}", output);
            file
        },
        Err(e) => return Err(format!("Failed to create output file '{}': {}", output, e).into()),
    };
    
    // Write metadata
    match ciborium::ser::into_writer(&metadata, &mut output_file) {
        Ok(_) => println!("Successfully wrote metadata to file"),
        Err(e) => return Err(format!("Failed to write metadata to '{}': {}", output, e).into()),
    }

    println!("Metadata written to: {}", output);

    // Write parameters file containing verifying key bytes
    let verifying_key = signing_key.verifying_key();
    fs::write(&parameters, verifying_key.to_bytes())
        .map_err(|e| format!("Failed to write parameters to '{}': {}", parameters, e))?;
    println!("Parameters written to: {}", parameters);
    
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Generate => generate_keys(),
        Commands::Sign { input, output, parameters, version } => sign_webapp(input, output, parameters, version),
    }
}
