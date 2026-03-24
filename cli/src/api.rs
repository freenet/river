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
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
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
use tracing::{debug, info, warn};

// Load the room contract WASM copied by build.rs
const ROOM_CONTRACT_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/room_contract.wasm"));

/// Compute the contract key for a room from its owner verifying key.
/// This uses the current bundled WASM to ensure consistency.
pub fn compute_contract_key(owner_vk: &VerifyingKey) -> ContractKey {
    let params = ChatRoomParametersV1 { owner: *owner_vk };
    let params_bytes = {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&params, &mut buf).expect("Failed to serialize parameters");
        buf
    };
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code)
}

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
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

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
        // Use the full ContractKey constructor that includes the code hash
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(params_bytes.clone()),
            &contract_code,
        );

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

        // Create PUT request - subscribe: true so we receive updates to our own room
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
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
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for PUT response after 60 seconds")),
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

                        Ok((owner_vk, contract_key))
                    }
                    ContractResponse::UpdateNotification { key, .. } => {
                        // When subscribing on PUT, we may receive an UpdateNotification first
                        // This indicates the PUT succeeded and we're now subscribed
                        info!(
                            "Room created (received subscription update) with contract key: {}",
                            key.id()
                        );

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

                        Ok((owner_vk, contract_key))
                    }
                    other => Err(anyhow!(
                        "Unexpected contract response type for PUT request: {:?}",
                        other
                    )),
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

                Ok((owner_vk, contract_key))
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Republish a room contract to the network
    ///
    /// This re-PUTs the contract with its current state, making this node seed it again.
    /// Use this when the contract exists locally but isn't being served on the network.
    pub async fn republish_room(&self, room_owner_key: &VerifyingKey) -> Result<()> {
        info!(
            "Republishing room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the room state from local storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found in local storage. Cannot republish without local state.")
        })?;
        let (_signing_key, room_state, _contract_key_str) = room_data;

        // Create parameters
        let parameters = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize parameters: {}", e))?;
            buf
        };

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(params_bytes.clone()),
            &contract_code,
        );

        // Create contract container
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));

        // Serialize state
        let state_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&room_state, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize room state: {}", e))?;
            buf
        };
        let wrapped_state = WrappedState::new(state_bytes);

        // Create PUT request with subscribe=true to start seeding
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(put_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send PUT request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for PUT response after 60 seconds")),
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::PutResponse { key }) => {
                info!(
                    "Room republished successfully with contract key: {}",
                    key.id()
                );
                if key != contract_key {
                    return Err(anyhow!(
                        "Contract key mismatch: expected {}, got {}",
                        contract_key.id(),
                        key.id()
                    ));
                }
                Ok(())
            }
            HostResponse::Ok => {
                info!("Room republished successfully (Ok response)");
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub async fn get_room(
        &self,
        room_owner_key: &VerifyingKey,
        subscribe: bool,
    ) -> Result<ChatRoomStateV1> {
        // Ensure room is migrated to the current contract version before any GET.
        // This handles the case where bundled WASM changed (e.g., after a release)
        // and no other client has migrated the state to the new contract key yet.
        let contract_key = self.ensure_room_migrated(room_owner_key).await?;
        info!("Getting room state for contract: {}", contract_key.id());

        let get_request = ContractRequest::Get {
            key: *contract_key.id(),    // GET uses ContractInstanceId
            return_contract_code: true, // Request full contract to enable caching
            subscribe: false,           // Always false, we'll subscribe separately if needed
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(get_request);

        let mut web_api = self.web_api.lock().await;
        tracing::info!("ACCEPT: sending GET for contract {}", contract_key.id());
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send GET request: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(30), web_api.recv()).await {
                Ok(result) => {
                    result.map_err(|e| anyhow!("Failed to receive GET response: {}", e))?
                }
                Err(_) => return Err(anyhow!("Timeout waiting for GET response after 30 seconds")),
            };

        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse { state, .. } => {
                        // Deserialize the state properly
                        let mut room_state: ChatRoomStateV1 = ciborium::de::from_reader(&state[..])
                            .map_err(|e| anyhow!("Failed to deserialize room state: {}", e))?;

                        // Rebuild actions state (edits, deletes, reactions) from message content
                        room_state.recent_messages.rebuild_actions_state();

                        info!(
                            "Successfully retrieved room state with {} messages",
                            room_state.recent_messages.messages.len()
                        );

                        // Follow upgrade pointer (single hop) if present
                        if let Some(ref authorized_upgrade) = room_state.upgrade.0 {
                            let new_address = authorized_upgrade.upgrade.new_chatroom_address;
                            let current_id_bytes = contract_key.id().as_bytes();
                            let mut current_hash = [0u8; 32];
                            current_hash.copy_from_slice(current_id_bytes);

                            if blake3::Hash::from(current_hash) != new_address {
                                info!("Following upgrade pointer to new contract: {}", new_address);
                                // Drop the lock before recursive call
                                drop(web_api);

                                // Follow the pointer by constructing a new contract ID
                                let new_id = ContractInstanceId::new(*new_address.as_bytes());
                                let follow_request = ContractRequest::Get {
                                    key: new_id,
                                    return_contract_code: true,
                                    subscribe: false,
                                    blocking_subscribe: false,
                                };

                                let mut web_api = self.web_api.lock().await;
                                web_api
                                    .send(ClientRequest::ContractOp(follow_request))
                                    .await
                                    .map_err(|e| {
                                        anyhow!("Failed to follow upgrade pointer: {}", e)
                                    })?;

                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(15),
                                    web_api.recv(),
                                )
                                .await
                                {
                                    Ok(Ok(HostResponse::ContractResponse(
                                        ContractResponse::GetResponse {
                                            state: new_state, ..
                                        },
                                    ))) => {
                                        let mut followed_state: ChatRoomStateV1 =
                                            ciborium::de::from_reader(&new_state[..]).map_err(
                                                |e| {
                                                    anyhow!(
                                                        "Failed to deserialize followed state: {}",
                                                        e
                                                    )
                                                },
                                            )?;
                                        followed_state.recent_messages.rebuild_actions_state();
                                        info!("Successfully followed upgrade pointer");

                                        // Subscribe to the new contract if requested
                                        if subscribe {
                                            let new_id =
                                                ContractInstanceId::new(*new_address.as_bytes());
                                            let subscribe_request = ContractRequest::Subscribe {
                                                key: new_id,
                                                summary: None,
                                            };
                                            let mut web_api = self.web_api.lock().await;
                                            if let Err(e) = web_api
                                                .send(ClientRequest::ContractOp(subscribe_request))
                                                .await
                                            {
                                                warn!(
                                                    "Failed to subscribe to followed contract: {}",
                                                    e
                                                );
                                            }
                                        }

                                        return Ok(followed_state);
                                    }
                                    _ => {
                                        info!(
                                            "Could not follow upgrade pointer, using current state"
                                        );
                                    }
                                }
                                return Ok(room_state);
                            }
                        }

                        // Drop the lock before subscribing
                        drop(web_api);

                        // If subscribe was requested, do it separately
                        if subscribe {
                            info!("Subscribing to contract to receive updates");
                            let subscribe_request = ContractRequest::Subscribe {
                                key: *contract_key.id(), // Subscribe uses ContractInstanceId
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
            key: *contract_key.id(),    // GET uses ContractInstanceId
            return_contract_code: true, // Request full contract to enable caching
            subscribe: false,           // We'll subscribe separately after GET succeeds
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(get_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send GET request: {}", e))?;

        // Wait for response with timeout
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(result) => {
                    tracing::info!("ACCEPT: received GET response");
                    result.map_err(|e| anyhow!("Failed to receive response: {}", e))?
                }
                Err(_) => return Err(anyhow!("Timeout waiting for GET response after 60 seconds")),
            };

        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse { state, .. } => {
                        info!("Successfully retrieved room state");

                        // Parse the actual room state from the response
                        let room_state: ChatRoomStateV1 = ciborium::de::from_reader(&state[..])
                            .map_err(|e| anyhow!("Failed to deserialize room state: {}", e))?;

                        info!(
                            "Room state retrieved: name={}, members={}, messages={}",
                            room_state
                                .configuration
                                .configuration
                                .display
                                .name
                                .to_string_lossy(),
                            room_state.members.members.len(),
                            room_state.recent_messages.messages.len()
                        );

                        // Validate the room state is properly initialized
                        if room_state.configuration.configuration.owner_member_id
                            == river_core::room_state::member::MemberId(
                                freenet_scaffold::util::FastHash(0),
                            )
                        {
                            return Err(anyhow!("Room state has invalid owner_member_id"));
                        }

                        // Compute invite chain before storing (walks up from invitee
                        // to owner through existing members — doesn't require the
                        // invitee to be in the members list)
                        let params = ChatRoomParametersV1 {
                            owner: room_owner_vk,
                        };
                        let invite_chain = room_state
                            .members
                            .get_invite_chain(&invitation.invitee, &params)
                            .unwrap_or_default();

                        // Store credentials locally first
                        self.storage.add_room(
                            &room_owner_vk,
                            &invitation.invitee_signing_key,
                            room_state.clone(),
                            &contract_key,
                        )?;

                        self.storage.store_authorized_member(
                            &room_owner_vk,
                            &invitation.invitee,
                            &invite_chain,
                        )?;

                        // Immediately publish membership + join event atomically.
                        // The join event counts as a message, preventing
                        // post_apply_cleanup from pruning the new member.
                        let signing_key = &invitation.invitee_signing_key;
                        let self_id = MemberId::from(&signing_key.verifying_key());

                        // Build members delta: invitee + any missing invite chain members
                        let current_member_ids: HashSet<MemberId> = room_state
                            .members
                            .members
                            .iter()
                            .map(|m| m.member.id())
                            .collect();
                        let mut members_to_add = vec![invitation.invitee.clone()];
                        for chain_member in &invite_chain {
                            if !current_member_ids.contains(&chain_member.member.id()) {
                                members_to_add.push(chain_member.clone());
                            }
                        }
                        let members_delta = MembersDelta::new(members_to_add);

                        // Build member_info delta with the provided nickname
                        let member_info = river_core::room_state::member_info::MemberInfo {
                            member_id: self_id,
                            version: 0,
                            preferred_nickname:
                                river_core::room_state::privacy::SealedBytes::public(
                                    nickname.as_bytes().to_vec(),
                                ),
                        };
                        let authorized_info =
                            river_core::room_state::member_info::AuthorizedMemberInfo::new_with_member_key(
                                member_info, signing_key,
                            );

                        // Build join event message
                        let join_message = river_core::room_state::message::MessageV1 {
                            room_owner: params.owner_id(),
                            author: self_id,
                            content: river_core::room_state::message::RoomMessageBody::join_event(),
                            time: std::time::SystemTime::now(),
                        };
                        let auth_join_message =
                            river_core::room_state::message::AuthorizedMessageV1::new(
                                join_message,
                                signing_key,
                            );

                        let delta = ChatRoomStateV1Delta {
                            recent_messages: Some(vec![auth_join_message]),
                            members: Some(members_delta),
                            member_info: Some(vec![authorized_info]),
                            ..Default::default()
                        };

                        // Apply locally for validation
                        let mut local_state = room_state.clone();
                        local_state
                            .apply_delta(&room_state, &params, &Some(delta.clone()))
                            .map_err(|e| anyhow!("Failed to apply join delta: {:?}", e))?;

                        // Update stored state
                        self.storage
                            .update_room_state(&room_owner_vk, local_state)?;

                        // Send delta to network
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

                        web_api
                            .send(ClientRequest::ContractOp(update_request))
                            .await
                            .map_err(|e| anyhow!("Failed to send join delta: {}", e))?;

                        // Wait for update response
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(60),
                            web_api.recv(),
                        )
                        .await
                        {
                            Ok(Ok(HostResponse::ContractResponse(
                                ContractResponse::UpdateResponse { .. },
                            ))) => {
                                info!("Invitation accepted and membership published");
                            }
                            Ok(Ok(resp)) => {
                                tracing::warn!("Unexpected response after join delta: {:?}", resp);
                            }
                            Ok(Err(e)) => {
                                tracing::warn!("Error receiving join delta response: {}", e);
                            }
                            Err(_) => {
                                tracing::warn!("Timeout waiting for join delta response");
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
        // Use the full ContractKey constructor that includes the code hash
        ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code)
    }

    /// Check if a room needs migration to a new contract version and perform it if needed.
    ///
    /// This is called automatically when accessing a room. If the bundled contract WASM
    /// has changed (e.g., bug fixes), this will:
    /// 1. Detect the contract key mismatch
    /// 2. Try GET on the new contract (someone else may have migrated)
    /// 3. If no state on new key, try GET from old contract key (previous_contract_key)
    /// 4. PUT the state to the new contract
    /// 5. Send upgrade pointer on old contract (for old-client compat)
    /// 6. Update local storage
    ///
    /// Any member can perform this migration — not just the owner.
    ///
    /// Returns the current contract key (possibly updated).
    pub async fn ensure_room_migrated(&self, room_owner_key: &VerifyingKey) -> Result<ContractKey> {
        let expected_key = self.owner_vk_to_contract_key(room_owner_key);

        // Check if we have this room locally
        let storage = self.storage.load_rooms()?;
        let owner_key_str = bs58::encode(room_owner_key.as_bytes()).into_string();
        let room_info = match storage.rooms.get(&owner_key_str) {
            Some(info) => info,
            None => {
                // Room not in local storage, no migration needed
                return Ok(expected_key);
            }
        };

        let signing_key = SigningKey::from_bytes(&room_info.signing_key_bytes);
        let room_state = room_info.state.clone();
        let previous_contract_key_str = room_info.previous_contract_key.clone();

        // Check if migration is needed. load_rooms() already regenerates the
        // contract_key to match the current WASM and saves the old key in
        // previous_contract_key. If previous_contract_key is None, the room
        // is already on the current contract version.
        if previous_contract_key_str.is_none() {
            return Ok(expected_key);
        }

        // Safe to unwrap: we returned early above when previous_contract_key_str is None.
        let prev_key_str = previous_contract_key_str.as_deref().unwrap();
        let new_key_display = expected_key.id().to_string();
        info!(
            "Room contract version changed, migrating: {} -> {}",
            &prev_key_str[..12.min(prev_key_str.len())],
            &new_key_display[..12.min(new_key_display.len())]
        );

        // Try to GET from the new contract first - maybe someone else already migrated
        let get_request = ContractRequest::Get {
            key: *expected_key.id(),
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(get_request))
            .await
            .map_err(|e| anyhow!("Failed to check new contract: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(10), web_api.recv()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    // Timeout - assume contract doesn't exist yet, we need to migrate
                    drop(web_api);
                    let state_to_migrate = self
                        .get_migration_state(
                            room_owner_key,
                            &room_state,
                            &previous_contract_key_str,
                        )
                        .await?;
                    let result = self
                        .migrate_room_to_new_contract(
                            room_owner_key,
                            &signing_key,
                            &state_to_migrate,
                            expected_key,
                        )
                        .await?;
                    // Send upgrade pointer on old contract
                    self.send_upgrade_pointer(
                        room_owner_key,
                        &signing_key,
                        &previous_contract_key_str,
                        &result,
                    )
                    .await;
                    self.clear_previous_contract_key(room_owner_key)?;
                    return Ok(result);
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::GetResponse { .. }) => {
                // New contract already exists, just update our local storage
                info!("New contract already exists, updating local reference");
                self.storage
                    .update_contract_key(room_owner_key, &expected_key)?;
                self.clear_previous_contract_key(room_owner_key)?;
                Ok(expected_key)
            }
            _ => {
                // Contract doesn't exist, try to get state from old contract and migrate
                drop(web_api);
                let state_to_migrate = self
                    .get_migration_state(room_owner_key, &room_state, &previous_contract_key_str)
                    .await?;
                let result = self
                    .migrate_room_to_new_contract(
                        room_owner_key,
                        &signing_key,
                        &state_to_migrate,
                        expected_key,
                    )
                    .await?;
                // Send upgrade pointer on old contract
                self.send_upgrade_pointer(
                    room_owner_key,
                    &signing_key,
                    &previous_contract_key_str,
                    &result,
                )
                .await;
                self.clear_previous_contract_key(room_owner_key)?;
                Ok(result)
            }
        }
    }

    /// Try to get the latest state for migration. First tries the old contract key
    /// (previous_contract_key), falls back to local cached state.
    async fn get_migration_state(
        &self,
        room_owner_key: &VerifyingKey,
        local_state: &ChatRoomStateV1,
        previous_contract_key_str: &Option<String>,
    ) -> Result<ChatRoomStateV1> {
        // If we have a previous contract key, try to GET from it for fresher state
        if let Some(prev_key_str) = previous_contract_key_str {
            info!(
                "Trying GET from old contract {} for migration",
                prev_key_str
            );
            let prev_id: ContractInstanceId = prev_key_str
                .parse()
                .map_err(|e| anyhow!("Invalid previous contract key: {}", e))?;

            let get_request = ContractRequest::Get {
                key: prev_id,
                return_contract_code: false,
                subscribe: false,
                blocking_subscribe: false,
            };

            let mut web_api = self.web_api.lock().await;
            if let Ok(()) = web_api
                .send(ClientRequest::ContractOp(get_request))
                .await
                .map_err(|e| anyhow!("Failed to GET old contract: {}", e))
            {
                match tokio::time::timeout(std::time::Duration::from_secs(10), web_api.recv()).await
                {
                    Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                        state,
                        ..
                    }))) => {
                        if let Ok(mut old_state) =
                            ciborium::de::from_reader::<ChatRoomStateV1, _>(&state[..])
                        {
                            info!("Got state from old contract, using for migration");
                            old_state.recent_messages.rebuild_actions_state();
                            // Merge with local state to get the best of both
                            let mut merged = old_state.clone();
                            let params = ChatRoomParametersV1 {
                                owner: *room_owner_key,
                            };
                            if let Err(e) = merged.merge(&old_state, &params, local_state) {
                                info!(
                                    "Merge with local state failed ({}), using old contract state",
                                    e
                                );
                                return Ok(old_state);
                            }
                            return Ok(merged);
                        }
                    }
                    _ => {
                        info!("Could not GET from old contract, using local cached state");
                    }
                }
            }
        }
        Ok(local_state.clone())
    }

    /// Send an upgrade pointer to the old contract key for old-client compatibility.
    /// Note: The CLI cannot send upgrade pointers because it only stores the contract
    /// instance ID (not the full ContractKey with code hash). The UI handles upgrade
    /// pointer sending since it has the full ContractKey from the in-memory migration.
    async fn send_upgrade_pointer(
        &self,
        _room_owner_key: &VerifyingKey,
        _signing_key: &SigningKey,
        _previous_contract_key_str: &Option<String>,
        _new_contract_key: &ContractKey,
    ) {
        // Upgrade pointer sending requires a full ContractKey (with code hash),
        // but CLI storage only preserves the contract instance ID string.
        // The UI handles this since it captures the full ContractKey before regeneration.
        // The critical migration path (GET old → PUT new) works without this.
    }

    /// Clear the previous_contract_key after successful migration.
    fn clear_previous_contract_key(&self, owner_vk: &VerifyingKey) -> Result<()> {
        let mut storage = self.storage.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
            room_info.previous_contract_key = None;
            self.storage.save_rooms(&storage)?;
        }
        Ok(())
    }

    /// Migrate a room to a new contract by PUTting the state
    async fn migrate_room_to_new_contract(
        &self,
        room_owner_key: &VerifyingKey,
        _signing_key: &SigningKey, // Kept for potential future use (e.g., signing migration metadata)
        room_state: &ChatRoomStateV1,
        new_contract_key: ContractKey,
    ) -> Result<ContractKey> {
        info!("Migrating room to new contract: {}", new_contract_key.id());

        let parameters = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize parameters: {}", e))?;
            buf
        };

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));

        let state_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(room_state, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize room state: {}", e))?;
            buf
        };
        let wrapped_state = WrappedState::new(state_bytes);

        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
        };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(put_request))
            .await
            .map_err(|e| anyhow!("Failed to send PUT for migration: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive migration response: {}", e)),
                Err(_) => return Err(anyhow!("Timeout during room migration")),
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::PutResponse { key }) => {
                info!("Room migrated successfully to: {}", key.id());
                // Update local storage with new contract key
                self.storage.update_contract_key(room_owner_key, &key)?;
                Ok(key)
            }
            HostResponse::Ok => {
                info!("Room migrated successfully (Ok response)");
                self.storage
                    .update_contract_key(room_owner_key, &new_contract_key)?;
                Ok(new_contract_key)
            }
            _ => Err(anyhow!(
                "Unexpected response during migration: {:?}",
                response
            )),
        }
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

    /// Build a rejoin delta if the user has been pruned from the members list.
    /// Returns (members_delta, member_info_delta) if the user needs to re-add themselves.
    ///
    /// This serves as a fallback for the join event sent at invitation acceptance
    /// time — if the join event ages out of `recent_messages` and the member gets
    /// pruned before sending a regular message, this re-adds them on next send.
    fn build_rejoin_delta(
        &self,
        room_state: &ChatRoomStateV1,
        room_owner_key: &VerifyingKey,
        signing_key: &SigningKey,
    ) -> (Option<MembersDelta>, Option<Vec<AuthorizedMemberInfo>>) {
        let self_vk = signing_key.verifying_key();

        // Owner doesn't need to re-add
        if self_vk == *room_owner_key {
            return (None, None);
        }

        // Already in members list
        if room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == self_vk)
        {
            return (None, None);
        }

        // Try to get stored authorized member
        let storage = match self.storage.load_rooms() {
            Ok(s) => s,
            Err(_) => return (None, None),
        };
        let key_str = bs58::encode(room_owner_key.as_bytes()).into_string();
        let (authorized_member, invite_chain) = match storage.rooms.get(&key_str) {
            Some(info) => match &info.self_authorized_member {
                Some(am) => (am.clone(), info.invite_chain.clone()),
                None => return (None, None),
            },
            None => return (None, None),
        };

        // Build members delta - include self and any missing chain members
        let current_member_ids: HashSet<MemberId> = room_state
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();
        let mut members_to_add = vec![authorized_member.clone()];
        for chain_member in &invite_chain {
            if !current_member_ids.contains(&chain_member.member.id()) {
                members_to_add.push(chain_member.clone());
            }
        }

        // Build member_info delta
        let self_id = MemberId::from(&self_vk);
        let existing_version = room_state
            .member_info
            .member_info
            .iter()
            .find(|i| i.member_info.member_id == self_id)
            .map(|i| i.member_info.version)
            .unwrap_or(0);

        let member_info = MemberInfo {
            member_id: self_id,
            version: existing_version,
            preferred_nickname: SealedBytes::public("Member".to_string().into_bytes()),
        };
        let authorized_info = AuthorizedMemberInfo::new_with_member_key(member_info, signing_key);

        (
            Some(MembersDelta::new(members_to_add)),
            Some(vec![authorized_info]),
        )
    }

    /// Send a message using an explicit signing key (without requiring local storage)
    ///
    /// This fetches the room state from the network and attempts to re-add the sender
    /// if they were pruned for inactivity. Useful for automation, bots, and CI/CD pipelines.
    pub async fn send_message_with_key(
        &self,
        room_owner_key: &VerifyingKey,
        message_content: String,
        signing_key: &SigningKey,
    ) -> Result<()> {
        info!(
            "Sending message (with explicit key) to room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Fetch room state from the network
        let mut room_state = self.get_room(room_owner_key, false).await?;

        let sender_vk = signing_key.verifying_key();
        let sender_member_id: MemberId = (&sender_vk).into();

        // Create the message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: sender_member_id,
            content: river_core::room_state::message::RoomMessageBody::public(message_content),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let is_member = room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == sender_vk);
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, signing_key);

        if !is_member && members_delta.is_none() {
            return Err(anyhow!(
                "Signing key is not a current member of this room and no stored membership \
                 credentials were found for automatic rejoin. If you were pruned for inactivity, \
                 ensure you first accepted an invitation via `riverctl invite accept`."
            ));
        }

        // Create a delta with the new message
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message.clone()]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta locally for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply message delta: {:?}", e))?;

        // Send the delta to the network
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

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

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Message sent successfully to contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
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

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to send messages.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

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

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the new message
        let delta = river_core::room_state::ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message.clone()]),
            members: members_delta,
            member_info: member_info_delta,
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
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Message sent successfully to contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Edit a message you sent
    pub async fn edit_message(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        new_content: String,
    ) -> Result<()> {
        info!(
            "Editing message in room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to edit messages.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Create the edit action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::edit(
                target_message_id,
                new_content,
            ),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the edit action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply edit delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Delete a message you sent
    pub async fn delete_message(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
    ) -> Result<()> {
        info!(
            "Deleting message in room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to delete messages.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Create the delete action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::delete(target_message_id),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the delete action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply delete delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Add a reaction to a message
    pub async fn add_reaction(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        emoji: String,
    ) -> Result<()> {
        info!(
            "Adding reaction '{}' in room owned by: {}",
            emoji,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to add reactions.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Create the reaction action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::reaction(
                target_message_id,
                emoji,
            ),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the reaction action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply reaction delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Remove a reaction from a message
    pub async fn remove_reaction(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        emoji: String,
    ) -> Result<()> {
        info!(
            "Removing reaction '{}' in room owned by: {}",
            emoji,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to remove reactions.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Create the remove_reaction action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::remove_reaction(
                target_message_id,
                emoji,
            ),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the remove_reaction action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply remove_reaction delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Reply to a message
    pub async fn send_reply(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        reply_text: String,
    ) -> Result<()> {
        info!(
            "Sending reply in room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to send replies.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Find the target message to extract author name and content preview
        let target_msg = room_state
            .recent_messages
            .display_messages()
            .find(|m| m.id() == target_message_id)
            .ok_or_else(|| {
                anyhow!("Target message not found in recent messages. Cannot reply to expired messages via CLI.")
            })?;

        let target_author_name = room_state
            .member_info
            .member_info
            .iter()
            .find(|info| info.member_info.member_id == target_msg.message.author)
            .map(|info| info.member_info.preferred_nickname.to_string_lossy())
            .unwrap_or_else(|| target_msg.message.author.to_string());

        let target_content_preview: String = room_state
            .recent_messages
            .effective_text(target_msg)
            .unwrap_or_else(|| "<encrypted>".to_string())
            .chars()
            .take(100)
            .collect();

        // Create the reply message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::reply(
                reply_text,
                target_message_id,
                target_author_name,
                target_content_preview,
            ),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the reply message
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply reply delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Helper to send a delta to the network.
    /// Assumes migration has already been triggered by the caller (via get_room
    /// or ensure_room_migrated), so owner_vk_to_contract_key returns the correct key.
    async fn send_delta(
        &self,
        room_owner_key: &VerifyingKey,
        delta: ChatRoomStateV1Delta,
    ) -> Result<()> {
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
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Action sent successfully to contract: {}", key.id());
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

            // Use display_messages() to filter out action/deleted messages (matches `message list`)
            let all_msgs: Vec<_> = room_state.recent_messages.display_messages().collect();
            let start = all_msgs.len().saturating_sub(initial_messages);

            for msg in &all_msgs[start..] {
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

            // Poll for new messages (use display_messages to match `message list` filtering)
            match self.get_room(room_owner_key, false).await {
                Ok(room_state) => {
                    for msg in room_state.recent_messages.display_messages() {
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
        // Get effective content (handles edits)
        let content = room_state
            .recent_messages
            .effective_text(msg)
            .unwrap_or_else(|| "<encrypted>".to_string());

        // Get message ID for checking edited status and reactions
        let msg_id = msg.id();
        let edited = room_state.recent_messages.is_edited(&msg_id);
        let reactions = room_state.recent_messages.reactions(&msg_id);

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

                let edited_indicator = if edited { " (edited)" } else { "" };
                let reactions_str = reactions
                    .map(|r| {
                        if r.is_empty() {
                            String::new()
                        } else {
                            let parts: Vec<_> = r
                                .iter()
                                .map(|(emoji, reactors)| format!("{}×{}", emoji, reactors.len()))
                                .collect();
                            format!(" [{}]", parts.join(" "))
                        }
                    })
                    .unwrap_or_default();

                println!(
                    "[{} - {}]: {}{}{}",
                    local_time.format("%H:%M:%S"),
                    nickname,
                    content,
                    edited_indicator,
                    reactions_str
                );
            }
            OutputFormat::Json => {
                let author_str = msg.message.author.to_string();

                let nickname = room_state
                    .member_info
                    .member_info
                    .iter()
                    .find(|info| info.member_info.member_id == msg.message.author)
                    .map(|info| info.member_info.preferred_nickname.to_string_lossy());

                let datetime: DateTime<Utc> = msg.message.time.into();

                let reactions_map: std::collections::HashMap<String, usize> = reactions
                    .map(|r| r.iter().map(|(k, v)| (k.clone(), v.len())).collect())
                    .unwrap_or_default();

                let message_id_str = msg_id.0 .0.to_string();

                // Output as JSONL (one JSON object per line)
                let json_msg = json!({
                    "type": "message",
                    "message_id": message_id_str,
                    "room": bs58::encode(room_owner_key.as_bytes()).into_string(),
                    "author": author_str,
                    "nickname": nickname,
                    "content": content,
                    "timestamp": datetime.to_rfc3339(),
                    "edited": edited,
                    "reactions": reactions_map,
                });

                println!("{}", serde_json::to_string(&json_msg)?);
            }
        }

        // Flush stdout immediately for real-time output
        std::io::stdout().flush()?;
        Ok(())
    }

    /// Set the current user's nickname in a room
    pub async fn set_nickname(
        &self,
        room_owner_key: &VerifyingKey,
        new_nickname: String,
    ) -> Result<()> {
        info!(
            "Setting nickname to '{}' in room owned by: {}",
            new_nickname,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to change your nickname.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        let my_member_id = signing_key.verifying_key().into();

        // Find our current member info to get the version
        let current_version = room_state
            .member_info
            .member_info
            .iter()
            .find(|info| info.member_info.member_id == my_member_id)
            .map(|info| info.member_info.version)
            .unwrap_or(0);

        // Create new member info with incremented version
        let new_member_info = MemberInfo {
            member_id: my_member_id,
            version: current_version + 1,
            preferred_nickname: SealedBytes::public(new_nickname.into_bytes()),
        };

        // Sign with our member key
        let authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(new_member_info, &signing_key);

        // Update local state first
        if let Some(existing_info) = room_state
            .member_info
            .member_info
            .iter_mut()
            .find(|info| info.member_info.member_id == my_member_id)
        {
            *existing_info = authorized_member_info.clone();
        } else {
            room_state
                .member_info
                .member_info
                .push(authorized_member_info.clone());
        }

        // Save the updated state locally
        self.storage
            .update_room_state(room_owner_key, room_state.clone())?;

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, _) = self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create delta with member info update (and members delta if needed)
        let delta = ChatRoomStateV1Delta {
            member_info: Some(vec![authorized_member_info]),
            members: members_delta,
            ..Default::default()
        };

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        // Get contract key and send the update
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

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
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Nickname updated successfully for contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Ban a member from the room
    ///
    /// The banning member must be either the room owner or an upstream member in the
    /// invite chain of the member being banned.
    pub async fn ban_member(
        &self,
        room_owner_key: &VerifyingKey,
        member_id_short: &str,
    ) -> Result<()> {
        info!(
            "Banning member '{}' from room owned by: {}",
            member_id_short,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to ban members.")
        })?;
        let (signing_key, _stored_state, _contract_key_str) = room_data;

        // Fetch fresh room state from the network
        let room_state = self.get_room(room_owner_key, false).await?;

        let my_member_id: MemberId = signing_key.verifying_key().into();
        let owner_member_id: MemberId = room_owner_key.into();

        // Find the member to ban by their short ID (first 8 chars of member_id string)
        let target_member = room_state
            .member_info
            .member_info
            .iter()
            .find(|info| {
                let member_id_str = info.member_info.member_id.to_string();
                member_id_str.starts_with(member_id_short)
                    || member_id_str[..8.min(member_id_str.len())]
                        .eq_ignore_ascii_case(member_id_short)
            })
            .ok_or_else(|| {
                anyhow!(
                    "Member '{}' not found. Use 'member list' to see member IDs.",
                    member_id_short
                )
            })?;

        let banned_member_id = target_member.member_info.member_id;

        // Prevent self-banning
        if banned_member_id == my_member_id {
            return Err(anyhow!("Cannot ban yourself"));
        }

        // Prevent banning the room owner
        if banned_member_id == owner_member_id {
            return Err(anyhow!("Cannot ban the room owner"));
        }

        // Verify authorization: must be room owner OR in the invite chain of the banned member
        if my_member_id != owner_member_id {
            // Build a map of member IDs to their AuthorizedMember for invite chain traversal
            let members_by_id: std::collections::HashMap<_, _> = room_state
                .members
                .members
                .iter()
                .map(|m| (m.member.id(), m))
                .collect();

            // Find the banned member in the members list
            let banned_member = members_by_id.get(&banned_member_id).ok_or_else(|| {
                anyhow!(
                    "Banned member not found in members list (may already be banned or removed)"
                )
            })?;

            // Walk up the invite chain from the banned member to verify authorization
            let mut current_id = banned_member.member.invited_by;
            let mut found_in_chain = false;
            let mut visited = std::collections::HashSet::new();

            while current_id != owner_member_id {
                if current_id == my_member_id {
                    found_in_chain = true;
                    break;
                }

                if !visited.insert(current_id) {
                    return Err(anyhow!("Circular invite chain detected"));
                }

                let inviter = members_by_id
                    .get(&current_id)
                    .ok_or_else(|| anyhow!("Invite chain broken: inviter not found"))?;
                current_id = inviter.member.invited_by;
            }

            if !found_in_chain {
                return Err(anyhow!(
                    "Not authorized to ban this member. You can only ban members you invited (directly or indirectly)."
                ));
            }
        }

        info!("Banning member with ID: {}", banned_member_id.to_string());

        // Create the ban
        let user_ban = UserBan {
            owner_member_id,
            banned_at: std::time::SystemTime::now(),
            banned_user: banned_member_id,
        };

        let authorized_ban = AuthorizedUserBan::new(user_ban, my_member_id, &signing_key);

        // Create delta with just the ban
        let delta = ChatRoomStateV1Delta {
            bans: Some(vec![authorized_ban.clone()]),
            ..Default::default()
        };

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        // Get contract key and send the update
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

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
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Ban applied successfully for contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Update room configuration. Only the room owner can do this.
    pub async fn update_config(
        &self,
        room_owner_key: &VerifyingKey,
        modify: impl FnOnce(&mut Configuration),
    ) -> Result<()> {
        // Get the signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be the room owner to update configuration.")
        })?;
        let (signing_key, _stored_state, _contract_key_str) = room_data;

        // Verify we are the room owner
        let my_vk = signing_key.verifying_key();
        if my_vk != *room_owner_key {
            return Err(anyhow!("Only the room owner can update configuration"));
        }

        // Fetch fresh room state from the network
        let room_state = self.get_room(room_owner_key, false).await?;

        // Clone current config and apply modifications
        let mut new_config = room_state.configuration.configuration.clone();
        new_config.configuration_version += 1;
        modify(&mut new_config);

        // Sign the new configuration
        let authorized_config = AuthorizedConfigurationV1::new(new_config, &signing_key);

        // Create delta with just the configuration change
        let delta = ChatRoomStateV1Delta {
            configuration: Some(authorized_config),
            ..Default::default()
        };

        // Serialize and send
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

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

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!(
                    "Configuration updated successfully for contract: {}",
                    key.id()
                );
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Subscribe to a room and stream updates using Freenet subscriptions
    ///
    /// Unlike `stream_messages` which polls, this method subscribes to the contract
    /// and receives push notifications when the contract state changes.
    pub async fn subscribe_and_stream(
        &self,
        room_owner_key: &VerifyingKey,
        timeout_secs: u64,
        max_messages: usize,
        initial_messages: usize,
        format: OutputFormat,
    ) -> Result<()> {
        // Verify room exists in local storage before attempting to subscribe
        self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found in local storage. You may need to create or join it first.")
        })?;

        // Print header for human format
        if matches!(format, OutputFormat::Human) {
            eprintln!(
                "Subscribing to room {} (press Ctrl+C to stop)...",
                bs58::encode(room_owner_key.as_bytes()).into_string()
            );
        }

        // Track seen message IDs to avoid duplicates
        let mut seen_messages = HashSet::new();
        let mut new_message_count = 0;
        let start_time = std::time::Instant::now();

        // Fetch current room state to pre-populate seen_messages and trigger
        // migration if needed (get_room calls ensure_room_migrated internally).
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);
        let contract_instance_id = *contract_key.id();
        {
            let room_state = self.get_room(room_owner_key, false).await?;

            // Mark ALL non-action messages as seen (including deleted ones),
            // so deleted messages arriving in subscription deltas are not
            // mistakenly shown as new. See: https://github.com/freenet/river/issues/173
            for msg in &room_state.recent_messages.messages {
                if !msg.message.content.is_action() {
                    let msg_id = format!("{:?}:{:?}", msg.message.author, msg.message.time);
                    seen_messages.insert(msg_id);
                }
            }

            // Show the last N display messages if requested
            let display_msgs: Vec<_> = room_state.recent_messages.display_messages().collect();
            let display_start = if initial_messages > 0 {
                display_msgs.len().saturating_sub(initial_messages)
            } else {
                display_msgs.len() // display nothing
            };

            for (i, msg) in display_msgs.iter().enumerate() {
                if i >= display_start {
                    Self::output_message(&room_state, msg, room_owner_key, &format)?;
                }
            }
        }

        // Subscribe to the contract
        {
            let subscribe_request = ContractRequest::Subscribe {
                key: contract_instance_id, // Subscribe uses ContractInstanceId
                summary: None,
            };

            let client_request = ClientRequest::ContractOp(subscribe_request);

            let mut web_api = self.web_api.lock().await;
            web_api
                .send(client_request)
                .await
                .map_err(|e| anyhow!("Failed to send SUBSCRIBE request: {}", e))?;

            // Wait for subscription response (30s to accommodate slow gateways)
            let response = match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                web_api.recv(),
            )
            .await
            {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for SUBSCRIBE response")),
            };

            match response {
                HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                    subscribed,
                    ..
                }) => {
                    if subscribed {
                        if matches!(format, OutputFormat::Human) {
                            eprintln!("Successfully subscribed. Waiting for updates...\n");
                        }
                    } else {
                        return Err(anyhow!("Failed to subscribe to contract"));
                    }
                }
                _ => return Err(anyhow!("Unexpected response to SUBSCRIBE request")),
            }
        }

        // Set up Ctrl+C handler
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let _ = shutdown_tx.send(()).await;
        });

        // Main loop: wait for UpdateNotification messages
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
                debug!("Timeout reached, exiting subscription stream");
                return Ok(());
            }

            // Check max messages
            if max_messages > 0 && new_message_count >= max_messages {
                debug!("Maximum message count reached, exiting subscription stream");
                return Ok(());
            }

            // Wait for next message with a short timeout to allow checking shutdown
            let mut web_api = self.web_api.lock().await;
            let recv_result =
                tokio::time::timeout(std::time::Duration::from_millis(500), web_api.recv()).await;

            match recv_result {
                Ok(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                    key,
                    update,
                }))) => {
                    // We received an update notification
                    debug!("Received update notification for contract: {}", key.id());

                    match update {
                        UpdateData::Delta(delta_bytes) => {
                            // Parse the delta and filter action/deleted messages before display
                            if let Ok(delta) = ciborium::de::from_reader::<ChatRoomStateV1Delta, _>(
                                &delta_bytes[..],
                            ) {
                                if let Some(messages) = &delta.recent_messages {
                                    for msg in messages {
                                        // Skip action messages (edits, deletions, reactions)
                                        if msg.message.content.is_action() {
                                            continue;
                                        }
                                        let msg_id = format!(
                                            "{:?}:{:?}",
                                            msg.message.author, msg.message.time
                                        );

                                        if seen_messages.insert(msg_id.clone()) {
                                            // Fetch full room state to check deleted status
                                            // and get display context (nicknames, reactions)
                                            drop(web_api);
                                            match self.get_room(room_owner_key, false).await {
                                                Ok(room_state) => {
                                                    // Skip deleted messages (fixes #173: phantom messages)
                                                    if !room_state
                                                        .recent_messages
                                                        .actions_state
                                                        .deleted
                                                        .contains(&msg.id())
                                                    {
                                                        Self::output_message(
                                                            &room_state,
                                                            msg,
                                                            room_owner_key,
                                                            &format,
                                                        )?;
                                                        new_message_count += 1;
                                                    }
                                                }
                                                Err(e) => {
                                                    // Remove from seen so the message can be
                                                    // retried on the next delta
                                                    debug!("Failed to fetch room state: {}", e);
                                                    seen_messages.remove(&msg_id);
                                                }
                                            }
                                            web_api = self.web_api.lock().await;

                                            if max_messages > 0 && new_message_count >= max_messages
                                            {
                                                return Ok(());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        UpdateData::State(state_bytes) => {
                            // Full state update — use display_messages() for consistent filtering
                            if let Ok(room_state) =
                                ciborium::de::from_reader::<ChatRoomStateV1, _>(&state_bytes[..])
                            {
                                for msg in room_state.recent_messages.display_messages() {
                                    let msg_id =
                                        format!("{:?}:{:?}", msg.message.author, msg.message.time);

                                    if seen_messages.insert(msg_id.clone()) {
                                        Self::output_message(
                                            &room_state,
                                            msg,
                                            room_owner_key,
                                            &format,
                                        )?;
                                        new_message_count += 1;

                                        if max_messages > 0 && new_message_count >= max_messages {
                                            return Ok(());
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            debug!("Received non-delta/state update, skipping");
                        }
                    }
                }
                Ok(Ok(other)) => {
                    // Other message type, log and continue
                    debug!("Received unexpected message: {:?}", other);
                }
                Ok(Err(e)) => {
                    // WebSocket error
                    return Err(anyhow!("WebSocket error: {}", e));
                }
                Err(_) => {
                    // Timeout, continue loop (allows checking shutdown signal)
                }
            }
        }
    }
}
