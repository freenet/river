use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer, SigningKey};
use river_core::crypto_values::CryptoValue;
use river_core::web_container::WebContainerMetadata;
use std::fs;
use std::io::Write;
#[cfg(test)]
use std::path::Path;
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
    /// Generate a new keypair and save to config or file
    Generate {
        /// Output file for keys (default: ~/.config/river/web-container-keys.toml)
        #[arg(long, short)]
        output: Option<String>,
    },
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
        /// Key file to use (default: ~/.config/river/web-container-keys.toml)
        #[arg(long, short)]
        key_file: Option<String>,
    },
    /// Write the web-container contract parameters (the signing identity's
    /// verifying-key bytes) without needing a webapp archive to sign.
    ///
    /// The contract parameters are exactly the 32-byte verifying key, and the
    /// contract ID is derived from `(web_container_contract.wasm, parameters)`.
    /// `compress-webapp-test` needs the test contract ID *before* it builds the
    /// UI (to bake the correct `base_path`/`DIOXUS_ASSET_ROOT` into the WASM),
    /// which is a chicken-and-egg with `sign` (sign needs the built archive).
    /// This command breaks that cycle. See freenet/river#257.
    ExportParameters {
        /// Output file for contract parameters
        #[arg(long)]
        parameters: String,
        /// Key file to use (default: ~/.config/river/web-container-keys.toml)
        #[arg(long, short)]
        key_file: Option<String>,
    },
}

fn generate_keys(output_path: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    // Generate keys
    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();
    let signing_key_str = CryptoValue::SigningKey(signing_key).to_encoded_string();
    let verifying_key_str = CryptoValue::VerifyingKey(verifying_key).to_encoded_string();

    // Create config structure
    let config = toml::toml! {
        [keys]
        signing_key = signing_key_str
        verifying_key = verifying_key_str
    };

    // Determine output path
    let config_path = if let Some(path) = output_path {
        PathBuf::from(path)
    } else {
        // Default to ~/.config/river/web-container-keys.toml
        let mut config_dir = dirs::config_dir().ok_or("Could not find config directory")?;
        config_dir.push("river");
        fs::create_dir_all(&config_dir)?;
        config_dir.push("web-container-keys.toml");
        config_dir
    };

    // Create parent directory if needed
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Write config file
    fs::write(&config_path, toml::to_string(&config)?)?;
    println!("Keys written to: {}", config_path.display());

    Ok(())
}

fn get_config_path(key_file: Option<&str>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = key_file {
        Ok(PathBuf::from(path))
    } else {
        let mut config_dir = dirs::config_dir().ok_or("Could not find config directory")?;
        config_dir.push("river");
        config_dir.push("web-container-keys.toml");
        Ok(config_dir)
    }
}

fn read_signing_key(key_file: Option<&str>) -> Result<SigningKey, Box<dyn std::error::Error>> {
    let config_path = get_config_path(key_file)?;
    let config_str = fs::read_to_string(&config_path)?;
    tracing::info!("Read config from: {}", config_path.display());

    let config: toml::Table = toml::from_str(&config_str)?;
    tracing::info!("Parsed TOML config");

    let signing_key_str = config["keys"]["signing_key"]
        .as_str()
        .ok_or("Could not find signing key in config")?;
    tracing::info!("Found signing key string: {}", signing_key_str);

    // Remove the "river:v1:sk:" prefix
    let key_data = signing_key_str
        .strip_prefix("river:v1:sk:")
        .ok_or("Signing key must start with 'river:v1:sk:'")?;
    tracing::info!("Stripped prefix, parsing key data: {}", key_data);

    tracing::info!("Attempting to parse key data as CryptoValue: {}", key_data);
    let signing_key = key_data
        .parse::<CryptoValue>()
        .map_err(|e| format!("Failed to parse signing key data: {}", e))?;
    tracing::info!("Successfully parsed as CryptoValue: {:?}", signing_key);

    match signing_key {
        CryptoValue::SigningKey(sk) => {
            tracing::info!("Successfully extracted SigningKey");
            Ok(sk)
        }
        other => Err(format!("Expected SigningKey, got {:?}", other).into()),
    }
}

