use anyhow::{anyhow, Result};
use directories::ProjectDirs;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::prelude::ContractKey;
use river_core::room_state::ChatRoomStateV1;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRoomInfo {
    pub signing_key_bytes: [u8; 32],
    pub state: ChatRoomStateV1,
    pub contract_key: String, // Store as string for serialization
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomStorage {
    /// Map from room owner verifying key (as base58) to room info
    pub rooms: HashMap<String, StoredRoomInfo>,
}

impl Default for RoomStorage {
    fn default() -> Self {
        Self {
            rooms: HashMap::new(),
        }
    }
}

pub struct Storage {
    storage_path: PathBuf,
}

impl Storage {
    pub fn new() -> Result<Self> {
        let proj_dirs = ProjectDirs::from("", "Freenet", "River")
            .ok_or_else(|| anyhow!("Failed to determine project directories"))?;
        
        let data_dir = proj_dirs.data_dir();
        fs::create_dir_all(data_dir)?;
        
        let storage_path = data_dir.join("rooms.json");
        
        Ok(Self { storage_path })
    }
    
    pub fn load_rooms(&self) -> Result<RoomStorage> {
        if !self.storage_path.exists() {
            return Ok(RoomStorage::default());
        }
        
        let contents = fs::read_to_string(&self.storage_path)?;
        let storage: RoomStorage = serde_json::from_str(&contents)?;
        Ok(storage)
    }
    
    pub fn save_rooms(&self, storage: &RoomStorage) -> Result<()> {
        let contents = serde_json::to_string_pretty(storage)?;
        fs::write(&self.storage_path, contents)?;
        Ok(())
    }
    
    pub fn add_room(
        &self,
        owner_vk: &VerifyingKey,
        signing_key: &SigningKey,
        state: ChatRoomStateV1,
        contract_key: &ContractKey,
    ) -> Result<()> {
        let mut storage = self.load_rooms()?;
        
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        let room_info = StoredRoomInfo {
            signing_key_bytes: signing_key.to_bytes(),
            state,
            contract_key: contract_key.id().to_string(),
        };
        
        storage.rooms.insert(owner_key_str, room_info);
        self.save_rooms(&storage)?;
        
        Ok(())
    }
    
    pub fn get_room(&self, owner_vk: &VerifyingKey) -> Result<Option<(SigningKey, ChatRoomStateV1, String)>> {
        let storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        
        if let Some(room_info) = storage.rooms.get(&owner_key_str) {
            let signing_key = SigningKey::from_bytes(&room_info.signing_key_bytes);
            Ok(Some((signing_key, room_info.state.clone(), room_info.contract_key.clone())))
        } else {
            Ok(None)
        }
    }
    
    pub fn update_room_state(&self, owner_vk: &VerifyingKey, state: ChatRoomStateV1) -> Result<()> {
        let mut storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        
        if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
            room_info.state = state;
            self.save_rooms(&storage)?;
            Ok(())
        } else {
            Err(anyhow!("Room not found"))
        }
    }
    
    pub fn list_rooms(&self) -> Result<Vec<(VerifyingKey, String, String)>> {
        let storage = self.load_rooms()?;
        let mut rooms = Vec::new();
        
        for (owner_key_str, room_info) in storage.rooms.iter() {
            let owner_key_bytes = bs58::decode(owner_key_str).into_vec()?;
            if owner_key_bytes.len() == 32 {
                let mut key_array = [0u8; 32];
                key_array.copy_from_slice(&owner_key_bytes);
                if let Ok(owner_vk) = VerifyingKey::from_bytes(&key_array) {
                    let room_name = room_info.state.configuration.configuration.name.clone();
                    rooms.push((owner_vk, room_name, room_info.contract_key.clone()));
                }
            }
        }
        
        Ok(rooms)
    }
}