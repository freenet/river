use std::{
    collections::{HashMap, HashSet},
    env,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Output,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use assert_cmd::cargo::cargo_bin_cmd;
use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, HostResponse, NetworkDebugInfo,
    NodeDiagnosticsConfig, NodeDiagnosticsResponse, NodeQuery, QueryResponse, WebApi,
};
use freenet_stdlib::prelude::ContractKey;
use freenet_test_network::{Backend, BuildProfile, DockerNatConfig, FreenetBinary, TestNetwork};
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;

#[derive(Deserialize)]
struct CreateRoomOutput {
    owner_key: String,
    contract_key: String,
}

#[derive(Deserialize)]
struct InviteCreateOutput {
    invitation_code: String,
}

#[derive(Deserialize)]
struct InviteAcceptOutput {
    room_owner_key: String,
    contract_key: String,
}

#[derive(Clone)]
struct ScenarioOptions {
    peer_count: usize,
    room_count: usize,
    users_per_room: usize,
    rounds: usize,
    rng_seed: u64,
}

impl ScenarioOptions {
    fn from_env(default_peers: usize, default_rounds: usize) -> Result<Self> {
        let peer_count = read_env_usize("RIVER_TEST_PEER_COUNT")?.unwrap_or(default_peers);
        let room_count = read_env_usize("RIVER_TEST_ROOM_COUNT")?.unwrap_or(1);
        let users_per_room = read_env_usize("RIVER_TEST_USERS_PER_ROOM")?.unwrap_or(2);
        let rounds = read_env_usize("RIVER_TEST_ROUNDS")?.unwrap_or(default_rounds);
        let rng_seed = read_env_u64("RIVER_TEST_SCENARIO_SEED")?.unwrap_or(42);

        anyhow::ensure!(
            room_count > 0,
            "RIVER_TEST_ROOM_COUNT must be greater than zero"
        );
        anyhow::ensure!(
            users_per_room >= 2,
            "RIVER_TEST_USERS_PER_ROOM must be at least 2"
        );
        anyhow::ensure!(peer_count >= 2, "peer count must be at least 2");

        anyhow::ensure!(rounds >= 1, "RIVER_TEST_ROUNDS must be at least 1");

        Ok(Self {
            peer_count,
            room_count,
            users_per_room,
            rounds,
            rng_seed,
        })
    }
}

