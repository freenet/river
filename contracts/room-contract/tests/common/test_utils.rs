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
use freenet_stdlib::client_api::{ClientRequest, HostResponse, NodeDiagnosticsConfig, NodeQuery, QueryResponse};

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

pub async fn collect_river_node_diagnostics(
    clients: &mut [&mut WebApi],
    node_names: &[&str],
    contract_keys: Vec<ContractKey>,
    phase: &str,
) -> Result<()> {
    println!("\n[DIAGNOSTICS] Collecting node diagnostics for phase: {}", phase);
    
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
        
        client.send(ClientRequest::NodeQueries(NodeQuery::NodeDiagnostics {
            config: config.clone(),
        })).await?;

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.recv()
        ).await.map_err(|_| anyhow::anyhow!("Diagnostics timeout after 30s for {}", node_name))??;

        let HostResponse::QueryResponse(QueryResponse::NodeDiagnostics(diag)) = response else {
            anyhow::bail!("Unexpected response from {}", node_name);
        };

        if let Some(node_info) = &diag.node_info {
            println!("  Node Type: {} | Peer ID: {}", 
                if node_info.is_gateway { "Gateway" } else { "Regular" },
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
                println!("  Connected Peers: {}", 
                    network.connected_peers.iter()
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
                println!("    Contract: {} | Client ID: {}", 
                    format!("{:.8}", sub.contract_key.to_string()),
                    sub.client_id
                );
            }
        } else {
            println!("  Contract Subscriptions: None");
        }

        if !diag.contract_states.is_empty() {
            println!("  Contract States:");
            for (key, state) in &diag.contract_states {
                println!("    Contract: {} | Subscribers: {}", 
                    format!("{:.8}", key.to_string()),
                    state.subscribers
                );
                if !state.subscriber_peer_ids.is_empty() {
                    println!("      Subscriber Peer IDs: {}", 
                        state.subscriber_peer_ids.iter()
                            .map(|p| format!("{:.16}", p))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }

        if let Some(metrics) = &diag.system_metrics {
            println!("  System Metrics: {} active connections, {} seeding contracts", 
                metrics.active_connections,
                metrics.seeding_contracts
            );
        }
    }
    
    println!("[DIAGNOSTICS] Completed diagnostics collection for phase: {}\n", phase);
    Ok(())
}

pub async fn analyze_river_state_consistency(
    clients: &mut [&mut WebApi],
    node_names: &[&str], 
    contract_key: ContractKey,
) -> Result<()> {
    println!("\n[STATE ANALYSIS] Analyzing River state consistency across nodes...");
    
    let mut states = Vec::new();
    for (client, node_name) in clients.iter_mut().zip(node_names.iter()) {
        println!("[STATE ANALYSIS] Requesting state from {} node...", node_name);
        match get_contract_state_from_client(client, contract_key).await {
            Ok(state) => {
                println!("[STATE ANALYSIS] {} state retrieved successfully", node_name);
                states.push((node_name, Some(state)));
            }
            Err(e) => {
                println!("[STATE ANALYSIS] {} state retrieval failed: {}", node_name, e);
                if node_name == &"Node3" {
                    println!("[STATE ANALYSIS] Node3 SPECIFIC ERROR: This may indicate:");
                    println!("  - WebSocket connection lost");
                    println!("  - Node3 not properly subscribed to contract");
                    println!("  - P2P network connectivity issues with Node3");
                    println!("  - Contract state not replicated to Node3");
                }
                states.push((node_name, None));
            }
        }
    }
    
    println!("\n[STATE DETAILS] Node state information:");
    for (node_name, state_opt) in &states {
        match state_opt {
            Some(state) => {
                println!("  {}: AVAILABLE", node_name);
                println!("    Configuration Version: {}", state.configuration.configuration.configuration_version);
                println!("    Room Name: {}", state.configuration.configuration.name);
                println!("    Members Count: {}", state.members.members.len());
                println!("    Messages Count: {}", state.recent_messages.messages.len());
                println!("    Bans Count: {}", state.bans.0.len());
                println!("    Max Members: {}", state.configuration.configuration.max_members);
                println!("    Max Messages: {}", state.configuration.configuration.max_recent_messages);
                
                if !state.members.members.is_empty() {
                    println!("    Member IDs: {}", 
                        state.members.members.iter()
                            .map(|member| format!("{:.8}", member.member.id().0 .0))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                
                if !state.recent_messages.messages.is_empty() {
                    println!("    Recent Messages: {} total", state.recent_messages.messages.len());
                    for (i, msg) in state.recent_messages.messages.iter().take(3).enumerate() {
                        println!("      Message {}: '{}' by {:.8}", 
                            i + 1, 
                            msg.message.content,
                            msg.message.author.0 .0
                        );
                    }
                    if state.recent_messages.messages.len() > 3 {
                        println!("      ... and {} more messages", state.recent_messages.messages.len() - 3);
                    }
                } else {
                    println!("    Recent Messages: EMPTY");
                }
            }
            None => {
                println!("  {}: NOT AVAILABLE", node_name);
            }
        }
    }
    
    println!("\n[STATE COMPARISON] Comparing states between nodes:");
    let mut all_consistent = true;
    for i in 0..states.len() {
        for j in (i+1)..states.len() {
            let (name_a, state_a) = &states[i];
            let (name_b, state_b) = &states[j];
            
            match (state_a, state_b) {
                (Some(a), Some(b)) => {
                    if crate::river_states_equal(a, b) {
                        println!("  {} <-> {}: CONSISTENT", name_a, name_b);
                    } else {
                        println!("  {} <-> {}: MISMATCH DETECTED", name_a, name_b);
                        
                        if a.configuration != b.configuration {
                            println!("    Configuration differs");
                        }
                        if a.members != b.members {
                            println!("    Members differ (A: {}, B: {})", 
                                a.members.members.len(), b.members.members.len());
                        }
                        if a.recent_messages != b.recent_messages {
                            println!("    Messages differ (A: {}, B: {})", 
                                a.recent_messages.messages.len(), b.recent_messages.messages.len());
                        }
                        if a.bans != b.bans {
                            println!("    Bans differ (A: {}, B: {})", 
                                a.bans.0.len(), b.bans.0.len());
                        }
                        
                        all_consistent = false;
                    }
                }
                (None, Some(_)) => {
                    println!("  {} <-> {}: AVAILABILITY MISMATCH (A: missing, B: available)", name_a, name_b);
                    all_consistent = false;
                }
                (Some(_), None) => {
                    println!("  {} <-> {}: AVAILABILITY MISMATCH (A: available, B: missing)", name_a, name_b);
                    all_consistent = false;
                }
                (None, None) => {
                    println!("  {} <-> {}: BOTH UNAVAILABLE", name_a, name_b);
                    all_consistent = false;
                }
            }
        }
    }
    
    if all_consistent {
        println!("\n[STATE ANALYSIS] RESULT: All available states are consistent");
    } else {
        println!("\n[STATE ANALYSIS] RESULT: State inconsistencies detected");
    }
    
    // Add detailed state dump for debugging
    println!("\n[DETAILED STATE DUMP] Complete state information for debugging:");
    for (node_name, state_opt) in &states {
        match state_opt {
            Some(state) => {
                println!("\n  === {} STATE DUMP ===", node_name.to_uppercase());
                println!("  Configuration:");
                println!("    Version: {}", state.configuration.configuration.configuration_version);
                println!("    Room Name: '{}'", state.configuration.configuration.name);
                println!("    Owner Member ID: {} (Room owner)", state.configuration.configuration.owner_member_id.0 .0);
                println!("    Max Members: {}", state.configuration.configuration.max_members);
                println!("    Max Messages: {}", state.configuration.configuration.max_recent_messages);
                
                println!("  Members ({}):", state.members.members.len());
                if state.members.members.is_empty() {
                    println!("    (No members)");
                } else {
                    for (i, member) in state.members.members.iter().enumerate() {
                        println!("    {}: ID={} InvitedBy={}", 
                            i + 1, 
                            member.member.id().0 .0,
                            member.member.invited_by.0 .0
                        );
                    }
                }
                
                println!("  Recent Messages ({}):", state.recent_messages.messages.len());
                if state.recent_messages.messages.is_empty() {
                    println!("    (No messages)");
                } else {
                    for (i, msg) in state.recent_messages.messages.iter().enumerate() {
                        let is_owner = msg.message.author.0 .0 == state.configuration.configuration.owner_member_id.0 .0;
                        let sender_type = if is_owner { "(Room Owner)" } else { "(Member)" };
                        println!("    Message {}: Author={} {} Content='{}' Time={:?}", 
                            i + 1,
                            msg.message.author.0 .0,
                            sender_type,
                            msg.message.content,
                            msg.message.time
                        );
                    }
                }
                
                println!("  Bans ({}):", state.bans.0.len());
                if state.bans.0.is_empty() {
                    println!("    (No bans)");
                } else {
                    for (i, ban) in state.bans.0.iter().enumerate() {
                        println!("    Ban {}: {:?}", i + 1, ban);
                    }
                }
            }
            None => {
                println!("\n  === {} STATE DUMP ===", node_name.to_uppercase());
                println!("  STATE NOT AVAILABLE");
            }
        }
    }
    
    Ok(())
}

async fn get_contract_state_from_client(
    client: &mut WebApi,
    contract_key: ContractKey,
) -> Result<river_common::ChatRoomStateV1> {
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
    
    println!("[STATE_DEBUG] Sending GET request for contract: {:?}", contract_key);
    client.send(ClientRequest::ContractOp(ContractRequest::Get {
        key: contract_key,
        return_contract_code: false,
        subscribe: false,
    })).await?;

    println!("[STATE_DEBUG] Waiting for response...");
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.recv()
    ).await.map_err(|_| anyhow::anyhow!("Contract state request timeout after 30s"))??;

    println!("[STATE_DEBUG] Received response type: {:?}", std::mem::discriminant(&response));
    match response {
        HostResponse::ContractResponse(
            freenet_stdlib::client_api::ContractResponse::GetResponse { state, .. }
        ) => {
            println!("[STATE_DEBUG] Processing GetResponse with {} bytes", state.as_ref().len());
            let room_state: river_common::ChatRoomStateV1 = 
                ciborium::de::from_reader(state.as_ref())?;
            println!("[STATE_DEBUG] Successfully parsed state: {} messages, {} members", 
                room_state.recent_messages.messages.len(), 
                room_state.members.members.len());
            Ok(room_state)
        }
        HostResponse::ContractResponse(
            freenet_stdlib::client_api::ContractResponse::UpdateResponse { summary, .. }
        ) => {
            println!("[STATE_DEBUG] Processing UpdateResponse with summary {} bytes", summary.as_ref().len());
            // UpdateResponse contains the full state in summary format
            let room_state: river_common::ChatRoomStateV1 = 
                ciborium::de::from_reader(summary.as_ref())?;
            println!("[STATE_DEBUG] Successfully parsed UpdateResponse state: {} messages, {} members", 
                room_state.recent_messages.messages.len(), 
                room_state.members.members.len());
            Ok(room_state)
        }
        HostResponse::ContractResponse(
            freenet_stdlib::client_api::ContractResponse::UpdateNotification { update, .. }
        ) => {
            println!("[STATE_DEBUG] Processing UpdateNotification");
            match update {
                freenet_stdlib::prelude::UpdateData::State(state) => {
                    println!("[STATE_DEBUG] UpdateNotification contains State with {} bytes", state.as_ref().len());
                    let room_state: river_common::ChatRoomStateV1 = 
                        ciborium::de::from_reader(state.as_ref())?;
                    println!("[STATE_DEBUG] Successfully parsed update state: {} messages, {} members", 
                        room_state.recent_messages.messages.len(), 
                        room_state.members.members.len());
                    Ok(room_state)
                }
                other_update => {
                    println!("[STATE_DEBUG] Unexpected update type in UpdateNotification: {:?}", other_update);
                    anyhow::bail!("Unexpected update type: {:?}", other_update)
                }
            }
        }
        other => {
            println!("[STATE_DEBUG] Unexpected response: {:?}", other);
            anyhow::bail!("Unexpected response: {:?}", other)
        }
    }
}