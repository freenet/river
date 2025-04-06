pub const KEY_VERSION_PREFIX: &str = "river:v1";

pub const ROOM_CONTRACT_WASM: &[u8] =
    include_bytes!("../../target/wasm32-unknown-unknown/release/room_contract.wasm");

pub const CHAT_DELEGATE_WASM: &[u8] =
    include_bytes!("../../target/wasm32-unknown-unknown/release/chat_delegate.wasm");

// pub const ROOM_CONTRACT_CODE_HASH: CodeHash = CodeHash::from_code(ROOM_CONTRACT_WASM);