fn sign_webapp(
    input: String,
    output: String,
    parameters: String,
    version: u32,
    key_file: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read the signing key
    let signing_key = match read_signing_key(key_file.as_deref()) {
        Ok(key) => {
            tracing::info!("Read signing key successfully");
            key
        }
        Err(e) => return Err(format!("Failed to read signing key: {}", e).into()),
    };

    // Read the compressed webapp
    let webapp_bytes = match fs::read(&input) {
        Ok(bytes) => {
            tracing::info!("Read {} bytes from webapp file", bytes.len());
            bytes
        }
        Err(e) => return Err(format!("Failed to read webapp file '{}': {}", input, e).into()),
    };

    // Create message to sign (version + webapp)
    let mut message = Vec::new();
    message.extend_from_slice(&version.to_be_bytes());
    message.extend_from_slice(&webapp_bytes);

    tracing::info!(
        "Created message to sign: {} bytes total ({} bytes version + {} bytes webapp)",
        message.len(),
        std::mem::size_of::<u32>(),
        webapp_bytes.len()
    );
    tracing::debug!("Version bytes (hex): {:02x?}", &version.to_be_bytes());
    tracing::debug!(
        "First 100 webapp bytes (hex): {:02x?}",
        &webapp_bytes[..100.min(webapp_bytes.len())]
    );
    tracing::debug!(
        "First 100 message bytes (hex): {:02x?}",
        &message[..100.min(message.len())]
    );

    // Output debug info
    let verifying_key = signing_key.verifying_key();
    tracing::debug!(
        "Verifying key (base58): {}",
        bs58::encode(verifying_key.to_bytes()).into_string()
    );
    tracing::debug!("Verifying key (hex): {:02x?}", verifying_key.to_bytes());
    tracing::info!("Message length: {} bytes", message.len());
    if message.len() > 20 {
        tracing::debug!(
            "Message first 10 bytes (base58): {}",
            bs58::encode(&message[..10]).into_string()
        );
        tracing::debug!(
            "Message last 10 bytes (base58): {}",
            bs58::encode(&message[message.len() - 10..]).into_string()
        );
    } else {
        tracing::debug!("Message (base58): {}", bs58::encode(&message).into_string());
    }

    // Sign the message
    let signature = signing_key.sign(&message);
    tracing::info!(
        "Generated signature (base58): {}",
        bs58::encode(signature.to_bytes()).into_string()
    );
    tracing::info!("Signature length: {} bytes", signature.to_bytes().len());

    // Create metadata
    let metadata = WebContainerMetadata { version, signature };
    tracing::info!("Created metadata struct with version {}", version);

    // Serialize metadata to check exact bytes
    let mut metadata_bytes = Vec::new();
    ciborium::ser::into_writer(&metadata, &mut metadata_bytes)
        .map_err(|e| format!("Failed to serialize metadata: {}", e))?;
    tracing::debug!("Serialized metadata size: {} bytes", metadata_bytes.len());
    tracing::debug!(
        "First 32 metadata bytes (hex): {:02x?}",
        &metadata_bytes[..32.min(metadata_bytes.len())]
    );

    // Create output file
    let mut output_file = match fs::File::create(&output) {
        Ok(file) => {
            tracing::info!("Created output file: {}", output);
            file
        }
        Err(e) => return Err(format!("Failed to create output file '{}': {}", output, e).into()),
    };

    // Serialize and write metadata as CBOR
    let mut metadata_bytes = Vec::new();
    ciborium::ser::into_writer(&metadata, &mut metadata_bytes)
        .map_err(|e| format!("Failed to serialize metadata: {}", e))?;

    output_file
        .write_all(&metadata_bytes)
        .map_err(|e| format!("Failed to write metadata: {}", e))?;
    if metadata_bytes.len() > 64 {
        tracing::debug!(
            "First 32 metadata bytes (hex): {:02x?}",
            &metadata_bytes[..32]
        );
        tracing::debug!(
            "Last 32 metadata bytes (hex): {:02x?}",
            &metadata_bytes[metadata_bytes.len() - 32..]
        );
    } else {
        tracing::debug!("Metadata bytes (hex): {:02x?}", &metadata_bytes);
    }

    println!("Metadata written to: {}", output);

    // Write parameters file containing verifying key bytes
    write_parameters(&signing_key, &parameters)?;

    Ok(())
}

