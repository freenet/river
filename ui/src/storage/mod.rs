mod room_storage;

use crate::util::{from_cbor_slice, to_cbor_vec};
use dioxus::logger::tracing::{error, info};
use ed25519_dalek::VerifyingKey;
use river_common::crypto_values::CryptoValue;
use std::collections::HashMap;

pub use room_storage::StoredRoomData;

const ROOMS_STORAGE_KEY: &str = "river_rooms";

pub fn get_local_storage() -> Result<web_sys::Window, String> {
    web_sys::window()
        .ok_or_else(|| "No window object found".to_string())
}

pub fn save_rooms(rooms: &HashMap<VerifyingKey, StoredRoomData>) -> Result<(), String> {
    let window = get_local_storage()?;
    
    // Convert the map to a serializable format
    let serializable_map: HashMap<String, StoredRoomData> = rooms
        .iter()
        .map(|(vk, data)| {
            let vk_str = CryptoValue::VerifyingKey(*vk).to_encoded_string();
            (vk_str, data.clone())
        })
        .collect();
    
    // Serialize to CBOR
    let cbor_data = to_cbor_vec(&serializable_map);
    
    // Convert to URI component for safe storage
    let encoded = js_sys::encode_uri_component(&String::from_utf8_lossy(&cbor_data));
    
    info!("Saving {} rooms to local storage", rooms.len());
    
    // Store in localStorage
    if let Ok(Some(storage)) = window.local_storage() {
        storage
            .set_item(ROOMS_STORAGE_KEY, &encoded.as_string().unwrap_or_default())
            .map_err(|_| "Failed to store rooms data".to_string())
    } else {
        Err("Failed to access localStorage".to_string())
    }
}

pub fn load_rooms() -> Result<HashMap<VerifyingKey, StoredRoomData>, String> {
    let window = get_local_storage()?;
    
    // Get the storage object
    let storage = match window.local_storage() {
        Ok(Some(storage)) => storage,
        Ok(None) => return Err("localStorage is not available".to_string()),
        Err(_) => return Err("Failed to access localStorage".to_string()),
    };
    
    // Get the stored data
    let encoded = match storage.get_item(ROOMS_STORAGE_KEY) {
        Ok(Some(data)) => data,
        Ok(None) => return Ok(HashMap::new()), // No stored data yet
        Err(_) => return Err("Failed to retrieve rooms data".to_string()),
    };
    
    // Decode from URI component
    let decoded = js_sys::decode_uri_component(&encoded)
        .map_err(|_| "Failed to decode URI component".to_string())?;
    let decoded_str = decoded.as_string().ok_or_else(|| "Failed to convert to string".to_string())?;
    let cbor_data = decoded_str.as_bytes().to_vec();
    
    // Deserialize from CBOR
    let serialized_map: HashMap<String, StoredRoomData> = from_cbor_slice(&cbor_data);
    
    // Convert back to our internal format
    let mut result = HashMap::new();
    for (vk_str, data) in serialized_map {
        match CryptoValue::from_encoded_string(&vk_str) {
            Ok(CryptoValue::VerifyingKey(vk)) => {
                result.insert(vk, data);
            }
            _ => {
                error!("Invalid verifying key format in stored data: {}", vk_str);
            }
        }
    }
    
    info!("Loaded {} rooms from local storage", result.len());
    Ok(result)
}

pub fn persist_room_data(owner_vk: VerifyingKey, signing_key: &ed25519_dalek::SigningKey) {
    let mut stored_rooms = match load_rooms() {
        Ok(rooms) => rooms,
        Err(e) => {
            error!("Failed to load rooms for persistence: {}", e);
            HashMap::new()
        }
    };
    
    stored_rooms.insert(owner_vk, StoredRoomData::new(signing_key));
    
    if let Err(e) = save_rooms(&stored_rooms) {
        error!("Failed to persist room data: {}", e);
    } else {
        info!("Successfully persisted room data for {:?}", owner_vk);
    }
}

pub fn remove_persisted_room(owner_vk: &VerifyingKey) {
    let mut stored_rooms = match load_rooms() {
        Ok(rooms) => rooms,
        Err(e) => {
            error!("Failed to load rooms for removal: {}", e);
            return;
        }
    };
    
    stored_rooms.remove(owner_vk);
    
    if let Err(e) = save_rooms(&stored_rooms) {
        error!("Failed to update persisted rooms after removal: {}", e);
    } else {
        info!("Successfully removed persisted room data for {:?}", owner_vk);
    }
}
