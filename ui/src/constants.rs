pub const KEY_VERSION_PREFIX: &str = "river:v1:user:vk:";

// TODO: Should be built as a dependency with cargo build --release --target wasm32-unknown-unknown -p room-contract
pub const ROOM_CONTRACT_WASM: &[u8] = include_bytes!("../public/contracts/room_contract.wasm");

// pub const ROOM_CONTRACT_CODE_HASH: CodeHash = CodeHash::from_code(ROOM_CONTRACT_WASM);
