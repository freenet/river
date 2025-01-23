pub use crate::crypto_values::CryptoValue;
pub const KEY_VERSION_PREFIX: &str = "river:v1";

pub fn key_version_prefix(crypto_value: &CryptoValue) -> String {
    key_type.to_encoded_string()
}

pub const ROOM_CONTRACT_WASM: &[u8] =
    include_bytes!("../../target/wasm32-unknown-unknown/release/room_contract.wasm");

// pub const ROOM_CONTRACT_CODE_HASH: CodeHash = CodeHash::from_code(ROOM_CONTRACT_WASM);