fn read_env_usize(var: &str) -> Result<Option<usize>> {
    match env::var(var) {
        Ok(val) => {
            if val.trim().is_empty() {
                Ok(None)
            } else {
                let parsed = val.parse::<usize>().with_context(|| {
                    format!("Environment variable {var} must be an unsigned integer")
                })?;
                Ok(Some(parsed))
            }
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(err).context(format!("Failed to read environment variable {var}")),
    }
}

fn read_env_u64(var: &str) -> Result<Option<u64>> {
    match env::var(var) {
        Ok(val) => {
            if val.trim().is_empty() {
                Ok(None)
            } else {
                let parsed = val.parse::<u64>().with_context(|| {
                    format!("Environment variable {var} must be an unsigned integer")
                })?;
                Ok(Some(parsed))
            }
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(err).context(format!("Failed to read environment variable {var}")),
    }
}

fn node_url(peer: &freenet_test_network::TestPeer) -> String {
    format!("{}?encodingProtocol=native", peer.ws_url())
}

struct UserClient {
    label: String,
    peer_index: usize,
    config_dir: TempDir,
}

struct RoomContext {
    id: usize,
    users: Vec<UserClient>,
    owner_key: Option<String>,
    contract_key: Option<ContractKey>,
    expected_messages: Vec<String>,
}

impl RoomContext {
    fn owner_key(&self) -> &str {
        self.owner_key
            .as_deref()
            .expect("room owner key should be initialized")
    }

    fn contract_key(&self) -> &ContractKey {
        self.contract_key
            .as_ref()
            .expect("room contract key should be initialized")
    }
}

#[derive(Default)]
struct LatencyTracker {
    sent: HashMap<String, Instant>,
    delivered: HashMap<String, Duration>,
}

impl LatencyTracker {
    fn record_send(&mut self, message: &str) {
        self.sent.insert(message.to_string(), Instant::now());
    }

    fn record_delivery(&mut self, message: &str) {
        if self.delivered.contains_key(message) {
            return;
        }
        if let Some(start) = self.sent.get(message) {
            self.delivered
                .insert(message.to_string(), Instant::now() - *start);
        }
    }

    fn summary(&self) -> Option<(Duration, Duration, Duration, usize)> {
        if self.delivered.is_empty() {
            return None;
        }
        let mut total = Duration::ZERO;
        let mut min = Duration::MAX;
        let mut max = Duration::ZERO;
        for latency in self.delivered.values() {
            total += *latency;
            if *latency < min {
                min = *latency;
            }
            if *latency > max {
                max = *latency;
            }
        }
        let count = self.delivered.len();
        Some((total / count as u32, min, max, count))
    }
}

async fn run_riverctl(config_dir: &Path, node_url: &str, args: &[&str]) -> Result<String> {
    fn extract_json_payload(stdout: &str) -> Option<String> {
        fn strip_ansi_sequences(input: &str) -> String {
            let mut result = String::with_capacity(input.len());
            let mut chars = input.chars();
            while let Some(ch) = chars.next() {
                if ch == '\u{1b}' {
                    // Skip ANSI escape sequence (ESC [ ... letter)
                    if let Some('[') = chars.next() {
                        while let Some(next) = chars.next() {
                            if ('@'..='~').contains(&next) {
                                break;
                            }
                        }
                    }
                    continue;
                }
                result.push(ch);
            }
            result
        }

        let cleaned = strip_ansi_sequences(stdout);
        let mut chars = cleaned.char_indices();
        let mut start_idx: Option<usize> = None;
        let mut end_idx: Option<usize> = None;
        let mut stack: Vec<char> = Vec::new();
        let mut in_string = false;
        let mut escape = false;
        let mut line_ws_only = true;

        while let Some((idx, ch)) = chars.next() {
            if in_string {
                if escape {
                    escape = false;
                    continue;
                }
                match ch {
                    '\\' => escape = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }

            if ch == '\n' {
                line_ws_only = true;
                continue;
            }

            match ch {
                '"' => {
                    in_string = true;
                    line_ws_only = false;
                }
                '{' | '[' if line_ws_only => {
                    let rest = cleaned.get(idx + ch.len_utf8()..).unwrap_or("");
                    let next_sig = rest.chars().skip_while(|c| c.is_whitespace()).next();
                    let is_likely_json = match (ch, next_sig) {
                        ('{', Some('"') | Some('}') | Some('[') | Some('{')) => true,
                        ('[', Some('{') | Some('[') | Some('"') | Some(']')) => true,
                        ('[', None) | ('{', None) => true,
                        _ => false,
                    };
                    if !is_likely_json {
                        line_ws_only = false;
                        continue;
                    }
                    if start_idx.is_none() {
                        start_idx = Some(idx);
                    }
                    stack.push(if ch == '{' { '}' } else { ']' });
                    line_ws_only = false;
                }
                '}' | ']' => {
                    if let Some(expected) = stack.pop() {
                        if ch != expected {
                            // malformed JSON, abort
                            return None;
                        }
                        if stack.is_empty() {
                            end_idx = Some(idx);
                            break;
                        }
                    } else {
                        // unmatched closing brace
                        return None;
                    }
                    line_ws_only = false;
                }
                ch if ch.is_whitespace() => {}
                _ => {
                    line_ws_only = false;
                }
            }
        }

        match (start_idx, end_idx) {
            (Some(start), Some(end)) => cleaned.get(start..=end).map(|s| s.to_string()),
            _ => None,
        }
    }

    let mut full_args = vec![
        "--node-url".to_string(),
        node_url.to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    full_args.extend(args.iter().map(|s| s.to_string()));

    let config_dir = config_dir.to_owned();
    let args_clone = full_args.clone();

    let output: Output = tokio::task::spawn_blocking(move || -> Result<Output> {
        let mut cmd = cargo_bin_cmd!("riverctl");
        cmd.env("RIVER_CONFIG_DIR", &config_dir).args(&args_clone);
        cmd.output()
            .context("Failed to execute riverctl command")
            .map_err(Into::into)
    })
    .await
    .context("Failed to join riverctl command task")??;

    if !output.status.success() {
        return Err(anyhow!(
            "riverctl command failed: {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }

    let stdout =
        String::from_utf8(output.stdout).context("riverctl command produced non-UTF8 stdout")?;

    if let Some(payload) = extract_json_payload(&stdout) {
        return Ok(payload);
    }

    let trimmed = stdout.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Ok(trimmed.to_string());
    }

    Err(anyhow!(
        "Failed to find JSON payload in output:\n{}",
        stdout
    ))
}

fn decode_plaintext_messages(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(|entry| {
            entry
                .get("content")
                .and_then(|content| content.get("Public"))
                .and_then(|public| public.get("plaintext"))
                .and_then(|text| text.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

fn dump_network_logs(network: &TestNetwork) {
    if let Ok(logs) = network.read_logs() {
        println!("--- Recent network logs ---");
        for entry in logs.iter().rev().take(5000).rev() {
            let level = entry.level.as_deref().unwrap_or("INFO");
            let ts = entry.timestamp_raw.as_deref().unwrap_or("<no-ts>");
            println!("[{}] [{}] {}: {}", ts, level, entry.peer_id, entry.message);
        }
        println!("--- End network logs ---");
    } else {
        println!("Unable to read network logs");
    }
}

fn dump_initial_log_lines(network: &TestNetwork, limit: usize) {
    println!("--- Initial network log lines ---");
    for (peer_id, path) in network.log_files() {
        println!(
            "{} log snippet (first {} lines) from {}:",
            peer_id,
            limit,
            path.display()
        );
        match File::open(&path) {
            Ok(file) => {
                for line in BufReader::new(file).lines().flatten().take(limit) {
                    println!("{}", line);
                }
            }
            Err(err) => println!("  <error opening log: {}>", err),
        }
    }
    println!("--- End initial network log lines ---");
}

async fn fetch_connected_peers(peer: &freenet_test_network::TestPeer) -> Result<Vec<String>> {
    let url = format!("{}?encodingProtocol=native", peer.ws_url());
    let (ws_stream, _) = connect_async(&url)
        .await
        .map_err(|e| anyhow!("Failed to connect to {}: {}", url, e))?;
    let mut client = WebApi::start(ws_stream);

    client
        .send(ClientRequest::NodeQueries(NodeQuery::ConnectedPeers))
        .await
        .map_err(|e| anyhow!("Failed to send ConnectedPeers query: {}", e))?;

    let response = tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .map_err(|_| anyhow!("Timeout waiting for ConnectedPeers response"))?;

    let peers = match response {
        Ok(HostResponse::QueryResponse(QueryResponse::ConnectedPeers { peers })) => peers
            .into_iter()
            .map(|(peer_id, _)| peer_id.to_string())
            .collect(),
        Ok(other) => return Err(anyhow!("Unexpected ConnectedPeers response: {:?}", other)),
        Err(e) => return Err(anyhow!("ConnectedPeers query failed: {}", e)),
    };

    client.disconnect("topology probe").await;
    Ok(peers)
}

async fn fetch_subscription_info(
    peer: &freenet_test_network::TestPeer,
) -> Result<NetworkDebugInfo> {
    let url = format!("{}?encodingProtocol=native", peer.ws_url());
    let (ws_stream, _) = connect_async(&url)
        .await
        .map_err(|e| anyhow!("Failed to connect to {}: {}", url, e))?;
    let mut client = WebApi::start(ws_stream);

    client
        .send(ClientRequest::NodeQueries(NodeQuery::SubscriptionInfo))
        .await
        .map_err(|e| anyhow!("Failed to send SubscriptionInfo query: {}", e))?;

    let response = tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .map_err(|_| anyhow!("Timeout waiting for SubscriptionInfo response"))?;

    let info = match response {
        Ok(HostResponse::QueryResponse(QueryResponse::NetworkDebug(info))) => info,
        Ok(other) => return Err(anyhow!("Unexpected SubscriptionInfo response: {:?}", other)),
        Err(e) => return Err(anyhow!("SubscriptionInfo query failed: {}", e)),
    };

    client.disconnect("subscription probe").await;
    Ok(info)
}

async fn wait_for_peer_subscription(
    peer: &freenet_test_network::TestPeer,
    contract_key: &ContractKey,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match fetch_subscription_info(peer).await {
            Ok(info) => {
                let subscribed = info
                    .subscriptions
                    .iter()
                    .any(|entry| &entry.contract_key == contract_key);
                if subscribed {
                    return Ok(());
                }
            }
            Err(err) => {
                tracing::warn!("failed to fetch subscription info for {}: {err}", peer.id());
            }
        }

        if Instant::now() >= deadline {
            anyhow::bail!(
                "Timed out waiting for peer {} to report subscription to {}",
                peer.id(),
                contract_key
            );
        }

        sleep(poll_interval).await;
    }
}

async fn dump_topology(network: &TestNetwork) {
    println!("--- Network topology snapshot ---");
    let gw_count = network.gateway_ws_urls().len();
    for idx in 0..gw_count {
        let peer = network.gateway(idx);
        match fetch_connected_peers(peer).await {
            Ok(connections) => println!("gateway {} connections: {:?}", idx, connections),
            Err(err) => println!("gateway {} connections: ERROR {err}", idx),
        }
    }

    let peer_count = network.peer_ws_urls().len();
    for idx in 0..peer_count {
        let peer = network.peer(idx);
        match fetch_connected_peers(peer).await {
            Ok(connections) => println!("peer {} connections: {:?}", idx, connections),
            Err(err) => println!("peer {} connections: ERROR {err}", idx),
        }
    }
    println!("--- End topology snapshot ---");
}

/// Assert that the network has mesh topology (peers connected to each other, not just gateway).
/// This validates that NAT hole punching is working correctly.
/// Returns error if any peer is connected only to the gateway (star topology).
async fn assert_mesh_topology(network: &TestNetwork) -> Result<()> {
    let use_docker_nat = env::var_os("FREENET_TEST_DOCKER_NAT").is_some();
    if !use_docker_nat {
        println!("Skipping mesh topology assertion (Docker NAT not enabled)");
        return Ok(());
    }

    println!("--- Mesh topology assertion (Docker NAT enabled) ---");

    // Collect all gateway IDs
    let mut gateway_ids: HashSet<String> = HashSet::new();
    for idx in 0..network.gateway_ws_urls().len() {
        gateway_ids.insert(network.gateway(idx).id().to_string());
    }

    // Check each peer's connections
    let peer_count = network.peer_ws_urls().len();
    let mut peers_with_p2p_connections = 0usize;

    for idx in 0..peer_count {
        let peer = network.peer(idx);
        let peer_id = peer.id().to_string();
        match fetch_connected_peers(peer).await {
            Ok(connections) => {
                // Count how many connections are to other peers (not gateways)
                let p2p_connections: Vec<_> = connections
                    .iter()
                    .filter(|conn| !gateway_ids.contains(*conn))
                    .collect();

                if !p2p_connections.is_empty() {
                    peers_with_p2p_connections += 1;
                    println!(
                        "peer {} ({}) has {} P2P connections: {:?}",
                        idx,
                        peer_id,
                        p2p_connections.len(),
                        p2p_connections
                    );
                } else {
                    println!(
                        "peer {} ({}) has NO P2P connections (only gateway: {:?})",
                        idx, peer_id, connections
                    );
                }
            }
            Err(err) => {
                println!("peer {} ({}) topology check ERROR: {}", idx, peer_id, err);
            }
        }
    }

    // At least some peers should have P2P connections for mesh topology.
    // With 6 peers, we expect most to have at least one P2P connection.
    let min_expected_p2p_peers = peer_count / 2;
    if peers_with_p2p_connections < min_expected_p2p_peers {
        dump_topology(network).await;
        anyhow::bail!(
            "Mesh topology assertion failed: only {}/{} peers have P2P connections \
             (expected at least {}). This indicates NAT hole punching may not be working. \
             The network has star topology (peers only connected to gateway) instead of mesh.",
            peers_with_p2p_connections,
            peer_count,
            min_expected_p2p_peers
        );
    }

    println!(
        "Mesh topology assertion PASSED: {}/{} peers have P2P connections",
        peers_with_p2p_connections, peer_count
    );
    println!("--- End mesh topology assertion ---");
    Ok(())
}

async fn dump_subscriptions(network: &TestNetwork) {
    println!("--- Subscription snapshot ---");
    let gw_count = network.gateway_ws_urls().len();
    for idx in 0..gw_count {
        let peer = network.gateway(idx);
        match fetch_subscription_info(peer).await {
            Ok(info) => println!("gateway {} subscriptions: {:?}", idx, info),
            Err(err) => println!("gateway {} subscriptions: ERROR {err}", idx),
        }
    }
    let peer_count = network.peer_ws_urls().len();
    for idx in 0..peer_count {
        let peer = network.peer(idx);
        match fetch_subscription_info(peer).await {
            Ok(info) => println!("peer {} subscriptions: {:?}", idx, info),
            Err(err) => println!("peer {} subscriptions: ERROR {err}", idx),
        }
    }
    println!("--- End subscription snapshot ---");
}

async fn subscribe_peer_to_contract(
    peer: &freenet_test_network::TestPeer,
    contract_key: &ContractKey,
) -> Result<WebApi> {
    let url = format!("{}?encodingProtocol=native", peer.ws_url());
    if std::env::var_os("FREENET_TEST_DEBUG_SUBSCRIBE").is_some() {
        println!(
            "debug: subscribing via peer {} at {}",
            peer.id(),
            peer.ws_url()
        );
    }
    let (ws_stream, _) = connect_async(&url)
        .await
        .map_err(|e| anyhow!("Failed to connect to {}: {}", url, e))?;
    let mut client = WebApi::start(ws_stream);

    client
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key: contract_key.clone(),
            return_contract_code: true,
            subscribe: false,
        }))
        .await
        .map_err(|e| anyhow!("Failed to send initial GET request: {}", e))?;

    let effective_key = match tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .map_err(|_| anyhow!("Timeout waiting for initial GET response"))?
    {
        Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { key, .. })) => key,
        Ok(other) => {
            client.disconnect("subscribe init error").await;
            return Err(anyhow!("Unexpected initial GET response: {:?}", other));
        }
        Err(e) => {
            client.disconnect("subscribe init error").await;
            return Err(anyhow!("Initial GET response error: {}", e));
        }
    };

    client
        .send(ClientRequest::ContractOp(ContractRequest::Subscribe {
            key: effective_key.clone(),
            summary: None,
        }))
        .await
        .map_err(|e| anyhow!("Failed to send subscribe request: {}", e))?;

    let subscribe_deadline = Instant::now() + Duration::from_secs(20);

    loop {
        let remaining = subscribe_deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            client.disconnect("subscribe timeout").await;
            return Err(anyhow!("Timeout waiting for subscribe response"));
        }

        match tokio::time::timeout(remaining, client.recv()).await {
            Err(_) => {
                client.disconnect("subscribe timeout").await;
                return Err(anyhow!("Timeout waiting for subscribe response"));
            }
            Ok(Err(e)) => {
                client.disconnect("subscribe error").await;
                return Err(anyhow!("Subscribe response error: {}", e));
            }
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                subscribed,
                ..
            }))) => {
                if !subscribed {
                    client.disconnect("subscribe rejected").await;
                    return Err(anyhow!("Subscription request rejected"));
                }
                break;
            }
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                ..
            }))) => {
                if std::env::var_os("FREENET_TEST_DEBUG_SUBSCRIBE").is_some() {
                    println!(
                        "debug: received update before subscribe response for contract {}",
                        effective_key
                    );
                }
                continue;
            }
            Ok(Ok(other)) => {
                client.disconnect("subscribe unexpected").await;
                return Err(anyhow!("Unexpected subscribe response: {:?}", other));
            }
        }
    }

    Ok(client)
}

