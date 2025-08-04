pub mod test_utils;

use anyhow::Result;
use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, WebApi, HostResponse},
    prelude::*,
};
use river_common::{
    room_state::{
        configuration::AuthorizedConfigurationV1,
        ChatRoomParametersV1,
    },
    ChatRoomStateV1,
};
use std::time::Duration;
pub use test_utils::*;

#[derive(Debug, Clone)]
pub struct RoomTestState {
    pub room_state: ChatRoomStateV1,
    pub parameters: ChatRoomParametersV1,
    pub owner_key: SigningKey,
}

impl RoomTestState {
    pub fn new_test_room() -> Self {
        let owner_key = SigningKey::from_bytes(&[1u8; 32]);
        let owner_verifying_key = owner_key.verifying_key();
        
        let config = AuthorizedConfigurationV1::new(
            river_common::room_state::configuration::Configuration::default(), 
            &owner_key
        );
        let room_state = ChatRoomStateV1 {
            configuration: config,
            bans: river_common::room_state::ban::BansV1::default(),
            members: river_common::room_state::member::MembersV1::default(),
            member_info: river_common::room_state::member_info::MemberInfoV1::default(),
            recent_messages: river_common::room_state::message::MessagesV1::default(),
            upgrade: river_common::room_state::upgrade::OptionalUpgradeV1(None),
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
    
    println!("Loading River room contract from project root: {:?}", path_to_code);
    println!("Target directory: {:?}", std::env::var("CARGO_TARGET_DIR"));
    
    println!("[VALIDATION] Validating initial room state structure and signatures...");
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
    
    println!("[SERIALIZATION] Testing state serialization and deserialization...");
    let serialized_size = state_bytes.len();
    println!("[SERIALIZATION] State serialized to {} bytes", serialized_size);
    
    let deserialized_state: ChatRoomStateV1 = ciborium::de::from_reader(state_bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("State deserialization failed: {}", e))?;
    println!("[SERIALIZATION] State deserialization completed");
    
    if deserialized_state != initial_room_state {
        return Err(anyhow::anyhow!("Deserialized state doesn't match original"));
    }
    println!("[SERIALIZATION] Round-trip validation passed - serialized and deserialized states match");
    
    let wrapped_state = WrappedState::new(state_bytes);

    client
        .send(ClientRequest::ContractOp(ContractRequest::Put {
            contract: container,
            state: wrapped_state,
            related_contracts: RelatedContracts::new(),
            subscribe,
        }))
        .await?;
    
    wait_for_put_response(client, &contract_key).await
}

pub async fn subscribe_to_contract(client: &mut WebApi, key: ContractKey) -> Result<()> {
    client
        .send(ClientRequest::ContractOp(ContractRequest::Subscribe {
            key,
            summary: None,
        }))
        .await?;
    wait_for_subscribe_response(client, &key).await
}

pub async fn get_contract_state(
    client: &mut WebApi,
    key: ContractKey,
    fetch_contract: bool,
) -> Result<ChatRoomStateV1> {
    client
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key,
            return_contract_code: fetch_contract,
            subscribe: false,
        }))
        .await?;
    wait_for_get_response(client, &key).await
}

const WASM_TARGET: &str = "wasm32-unknown-unknown";
const PATH_TO_CONTRACT: &str = "contracts/room-contract";
const WASM_FILE_NAME: &str = "room-contract";

pub fn load_contract(
    contract_path: &std::path::PathBuf,
    params: Parameters<'static>,
) -> anyhow::Result<ContractContainer> {
    let contract_code = compile_contract(contract_path)?;
    println!("Contract compiled successfully, {} bytes", contract_code.len());
    
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
            features: Some("contract,freenet-main-contract".to_string()),
            package_type: PackageType::Contract,
            debug: false, // Compile in release mode to reduce size
        },
        &contract_path.join(PATH_TO_CONTRACT),
    )?;

    let output_file = std::path::Path::new(&target)
        .join(WASM_TARGET)
        .join("release") // Use release build directory
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

