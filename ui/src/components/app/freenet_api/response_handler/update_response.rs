use dioxus::logger::tracing::info;
use freenet_stdlib::prelude::ContractKey;

pub fn handle_update_response(key: ContractKey, summary: Vec<u8>) {
    let summary_len = summary.len();
    info!("Received update response for key {key}, summary length {summary_len}, currently ignored");
}
