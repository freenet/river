use anyhow::{anyhow, Result};
use crate::config::Config;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, HostResponse, WebApi};
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion,
    Parameters, WrappedContract, WrappedState,
};
use river_common::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_common::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tracing::info;

// Load the room contract WASM from the UI's public folder
const ROOM_CONTRACT_WASM: &[u8] = include_bytes!("../../ui/public/contracts/room_contract.wasm");

pub struct ApiClient {
    web_api: Arc<Mutex<WebApi>>,
    config: Config,
    // Track rooms by their owner's verifying key
    rooms: Arc<Mutex<HashMap<VerifyingKey, RoomInfo>>>,
}

struct RoomInfo {
    signing_key: SigningKey,
    state: ChatRoomStateV1,
    contract_key: ContractKey,
}

impl ApiClient {
    pub async fn new(node_url: &str, config: Config) -> Result<Self> {
        // Adjust URL format for Freenet WebSocket
        let ws_url = if node_url.starts_with("ws://") {
            // Convert ws:// URL to format expected by Freenet
            let base = node_url.trim_end_matches('/');
            if base.contains("/ws/v1") {
                format!("{}/contract/command?encodingProtocol=native", base)
            } else {
                format!("{}/v1/contract/command?encodingProtocol=native", base)
            }
        } else {
            return Err(anyhow!("URL must start with ws://"));
        };

        info!("Connecting to Freenet node at: {}", ws_url);
        
        // Connect using tokio-tungstenite
        let (ws_stream, _) = connect_async(&ws_url).await
            .map_err(|e| anyhow!("Failed to connect to WebSocket: {}", e))?;
        
        info!("WebSocket connected successfully");
        
        // Create WebApi instance
        let web_api = WebApi::start(ws_stream);
        
        Ok(Self {
            web_api: Arc::new(Mutex::new(web_api)),
            config,
            rooms: Arc::new(Mutex::new(HashMap::new())),
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
        
        // Wait for response
        let response = web_api.recv().await
            .map_err(|e| anyhow!("Failed to receive response: {}", e))?;
        
        match response {
            HostResponse::ContractResponse(_contract_response) => {
                info!("Room created successfully with contract key: {}", contract_key.id());
                
                // Store room info
                let room_info = RoomInfo {
                    signing_key,
                    state: room_state,
                    contract_key: contract_key.clone(),
                };
                self.rooms.lock().await.insert(owner_vk, room_info);
                
                Ok((owner_vk, contract_key))
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub async fn get_room(&self, contract_key: &ContractKey) -> Result<ChatRoomStateV1> {
        info!("Getting room state for contract: {}", contract_key.id());
        
        let get_request = ContractRequest::Get {
            key: contract_key.clone(),
            return_contract_code: false,
            subscribe: false,
        };
        
        let client_request = ClientRequest::ContractOp(get_request);
        
        let mut web_api = self.web_api.lock().await;
        web_api.send(client_request).await
            .map_err(|e| anyhow!("Failed to send GET request: {}", e))?;
        
        let response = web_api.recv().await
            .map_err(|e| anyhow!("Failed to receive response: {}", e))?;
        
        match response {
            HostResponse::ContractResponse(_contract_response) => {
                // TODO: Properly deserialize the state from the response
                info!("Received room state");
                Ok(ChatRoomStateV1::default())
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
}