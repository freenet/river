use std::{
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
use freenet_test_network::{BuildProfile, FreenetBinary, TestNetwork};
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

fn node_url(peer: &freenet_test_network::TestPeer) -> String {
    format!("{}?encodingProtocol=native", peer.ws_url())
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

            match ch {
                '"' => in_string = true,
                '{' | '[' => {
                    if start_idx.is_none() {
                        start_idx = Some(idx);
                    }
                    stack.push(if ch == '{' { '}' } else { ']' });
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
                }
                _ => {}
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
    extract_json_payload(&stdout)
        .ok_or_else(|| anyhow!("Failed to find JSON payload in output:\n{}", stdout))
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

    match tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .map_err(|_| anyhow!("Timeout waiting for initial GET response"))?
    {
        Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { .. })) => {}
        Ok(other) => {
            client.disconnect("subscribe init error").await;
            return Err(anyhow!("Unexpected initial GET response: {:?}", other));
        }
        Err(e) => {
            client.disconnect("subscribe init error").await;
            return Err(anyhow!("Initial GET response error: {}", e));
        }
    }

    client
        .send(ClientRequest::ContractOp(ContractRequest::Subscribe {
            key: contract_key.clone(),
            summary: None,
        }))
        .await
        .map_err(|e| anyhow!("Failed to send subscribe request: {}", e))?;

    match tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .map_err(|_| anyhow!("Timeout waiting for subscribe response"))?
    {
        Ok(HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
            subscribed,
            ..
        })) => {
            if !subscribed {
                client.disconnect("subscribe rejected").await;
                return Err(anyhow!("Subscription request rejected"));
            }
        }
        Ok(other) => {
            client.disconnect("subscribe unexpected").await;
            return Err(anyhow!("Unexpected subscribe response: {:?}", other));
        }
        Err(e) => {
            client.disconnect("subscribe error").await;
            return Err(anyhow!("Subscribe response error: {}", e));
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
) -> Result<Vec<String>> {
    let timeout = Duration::from_secs(30);
    let poll_interval = Duration::from_millis(500);
    let deadline = Instant::now() + timeout;

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

        let missing: Vec<_> = expected
            .iter()
            .filter(|msg| !plaintexts.contains(msg))
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../freenet-core/main")
}

