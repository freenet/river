pub mod test_utils;

use anyhow::Result;
use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, HostResponse, WebApi},
    prelude::*,
};
use river_core::{
    room_state::{configuration::AuthorizedConfigurationV1, ChatRoomParametersV1},
    ChatRoomStateV1,
};
use std::time::Duration;
pub use test_utils::*;

#[derive(Debug, Clone)]
pub struct RoomTestState {
    pub room_state: ChatRoomStateV1,
    pub parameters: ChatRoomParametersV1,
    #[allow(dead_code)]
    pub owner_key: SigningKey,
}

impl RoomTestState {
    pub fn new_test_room() -> Self {
        let owner_key = SigningKey::from_bytes(&[1u8; 32]);
        let owner_verifying_key = owner_key.verifying_key();

        let config = AuthorizedConfigurationV1::new(
            river_core::room_state::configuration::Configuration::default(),
            &owner_key,
        );

        let member1_key = SigningKey::from_bytes(&[2u8; 32]);
        let member2_key = SigningKey::from_bytes(&[3u8; 32]);
        let member3_key = SigningKey::from_bytes(&[4u8; 32]);

        let member1_verifying_key = member1_key.verifying_key();
        let member2_verifying_key = member2_key.verifying_key();
        let member3_verifying_key = member3_key.verifying_key();

        let owner_id = owner_verifying_key.into();

        let member1 = river_core::room_state::member::Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member1_verifying_key,
        };

        let member2 = river_core::room_state::member::Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member2_verifying_key,
        };

        let member3 = river_core::room_state::member::Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member3_verifying_key,
        };

        let authorized_member1 =
            river_core::room_state::member::AuthorizedMember::new(member1, &owner_key);
        let authorized_member2 =
            river_core::room_state::member::AuthorizedMember::new(member2, &owner_key);
        let authorized_member3 =
            river_core::room_state::member::AuthorizedMember::new(member3, &owner_key);

        let members = river_core::room_state::member::MembersV1 {
            members: vec![authorized_member1, authorized_member2, authorized_member3],
        };

        let room_state = ChatRoomStateV1 {
            configuration: config,
            bans: river_core::room_state::ban::BansV1::default(),
            members,
            member_info: river_core::room_state::member_info::MemberInfoV1::default(),
            secrets: river_core::room_state::secret::RoomSecretsV1::default(),
            recent_messages: river_core::room_state::message::MessagesV1::default(),
            upgrade: river_core::room_state::upgrade::OptionalUpgradeV1(None),
            ..Default::default()
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        Self {
            room_state,
            parameters,
            owner_key,
        }
    }

    #[allow(dead_code)]
    pub fn get_member_key(node_index: u8) -> SigningKey {
        match node_index {
            1 => SigningKey::from_bytes(&[2u8; 32]),
            2 => SigningKey::from_bytes(&[3u8; 32]),
            3 => SigningKey::from_bytes(&[4u8; 32]),
            _ => panic!("Invalid node index: {}", node_index),
        }
    }
}

pub fn river_states_equal(a: &ChatRoomStateV1, b: &ChatRoomStateV1) -> bool {
    a.configuration == b.configuration
        && a.bans == b.bans
        && a.members == b.members
        && a.member_info == b.member_info
        && a.recent_messages == b.recent_messages
        && a.upgrade == b.upgrade
}

pub async fn deploy_room_contract(
    client: &mut WebApi,
    initial_room_state: ChatRoomStateV1,
    parameters: &ChatRoomParametersV1,
    subscribe: bool,
) -> Result<ContractKey> {
    let mut path_to_code = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path_to_code.pop(); // go up from room-contract
    path_to_code.pop(); // go up from contracts to river root

    println!(
        "Loading River room contract from project root: {:?}",
        path_to_code
    );
    println!("Target directory: {:?}", std::env::var("CARGO_TARGET_DIR"));

    let validation_result = initial_room_state.verify(&initial_room_state, parameters);
    match validation_result {
        Ok(_) => println!("[VALIDATION] Initial room state validation completed successfully"),
        Err(e) => {
            println!("[VALIDATION] Initial room state validation failed: {}", e);
            return Err(anyhow::anyhow!("Invalid initial room state: {}", e));
        }
    }

    let mut params_bytes = Vec::new();
    ciborium::ser::into_writer(parameters, &mut params_bytes)?;
    let params = Parameters::from(params_bytes);

    let container = load_contract(&path_to_code, params).map_err(|e| {
        println!("Failed to load River contract: {}", e);
        e
    })?;
    let contract_key = container.key();
    println!("River contract loaded with key: {:?}", contract_key);

    let mut state_bytes = Vec::new();
    ciborium::ser::into_writer(&initial_room_state, &mut state_bytes)?;

    let deserialized_state: ChatRoomStateV1 = ciborium::de::from_reader(state_bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("State deserialization failed: {}", e))?;

    if deserialized_state != initial_room_state {
        return Err(anyhow::anyhow!("Deserialized state doesn't match original"));
    }

    let wrapped_state = WrappedState::new(state_bytes);

    client
        .send(ClientRequest::ContractOp(ContractRequest::Put {
            contract: container,
            state: wrapped_state,
            related_contracts: RelatedContracts::new(),
            subscribe,
            blocking_subscribe: false,
        }))
        .await?;

    wait_for_put_response(client, &contract_key).await
}

