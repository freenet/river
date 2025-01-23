pub use crate::crypto_keys::CryptoKeyType;

pub fn key_version_prefix(key_type: &CryptoKeyType) -> String {
    key_type.to_encoded_string()
}

pub const ROOM_CONTRACT_WASM: &[u8] =
    include_bytes!("../../target/wasm32-unknown-unknown/release/room_contract.wasm");

// pub const ROOM_CONTRACT_CODE_HASH: CodeHash = CodeHash::from_code(ROOM_CONTRACT_WASM);