fn compile_rust_wasm_lib(cli_config: &BuildToolConfig, work_dir: &std::path::Path) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    use std::io::IsTerminal;

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
    
    // Print the exact command being run
    println!("Running command: cargo {}", cmd_args.join(" "));
    println!("Working directory: {:?}", work_dir);
    
    let mut child = Command::new("cargo");
    child
        .args(&cmd_args)
        .current_dir(work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        
    let child = child.spawn()
        .map_err(|e| {
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

/// Update room state using delta (more realistic like CLI)
pub async fn send_test_message(
    client: &mut WebApi,
    key: ContractKey,
    room_state: &ChatRoomStateV1,
    parameters: &ChatRoomParametersV1,
    message_content: String,
    signing_key: &SigningKey,
) -> Result<()> {
    println!("[UPDATE] Sending test message: '{}'", message_content);
    
    let message = river_common::room_state::message::MessageV1 {
        room_owner: parameters.owner_id(),
        author: signing_key.verifying_key().into(),
        content: message_content.clone(),
        time: std::time::SystemTime::now(),
    };
    
    let auth_message = river_common::room_state::message::AuthorizedMessageV1::new(message, signing_key);
    
    let delta = river_common::room_state::ChatRoomStateV1Delta {
        recent_messages: Some(vec![auth_message.clone()]),
        ..Default::default()
    };
    
    println!("[UPDATE] Created message delta with {} chars", message_content.len());
    
    let mut test_state = room_state.clone();
    test_state.apply_delta(room_state, parameters, &Some(delta.clone()))
        .map_err(|e| anyhow::anyhow!("Failed to apply message delta locally: {:?}", e))?;
    println!("[UPDATE] Delta validation passed - message can be applied");
    
    let mut delta_bytes = Vec::new();
    ciborium::ser::into_writer(&delta, &mut delta_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to serialize delta: {}", e))?;
    println!("[UPDATE] Serialized delta to {} bytes", delta_bytes.len());
    
    let update_request = ContractRequest::Update {
        key,
        data: UpdateData::Delta(StateDelta::from(delta_bytes)),
    };
    
    client.send(ClientRequest::ContractOp(update_request)).await?;
    println!("[UPDATE] Message delta sent to network");
    
    Ok(())
}

pub async fn update_room_state(
    client: &mut WebApi,
    key: ContractKey,
    delta: ChatRoomStateV1,
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

/// Wait for update response (like CLI does)
pub async fn wait_for_update_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<()> {
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        client.recv()
    ).await.map_err(|_| anyhow::anyhow!("Update response timeout after 30s"))??;

    match response {
        HostResponse::ContractResponse(
            freenet_stdlib::client_api::ContractResponse::UpdateResponse { key, .. }
        ) => {
            if &key == contract_key {
                println!("[UPDATE] Update response received for contract: {:.8}", key.id());
                Ok(())
            } else {
                Err(anyhow::anyhow!("Update response for wrong contract"))
            }
        }
        HostResponse::ContractResponse(
            freenet_stdlib::client_api::ContractResponse::UpdateNotification { key, .. }
        ) => {
            if &key == contract_key {
                println!("[UPDATE] Update notification received for contract: {:.8}", key.id());
                Ok(())
            } else {
                Err(anyhow::anyhow!("Update notification for wrong contract"))
            }
        }
        other => Err(anyhow::anyhow!("Unexpected response: {:?}", other)),
    }
}

pub async fn get_all_room_states(
    client_gw: &mut WebApi,
    client_node1: &mut WebApi,
    client_node2: &mut WebApi,
    key: ContractKey,
) -> Result<(ChatRoomStateV1, ChatRoomStateV1, ChatRoomStateV1)> {
    client_gw
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key,
            return_contract_code: false,
            subscribe: false,
        }))
        .await?;

    client_node1
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key,
            return_contract_code: false,
            subscribe: false,
        }))
        .await?;

    client_node2
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key,
            return_contract_code: false,
            subscribe: false,
        }))
        .await?;

    let state_gw = tokio::time::timeout(
        Duration::from_secs(45),
        wait_for_get_response(client_gw, &key),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Gateway get request timed out after 45s"))?;

    let state_node1 = tokio::time::timeout(
        Duration::from_secs(45),
        wait_for_get_response(client_node1, &key),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Node1 get request timed out after 45s"))?;

    let state_node2 = tokio::time::timeout(
        Duration::from_secs(45),
        wait_for_get_response(client_node2, &key),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Node2 get request timed out after 45s"))?;

    let room_gw = state_gw.map_err(|e| anyhow::anyhow!("Failed to get gateway state: {}", e))?;
    let room_node1 = state_node1.map_err(|e| anyhow::anyhow!("Failed to get node1 state: {}", e))?;
    let room_node2 = state_node2.map_err(|e| anyhow::anyhow!("Failed to get node2 state: {}", e))?;

    Ok((room_gw, room_node1, room_node2))
}

pub async fn get_room_states_two_nodes(
    client_gw: &mut WebApi,
    client_node1: &mut WebApi,
    key: ContractKey,
) -> Result<(ChatRoomStateV1, ChatRoomStateV1)> {
    client_gw
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key,
            return_contract_code: false,
            subscribe: false,
        }))
        .await?;

    client_node1
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key,
            return_contract_code: false,
            subscribe: false,
        }))
        .await?;

    let state_gw = tokio::time::timeout(
        Duration::from_secs(45),
        wait_for_get_response(client_gw, &key),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Gateway get request timed out after 45s"))?;

    let state_node1 = tokio::time::timeout(
        Duration::from_secs(45),
        wait_for_get_response(client_node1, &key),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Node1 get request timed out after 45s"))?;

    let room_gw = state_gw.map_err(|e| anyhow::anyhow!("Failed to get gateway state: {}", e))?;
    let room_node1 = state_node1.map_err(|e| anyhow::anyhow!("Failed to get node1 state: {}", e))?;

    Ok((room_gw, room_node1))
}

async fn wait_for_put_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<ContractKey> {
    loop {
        let response = client.recv().await?;
        match response {
            freenet_stdlib::client_api::HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    freenet_stdlib::client_api::ContractResponse::PutResponse { key, .. } => {
                        if &key == contract_key {
                            return Ok(key);
                        }
                    }
                    _ => continue,
                }
            }
            _ => continue,
        }
    }
}

async fn wait_for_subscribe_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<()> {
    loop {
        let response = client.recv().await?;
        match response {
            freenet_stdlib::client_api::HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    freenet_stdlib::client_api::ContractResponse::SubscribeResponse { key, .. } => {
                        if &key == contract_key {
                            return Ok(());
                        }
                    }
                    _ => continue,
                }
            }
            _ => continue,
        }
    }
}

async fn wait_for_get_response(
    client: &mut WebApi,
    contract_key: &ContractKey,
) -> Result<ChatRoomStateV1> {
    loop {
        let response = client.recv().await?;
        match response {
            freenet_stdlib::client_api::HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    freenet_stdlib::client_api::ContractResponse::GetResponse { 
                        key, 
                        state, 
                        .. 
                    } => {
                        if &key == contract_key {
                            let room_state: ChatRoomStateV1 = 
                                ciborium::de::from_reader(state.as_ref())?;
                            return Ok(room_state);
                        }
                    }
                    _ => continue,
                }
            }
            _ => continue,
        }
    }
}