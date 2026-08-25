use clap::{Parser, Subcommand};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
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
    /// Read a PACKED web-container state (the bytes `fdev execute get` returns)
    /// and print its version, so the publish path can compare its intended
    /// version against the one the network actually holds.
    ///
    /// This exists because the publish path had no way to answer "what version
    /// is live right now?". Without it, `sign-webapp` signed at
    /// `local_counter + 1` and `publish-river` trusted `fdev`'s exit code —
    /// and on 2026-08-04 (River commit 1032d373) that combination signed two
    /// DIFFERENT archives at the same version 30000377 and put both on the
    /// network. See scripts/publish-web-container.sh for the whole story.
    ///
    /// Prints `version=<n>` on stdout, in the same `key=value` shape
    /// `pointer-record verify` uses, so callers can `sed -n 's/^version=//p'`.
    Inspect {
        /// Packed state file (`[u64 metadata_len][metadata][u64 web_len][web]`)
        #[arg(long, short)]
        state: String,
        /// Contract parameters (the raw 32-byte verifying key). When given,
        /// the embedded signature MUST verify under it or this fails.
        ///
        /// Pass it. A version read out of unverified bytes is not evidence of
        /// anything: it is whatever the responder chose to say.
        #[arg(long)]
        parameters: Option<String>,
        /// Write the embedded webapp archive here, so the caller can compare
        /// it byte-for-byte against the archive it published.
        #[arg(long)]
        archive_out: Option<String>,
        /// Fail unless the state's version is exactly this.
        #[arg(long)]
        expect_version: Option<u32>,
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

/// The parsed pieces of a packed web-container state.
struct PackedWebApp {
    version: u32,
    signature: Signature,
    archive: Vec<u8>,
}

impl PackedWebApp {
    /// Parse the packed WebApp container:
    /// `[metadata_len: u64 BE][metadata: CBOR][web_len: u64 BE][web]`.
    ///
    /// This layout is defined by `WebApp::pack` in freenet-core and read back
    /// by the web-container contract's own `validate_state`. Those two are the
    /// authority; `sign_output_parses_back_and_verifies` below pins that this
    /// parser still agrees with what `sign` produces.
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        fn take_u64(bytes: &[u8], off: &mut usize) -> Result<u64, String> {
            let end = off
                .checked_add(8)
                .ok_or_else(|| "length field offset overflows".to_string())?;
            let slice = bytes
                .get(*off..end)
                .ok_or_else(|| "state truncated: no room for a length field".to_string())?;
            *off = end;
            let mut buf = [0u8; 8];
            buf.copy_from_slice(slice);
            Ok(u64::from_be_bytes(buf))
        }

        fn take<'a>(bytes: &'a [u8], off: &mut usize, len: u64) -> Result<&'a [u8], String> {
            let len =
                usize::try_from(len).map_err(|_| "declared length exceeds usize".to_string())?;
            let end = off
                .checked_add(len)
                .ok_or_else(|| "declared length overflows".to_string())?;
            let slice = bytes.get(*off..end).ok_or_else(|| {
                format!(
                    "state truncated: declared {len} bytes, {} remain",
                    bytes.len().saturating_sub(*off)
                )
            })?;
            *off = end;
            Ok(slice)
        }

        let mut off = 0usize;
        let metadata_len = take_u64(bytes, &mut off)?;
        let metadata_bytes = take(bytes, &mut off, metadata_len)?;
        let metadata: WebContainerMetadata = ciborium::de::from_reader(metadata_bytes)
            .map_err(|e| format!("metadata is not valid WebContainerMetadata CBOR: {e}"))?;
        let web_len = take_u64(bytes, &mut off)?;
        let archive = take(bytes, &mut off, web_len)?.to_vec();

        Ok(Self {
            version: metadata.version,
            signature: metadata.signature,
            archive,
        })
    }

    /// Verify the state's signature against the contract parameters.
    ///
    /// The parameters file is exactly the 32-byte verifying key (see
    /// `write_parameters`), and the contract ID is derived from
    /// `(wasm, parameters)`. So "verifies under these parameters" means "this
    /// state was signed by the key that owns this contract" — which is the
    /// only reason to believe a version number that came off the network.
    /// An unverified version is not evidence; it is whatever the responder
    /// chose to say.
    fn verify(&self, parameters: &[u8]) -> Result<(), String> {
        let key_bytes: [u8; 32] = parameters
            .try_into()
            .map_err(|_| format!("parameters must be 32 bytes, got {}", parameters.len()))?;
        let verifying_key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|e| format!("parameters are not a valid verifying key: {e}"))?;

        // The signed message is `version || archive`, exactly as `sign_webapp`
        // builds it and `validate_state` re-builds it.
        let mut message = self.version.to_be_bytes().to_vec();
        message.extend_from_slice(&self.archive);

        verifying_key
            .verify_strict(&message, &self.signature)
            .map_err(|e| format!("signature does not verify under the contract parameters: {e}"))
    }
}