async fn fetch_node_diagnostics(
    peer: &freenet_test_network::TestPeer,
    contract_key: &ContractKey,
) -> Result<NodeDiagnosticsResponse> {
    let url = format!("{}?encodingProtocol=native", peer.ws_url());
    let (ws_stream, _) = connect_async(&url)
        .await
        .map_err(|e| anyhow!("Failed to connect to {}: {}", url, e))?;
    let mut client = WebApi::start(ws_stream);

    client
        .send(ClientRequest::NodeQueries(NodeQuery::NodeDiagnostics {
            config: NodeDiagnosticsConfig::for_update_propagation_debugging(contract_key.clone()),
        }))
        .await
        .map_err(|e| anyhow!("Failed to send NodeDiagnostics query: {}", e))?;

    let response = tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .map_err(|_| anyhow!("Timeout waiting for NodeDiagnostics response"))?;

    let info = match response {
        Ok(HostResponse::QueryResponse(QueryResponse::NodeDiagnostics(info))) => info,
        Ok(other) => {
            client.disconnect("diagnostics error").await;
            return Err(anyhow!("Unexpected NodeDiagnostics response: {:?}", other));
        }
        Err(e) => {
            client.disconnect("diagnostics error").await;
            return Err(anyhow!("NodeDiagnostics query failed: {}", e));
        }
    };

    client.disconnect("diagnostics done").await;
    Ok(info)
}

