use anyhow::{anyhow, Result};
use crate::config::Config;
use crate::storage::Storage;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi};
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion,
    Parameters, UpdateData, WrappedContract, WrappedState,
};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use river_core::room_state::member::{AuthorizedMember, Member};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tracing::info;

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
    config: Config,
    storage: Storage,
}


impl ApiClient {
    pub async fn new(node_url: &str, config: Config, config_dir: Option<&str>) -> Result<Self> {
        // Use the URL as provided - it should already be in the correct format
        info!("Connecting to Freenet node at: {}", node_url);
        
        // Connect using tokio-tungstenite
        let (ws_stream, _) = connect_async(node_url).await
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

    pub async fn create_room(&self, name: String, nickname: String) -> Result<(VerifyingKey, ContractKey)> {
        info!("Creating room: {}", name);
        
        // Generate signing key for the room owner
        let signing_key = SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()));
        let owner_vk = signing_key.verifying_key();
        
        // Create initial room state
        let mut room_state = ChatRoomStateV1::default();
        
        // Set initial configuration
        let mut config = Configuration::default();
        config.name = name.clone();
        config.owner_member_id = owner_vk.into();
        room_state.configuration = AuthorizedConfigurationV1::new(config, &signing_key);
        
        // Add owner to member_info
        let owner_info = MemberInfo {
            member_id: owner_vk.into(),
            version: 0,
            preferred_nickname: nickname,
        };
        let authorized_owner_info = AuthorizedMemberInfo::new(owner_info, &signing_key);
        room_state.member_info.member_info.push(authorized_owner_info);
        
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
            contract_code.clone()
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
        let wrapped_state = WrappedState::new(state_bytes.into());
        
