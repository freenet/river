// TODO: Should be built as a dependency with cargo build --release --target wasm32-unknown-unknown -p room-contract
pub const ROOM_CONTRACT_WASM: &[u8] = include_bytes!("../../../../target/wasm32-unknown-unknown/release/room_contract.wasm");
