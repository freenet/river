use anyhow::Result;
use freenet::{
    config::{ConfigArgs, InlineGwConfig, NetworkArgs, SecretArgs, WebsocketApiArgs},
    dev_tool::TransportKeypair,
};
use freenet_stdlib::client_api::{
    ClientRequest, HostResponse, NodeDiagnosticsConfig, NodeQuery, QueryResponse,
};
use freenet_stdlib::{client_api::WebApi, prelude::*};
use rand::Rng;
use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::Path,
};
use tokio_tungstenite::connect_async;

// Timeout constants
const DIAGNOSTICS_TIMEOUT_SECS: u64 = 10;
const CONTRACT_STATE_TIMEOUT_SECS: u64 = 30;

#[derive(Debug)]
pub struct PresetConfig {
    pub temp_dir: tempfile::TempDir,
}

pub fn get_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
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
    location: Option<f64>,
    rng: &mut rand::rngs::StdRng,
) -> Result<(ConfigArgs, PresetConfig)> {
    let location = location.unwrap_or_else(|| rng.gen());

    println!(
        "Node {} assigned location: {:.6}",
        data_dir_suffix, location
    );
    if is_gateway {
        assert!(public_port.is_some());
    }

    let temp_dir = if let Some(base) = base_tmp_dir {
        tempfile::tempdir_in(base)?
    } else {
        tempfile::Builder::new().prefix(data_dir_suffix).tempdir()?
    };

    let key = TransportKeypair::new();
    let transport_keypair = temp_dir.path().join("private.pem");
    key.save(&transport_keypair)?;
    key.public().save(temp_dir.path().join("public.pem"))?;

    let config = ConfigArgs {
        ws_api: WebsocketApiArgs {
            address: Some(Ipv4Addr::LOCALHOST.into()),
            ws_api_port: Some(ws_api_port),
            token_ttl_seconds: None,
            token_cleanup_interval_seconds: None,
        },
        network_api: NetworkArgs {
            public_address: Some(Ipv4Addr::LOCALHOST.into()),
            public_port,
            is_gateway,
            skip_load_from_network: true,
            gateways: Some(gateways),
            location: Some(location),
            ignore_protocol_checking: true,
            address: Some(Ipv4Addr::LOCALHOST.into()),
            network_port: public_port,
            bandwidth_limit: None,
            blocked_addresses,
            transient_budget: None,
            transient_ttl_secs: None,
            min_connections: None,
            max_connections: None,
            total_bandwidth_limit: None,
            min_bandwidth_per_connection: None,
            streaming_enabled: None,
            streaming_threshold: None,
            ledbat_min_ssthresh: None,
            bbr_startup_rate: None,
            congestion_control: None,
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

pub async fn connect_ws_with_retries(
    port: u16,
    node_name: &str,
    max_attempts: u32,
) -> Result<WebApi> {
    const RETRY_DELAY_SECS: u64 = 3;

    for attempt in 1..=max_attempts {
        match connect_ws_client(port).await {
            Ok(client) => {
                println!("{} WebSocket connection successful", node_name);
                return Ok(client);
            }
            Err(e) if attempt < max_attempts => {
                println!(
                    "{} connection attempt {} failed: {}. Retrying in {} seconds...",
                    node_name, attempt, e, RETRY_DELAY_SECS
                );
                tokio::time::sleep(std::time::Duration::from_secs(RETRY_DELAY_SECS)).await;
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to connect to {} WebSocket after {} attempts: {}",
                    node_name,
                    max_attempts,
                    e
                ));
            }
        }
    }

    unreachable!("Loop should always return via Ok or Err")
}

pub fn gw_config_from_path_with_rng(
    port: u16,
    path: &Path,
    _rng: &mut rand::rngs::StdRng,
) -> Result<InlineGwConfig> {
    Ok(InlineGwConfig {
        address: (std::net::Ipv4Addr::LOCALHOST, port).into(),
        location: Some(0.1),
        public_key_path: path.join("public.pem"),
    })
}

pub async fn collect_river_node_diagnostics(
    clients: &mut [&mut WebApi],
    node_names: &[&str],
    contract_keys: Vec<ContractKey>,
    phase: &str,
) -> Result<()> {
    println!(
        "\n[DIAGNOSTICS] Collecting node diagnostics for phase: {}",
        phase
    );

    let config = NodeDiagnosticsConfig {
        include_node_info: true,
        include_network_info: true,
        include_subscriptions: true,
        contract_keys,
        include_system_metrics: true,
        include_detailed_peer_info: true,
        include_subscriber_peer_ids: true,
    };

    for (client, node_name) in clients.iter_mut().zip(node_names.iter()) {
        println!("\n[DIAGNOSTICS] Querying {} node status...", node_name);

        let diag_result = async {
            client
                .send(ClientRequest::NodeQueries(NodeQuery::NodeDiagnostics {
                    config: config.clone(),
                }))
                .await?;

            let response = tokio::time::timeout(
                std::time::Duration::from_secs(DIAGNOSTICS_TIMEOUT_SECS),
                client.recv(),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Diagnostics timeout after {}s for {}",
                    DIAGNOSTICS_TIMEOUT_SECS,
                    node_name
                )
            })??;

            let HostResponse::QueryResponse(QueryResponse::NodeDiagnostics(diag)) = response else {
                anyhow::bail!("Unexpected response from {}", node_name);
            };

            Ok::<_, anyhow::Error>(diag)
        }
        .await;

        let diag = match diag_result {
            Ok(diag) => diag,
            Err(e) => {
                println!("  Failed to get diagnostics for {}: {}", node_name, e);
                continue;
            }
        };

        if let Some(node_info) = &diag.node_info {
            println!(
                "  Node Type: {} | Peer ID: {}",
                if node_info.is_gateway {
                    "Gateway"
                } else {
                    "Regular"
                },
                node_info.peer_id
            );
            if let Some(addr) = &node_info.listening_address {
                println!("  Listening Address: {}", addr);
            }
            if let Some(loc) = &node_info.location {
                println!("  Network Location: {:.6}", loc);
            }
        }

        if let Some(network) = &diag.network_info {
            println!("  Active Connections: {}", network.active_connections);
            if !network.connected_peers.is_empty() {
                println!(
                    "  Connected Peers: {}",
                    network
                        .connected_peers
                        .iter()
                        .map(|(peer_id, _)| format!("{:.8}", peer_id))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            } else {
                println!("  Connected Peers: None");
            }
        }

        if !diag.subscriptions.is_empty() {
            println!("  Contract Subscriptions:");
            for sub in &diag.subscriptions {
                println!(
                    "    Contract: {:.8} | Client ID: {}",
                    sub.contract_key.to_string(),
                    sub.client_id
                );
            }
        } else {
            println!("  Contract Subscriptions: None");
        }

        if !diag.contract_states.is_empty() {
            println!("  Contract States:");
            for (key, state) in &diag.contract_states {
                println!(
                    "    Contract: {:.8} | Subscribers: {}",
                    key.to_string(),
                    state.subscribers
                );
                if !state.subscriber_peer_ids.is_empty() {
                    println!(
                        "      Subscriber Peer IDs: {}",
                        state
                            .subscriber_peer_ids
                            .iter()
                            .map(|p| format!("{:.16}", p))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }

        if let Some(metrics) = &diag.system_metrics {
            println!(
                "  System Metrics: {} active connections, {} seeding contracts",
                metrics.active_connections, metrics.seeding_contracts
            );
        }
    }

    println!(
        "[DIAGNOSTICS] Completed diagnostics collection for phase: {}\n",
        phase
    );
    Ok(())
}
