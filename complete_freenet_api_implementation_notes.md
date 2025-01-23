********# River Freenet Integration Plan

## Phase 1: Web Container Contract Preparation

1. **Key Management**  
   - Generate dedicated Ed25519 keypair for UI publishing
   - Store private key at `~/.config/freenet/river/ui_signing.key`
   - Add public key to contract parameters (web-container-contract/src/lib.rs):
     ```rust
     let params_bytes = verifying_key.to_bytes().to_vec();
     Parameters::from(params_bytes)
     ```

2. **Asset Packaging** (confirmed via app_packaging.rs):
   - UI assets compressed as tar.xz
   - Structure: metadata header + compressed web dir
   - Built assets path: `ui/dist/*` (set in freenet.toml)

3. **Signature Verification** (web-container-contract/src/lib.rs):
   - Maintain existing validate_state() that checks:
     - Web content hash matches signed version
     - Publisher signature validation
     - Version monotonic increase

## Phase 2: Deployment Automation

1. **Makefile.toml Additions**:
   ```toml
   [tasks.publish-ui]
   description = "Publish UI to local Freenet node"
   dependencies = ["build-web-container", "build-ui"]
   command = "fdev"
   args = [
       "publish",
       "--code", "./target/wasm32-unknown-unknown/release/web_container_contract.wasm",
       "--state", "./ui/dist",
       "--parameters", "${UI_PUBKEY_FILE}"
   ]
   ```

2. **Address Capture**:
   - Parse output of `fdev publish` for contract address
   - Store in `deployed_contracts.txt`
   - Add post-publish verification curl:
     ```bash
     curl http://localhost:50509/contract/web/${CONTRACT_ADDRESS}
     ```

## Phase 3: Authentication Flow

1. **Signing Process**:
   - Before publishing:
     ```rust
     let signature = sign_struct(&web_content, &ui_signing_key);
     ```
   - Inject signature into contract state metadata

2. **Parameter Encoding**:
   - Serialize public key using CBOR
   - Verify in contract:
     ```rust
     let verifying_key = VerifyingKey::from_bytes(&params_bytes)
         .map_err(|e| ContractError::Other(format!("Invalid public key: {}", e)))?;
     ```

## Open Questions

1. **Key Management**:
   - Where should the UI signing key be stored during CI/CD?
   - How to handle key rotation for published contracts?

2. **Parameter Handling**:
   - Should parameters be hardcoded or dynamic per environment?
   - How to handle parameter changes after initial deployment?

3. **State Management**:
   - How to verify the built UI assets match the packaged state?
   - What retention policy for previous UI versions?

4. **fdev Integration**:
   - Exact output format of `fdev publish` for address extraction
   - Error handling for partial publish failures

5. **Dependency Management**:
   - Should related contracts be specified in freenet.toml?
   - How to handle version mismatches between contracts?

## Existing Code References

1. **Web Container Contract** (web-container-contract/src/lib.rs):
   - validate_state() already handles version/signature checks
   - update_state() enforces version increments

2. **Asset Packaging** (app_packaging.rs):
   - WebApp struct handles tar.xz compression
   - Metadata header format: [u64 len][metadata][u64 len][web_content]

3. **Makefile.toml**:
   - Existing build-web-container task
   - Need to add publish-ui task with parameter injection

## Verification Steps

1. Contract address validation via HTTP gateway
2. Signature verification test suite
3. Version conflict simulation tests
4. End-to-edge publish/update workflow test



# Relevant code

## fdev code

Relevant code from the fdev command line tool, to resolve any ambiguity over how to use it

### fdev entry point

```rust
use std::borrow::Cow;

use clap::Parser;
use freenet_stdlib::client_api::ClientRequest;

mod build;
mod commands;
mod config;
mod inspect;
pub(crate) mod network_metrics_server;
mod new_package;
mod query;
mod testing;
mod util;
mod wasm_runtime;

use crate::{
    build::build_package,
    commands::{put, update},
    config::{Config, SubCommand},
    inspect::inspect,
    new_package::create_new_package,
    wasm_runtime::run_local_executor,
};

type CommandReceiver = tokio::sync::mpsc::Receiver<ClientRequest<'static>>;
type CommandSender = tokio::sync::mpsc::Sender<ClientRequest<'static>>;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("Configuration error: {0}")]
    MissConfiguration(Cow<'static, str>),
    #[error("Command failed: {0}")]
    CommandFailed(&'static str),
}

fn main() -> anyhow::Result<()> {
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let config = Config::parse();
    if !config.sub_command.is_child() {
        freenet::config::set_logger(None, None);
    }
    tokio_rt.block_on(async move {
        let cwd = std::env::current_dir()?;
        let r = match config.sub_command {
            SubCommand::WasmRuntime(local_node_config) => {
                run_local_executor(local_node_config).await
            }
            SubCommand::Build(build_tool_config) => build_package(build_tool_config, &cwd),
            SubCommand::Inspect(inspect_config) => inspect(inspect_config),
            SubCommand::New(new_pckg_config) => create_new_package(new_pckg_config),
            SubCommand::Publish(publish_config) => put(publish_config, config.additional).await,
            SubCommand::Execute(cmd_config) => match cmd_config.command {
                config::NodeCommand::Put(put_config) => put(put_config, config.additional).await,
                config::NodeCommand::Update(update_config) => {
                    update(update_config, config.additional).await
                }
            },
            SubCommand::Test(test_config) => testing::test_framework(test_config).await,
            SubCommand::NetworkMetricsServer(server_config) => {
                let (server, _) = crate::network_metrics_server::start_server(&server_config).await;
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = server => {}
                }
                Ok(())
            }
            SubCommand::Query {} => {
                query::query(config.additional).await?;
                Ok(())
            }
        };
        // todo: make all commands return concrete `thiserror` compatible errors so we can use anyhow
        r.map_err(|e| anyhow::format_err!(e))
    })
}
```

