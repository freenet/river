//! Read-only: GET one contract's state from a node and write the raw bytes.
//! No subscription, no PUT, no heal: unlike `riverctl`'s room commands it
//! never writes to the network. Used to take the Official room's live v1
//! state as input to the v2 conversion measurement.
//!
//! `cargo run -p riverctl --example fetch_room_state -- <contract-id> <out> [node-url]`

use anyhow::{anyhow, Context, Result};
use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
};
use freenet_stdlib::prelude::ContractInstanceId;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let id = args.next().context("contract id (base58)")?;
    let out = args.next().context("output path")?;
    let url = args.next().unwrap_or_else(|| {
        "ws://127.0.0.1:7509/v1/contract/command?encodingProtocol=native".to_string()
    });
    let id = ContractInstanceId::try_from(id).map_err(|e| anyhow!("bad contract id: {e}"))?;
    let (ws, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let mut api = WebApi::start(ws);
    api.send(ClientRequest::ContractOp(ContractRequest::Get {
        key: id,
        return_contract_code: false,
        subscribe: false,
        blocking_subscribe: false,
    }))
    .await?;
    let resp = tokio::time::timeout(Duration::from_secs(60), api.recv()).await??;
    match resp {
        HostResponse::ContractResponse(ContractResponse::GetResponse { state, .. }) => {
            std::fs::write(&out, state.as_ref())?;
            println!("wrote {} bytes to {out}", state.as_ref().len());
            Ok(())
        }
        other => Err(anyhow!("unexpected response: {other:?}")),
    }
}
