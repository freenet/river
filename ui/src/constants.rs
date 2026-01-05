#![allow(dead_code)]

pub const KEY_VERSION_PREFIX: &str = "river:v1";

// Use ui/public/contracts/ as single source of truth for WASM files
// These are the authoritative versions that match deployed contracts
pub const ROOM_CONTRACT_WASM: &[u8] = include_bytes!("../public/contracts/room_contract.wasm");

pub const CHAT_DELEGATE_WASM: &[u8] = include_bytes!("../public/contracts/chat_delegate.wasm");

// pub const ROOM_CONTRACT_CODE_HASH: CodeHash = CodeHash::from_code(ROOM_CONTRACT_WASM);
