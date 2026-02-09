//! Add GitHub bot to Freenet Official room
//!
//! Run with: cargo run --example add_github_bot

use anyhow::{anyhow, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
};
use freenet_stdlib::prelude::*;
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::privacy::SealedBytes;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;

// Room contract WASM bytes (bundled)
const ROOM_CONTRACT_WASM: &[u8] = include_bytes!("../contracts/room_contract.wasm");

#[tokio::main]
async fn main() -> Result<()> {
    // Configuration
    let node_url = "ws://127.0.0.1:7509/v1/contract/command?encodingProtocol=native";

    // Room owner verifying key
    let room_owner_vk_bytes: [u8; 32] =
        bs58::decode("69Ht4YjZsT884MndR2uWhQYe1wb9b2x77HRq7Dgq7wYE")
            .into_vec()?
            .try_into()
            .map_err(|_| anyhow!("Invalid room owner key"))?;
    let room_owner_vk = VerifyingKey::from_bytes(&room_owner_vk_bytes)?;
    let room_owner_id = MemberId::from(&room_owner_vk);

    // Invite bot signing key (existing member who can invite)
    let invite_bot_sk_bytes: [u8; 32] =
        bs58::decode("DQREBvjYDAYxHJ5tb1SiwpTtZyTRtqr3uwzXEXQsXsvt")
            .into_vec()?
            .try_into()
            .map_err(|_| anyhow!("Invalid invite bot key"))?;
    let invite_bot_sk = SigningKey::from_bytes(&invite_bot_sk_bytes);
    let invite_bot_vk = invite_bot_sk.verifying_key();
    let invite_bot_id = MemberId::from(&invite_bot_vk);

    // GitHub bot verifying key (new member to add)
    let github_bot_vk_bytes: [u8; 32] =
        bs58::decode("J1AZJYr1fT7kyqusdzii3AESjnuDGqkpwgVWLE1CVfyz")
            .into_vec()?
            .try_into()
            .map_err(|_| anyhow!("Invalid github bot key"))?;
    let github_bot_vk = VerifyingKey::from_bytes(&github_bot_vk_bytes)?;
    let github_bot_id = MemberId::from(&github_bot_vk);

    // GitHub bot signing key (for member info)
    let github_bot_sk_bytes: [u8; 32] =
        bs58::decode("ATiVkfVEPP5RTciXhQkD7ZSjNybBYf9vgQLLy3ULG3fx")
            .into_vec()?
            .try_into()
            .map_err(|_| anyhow!("Invalid github bot signing key"))?;
    let github_bot_sk = SigningKey::from_bytes(&github_bot_sk_bytes);

    println!("Connecting to Freenet node at {}...", node_url);
    let (ws_stream, _) = connect_async(node_url).await?;
    let web_api = WebApi::start(ws_stream);
    let web_api = Arc::new(Mutex::new(web_api));

    // Create contract key from room owner
    let params = ChatRoomParametersV1 {
        owner: room_owner_vk,
    };
    let params_bytes = {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&params, &mut buf)?;
        buf
    };
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    let contract_key =
        ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);

    println!("Contract key: {}", contract_key.id());

    // Fetch current room state
    println!("Fetching room state...");
    let get_request = ContractRequest::Get {
        key: *contract_key.id(),
        return_contract_code: false,
        subscribe: false,
        blocking_subscribe: false,
    };

    {
        let mut api = web_api.lock().await;
        api.send(ClientRequest::ContractOp(get_request)).await?;
    }

    let response = {
        let mut api = web_api.lock().await;
        tokio::time::timeout(std::time::Duration::from_secs(30), api.recv()).await??
    };

    let mut room_state: ChatRoomStateV1 = match response {
        HostResponse::ContractResponse(ContractResponse::GetResponse { state, .. }) => {
            ciborium::de::from_reader(&state[..])?
        }
        _ => return Err(anyhow!("Unexpected response")),
    };

    // Rebuild actions state
    room_state.recent_messages.rebuild_actions_state();

    println!(
        "Room name: {}",
        room_state
            .configuration
            .configuration
            .display
            .name
            .to_string_lossy()
    );
    println!("Current members: {}", room_state.members.members.len());

    // List all members with their nicknames
    println!("\nMember list:");
    for am in &room_state.members.members {
        let member_id = MemberId::from(&am.member.member_vk);
        let nickname = room_state
            .member_info
            .member_info
            .iter()
            .find(|mi| mi.member_info.member_id == member_id)
            .map(|mi| mi.member_info.preferred_nickname.to_string_lossy())
            .unwrap_or_else(|| "(no nickname)".to_string());
        let vk_b58 = bs58::encode(am.member.member_vk.as_bytes()).into_string();
        println!("  - {} (vk: {}...)", nickname, &vk_b58[..8]);
    }
    println!();

    // Check if GitHub bot is already a member
    let already_member = room_state
        .members
        .members
        .iter()
        .any(|m| m.member.member_vk == github_bot_vk);
    if already_member {
        println!("GitHub bot is already a member!");
        return Ok(());
    }

    // Create new member entry (invited by invite bot)
    println!("Creating member entry for GitHub bot...");
    let member = Member {
        owner_member_id: room_owner_id,
        invited_by: invite_bot_id,
        member_vk: github_bot_vk,
    };
    let authorized_member = AuthorizedMember::new(member, &invite_bot_sk);

    // Create member info entry
    let member_info = MemberInfo {
        member_id: github_bot_id,
        version: 0,
        preferred_nickname: SealedBytes::public("GitHub Bot".to_string().into_bytes()),
    };
    let authorized_member_info = AuthorizedMemberInfo::new(member_info, &github_bot_sk);

    // Create delta with new member
    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![authorized_member])),
        member_info: Some(vec![authorized_member_info]),
        ..Default::default()
    };

    // Serialize delta
    let delta_bytes = {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&delta, &mut buf)?;
        buf
    };

    // Send update
    println!("Sending member addition to network...");
    let update_request = ContractRequest::Update {
        key: contract_key.clone(),
        data: UpdateData::Delta(StateDelta::from(delta_bytes)),
    };

    {
        let mut api = web_api.lock().await;
        api.send(ClientRequest::ContractOp(update_request)).await?;
    }

    // Wait for response
    let update_response = {
        let mut api = web_api.lock().await;
        tokio::time::timeout(std::time::Duration::from_secs(30), api.recv()).await??
    };

    match update_response {
        HostResponse::ContractResponse(ContractResponse::UpdateResponse { .. }) => {
            println!("✓ GitHub bot successfully added as member!");
        }
        HostResponse::ContractResponse(ContractResponse::UpdateNotification { .. }) => {
            println!("✓ Received update notification - member likely added!");
        }
        other => {
            println!("Response: {:?}", other);
        }
    }

    Ok(())
}