/// Write the contract parameters file: the raw 32-byte verifying key.
///
/// This is the single source of truth for what the parameters file contains.
/// The web-container contract ID is `derive(web_container_contract.wasm,
/// parameters)`, so the parameters must be byte-identical regardless of which
/// command produced them (`sign` or `export-parameters`).
fn write_parameters(
    signing_key: &SigningKey,
    parameters: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let verifying_key = signing_key.verifying_key();
    fs::write(parameters, verifying_key.to_bytes())
        .map_err(|e| format!("Failed to write parameters to '{}': {}", parameters, e))?;
    println!("Parameters written to: {}", parameters);
    Ok(())
}

/// Export the contract parameters (verifying-key bytes) from a key file,
/// without signing a webapp archive.
fn export_parameters(
    parameters: String,
    key_file: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let signing_key = read_signing_key(key_file.as_deref())
        .map_err(|e| format!("Failed to read signing key: {}", e))?;
    write_parameters(&signing_key, &parameters)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Generate { output } => generate_keys(output),
        Commands::Sign {
            input,
            output,
            parameters,
            version,
            key_file,
        } => sign_webapp(input, output, parameters, version, key_file),
        Commands::ExportParameters {
            parameters,
            key_file,
        } => export_parameters(parameters, key_file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "river-web-container-tool-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_key_file(dir: &Path, signing_key: &SigningKey) -> PathBuf {
        let signing_key_str = CryptoValue::SigningKey(signing_key.clone()).to_encoded_string();
        let verifying_key_str =
            CryptoValue::VerifyingKey(signing_key.verifying_key()).to_encoded_string();
        let config = toml::toml! {
            [keys]
            signing_key = signing_key_str
            verifying_key = verifying_key_str
        };
        let path = dir.join("keys.toml");
        fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        path
    }

    /// The contract ID is `derive(wasm, parameters)`, and `compress-webapp-test`
    /// derives the test ID from parameters written by `export-parameters` while
    /// `sign-webapp-test` later writes them via `sign`. If the two ever produced
    /// different parameter bytes, the baked base_path would target a different
    /// contract than the one actually published — exactly the class of bug #257
    /// is about. Pin that they are byte-identical (both are the verifying key).
    #[test]
    fn export_parameters_matches_sign_parameters() {
        let dir = tmpdir("export-params");
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let key_file = write_key_file(&dir, &signing_key);

        // export-parameters path
        let export_path = dir.join("export.parameters");
        export_parameters(
            export_path.to_str().unwrap().to_string(),
            Some(key_file.to_str().unwrap().to_string()),
        )
        .unwrap();

        // sign path (needs an archive to sign; contents are irrelevant to params)
        let archive = dir.join("webapp.tar.xz");
        fs::write(&archive, b"not a real archive, irrelevant to parameters").unwrap();
        let sign_params = dir.join("sign.parameters");
        sign_webapp(
            archive.to_str().unwrap().to_string(),
            dir.join("metadata").to_str().unwrap().to_string(),
            sign_params.to_str().unwrap().to_string(),
            1,
            Some(key_file.to_str().unwrap().to_string()),
        )
        .unwrap();

        let exported = fs::read(&export_path).unwrap();
        let signed = fs::read(&sign_params).unwrap();
        assert_eq!(
            exported,
            signing_key.verifying_key().to_bytes().to_vec(),
            "export-parameters must write the raw verifying key"
        );
        assert_eq!(
            exported, signed,
            "export-parameters and sign must produce byte-identical parameters"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
