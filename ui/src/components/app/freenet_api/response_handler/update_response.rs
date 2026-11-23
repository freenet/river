use dioxus::logger::tracing::info;
use freenet_stdlib::prelude::ContractKey;

/// A reply to one of our UPDATEs. It names the contract, not the request, so
/// all it does is end that room's best-effort wait feedback (`user_waits`).
pub fn handle_update_response(key: ContractKey, summary: Vec<u8>) {
    let summary_len = summary.len();
    info!("Received update response for key {key}, summary length {summary_len}");
    crate::components::app::user_waits::note_update_response(key);
}