pub async fn subscribe_to_contract(client: &mut WebApi, key: ContractKey) -> Result<()> {
    println!("Starting subscribe_to_contract for key: {}", key);

    println!("Sending Subscribe request to WebSocket...");
    let send_result = client
        .send(ClientRequest::ContractOp(ContractRequest::Subscribe {
            key: *key.id(), // Subscribe uses ContractInstanceId
            summary: None,
        }))
        .await;

    match send_result {
        Ok(()) => {
            println!("Subscribe request sent successfully to WebSocket");
        }
        Err(e) => {
            println!("Failed to send Subscribe request: {}", e);
            return Err(e.into());
        }
    }

    println!("Now waiting for SubscribeResponse via WebSocket...");
    let wait_result = wait_for_subscribe_response(client, &key).await;

    match &wait_result {
        Ok(()) => {
            println!("wait_for_subscribe_response completed successfully");
        }
        Err(e) => {
            println!("wait_for_subscribe_response failed: {}", e);
        }
    }

    wait_result
}

// Contract compilation constants
const WASM_TARGET: &str = "wasm32-unknown-unknown";
const PATH_TO_CONTRACT: &str = "contracts/room-contract";
const WASM_FILE_NAME: &str = "room-contract";
const CONTRACT_FEATURES: &str = "contract,freenet-main-contract";

// Timeout constants
const UPDATE_RESPONSE_TIMEOUT_SECS: u64 = 30;
const GET_RESPONSE_TIMEOUT_SECS: u64 = 45;
const SUBSCRIBE_RESPONSE_TIMEOUT_SECS: u64 = 60;
const PUT_RESPONSE_TIMEOUT_SECS: u64 = 60;

pub fn load_contract(
    contract_path: &std::path::PathBuf,
    params: Parameters<'static>,
) -> anyhow::Result<ContractContainer> {
    let contract_code = compile_contract(contract_path)?;
    println!(
        "Contract compiled successfully, {} bytes",
        contract_code.len()
    );

    let contract_bytes = WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(contract_code)),
        params,
    );
    let contract = ContractContainer::Wasm(ContractWasmAPIVersion::V1(contract_bytes));
    Ok(contract)
}

fn compile_contract(contract_path: &std::path::PathBuf) -> anyhow::Result<Vec<u8>> {
    println!("module path: {contract_path:?}");
    let target = std::env::var("CARGO_TARGET_DIR")
        .map_err(|_| anyhow::anyhow!("CARGO_TARGET_DIR should be set"))?;
    println!("trying to compile the test contract, target: {target}");

    compile_rust_wasm_lib(
        &BuildToolConfig {
            features: Some(CONTRACT_FEATURES.to_string()),
            package_type: PackageType::Contract,
            debug: false,
        },
        &contract_path.join(PATH_TO_CONTRACT),
    )?;

    let output_file = std::path::Path::new(&target)
        .join(WASM_TARGET)
        .join("release")
        .join(WASM_FILE_NAME.replace('-', "_"))
        .with_extension("wasm");
    println!("output file: {output_file:?}");
    Ok(std::fs::read(output_file)?)
}

#[derive(Clone, Debug)]
struct BuildToolConfig {
    features: Option<String>,
    package_type: PackageType,
    debug: bool,
}

#[derive(Default, Debug, Clone, Copy)]
enum PackageType {
    #[default]
    Contract,
}

impl PackageType {
    fn feature(&self) -> &'static str {
        match self {
            PackageType::Contract => "freenet-main-contract",
        }
    }
}

fn compile_options(cli_config: &BuildToolConfig) -> impl Iterator<Item = String> {
    let release: &[&str] = if cli_config.debug {
        &[]
    } else {
        &["--release"]
    };
    let feature_list = cli_config
        .features
        .iter()
        .flat_map(|s| {
            s.split(',')
                .filter(|p| *p != cli_config.package_type.feature())
        })
        .chain([cli_config.package_type.feature()]);
    let features = [
        "--features".to_string(),
        feature_list.collect::<Vec<_>>().join(","),
    ];
    features
        .into_iter()
        .chain(release.iter().map(|s| s.to_string()))
}

