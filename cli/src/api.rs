use crate::config::Config;
use crate::output::OutputFormat;
use crate::storage::Storage;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
};
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion,
    Parameters, UpdateData, WrappedContract, WrappedState,
};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::privacy::{RoomDisplayMetadata, SealedBytes};
use river_core::room_state::ChatRoomStateV1Delta;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tracing::{debug, info};

// Load the room contract WASM copied by build.rs
const ROOM_CONTRACT_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/room_contract.wasm"));

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Invitation {
    pub room: VerifyingKey,
    pub invitee_signing_key: SigningKey,
    pub invitee: AuthorizedMember,
}

pub struct ApiClient {
    web_api: Arc<Mutex<WebApi>>,
    #[allow(dead_code)]
    config: Config,
    storage: Storage,
}

impl ApiClient {
    pub async fn new(node_url: &str, config: Config, config_dir: Option<&str>) -> Result<Self> {
        // Use the URL as provided - it should already be in the correct format
        info!("Connecting to Freenet node at: {}", node_url);

        // Connect using tokio-tungstenite
        let (ws_stream, _) = connect_async(node_url)
            .await
            .map_err(|e| anyhow!("Failed to connect to WebSocket: {}", e))?;

        info!("WebSocket connected successfully");

        // Create WebApi instance
        let web_api = WebApi::start(ws_stream);

        let storage = Storage::new(config_dir)?;

        Ok(Self {
            web_api: Arc::new(Mutex::new(web_api)),
            config,
            storage,
        })
    }

