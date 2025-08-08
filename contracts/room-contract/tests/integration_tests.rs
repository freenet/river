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

        let gw_port = common::get_free_port()?;
        let node1_port = common::get_free_port()?;
        let node2_port = common::get_free_port()?;
        let node3_port = common::get_free_port()?;

        let gw_ws_port = common::get_free_port()?;
        let node1_ws_port = common::get_free_port()?;
        let node2_ws_port = common::get_free_port()?;
        let node3_ws_port = common::get_free_port()?;

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

        let (node3_config, _node3_preset) = base_node_test_config(
            false,
            vec![serde_json::to_string(&gw_config_info)?],
            Some(node3_port),
            node3_ws_port,
            "river_test_node3",
            None,
            None,
        )
        .await?;
        let gateway_node = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = gw_config.build().await?;
            let node = NodeConfig::new(config.clone()).await?;
            let gateway_services = serve_gateway(config.ws_api).await;
            let node = node.build(gateway_services).await?;
            node.run().await
        };

        let node1 = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = node1_config.build().await?;
            let node = NodeConfig::new(config.clone()).await?;
            let node1_services = serve_gateway(config.ws_api).await;
            let node = node.build(node1_services).await?;
            node.run().await
        };

        let node2 = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = node2_config.build().await?;
            let node = NodeConfig::new(config.clone()).await?;
            let node2_services = serve_gateway(config.ws_api).await;
            let node = node.build(node2_services).await?;
            node.run().await
        };

        let node3 = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = node3_config.build().await?;
            let node = NodeConfig::new(config.clone()).await?;
            let node3_services = serve_gateway(config.ws_api).await;
            let node = node.build(node3_services).await?;
            node.run().await
        };

        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;

        let network_test = tokio::time::timeout(Duration::from_secs(700),async {
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
            let mut client_node3 = {
                let mut attempts = 0;
                loop {
                    match connect_ws_client(node3_ws_port).await {
                        Ok(client) => break client,
                        Err(e) if attempts < 5 => {
                            attempts += 1;
                            println!("Node3 connection attempt {} failed: {}. Retrying in 3 seconds...", attempts, e);
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        }
                        Err(e) => return Err(format!("Failed to connect to node3 WebSocket after {} attempts: {}", attempts + 1, e).into()),
                    }
                }
            };

            {
                let mut clients_for_diagnostics = vec![&mut client_node1, &mut client_node2, &mut client_node3];
                let node_names = ["Node1", "Node2", "Node3"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![], "INITIAL NETWORK STATE").await;
            }

            let initial_state = RoomTestState::new_test_room();

            // Step 1: Deploy River contract on Node1 (Room Owner's node)
            let contract_key = deploy_room_contract(
                &mut client_node1,
                initial_state.room_state.clone(),
                &initial_state.parameters,
                false, // Don't auto-subscribe during deploy
            ).await.map_err(|e| format!("Failed to deploy River contract: {}", e))?;

            // Step 2: Subscribe user nodes to the contract (Gateway is just infrastructure)
            
            subscribe_to_contract(&mut client_node1, contract_key).await
                .map_err(|e| format!("Node1 subscribe failed: {}", e))?;
            
            subscribe_to_contract(&mut client_node2, contract_key).await
                .map_err(|e| format!("Node2 subscribe failed: {}", e))?;
            
            subscribe_to_contract(&mut client_node3, contract_key).await
                .map_err(|e| format!("Node3 subscribe failed: {}", e))?;


            {
                let mut clients_for_diagnostics = vec![&mut client_node1, &mut client_node2, &mut client_node3];
                let node_names = ["Node1", "Node2", "Node3"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "AFTER SUBSCRIPTIONS").await;
            }

            // Step 3: Wait for contract propagation
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;

            // Step 4: Verify user nodes can retrieve the same state
            
            {
                let mut clients_for_diagnostics = vec![&mut client_node1, &mut client_node2, &mut client_node3];
                let node_names = ["Node1", "Node2", "Node3"];
                let _ = analyze_river_state_consistency(&mut clients_for_diagnostics, &node_names, contract_key).await;
            }
            
            let mut all_clients = vec![&mut client_node1, &mut client_node2, &mut client_node3];
            let states = get_all_room_states(&mut all_clients, contract_key).await
                .map_err(|e| format!("Failed to get states from user nodes: {}", e))?;
            
            if states.len() != 3 {
                return Err("Expected states from 3 nodes".into());
            }
            let (state_node1, state_node2, state_node3) = (&states[0], &states[1], &states[2]);
            
            if !river_states_equal(state_node1, state_node2) {
                let mut clients_for_diagnostics = vec![&mut client_node1, &mut client_node2, &mut client_node3];
                let node_names = ["Node1", "Node2", "Node3"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "STATE MISMATCH DETECTED").await;
                return Err("Node1 and Node2 states differ".into());
            }
            if !river_states_equal(state_node1, state_node3) {
                let mut clients_for_diagnostics = vec![&mut client_node1, &mut client_node2, &mut client_node3];
                let node_names = ["Node1", "Node2", "Node3"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "STATE MISMATCH DETECTED").await;
                return Err("Node1 and Node3 states differ".into());
            }
            if !river_states_equal(state_node2, state_node3) {
                let mut clients_for_diagnostics = vec![&mut client_node1, &mut client_node2, &mut client_node3];
                let node_names = ["Node1", "Node2", "Node3"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "STATE MISMATCH DETECTED").await;
                return Err("Node2 and Node3 states differ".into());
            }

            // Node1 sends message
            let mut all_clients = vec![&mut client_node1, &mut client_node2, &mut client_node3];
            let states = get_all_room_states(&mut all_clients, contract_key).await?;
            let member1_key = RoomTestState::get_member_key(1);
            send_test_message(&mut client_node1, contract_key, &states[0], &initial_state.parameters, "Message from Node1".to_string(), &member1_key).await?;
            wait_for_update_response(&mut client_node1, &contract_key).await?;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // Node2 sends message  
            let mut all_clients = vec![&mut client_node1, &mut client_node2, &mut client_node3];
            let states = get_all_room_states(&mut all_clients, contract_key).await?;
            let member2_key = RoomTestState::get_member_key(2);
            send_test_message(&mut client_node2, contract_key, &states[0], &initial_state.parameters, "Message from Node2".to_string(), &member2_key).await?;
            wait_for_update_response(&mut client_node2, &contract_key).await?;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // Node3 sends message
            let mut all_clients = vec![&mut client_node1, &mut client_node2, &mut client_node3];
            let states = get_all_room_states(&mut all_clients, contract_key).await?;
            let member3_key = RoomTestState::get_member_key(3);
            send_test_message(&mut client_node3, contract_key, &states[0], &initial_state.parameters, "Message from Node3".to_string(), &member3_key).await?;
            wait_for_update_response(&mut client_node3, &contract_key).await?;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            {
                let mut clients_for_diagnostics = vec![&mut client_node1, &mut client_node2, &mut client_node3];
                let node_names = ["Node1", "Node2", "Node3"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![contract_key], "FINAL STATE AFTER UPDATE").await;
            }

            // Final consistency check
            {
                let mut clients_for_diagnostics = vec![&mut client_node1, &mut client_node2, &mut client_node3];
                let node_names = ["Node1", "Node2", "Node3"];
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
            result = node3 => {
                match result {
                    Ok(_) => Err("Node3 exited unexpectedly".into()),
                    Err(e) => Err(format!("Node3 failed: {}", e).into())
                }
            }
            result = network_test => {
                match result {
                    Ok(inner_result) => inner_result,
                    Err(_timeout_error) => Err("Network test timed out after 700 seconds".into())
                }
            }
        }
    }
    .instrument(span)
    .await
}