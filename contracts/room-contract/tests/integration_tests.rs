#![cfg(not(target_arch = "wasm32"))]

mod common;

use std::time::Duration;
use common::{
    base_node_test_config, connect_ws_client, river_states_equal, RoomTestState, gw_config_from_path,
    deploy_room_contract, subscribe_to_contract, send_test_message, wait_for_update_response,
    get_all_room_states, collect_river_node_diagnostics, analyze_river_state_consistency,
};
use freenet_stdlib::prelude::*;
use testresult::TestResult;
use tracing::{level_filters::LevelFilter, span, Instrument, Level};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_river_multi_node() -> TestResult {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::INFO)
        .init();

    let span = span!(Level::INFO, "test_river_multi_node");
    async move {
        println!("=== REAL NETWORK MULTI-NODE RIVER TEST ===");
        println!("This test uses actual Freenet nodes with real network communication");

        let gw_port = common::get_free_port()?;
        let node1_port = common::get_free_port()?;
        let node2_port = common::get_free_port()?;

        let gw_ws_port = common::get_free_port()?;
        let node1_ws_port = common::get_free_port()?;
        let node2_ws_port = common::get_free_port()?;

        println!("Network topology configured:");
        println!("  Gateway: {}:{} (WebSocket: {})", "127.0.0.1", gw_port, gw_ws_port);
        println!("  Node1:   {}:{} (WebSocket: {})", "127.0.0.1", node1_port, node1_ws_port);
        println!("  Node2:   {}:{} (WebSocket: {})", "127.0.0.1", node2_port, node2_ws_port);

        println!("\n=== CONFIGURING FREENET NODES ===");
        let (gw_config, _gw_preset, gw_config_info) = {
            let (cfg, preset) = base_node_test_config(
                true,
                vec![],
                Some(gw_port),
                gw_ws_port,
                "river_test_gw",
                None,
                None,
            )
            .await?;
            let public_port = cfg.network_api.public_port.unwrap();
            let path = preset.temp_dir.path().to_path_buf();
            (cfg, preset, gw_config_from_path(public_port, &path)?)
        };
        println!("Gateway configuration created");

        let (node1_config, _node1_preset) = base_node_test_config(
            false,
            vec![serde_json::to_string(&gw_config_info)?],
            Some(node1_port),
            node1_ws_port,
            "river_test_node1",
            None,
            None,
        )
        .await?;
        println!("Node1 configuration created (connects to gateway)");

        let (node2_config, _node2_preset) = base_node_test_config(
            false,
            vec![serde_json::to_string(&gw_config_info)?],
            Some(node2_port),
            node2_ws_port,
            "river_test_node2",
            None,
            None,
        )
        .await?;
        println!("Node2 configuration created (connects to gateway)");

        // Start gateway node
        println!("\n=== STARTING FREENET NODES ===");
        println!("Starting gateway node...");
        let gateway_node = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            println!("Building gateway config...");
            let config = gw_config.build().await.map_err(|e| {
                println!("Gateway config build failed: {}", e);
                e
            })?;
            println!("Gateway config built successfully");
            
            println!("Creating gateway NodeConfig...");
            let node = NodeConfig::new(config.clone()).await.map_err(|e| {
                println!("Gateway NodeConfig creation failed: {}", e);
                e
            })?;
            println!("Gateway NodeConfig created");
            
            println!("Starting gateway WebSocket services...");
            let gateway_services = serve_gateway(config.ws_api).await;
            println!("Gateway services started");
            
            println!("Building gateway node...");
            let node = node.build(gateway_services).await.map_err(|e| {
                println!("Gateway node build failed: {}", e);
                e
            })?;
            println!("Gateway node built successfully, starting to run...");
            
            node.run().await.map_err(|e| {
                println!("Gateway node run failed: {}", e);
                e
            })
        };

        // Start regular nodes
        println!("Starting Node1 with WebSocket API...");
        let node1 = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = node1_config.build().await.map_err(|e| {
                println!("Node1 config build failed: {}", e);
                e
            })?;
            println!("Node1 config built successfully");
            let node = NodeConfig::new(config.clone()).await.map_err(|e| {
                println!("Node1 NodeConfig creation failed: {}", e);
                e
            })?;
            println!("Node1 NodeConfig created");
            let node1_services = serve_gateway(config.ws_api).await;
            let node = node.build(node1_services).await.map_err(|e| {
                println!("Node1 build failed: {}", e);
                e
            })?;
            println!("Node1 built with WebSocket API and running");
            node.run().await.map_err(|e| {
                println!("Node1 run failed: {}", e);
                e
            })
        };

        println!("Starting Node2 with WebSocket API...");
        let node2 = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = node2_config.build().await.map_err(|e| {
                println!("Node2 config build failed: {}", e);
                e
            })?;
            println!("Node2 config built successfully");
            let node = NodeConfig::new(config.clone()).await.map_err(|e| {
                println!("Node2 NodeConfig creation failed: {}", e);
                e
            })?;
            println!("Node2 NodeConfig created");
            let node2_services = serve_gateway(config.ws_api).await;
            let node = node.build(node2_services).await.map_err(|e| {
                println!("Node2 build failed: {}", e);
                e
            })?;
            println!("Node2 built with WebSocket API and running");
            node.run().await.map_err(|e| {
                println!("Node2 run failed: {}", e);
                e
            })
        };

        // Allow nodes time to start and establish connections
        println!("Waiting for nodes to start and establish network connections...");
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        println!("Initial startup phase completed");
        
        // Wait specifically for WebSocket services to be available
        println!("Waiting for WebSocket services to become available...");
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        println!("WebSocket availability window completed");

        let network_test = tokio::time::timeout(Duration::from_secs(300),async {
            println!("\n=== ESTABLISHING WEBSOCKET CONNECTIONS ===");
            
            println!("Connecting to gateway WebSocket on port {}...", gw_ws_port);
            let mut client_gw = {
                let mut attempts = 0;
                loop {
                    match connect_ws_client(gw_ws_port).await {
                        Ok(client) => {
                            println!("Gateway WebSocket connection established successfully on attempt {}", attempts + 1);
                            break client;
                        },
                        Err(e) if attempts < 10 => { // Increased retry attempts
                            attempts += 1;
                            println!("Gateway connection attempt {} failed: {}. Retrying in 2 seconds...", attempts, e);
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                        Err(e) => {
                            println!("CRITICAL: Gateway WebSocket failed after {} attempts", attempts + 1);
                            return Err(format!("Failed to connect to gateway WebSocket after {} attempts: {}", attempts + 1, e).into());
                        }
                    }
                }
            };
            println!("Gateway WebSocket connection established");

            println!("Connecting to Node1 WebSocket on port {}...", node1_ws_port);
            let mut client_node1 = {
                let mut attempts = 0;
                loop {
                    match connect_ws_client(node1_ws_port).await {
                        Ok(client) => break client,
                        Err(e) if attempts < 5 => {
                            attempts += 1;
                            println!("Node1 connection attempt {} failed: {}. Retrying in 3 seconds...", attempts, e);
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        }
                        Err(e) => return Err(format!("Failed to connect to node1 WebSocket after {} attempts: {}", attempts + 1, e).into()),
                    }
                }
            };
            println!("Node1 WebSocket connection established");

            println!("Connecting to Node2 WebSocket on port {}...", node2_ws_port);
            let mut client_node2 = {
                let mut attempts = 0;
                loop {
                    match connect_ws_client(node2_ws_port).await {
                        Ok(client) => break client,
                        Err(e) if attempts < 5 => {
                            attempts += 1;
                            println!("Node2 connection attempt {} failed: {}. Retrying in 3 seconds...", attempts, e);
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        }
                        Err(e) => return Err(format!("Failed to connect to node2 WebSocket after {} attempts: {}", attempts + 1, e).into()),
                    }
                }
            };
            println!("Node2 WebSocket connection established");

            println!("All WebSocket connections active - network communication ready");

            {
                let mut clients_for_diagnostics = vec![&mut client_gw, &mut client_node1, &mut client_node2];
                let node_names = ["Gateway", "Node1", "Node2"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![], "INITIAL NETWORK STATE").await;
            }

            println!("\n=== TESTING REAL RIVER CONTRACT DEPLOYMENT AND SYNCHRONIZATION ===");
            let initial_state = RoomTestState::new_test_room();
            println!("Created test River room state:");
            println!("  Room owner: {:?}", initial_state.parameters.owner);
            println!("  Initial members: {}", initial_state.room_state.members.members.len());
            println!("  Initial messages: {}", initial_state.room_state.recent_messages.messages.len());
            println!("  Initial bans: {}", initial_state.room_state.bans.0.len());
            println!("  Max members allowed: {}", initial_state.room_state.configuration.configuration.max_members);
            println!("  Max messages: {}", initial_state.room_state.configuration.configuration.max_recent_messages);

            // Step 1: Deploy River contract on gateway node
            println!("\n=== STEP 1: DEPLOYING RIVER CONTRACT ON GATEWAY ===");
            let contract_key = deploy_room_contract(
                &mut client_gw,
                initial_state.room_state.clone(),
                &initial_state.parameters,
                false, // Don't auto-subscribe during deploy
            ).await.map_err(|e| format!("Failed to deploy River contract: {}", e))?;
            println!("✓ River contract deployed successfully with key: {:?}", contract_key);

            // Step 2: Subscribe all nodes to the contract
            println!("\n=== STEP 2: SUBSCRIBING ALL NODES TO RIVER CONTRACT ===");
            subscribe_to_contract(&mut client_gw, contract_key).await
                .map_err(|e| format!("Gateway subscribe failed: {}", e))?;
            println!("✓ Gateway subscribed to River contract");
            
            subscribe_to_contract(&mut client_node1, contract_key).await
                .map_err(|e| format!("Node1 subscribe failed: {}", e))?;
            println!("✓ Node1 subscribed to River contract");
            
            subscribe_to_contract(&mut client_node2, contract_key).await
                .map_err(|e| format!("Node2 subscribe failed: {}", e))?;
            println!("✓ Node2 subscribed to River contract");

            {
                let mut clients_for_diagnostics = vec![&mut client_gw, &mut client_node1, &mut client_node2];
                let node_names = ["Gateway", "Node1", "Node2"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "AFTER SUBSCRIPTIONS").await;
            }

            // Step 3: Wait for contract propagation
            println!("\n=== STEP 3: WAITING FOR CONTRACT PROPAGATION ===");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            println!("Contract propagation wait completed");

            // Step 4: Verify all nodes can retrieve the same state
            println!("\n=== STEP 4: VERIFYING STATE CONSISTENCY ACROSS NODES ===");
            
            {
                let mut clients_for_diagnostics = vec![&mut client_gw, &mut client_node1, &mut client_node2];
                let node_names = ["Gateway", "Node1", "Node2"];
                let _ = analyze_river_state_consistency(&mut clients_for_diagnostics, &node_names, contract_key).await;
            }
            
            let (state_gw, state_node1, state_node2) = get_all_room_states(
                &mut client_gw,
                &mut client_node1, 
                &mut client_node2,
                contract_key
            ).await.map_err(|e| format!("Failed to get states from all nodes: {}", e))?;
            
            println!("✓ Retrieved states from all nodes successfully");
            
            if !river_states_equal(&state_gw, &state_node1) {
                let mut clients_for_diagnostics = vec![&mut client_gw, &mut client_node1, &mut client_node2];
                let node_names = ["Gateway", "Node1", "Node2"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "STATE MISMATCH DETECTED").await;
                return Err("Gateway and Node1 states differ".into());
            }
            if !river_states_equal(&state_gw, &state_node2) {
                let mut clients_for_diagnostics = vec![&mut client_gw, &mut client_node1, &mut client_node2];
                let node_names = ["Gateway", "Node1", "Node2"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "STATE MISMATCH DETECTED").await;
                return Err("Gateway and Node2 states differ".into());
            }
            if !river_states_equal(&state_node1, &state_node2) {
                let mut clients_for_diagnostics = vec![&mut client_gw, &mut client_node1, &mut client_node2];
                let node_names = ["Gateway", "Node1", "Node2"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "STATE MISMATCH DETECTED").await;
                return Err("Node1 and Node2 states differ".into());
            }
            println!("✓ All nodes have identical River room states");

            // Step 5: Test state update propagation using realistic message sending
            println!("\n=== STEP 5: TESTING STATE UPDATE VIA MESSAGE SENDING ===");
            
            let test_message = "Hello from integration test!";
            send_test_message(
                &mut client_gw, 
                contract_key, 
                &state_gw, 
                &initial_state.parameters,
                test_message.to_string(),
                &initial_state.owner_key
            ).await.map_err(|e| format!("Failed to send test message: {}", e))?;
            
            wait_for_update_response(&mut client_gw, &contract_key).await
                .map_err(|e| format!("Failed to receive update response: {}", e))?;
            println!("[UPDATE] Message update completed successfully");

            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            println!("[UPDATE] Waiting for message propagation across nodes...");

            {
                let mut clients_for_diagnostics = vec![&mut client_gw, &mut client_node1, &mut client_node2];
                let node_names = ["Gateway", "Node1", "Node2"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "FINAL STATE AFTER UPDATE").await;
            }

            {
                let mut clients_for_diagnostics = vec![&mut client_gw, &mut client_node1, &mut client_node2];
                let node_names = ["Gateway", "Node1", "Node2"];
                let _ = analyze_river_state_consistency(&mut clients_for_diagnostics, &node_names, contract_key).await;
            }
            
            Ok(())
        }).instrument(span!(Level::INFO, "test_river_multi_node_network_test"));

        tokio::select! {
            result = gateway_node => {
                match result {
                    Ok(_) => Err("Gateway node exited unexpectedly".into()),
                    Err(e) => Err(format!("Gateway node failed: {}", e).into())
                }
            }
            result = node1 => {
                match result {
                    Ok(_) => Err("Node1 exited unexpectedly".into()),
                    Err(e) => Err(format!("Node1 failed: {}", e).into())
                }
            }
            result = node2 => {
                match result {
                    Ok(_) => Err("Node2 exited unexpectedly".into()),
                    Err(e) => Err(format!("Node2 failed: {}", e).into())
                }
            }
            result = network_test => {
                match result {
                    Ok(inner_result) => inner_result,
                    Err(_timeout_error) => Err("Network test timed out after 300 seconds".into())
                }
            }
        }
    }
    .instrument(span)
    .await
}