## fdev commands.rs

```rust
use std::{fs::File, io::Read, net::SocketAddr, path::PathBuf};

use freenet::dev_tool::OperationMode;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, DelegateRequest, WebApi},
    prelude::*,
};

use crate::config::{BaseConfig, PutConfig, UpdateConfig};

mod v1;

#[derive(Debug, Clone, clap::Subcommand)]
pub(crate) enum PutType {
    /// Puts a new contract
    Contract(PutContract),
    /// Puts a new delegate
    Delegate(PutDelegate),
}

#[derive(clap::Parser, Clone, Debug)]
pub(crate) struct PutContract {
    /// A path to a JSON file listing the related contracts.
    #[arg(long)]
    pub(crate) related_contracts: Option<PathBuf>,
    /// A path to the initial state for the contract being published.
    #[arg(long)]
    pub(crate) state: Option<PathBuf>,
}

#[derive(clap::Parser, Clone, Debug)]
pub(crate) struct PutDelegate {
    /// Base58 encoded nonce. If empty the default value will be used, this is only allowed in local mode.
    #[arg(long, env = "DELEGATE_NONCE", default_value_t = String::new())]
    pub(crate) nonce: String,
    /// Base58 encoded cipher. If empty the default value will be used, this is only allowed in local mode.
    #[arg(long, env = "DELEGATE_CIPHER", default_value_t = String::new())]
    pub(crate) cipher: String,
}

pub async fn put(config: PutConfig, other: BaseConfig) -> anyhow::Result<()> {
    if config.release {
        anyhow::bail!("Cannot publish contracts in the network yet");
    }
    let params = if let Some(params) = &config.parameters {
        let mut buf = vec![];
        File::open(params)?.read_to_end(&mut buf)?;
        Parameters::from(buf)
    } else {
        Parameters::from(&[] as &[u8])
    };
    match &config.package_type {
        PutType::Contract(contract) => put_contract(&config, contract, other, params).await,
        PutType::Delegate(delegate) => put_delegate(&config, delegate, other, params).await,
    }
}

async fn put_contract(
    config: &PutConfig,
    contract_config: &PutContract,
    other: BaseConfig,
    params: Parameters<'static>,
) -> anyhow::Result<()> {
    let contract = ContractContainer::try_from((config.code.as_path(), params))?;
    let state = if let Some(ref state_path) = contract_config.state {
        let mut buf = vec![];
        File::open(state_path)?.read_to_end(&mut buf)?;
        buf.into()
    } else {
        tracing::warn!("no state provided for contract, if your contract cannot handle empty state correctly, this will always cause an error.");
        vec![].into()
    };
    let related_contracts = if let Some(_related) = &contract_config.related_contracts {
        todo!("use `related` contracts")
    } else {
        Default::default()
    };

    println!("Putting contract {}", contract.key());
    let request = ContractRequest::Put {
        contract,
        state,
        related_contracts,
    }
    .into();
    let mut client = start_api_client(other).await?;
    execute_command(request, &mut client).await
}

async fn put_delegate(
    config: &PutConfig,
    delegate_config: &PutDelegate,
    other: BaseConfig,
    params: Parameters<'static>,
) -> anyhow::Result<()> {
    let delegate = DelegateContainer::try_from((config.code.as_path(), params))?;

    let (cipher, nonce) = if delegate_config.cipher.is_empty() && delegate_config.nonce.is_empty() {
        println!(
"Using default cipher and nonce.
For additional hardening is recommended to use a different cipher and nonce to encrypt secrets in storage.");
        (
            ::freenet_stdlib::client_api::DelegateRequest::DEFAULT_CIPHER,
            ::freenet_stdlib::client_api::DelegateRequest::DEFAULT_NONCE,
        )
    } else {
        let mut cipher = [0; 32];
        bs58::decode(delegate_config.cipher.as_bytes())
            .with_alphabet(bs58::Alphabet::BITCOIN)
            .onto(&mut cipher)?;

        let mut nonce = [0; 24];
        bs58::decode(delegate_config.nonce.as_bytes())
            .with_alphabet(bs58::Alphabet::BITCOIN)
            .onto(&mut nonce)?;
        (cipher, nonce)
    };

    println!("Putting delegate {} ", delegate.key().encode());
    let request = DelegateRequest::RegisterDelegate {
        delegate,
        cipher,
        nonce,
    }
    .into();
    let mut client = start_api_client(other).await?;
    execute_command(request, &mut client).await
}

pub async fn update(config: UpdateConfig, other: BaseConfig) -> anyhow::Result<()> {
    if config.release {
        anyhow::bail!("Cannot publish contracts in the network yet");
    }
    let key = ContractInstanceId::try_from(config.key)?.into();
    println!("Updating contract {key}");
    let data = {
        let mut buf = vec![];
        File::open(&config.delta)?.read_to_end(&mut buf)?;
        StateDelta::from(buf).into()
    };
    let request = ContractRequest::Update { key, data }.into();
    let mut client = start_api_client(other).await?;
    execute_command(request, &mut client).await
}

pub(crate) async fn start_api_client(cfg: BaseConfig) -> anyhow::Result<WebApi> {
    v1::start_api_client(cfg).await
}

pub(crate) async fn execute_command(
    request: ClientRequest<'static>,
    api_client: &mut WebApi,
) -> anyhow::Result<()> {
    v1::execute_command(request, api_client).await
}
```