    pub async fn create_room(
        &self,
        name: String,
        nickname: String,
    ) -> Result<(VerifyingKey, ContractKey)> {
        info!("Creating room: {}", name);

        // Generate signing key for the room owner
        let signing_key =
            SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()));
        let owner_vk = signing_key.verifying_key();

        // Create initial room state
        let mut room_state = ChatRoomStateV1::default();

        // Set initial configuration
        let config = Configuration {
            owner_member_id: owner_vk.into(),
            display: RoomDisplayMetadata {
                name: SealedBytes::public(name.clone().into_bytes()),
                description: None,
            },
            ..Configuration::default()
        };
        room_state.configuration = AuthorizedConfigurationV1::new(config, &signing_key);

        // Add owner to member_info
        let owner_info = MemberInfo {
            member_id: owner_vk.into(),
            version: 0,
            preferred_nickname: SealedBytes::public(nickname.into_bytes()),
        };
        let authorized_owner_info = AuthorizedMemberInfo::new(owner_info, &signing_key);
        room_state
            .member_info
            .member_info
            .push(authorized_owner_info);

        // Generate contract key using ciborium for serialization (matching UI code)
        let parameters = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize parameters: {}", e))?;
            buf
        };

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let instance_id = ContractInstanceId::from_params_and_code(
            Parameters::from(params_bytes.clone()),
            contract_code.clone(),
        );
        let contract_key = ContractKey::from(instance_id);

        // Create contract container
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));

        // Create wrapped state using ciborium
        let state_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&room_state, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize room state: {}", e))?;
            buf
        };
        let wrapped_state = WrappedState::new(state_bytes);

        // Create PUT request
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(put_request);

        // Send request
        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send PUT request: {}", e))?;

        // Wait for response with a more generous timeout to handle network delays
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(10), web_api.recv()).await {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for PUT response after 10 seconds")),
            };

        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::PutResponse { key } => {
                        info!("Room created successfully with contract key: {}", key.id());

                        // Verify the key matches what we expected
                        if key != contract_key {
                            return Err(anyhow!(
                                "Contract key mismatch: expected {}, got {}",
                                contract_key.id(),
                                key.id()
                            ));
                        }

                        // Store room info persistently
                        self.storage.add_room(
                            &owner_vk,
                            &signing_key,
                            room_state,
                            &contract_key,
                        )?;

                        // Note: Subscription removed as riverctl is a one-shot CLI tool
                        // that exits immediately. Subscription will be re-added when
                        // streaming functionality is implemented.

                        Ok((owner_vk, contract_key))
                    }
                    _ => Err(anyhow!("Unexpected contract response type for PUT request")),
                }
            }
            HostResponse::Ok => {
                // Some versions might return Ok for successful operations
                info!(
                    "Room created (Ok response) with contract key: {}",
                    contract_key.id()
                );

                // Store room info persistently
                self.storage
                    .add_room(&owner_vk, &signing_key, room_state, &contract_key)?;

                // Note: Subscription removed as riverctl is a one-shot CLI tool
                // that exits immediately. Subscription will be re-added when
                // streaming functionality is implemented.

                Ok((owner_vk, contract_key))
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub async fn get_room(
        &self,
        room_owner_key: &VerifyingKey,
        subscribe: bool,
    ) -> Result<ChatRoomStateV1> {
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);
        info!("Getting room state for contract: {}", contract_key.id());

        let get_request = ContractRequest::Get {
            key: contract_key,
            return_contract_code: true, // Request full contract to enable caching
            subscribe: false,           // Always false, we'll subscribe separately if needed
        };

        let client_request = ClientRequest::ContractOp(get_request);

        let mut web_api = self.web_api.lock().await;
        tracing::info!("ACCEPT: sending GET for contract {}", contract_key.id());
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send GET request: {}", e))?;

        let response = web_api
            .recv()
            .await
            .map_err(|e| anyhow!("Failed to receive response: {}", e))?;

        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse { state, .. } => {
                        // Deserialize the state properly
                        let room_state: ChatRoomStateV1 = ciborium::de::from_reader(&state[..])
                            .map_err(|e| anyhow!("Failed to deserialize room state: {}", e))?;
                        info!(
                            "Successfully retrieved room state with {} messages",
                            room_state.recent_messages.messages.len()
                        );

                        // Drop the lock before subscribing
                        drop(web_api);

                        // If subscribe was requested, do it separately
                        if subscribe {
                            info!("Subscribing to contract to receive updates");
                            let subscribe_request = ContractRequest::Subscribe {
                                key: contract_key,
                                summary: None,
                            };

                            let subscribe_client_request =
                                ClientRequest::ContractOp(subscribe_request);

                            let mut web_api = self.web_api.lock().await;
                            web_api
                                .send(subscribe_client_request)
                                .await
                                .map_err(|e| anyhow!("Failed to send SUBSCRIBE request: {}", e))?;

                            // Wait for subscription response
                            let subscribe_response = match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                web_api.recv(),
                            )
                            .await
                            {
                                Ok(result) => result.map_err(|e| {
                                    anyhow!("Failed to receive subscription response: {}", e)
                                })?,
                                Err(_) => {
                                    return Err(anyhow!(
                                        "Timeout waiting for SUBSCRIBE response after 5 seconds"
                                    ))
                                }
                            };

                            match subscribe_response {
                                HostResponse::ContractResponse(
                                    ContractResponse::SubscribeResponse { subscribed, .. },
                                ) => {
                                    if subscribed {
                                        info!("Successfully subscribed to contract");
                                    } else {
                                        return Err(anyhow!("Failed to subscribe to contract"));
                                    }
                                }
                                _ => {
                                    return Err(anyhow!("Unexpected response to SUBSCRIBE request"))
                                }
                            }
                        }

                        Ok(room_state)
                    }
                    _ => Err(anyhow!("Unexpected contract response type")),
                }
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub async fn test_connection(&self) -> Result<()> {
        info!("Testing WebSocket connection...");

        // Send a simple disconnect request to test the connection
        let test_request = ClientRequest::Disconnect { cause: None };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(test_request)
            .await
            .map_err(|e| anyhow!("Failed to send test request: {}", e))?;

        info!("Connection test successful");
        Ok(())
    }

    pub async fn create_invitation(&self, room_owner_key: &VerifyingKey) -> Result<String> {
        info!(
            "Creating invitation for room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the room info from persistent storage
        let room_data = self.storage.get_room(room_owner_key)?
            .ok_or_else(|| anyhow!("Room not found in local storage. You must be the room owner to create invitations."))?;
        let (signing_key, _state, _contract_key) = room_data;

        // Generate a new signing key for the invitee
        let invitee_signing_key =
            SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()));
        let invitee_vk = invitee_signing_key.verifying_key();

        // Create the member entry for the invitee
        let member = Member {
            owner_member_id: (*room_owner_key).into(),
            member_vk: invitee_vk,
            invited_by: signing_key.verifying_key().into(),
        };

        // Sign the member entry with the inviter's key (room owner in this case)
        let authorized_member = AuthorizedMember::new(member, &signing_key);

        // Create the invitation struct
        let invitation = Invitation {
            room: *room_owner_key,
            invitee_signing_key,
            invitee: authorized_member,
        };

        // Encode as base58
        let mut data = Vec::new();
        ciborium::ser::into_writer(&invitation, &mut data)
            .map_err(|e| anyhow!("Failed to serialize invitation: {}", e))?;
        let encoded = bs58::encode(data).into_string();

        Ok(encoded)
    }

    pub async fn accept_invitation(
        &self,
        invitation_code: &str,
        nickname: &str,
    ) -> Result<(VerifyingKey, ContractKey)> {
        info!("Accepting invitation with nickname: {}", nickname);

        // Decode the invitation
        let decoded = bs58::decode(invitation_code)
            .into_vec()
            .map_err(|e| anyhow!("Failed to decode invitation: {}", e))?;
        let invitation: Invitation = ciborium::de::from_reader(&decoded[..])
            .map_err(|e| anyhow!("Failed to deserialize invitation: {}", e))?;

        let room_owner_vk = invitation.room;
        let contract_key = self.owner_vk_to_contract_key(&room_owner_vk);

        info!(
            "Invitation is for room owned by: {}",
            bs58::encode(room_owner_vk.as_bytes()).into_string()
        );
        info!("Contract key: {}", contract_key.id());

        // Perform a GET request to fetch the room state
        let get_request = ContractRequest::Get {
            key: contract_key,
            return_contract_code: true, // Request full contract to enable caching
            subscribe: false,           // We'll subscribe separately after GET succeeds
        };

        let client_request = ClientRequest::ContractOp(get_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send GET request: {}", e))?;

        // Wait for response with timeout
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(10), web_api.recv()).await {
                Ok(result) => {
                    tracing::info!("ACCEPT: received GET response");
                    result.map_err(|e| anyhow!("Failed to receive response: {}", e))?
                }
                Err(_) => return Err(anyhow!("Timeout waiting for GET response after 10 seconds")),
            };

        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse { state, .. } => {
                        info!("Successfully retrieved room state");

                        // Parse the actual room state from the response
                        let mut room_state: ChatRoomStateV1 = ciborium::de::from_reader(&state[..])
                            .map_err(|e| anyhow!("Failed to deserialize room state: {}", e))?;

                        info!(
                            "Room state retrieved: name={}, members={}, messages={}",
                            room_state.configuration.configuration.display.name.to_string_lossy(),
                            room_state.members.members.len(),
                            room_state.recent_messages.messages.len()
                        );

                        // Apply the invitation's member data to add this user to the members list
                        let members_delta =
                            river_core::room_state::member::MembersDelta::new(vec![invitation
                                .invitee
                                .clone()]);

                        // Create parameters for applying delta
                        let parameters = ChatRoomParametersV1 {
                            owner: room_owner_vk,
                        };

                        // Apply the member delta to add ourselves to the room
                        room_state
                            .members
                            .apply_delta(&room_state.clone(), &parameters, &Some(members_delta))
                            .map_err(|e| anyhow!("Failed to add member to room: {}", e))?;

                        info!(
                            "Added self to members list, total members: {}",
                            room_state.members.members.len()
                        );

                        // Create member info entry with nickname
                        let member_info = MemberInfo {
                            member_id: invitation.invitee_signing_key.verifying_key().into(),
                            version: 0,
                            preferred_nickname: SealedBytes::public(nickname.to_string().into_bytes()),
                        };
                        let authorized_member_info =
                            AuthorizedMemberInfo::new(member_info, &invitation.invitee_signing_key);

                        // Add the member info to the room state
                        room_state
                            .member_info
                            .member_info
                            .push(authorized_member_info.clone());

                        info!("Added member info with nickname: {}", nickname);

                        // Validate the room state is properly initialized
                        let self_member_id = invitation.invitee_signing_key.verifying_key().into();

                        // Check owner_member_id is set correctly
                        if room_state.configuration.configuration.owner_member_id
                            == river_core::room_state::member::MemberId(
                                freenet_scaffold::util::FastHash(0),
                            )
                        {
                            return Err(anyhow!("Room state has invalid owner_member_id"));
                        }

                        // Check we're in the members list
                        let is_member = room_state.members.members.iter().any(|m| {
                            m.member.member_vk == invitation.invitee_signing_key.verifying_key()
                        });
                        if !is_member {
                            return Err(anyhow!("Failed to add self to members list"));
                        }

                        // Check we have member info
                        let has_member_info = room_state
                            .member_info
                            .member_info
                            .iter()
                            .any(|info| info.member_info.member_id == self_member_id);
                        if !has_member_info {
                            return Err(anyhow!("Failed to add member info"));
                        }

                        info!("Validation passed: owner_member_id={:?}, is_member={}, has_member_info={}", 
                              room_state.configuration.configuration.owner_member_id, is_member, has_member_info);

                        // Store the properly initialized room state locally
                        self.storage.add_room(
                            &room_owner_vk,
                            &invitation.invitee_signing_key,
                            room_state,
                            &contract_key,
                        )?;

                        // Drop the original lock before update
                        drop(web_api);

                        // Note: Subscription removed as riverctl is a one-shot CLI tool
                        // that exits immediately. Subscription will be re-added when
                        // streaming functionality is implemented.

                        // Publish membership and member info to the network (non-blocking)
                        // Build a delta containing the authorized member and member info we just applied
                        let membership_delta = ChatRoomStateV1Delta {
                            members: Some(river_core::room_state::member::MembersDelta::new(vec![
                                invitation.invitee.clone(),
                            ])),
                            member_info: Some(vec![authorized_member_info.clone()]),
                            ..Default::default()
                        };
                        // Serialize delta
                        let delta_bytes = {
                            let mut buf = Vec::new();
                            ciborium::ser::into_writer(&membership_delta, &mut buf).map_err(
                                |e| anyhow!("Failed to serialize membership delta: {}", e),
                            )?;
                            buf
                        };
                        let mut web_api = self.web_api.lock().await;
                        let update_request = ContractRequest::Update {
                            key: contract_key,
                            data: UpdateData::Delta(delta_bytes.into()),
                        };
                        let update_client_request = ClientRequest::ContractOp(update_request);
                        tracing::info!(
                            "ACCEPT: sending membership UPDATE for contract {}",
                            contract_key.id()
                        );
                        web_api
                            .send(update_client_request)
                            .await
                            .map_err(|e| anyhow!("Failed to send membership update: {}", e))?;
                        // Try to receive an ack briefly, but don't fail if none arrives quickly
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            web_api.recv(),
                        )
                        .await
                        {
                            Ok(Ok(HostResponse::ContractResponse(
                                ContractResponse::UpdateResponse { .. },
                            ))) => {
                                tracing::info!("ACCEPT: received UPDATE ack");
                                info!("Membership published to network");
                            }
                            _ => {
                                tracing::info!("ACCEPT: no immediate UPDATE ack");
                                info!("Membership update sent (no immediate ack)");
                            }
                        }
                        drop(web_api);

                        Ok((room_owner_vk, contract_key))
                    }
                    _ => Err(anyhow!("Unexpected contract response type")),
                }
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub fn owner_vk_to_contract_key(&self, owner_vk: &VerifyingKey) -> ContractKey {
        let parameters = ChatRoomParametersV1 { owner: *owner_vk };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .expect("Serialization should not fail");
            buf
        };
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let instance_id =
            ContractInstanceId::from_params_and_code(Parameters::from(params_bytes), contract_code);
        ContractKey::from(instance_id)
    }

    pub async fn list_rooms(&self) -> Result<Vec<(String, String, String)>> {
        self.storage.list_rooms().map(|rooms| {
            rooms
                .into_iter()
                .map(|(owner_vk, name, contract_key)| {
                    (
                        bs58::encode(owner_vk.as_bytes()).into_string(),
                        name,
                        contract_key,
                    )
                })
                .collect()
        })
    }

    pub async fn send_message(
        &self,
        room_owner_key: &VerifyingKey,
        message_content: String,
    ) -> Result<()> {
        info!(
            "Sending message to room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the room info from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to send messages.")
        })?;
        let (signing_key, mut room_state, _contract_key_str) = room_data;

        // Create the message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: river_core::room_state::member::MemberId::from(*room_owner_key),
            author: river_core::room_state::member::MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::public(message_content),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Create a delta with the new message
        let delta = river_core::room_state::ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message.clone()]),
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply message delta: {:?}", e))?;

        // Update the stored state
        self.storage
            .update_room_state(room_owner_key, room_state.clone())?;

        // Send the delta to the network
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(10), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => return Err(anyhow!("Timeout waiting for update response")),
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Message sent successfully to contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Stream messages from a room by polling for updates
    pub async fn stream_messages(
        &self,
        room_owner_key: &VerifyingKey,
        poll_interval_ms: u64,
        timeout_secs: u64,
        max_messages: usize,
        initial_messages: usize,
        format: OutputFormat,
    ) -> Result<()> {
        // Get the contract key for the room
        let room = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found in local storage. You may need to create or join it first.")
        })?;

        let (_signing_key, _room_state, contract_key_str) = room;
        let _contract_key = contract_key_str.clone();

        // Print header for human format
        if matches!(format, OutputFormat::Human) {
            eprintln!(
                "Streaming messages from room {} (press Ctrl+C to stop)...\n",
                bs58::encode(room_owner_key.as_bytes()).into_string()
            );
        }

        // Track seen message IDs to avoid duplicates
        let mut seen_messages = HashSet::new();
        let mut new_message_count = 0;
        let start_time = std::time::Instant::now();

        // Show initial messages if requested
        if initial_messages > 0 {
            let room_state = self.get_room(room_owner_key, false).await?;
            let messages = &room_state.recent_messages.messages;

            let initial_msgs: Vec<_> = messages.iter().rev().take(initial_messages).rev().collect();

            for msg in &initial_msgs {
                // Generate a unique ID for this message
                let msg_id = format!("{:?}:{:?}", msg.message.author, msg.message.time);
                seen_messages.insert(msg_id);

                Self::output_message(&room_state, msg, room_owner_key, &format)?;
            }
        }

        // Set up Ctrl+C handler
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);

        // Spawn task to handle Ctrl+C
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let _ = shutdown_tx.send(()).await;
        });

        // Main polling loop
        loop {
            // Check for shutdown signal
            if shutdown_rx.try_recv().is_ok() {
                if matches!(format, OutputFormat::Human) {
                    eprintln!("\nStopped monitoring.");
                }
                return Ok(());
            }

            // Check timeout
            if timeout_secs > 0 && start_time.elapsed().as_secs() >= timeout_secs {
                debug!("Timeout reached, exiting stream");
                return Ok(());
            }

            // Check max messages
            if max_messages > 0 && new_message_count >= max_messages {
                debug!("Maximum message count reached, exiting stream");
                return Ok(());
            }

            // Poll for new messages
            match self.get_room(room_owner_key, false).await {
                Ok(room_state) => {
                    let messages = &room_state.recent_messages.messages;

                    for msg in messages {
                        // Generate a unique ID for this message
                        let msg_id = format!("{:?}:{:?}", msg.message.author, msg.message.time);

                        // Only show if we haven't seen it before
                        if seen_messages.insert(msg_id.clone()) {
                            Self::output_message(&room_state, msg, room_owner_key, &format)?;
                            new_message_count += 1;

                            // Check max messages after each new message
                            if max_messages > 0 && new_message_count >= max_messages {
                                return Ok(());
                            }
                        }
                    }
                }
                Err(e) => {
                    // Log error but continue polling
                    debug!("Error fetching room state: {}", e);
                }
            }

            // Wait for next poll interval
            tokio::time::sleep(std::time::Duration::from_millis(poll_interval_ms)).await;
        }
    }

    /// Helper function to output a message in the requested format
    fn output_message(
        room_state: &ChatRoomStateV1,
        msg: &river_core::room_state::message::AuthorizedMessageV1,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
    ) -> Result<()> {
        match format {
            OutputFormat::Human => {
                let author_str = msg.message.author.to_string();
                let author_short = author_str.chars().take(8).collect::<String>();

                // Get nickname if available
                let nickname = room_state
                    .member_info
                    .member_info
                    .iter()
                    .find(|info| info.member_info.member_id == msg.message.author)
                    .map(|info| info.member_info.preferred_nickname.to_string_lossy())
                    .unwrap_or(author_short);

                let datetime: DateTime<Utc> = msg.message.time.into();
                let local_time: DateTime<Local> = datetime.into();

                println!(
                    "[{} - {}]: {}",
                    local_time.format("%H:%M:%S"),
                    nickname,
                    msg.message.content
                );
            }
            OutputFormat::Json => {
                let author_str = msg.message.author.to_string();

                let nickname = room_state
                    .member_info
                    .member_info
                    .iter()
                    .find(|info| info.member_info.member_id == msg.message.author)
                    .map(|info| info.member_info.preferred_nickname.clone());

                let datetime: DateTime<Utc> = msg.message.time.into();

                // Output as JSONL (one JSON object per line)
                let json_msg = json!({
                    "type": "message",
                    "room": bs58::encode(room_owner_key.as_bytes()).into_string(),
                    "author": author_str,
                    "nickname": nickname,
                    "content": msg.message.content,
                    "timestamp": datetime.to_rfc3339(),
                });

                println!("{}", serde_json::to_string(&json_msg)?);
            }
        }

        // Flush stdout immediately for real-time output
        std::io::stdout().flush()?;
        Ok(())
    }
}
