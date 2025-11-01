use std::{
    path::{Path, PathBuf},
    process::Output,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use assert_cmd::cargo::cargo_bin_cmd;
use freenet_test_network::{BuildProfile, FreenetBinary, TestNetwork};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;
use tokio::time::sleep;

#[derive(Deserialize)]
struct CreateRoomOutput {
    owner_key: String,
}

#[derive(Deserialize)]
struct InviteCreateOutput {
    invitation_code: String,
}

#[derive(Deserialize)]
struct InviteAcceptOutput {
    room_owner_key: String,
}

fn node_url(peer: &freenet_test_network::TestPeer) -> String {
    format!("{}?encodingProtocol=native", peer.ws_url())
}

async fn run_riverctl(
    config_dir: &Path,
    node_url: &str,
    args: &[&str],
) -> Result<String> {
    let mut full_args = vec![
        "--node-url".to_string(),
        node_url.to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    full_args.extend(args.iter().map(|s| s.to_string()));

    let config_dir = config_dir.to_owned();
    let args_clone = full_args.clone();

    let output: Output = tokio::task::spawn_blocking(move || -> Result<Output> {
        let mut cmd = cargo_bin_cmd!("riverctl");
        cmd.env("RIVER_CONFIG_DIR", &config_dir)
            .args(&args_clone);
        cmd.output()
            .context("Failed to execute riverctl command")
            .map_err(Into::into)
    })
    .await
    .context("Failed to join riverctl command task")??;

    if !output.status.success() {
        return Err(anyhow!(
            "riverctl command failed: {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }

    let stdout = String::from_utf8(output.stdout)
        .context("riverctl command produced non-UTF8 stdout")?;
    let mut start_index = None;
    let mut closing = '\0';
    for (idx, ch) in stdout.char_indices() {
        match ch {
            '{' => {
                start_index = Some(idx);
                closing = '}';
                break;
            }
            '[' => {
                start_index = Some(idx);
                closing = ']';
                break;
            }
            _ => continue,
        }
    }

    let start = start_index.ok_or_else(|| anyhow!("Failed to find JSON payload in output:\n{}", stdout))?;
    let end = stdout
        .rfind(closing)
        .ok_or_else(|| anyhow!("Failed to find JSON payload in output:\n{}", stdout))?;

    Ok(stdout[start..=end].to_string())
}

fn decode_plaintext_messages(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(|entry| {
            entry
                .get("content")
                .and_then(|content| content.get("Public"))
                .and_then(|public| public.get("plaintext"))
                .and_then(|text| text.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

fn freenet_core_workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../freenet-core/main")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires freenet-core workspace and is currently unstable"]
async fn river_message_flow_over_freenet() -> Result<()> {
    let freenet_core = freenet_core_workspace();
    if !freenet_core.exists() {
        anyhow::bail!(
            "Expected freenet-core workspace at {}",
            freenet_core.display()
        );
    }

    let network = TestNetwork::builder()
        .gateways(1)
        .peers(2)
        .binary(FreenetBinary::Workspace {
            path: freenet_core.clone(),
            profile: BuildProfile::Debug,
        })
        .require_connectivity(1.0)
        .connectivity_timeout(Duration::from_secs(120))
        .preserve_temp_dirs_on_failure(true)
        .build()
        .await
        .context("Failed to start Freenet test network")?;

    let alice_dir = TempDir::new().context("Failed to create Alice config dir")?;
    let bob_dir = TempDir::new().context("Failed to create Bob config dir")?;

    let peer0 = network.peer(0);
    let peer1 = network.peer(1);
    let peer0_url = node_url(peer0);
    let peer1_url = node_url(peer1);

    let create_stdout = run_riverctl(
        alice_dir.path(),
        &peer0_url,
        &["room", "create", "--name", "River Test Room", "--nickname", "Alice"],
    )
    .await
    .context("riverctl room create failed")?;
    let create_output: CreateRoomOutput =
        serde_json::from_str(&create_stdout).context("Failed to parse room create output")?;

    let invite_stdout = run_riverctl(
        alice_dir.path(),
        &peer0_url,
        &["invite", "create", &create_output.owner_key],
    )
    .await
    .context("riverctl invite create failed")?;
    let invite_output: InviteCreateOutput =
        serde_json::from_str(&invite_stdout).context("Failed to parse invite create output")?;

    let accept_stdout = run_riverctl(
        bob_dir.path(),
        &peer1_url,
        &[
            "invite",
            "accept",
            &invite_output.invitation_code,
            "--nickname",
            "Bob",
        ],
    )
    .await
    .context("riverctl invite accept failed")?;
    let accept_output: InviteAcceptOutput =
        serde_json::from_str(&accept_stdout).context("Failed to parse invite accept output")?;

    assert_eq!(
        accept_output.room_owner_key, create_output.owner_key,
        "Invite acceptance should reference the same room owner key"
    );

    sleep(Duration::from_secs(5)).await;

    let alice_message = "Hello from Alice!";
    run_riverctl(
        alice_dir.path(),
        &peer0_url,
        &["message", "send", &create_output.owner_key, alice_message],
    )
    .await
    .context("riverctl message send (Alice) failed")?;

    sleep(Duration::from_secs(5)).await;

    let bob_message = "Hello from Bob!";
    run_riverctl(
        bob_dir.path(),
        &peer1_url,
        &["message", "send", &create_output.owner_key, bob_message],
    )
    .await
    .context("riverctl message send (Bob) failed")?;

    sleep(Duration::from_secs(10)).await;

    let alice_list_stdout = run_riverctl(
        alice_dir.path(),
        &peer0_url,
        &["message", "list", &create_output.owner_key],
    )
    .await
    .context("riverctl message list (Alice) failed")?;
    let alice_messages: Vec<Value> = serde_json::from_str(&alice_list_stdout)
        .context("Failed to parse Alice message list output")?;

    let bob_list_stdout = run_riverctl(
        bob_dir.path(),
        &peer1_url,
        &["message", "list", &create_output.owner_key],
    )
    .await
    .context("riverctl message list (Bob) failed")?;
    let bob_messages: Vec<Value> = serde_json::from_str(&bob_list_stdout)
        .context("Failed to parse Bob message list output")?;

    let alice_plaintexts = decode_plaintext_messages(&alice_messages);
    let bob_plaintexts = decode_plaintext_messages(&bob_messages);

    anyhow::ensure!(
        alice_plaintexts.contains(&alice_message.to_string()),
        "Alice did not see her own message. Messages: {:?}",
        alice_plaintexts
    );
    anyhow::ensure!(
        alice_plaintexts.contains(&bob_message.to_string()),
        "Alice did not see Bob's message. Messages: {:?}",
        alice_plaintexts
    );
    anyhow::ensure!(
        bob_plaintexts.contains(&alice_message.to_string()),
        "Bob did not see Alice's message. Messages: {:?}",
        bob_plaintexts
    );
    anyhow::ensure!(
        bob_plaintexts.contains(&bob_message.to_string()),
        "Bob did not see his own message. Messages: {:?}",
        bob_plaintexts
    );

    Ok(())
}