## app_packaging.rs, relates to how the UI is packaged for deployment in the container contract

```rust
//! Helper functions and types for dealing with HTTP gateway compatible contracts.
use std::{
    io::{Cursor, Read},
    path::Path,
};

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use tar::{Archive, Builder};
use xz2::read::{XzDecoder, XzEncoder};

#[derive(Debug, thiserror::Error)]
pub enum WebContractError {
    #[error("unpacking error: {0}")]
    UnpackingError(anyhow::Error),
    #[error("{0}")]
    StoringError(std::io::Error),
    #[error("file not found: {0}")]
    FileNotFound(String),
}

#[non_exhaustive]
pub struct WebApp {
    pub metadata: Vec<u8>,
    pub web: Vec<u8>,
}

impl WebApp {
    pub fn from_data(
        metadata: Vec<u8>,
        web: Builder<Cursor<Vec<u8>>>,
    ) -> Result<Self, WebContractError> {
        let buf = web.into_inner().unwrap().into_inner();
        let mut encoder = XzEncoder::new(Cursor::new(buf), 6);
        let mut compressed = vec![];
        encoder.read_to_end(&mut compressed).unwrap();
        Ok(Self {
            metadata,
            web: compressed,
        })
    }

    pub fn pack(mut self) -> std::io::Result<Vec<u8>> {
        let mut output = Vec::with_capacity(
            self.metadata.len() + self.web.len() + (std::mem::size_of::<u64>() * 2),
        );
        output.write_u64::<BigEndian>(self.metadata.len() as u64)?;
        output.append(&mut self.metadata);
        output.write_u64::<BigEndian>(self.web.len() as u64)?;
        output.append(&mut self.web);
        Ok(output)
    }

    pub fn unpack(&mut self, dst: impl AsRef<Path>) -> Result<(), WebContractError> {
        let mut decoded_web = self.decode_web();
        decoded_web
            .unpack(dst)
            .map_err(WebContractError::StoringError)?;
        Ok(())
    }

    pub fn get_file(&mut self, path: &str) -> Result<Vec<u8>, WebContractError> {
        let mut decoded_web = self.decode_web();
        for e in decoded_web
            .entries()
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?
        {
            let mut e = e.map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;
            if e.path()
                .ok()
                .filter(|p| p.to_string_lossy() == path)
                .is_some()
            {
                let mut bytes = vec![];
                e.read_to_end(&mut bytes)
                    .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;
                return Ok(bytes);
            }
        }
        Err(WebContractError::FileNotFound(path.to_owned()))
    }

    fn decode_web(&self) -> Archive<XzDecoder<&[u8]>> {
        let decoder = XzDecoder::new(self.web.as_slice());
        Archive::new(decoder)
    }
}

impl<'a> TryFrom<&'a [u8]> for WebApp {
    type Error = WebContractError;

    fn try_from(state: &'a [u8]) -> Result<Self, Self::Error> {
        const MAX_METADATA_SIZE: u64 = 1024;
        const MAX_WEB_SIZE: u64 = 1024 * 1024 * 100;
        // Decompose the state and extract the compressed web interface
        let mut state = Cursor::new(state);

        let metadata_size = state
            .read_u64::<BigEndian>()
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;
        if metadata_size > MAX_METADATA_SIZE {
            return Err(WebContractError::UnpackingError(anyhow::anyhow!(
                "Exceeded metadata size of 1kB: {} bytes",
                metadata_size
            )));
        }
        let mut metadata = vec![0; metadata_size as usize];
        state
            .read_exact(&mut metadata)
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;

        let web_size = state
            .read_u64::<BigEndian>()
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;
        if web_size > MAX_WEB_SIZE {
            return Err(WebContractError::UnpackingError(anyhow::anyhow!(
                "Exceeded packed web size of 100MB: {} bytes",
                web_size
            )));
        }
        let mut web = vec![0; web_size as usize];
        state
            .read_exact(&mut web)
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;

        Ok(Self { metadata, web })
    }
}

```********
