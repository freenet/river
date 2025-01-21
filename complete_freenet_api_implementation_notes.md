# Task overview

Complete tasks necessary to publish River on Freenet, including:

* Publish the River user interface on Freenet using the fdev utility, using a cargo make task, which should output the address of the published app

# Prerequisites

 * This is a tutorial for how to create an app using "fdev", a Freenet cli tool, it discusses using Node.JS/Typescript, however in this project
   we're using Rust and the Dioxus framework (version 0.6) - please review: https://docs.freenet.org/tutorial.html

## Relevant files in the codebase (ensure these are added to context)

* Makefile.toml - the cargo make file used to build and test various aspects of the  project
* contracts/web-container-contract/src/lib.rs (source code for the contract that contains the user interface, compiled from ui/ directory)



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
