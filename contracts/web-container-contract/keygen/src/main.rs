use ed25519_dalek::{SigningKey, VerifyingKey};
use river_common::crypto_values::CryptoValue;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Generate keys
    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();

    // Create config structure
    let config = toml::toml! {
        [keys]
        signing_key = CryptoValue::SigningKey(signing_key).to_encoded_string()
        verifying_key = CryptoValue::VerifyingKey(verifying_key).to_encoded_string()
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