async fn run_message_flow_test(peer_count: usize, rounds: usize) -> Result<()> {
    anyhow::ensure!(peer_count >= 2, "test requires at least two peers");
    anyhow::ensure!(rounds >= 1, "test requires at least one messaging round");
    let freenet_core = freenet_core_workspace();
    if !freenet_core.exists() {
        anyhow::bail!(
            "Expected freenet-core workspace at {}",
            freenet_core.display()
        );
    }
    println!(
        "test-process: RUST_LOG={}",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "<unset>".into())
    );

    let network = TestNetwork::builder()
        .gateways(1)
        .peers(peer_count)
        .binary(FreenetBinary::Workspace {
            path: freenet_core.clone(),
            profile: BuildProfile::Debug,
        })
        .require_connectivity(1.0)
        .connectivity_timeout(Duration::from_secs(120))
        .preserve_temp_dirs_on_failure(true)
        .build()
        .await
        .context("Failed to start Freenet test network")?;

    let alice_dir = TempDir::new().context("Failed to create Alice config dir")?;
    let bob_dir = TempDir::new().context("Failed to create Bob config dir")?;
    let mut subscription_handles: Vec<WebApi> = Vec::new();

    let peer0 = network.peer(0);
    let peer1 = network.peer(1);
    let peer0_url = node_url(peer0);
    let peer1_url = node_url(peer1);

    sleep(Duration::from_secs(3)).await;
    for idx in 0..network.gateway_ws_urls().len() {
        println!("gateway[{idx}] id={}", network.gateway(idx).id());
    }
    for idx in 0..network.peer_ws_urls().len() {
        println!("peer[{idx}] id={}", network.peer(idx).id());
    }
    dump_topology(&network).await;
    dump_subscriptions(&network).await;

    let create_stdout = match run_riverctl(
        alice_dir.path(),
        &peer0_url,
        &[
            "room",
            "create",
            "--name",
            "River Test Room",
            "--nickname",
            "Alice",
        ],
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            dump_initial_log_lines(&network, 20);
            dump_network_logs(&network);
            dump_topology(&network).await;
            dump_subscriptions(&network).await;
            return Err(anyhow!("riverctl room create failed: {}", err));
        }
    };
    let create_output: CreateRoomOutput =
        serde_json::from_str(&create_stdout).context("Failed to parse room create output")?;
    let contract_key = ContractKey::from_id(create_output.contract_key.clone())
        .context("Failed to parse contract key from room create output")?;

    let invite_stdout = match run_riverctl(
        alice_dir.path(),
        &peer0_url,
        &["invite", "create", &create_output.owner_key],
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            dump_initial_log_lines(&network, 20);
            dump_network_logs(&network);
            dump_topology(&network).await;
            dump_subscriptions(&network).await;
            return Err(anyhow!("riverctl invite create failed: {}", err));
        }
    };
    let invite_output: InviteCreateOutput =
        serde_json::from_str(&invite_stdout).context("Failed to parse invite create output")?;

    let accept_stdout = match run_riverctl(
        bob_dir.path(),
        &peer1_url,
        &[
            "invite",
            "accept",
            &invite_output.invitation_code,
            "--nickname",
            "Bob",
        ],
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            dump_initial_log_lines(&network, 20);
            dump_network_logs(&network);
            dump_topology(&network).await;
            dump_subscriptions(&network).await;
            return Err(anyhow!("riverctl invite accept failed: {}", err));
        }
    };
    let accept_output: InviteAcceptOutput =
        serde_json::from_str(&accept_stdout).context("Failed to parse invite accept output")?;
    let contract_key_accept = ContractKey::from_id(accept_output.contract_key.clone())
        .context("Failed to parse contract key from invite accept output")?;

    assert_eq!(
        accept_output.room_owner_key, create_output.owner_key,
        "Invite acceptance should reference the same room owner key"
    );
    assert_eq!(
        contract_key_accept, contract_key,
        "Contract key from invite accept must match room create"
    );

    subscription_handles.push(
        match subscribe_peer_to_contract(peer0, &contract_key).await {
            Ok(handle) => handle,
            Err(err) => {
                dump_initial_log_lines(&network, 20);
                dump_network_logs(&network);
                dump_subscriptions(&network).await;
                return Err(anyhow!("Failed to subscribe peer0 to contract: {}", err));
            }
        },
    );
    subscription_handles.push(
        match subscribe_peer_to_contract(peer1, &contract_key).await {
            Ok(handle) => handle,
            Err(err) => {
                dump_initial_log_lines(&network, 20);
                dump_network_logs(&network);
                dump_subscriptions(&network).await;
                return Err(anyhow!("Failed to subscribe peer1 to contract: {}", err));
            }
        },
    );
    dump_subscriptions(&network).await;

    sleep(Duration::from_secs(2)).await;

    let mut expected_messages: Vec<String> = Vec::new();
    for round in 0..rounds {
        let alice_message = format!("Hello from Alice! round {}", round + 1);
        expected_messages.push(alice_message.clone());
        send_message_or_dump(
            &network,
            &alice_dir,
            &peer0_url,
            &create_output.owner_key,
            alice_message.as_str(),
            "Alice",
            &contract_key,
        )
        .await?;

        sleep(Duration::from_secs(1)).await;

        let bob_message = format!("Hello from Bob! round {}", round + 1);
        expected_messages.push(bob_message.clone());
        send_message_or_dump(
            &network,
            &bob_dir,
            &peer1_url,
            &create_output.owner_key,
            bob_message.as_str(),
            "Bob",
            &contract_key,
        )
        .await?;

        sleep(Duration::from_secs(1)).await;
    }

    let _alice_plaintexts = wait_for_expected_messages(
        &network,
        &alice_dir,
        &peer0_url,
        &create_output.owner_key,
        &expected_messages,
        "Alice",
        &contract_key,
    )
    .await?;
    let _bob_plaintexts = wait_for_expected_messages(
        &network,
        &bob_dir,
        &peer1_url,
        &create_output.owner_key,
        &expected_messages,
        "Bob",
        &contract_key,
    )
    .await?;

    for client in subscription_handles {
        client.disconnect("test complete").await;
    }

    Ok(())
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