async fn dump_diagnostics(network: &TestNetwork, contract_key: &ContractKey) {
    println!("--- Node diagnostics snapshot ---");
    let gw_count = network.gateway_ws_urls().len();
    for idx in 0..gw_count {
        let peer = network.gateway(idx);
        match fetch_node_diagnostics(peer, contract_key).await {
            Ok(info) => println!("gateway {} diagnostics: {:?}", idx, info.contract_states),
            Err(err) => println!("gateway {} diagnostics: ERROR {err}", idx),
        }
    }
    let peer_count = network.peer_ws_urls().len();
    for idx in 0..peer_count {
        let peer = network.peer(idx);
        match fetch_node_diagnostics(peer, contract_key).await {
            Ok(info) => println!("peer {} diagnostics: {:?}", idx, info.contract_states),
            Err(err) => println!("peer {} diagnostics: ERROR {err}", idx),
        }
    }
    println!("--- End node diagnostics snapshot ---");
}

async fn send_message_or_dump(
    network: &TestNetwork,
    config_dir: &TempDir,
    node_url: &str,
    owner_key: &str,
    message: &str,
    sender_label: &str,
    contract_key: &ContractKey,
) -> Result<()> {
    if let Err(err) = run_riverctl(
        config_dir.path(),
        node_url,
        &["message", "send", owner_key, message],
    )
    .await
    {
        dump_initial_log_lines(network, 20);
        dump_network_logs(network);
        dump_topology(network).await;
        dump_subscriptions(network).await;
        dump_diagnostics(network, contract_key).await;
        return Err(anyhow!(
            "riverctl message send ({sender_label}) failed: {err}"
        ));
    }
    Ok(())
}

