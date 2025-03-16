pub const KEY_VERSION_PREFIX: &str = "river:v1";

pub const ROOM_CONTRACT_WASM: &[u8] =
    include_bytes!("../../target/wasm32-unknown-unknown/release/room_contract.wasm");

// The chat delegate will be included once it's built
#[cfg(feature = "with_delegate")]
pub const CHAT_DELEGATE_WASM: &[u8] =
    include_bytes!("../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");

#[cfg(not(feature = "with_delegate"))]
pub const CHAT_DELEGATE_WASM: &[u8] = &[];

// pub const ROOM_CONTRACT_CODE_HASH: CodeHash = CodeHash::from_code(ROOM_CONTRACT_WASM);
