#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{
    collect_river_node_diagnostics, connect_ws_with_retries, deploy_room_contract,
    get_all_room_states, river_states_equal, send_test_message, subscribe_to_contract,
    update_room_state_delta, wait_for_update_response, RoomTestState,
};
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::*;
use rand::SeedableRng;
use std::time::Duration;
use testresult::TestResult;
use tracing::{level_filters::LevelFilter, span, Instrument, Level};

// TODO-MUST-FIX: Test is flaky - multiple issues including network setup and message duplication.
// See: https://github.com/freenet/river/issues/50
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(unreachable_code)]
async fn test_invitation_message_propagation() -> TestResult {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::INFO)
        .init();

    let span = span!(Level::INFO, "test_invitation_message_propagation");
    async move {

        let gw_port = common::get_free_port()?;
        let alice_port = common::get_free_port()?;
        let bob_port = common::get_free_port()?;
        let charlie_port = common::get_free_port()?;

        let gw_ws_port = common::get_free_port()?;
        let alice_ws_port = common::get_free_port()?;
        let bob_ws_port = common::get_free_port()?;
        let charlie_ws_port = common::get_free_port()?;

            let test_seed = *b"river_invite_test_12345678901234";
        let mut test_rng = rand::rngs::StdRng::from_seed(test_seed);

        println!("Using deterministic test seed: {test_seed:?}");
        println!("Test RNG initial state configured for deterministic network topology");

        let (gw_config, _gw_preset) = common::base_node_test_config_with_rng(
            true,
            vec![],
            Some(gw_port),
            gw_ws_port,
            "river_test_gw_invite",
            None,
            None,
            Some(0.0), // Gateway at 0.0
            &mut test_rng,
        ).await?;
        let gw_config_info = common::gw_config_from_path_with_rng(
            gw_config.network_api.public_port.unwrap(),
            _gw_preset.temp_dir.path(),
            &mut test_rng,
        )?;

        let (alice_config, _alice_preset) = common::base_node_test_config_with_rng(
            false,
            vec![serde_json::to_string(&gw_config_info)?],
            Some(alice_port),
            alice_ws_port,
            "river_test_alice_invite",
            None,
            None,
            Some(0.01), // Alice at 0.01 (close to gateway)
            &mut test_rng,
        ).await?;

        let (bob_config, _bob_preset) = common::base_node_test_config_with_rng(
            false,
            vec![serde_json::to_string(&gw_config_info)?],
            Some(bob_port),
            bob_ws_port,
            "river_test_bob_invite",
            None,
            None,
            Some(0.02), // Bob at 0.02 (close to Alice)
            &mut test_rng,
        ).await?;

        let (charlie_config, _charlie_preset) = common::base_node_test_config_with_rng(
            false,
            vec![serde_json::to_string(&gw_config_info)?],
            Some(charlie_port),
            charlie_ws_port,
            "river_test_charlie_invite",
            None,
            None,
            Some(0.03), // Charlie at 0.03 (close to Bob)
            &mut test_rng,
        ).await?;

        let gateway_node = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = gw_config.build().await?;
            let node = NodeConfig::new(config.clone()).await?;
            let gateway_services = serve_gateway(config.ws_api).await?;
            let node = node.build(gateway_services).await?;
            node.run().await
        };

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let alice_node = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = alice_config.build().await?;
            let node = NodeConfig::new(config.clone()).await?;
            let alice_services = serve_gateway(config.ws_api).await?;
            let node = node.build(alice_services).await?;
            node.run().await
        };

        let bob_node = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = bob_config.build().await?;
            let node = NodeConfig::new(config.clone()).await?;
            let bob_services = serve_gateway(config.ws_api).await?;
            let node = node.build(bob_services).await?;
            node.run().await
        };

        let charlie_node = async {
            use freenet::{local_node::NodeConfig, server::serve_gateway};
            let config = charlie_config.build().await?;
            let node = NodeConfig::new(config.clone()).await?;
            let charlie_services = serve_gateway(config.ws_api).await?;
            let node = node.build(charlie_services).await?;
            node.run().await
        };


        let alice_signing_key = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
        let alice_verifying_key = alice_signing_key.verifying_key();
        let initial_state = RoomTestState::new_test_room();

        println!("[CONFIG] Pre-configured Alice owner key: {:?}", alice_verifying_key);
        println!("[CONFIG] Pre-configured initial state with {} members", initial_state.room_state.members.members.len());

        tokio::time::sleep(std::time::Duration::from_secs(20)).await;

        let network_test = tokio::time::timeout(Duration::from_secs(200), async move {
            let mut client_node1 = connect_ws_with_retries(alice_ws_port, "Alice", 5).await?;
            let mut _bob_client = connect_ws_with_retries(bob_ws_port, "Bob", 5).await?;
            let mut _charlie_client = connect_ws_with_retries(charlie_ws_port, "Charlie", 5).await?;
            let mut _gateway_client = connect_ws_with_retries(gw_ws_port, "Gateway", 5).await?;

            println!("Waiting for P2P connectivity with close topology (4 nodes)...");
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            {
                let mut clients_for_diagnostics = vec![&mut client_node1, &mut _bob_client, &mut _charlie_client, &mut _gateway_client];
                let node_names = ["Alice", "Bob", "Charlie", "Gateway"];
                let _ = collect_river_node_diagnostics(&mut clients_for_diagnostics, &node_names, vec![], "AFTER 100S WAIT - P2P CONNECTIVITY CHECK").await;
            }

            println!("=== River Invitation Flow Test ===");
            println!("Testing: Gateway + Alice (Peer 1) + Bob (Peer 2)");

            println!("\n[STEP 1] Create a room on Alice");
            println!("   - Using pre-configured Alice owner key: {:?}", alice_verifying_key);

            println!("Deploying contract...");
            let contract_key = deploy_room_contract(
                &mut client_node1,
                initial_state.room_state.clone(),
                &initial_state.parameters,
                false,
            ).await.map_err(|e| format!("Failed to deploy River contract: {}", e))?;

            println!("\n[STEP 2] Alice subscribes to the room");
            println!("About to call subscribe_to_contract for Alice...");

            let subscribe_start = std::time::Instant::now();
            subscribe_to_contract(&mut client_node1, contract_key).await
                .map_err(|e| format!("Alice subscribe failed after {:?}: {}", subscribe_start.elapsed(), e))?;

            println!("[SUCCESS] Alice subscribe completed successfully in {:?}", subscribe_start.elapsed());

            tokio::time::sleep(Duration::from_secs(2)).await;
            println!("[STEP 3] Continuing to Step 3 after 2s sleep...");

            println!("\n[STEP 3] Alice creates invitation for Bob");

            let bob_signing_key = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
            let bob_verifying_key = bob_signing_key.verifying_key();

            println!("   - Bob's key: {:?}", bob_verifying_key);

            let bob_member = river_core::room_state::member::Member {
                owner_member_id: alice_verifying_key.into(),
                member_vk: bob_verifying_key,
                invited_by: alice_verifying_key.into(),
            };

            let authorized_bob_member = river_core::room_state::member::AuthorizedMember::new(
                bob_member, &alice_signing_key
            );

            println!("   - Created authorized member for Bob");

            println!("\n Step 4: Bob accepts invitation and joins room");

            subscribe_to_contract(&mut _bob_client, contract_key).await
                .map_err(|e| format!("Bob subscribe failed: {}", e))?;

            let mut bob_clients = vec![&mut _bob_client];
            let bob_room_states = get_all_room_states(&mut bob_clients, contract_key).await
                .map_err(|e| format!("Bob failed to get room state: {}", e))?;
            let mut bob_room_state = bob_room_states[0].clone();

            let bob_members_delta = river_core::room_state::member::MembersDelta::new(
                vec![authorized_bob_member.clone()]
            );

            bob_room_state.members.apply_delta(&bob_room_state.clone(), &initial_state.parameters, &Some(bob_members_delta))
                .map_err(|e| format!("Failed to add Bob to members: {}", e))?;

            let bob_member_info = river_core::room_state::member_info::MemberInfo {
                member_id: bob_signing_key.verifying_key().into(),
                version: 0,
                preferred_nickname: river_core::room_state::privacy::SealedBytes::public("Bob".to_string().into_bytes()),
            };
            let authorized_bob_info = river_core::room_state::member_info::AuthorizedMemberInfo::new_with_member_key(
                bob_member_info, &bob_signing_key
            );

            bob_room_state.member_info.member_info.push(authorized_bob_info.clone());

            let bob_membership_delta = river_core::room_state::ChatRoomStateV1Delta {
                members: Some(river_core::room_state::member::MembersDelta::new(vec![authorized_bob_member.clone()])),
                ..Default::default()
            };

            println!("   - Sending Bob's membership delta to network...");
            update_room_state_delta(&mut _bob_client, contract_key, bob_membership_delta).await
                .map_err(|e| format!("Failed to update Bob's membership: {}", e))?;

            println!("   - Waiting for membership update response...");
            wait_for_update_response(&mut _bob_client, &contract_key).await
                .map_err(|e| format!("Bob membership update response failed: {}", e))?;

            println!("   - Bob membership update successful!");

            println!("   - Sending Bob's member info (nickname)...");
            let bob_info_delta = river_core::room_state::ChatRoomStateV1Delta {
                member_info: Some(vec![authorized_bob_info.clone()]),
                ..Default::default()
            };

            update_room_state_delta(&mut _bob_client, contract_key, bob_info_delta).await
                .map_err(|e| format!("Failed to update Bob's member info: {}", e))?;

            wait_for_update_response(&mut _bob_client, &contract_key).await
                .map_err(|e| format!("Bob member info update response failed: {}", e))?;

            println!("   - Bob member info update successful!");

            println!("   - Bob successfully joined the room");
            tokio::time::sleep(Duration::from_secs(3)).await;

            println!("\n Step 5: Testing message propagation between Alice and Bob");

            println!("   - Alice sends message: 'Hello Bob!'");
            let mut alice_clients = vec![&mut client_node1];
            let alice_room_states = get_all_room_states(&mut alice_clients, contract_key).await?;

            send_test_message(&mut client_node1, contract_key, &alice_room_states[0], &initial_state.parameters,
                "Hello Bob!".to_string(), &alice_signing_key).await
                .map_err(|e| format!("Alice failed to send message: {}", e))?;
            wait_for_update_response(&mut client_node1, &contract_key).await?;

            tokio::time::sleep(Duration::from_secs(2)).await;

            println!("   - Bob sends message: 'Hello Alice!'");
            let mut bob_clients = vec![&mut _bob_client];
            let bob_room_states = get_all_room_states(&mut bob_clients, contract_key).await?;

            send_test_message(&mut _bob_client, contract_key, &bob_room_states[0], &initial_state.parameters,
                "Hello Alice!".to_string(), &bob_signing_key).await
                .map_err(|e| format!("Bob failed to send message: {}", e))?;
            wait_for_update_response(&mut _bob_client, &contract_key).await?;

            tokio::time::sleep(Duration::from_secs(3)).await;

            println!("\n Step 6: Verifying message propagation (Issue #1775 test)");

            let mut all_clients = vec![&mut client_node1, &mut _bob_client];
            let final_states = get_all_room_states(&mut all_clients, contract_key).await?;
            let (alice_final_state, bob_final_state) = (&final_states[0], &final_states[1]);

            println!("   - Alice sees {} messages", alice_final_state.recent_messages.messages.len());
            println!("   - Bob sees {} messages", bob_final_state.recent_messages.messages.len());

            for (i, msg) in alice_final_state.recent_messages.messages.iter().enumerate() {
                println!("     Alice msg {}: '{}'", i+1, msg.message.content);
            }

            for (i, msg) in bob_final_state.recent_messages.messages.iter().enumerate() {
                println!("     Bob msg {}: '{}'", i+1, msg.message.content);
            }

            if alice_final_state.recent_messages.messages.len() != bob_final_state.recent_messages.messages.len() {
                println!("   Alice has {} messages, Bob has {} messages",
                    alice_final_state.recent_messages.messages.len(),
                    bob_final_state.recent_messages.messages.len());
                return Err("Inconsistent message propagation detected".into());
            }

            if !river_states_equal(alice_final_state, bob_final_state) {
                println!("State inconsistency between Alice and Bob!");
                return Err("Room state inconsistency detected".into());
            }

            println!("SUCCESS: Both Alice and Bob see all {} messages consistently!",
                alice_final_state.recent_messages.messages.len());
            println!("TEST PASSED - Message propagation works correctly!");

            Ok(())
        }).instrument(span!(Level::INFO, "test_invitation_message_propagation_network_test"));

        tokio::select! {
            result = gateway_node => {
                match result {
                    Ok(_) => Err("Gateway node exited unexpectedly".into()),
                    Err(e) => Err(format!("Gateway node failed: {}", e).into())
                }
            }
            result = alice_node => {
                match result {
                    Ok(_) => Err("Alice node exited unexpectedly".into()),
                    Err(e) => Err(format!("Alice node failed: {}", e).into())
                }
            }
            result = bob_node => {
                match result {
                    Ok(_) => Err("Bob node exited unexpectedly".into()),
                    Err(e) => Err(format!("Bob node failed: {}", e).into())
                }
            }
            result = charlie_node => {
                match result {
                    Ok(_) => Err("Charlie node exited unexpectedly".into()),
                    Err(e) => Err(format!("Charlie node failed: {}", e).into())
                }
            }
            result = network_test => {
                match result {
                    Ok(inner_result) => inner_result,
                    Err(_timeout_error) => Err("Invitation message propagation test timed out after 500 seconds".into())
                }
            }
        }
    }
    .instrument(span)
    .await
}