fn compile_rust_wasm_lib(
    cli_config: &BuildToolConfig,
    work_dir: &std::path::Path,
) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    use std::process::{Command, Stdio};

    const RUST_TARGET_ARGS: &[&str] = &["build", "--lib", "--target"];
    let comp_opts = compile_options(cli_config).collect::<Vec<_>>();
    let cmd_args = if std::io::stdout().is_terminal() && std::io::stderr().is_terminal() {
        RUST_TARGET_ARGS
            .iter()
            .copied()
            .chain([WASM_TARGET, "--color", "always"])
            .chain(comp_opts.iter().map(|s| s.as_str()))
            .collect::<Vec<_>>()
    } else {
        RUST_TARGET_ARGS
            .iter()
            .copied()
            .chain([WASM_TARGET])
            .chain(comp_opts.iter().map(|s| s.as_str()))
            .collect::<Vec<_>>()
    };

    let package_type = cli_config.package_type;
    println!("Compiling {package_type:?} with rust");

    println!("Running command: cargo {}", cmd_args.join(" "));
    println!("Working directory: {:?}", work_dir);

    let mut child = Command::new("cargo");
    child
        .args(&cmd_args)
        .current_dir(work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = child.spawn().map_err(|e| {
        eprintln!("Error while executing cargo command: {e}");
        anyhow::anyhow!("Error while executing cargo command: {e}")
    })?;
    pipe_std_streams(child)?;
    Ok(())
}

fn pipe_std_streams(mut child: std::process::Child) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};

    let c_stdout = child.stdout.take().expect("Failed to open command stdout");
    let c_stderr = child.stderr.take().expect("Failed to open command stderr");

    let write_child_stderr = move || -> anyhow::Result<()> {
        let mut stderr = std::io::stderr();
        let reader = std::io::BufReader::new(c_stderr);
        for line in reader.lines() {
            let line = line?;
            writeln!(stderr, "{line}")?;
        }
        Ok(())
    };

    let write_child_stdout = move || -> anyhow::Result<()> {
        let mut stdout = std::io::stdout();
        let reader = std::io::BufReader::new(c_stdout);
        for line in reader.lines() {
            let line = line?;
            writeln!(stdout, "{line}")?;
        }
        Ok(())
    };
    std::thread::spawn(write_child_stdout);
    std::thread::spawn(write_child_stderr);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    anyhow::bail!("exit with status: {status}");
                }
                break;
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(err) => {
                return Err(err.into());
            }
        }
    }

    Ok(())
}

pub async fn send_test_message(
    client: &mut WebApi,
    key: ContractKey,
    room_state: &ChatRoomStateV1,
    parameters: &ChatRoomParametersV1,
    message_content: String,
    signing_key: &SigningKey,
) -> Result<()> {
    println!("--> [UPDATE] Sending test message: '{}'", message_content);

    let message = river_core::room_state::message::MessageV1 {
        room_owner: parameters.owner_id(),
        author: signing_key.verifying_key().into(),
        content: river_core::room_state::message::RoomMessageBody::public(message_content.clone()),
        time: std::time::SystemTime::now(),
    };

    let auth_message =
        river_core::room_state::message::AuthorizedMessageV1::new(message, signing_key);

    let delta = river_core::room_state::ChatRoomStateV1Delta {
        recent_messages: Some(vec![auth_message.clone()]),
        ..Default::default()
    };

    let mut test_state = room_state.clone();
    test_state
        .apply_delta(room_state, parameters, &Some(delta.clone()))
        .map_err(|e| anyhow::anyhow!("Failed to apply message delta locally: {:?}", e))?;

    let mut delta_bytes = Vec::new();
    ciborium::ser::into_writer(&delta, &mut delta_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to serialize delta: {}", e))?;

    let update_request = ContractRequest::Update {
        key,
        data: UpdateData::Delta(StateDelta::from(delta_bytes)),
    };

    client
        .send(ClientRequest::ContractOp(update_request))
        .await?;
    println!("--> [UPDATE] Message delta sent to network");

    Ok(())
}

pub async fn update_room_state_delta(
    client: &mut WebApi,
    key: ContractKey,
    delta: river_core::room_state::ChatRoomStateV1Delta,
) -> Result<()> {
    let mut delta_bytes = Vec::new();
    ciborium::ser::into_writer(&delta, &mut delta_bytes)?;
    client
        .send(ClientRequest::ContractOp(ContractRequest::Update {
            key,
            data: UpdateData::Delta(StateDelta::from(delta_bytes)),
        }))
        .await?;
    Ok(())
}

pub async fn wait_for_update_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<()> {
    let response = tokio::time::timeout(
        Duration::from_secs(UPDATE_RESPONSE_TIMEOUT_SECS),
        client.recv(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Update response timeout after {}s",
            UPDATE_RESPONSE_TIMEOUT_SECS
        )
    })??;

    match response {
        HostResponse::ContractResponse(
            freenet_stdlib::client_api::ContractResponse::UpdateResponse { key, .. },
        ) => {
            if &key == contract_key {
                println!(
                    "[UPDATE] Update response received for contract: {:.8}",
                    key.id()
                );
                Ok(())
            } else {
                Err(anyhow::anyhow!("Update response for wrong contract"))
            }
        }
        HostResponse::ContractResponse(
            freenet_stdlib::client_api::ContractResponse::UpdateNotification { key, .. },
        ) => {
            if &key == contract_key {
                println!(
                    "[UPDATE] Update notification received for contract: {:.8}",
                    key.id()
                );
                Ok(())
            } else {
                Err(anyhow::anyhow!("Update notification for wrong contract"))
            }
        }
        other => Err(anyhow::anyhow!("Unexpected response: {:?}", other)),
    }
}