        // Create PUT request
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
        };
        
        let client_request = ClientRequest::ContractOp(put_request);
        
        // Send request
        let mut web_api = self.web_api.lock().await;
        web_api.send(client_request).await
            .map_err(|e| anyhow!("Failed to send PUT request: {}", e))?;
        
        // Wait for response with timeout
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            web_api.recv()
        ).await {
            Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
            Err(_) => return Err(anyhow!("Timeout waiting for PUT response after 30 seconds")),
        };
        
        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::PutResponse { key } => {
                        info!("Room created successfully with contract key: {}", key.id());
                        
                        // Verify the key matches what we expected
                        if key != contract_key {
                            return Err(anyhow!("Contract key mismatch: expected {}, got {}", 
                                contract_key.id(), key.id()));
                        }
                        
                        // Store room info persistently
                        self.storage.add_room(&owner_vk, &signing_key, room_state, &contract_key)?;
                        
                        Ok((owner_vk, contract_key))
                    }
                    _ => Err(anyhow!("Unexpected contract response type for PUT request"))
                }
            }
            HostResponse::Ok => {
                // Some versions might return Ok for successful operations
                info!("Room created (Ok response) with contract key: {}", contract_key.id());
                
                // Store room info persistently
                self.storage.add_room(&owner_vk, &signing_key, room_state, &contract_key)?;
                
                Ok((owner_vk, contract_key))
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub async fn get_room(&self, room_owner_key: &VerifyingKey, subscribe: bool) -> Result<ChatRoomStateV1> {
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);
        info!("Getting room state for contract: {}", contract_key.id());
        
        let get_request = ContractRequest::Get {
            key: contract_key.clone(),
            return_contract_code: false,
            subscribe,
        };
        
        let client_request = ClientRequest::ContractOp(get_request);
        
        let mut web_api = self.web_api.lock().await;
        web_api.send(client_request).await
            .map_err(|e| anyhow!("Failed to send GET request: {}", e))?;
        
        let response = web_api.recv().await
            .map_err(|e| anyhow!("Failed to receive response: {}", e))?;
        
        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse { state, .. } => {
                        // Deserialize the state properly
                        let room_state: ChatRoomStateV1 = ciborium::de::from_reader(&state[..])
                            .map_err(|e| anyhow!("Failed to deserialize room state: {}", e))?;
                        info!("Successfully retrieved room state with {} messages", 
                              room_state.recent_messages.messages.len());
                        Ok(room_state)
                    }
                    _ => Err(anyhow!("Unexpected contract response type"))
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
        web_api.send(test_request).await
            .map_err(|e| anyhow!("Failed to send test request: {}", e))?;
        
        info!("Connection test successful");
        Ok(())
    }
    
    pub async fn create_invitation(&self, room_owner_key: &VerifyingKey) -> Result<String> {
        info!("Creating invitation for room owned by: {}", bs58::encode(room_owner_key.as_bytes()).into_string());
        
        // Get the room info from persistent storage
        let room_data = self.storage.get_room(room_owner_key)?
            .ok_or_else(|| anyhow!("Room not found in local storage. You must be the room owner to create invitations."))?;
        let (signing_key, _state, _contract_key) = room_data;
        
        // Generate a new signing key for the invitee
        let invitee_signing_key = SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()));
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
    
    pub async fn accept_invitation(&self, invitation_code: &str) -> Result<(VerifyingKey, ContractKey)> {
        info!("Accepting invitation...");
        
        // Decode the invitation
        let decoded = bs58::decode(invitation_code)
            .into_vec()
            .map_err(|e| anyhow!("Failed to decode invitation: {}", e))?;
        let invitation: Invitation = ciborium::de::from_reader(&decoded[..])
            .map_err(|e| anyhow!("Failed to deserialize invitation: {}", e))?;
        
        let room_owner_vk = invitation.room;
        let contract_key = self.owner_vk_to_contract_key(&room_owner_vk);
        
        info!("Invitation is for room owned by: {}", bs58::encode(room_owner_vk.as_bytes()).into_string());
        info!("Contract key: {}", contract_key.id());
        
        // Perform a GET request to fetch the room state
        let get_request = ContractRequest::Get {
            key: contract_key.clone(),
            return_contract_code: false,
            subscribe: true,
        };
        
        let client_request = ClientRequest::ContractOp(get_request);
        
        let mut web_api = self.web_api.lock().await;
        web_api.send(client_request).await
            .map_err(|e| anyhow!("Failed to send GET request: {}", e))?;
        
        // Wait for response with timeout
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            web_api.recv()
        ).await {
            Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
            Err(_) => return Err(anyhow!("Timeout waiting for GET response after 30 seconds")),
        };
        
        match response {
            HostResponse::ContractResponse(_contract_response) => {
                info!("Successfully retrieved room state");
                
                // Store the invitation details persistently
                self.storage.add_room(
                    &room_owner_vk,
                    &invitation.invitee_signing_key,
                    ChatRoomStateV1::default(), // TODO: Parse from response
                    &contract_key
                )?;
                
                Ok((room_owner_vk, contract_key))
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }
    
    pub fn owner_vk_to_contract_key(&self, owner_vk: &VerifyingKey) -> ContractKey {
        let parameters = ChatRoomParametersV1 { owner: *owner_vk };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf).expect("Serialization should not fail");
            buf
        };
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let instance_id = ContractInstanceId::from_params_and_code(
            Parameters::from(params_bytes),
            contract_code
        );
        ContractKey::from(instance_id)
    }
    
    pub async fn list_rooms(&self) -> Result<Vec<(String, String, String)>> {
        self.storage.list_rooms()
            .map(|rooms| rooms.into_iter()
                .map(|(owner_vk, name, contract_key)| {
                    (
                        bs58::encode(owner_vk.as_bytes()).into_string(),
                        name,
                        contract_key
                    )
                })
                .collect())
    }
    
    pub async fn send_message(&self, room_owner_key: &VerifyingKey, message_content: String) -> Result<()> {
        info!("Sending message to room owned by: {}", bs58::encode(room_owner_key.as_bytes()).into_string());
        
        // Get the room info from storage
        let room_data = self.storage.get_room(room_owner_key)?
            .ok_or_else(|| anyhow!("Room not found. You must be a member of the room to send messages."))?;
        let (signing_key, mut room_state, _contract_key_str) = room_data;
        
        // Create the message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: river_core::room_state::member::MemberId::from(*room_owner_key),
            author: river_core::room_state::member::MemberId::from(&signing_key.verifying_key()),
            content: message_content,
            time: std::time::SystemTime::now(),
        };
        
        // Sign the message
        let auth_message = river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);
        
        // Create a delta with the new message
        let delta = river_core::room_state::ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message.clone()]),
            ..Default::default()
        };
        
        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 { owner: *room_owner_key };
        room_state.apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply message delta: {:?}", e))?;
        
        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state.clone())?;
        
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
        web_api.send(client_request).await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;
        
        // Wait for response
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            web_api.recv()
        ).await {
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
}