async fn wait_for_expected_messages(
    network: &TestNetwork,
    config_dir: &TempDir,
    node_url: &str,
    owner_key: &str,
    expected: &[String],
    participant: &str,
    contract_key: &ContractKey,
    latency_tracker: &mut LatencyTracker,
) -> Result<Vec<String>> {
    let timeout = Duration::from_secs(30);
    let poll_interval = Duration::from_millis(500);
    let deadline = Instant::now() + timeout;
    let expected_lookup: HashSet<&String> = expected.iter().collect();
    let mut seen: HashSet<String> = HashSet::with_capacity(expected.len());

    loop {
        let stdout = match run_riverctl(
            config_dir.path(),
            node_url,
            &["message", "list", owner_key],
        )
        .await
        {
            Ok(output) => output,
            Err(err) => {
                dump_initial_log_lines(network, 20);
                dump_network_logs(network);
                dump_topology(network).await;
                dump_subscriptions(network).await;
                dump_diagnostics(network, contract_key).await;
                return Err(anyhow!(
                    "riverctl message list ({participant}) failed: {err}"
                ));
            }
        };

        let messages: Vec<Value> = serde_json::from_str(&stdout)
            .with_context(|| format!("{participant}: failed to parse message list output"))?;
        let plaintexts = decode_plaintext_messages(&messages);

        if std::env::var("FREENET_TEST_DEBUG_LIST").is_ok() {
            println!(
                "debug: {participant} sees {} messages: {:?}",
                plaintexts.len(),
                plaintexts
            );
        }

        for msg in plaintexts.iter() {
            if expected_lookup.contains(msg) && seen.insert(msg.clone()) {
                latency_tracker.record_delivery(msg);
            }
        }

        if seen.len() == expected.len() {
            return Ok(plaintexts);
        }

        let missing: Vec<_> = expected
            .iter()
            .filter(|msg| !seen.contains(*msg))
            .cloned()
            .collect();

        if missing.is_empty() {
            return Ok(plaintexts);
        }

        if Instant::now() >= deadline {
            dump_initial_log_lines(network, 20);
            dump_network_logs(network);
            dump_topology(network).await;
            dump_subscriptions(network).await;
            dump_diagnostics(network, contract_key).await;
            anyhow::bail!(
                "{participant} missing expected messages {:?}. Current messages: {:?}",
                missing,
                plaintexts
            );
        }

        sleep(poll_interval).await;
    }
}

fn freenet_core_workspace() -> PathBuf {
    if let Ok(path) = std::env::var("FREENET_CORE_PATH") {
        PathBuf::from(path)
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../freenet-core/main")
    }
}