pub async fn get_all_room_states(
    clients: &mut [&mut WebApi],
    key: ContractKey,
) -> Result<Vec<ChatRoomStateV1>> {
    let mut states = Vec::new();

    for (index, client) in clients.iter_mut().enumerate() {
        client
            .send(ClientRequest::ContractOp(ContractRequest::Get {
                key: *key.id(), // GET uses ContractInstanceId
                return_contract_code: true,
                subscribe: false,
                blocking_subscribe: false,
            }))
            .await?;

        let state_result = tokio::time::timeout(
            Duration::from_secs(GET_RESPONSE_TIMEOUT_SECS),
            wait_for_get_response(client, &key),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Node{} get request timed out after {}s",
                index + 1,
                GET_RESPONSE_TIMEOUT_SECS
            )
        })?;

        let state = state_result
            .map_err(|e| anyhow::anyhow!("Failed to get node{} state: {}", index + 1, e))?;
        states.push(state);
    }

    Ok(states)
}

async fn wait_for_put_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<ContractKey> {
    let deadline = std::time::Instant::now() + Duration::from_secs(PUT_RESPONSE_TIMEOUT_SECS);

    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            return Err(anyhow::anyhow!(
                "Put response timeout after {}s",
                PUT_RESPONSE_TIMEOUT_SECS
            ));
        }

        let response = tokio::time::timeout(remaining, client.recv())
            .await
            .map_err(|_| {
                anyhow::anyhow!("Put response timeout after {}s", PUT_RESPONSE_TIMEOUT_SECS)
            })??;

        if let HostResponse::ContractResponse(
            freenet_stdlib::client_api::ContractResponse::PutResponse { key, .. },
        ) = response
        {
            if &key == contract_key {
                return Ok(key);
            }
        }
    }
}

async fn wait_for_subscribe_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(SUBSCRIBE_RESPONSE_TIMEOUT_SECS);

    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            return Err(anyhow::anyhow!(
                "Subscribe response timeout after {}s",
                SUBSCRIBE_RESPONSE_TIMEOUT_SECS
            ));
        }

        let response = tokio::time::timeout(remaining, client.recv())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Subscribe response timeout after {}s",
                    SUBSCRIBE_RESPONSE_TIMEOUT_SECS
                )
            })??;

        match response {
            HostResponse::ContractResponse(
                freenet_stdlib::client_api::ContractResponse::SubscribeResponse {
                    key,
                    subscribed,
                    ..
                },
            ) => {
                if &key == contract_key {
                    if !subscribed {
                        return Err(anyhow::anyhow!("Subscription request rejected"));
                    }
                    return Ok(());
                }
            }
            // UpdateNotification can arrive before SubscribeResponse - continue waiting
            HostResponse::ContractResponse(
                freenet_stdlib::client_api::ContractResponse::UpdateNotification { .. },
            ) => {
                continue;
            }
            _ => {
                // Ignore other messages and continue waiting
                continue;
            }
        }
    }
}

async fn wait_for_get_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<ChatRoomStateV1> {
    loop {
        let response = client.recv().await?;
        if let HostResponse::ContractResponse(
            freenet_stdlib::client_api::ContractResponse::GetResponse { key, state, .. },
        ) = response
        {
            if &key == contract_key {
                let room_state: ChatRoomStateV1 = ciborium::de::from_reader(state.as_ref())?;
                return Ok(room_state);
            }
        }
    }
}
