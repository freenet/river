/// Shared integration testing framework for Freenet contracts
use anyhow::Result;
use freenet::{
    config::{ConfigArgs, InlineGwConfig, NetworkArgs, SecretArgs, WebsocketApiArgs},
    dev_tool::TransportKeypair,
};
use freenet_stdlib::{
    client_api::WebApi,
    prelude::*,
};
use rand::{Rng, SeedableRng};
use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::Path,
    sync::Mutex,
};
use tokio_tungstenite::connect_async;

pub static RNG: once_cell::sync::Lazy<Mutex<rand::rngs::StdRng>> =
    once_cell::sync::Lazy::new(|| {
        Mutex::new(rand::rngs::StdRng::from_seed(
            *b"0102030405060708090a0b0c0d0e0f10",
        ))
    });

#[derive(Debug)]
pub struct PresetConfig {
    pub temp_dir: tempfile::TempDir,
}

/// Contract deployment helper trait for different contract types
pub trait ContractTestHelper<StateType, ParamsType, DeltaType> {
    async fn deploy_contract(
        client: &mut WebApi,
        initial_state: StateType,
        parameters: &ParamsType,
        subscribe: bool,
    ) -> Result<ContractKey>;

    async fn update_state(
        client: &mut WebApi,
        key: ContractKey,
        delta: DeltaType,
    ) -> Result<()>;

    async fn get_state(
        client: &mut WebApi,
        key: ContractKey,
        fetch_contract: bool,
    ) -> Result<StateType>;

    fn states_equal(a: &StateType, b: &StateType) -> bool;

    fn create_test_state() -> (StateType, ParamsType);
}

pub fn get_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

pub fn get_free_socket_addr() -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

#[allow(clippy::too_many_arguments)]
pub async fn base_node_test_config(
    is_gateway: bool,
    gateways: Vec<String>,
    public_port: Option<u16>,
    ws_api_port: u16,
    data_dir_suffix: &str,
    base_tmp_dir: Option<&Path>,
    blocked_addresses: Option<Vec<SocketAddr>>,
) -> Result<(ConfigArgs, PresetConfig)> {
    let mut rng = RNG.lock().unwrap();
    base_node_test_config_with_rng(
        is_gateway,
        gateways,
        public_port,
        ws_api_port,
        data_dir_suffix,
        base_tmp_dir,
        blocked_addresses,
        &mut rng,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn base_node_test_config_with_rng(
    is_gateway: bool,
    gateways: Vec<String>,
    public_port: Option<u16>,
    ws_api_port: u16,
    data_dir_suffix: &str,
    base_tmp_dir: Option<&Path>,
    blocked_addresses: Option<Vec<SocketAddr>>,
    rng: &mut rand::rngs::StdRng,
) -> Result<(ConfigArgs, PresetConfig)> {
    if is_gateway {
        assert!(public_port.is_some());
    }

    let temp_dir = if let Some(base) = base_tmp_dir {
        tempfile::tempdir_in(base)?
    } else {
        tempfile::Builder::new().prefix(data_dir_suffix).tempdir()?
    };

    let key = TransportKeypair::new_with_rng(rng);
    let transport_keypair = temp_dir.path().join("private.pem");
    key.save(&transport_keypair)?;
    key.public().save(temp_dir.path().join("public.pem"))?;

    let config = ConfigArgs {
        ws_api: WebsocketApiArgs {
            address: Some(Ipv4Addr::LOCALHOST.into()),
            ws_api_port: Some(ws_api_port),
        },
        network_api: NetworkArgs {
            public_address: Some(Ipv4Addr::LOCALHOST.into()),
            public_port,
            is_gateway,
            skip_load_from_network: true,
            gateways: Some(gateways),
            location: Some(rng.gen()),
            ignore_protocol_checking: true,
            address: Some(Ipv4Addr::LOCALHOST.into()),
            network_port: public_port,
            bandwidth_limit: None,
            blocked_addresses,
        },
        config_paths: freenet::config::ConfigPathsArgs {
            config_dir: Some(temp_dir.path().to_path_buf()),
            data_dir: Some(temp_dir.path().to_path_buf()),
        },
        secrets: SecretArgs {
            transport_keypair: Some(transport_keypair),
            ..Default::default()
        },
        ..Default::default()
    };
    Ok((config, PresetConfig { temp_dir }))
}

pub async fn connect_ws_client(ws_port: u16) -> Result<WebApi> {
    let uri = format!("ws://127.0.0.1:{ws_port}/v1/contract/command?encodingProtocol=native");
    let (stream, _) = connect_async(&uri).await?;
    Ok(WebApi::start(stream))
}

pub async fn wait_for_put_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<ContractKey> {
    loop {
        let response = client.recv().await?;
        match response {
            freenet_stdlib::client_api::HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    freenet_stdlib::client_api::ContractResponse::PutResponse { key } => {
                        if &key == contract_key {
                            return Ok(key);
                        }
                    }
                    _ => continue,
                }
            }
            _ => continue,
        }
    }
}

pub async fn wait_for_subscribe_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<()> {
    loop {
        let response = client.recv().await?;
        match response {
            freenet_stdlib::client_api::HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    freenet_stdlib::client_api::ContractResponse::SubscribeResponse { key, .. } => {
                        if &key == contract_key {
                            return Ok(());
                        }
                    }
                    _ => continue,
                }
            }
            _ => continue,
        }
    }
}

pub fn gw_config_from_path(port: u16, path: &Path) -> Result<InlineGwConfig> {
    gw_config_from_path_with_rng(port, path, &mut RNG.lock().unwrap())
}

pub fn gw_config_from_path_with_rng(
    port: u16,
    path: &Path,
    rng: &mut rand::rngs::StdRng,
) -> Result<InlineGwConfig> {
    Ok(InlineGwConfig {
        address: (std::net::Ipv4Addr::LOCALHOST, port).into(),
        location: Some(rng.gen()),
        public_key_path: path.join("public.pem"),
    })
}