async fn run_message_flow_test(peer_count: usize, rounds: usize) -> Result<()> {
    let scenario = ScenarioOptions::from_env(peer_count, rounds)?;
    let freenet_core = freenet_core_workspace();
    if !freenet_core.exists() {
        anyhow::bail!(
            "Expected freenet-core workspace at {}",
            freenet_core.display()
        );
    }
    println!(
        "test-process: RUST_LOG={}",
        env::var("RUST_LOG").unwrap_or_else(|_| "<unset>".into())
    );

    let preserve_success = env::var_os("FREENET_TEST_NETWORK_KEEP_SUCCESS").is_some();

    let use_docker_nat = env::var_os("FREENET_TEST_DOCKER_NAT").is_some();
    let backend = if use_docker_nat {
        // Verify Docker is available before proceeding - fail fast with clear error
        // rather than failing later with a confusing Docker API error
        let docker_available = std::process::Command::new("docker")
            .args(["info"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        anyhow::ensure!(
            docker_available,
            "FREENET_TEST_DOCKER_NAT is set but Docker is not available. \
             Either install/start Docker or unset FREENET_TEST_DOCKER_NAT to use local backend."
        );
        println!("test-process: Using Docker NAT backend");
        Backend::DockerNat(DockerNatConfig::default())
    } else {
        Backend::Local
    };

    let build_start = Instant::now();
    let network = TestNetwork::builder()
        .gateways(1)
        .peers(scenario.peer_count)
        .binary(FreenetBinary::Workspace {
            path: freenet_core.clone(),
            profile: BuildProfile::Debug,
        })
        .backend(backend)
        .require_connectivity(1.0)
        .connectivity_timeout(Duration::from_secs(120))
        .preserve_temp_dirs_on_failure(true)
        .preserve_temp_dirs_on_success(preserve_success)
        .build()
        .await
        .context("Failed to start Freenet test network")?;
    let startup_duration = build_start.elapsed();

    // Wait for initial topology to stabilize.
    // With Docker NAT, peers need time for topology maintenance to establish P2P connections.
    let stabilization_secs: u64 = env::var("RIVER_TEST_STABILIZATION_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30); // Default 30 seconds for topology maintenance
    println!(
        "Waiting {}s for topology stabilization (set RIVER_TEST_STABILIZATION_SECS to override)...",
        stabilization_secs
    );
    sleep(Duration::from_secs(stabilization_secs)).await;

    for idx in 0..network.gateway_ws_urls().len() {
        println!("gateway[{idx}] id={}", network.gateway(idx).id());
    }
    for idx in 0..network.peer_ws_urls().len() {
        println!("peer[{idx}] id={}", network.peer(idx).id());
    }
    dump_topology(&network).await;
    dump_subscriptions(&network).await;

    // Assert mesh topology when using Docker NAT - this validates NAT hole punching is working
    assert_mesh_topology(&network).await?;

    let mut rooms = plan_rooms(&network, &scenario)?;
    let mut subscription_handles: Vec<WebApi> = Vec::new();
    let mut latency_tracker = LatencyTracker::default();
    let mut total_messages = 0usize;

    for room in rooms.iter_mut() {
        total_messages += setup_room_and_exchange_messages(
            room,
            &network,
            &scenario,
            &mut subscription_handles,
            &mut latency_tracker,
        )
        .await?;
    }

    if let Some(delay_secs) = env::var("FREENET_TEST_SUBSCRIBE_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        if delay_secs > 0 {
            println!(
                "debug: delaying {}s before disconnecting subscription handles",
                delay_secs
            );
            sleep(Duration::from_secs(delay_secs)).await;
        }
    }

    for client in subscription_handles {
        client.disconnect("test complete").await;
    }

    println!("--- Scenario Metrics ---");
    println!(
        "peers={}, rooms={}, users_per_room={}, rounds={}",
        scenario.peer_count, scenario.room_count, scenario.users_per_room, scenario.rounds
    );
    println!(
        "network_startup_seconds={:.2}",
        startup_duration.as_secs_f64()
    );
    println!("messages_sent={}", total_messages);
    if let Some((avg, min, max, count)) = latency_tracker.summary() {
        println!(
            "message_latency_ms: avg={:.2}, min={:.2}, max={:.2}, samples={}",
            avg.as_secs_f64() * 1000.0,
            min.as_secs_f64() * 1000.0,
            max.as_secs_f64() * 1000.0,
            count
        );
    } else {
        println!("No message latency samples recorded.");
    }

    Ok(())
}

fn plan_rooms(network: &TestNetwork, scenario: &ScenarioOptions) -> Result<Vec<RoomContext>> {
    let peer_count = network.peer_ws_urls().len();
    anyhow::ensure!(
        peer_count >= 2,
        "network must have at least two peers (found {})",
        peer_count
    );
    let mut rng = StdRng::seed_from_u64(scenario.rng_seed);
    let mut rooms = Vec::with_capacity(scenario.room_count);
    for room_id in 0..scenario.room_count {
        let mut users = Vec::with_capacity(scenario.users_per_room);
        for user_idx in 0..scenario.users_per_room {
            let peer_index = rng.gen_range(0..peer_count);
            let label = format!("room{}-user{}", room_id + 1, user_idx + 1);
            let config_dir = TempDir::new()
                .with_context(|| format!("Failed to create config dir for {label}"))?;
            users.push(UserClient {
                label,
                peer_index,
                config_dir,
            });
        }
        rooms.push(RoomContext {
            id: room_id,
            users,
            owner_key: None,
            contract_key: None,
            expected_messages: Vec::new(),
        });
    }
    Ok(rooms)
}

async fn setup_room_and_exchange_messages(
    room: &mut RoomContext,
    network: &TestNetwork,
    scenario: &ScenarioOptions,
    subscription_handles: &mut Vec<WebApi>,
    latency_tracker: &mut LatencyTracker,
) -> Result<usize> {
    println!("--- setting up room {} ---", room.id + 1);
    let owner = &room.users[0];
    let owner_peer = network.peer(owner.peer_index);
    let owner_url = node_url(owner_peer);

    let mut create_attempts = 0usize;
    let create_stdout = loop {
        create_attempts += 1;
        let start = Instant::now();
        match run_riverctl(
            owner.config_dir.path(),
            &owner_url,
            &[
                "room",
                "create",
                "--name",
                &format!("River Room {}", room.id + 1),
                "--nickname",
                &owner.label,
            ],
        )
        .await
        {
            Ok(output) => {
                println!(
                    "riverctl room create succeeded in {:.2?} (attempt {create_attempts})",
                    start.elapsed()
                );
                break output;
            }
            Err(err) => {
                println!(
                    "riverctl room create failed after {:.2?} (attempt {create_attempts}): {err}",
                    start.elapsed()
                );
                let timed_out = err.to_string().contains("Timeout waiting for PUT response");
                if timed_out && create_attempts == 1 {
                    println!("Retrying room create once after timeout...");
                    sleep(Duration::from_secs(3)).await;
                    continue;
                }
                dump_full_network_state(network).await;
                return Err(anyhow!("riverctl room create failed: {}", err));
            }
        }
    };
    let create_output: CreateRoomOutput =
        serde_json::from_str(&create_stdout).context("Failed to parse room create output")?;
    let contract_key = ContractKey::from_id(create_output.contract_key.clone())
        .context("Failed to parse contract key from room create output")?;
    room.owner_key = Some(create_output.owner_key.clone());
    room.contract_key = Some(contract_key.clone());

    for user in room.users.iter().skip(1) {
        let invite_stdout = run_riverctl_checked(
            network,
            owner.config_dir.path(),
            &owner_url,
            &["invite", "create", room.owner_key()],
            "riverctl invite create",
        )
        .await?;
        let invite_output: InviteCreateOutput =
            serde_json::from_str(&invite_stdout).context("Failed to parse invite create output")?;

        let peer = network.peer(user.peer_index);
        let user_url = node_url(peer);
        let mut accept_attempts = 0usize;
        let accept_stdout = loop {
            accept_attempts += 1;
            let start = Instant::now();
            match run_riverctl(
                user.config_dir.path(),
                &user_url,
                &[
                    "invite",
                    "accept",
                    &invite_output.invitation_code,
                    "--nickname",
                    &user.label,
                ],
            )
            .await
            {
                Ok(output) => {
                    println!(
                        "riverctl invite accept succeeded in {:.2?} (attempt {accept_attempts})",
                        start.elapsed()
                    );
                    break output;
                }
                Err(err) => {
                    println!(
                        "riverctl invite accept failed after {:.2?} (attempt {accept_attempts}): {err}",
                        start.elapsed()
                    );
                    let timed_out = err.to_string().contains("Timeout waiting for GET response");
                    if timed_out && accept_attempts == 1 {
                        println!("Retrying invite accept once after timeout...");
                        sleep(Duration::from_secs(3)).await;
                        continue;
                    }
                    dump_full_network_state(network).await;
                    return Err(anyhow!("riverctl invite accept failed: {}", err));
                }
            }
        };
        let accept_output: InviteAcceptOutput =
            serde_json::from_str(&accept_stdout).context("Failed to parse invite accept output")?;

        let contract_key_accept = ContractKey::from_id(accept_output.contract_key.clone())
            .context("Failed to parse contract key from invite accept output")?;
        anyhow::ensure!(
            accept_output.room_owner_key == create_output.owner_key,
            "Invite acceptance should reference the same room owner key"
        );
        anyhow::ensure!(
            contract_key_accept == contract_key,
            "Contract key from invite accept must match room create"
        );
    }

    let mut unique_peer_indices = HashSet::new();
    for user in &room.users {
        if unique_peer_indices.insert(user.peer_index) {
            let peer = network.peer(user.peer_index);
            match subscribe_peer_to_contract(peer, room.contract_key()).await {
                Ok(handle) => subscription_handles.push(handle),
                Err(err) => {
                    dump_full_network_state(network).await;
                    return Err(anyhow!(
                        "Failed to subscribe peer {}: {}",
                        user.peer_index,
                        err
                    ));
                }
            }
        }
    }
    dump_subscriptions(network).await;

    for peer_idx in &unique_peer_indices {
        wait_for_peer_subscription(
            network.peer(*peer_idx),
            room.contract_key(),
            Duration::from_secs(10),
            Duration::from_millis(200),
        )
        .await
        .with_context(|| format!("Peer {peer_idx} failed to confirm subscription"))?;
    }

    room.expected_messages.clear();
    for round in 0..scenario.rounds {
        for user in &room.users {
            let message = format!(
                "[room {}][round {}][{}] Hello from {}!",
                room.id + 1,
                round + 1,
                user.label,
                user.label
            );
            let peer = network.peer(user.peer_index);
            let node_url = node_url(peer);
            send_message_or_dump(
                network,
                &user.config_dir,
                &node_url,
                room.owner_key(),
                &message,
                &user.label,
                room.contract_key(),
            )
            .await?;
            latency_tracker.record_send(&message);
            room.expected_messages.push(message.clone());
            sleep(Duration::from_millis(200)).await;
        }
    }

    for user in &room.users {
        let peer = network.peer(user.peer_index);
        let node_url = node_url(peer);
        wait_for_expected_messages(
            network,
            &user.config_dir,
            &node_url,
            room.owner_key(),
            &room.expected_messages,
            &user.label,
            room.contract_key(),
            latency_tracker,
        )
        .await?;
    }

    Ok(room.expected_messages.len())
}

async fn run_riverctl_checked(
    network: &TestNetwork,
    config_dir: &Path,
    node_url: &str,
    args: &[&str],
    label: &str,
) -> Result<String> {
    match run_riverctl(config_dir, node_url, args).await {
        Ok(output) => Ok(output),
        Err(err) => {
            dump_full_network_state(network).await;
            Err(anyhow!("{label} failed: {err}"))
        }
    }
}

async fn dump_full_network_state(network: &TestNetwork) {
    dump_initial_log_lines(network, 20);
    dump_network_logs(network);
    dump_topology(network).await;
    dump_subscriptions(network).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires freenet-core workspace and is currently unstable"]
async fn river_message_flow_over_freenet() -> Result<()> {
    match run_message_flow_test(2, 1).await {
        Ok(()) => Ok(()),
        Err(err) => {
            println!("TEST FAILURE: {err:#?}");
            Err(err)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires freenet-core workspace and is currently unstable"]
async fn river_message_flow_over_freenet_four_peers_three_rounds() -> Result<()> {
    run_message_flow_test(4, 3).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires freenet-core workspace and is currently unstable"]
async fn river_message_flow_over_freenet_six_peers_five_rounds() -> Result<()> {
    run_message_flow_test(6, 5).await
}

/// Test that a peer joining late (via invite accept) can receive updates sent after joining.
///
/// This is a regression test for issue #2306 / PR #2360 where peers that fetch a contract
/// via GET (e.g., when accepting an invite) would fail to receive subsequent UPDATEs due to
/// a key mismatch in the contract store.
///
/// The scenario:
/// 1. Peer A creates a room and sends initial messages
/// 2. Peer B joins late via invite accept (triggers GET to fetch contract)
/// 3. Peer A sends messages AFTER Peer B joins
/// 4. Verify Peer B receives the post-join messages
///
/// Without the fix, Peer B's contract store would fail to find the contract during UPDATE
/// processing because `fetch_contract` uses the key from the GET response (which may have
/// `code: None`) while `store_contract` stored it using the container's key (with code hash).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires freenet-core workspace and is currently unstable"]
async fn river_late_joiner_receives_updates() -> Result<()> {
    run_late_joiner_test().await
}

async fn run_late_joiner_test() -> Result<()> {
    let freenet_core = freenet_core_workspace();
    if !freenet_core.exists() {
        anyhow::bail!(
            "Expected freenet-core workspace at {}",
            freenet_core.display()
        );
    }

    let use_docker_nat = env::var_os("FREENET_TEST_DOCKER_NAT").is_some();
    let backend = if use_docker_nat {
        let docker_available = std::process::Command::new("docker")
            .args(["info"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        anyhow::ensure!(
            docker_available,
            "FREENET_TEST_DOCKER_NAT is set but Docker is not available."
        );
        Backend::DockerNat(DockerNatConfig::default())
    } else {
        Backend::Local
    };

    // Use 3 peers: peer 0 creates room, peer 1 joins immediately, peer 2 joins late
    let network = TestNetwork::builder()
        .gateways(1)
        .peers(3)
        .binary(FreenetBinary::Workspace {
            path: freenet_core.clone(),
            profile: BuildProfile::Debug,
        })
        .backend(backend)
        .require_connectivity(1.0)
        .connectivity_timeout(Duration::from_secs(120))
        .preserve_temp_dirs_on_failure(true)
        .build()
        .await
        .context("Failed to start Freenet test network")?;

    let stabilization_secs: u64 = env::var("RIVER_TEST_STABILIZATION_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    println!("Waiting {}s for topology stabilization...", stabilization_secs);
    sleep(Duration::from_secs(stabilization_secs)).await;

    dump_topology(&network).await;

    // Create config directories for each user
    let owner_config = TempDir::new().context("Failed to create owner config dir")?;
    let early_joiner_config = TempDir::new().context("Failed to create early joiner config dir")?;
    let late_joiner_config = TempDir::new().context("Failed to create late joiner config dir")?;

    // Peer 0 creates the room
    let owner_peer = network.peer(0);
    let owner_url = node_url(owner_peer);
    println!("Step 1: Owner (peer 0) creates room");

    let create_stdout = run_riverctl(
        owner_config.path(),
        &owner_url,
        &["room", "create", "--name", "Late Joiner Test Room", "--nickname", "owner"],
    )
    .await
    .context("Failed to create room")?;

    let create_output: CreateRoomOutput =
        serde_json::from_str(&create_stdout).context("Failed to parse room create output")?;
    let contract_key = ContractKey::from_id(create_output.contract_key.clone())
        .context("Failed to parse contract key")?;
    let owner_key = create_output.owner_key.clone();

    println!("Room created: contract_key={}", contract_key);

    // Peer 1 joins immediately
    println!("Step 2: Early joiner (peer 1) joins via invite");
    let early_peer = network.peer(1);
    let early_url = node_url(early_peer);

    let invite1_stdout = run_riverctl(
        owner_config.path(),
        &owner_url,
        &["invite", "create", &owner_key],
    )
    .await
    .context("Failed to create invite for early joiner")?;
    let invite1: InviteCreateOutput = serde_json::from_str(&invite1_stdout)?;

    run_riverctl(
        early_joiner_config.path(),
        &early_url,
        &["invite", "accept", &invite1.invitation_code, "--nickname", "early_joiner"],
    )
    .await
    .context("Failed to accept invite for early joiner")?;

    // Subscribe both peers and wait for subscription confirmation
    let mut subscription_handles = Vec::new();
    for peer_idx in [0, 1] {
        let peer = network.peer(peer_idx);
        let handle = subscribe_peer_to_contract(peer, &contract_key)
            .await
            .with_context(|| format!("Failed to subscribe peer {}", peer_idx))?;
        subscription_handles.push(handle);
    }

    for peer_idx in [0, 1] {
        wait_for_peer_subscription(
            network.peer(peer_idx),
            &contract_key,
            Duration::from_secs(10),
            Duration::from_millis(200),
        )
        .await
        .with_context(|| format!("Peer {} failed to confirm subscription", peer_idx))?;
    }

    // Owner sends initial messages BEFORE late joiner joins
    println!("Step 3: Owner sends messages BEFORE late joiner joins");
    let pre_join_messages = vec![
        "Pre-join message 1 from owner".to_string(),
        "Pre-join message 2 from owner".to_string(),
    ];

    for msg in &pre_join_messages {
        run_riverctl(
            owner_config.path(),
            &owner_url,
            &["message", "send", &owner_key, msg],
        )
        .await
        .with_context(|| format!("Failed to send pre-join message: {}", msg))?;
        sleep(Duration::from_millis(200)).await;
    }

    // Verify early joiner received the pre-join messages
    println!("Verifying early joiner received pre-join messages...");
    wait_for_expected_messages(
        &network,
        &early_joiner_config,
        &early_url,
        &owner_key,
        &pre_join_messages,
        "early_joiner",
        &contract_key,
        &mut LatencyTracker::default(),
    )
    .await
    .context("Early joiner failed to receive pre-join messages")?;

    // NOW the late joiner (peer 2) joins
    println!("Step 4: Late joiner (peer 2) joins via invite");
    let late_peer = network.peer(2);
    let late_url = node_url(late_peer);

    let invite2_stdout = run_riverctl(
        owner_config.path(),
        &owner_url,
        &["invite", "create", &owner_key],
    )
    .await
    .context("Failed to create invite for late joiner")?;
    let invite2: InviteCreateOutput = serde_json::from_str(&invite2_stdout)?;

    // This is the critical step: late joiner accepts invite, triggering GET to fetch contract
    run_riverctl(
        late_joiner_config.path(),
        &late_url,
        &["invite", "accept", &invite2.invitation_code, "--nickname", "late_joiner"],
    )
    .await
    .context("Failed to accept invite for late joiner")?;

    // Subscribe late joiner
    let late_handle = subscribe_peer_to_contract(late_peer, &contract_key)
        .await
        .context("Failed to subscribe late joiner")?;
    subscription_handles.push(late_handle);

    wait_for_peer_subscription(
        late_peer,
        &contract_key,
        Duration::from_secs(10),
        Duration::from_millis(200),
    )
    .await
    .context("Late joiner failed to confirm subscription")?;

    // Owner sends messages AFTER late joiner joins - this is the critical test
    println!("Step 5: Owner sends messages AFTER late joiner joins");
    let post_join_messages = vec![
        "Post-join message 1 - late joiner should receive this".to_string(),
        "Post-join message 2 - testing UPDATE propagation".to_string(),
    ];

    for msg in &post_join_messages {
        run_riverctl(
            owner_config.path(),
            &owner_url,
            &["message", "send", &owner_key, msg],
        )
        .await
        .with_context(|| format!("Failed to send post-join message: {}", msg))?;
        sleep(Duration::from_millis(200)).await;
    }

    // THE CRITICAL ASSERTION: Late joiner must receive post-join messages
    // Without the fix (PR #2360), this fails because the contract store lookup fails
    println!("Step 6: Verifying late joiner receives post-join messages (CRITICAL TEST)");
    wait_for_expected_messages(
        &network,
        &late_joiner_config,
        &late_url,
        &owner_key,
        &post_join_messages,
        "late_joiner",
        &contract_key,
        &mut LatencyTracker::default(),
    )
    .await
    .context(
        "REGRESSION: Late joiner failed to receive post-join messages. \
         This indicates the contract key mismatch bug (PR #2360) may have regressed. \
         The late joiner fetched the contract via GET, but subsequent UPDATEs failed \
         because the contract store couldn't find the contract with the key used in the UPDATE."
    )?;

    // Also verify early joiner received all messages
    let all_messages: Vec<String> = pre_join_messages
        .into_iter()
        .chain(post_join_messages)
        .collect();

    wait_for_expected_messages(
        &network,
        &early_joiner_config,
        &early_url,
        &owner_key,
        &all_messages,
        "early_joiner",
        &contract_key,
        &mut LatencyTracker::default(),
    )
    .await
    .context("Early joiner failed to receive all messages")?;

    println!("SUCCESS: Late joiner received updates after joining");

    for client in subscription_handles {
        client.disconnect("test complete").await;
    }

    Ok(())
}