fn inspect_state(
    state: String,
    parameters: Option<String>,
    archive_out: Option<String>,
    expect_version: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read(&state).map_err(|e| format!("failed to read state '{state}': {e}"))?;
    let parsed = PackedWebApp::parse(&bytes)?;

    if let Some(parameters) = parameters.as_deref() {
        let params = fs::read(parameters)
            .map_err(|e| format!("failed to read parameters '{parameters}': {e}"))?;
        parsed.verify(&params)?;
    }

    if let Some(expected) = expect_version {
        if parsed.version != expected {
            return Err(format!(
                "state is at version {}, expected {}",
                parsed.version, expected
            )
            .into());
        }
    }

    // Written before the version is printed: a caller that trusts stdout must
    // not be handed a version for a state whose archive it failed to receive.
    if let Some(path) = archive_out.as_deref() {
        fs::write(path, &parsed.archive)
            .map_err(|e| format!("failed to write archive to '{path}': {e}"))?;
    }

    println!("version={}", parsed.version);
    Ok(())
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
        Commands::Inspect {
            state,
            parameters,
            archive_out,
            expect_version,
        } => inspect_state(state, parameters, archive_out, expect_version),
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

    // ---------------------------------------------------------------- inspect
    //
    // `inspect` is what lets the publish path answer "what version is live
    // right now?" — the question nobody could answer on 2026-08-04 (River
    // commit 1032d373), when a timed-out publish was retried and a SECOND,
    // differently-built archive was signed at the same version 30000377. Both
    // ended up on the network. The container's `update_state` gate rejects
    // `version <= current`, which converges DIFFERING versions and does
    // nothing for two states at the SAME one.

    /// Pack a state exactly as freenet-core's `WebApp::pack` does, which is
    /// what `fdev publish --webapp-archive/--webapp-metadata` sends and what
    /// `fdev execute get` hands back.
    fn pack(metadata: &[u8], archive: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(metadata.len() as u64).to_be_bytes());
        out.extend_from_slice(metadata);
        out.extend_from_slice(&(archive.len() as u64).to_be_bytes());
        out.extend_from_slice(archive);
        out
    }

    /// Sign `archive` at `version` and return the packed state, mirroring the
    /// full sign -> publish path.
    fn signed_state(signing_key: &SigningKey, version: u32, archive: &[u8]) -> Vec<u8> {
        let mut message = version.to_be_bytes().to_vec();
        message.extend_from_slice(archive);
        let metadata = WebContainerMetadata {
            version,
            signature: signing_key.sign(&message),
        };
        let mut metadata_bytes = Vec::new();
        ciborium::ser::into_writer(&metadata, &mut metadata_bytes).unwrap();
        pack(&metadata_bytes, archive)
    }

    /// The round trip the publish pre-flight depends on: what `sign` writes,
    /// packed the way the node packs it, must parse back to the same version
    /// and the same archive bytes, and must verify under the parameters `sign`
    /// wrote. If the packed layout ever changes in freenet-core, this fails
    /// here rather than at 3am in the middle of a publish.
    #[test]
    fn sign_output_parses_back_and_verifies() {
        let dir = tmpdir("inspect-roundtrip");
        let signing_key = SigningKey::from_bytes(&[11u8; 32]);
        let key_file = write_key_file(&dir, &signing_key);

        let archive_bytes = b"pretend this is webapp.tar.xz".to_vec();
        let archive = dir.join("webapp.tar.xz");
        fs::write(&archive, &archive_bytes).unwrap();

        let metadata_path = dir.join("webapp.metadata");
        let params_path = dir.join("webapp.parameters");
        sign_webapp(
            archive.to_str().unwrap().to_string(),
            metadata_path.to_str().unwrap().to_string(),
            params_path.to_str().unwrap().to_string(),
            30_000_377,
            Some(key_file.to_str().unwrap().to_string()),
        )
        .unwrap();

        let state = pack(&fs::read(&metadata_path).unwrap(), &archive_bytes);
        let parsed = PackedWebApp::parse(&state).expect("packed state must parse");
        assert_eq!(parsed.version, 30_000_377);
        assert_eq!(parsed.archive, archive_bytes);
        parsed
            .verify(&fs::read(&params_path).unwrap())
            .expect("must verify under the parameters `sign` wrote");

        fs::remove_dir_all(&dir).ok();
    }

    /// A version read from bytes we did not verify is not evidence. Reject a
    /// state signed by any other key, so a wrong-key or substituted response
    /// can never be mistaken for "the network is at version N".
    #[test]
    fn verify_rejects_a_state_signed_by_another_key() {
        let ours = SigningKey::from_bytes(&[1u8; 32]);
        let theirs = SigningKey::from_bytes(&[2u8; 32]);
        let state = signed_state(&theirs, 42, b"archive");
        let parsed = PackedWebApp::parse(&state).unwrap();
        let err = parsed
            .verify(&ours.verifying_key().to_bytes())
            .expect_err("a foreign signature must not verify");
        assert!(err.contains("does not verify"), "unexpected error: {err}");
    }

    /// The signature covers `version || archive`, so swapping either half
    /// invalidates it. This is why the pre-flight reads the version out of a
    /// VERIFIED state rather than out of whatever came back.
    #[test]
    fn verify_rejects_a_state_whose_archive_was_swapped() {
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let good = signed_state(&key, 7, b"the archive that was signed");
        let parsed = PackedWebApp::parse(&good).unwrap();

        // Re-pack the same metadata over a different archive.
        let metadata_len = u64::from_be_bytes(good[..8].try_into().unwrap()) as usize;
        let metadata = &good[8..8 + metadata_len];
        let tampered = pack(metadata, b"a different archive entirely");

        let tampered = PackedWebApp::parse(&tampered).unwrap();
        assert_eq!(tampered.version, parsed.version);
        assert!(tampered.verify(&key.verifying_key().to_bytes()).is_err());
    }

    /// Truncation must be an error, never a silently short read that yields a
    /// plausible-looking version.
    #[test]
    fn parse_rejects_truncated_and_malformed_states() {
        let key = SigningKey::from_bytes(&[4u8; 32]);
        let full = signed_state(&key, 5, b"archive bytes");

        assert!(PackedWebApp::parse(&[]).is_err(), "empty state");
        assert!(PackedWebApp::parse(&full[..4]).is_err(), "half a length");
        assert!(
            PackedWebApp::parse(&full[..full.len() - 1]).is_err(),
            "archive one byte short"
        );

        // A length field that claims far more than the buffer holds must be
        // rejected on the declared length, not trusted into an allocation.
        let mut lying = full.clone();
        lying[..8].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(PackedWebApp::parse(&lying).is_err(), "absurd metadata_len");
    }

    /// `--expect-version` is the caller's assertion that the state it just
    /// read back is the one it published. It has to be able to fail.
    #[test]
    fn inspect_expect_version_rejects_a_mismatch_and_writes_no_archive() {
        let dir = tmpdir("inspect-expect");
        let key = SigningKey::from_bytes(&[5u8; 32]);
        let state_path = dir.join("state.bin");
        fs::write(&state_path, signed_state(&key, 100, b"archive")).unwrap();
        let params_path = dir.join("params");
        fs::write(&params_path, key.verifying_key().to_bytes()).unwrap();
        let archive_out = dir.join("out.tar.xz");

        let err = inspect_state(
            state_path.to_str().unwrap().to_string(),
            Some(params_path.to_str().unwrap().to_string()),
            Some(archive_out.to_str().unwrap().to_string()),
            Some(101),
        )
        .expect_err("a version mismatch must fail");
        assert!(err.to_string().contains("expected 101"), "{err}");
        assert!(
            !archive_out.exists(),
            "no archive should be written when the version assertion failed"
        );

        inspect_state(
            state_path.to_str().unwrap().to_string(),
            Some(params_path.to_str().unwrap().to_string()),
            Some(archive_out.to_str().unwrap().to_string()),
            Some(100),
        )
        .expect("the matching version must succeed");
        assert_eq!(fs::read(&archive_out).unwrap(), b"archive".to_vec());

        fs::remove_dir_all(&dir).ok();
    }

    /// The publish path compares the archive it read back against the archive
    /// it published. That comparison is the ONLY thing that can distinguish
    /// "my state is live" from "a state at my version is live" — the 2026-08-04
    /// fork, where two different archives were both validly signed at 30000377.
    #[test]
    fn same_version_different_archives_both_verify() {
        let key = SigningKey::from_bytes(&[6u8; 32]);
        let first = signed_state(&key, 30_000_377, b"build A of the UI");
        let second = signed_state(&key, 30_000_377, b"build B of the UI");
        assert_ne!(first, second);

        let params = key.verifying_key().to_bytes();
        let a = PackedWebApp::parse(&first).unwrap();
        let b = PackedWebApp::parse(&second).unwrap();
        a.verify(&params).unwrap();
        b.verify(&params).unwrap();
        assert_eq!(a.version, b.version);
        assert_ne!(
            a.archive, b.archive,
            "signature validity cannot separate these; only the byte comparison can"
        );
    }
}
