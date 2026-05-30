use crate::config::Config;
use crate::output::OutputFormat;
use crate::storage::Storage;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
};
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion,
    Parameters, UpdateData, WrappedContract, WrappedState,
};
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::privacy::{RoomDisplayMetadata, SealedBytes};
use river_core::room_state::ChatRoomStateV1Delta;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tracing::{debug, info, warn};

// Load the room contract WASM copied by build.rs
const ROOM_CONTRACT_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/room_contract.wasm"));

/// Timeout for the GET against the current room contract.
const CURRENT_GET_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-probe timeout when searching older contract generations (freenet/river#292).
/// Kept short because a backward search may probe many generations; an existing
/// contract responds quickly, only an absent one runs the timeout down.
const LEGACY_PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Timeout for a single hop while following an `OptionalUpgradeV1` pointer chain.
const UPGRADE_HOP_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum upgrade-pointer hops to follow before giving up — guards against a
/// cyclic or runaway chain.
const MAX_UPGRADE_HOPS: usize = 32;

/// Decide the next contract to follow from `state`'s upgrade pointer.
///
/// Returns `Some(next)` when `state` carries an `OptionalUpgradeV1` pointer to
/// a contract not yet in `visited` — and records it in `visited`. Returns
/// `None` when there is no pointer, or it targets an already-visited contract
/// (a self-pointer or a cycle). Pure; the network GET is the caller's job.
/// Extracted from `follow_upgrade_chain` so the cycle guard is unit-testable
/// without a node (freenet/river#292).
fn next_upgrade_hop(
    state: &ChatRoomStateV1,
    visited: &mut HashSet<ContractInstanceId>,
) -> Option<ContractInstanceId> {
    let authorized_upgrade = state.upgrade.0.as_ref()?;
    let next = ContractInstanceId::new(*authorized_upgrade.upgrade.new_chatroom_address.as_bytes());
    // `HashSet::insert` returns false if `next` was already present — a cycle.
    visited.insert(next).then_some(next)
}

/// Compute the contract key for a room from its owner verifying key.
/// This uses the current bundled WASM to ensure consistency.
pub fn compute_contract_key(owner_vk: &VerifyingKey) -> ContractKey {
    let params = ChatRoomParametersV1 { owner: *owner_vk };
    let params_bytes = {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&params, &mut buf).expect("Failed to serialize parameters");
        buf
    };
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code)
}

/// Resolve a message's human-readable body for display.
///
/// `effective_text` only yields text for `Text`/`Reply` bodies (and any edited
/// content). Other *public* content — notably join events (`content_type = 4`,
/// `EventContentV1`) — is not encrypted but carries no "text" field, so
/// `effective_text` returns `None`. Such content is decoded to its own display
/// string ("joined the room" for a join event) instead of being mislabeled as
/// `<encrypted>`. Only genuinely private (encrypted) bodies fall back to
/// `<encrypted>`.
///
/// Before this helper, riverctl rendered join events as
/// `[nickname]: <encrypted>` because the display path conflated "no text
/// content" with "encrypted".
pub(crate) fn message_display_text(
    room_state: &ChatRoomStateV1,
    msg: &river_core::room_state::message::AuthorizedMessageV1,
) -> String {
    room_state
        .recent_messages
        .effective_text(msg)
        .unwrap_or_else(|| {
            msg.message
                .content
                .decode_content()
                .map(|decoded| decoded.to_display_string())
                .unwrap_or_else(|| "<encrypted>".to_string())
        })
}

/// If `msg` is a reply, return `(target_author_name, truncated_preview)` for
/// display. Returns `None` for non-reply content (or content that fails to
/// decode). Mirrors the reply rendering in `riverctl message list` so the
/// monitor stream surfaces reply context too (it previously did not — a reply
/// rendered as a plain message). freenet/river — Rogue Worm report.
///
/// Shared with `riverctl message list` (cli/src/commands/message.rs) so the two
/// renderings can't drift.
pub(crate) fn reply_context(
    msg: &river_core::room_state::message::AuthorizedMessageV1,
) -> Option<(String, String)> {
    use river_core::room_state::content::{DecodedContent, CONTENT_TYPE_REPLY};
    if msg.message.content.content_type() != CONTENT_TYPE_REPLY {
        return None;
    }
    match msg.message.content.decode_content() {
        Some(DecodedContent::Reply(reply)) => {
            let preview: String = reply.target_content_preview.chars().take(50).collect();
            Some((reply.target_author_name, preview))
        }
        _ => None,
    }
}

/// Whether a message seen by a monitor stream is brand new, an edit of one
/// already emitted, or unchanged since last emitted.
#[derive(Debug, PartialEq, Eq)]
enum EmitKind {
    New,
    Edited,
    Unchanged,
}

/// Decide how to surface a message in a monitor stream. `seen` maps a message's
/// dedup key to the effective content last emitted for it; a changed content
/// for an already-seen key means the message was edited. This is the core of
/// the monitor's edit detection (it previously keyed on identity only and so
/// never re-emitted an edited message). freenet/river — Rogue Worm report.
fn classify_seen(seen: &HashMap<String, String>, key: &str, content: &str) -> EmitKind {
    match seen.get(key) {
        None => EmitKind::New,
        Some(prev) if prev == content => EmitKind::Unchanged,
        Some(_) => EmitKind::Edited,
    }
}

/// Stable dedup key for a message in a monitor stream: its signature-derived
/// `MessageId`, NOT `author:time`. The id is unique per message and stable
/// across edits (an edit is a separate action message; the original message's
/// signature never changes), so two distinct messages from the same author with
/// an identical timestamp cannot collide. Keying on `author:time` instead would
/// let such a collision flip-flop forever as a spurious "edit" now that we
/// compare effective content. freenet/river — PR #322 review.
fn monitor_seen_key(msg: &river_core::room_state::message::AuthorizedMessageV1) -> String {
    msg.id().0 .0.to_string()
}

/// Whether a monitor stream should emit a deletion event for a now-deleted
/// message. True only if the message was previously surfaced to the stream
/// (`seen`) and a deletion hasn't already been emitted for it
/// (`deleted_emitted`). The caller has already confirmed the message is
/// deleted. Keeping this pure makes the one-shot / only-if-shown semantics
/// unit-testable. freenet/river#323.
fn should_emit_deletion(
    seen: &HashMap<String, String>,
    deleted_emitted: &HashSet<String>,
    key: &str,
) -> bool {
    seen.contains_key(key) && !deleted_emitted.contains(key)
}

/// Choose the `member_info` nickname to publish when re-adding a member who
/// was pruned for inactivity (see [`ApiClient::build_rejoin_delta`]).
///
/// Restores the member's persisted nickname — sealed for a private room via
/// [`crate::private_room::seal_invitee_nickname`] — falling back to the generic
/// public `"Member"` placeholder when any of these hold:
/// - no nickname was persisted (rooms joined before the `self_nickname` field,
///   or an older `rooms.json`);
/// - a private room has no secret available, so sealing returns `None` (we must
///   never publish a plaintext nickname into a private room);
/// - the stored nickname's byte length exceeds the room's current
///   `max_nickname_size`. The contract's `MemberInfoV1::apply_delta` rejects the
///   ENTIRE rejoin delta (members + member_info together) when
///   `declared_len() > max_nickname_size`, so an over-long restored nickname
///   would block the member from rejoining at all. `declared_len()` is the
///   plaintext byte length for both public and sealed values, so comparing
///   `nick.len()` here matches the contract check exactly. The 6-byte `"Member"`
///   placeholder keeps the rejoin working (regression guard — Codex/skeptical
///   review of PR #321).
fn rejoin_preferred_nickname(
    room_state: &ChatRoomStateV1,
    signing_key: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
    self_nickname: Option<&str>,
) -> SealedBytes {
    let max_nickname_size = room_state.configuration.configuration.max_nickname_size;
    self_nickname
        .filter(|nick| nick.len() <= max_nickname_size)
        .and_then(|nick| {
            crate::private_room::seal_invitee_nickname(
                room_state,
                signing_key,
                invitation_secrets,
                nick,
            )
        })
        .unwrap_or_else(|| SealedBytes::public("Member".to_string().into_bytes()))
}

/// On-wire invitation artifact. **MUST stay byte-identical to the UI's
/// `ui::components::members::Invitation`** — both clients exchange these via
/// base58+CBOR and the encoded string is fingerprinted for processed-invite
/// dedup. Any new field here MUST also be added to the UI copy, and vice
/// versa. Filed against issue freenet/river#302 — see point 4 there for a
/// future consolidation pass into a single shared (non-WASM-compiled) type.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Invitation {
    pub room: VerifyingKey,
    pub invitee_signing_key: SigningKey,
    pub invitee: AuthorizedMember,
    /// The room's symmetric secrets, one `(version, secret)` per version the
    /// inviting member holds (issue freenet/river#302; the UI counterpart was
    /// added in #301). Lets the invitee decrypt a private room immediately on
    /// join, instead of being stuck on `[Encrypted: N bytes, vN]` until the
    /// room owner's chat-delegate back-fills an `encrypted_secrets` blob.
    /// Works even when a non-owner issues the invitation — the inviter
    /// already holds the secret; the room contract is untouched.
    ///
    /// Carried in plaintext, NOT ECIES-wrapped. That is not a confidentiality
    /// regression: the invitation already carries `invitee_signing_key` in
    /// the clear, so the whole artifact is a bearer credential — anyone who
    /// can read these bytes can already read everything the room secret
    /// protects. Plaintext also avoids decrypting attacker-influenced
    /// ciphertext on the join path (`river_core::ecies::decrypt` panics on a
    /// malformed blob, and the release build is `panic = "abort"`).
    ///
    /// Sorted ascending by version for deterministic CBOR encoding (the
    /// encoded string is fingerprinted for processed-invite dedup, so it must
    /// be stable across decode/re-encode cycles).
    ///
    /// Empty for public rooms and for invitations created before this field
    /// existed (`#[serde(default)]` keeps old links decodable).
    #[serde(default)]
    pub room_secrets: Vec<(u32, [u8; 32])>,
}

/// Hand-written `Debug` that REDACTS `room_secrets`. The derived `Debug` for
/// `[u8; 32]` is fully transparent, so `{:?}`-logging an `Invitation` (e.g.
/// `info!("...{:?}", invitation)`) would print every room-secret byte.
/// `room` and `invitee` are non-sensitive; `SigningKey`'s own `Debug` is
/// already non-exhaustive (it does not print the secret), so it is safe to
/// delegate to. Mirrors the UI's hand-written `Debug` in
/// `ui/src/components/members.rs` — keep the two in sync.
impl std::fmt::Debug for Invitation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invitation")
            .field("room", &self.room)
            .field("invitee_signing_key", &self.invitee_signing_key)
            .field("invitee", &self.invitee)
            .field(
                "room_secrets",
                &format_args!("<{} room secret(s) redacted>", self.room_secrets.len()),
            )
            .finish()
    }
}

pub struct ApiClient {
    web_api: Arc<Mutex<WebApi>>,
    #[allow(dead_code)]
    config: Config,
    storage: Storage,
}

impl ApiClient {
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub async fn new(node_url: &str, config: Config, config_dir: Option<&str>) -> Result<Self> {
        Self::new_with_signing_key_override(node_url, config, config_dir, None).await
    }

    /// Construct an [`ApiClient`] with an optional in-memory signing-key
    /// override. The override is propagated to [`Storage`] so every
    /// `get_room` resolves the signing key from the override rather than
    /// the per-room `signing_key_bytes`. See [`Storage::signing_key_override`]
    /// for the motivating scenario.
    pub async fn new_with_signing_key_override(
        node_url: &str,
        config: Config,
        config_dir: Option<&str>,
        signing_key_override: Option<SigningKey>,
    ) -> Result<Self> {
        // Use the URL as provided - it should already be in the correct format
        info!("Connecting to Freenet node at: {}", node_url);

        // Connect using tokio-tungstenite
        let (ws_stream, _) = connect_async(node_url)
            .await
            .map_err(|e| anyhow!("Failed to connect to WebSocket: {}", e))?;

        info!("WebSocket connected successfully");

        // Create WebApi instance
        let web_api = WebApi::start(ws_stream);

        let storage = Storage::new_with_override(config_dir, signing_key_override)?;

        Ok(Self {
            web_api: Arc::new(Mutex::new(web_api)),
            config,
            storage,
        })
    }

    pub async fn create_room(
        &self,
        name: String,
        nickname: String,
    ) -> Result<(VerifyingKey, ContractKey)> {
        info!("Creating room: {}", name);

        // Generate signing key for the room owner
        let signing_key =
            SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()));
        let owner_vk = signing_key.verifying_key();

        // Create initial room state
        let mut room_state = ChatRoomStateV1::default();

        // Set initial configuration
        let config = Configuration {
            owner_member_id: owner_vk.into(),
            display: RoomDisplayMetadata {
                name: SealedBytes::public(name.clone().into_bytes()),
                description: None,
            },
            ..Configuration::default()
        };
        room_state.configuration = AuthorizedConfigurationV1::new(config, &signing_key);

        // Add owner to member_info
        let owner_info = MemberInfo {
            member_id: owner_vk.into(),
            version: 0,
            preferred_nickname: SealedBytes::public(nickname.into_bytes()),
        };
        let authorized_owner_info = AuthorizedMemberInfo::new(owner_info, &signing_key);
        room_state
            .member_info
            .member_info
            .push(authorized_owner_info);

        // Generate contract key using ciborium for serialization (matching UI code)
        let parameters = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize parameters: {}", e))?;
            buf
        };

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        // Use the full ContractKey constructor that includes the code hash
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(params_bytes.clone()),
            &contract_code,
        );

        // Create contract container
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));

        // Create wrapped state using ciborium
        let state_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&room_state, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize room state: {}", e))?;
            buf
        };
        let wrapped_state = WrappedState::new(state_bytes);

        // Create PUT request - subscribe: true so we receive updates to our own room
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(put_request);

        // Send request
        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send PUT request: {}", e))?;

        // Wait for response with a more generous timeout to handle network delays
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for PUT response after 60 seconds")),
            };

        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::PutResponse { key } => {
                        info!("Room created successfully with contract key: {}", key.id());

                        // Verify the key matches what we expected
                        if key != contract_key {
                            return Err(anyhow!(
                                "Contract key mismatch: expected {}, got {}",
                                contract_key.id(),
                                key.id()
                            ));
                        }

                        // Store room info persistently
                        self.storage.add_room(
                            &owner_vk,
                            &signing_key,
                            room_state,
                            &contract_key,
                        )?;

                        Ok((owner_vk, contract_key))
                    }
                    ContractResponse::UpdateNotification { key, .. } => {
                        // When subscribing on PUT, we may receive an UpdateNotification first
                        // This indicates the PUT succeeded and we're now subscribed
                        info!(
                            "Room created (received subscription update) with contract key: {}",
                            key.id()
                        );

                        // Verify the key matches what we expected
                        if key != contract_key {
                            return Err(anyhow!(
                                "Contract key mismatch: expected {}, got {}",
                                contract_key.id(),
                                key.id()
                            ));
                        }

                        // Store room info persistently
                        self.storage.add_room(
                            &owner_vk,
                            &signing_key,
                            room_state,
                            &contract_key,
                        )?;

                        Ok((owner_vk, contract_key))
                    }
                    other => Err(anyhow!(
                        "Unexpected contract response type for PUT request: {:?}",
                        other
                    )),
                }
            }
            HostResponse::Ok => {
                // Some versions might return Ok for successful operations
                info!(
                    "Room created (Ok response) with contract key: {}",
                    contract_key.id()
                );

                // Store room info persistently
                self.storage
                    .add_room(&owner_vk, &signing_key, room_state, &contract_key)?;

                Ok((owner_vk, contract_key))
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Republish a room contract to the network
    ///
    /// This re-PUTs the contract with its current state, making this node seed it again.
    /// Use this when the contract exists locally but isn't being served on the network.
    pub async fn republish_room(&self, room_owner_key: &VerifyingKey) -> Result<()> {
        info!(
            "Republishing room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the room state from local storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found in local storage. Cannot republish without local state.")
        })?;
        let (_signing_key, room_state, _contract_key_str) = room_data;

        // Create parameters
        let parameters = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize parameters: {}", e))?;
            buf
        };

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(params_bytes.clone()),
            &contract_code,
        );

        // Create contract container
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));

        // Serialize state
        let state_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&room_state, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize room state: {}", e))?;
            buf
        };
        let wrapped_state = WrappedState::new(state_bytes);

        // Create PUT request with subscribe=true to start seeding
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(put_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send PUT request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for PUT response after 60 seconds")),
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::PutResponse { key }) => {
                info!(
                    "Room republished successfully with contract key: {}",
                    key.id()
                );
                if key != contract_key {
                    return Err(anyhow!(
                        "Contract key mismatch: expected {}, got {}",
                        contract_key.id(),
                        key.id()
                    ));
                }
                Ok(())
            }
            HostResponse::Ok => {
                info!("Room republished successfully (Ok response)");
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub async fn get_room(
        &self,
        room_owner_key: &VerifyingKey,
        subscribe: bool,
    ) -> Result<ChatRoomStateV1> {
        // Ensure room is migrated to the current contract version before any GET.
        // This handles the case where bundled WASM changed (e.g., after a release)
        // and no other client has migrated the state to the new contract key yet.
        let contract_key = self.ensure_room_migrated(room_owner_key).await?;
        info!("Getting room state for contract: {}", contract_key.id());

        // Fetch the room state, recovering it across older contract-WASM
        // generations if the current contract has no state (freenet/river#292).
        let (room_state, found_id) = self
            .fetch_room_state_with_recovery(room_owner_key, *contract_key.id())
            .await?;

        info!(
            "Retrieved room state with {} messages",
            room_state.recent_messages.messages.len()
        );

        if subscribe {
            self.subscribe_to_contract(found_id).await?;
        }

        Ok(room_state)
    }

    /// Fetch a room's state, recovering it across contract-WASM generations.
    ///
    /// The room contract key is `BLAKE3(room_contract.wasm, params)`, so every
    /// WASM upgrade moves the key. A room dormant across one or more upgrades
    /// has its live state stranded under an older-generation key. This:
    ///   1. GETs the current contract (walking any upgrade-pointer chain forward);
    ///   2. if that yields nothing, probes every known previous generation
    ///      newest-to-oldest until one returns state;
    ///   3. migrates a recovered state forward onto the current contract so the
    ///      room is no longer stranded.
    ///
    /// Returns the recovered state and the contract instance it should be
    /// subscribed to.
    async fn fetch_room_state_with_recovery(
        &self,
        room_owner_key: &VerifyingKey,
        current_id: ContractInstanceId,
    ) -> Result<(ChatRoomStateV1, ContractInstanceId)> {
        // 1. Current generation (plus any forward upgrade-pointer chain).
        if let Some((state, id)) = self
            .try_fetch_room(room_owner_key, current_id, CURRENT_GET_TIMEOUT)
            .await
        {
            return Ok((state, id));
        }

        // 2. Backward search across previous contract generations.
        let legacy_keys = river_core::migration::legacy_contract_keys_for_owner(room_owner_key);
        info!(
            "Room not present on current contract {}; probing {} previous contract generation(s)",
            current_id,
            legacy_keys.len()
        );
        for (i, legacy_key) in legacy_keys.iter().enumerate() {
            if let Some((state, found_id)) = self
                .try_fetch_room(room_owner_key, *legacy_key.id(), LEGACY_PROBE_TIMEOUT)
                .await
            {
                info!(
                    "Recovered room from a previous contract generation (probe {}/{})",
                    i + 1,
                    legacy_keys.len()
                );
                // Migrate the recovered state forward onto the current contract
                // so the room is no longer stranded on an old generation. The
                // current contract was just confirmed empty/absent, so this PUT
                // creates it; the room contract's CRDT merge keeps a concurrent
                // migrator's PUT safe.
                if found_id != current_id {
                    match self.put_room_state(room_owner_key, &state).await {
                        Ok(()) => info!(
                            "Migrated recovered room forward onto current contract {current_id}"
                        ),
                        Err(e) => warn!(
                            "Could not migrate recovered room forward (returning it anyway): {e}"
                        ),
                    }
                }
                return Ok((state, current_id));
            }
        }

        Err(anyhow!(
            "Room not found on the current contract or any of the {} known previous \
             contract generations. The room may never have existed, or its state may \
             have been garbage-collected from the network.",
            legacy_keys.len()
        ))
    }

    /// GET a room state from `id`, then walk any `OptionalUpgradeV1` pointer
    /// chain forward to the newest generation that still has state. Returns
    /// `None` if `id` has no usable state.
    async fn try_fetch_room(
        &self,
        room_owner_key: &VerifyingKey,
        id: ContractInstanceId,
        timeout: Duration,
    ) -> Option<(ChatRoomStateV1, ContractInstanceId)> {
        let state = self.try_get_state(room_owner_key, id, timeout).await?;
        Some(self.follow_upgrade_chain(room_owner_key, state, id).await)
    }

    /// GET a `ChatRoomStateV1` from a contract instance, returning `None` for an
    /// absent contract, a timeout, an empty/default state, or a state whose
    /// bytes do not deserialize (an incompatible older generation).
    ///
    /// "No usable state" is defined as a `configuration` whose signature does
    /// not verify against `owner_vk` — the same predicate the UI uses
    /// (`RoomData::is_awaiting_initial_sync`). A real room always carries an
    /// owner-signed configuration; an absent or never-initialised contract
    /// does not.
    async fn try_get_state(
        &self,
        owner_vk: &VerifyingKey,
        id: ContractInstanceId,
        timeout: Duration,
    ) -> Option<ChatRoomStateV1> {
        let get_request = ContractRequest::Get {
            key: id,
            // Request the contract code: a legacy generation's contract may not
            // be cached on this node, and asking for the code lets the GET
            // resolve / cache it rather than failing. The pre-recovery
            // `get_room` used `true`; the recovery probes need the same.
            return_contract_code: true,
            subscribe: false,
            blocking_subscribe: false,
        };
        let mut web_api = self.web_api.lock().await;
        if web_api
            .send(ClientRequest::ContractOp(get_request))
            .await
            .is_err()
        {
            return None;
        }
        let recv = tokio::time::timeout(timeout, web_api.recv()).await;
        drop(web_api);
        match recv {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                state, ..
            }))) => match ciborium::de::from_reader::<ChatRoomStateV1, _>(&state[..]) {
                Ok(mut room_state) => {
                    // A real room always carries an owner-signed configuration;
                    // an absent / never-initialised contract does not.
                    if room_state.configuration.verify_signature(owner_vk).is_err() {
                        return None;
                    }
                    room_state.recent_messages.rebuild_actions_state();
                    Some(room_state)
                }
                Err(e) => {
                    // A state that doesn't deserialize means a genuine
                    // backwards-compat break in an older generation's
                    // `ChatRoomStateV1` — surface it rather than hiding it.
                    warn!("State at {id} did not deserialize ({e}); skipping generation");
                    None
                }
            },
            _ => None,
        }
    }

    /// Follow an `OptionalUpgradeV1` pointer chain forward from `id`, hop by hop,
    /// until a state has no upgrade pointer or a hop's target has no state.
    /// Bounded by [`MAX_UPGRADE_HOPS`] and a visited-set so a cyclic or
    /// self-referential pointer cannot loop forever (freenet/river#292, Part 3).
    async fn follow_upgrade_chain(
        &self,
        room_owner_key: &VerifyingKey,
        mut state: ChatRoomStateV1,
        mut id: ContractInstanceId,
    ) -> (ChatRoomStateV1, ContractInstanceId) {
        let mut visited: HashSet<ContractInstanceId> = HashSet::new();
        visited.insert(id);
        for _ in 0..MAX_UPGRADE_HOPS {
            // `next_upgrade_hop` carries the no-pointer / self-pointer / cycle
            // decision (pure, unit-tested); the network GET is done here.
            let Some(next) = next_upgrade_hop(&state, &mut visited) else {
                break;
            };
            match self
                .try_get_state(room_owner_key, next, UPGRADE_HOP_TIMEOUT)
                .await
            {
                Some(next_state) => {
                    info!("Followed upgrade pointer to newer contract generation {next}");
                    state = next_state;
                    id = next;
                }
                None => break, // Pointer dangles; keep the best state we have.
            }
        }
        (state, id)
    }

    /// PUT a room state onto the *current* room contract. Used to migrate a
    /// state recovered from an older generation forward (freenet/river#292).
    async fn put_room_state(
        &self,
        room_owner_key: &VerifyingKey,
        room_state: &ChatRoomStateV1,
    ) -> Result<()> {
        let parameters = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        let mut params_bytes = Vec::new();
        ciborium::ser::into_writer(&parameters, &mut params_bytes)
            .map_err(|e| anyhow!("Failed to serialize parameters: {e}"))?;
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));
        let mut state_bytes = Vec::new();
        ciborium::ser::into_writer(room_state, &mut state_bytes)
            .map_err(|e| anyhow!("Failed to serialize room state: {e}"))?;
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: WrappedState::new(state_bytes),
            related_contracts: Default::default(),
            subscribe: false,
            blocking_subscribe: false,
        };
        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(put_request))
            .await
            .map_err(|e| anyhow!("Failed to send PUT: {e}"))?;
        match tokio::time::timeout(Duration::from_secs(60), web_api.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { .. })))
            | Ok(Ok(HostResponse::Ok))
            | Ok(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                ..
            }))) => Ok(()),
            Ok(Ok(other)) => Err(anyhow!("Unexpected response to PUT: {other:?}")),
            Ok(Err(e)) => Err(anyhow!("Error receiving PUT response: {e}")),
            Err(_) => Err(anyhow!("Timeout during PUT")),
        }
    }

    /// Subscribe to a contract instance and wait for confirmation.
    async fn subscribe_to_contract(&self, id: ContractInstanceId) -> Result<()> {
        info!("Subscribing to contract {id} to receive updates");
        let subscribe_request = ContractRequest::Subscribe {
            key: id,
            summary: None,
        };
        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(subscribe_request))
            .await
            .map_err(|e| anyhow!("Failed to send SUBSCRIBE request: {e}"))?;
        match tokio::time::timeout(Duration::from_secs(5), web_api.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                subscribed,
                ..
            }))) => {
                if subscribed {
                    info!("Successfully subscribed to contract");
                    Ok(())
                } else {
                    Err(anyhow!("Failed to subscribe to contract"))
                }
            }
            Ok(Ok(_)) => Err(anyhow!("Unexpected response to SUBSCRIBE request")),
            Ok(Err(e)) => Err(anyhow!("Failed to receive subscription response: {e}")),
            Err(_) => Err(anyhow!(
                "Timeout waiting for SUBSCRIBE response after 5 seconds"
            )),
        }
    }

    pub async fn test_connection(&self) -> Result<()> {
        info!("Testing WebSocket connection...");

        // Send a simple disconnect request to test the connection
        let test_request = ClientRequest::Disconnect { cause: None };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(test_request)
            .await
            .map_err(|e| anyhow!("Failed to send test request: {}", e))?;

        info!("Connection test successful");
        Ok(())
    }

    pub async fn create_invitation(&self, room_owner_key: &VerifyingKey) -> Result<String> {
        info!(
            "Creating invitation for room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the room info from persistent storage
        let room_data = self.storage.get_room(room_owner_key)?
            .ok_or_else(|| anyhow!("Room not found in local storage. You must be the room owner to create invitations."))?;
        let (signing_key, state, _contract_key) = room_data;

        // Generate a new signing key for the invitee
        let invitee_signing_key =
            SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()));
        let invitee_vk = invitee_signing_key.verifying_key();

        // Create the member entry for the invitee
        let member = Member {
            owner_member_id: (*room_owner_key).into(),
            member_vk: invitee_vk,
            invited_by: signing_key.verifying_key().into(),
        };

        // Sign the member entry with the inviter's key (room owner in this case)
        let authorized_member = AuthorizedMember::new(member, &signing_key);

        // Collect every room secret the CLI holds so the invitee can decrypt
        // the room immediately on join — without waiting for the owner
        // chat-delegate to back-fill an `encrypted_secrets` blob (issue
        // freenet/river#302, mirrors UI behavior from #301). Empty for public
        // rooms. The owner addresses an owner-signed blob to themselves at
        // every version, so this path works uniformly for owners and non-
        // owners; see the doc-comment on `collect_secrets_for_room` for why
        // we do NOT derive owner secrets via `derive_room_secret` here.
        //
        // Note: `state` is the LOCAL snapshot from `storage.get_room`, not a
        // fresh network GET. If the room rotated since the CLI last synced,
        // the invitation may omit `current_version`'s secret and the invitee
        // will then defer member_info — a fresh GET before invitation
        // creation is a possible future hardening.
        let invitation_secrets = self.storage.get_invitation_secrets(room_owner_key)?;
        let secrets = crate::private_room::collect_secrets_for_room(
            &state,
            &signing_key,
            &invitation_secrets,
        );
        let room_secrets = crate::private_room::collect_invitation_secrets(&secrets);

        // Create the invitation struct
        let invitation = Invitation {
            room: *room_owner_key,
            invitee_signing_key,
            invitee: authorized_member,
            room_secrets,
        };

        // Encode as base58
        let mut data = Vec::new();
        ciborium::ser::into_writer(&invitation, &mut data)
            .map_err(|e| anyhow!("Failed to serialize invitation: {}", e))?;
        let encoded = bs58::encode(data).into_string();

        Ok(encoded)
    }

    pub async fn accept_invitation(
        &self,
        invitation_code: &str,
        nickname: &str,
    ) -> Result<(VerifyingKey, ContractKey)> {
        info!("Accepting invitation with nickname: {}", nickname);

        // Decode the invitation
        let decoded = bs58::decode(invitation_code)
            .into_vec()
            .map_err(|e| anyhow!("Failed to decode invitation: {}", e))?;
        let invitation: Invitation = ciborium::de::from_reader(&decoded[..])
            .map_err(|e| anyhow!("Failed to deserialize invitation: {}", e))?;

        let room_owner_vk = invitation.room;
        let contract_key = self.owner_vk_to_contract_key(&room_owner_vk);

        info!(
            "Invitation is for room owned by: {}",
            bs58::encode(room_owner_vk.as_bytes()).into_string()
        );
        info!("Contract key: {}", contract_key.id());

        // Perform a GET request to fetch the room state
        let get_request = ContractRequest::Get {
            key: *contract_key.id(),    // GET uses ContractInstanceId
            return_contract_code: true, // Request full contract to enable caching
            subscribe: false,           // We'll subscribe separately after GET succeeds
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(get_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send GET request: {}", e))?;

        // Wait for response with timeout
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(result) => {
                    tracing::info!("ACCEPT: received GET response");
                    result.map_err(|e| anyhow!("Failed to receive response: {}", e))?
                }
                Err(_) => return Err(anyhow!("Timeout waiting for GET response after 60 seconds")),
            };

        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse { state, .. } => {
                        info!("Successfully retrieved room state");

                        // Parse the actual room state from the response
                        let room_state: ChatRoomStateV1 = ciborium::de::from_reader(&state[..])
                            .map_err(|e| anyhow!("Failed to deserialize room state: {}", e))?;

                        info!(
                            "Room state retrieved: name={}, members={}, messages={}",
                            room_state
                                .configuration
                                .configuration
                                .display
                                .name
                                .to_string_lossy(),
                            room_state.members.members.len(),
                            room_state.recent_messages.messages.len()
                        );

                        // Validate the room state is properly initialized
                        if room_state.configuration.configuration.owner_member_id
                            == river_core::room_state::member::MemberId(
                                freenet_scaffold::util::FastHash(0),
                            )
                        {
                            return Err(anyhow!("Room state has invalid owner_member_id"));
                        }

                        // Compute invite chain before storing (walks up from invitee
                        // to owner through existing members — doesn't require the
                        // invitee to be in the members list)
                        let params = ChatRoomParametersV1 {
                            owner: room_owner_vk,
                        };
                        let invite_chain = room_state
                            .members
                            .get_invite_chain(&invitation.invitee, &params)
                            .unwrap_or_default();

                        // Persist any invitation-carried room secrets (issue
                        // freenet/river#302) alongside the room itself, so the
                        // CLI can decrypt private-room content across
                        // invocations without re-importing the invitation.
                        //
                        // Merge with any previously-persisted entries so a
                        // re-accept of an older invitation does not silently
                        // drop newer versions the CLI already holds — mirrors
                        // the UI's `extend()` semantics (see
                        // `crate::private_room::merge_invitation_secrets`
                        // for the rationale and the round-2 skeptical-review
                        // finding H1 on PR #303).
                        let invitation_secrets_map = crate::private_room::merge_invitation_secrets(
                            self.storage
                                .get_invitation_secrets(&room_owner_vk)
                                .unwrap_or_default(),
                            &invitation.room_secrets,
                        );

                        // Store credentials locally first
                        self.storage.add_room_with_invitation_secrets(
                            &room_owner_vk,
                            &invitation.invitee_signing_key,
                            room_state.clone(),
                            &contract_key,
                            invitation_secrets_map.clone(),
                        )?;

                        self.storage.store_authorized_member(
                            &room_owner_vk,
                            &invitation.invitee,
                            &invite_chain,
                        )?;

                        // Persist our chosen nickname so a later rejoin (after
                        // an inactivity prune) restores it instead of "Member".
                        self.storage
                            .update_self_nickname(&room_owner_vk, nickname)?;

                        // Immediately publish membership + join event atomically.
                        // The join event counts as a message, preventing
                        // post_apply_cleanup from pruning the new member.
                        let signing_key = &invitation.invitee_signing_key;
                        let self_id = MemberId::from(&signing_key.verifying_key());

                        // Build members delta: invitee + any missing invite chain members
                        let current_member_ids: HashSet<MemberId> = room_state
                            .members
                            .members
                            .iter()
                            .map(|m| m.member.id())
                            .collect();
                        let mut members_to_add = vec![invitation.invitee.clone()];
                        for chain_member in &invite_chain {
                            if !current_member_ids.contains(&chain_member.member.id()) {
                                members_to_add.push(chain_member.clone());
                            }
                        }
                        let members_delta = MembersDelta::new(members_to_add);

                        // Seal the invitee nickname — `SealedBytes::public` for
                        // a public room, AES-GCM at the room's current secret
                        // for a private room. Issue freenet/river#302; mirrors
                        // the UI's `seal_invitee_nickname` (PR #301). Returns
                        // `None` for a private room when neither the
                        // owner-signed contract blob nor the invitation
                        // artifact provides a secret at the room's
                        // `current_secret_version` — in that case we DEFER
                        // `member_info` rather than leak a plaintext nickname
                        // into a private room. The member surfaces as
                        // "Unknown" to other peers until a secret is back-
                        // filled and a future heal re-publishes member_info;
                        // see the UI's `build_member_info_heal` in
                        // `ui/src/room_data.rs` for the eventual remediation
                        // path (CLI counterpart filed as freenet/river#304).
                        let sealed_nickname = crate::private_room::seal_invitee_nickname(
                            &room_state,
                            signing_key,
                            &invitation_secrets_map,
                            nickname,
                        );
                        let member_info_delta = sealed_nickname.map(|sealed| {
                            let member_info = river_core::room_state::member_info::MemberInfo {
                                member_id: self_id,
                                version: 0,
                                preferred_nickname: sealed,
                            };
                            let authorized_info = river_core::room_state::member_info::AuthorizedMemberInfo::new_with_member_key(
                                member_info, signing_key,
                            );
                            vec![authorized_info]
                        });

                        if member_info_delta.is_none() {
                            tracing::warn!(
                                "Private room: no secret available at current_version {} \
                                 (owner blob not yet issued and invitation carries no matching \
                                 secret); deferring member_info — your nickname will not appear \
                                 to other members until a heal publishes it.",
                                room_state.secrets.current_version
                            );
                        }

                        // Build join event message
                        let join_message = river_core::room_state::message::MessageV1 {
                            room_owner: params.owner_id(),
                            author: self_id,
                            content: river_core::room_state::message::RoomMessageBody::join_event(),
                            time: std::time::SystemTime::now(),
                        };
                        let auth_join_message =
                            river_core::room_state::message::AuthorizedMessageV1::new(
                                join_message,
                                signing_key,
                            );

                        let delta = ChatRoomStateV1Delta {
                            recent_messages: Some(vec![auth_join_message]),
                            members: Some(members_delta),
                            member_info: member_info_delta,
                            ..Default::default()
                        };

                        // Apply locally for validation
                        let mut local_state = room_state.clone();
                        local_state
                            .apply_delta(&room_state, &params, &Some(delta.clone()))
                            .map_err(|e| anyhow!("Failed to apply join delta: {:?}", e))?;

                        // Update stored state
                        self.storage
                            .update_room_state(&room_owner_vk, local_state)?;

                        // Send delta to network
                        let delta_bytes = {
                            let mut buf = Vec::new();
                            ciborium::ser::into_writer(&delta, &mut buf)
                                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
                            buf
                        };

                        let update_request = ContractRequest::Update {
                            key: contract_key,
                            data: UpdateData::Delta(delta_bytes.into()),
                        };

                        web_api
                            .send(ClientRequest::ContractOp(update_request))
                            .await
                            .map_err(|e| anyhow!("Failed to send join delta: {}", e))?;

                        // Wait for update response
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(60),
                            web_api.recv(),
                        )
                        .await
                        {
                            Ok(Ok(HostResponse::ContractResponse(
                                ContractResponse::UpdateResponse { .. },
                            ))) => {
                                info!("Invitation accepted and membership published");
                            }
                            Ok(Ok(resp)) => {
                                tracing::warn!("Unexpected response after join delta: {:?}", resp);
                            }
                            Ok(Err(e)) => {
                                tracing::warn!("Error receiving join delta response: {}", e);
                            }
                            Err(_) => {
                                tracing::warn!("Timeout waiting for join delta response");
                            }
                        }

                        drop(web_api);

                        Ok((room_owner_vk, contract_key))
                    }
                    _ => Err(anyhow!("Unexpected contract response type")),
                }
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub fn owner_vk_to_contract_key(&self, owner_vk: &VerifyingKey) -> ContractKey {
        let parameters = ChatRoomParametersV1 { owner: *owner_vk };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .expect("Serialization should not fail");
            buf
        };
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        // Use the full ContractKey constructor that includes the code hash
        ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code)
    }

    /// Check if a room needs migration to a new contract version and perform it if needed.
    ///
    /// This is called automatically when accessing a room. If the bundled contract WASM
    /// has changed (e.g., bug fixes), this will:
    /// 1. Detect the contract key mismatch
    /// 2. Try GET on the new contract (someone else may have migrated)
    /// 3. If no state on new key, try GET from old contract key (previous_contract_key)
    /// 4. PUT the state to the new contract
    /// 5. Send upgrade pointer on old contract (for old-client compat)
    /// 6. Update local storage
    ///
    /// Any member can perform this migration — not just the owner.
    ///
    /// Returns the current contract key (possibly updated).
    pub async fn ensure_room_migrated(&self, room_owner_key: &VerifyingKey) -> Result<ContractKey> {
        let expected_key = self.owner_vk_to_contract_key(room_owner_key);

        // Check if we have this room locally
        let storage = self.storage.load_rooms()?;
        let owner_key_str = bs58::encode(room_owner_key.as_bytes()).into_string();
        let room_info = match storage.rooms.get(&owner_key_str) {
            Some(info) => info,
            None => {
                // Room not in local storage, no migration needed
                return Ok(expected_key);
            }
        };

        let signing_key = self
            .storage
            .resolve_signing_key(&room_info.signing_key_bytes);
        let room_state = room_info.state.clone();
        let previous_contract_key_str = room_info.previous_contract_key.clone();

        // Check if migration is needed. load_rooms() already regenerates the
        // contract_key to match the current WASM and saves the old key in
        // previous_contract_key. If previous_contract_key is None, the room
        // is already on the current contract version.
        if previous_contract_key_str.is_none() {
            return Ok(expected_key);
        }

        // Safe to unwrap: we returned early above when previous_contract_key_str is None.
        let prev_key_str = previous_contract_key_str.as_deref().unwrap();
        let new_key_display = expected_key.id().to_string();
        info!(
            "Room contract version changed, migrating: {} -> {}",
            &prev_key_str[..12.min(prev_key_str.len())],
            &new_key_display[..12.min(new_key_display.len())]
        );

        // Try to GET from the new contract first - maybe someone else already migrated
        let get_request = ContractRequest::Get {
            key: *expected_key.id(),
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(get_request))
            .await
            .map_err(|e| anyhow!("Failed to check new contract: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(10), web_api.recv()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    // Timeout - assume contract doesn't exist yet, we need to migrate
                    drop(web_api);
                    let state_to_migrate = self
                        .get_migration_state(
                            room_owner_key,
                            &room_state,
                            &previous_contract_key_str,
                        )
                        .await?;
                    let result = self
                        .migrate_room_to_new_contract(
                            room_owner_key,
                            &signing_key,
                            &state_to_migrate,
                            expected_key,
                        )
                        .await?;
                    // Send upgrade pointer on old contract
                    self.send_upgrade_pointer(
                        room_owner_key,
                        &signing_key,
                        &previous_contract_key_str,
                        &result,
                    )
                    .await;
                    self.clear_previous_contract_key(room_owner_key)?;
                    return Ok(result);
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::GetResponse { .. }) => {
                // New contract exists — but may have incomplete state if it was seeded
                // before the old contract's full state was available.
                // Always PUT old state into new contract: the room contract uses CRDT
                // merge (additive only, no data loss), so this is safe and idempotent.
                // Skipping the merge when counts match would miss cases where old and
                // new have different message sets with the same count.
                info!("New contract already exists, merging old contract state");
                drop(web_api);

                if let Some(prev_key_str) = &previous_contract_key_str {
                    match self.get_state_from_contract(prev_key_str).await {
                        Ok(old_state) => {
                            info!("Got old contract state, PUTting into new contract");
                            match self
                                .migrate_room_to_new_contract(
                                    room_owner_key,
                                    &signing_key,
                                    &old_state,
                                    expected_key,
                                )
                                .await
                            {
                                Ok(key) => {
                                    self.storage.update_contract_key(room_owner_key, &key)?;
                                    self.clear_previous_contract_key(room_owner_key)?;
                                    // Upgrade pointer not sent here: the contract already
                                    // exists, so another migrator likely already sent it.
                                    // The CLI cannot send pointers anyway (needs full
                                    // ContractKey, not just instance ID).
                                    return Ok(key);
                                }
                                Err(e) => {
                                    // Don't clear previous_contract_key on failure —
                                    // preserving it allows retry on next run.
                                    warn!("Failed to merge old state into new contract: {}", e);
                                    self.storage
                                        .update_contract_key(room_owner_key, &expected_key)?;
                                    return Ok(expected_key);
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Could not fetch old contract {} for merge: {}",
                                prev_key_str, e
                            );
                            // Old contract unreachable (GC'd, network issue). Clear
                            // previous_contract_key since we can't merge what doesn't exist.
                        }
                    }
                }

                self.storage
                    .update_contract_key(room_owner_key, &expected_key)?;
                self.clear_previous_contract_key(room_owner_key)?;
                Ok(expected_key)
            }
            _ => {
                // Contract doesn't exist, try to get state from old contract and migrate
                drop(web_api);
                let state_to_migrate = self
                    .get_migration_state(room_owner_key, &room_state, &previous_contract_key_str)
                    .await?;
                let result = self
                    .migrate_room_to_new_contract(
                        room_owner_key,
                        &signing_key,
                        &state_to_migrate,
                        expected_key,
                    )
                    .await?;
                // Send upgrade pointer on old contract
                self.send_upgrade_pointer(
                    room_owner_key,
                    &signing_key,
                    &previous_contract_key_str,
                    &result,
                )
                .await;
                self.clear_previous_contract_key(room_owner_key)?;
                Ok(result)
            }
        }
    }

    /// GET a ChatRoomStateV1 from a contract by instance ID string.
    async fn get_state_from_contract(&self, contract_id: &str) -> Result<ChatRoomStateV1> {
        let id: ContractInstanceId = contract_id
            .parse()
            .map_err(|e| anyhow!("Invalid contract key: {}", e))?;

        let get_request = ContractRequest::Get {
            key: id,
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(get_request))
            .await
            .map_err(|e| anyhow!("Failed to send GET: {}", e))?;

        match tokio::time::timeout(std::time::Duration::from_secs(30), web_api.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                state, ..
            }))) => {
                let mut room_state = ciborium::de::from_reader::<ChatRoomStateV1, _>(&state[..])
                    .map_err(|e| anyhow!("Failed to deserialize state: {}", e))?;
                room_state.recent_messages.rebuild_actions_state();
                Ok(room_state)
            }
            Ok(Ok(other)) => Err(anyhow!("Unexpected response: {:?}", other)),
            Ok(Err(e)) => Err(anyhow!("Error receiving response: {}", e)),
            Err(_) => Err(anyhow!("Timeout getting contract state")),
        }
    }

    /// Find the freshest state to migrate forward, searching the network across
    /// contract generations and merging in the local cache.
    ///
    /// Tries, in order: the immediately-previous contract key recorded in
    /// storage; then every known previous contract generation newest-first
    /// (covers a room dormant across several WASM upgrades — freenet/river#292).
    /// Whatever network state is found is CRDT-merged with the local cache, so
    /// the migrating PUT carries the real network state rather than a possibly
    /// stale local snapshot. Falls back to the local cache only when nothing is
    /// reachable on-network.
    async fn get_migration_state(
        &self,
        room_owner_key: &VerifyingKey,
        local_state: &ChatRoomStateV1,
        previous_contract_key_str: &Option<String>,
    ) -> Result<ChatRoomStateV1> {
        let mut network_state: Option<ChatRoomStateV1> = None;

        // Fast path: the immediately-previous contract key recorded in storage.
        if let Some(prev_key_str) = previous_contract_key_str {
            match prev_key_str.parse::<ContractInstanceId>() {
                Ok(prev_id) => {
                    info!("Trying GET from previous contract {prev_id} for migration");
                    network_state = self
                        .try_get_state(room_owner_key, prev_id, LEGACY_PROBE_TIMEOUT)
                        .await;
                }
                Err(e) => warn!("Stored previous_contract_key is not a valid contract id: {e}"),
            }
        }

        // Deep path: probe every known previous contract generation
        // newest-first. Covers a room dormant across several WASM upgrades.
        if network_state.is_none() {
            for legacy_key in river_core::migration::legacy_contract_keys_for_owner(room_owner_key)
            {
                if let Some(state) = self
                    .try_get_state(room_owner_key, *legacy_key.id(), LEGACY_PROBE_TIMEOUT)
                    .await
                {
                    info!("Found state on a previous contract generation for migration");
                    network_state = Some(state);
                    break;
                }
            }
        }

        match network_state {
            Some(net_state) => {
                // CRDT-merge the network state with the local cache so neither a
                // fresher network state nor unsynced local edits are lost.
                let params = ChatRoomParametersV1 {
                    owner: *room_owner_key,
                };
                let mut merged = net_state.clone();
                if let Err(e) = merged.merge(&net_state, &params, local_state) {
                    info!("Merge with local state failed ({e}); using network state alone");
                    return Ok(net_state);
                }
                Ok(merged)
            }
            None => {
                info!("No network state on any contract generation; using local cached state");
                Ok(local_state.clone())
            }
        }
    }

    /// Send an upgrade pointer to the old contract key for old-client compatibility.
    /// Note: The CLI cannot send upgrade pointers because it only stores the contract
    /// instance ID (not the full ContractKey with code hash). The UI handles upgrade
    /// pointer sending since it has the full ContractKey from the in-memory migration.
    async fn send_upgrade_pointer(
        &self,
        _room_owner_key: &VerifyingKey,
        _signing_key: &SigningKey,
        _previous_contract_key_str: &Option<String>,
        _new_contract_key: &ContractKey,
    ) {
        // Upgrade pointer sending requires a full ContractKey (with code hash),
        // but CLI storage only preserves the contract instance ID string.
        // The UI handles this since it captures the full ContractKey before regeneration.
        // The critical migration path (GET old → PUT new) works without this.
    }

    /// Clear the previous_contract_key after successful migration.
    fn clear_previous_contract_key(&self, owner_vk: &VerifyingKey) -> Result<()> {
        let mut storage = self.storage.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
            room_info.previous_contract_key = None;
            self.storage.save_rooms(&storage)?;
        }
        Ok(())
    }

    /// Migrate a room to a new contract by PUTting the state
    async fn migrate_room_to_new_contract(
        &self,
        room_owner_key: &VerifyingKey,
        _signing_key: &SigningKey, // Kept for potential future use (e.g., signing migration metadata)
        room_state: &ChatRoomStateV1,
        new_contract_key: ContractKey,
    ) -> Result<ContractKey> {
        info!("Migrating room to new contract: {}", new_contract_key.id());

        let parameters = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize parameters: {}", e))?;
            buf
        };

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));

        let state_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(room_state, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize room state: {}", e))?;
            buf
        };
        let wrapped_state = WrappedState::new(state_bytes);

        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
        };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(put_request))
            .await
            .map_err(|e| anyhow!("Failed to send PUT for migration: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive migration response: {}", e)),
                Err(_) => return Err(anyhow!("Timeout during room migration")),
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::PutResponse { key }) => {
                info!("Room migrated successfully to: {}", key.id());
                // Update local storage with new contract key
                self.storage.update_contract_key(room_owner_key, &key)?;
                Ok(key)
            }
            HostResponse::Ok => {
                info!("Room migrated successfully (Ok response)");
                self.storage
                    .update_contract_key(room_owner_key, &new_contract_key)?;
                Ok(new_contract_key)
            }
            HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                key, ..
            }) => {
                // PUT to an existing contract triggers an UpdateNotification (merge).
                // This is a successful migration.
                info!("Room migrated successfully via merge (UpdateNotification)");
                self.storage.update_contract_key(room_owner_key, &key)?;
                Ok(key)
            }
            _ => Err(anyhow!(
                "Unexpected response during migration: {:?}",
                response
            )),
        }
    }

    pub async fn list_rooms(&self) -> Result<Vec<(String, String, String)>> {
        self.storage.list_rooms().map(|rooms| {
            rooms
                .into_iter()
                .map(|(owner_vk, name, contract_key)| {
                    (
                        bs58::encode(owner_vk.as_bytes()).into_string(),
                        name,
                        contract_key,
                    )
                })
                .collect()
        })
    }

    /// Build a rejoin delta if the user has been pruned from the members list.
    /// Returns (members_delta, member_info_delta) if the user needs to re-add themselves.
    ///
    /// This serves as a fallback for the join event sent at invitation acceptance
    /// time — if the join event ages out of `recent_messages` and the member gets
    /// pruned before sending a regular message, this re-adds them on next send.
    ///
    /// Exposed `pub(crate)` so the `dm` subcommand can bundle the same rejoin
    /// pieces into a DM-bearing delta (Bug #1, reported by Ivvor on Matrix
    /// 2026-05-16) — without this, an invited-but-inactive sender's DM was
    /// silent-dropped by the contract.
    pub(crate) fn build_rejoin_delta(
        &self,
        room_state: &ChatRoomStateV1,
        room_owner_key: &VerifyingKey,
        signing_key: &SigningKey,
    ) -> (Option<MembersDelta>, Option<Vec<AuthorizedMemberInfo>>) {
        let self_vk = signing_key.verifying_key();

        // Owner doesn't need to re-add
        if self_vk == *room_owner_key {
            return (None, None);
        }

        // Already in members list
        if room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == self_vk)
        {
            return (None, None);
        }

        // Try to get stored authorized member
        let storage = match self.storage.load_rooms() {
            Ok(s) => s,
            Err(_) => return (None, None),
        };
        let key_str = bs58::encode(room_owner_key.as_bytes()).into_string();
        let (authorized_member, invite_chain, self_nickname, invitation_secrets) =
            match storage.rooms.get(&key_str) {
                Some(info) => match &info.self_authorized_member {
                    Some(am) => (
                        am.clone(),
                        info.invite_chain.clone(),
                        info.self_nickname.clone(),
                        info.invitation_secrets.clone(),
                    ),
                    None => return (None, None),
                },
                None => return (None, None),
            };

        // Build members delta - include self and any missing chain members
        let current_member_ids: HashSet<MemberId> = room_state
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();
        let mut members_to_add = vec![authorized_member.clone()];
        for chain_member in &invite_chain {
            if !current_member_ids.contains(&chain_member.member.id()) {
                members_to_add.push(chain_member.clone());
            }
        }

        // Build member_info delta
        let self_id = MemberId::from(&self_vk);
        let existing_version = room_state
            .member_info
            .member_info
            .iter()
            .find(|i| i.member_info.member_id == self_id)
            .map(|i| i.member_info.version)
            .unwrap_or(0);

        // Restore the member's real nickname (persisted on join / set-nickname /
        // import) rather than the generic "Member" placeholder. The selection —
        // public vs sealed, the no-secret fallback, and the max_nickname_size
        // clamp — lives in `rejoin_preferred_nickname` so it is unit-testable
        // without a node connection.
        let preferred_nickname = rejoin_preferred_nickname(
            room_state,
            signing_key,
            &invitation_secrets,
            self_nickname.as_deref(),
        );

        let member_info = MemberInfo {
            member_id: self_id,
            version: existing_version,
            preferred_nickname,
        };
        let authorized_info = AuthorizedMemberInfo::new_with_member_key(member_info, signing_key);

        (
            Some(MembersDelta::new(members_to_add)),
            Some(vec![authorized_info]),
        )
    }

    /// Send a message using an explicit signing key (without requiring local storage)
    ///
    /// This fetches the room state from the network and attempts to re-add the sender
    /// if they were pruned for inactivity. Useful for automation, bots, and CI/CD pipelines.
    pub async fn send_message_with_key(
        &self,
        room_owner_key: &VerifyingKey,
        message_content: String,
        signing_key: &SigningKey,
    ) -> Result<()> {
        info!(
            "Sending message (with explicit key) to room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Fetch room state from the network
        let mut room_state = self.get_room(room_owner_key, false).await?;

        let sender_vk = signing_key.verifying_key();
        let sender_member_id: MemberId = (&sender_vk).into();

        // Create the message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: sender_member_id,
            content: river_core::room_state::message::RoomMessageBody::public(message_content),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let is_member = room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == sender_vk);
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, signing_key);

        if !is_member && members_delta.is_none() {
            return Err(anyhow!(
                "Signing key is not a current member of this room and no stored membership \
                 credentials were found for automatic rejoin. If you were pruned for inactivity, \
                 ensure you first accepted an invitation via `riverctl invite accept`."
            ));
        }

        // Create a delta with the new message
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message.clone()]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta locally for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply message delta: {:?}", e))?;

        // Send the delta to the network
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Message sent successfully to contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub async fn send_message(
        &self,
        room_owner_key: &VerifyingKey,
        message_content: String,
    ) -> Result<()> {
        info!(
            "Sending message to room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to send messages.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Create the message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: river_core::room_state::member::MemberId::from(*room_owner_key),
            author: river_core::room_state::member::MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::public(message_content),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the new message
        let delta = river_core::room_state::ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message.clone()]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply message delta: {:?}", e))?;

        // Update the stored state
        self.storage
            .update_room_state(room_owner_key, room_state.clone())?;

        // Send the delta to the network
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Message sent successfully to contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Send a pre-built `ChatRoomStateV1Delta` for a room. Used by call sites
    /// that build the delta themselves (e.g. `riverctl dm send`/`dm purge`)
    /// so they don't have to duplicate the contract-key + serialize + recv
    /// dance.
    pub async fn send_state_delta(
        &self,
        room_owner_key: &VerifyingKey,
        delta: &ChatRoomStateV1Delta,
    ) -> Result<()> {
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };
        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { .. }) => Ok(()),
            other => Err(anyhow!("Unexpected response type: {:?}", other)),
        }
    }

    /// Edit a message you sent
    pub async fn edit_message(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        new_content: String,
    ) -> Result<()> {
        info!(
            "Editing message in room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to edit messages.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Create the edit action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::edit(
                target_message_id,
                new_content,
            ),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the edit action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply edit delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Delete a message you sent
    pub async fn delete_message(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
    ) -> Result<()> {
        info!(
            "Deleting message in room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to delete messages.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Create the delete action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::delete(target_message_id),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the delete action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply delete delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Add a reaction to a message
    pub async fn add_reaction(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        emoji: String,
    ) -> Result<()> {
        info!(
            "Adding reaction '{}' in room owned by: {}",
            emoji,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to add reactions.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Create the reaction action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::reaction(
                target_message_id,
                emoji,
            ),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the reaction action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply reaction delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Remove a reaction from a message
    pub async fn remove_reaction(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        emoji: String,
    ) -> Result<()> {
        info!(
            "Removing reaction '{}' in room owned by: {}",
            emoji,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to remove reactions.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Create the remove_reaction action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::remove_reaction(
                target_message_id,
                emoji,
            ),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the remove_reaction action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply remove_reaction delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Reply to a message
    pub async fn send_reply(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        reply_text: String,
    ) -> Result<()> {
        info!(
            "Sending reply in room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to send replies.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Find the target message to extract author name and content preview
        let target_msg = room_state
            .recent_messages
            .display_messages()
            .find(|m| m.id() == target_message_id)
            .ok_or_else(|| {
                anyhow!("Target message not found in recent messages. Cannot reply to expired messages via CLI.")
            })?;

        let target_author_name = room_state
            .member_info
            .member_info
            .iter()
            .find(|info| info.member_info.member_id == target_msg.message.author)
            .map(|info| info.member_info.preferred_nickname.to_string_lossy())
            .unwrap_or_else(|| target_msg.message.author.to_string());

        let target_content_preview: String = message_display_text(&room_state, target_msg)
            .chars()
            .take(100)
            .collect();

        // Create the reply message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: MemberId::from(&signing_key.verifying_key()),
            content: river_core::room_state::message::RoomMessageBody::reply(
                reply_text,
                target_message_id,
                target_author_name,
                target_content_preview,
            ),
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the reply message
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply reply delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Helper to send a delta to the network.
    /// Assumes migration has already been triggered by the caller (via get_room
    /// or ensure_room_migrated), so owner_vk_to_contract_key returns the correct key.
    async fn send_delta(
        &self,
        room_owner_key: &VerifyingKey,
        delta: ChatRoomStateV1Delta,
    ) -> Result<()> {
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Action sent successfully to contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Stream messages from a room by polling for updates
    pub async fn stream_messages(
        &self,
        room_owner_key: &VerifyingKey,
        poll_interval_ms: u64,
        timeout_secs: u64,
        max_messages: usize,
        initial_messages: usize,
        format: OutputFormat,
    ) -> Result<()> {
        // Get the contract key for the room
        let room = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found in local storage. You may need to create or join it first.")
        })?;

        let (_signing_key, _room_state, contract_key_str) = room;
        let _contract_key = contract_key_str.clone();

        // Print header for human format
        if matches!(format, OutputFormat::Human) {
            eprintln!(
                "Streaming messages from room {} (press Ctrl+C to stop)...\n",
                bs58::encode(room_owner_key.as_bytes()).into_string()
            );
        }

        // Track seen messages: key -> last-emitted effective content, so a later
        // edit (content change) is detected and re-emitted, not just new ids.
        let mut seen_messages: HashMap<String, String> = HashMap::new();
        // Messages for which a deletion has already been emitted (one-shot).
        let mut deleted_emitted: HashSet<String> = HashSet::new();
        let mut new_message_count = 0;
        let start_time = std::time::Instant::now();

        // Show initial messages if requested
        if initial_messages > 0 {
            let room_state = self.get_room(room_owner_key, false).await?;

            // Use display_messages() to filter out action/deleted messages (matches `message list`)
            let all_msgs: Vec<_> = room_state.recent_messages.display_messages().collect();
            let start = all_msgs.len().saturating_sub(initial_messages);

            for msg in &all_msgs[start..] {
                let key = monitor_seen_key(msg);
                seen_messages.insert(key, message_display_text(&room_state, msg));

                Self::output_message(&room_state, msg, room_owner_key, &format, false)?;
            }
        }

        // Set up Ctrl+C handler
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);

        // Spawn task to handle Ctrl+C
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let _ = shutdown_tx.send(()).await;
        });

        // Main polling loop
        loop {
            // Check for shutdown signal
            if shutdown_rx.try_recv().is_ok() {
                if matches!(format, OutputFormat::Human) {
                    eprintln!("\nStopped monitoring.");
                }
                return Ok(());
            }

            // Check timeout
            if timeout_secs > 0 && start_time.elapsed().as_secs() >= timeout_secs {
                debug!("Timeout reached, exiting stream");
                return Ok(());
            }

            // Check max messages
            if max_messages > 0 && new_message_count >= max_messages {
                debug!("Maximum message count reached, exiting stream");
                return Ok(());
            }

            // Poll for new + edited messages. emit_new_and_edited re-emits a
            // message whose effective content changed (an edit) and emits ones
            // not seen before; it respects max_messages for NEW messages.
            match self.get_room(room_owner_key, false).await {
                Ok(room_state) => {
                    Self::emit_new_and_edited(
                        &room_state,
                        &mut seen_messages,
                        room_owner_key,
                        &format,
                        max_messages,
                        &mut new_message_count,
                    )?;
                    Self::emit_deletions(
                        &room_state,
                        &seen_messages,
                        &mut deleted_emitted,
                        room_owner_key,
                        &format,
                    )?;
                    if max_messages > 0 && new_message_count >= max_messages {
                        return Ok(());
                    }
                }
                Err(e) => {
                    // Log error but continue polling
                    debug!("Error fetching room state: {}", e);
                }
            }

            // Wait for next poll interval
            tokio::time::sleep(std::time::Duration::from_millis(poll_interval_ms)).await;
        }
    }

    /// Scan the room's display messages and emit any that are NEW or whose
    /// effective content changed (an EDIT) since last seen. `seen` maps each
    /// message's dedup key to the content last emitted for it, so a later edit
    /// is detected as a content change. `new_count` is incremented only for new
    /// messages (edits don't count toward `max_new`); when `max_new > 0` the
    /// scan stops once that many new messages have been emitted this session.
    ///
    /// This is the shared core of both monitor paths (polling `stream_messages`
    /// and subscription-based `monitor`); before it, edits never surfaced in
    /// either stream because dedup keyed on identity alone. freenet/river —
    /// Rogue Worm report.
    fn emit_new_and_edited(
        room_state: &ChatRoomStateV1,
        seen: &mut HashMap<String, String>,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
        max_new: usize,
        new_count: &mut usize,
    ) -> Result<()> {
        for msg in room_state.recent_messages.display_messages() {
            let key = monitor_seen_key(msg);
            let content = message_display_text(room_state, msg);
            match classify_seen(seen, &key, &content) {
                EmitKind::Unchanged => continue,
                EmitKind::Edited => {
                    Self::output_message(room_state, msg, room_owner_key, format, true)?;
                    seen.insert(key, content);
                }
                EmitKind::New => {
                    Self::output_message(room_state, msg, room_owner_key, format, false)?;
                    seen.insert(key, content);
                    *new_count += 1;
                    if max_new > 0 && *new_count >= max_new {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Emit a deletion event for any previously-surfaced message that has since
    /// been deleted (once per message). Deleted messages are excluded from
    /// `display_messages`, so `emit_new_and_edited` never sees them — this is
    /// the only path that surfaces a deletion to the stream. `deleted_emitted`
    /// tracks already-reported deletions (and is pre-seeded with deletions that
    /// existed at stream start, so only deletions observed live are emitted).
    /// freenet/river#323.
    fn emit_deletions(
        room_state: &ChatRoomStateV1,
        seen: &HashMap<String, String>,
        deleted_emitted: &mut HashSet<String>,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
    ) -> Result<()> {
        for msg in &room_state.recent_messages.messages {
            if !room_state
                .recent_messages
                .actions_state
                .deleted
                .contains(&msg.id())
            {
                continue;
            }
            let key = monitor_seen_key(msg);
            if should_emit_deletion(seen, deleted_emitted, &key) {
                Self::output_deletion(room_state, msg, room_owner_key, format)?;
                deleted_emitted.insert(key);
            }
        }
        Ok(())
    }

    /// Emit a deletion event (the message's content is gone, so only its
    /// identity/author/time are reported). JSON `type: "delete"`; human line is
    /// `[deleted]`-prefixed. freenet/river#323.
    fn output_deletion(
        room_state: &ChatRoomStateV1,
        msg: &river_core::room_state::message::AuthorizedMessageV1,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
    ) -> Result<()> {
        let msg_id = msg.id();
        let author_str = msg.message.author.to_string();
        let nickname = room_state
            .member_info
            .member_info
            .iter()
            .find(|info| info.member_info.member_id == msg.message.author)
            .map(|info| info.member_info.preferred_nickname.to_string_lossy());
        let datetime: DateTime<Utc> = msg.message.time.into();

        match format {
            OutputFormat::Human => {
                let local_time: DateTime<Local> = datetime.into();
                let display_name = nickname
                    .clone()
                    .unwrap_or_else(|| author_str.chars().take(8).collect());
                println!(
                    "[deleted] [{} - {}]: (message deleted)",
                    local_time.format("%H:%M:%S"),
                    display_name
                );
            }
            OutputFormat::Json => {
                let json_msg = json!({
                    "type": "delete",
                    "message_id": msg_id.0 .0.to_string(),
                    "room": bs58::encode(room_owner_key.as_bytes()).into_string(),
                    "author": author_str,
                    "nickname": nickname,
                    "timestamp": datetime.to_rfc3339(),
                });
                println!("{}", serde_json::to_string(&json_msg)?);
            }
        }
        std::io::stdout().flush()?;
        Ok(())
    }

    /// Helper function to output a message in the requested format.
    ///
    /// `is_edit` marks a re-emission of a message whose content changed since it
    /// was first streamed (the monitor's edit detection): the JSON `type`
    /// becomes `"edit"` and the human line is prefixed so a downstream relay can
    /// tell an edit from a fresh message.
    ///
    /// Note `type: "edit"` differs from the `edited` boolean: `edited` is true
    /// whenever an edit action exists for the message (so a message already
    /// edited *before* the stream first saw it is emitted once as
    /// `type: "message"` with `edited: true`), whereas `type: "edit"` marks a
    /// re-emission triggered by a content change observed live.
    fn output_message(
        room_state: &ChatRoomStateV1,
        msg: &river_core::room_state::message::AuthorizedMessageV1,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
        is_edit: bool,
    ) -> Result<()> {
        // Get display content (handles edits and non-text public content like
        // join events; only genuinely encrypted bodies render as "<encrypted>")
        let content = message_display_text(room_state, msg);

        // Get message ID for checking edited status and reactions
        let msg_id = msg.id();
        let edited = room_state.recent_messages.is_edited(&msg_id);
        let reactions = room_state.recent_messages.reactions(&msg_id);
        let reply = reply_context(msg);

        match format {
            OutputFormat::Human => {
                let author_str = msg.message.author.to_string();
                let author_short = author_str.chars().take(8).collect::<String>();

                // Get nickname if available
                let nickname = room_state
                    .member_info
                    .member_info
                    .iter()
                    .find(|info| info.member_info.member_id == msg.message.author)
                    .map(|info| info.member_info.preferred_nickname.to_string_lossy())
                    .unwrap_or(author_short);

                let datetime: DateTime<Utc> = msg.message.time.into();
                let local_time: DateTime<Local> = datetime.into();

                let edited_indicator = if edited { " (edited)" } else { "" };
                // Re-emission of an edited message — distinguish it from a fresh
                // one for a downstream relay reading the human stream.
                let edit_prefix = if is_edit { "[edit] " } else { "" };
                let reply_prefix = reply
                    .as_ref()
                    .map(|(author, preview)| format!("[reply to {}: {}...] ", author, preview))
                    .unwrap_or_default();
                let reactions_str = reactions
                    .map(|r| {
                        if r.is_empty() {
                            String::new()
                        } else {
                            let parts: Vec<_> = r
                                .iter()
                                .map(|(emoji, reactors)| format!("{}×{}", emoji, reactors.len()))
                                .collect();
                            format!(" [{}]", parts.join(" "))
                        }
                    })
                    .unwrap_or_default();

                println!(
                    "{}[{} - {}]: {}{}{}{}",
                    edit_prefix,
                    local_time.format("%H:%M:%S"),
                    nickname,
                    reply_prefix,
                    content,
                    edited_indicator,
                    reactions_str
                );
            }
            OutputFormat::Json => {
                let author_str = msg.message.author.to_string();

                let nickname = room_state
                    .member_info
                    .member_info
                    .iter()
                    .find(|info| info.member_info.member_id == msg.message.author)
                    .map(|info| info.member_info.preferred_nickname.to_string_lossy());

                let datetime: DateTime<Utc> = msg.message.time.into();

                let reactions_map: std::collections::HashMap<String, usize> = reactions
                    .map(|r| r.iter().map(|(k, v)| (k.clone(), v.len())).collect())
                    .unwrap_or_default();

                let message_id_str = msg_id.0 .0.to_string();

                // Reply context (null for non-replies) so a relay can thread the
                // message; previously absent from the monitor's JSON output.
                let reply_to = reply
                    .as_ref()
                    .map(|(author, preview)| json!({ "author": author, "preview": preview }));

                // Output as JSONL (one JSON object per line). `type` is "edit"
                // for a re-emitted message whose content changed, else "message".
                let json_msg = json!({
                    "type": if is_edit { "edit" } else { "message" },
                    "message_id": message_id_str,
                    "room": bs58::encode(room_owner_key.as_bytes()).into_string(),
                    "author": author_str,
                    "nickname": nickname,
                    "content": content,
                    "timestamp": datetime.to_rfc3339(),
                    "edited": edited,
                    "reply_to": reply_to,
                    "reactions": reactions_map,
                });

                println!("{}", serde_json::to_string(&json_msg)?);
            }
        }

        // Flush stdout immediately for real-time output
        std::io::stdout().flush()?;
        Ok(())
    }

    /// Set the current user's nickname in a room
    pub async fn set_nickname(
        &self,
        room_owner_key: &VerifyingKey,
        new_nickname: String,
    ) -> Result<()> {
        info!(
            "Setting nickname to '{}' in room owned by: {}",
            new_nickname,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to change your nickname.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        let my_member_id = signing_key.verifying_key().into();

        // Find our current member info to get the version
        let current_version = room_state
            .member_info
            .member_info
            .iter()
            .find(|info| info.member_info.member_id == my_member_id)
            .map(|info| info.member_info.version)
            .unwrap_or(0);

        // Create new member info with incremented version
        let new_member_info = MemberInfo {
            member_id: my_member_id,
            version: current_version + 1,
            preferred_nickname: SealedBytes::public(new_nickname.clone().into_bytes()),
        };

        // Sign with our member key
        let authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(new_member_info, &signing_key);

        // Update local state first
        if let Some(existing_info) = room_state
            .member_info
            .member_info
            .iter_mut()
            .find(|info| info.member_info.member_id == my_member_id)
        {
            *existing_info = authorized_member_info.clone();
        } else {
            room_state
                .member_info
                .member_info
                .push(authorized_member_info.clone());
        }

        // Save the updated state locally
        self.storage
            .update_room_state(room_owner_key, room_state.clone())?;

        // Persist our chosen nickname so a later rejoin (after an inactivity
        // prune) restores it instead of "Member".
        self.storage
            .update_self_nickname(room_owner_key, &new_nickname)?;

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, _) = self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create delta with member info update (and members delta if needed)
        let delta = ChatRoomStateV1Delta {
            member_info: Some(vec![authorized_member_info]),
            members: members_delta,
            ..Default::default()
        };

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        // Get contract key and send the update
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Nickname updated successfully for contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Ban a member from the room
    ///
    /// The banning member must be either the room owner or an upstream member in the
    /// invite chain of the member being banned.
    pub async fn ban_member(
        &self,
        room_owner_key: &VerifyingKey,
        member_id_short: &str,
    ) -> Result<()> {
        info!(
            "Banning member '{}' from room owned by: {}",
            member_id_short,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to ban members.")
        })?;
        let (signing_key, _stored_state, _contract_key_str) = room_data;

        // Fetch fresh room state from the network
        let room_state = self.get_room(room_owner_key, false).await?;

        let my_member_id: MemberId = signing_key.verifying_key().into();
        let owner_member_id: MemberId = room_owner_key.into();

        // Find the member to ban by their short ID (first 8 chars of member_id string)
        let target_member = room_state
            .member_info
            .member_info
            .iter()
            .find(|info| {
                let member_id_str = info.member_info.member_id.to_string();
                member_id_str.starts_with(member_id_short)
                    || member_id_str[..8.min(member_id_str.len())]
                        .eq_ignore_ascii_case(member_id_short)
            })
            .ok_or_else(|| {
                anyhow!(
                    "Member '{}' not found. Use 'member list' to see member IDs.",
                    member_id_short
                )
            })?;

        let banned_member_id = target_member.member_info.member_id;

        // Prevent self-banning
        if banned_member_id == my_member_id {
            return Err(anyhow!("Cannot ban yourself"));
        }

        // Prevent banning the room owner
        if banned_member_id == owner_member_id {
            return Err(anyhow!("Cannot ban the room owner"));
        }

        // Verify authorization: must be room owner OR in the invite chain of the banned member
        if my_member_id != owner_member_id {
            // Build a map of member IDs to their AuthorizedMember for invite chain traversal
            let members_by_id: std::collections::HashMap<_, _> = room_state
                .members
                .members
                .iter()
                .map(|m| (m.member.id(), m))
                .collect();

            // Find the banned member in the members list
            let banned_member = members_by_id.get(&banned_member_id).ok_or_else(|| {
                anyhow!(
                    "Banned member not found in members list (may already be banned or removed)"
                )
            })?;

            // Walk up the invite chain from the banned member to verify authorization
            let mut current_id = banned_member.member.invited_by;
            let mut found_in_chain = false;
            let mut visited = std::collections::HashSet::new();

            while current_id != owner_member_id {
                if current_id == my_member_id {
                    found_in_chain = true;
                    break;
                }

                if !visited.insert(current_id) {
                    return Err(anyhow!("Circular invite chain detected"));
                }

                let inviter = members_by_id
                    .get(&current_id)
                    .ok_or_else(|| anyhow!("Invite chain broken: inviter not found"))?;
                current_id = inviter.member.invited_by;
            }

            if !found_in_chain {
                return Err(anyhow!(
                    "Not authorized to ban this member. You can only ban members you invited (directly or indirectly)."
                ));
            }
        }

        info!("Banning member with ID: {}", banned_member_id.to_string());

        // Create the ban
        let user_ban = UserBan {
            owner_member_id,
            banned_at: std::time::SystemTime::now(),
            banned_user: banned_member_id,
        };

        let authorized_ban = AuthorizedUserBan::new(user_ban, my_member_id, &signing_key);

        // Create delta with just the ban
        let delta = ChatRoomStateV1Delta {
            bans: Some(vec![authorized_ban.clone()]),
            ..Default::default()
        };

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        // Get contract key and send the update
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Ban applied successfully for contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Update room configuration. Only the room owner can do this.
    pub async fn update_config(
        &self,
        room_owner_key: &VerifyingKey,
        modify: impl FnOnce(&mut Configuration),
    ) -> Result<()> {
        // Get the signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be the room owner to update configuration.")
        })?;
        let (signing_key, _stored_state, _contract_key_str) = room_data;

        // Verify we are the room owner
        let my_vk = signing_key.verifying_key();
        if my_vk != *room_owner_key {
            return Err(anyhow!("Only the room owner can update configuration"));
        }

        // Fetch fresh room state from the network
        let room_state = self.get_room(room_owner_key, false).await?;

        // Clone current config and apply modifications
        let mut new_config = room_state.configuration.configuration.clone();
        new_config.configuration_version += 1;
        modify(&mut new_config);

        // Sign the new configuration
        let authorized_config = AuthorizedConfigurationV1::new(new_config, &signing_key);

        // Create delta with just the configuration change
        let delta = ChatRoomStateV1Delta {
            configuration: Some(authorized_config),
            ..Default::default()
        };

        // Serialize and send
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!(
                    "Configuration updated successfully for contract: {}",
                    key.id()
                );
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Subscribe to a room and stream updates using Freenet subscriptions
    ///
    /// Unlike `stream_messages` which polls, this method subscribes to the contract
    /// and receives push notifications when the contract state changes.
    pub async fn subscribe_and_stream(
        &self,
        room_owner_key: &VerifyingKey,
        timeout_secs: u64,
        max_messages: usize,
        initial_messages: usize,
        format: OutputFormat,
    ) -> Result<()> {
        // Verify room exists in local storage before attempting to subscribe
        self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found in local storage. You may need to create or join it first.")
        })?;

        // Print header for human format
        if matches!(format, OutputFormat::Human) {
            eprintln!(
                "Subscribing to room {} (press Ctrl+C to stop)...",
                bs58::encode(room_owner_key.as_bytes()).into_string()
            );
        }

        // Track seen messages: key -> last-emitted effective content, so a later
        // edit (content change) is detected and re-emitted, not just new ids.
        let mut seen_messages: HashMap<String, String> = HashMap::new();
        // Messages for which a deletion has already been emitted (one-shot).
        // Pre-seeded below with deletions that existed at stream start, so only
        // deletions observed live are surfaced.
        let mut deleted_emitted: HashSet<String> = HashSet::new();
        let mut new_message_count = 0;
        let start_time = std::time::Instant::now();

        // Fetch current room state to pre-populate seen_messages and trigger
        // migration if needed (get_room calls ensure_room_migrated internally).
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);
        let contract_instance_id = *contract_key.id();
        {
            let room_state = self.get_room(room_owner_key, false).await?;

            // Mark ALL non-action messages as seen (key -> effective content),
            // including deleted ones, so deleted messages arriving in deltas are
            // not mistakenly shown as new (https://github.com/freenet/river/issues/173)
            // and later edits are detected as content changes. Pre-existing
            // deletions are recorded in deleted_emitted so they are NOT surfaced
            // as live deletion events (#323).
            for msg in &room_state.recent_messages.messages {
                if !msg.message.content.is_action() {
                    let key = monitor_seen_key(msg);
                    if room_state
                        .recent_messages
                        .actions_state
                        .deleted
                        .contains(&msg.id())
                    {
                        deleted_emitted.insert(key.clone());
                    }
                    seen_messages.insert(key, message_display_text(&room_state, msg));
                }
            }

            // Show the last N display messages if requested
            let display_msgs: Vec<_> = room_state.recent_messages.display_messages().collect();
            let display_start = if initial_messages > 0 {
                display_msgs.len().saturating_sub(initial_messages)
            } else {
                display_msgs.len() // display nothing
            };

            for (i, msg) in display_msgs.iter().enumerate() {
                if i >= display_start {
                    Self::output_message(&room_state, msg, room_owner_key, &format, false)?;
                }
            }
        }

        // Subscribe to the contract
        {
            let subscribe_request = ContractRequest::Subscribe {
                key: contract_instance_id, // Subscribe uses ContractInstanceId
                summary: None,
            };

            let client_request = ClientRequest::ContractOp(subscribe_request);

            let mut web_api = self.web_api.lock().await;
            web_api
                .send(client_request)
                .await
                .map_err(|e| anyhow!("Failed to send SUBSCRIBE request: {}", e))?;

            // Wait for subscription response (30s to accommodate slow gateways)
            let response = match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                web_api.recv(),
            )
            .await
            {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for SUBSCRIBE response")),
            };

            match response {
                HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                    subscribed,
                    ..
                }) => {
                    if subscribed {
                        if matches!(format, OutputFormat::Human) {
                            eprintln!("Successfully subscribed. Waiting for updates...\n");
                        }
                    } else {
                        return Err(anyhow!("Failed to subscribe to contract"));
                    }
                }
                _ => return Err(anyhow!("Unexpected response to SUBSCRIBE request")),
            }
        }

        // Set up Ctrl+C handler
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let _ = shutdown_tx.send(()).await;
        });

        // Main loop: wait for UpdateNotification messages
        loop {
            // Check for shutdown signal
            if shutdown_rx.try_recv().is_ok() {
                if matches!(format, OutputFormat::Human) {
                    eprintln!("\nStopped monitoring.");
                }
                return Ok(());
            }

            // Check timeout
            if timeout_secs > 0 && start_time.elapsed().as_secs() >= timeout_secs {
                debug!("Timeout reached, exiting subscription stream");
                return Ok(());
            }

            // Check max messages
            if max_messages > 0 && new_message_count >= max_messages {
                debug!("Maximum message count reached, exiting subscription stream");
                return Ok(());
            }

            // Wait for next message with a short timeout to allow checking shutdown
            let mut web_api = self.web_api.lock().await;
            let recv_result =
                tokio::time::timeout(std::time::Duration::from_millis(500), web_api.recv()).await;

            match recv_result {
                Ok(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                    key,
                    update,
                }))) => {
                    // We received an update notification
                    debug!("Received update notification for contract: {}", key.id());

                    // Any notification — a delta (INCLUDING edit/delete/reaction
                    // action deltas) or a full-state update — can change what
                    // should be shown. Rather than parse the delta and skip
                    // actions (which made the stream oblivious to edits), re-fetch
                    // the authoritative full state and emit any NEW or EDITED
                    // messages. Deleted messages are excluded by display_messages
                    // and stay marked seen, so #173 (phantom deleted messages)
                    // still holds. The delta payload itself is advisory here.
                    let _ = update;
                    drop(web_api); // get_room needs the web_api lock
                    match self.get_room(room_owner_key, false).await {
                        Ok(room_state) => {
                            Self::emit_new_and_edited(
                                &room_state,
                                &mut seen_messages,
                                room_owner_key,
                                &format,
                                max_messages,
                                &mut new_message_count,
                            )?;
                            Self::emit_deletions(
                                &room_state,
                                &seen_messages,
                                &mut deleted_emitted,
                                room_owner_key,
                                &format,
                            )?;
                        }
                        Err(e) => {
                            debug!("Failed to fetch room state after notification: {}", e);
                        }
                    }
                    if max_messages > 0 && new_message_count >= max_messages {
                        return Ok(());
                    }
                }
                Ok(Ok(other)) => {
                    // Other message type, log and continue
                    debug!("Received unexpected message: {:?}", other);
                }
                Ok(Err(e)) => {
                    // WebSocket error
                    return Err(anyhow!("WebSocket error: {}", e));
                }
                Err(_) => {
                    // Timeout, continue loop (allows checking shutdown signal)
                }
            }
        }
    }
}

/// Tests for the `Invitation` struct's wire format (issue freenet/river#302).
/// The CLI invitation MUST stay byte-identical to the UI's
/// `ui::components::members::Invitation` — the UI's tests
/// (`members::tests::invitation_cbor_*`) pin the same shape on that side; keep
/// the two suites in step.
#[cfg(test)]
mod invitation_tests {
    use super::*;
    use river_core::room_state::member::Member;

    /// Build a deterministic test `Invitation` with the given `room_secrets`.
    fn fixture(room_secrets: Vec<(u32, [u8; 32])>) -> Invitation {
        let inviter = SigningKey::from_bytes(&[1u8; 32]);
        let invitee_signing_key = SigningKey::from_bytes(&[2u8; 32]);
        let owner_vk = SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        let member = Member {
            owner_member_id: owner_vk.into(),
            member_vk: invitee_signing_key.verifying_key(),
            invited_by: inviter.verifying_key().into(),
        };
        Invitation {
            room: owner_vk,
            invitee_signing_key,
            invitee: AuthorizedMember::new(member, &inviter),
            room_secrets,
        }
    }

    /// CBOR round-trip preserves `room_secrets` byte-for-byte. The encoded
    /// invitation is fingerprinted for processed-invite dedup, so the
    /// encode/decode cycle must be stable.
    #[test]
    fn invitation_cbor_round_trip_with_secrets() {
        let original = fixture(vec![(0, [0xAAu8; 32]), (1, [0xBBu8; 32])]);
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&original, &mut bytes).expect("encode");
        let decoded: Invitation = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(original, decoded);
        assert_eq!(
            decoded.room_secrets,
            vec![(0, [0xAAu8; 32]), (1, [0xBBu8; 32])]
        );
    }

    /// Backward compatibility: a CBOR-encoded invitation that PRE-dates
    /// `room_secrets` (i.e. lacks the field entirely) must still decode, with
    /// `room_secrets` defaulting to `Vec::new()`. This is the same
    /// `#[serde(default)]` invariant that keeps UI-issued legacy invitations
    /// decodable by post-#302 riverctl.
    #[test]
    fn invitation_cbor_decodes_legacy_invitation_without_secrets_field() {
        // Build a pre-#302 wire shape: same three fields as the original CLI
        // `Invitation`, serialized as a CBOR map. `serde`'s `#[serde(default)]`
        // on `room_secrets` should fill in `vec![]`.
        #[derive(serde::Serialize)]
        struct LegacyInvitation {
            room: VerifyingKey,
            invitee_signing_key: SigningKey,
            invitee: AuthorizedMember,
        }
        let template = fixture(vec![]);
        let legacy = LegacyInvitation {
            room: template.room,
            invitee_signing_key: template.invitee_signing_key.clone(),
            invitee: template.invitee.clone(),
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&legacy, &mut bytes).expect("encode");
        let decoded: Invitation = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded.room, template.room);
        assert_eq!(decoded.invitee, template.invitee);
        assert!(
            decoded.room_secrets.is_empty(),
            "legacy invitation must decode with empty room_secrets"
        );
    }

    /// `room_secrets` defaults to empty when the inviter holds none — a
    /// public-room invitation must NOT carry any per-version entry, so the
    /// wire bytes stay small.
    #[test]
    fn invitation_with_empty_secrets_round_trips() {
        let original = fixture(vec![]);
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&original, &mut bytes).expect("encode");
        let decoded: Invitation = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, original);
        assert!(decoded.room_secrets.is_empty());
    }

    /// The hand-written `Debug` REDACTS `room_secrets` — `{:?}`-logging an
    /// invitation must not print the secret bytes to stdout/logs. Mirrors the
    /// UI's `Debug` for `ui::components::members::Invitation` (added in #301
    /// review). We check both that the redaction text appears AND that the
    /// derived `Debug` form of `[u8; 32]` (`[205, 205, 205, ..., 205]`) is
    /// absent — the literal byte 0xCD repeats 32 times, which would only
    /// appear in a non-redacted print.
    #[test]
    fn invitation_debug_redacts_room_secrets() {
        let secret_bytes = [0xCDu8; 32];
        let inv = fixture(vec![(0, secret_bytes), (1, [0xEFu8; 32])]);
        let debug_output = format!("{:?}", inv);
        assert!(
            debug_output.contains("redacted"),
            "Debug output should mention redaction: {}",
            debug_output
        );
        // The placeholder must still report the COUNT so an operator can
        // tell the field was populated.
        assert!(
            debug_output.contains("2 room secret(s)"),
            "Debug output should report the secret count: {}",
            debug_output
        );
        // The unredacted `[u8; 32]` Debug form would print the byte 32 times
        // in a row separated by ", " — anchor on that exact shape to avoid
        // false positives from unrelated key material that happens to contain
        // the substring "205".
        let unredacted_form = "[205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205]";
        assert!(
            !debug_output.contains(unredacted_form),
            "Debug output must not print secret bytes (32x 0xCD in array form): {}",
            debug_output
        );
        let unredacted_ef =
            "[239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239]";
        assert!(
            !debug_output.contains(unredacted_ef),
            "Debug output must not print secret bytes (32x 0xEF in array form): {}",
            debug_output
        );
    }
}

#[cfg(test)]
mod migration_recovery_tests {
    use super::*;

    /// The legacy registry derives a contract key exactly as the live code path
    /// (`compute_contract_key` / `owner_vk_to_contract_key`) does. If this ever
    /// drifts, every backward probe would target the wrong contract instance
    /// and silently fail to recover any room. (freenet/river#292)
    #[test]
    fn legacy_derivation_matches_live_key_for_current_wasm() {
        // Any valid signing key works; SigningKey::from_bytes treats the bytes
        // as the seed and is infallible for any 32-byte input.
        let owner = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        let current_code_hash: [u8; 32] = *blake3::hash(ROOM_CONTRACT_WASM).as_bytes();
        let via_registry =
            river_core::migration::contract_key_for_code_hash(&owner, &current_code_hash);
        let via_live = compute_contract_key(&owner);
        assert_eq!(
            via_registry.id(),
            via_live.id(),
            "registry-derived key must match the live owner_vk_to_contract_key derivation"
        );
    }

    /// The current room-contract WASM must NOT be in the legacy registry — the
    /// registry holds only *previous* generations. Listing the current hash
    /// would make a probe redundantly re-fetch the current contract.
    #[test]
    fn current_wasm_is_not_in_legacy_registry() {
        let current_code_hash: [u8; 32] = *blake3::hash(ROOM_CONTRACT_WASM).as_bytes();
        assert!(
            !river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES.contains(&current_code_hash),
            "current room-contract WASM hash {} is listed in legacy_room_contracts.toml; \
             the registry must contain only previous generations",
            blake3::hash(ROOM_CONTRACT_WASM).to_hex()
        );
    }

    /// Build a `ChatRoomStateV1` carrying an upgrade pointer to the contract
    /// instance whose 32-byte id is `target`.
    fn state_pointing_at(target: [u8; 32]) -> ChatRoomStateV1 {
        use river_core::room_state::upgrade::{AuthorizedUpgradeV1, OptionalUpgradeV1, UpgradeV1};
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let upgrade = UpgradeV1 {
            owner_member_id: MemberId::from(&sk.verifying_key()),
            version: 1,
            new_chatroom_address: blake3::Hash::from(target),
        };
        ChatRoomStateV1 {
            upgrade: OptionalUpgradeV1(Some(AuthorizedUpgradeV1::new(upgrade, &sk))),
            ..Default::default()
        }
    }

    /// `next_upgrade_hop` returns `None` for a state with no upgrade pointer —
    /// the chain walk terminates.
    #[test]
    fn next_upgrade_hop_none_without_pointer() {
        let mut visited = HashSet::new();
        assert!(next_upgrade_hop(&ChatRoomStateV1::default(), &mut visited).is_none());
    }

    /// `next_upgrade_hop` follows a pointer to an unvisited contract and
    /// records it in the visited-set.
    #[test]
    fn next_upgrade_hop_follows_unvisited_pointer() {
        let target = [5u8; 32];
        let mut visited = HashSet::new();
        let next = next_upgrade_hop(&state_pointing_at(target), &mut visited)
            .expect("a pointer to a fresh contract must be followed");
        assert_eq!(next, ContractInstanceId::new(target));
        assert!(
            visited.contains(&next),
            "the followed target must be recorded"
        );
    }

    /// `next_upgrade_hop` returns `None` when the pointer targets an
    /// already-visited contract — the cycle guard that stops a chain that
    /// loops back on itself.
    #[test]
    fn next_upgrade_hop_stops_on_cycle() {
        let target = [5u8; 32];
        let mut visited = HashSet::new();
        visited.insert(ContractInstanceId::new(target));
        assert!(
            next_upgrade_hop(&state_pointing_at(target), &mut visited).is_none(),
            "a pointer back to an already-visited contract must stop the walk"
        );
    }
}

#[cfg(test)]
mod display_text_tests {
    use super::*;
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use std::time::SystemTime;

    /// Build a `ChatRoomStateV1` whose `recent_messages` holds a single
    /// authored message with `body`.
    fn state_with_message(body: RoomMessageBody) -> (ChatRoomStateV1, AuthorizedMessageV1) {
        let author_sk = SigningKey::from_bytes(&[11u8; 32]);
        let owner_vk = SigningKey::from_bytes(&[12u8; 32]).verifying_key();
        let message = MessageV1 {
            room_owner: MemberId::from(owner_vk),
            author: MemberId::from(&author_sk.verifying_key()),
            content: body,
            time: SystemTime::UNIX_EPOCH,
        };
        let authored = AuthorizedMessageV1::new(message, &author_sk);
        let mut state = ChatRoomStateV1::default();
        state.recent_messages.messages.push(authored.clone());
        (state, authored)
    }

    /// Regression: a join event is a *public* `content_type = 4` message, not
    /// encrypted. riverctl previously rendered it as "<encrypted>" because the
    /// display path fell back to that literal whenever `effective_text` (which
    /// only yields text/reply bodies) returned `None`. It must now read
    /// "joined the room".
    #[test]
    fn join_event_renders_as_joined_not_encrypted() {
        let (state, msg) = state_with_message(RoomMessageBody::join_event());
        assert_eq!(message_display_text(&state, &msg), "joined the room");
    }

    /// A genuinely private (encrypted) body still renders as "<encrypted>" —
    /// the fix must not leak ciphertext details or mislabel real encryption.
    #[test]
    fn private_body_still_renders_as_encrypted() {
        let body = RoomMessageBody::private(1, 1, vec![0xDE, 0xAD, 0xBE, 0xEF], [0u8; 12], 0);
        let (state, msg) = state_with_message(body);
        assert_eq!(message_display_text(&state, &msg), "<encrypted>");
    }

    /// A public text message is unaffected — it renders its plaintext.
    #[test]
    fn public_text_renders_plaintext() {
        let (state, msg) = state_with_message(RoomMessageBody::public("hello world".to_string()));
        assert_eq!(message_display_text(&state, &msg), "hello world");
    }

    /// An unrecognized *public* content type (a future content_type this CLI
    /// doesn't understand) is not encrypted, so it renders the "please upgrade"
    /// placeholder rather than "<encrypted>". Pins that the fallback narrowing
    /// applies to all public content, not just join events.
    #[test]
    fn unknown_public_content_renders_upgrade_placeholder() {
        let (state, msg) = state_with_message(RoomMessageBody::public_raw(99, 1, vec![0x01, 0x02]));
        assert_eq!(
            message_display_text(&state, &msg),
            "[Unsupported message type 99.1 - please upgrade]"
        );
    }
}

#[cfg(test)]
mod rejoin_nickname_tests {
    use super::*;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::privacy::PrivacyMode;

    fn member_key() -> SigningKey {
        SigningKey::from_bytes(&[11u8; 32])
    }

    /// Build a `ChatRoomStateV1` with the given privacy mode, nickname-size
    /// limit, and current secret version.
    fn state_with(
        privacy: PrivacyMode,
        max_nickname_size: usize,
        current_version: u32,
    ) -> ChatRoomStateV1 {
        let owner_sk = SigningKey::from_bytes(&[3u8; 32]);
        let config = Configuration {
            owner_member_id: owner_sk.verifying_key().into(),
            privacy_mode: privacy,
            max_nickname_size,
            ..Default::default()
        };
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(config, &owner_sk),
            ..Default::default()
        };
        state.secrets.current_version = current_version;
        state
    }

    /// Public room → the real nickname is restored as public plaintext.
    #[test]
    fn public_room_restores_real_nickname() {
        let state = state_with(PrivacyMode::Public, 50, 0);
        let out = rejoin_preferred_nickname(&state, &member_key(), &HashMap::new(), Some("Alice"));
        assert!(out.is_public());
        assert_eq!(out.to_string_lossy(), "Alice");
    }

    /// No persisted nickname → generic "Member" placeholder.
    #[test]
    fn no_stored_nickname_falls_back_to_member() {
        let state = state_with(PrivacyMode::Public, 50, 0);
        let out = rejoin_preferred_nickname(&state, &member_key(), &HashMap::new(), None);
        assert!(out.is_public());
        assert_eq!(out.to_string_lossy(), "Member");
    }

    /// A nickname longer than the room's current `max_nickname_size` must NOT
    /// be published (the contract would reject the whole rejoin delta) — fall
    /// back to "Member" so the member can still rejoin. Regression guard for the
    /// PR #321 Codex/skeptical finding.
    #[test]
    fn over_long_nickname_falls_back_to_member() {
        // max 8: "Member" (6) fits, but the stored nickname (20) does not.
        let state = state_with(PrivacyMode::Public, 8, 0);
        let out = rejoin_preferred_nickname(
            &state,
            &member_key(),
            &HashMap::new(),
            Some("this_is_way_too_long"),
        );
        assert_eq!(out.to_string_lossy(), "Member");
    }

    /// Private room with a secret available → the nickname is SEALED
    /// (ciphertext), never published as plaintext.
    #[test]
    fn private_room_with_secret_seals_nickname() {
        let state = state_with(PrivacyMode::Private, 50, 1);
        let mut secrets = HashMap::new();
        secrets.insert(1u32, [7u8; 32]);
        let out = rejoin_preferred_nickname(&state, &member_key(), &secrets, Some("Alice"));
        assert!(out.is_private(), "private-room nickname must be sealed");
        // Declared plaintext length is preserved even though the bytes are sealed.
        assert_eq!(out.declared_len(), "Alice".len());
    }

    /// Private room with NO secret available → must fall back to the generic
    /// public "Member" placeholder, NEVER leak the real nickname as plaintext.
    #[test]
    fn private_room_without_secret_does_not_leak_real_nickname() {
        let state = state_with(PrivacyMode::Private, 50, 1);
        let out = rejoin_preferred_nickname(&state, &member_key(), &HashMap::new(), Some("Alice"));
        assert!(out.is_public());
        assert_eq!(out.to_string_lossy(), "Member");
        assert_ne!(
            out.to_string_lossy(),
            "Alice",
            "real nickname must never be published as plaintext in a private room"
        );
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::*;
    use river_core::room_state::message::{
        AuthorizedMessageV1, MessageId, MessageV1, RoomMessageBody,
    };
    use std::time::SystemTime;

    fn authored(body: RoomMessageBody) -> AuthorizedMessageV1 {
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        let owner = SigningKey::from_bytes(&[6u8; 32]).verifying_key();
        let m = MessageV1 {
            room_owner: MemberId::from(owner),
            author: MemberId::from(&sk.verifying_key()),
            content: body,
            time: SystemTime::UNIX_EPOCH,
        };
        AuthorizedMessageV1::new(m, &sk)
    }

    /// A reply message yields its target author and a preview truncated to 50
    /// chars — the context the monitor stream now renders (it previously didn't).
    #[test]
    fn reply_context_extracts_author_and_truncated_preview() {
        let long_preview = "x".repeat(80);
        let msg = authored(RoomMessageBody::reply(
            "my reply".to_string(),
            MessageId(freenet_scaffold::util::FastHash(0)),
            "Alice".to_string(),
            long_preview,
        ));
        let (author, preview) = reply_context(&msg).expect("should detect a reply");
        assert_eq!(author, "Alice");
        assert_eq!(preview.chars().count(), 50, "preview truncated to 50 chars");
    }

    /// A plain (non-reply) message has no reply context.
    #[test]
    fn reply_context_none_for_plain_message() {
        let msg = authored(RoomMessageBody::public("hello".to_string()));
        assert!(reply_context(&msg).is_none());
    }

    /// A join event (public, non-text, non-reply) has no reply context.
    #[test]
    fn reply_context_none_for_event() {
        let msg = authored(RoomMessageBody::join_event());
        assert!(reply_context(&msg).is_none());
    }

    fn reply_with_preview(preview: &str) -> AuthorizedMessageV1 {
        authored(RoomMessageBody::reply(
            "my reply".to_string(),
            MessageId(freenet_scaffold::util::FastHash(0)),
            "Alice".to_string(),
            preview.to_string(),
        ))
    }

    /// Preview truncation boundaries: a short preview is returned whole, an
    /// exactly-50 preview is untouched, and a multi-byte/emoji preview is
    /// truncated by CHARACTERS (not bytes), so `.chars().take(50)` never panics
    /// or splits a codepoint.
    #[test]
    fn reply_context_preview_boundaries() {
        // Shorter than 50 → returned whole.
        let (_, short) = reply_context(&reply_with_preview("hi")).unwrap();
        assert_eq!(short, "hi");

        // Empty preview → still a reply, empty body.
        let (author, empty) = reply_context(&reply_with_preview("")).unwrap();
        assert_eq!(author, "Alice");
        assert_eq!(empty, "");

        // Exactly 50 → unchanged.
        let exactly = "a".repeat(50);
        let (_, p50) = reply_context(&reply_with_preview(&exactly)).unwrap();
        assert_eq!(p50.chars().count(), 50);
        assert_eq!(p50, exactly);

        // 60 emoji (multi-byte) → truncated to 50 chars, no panic / no split.
        let emojis = "🦀".repeat(60);
        let (_, pe) = reply_context(&reply_with_preview(&emojis)).unwrap();
        assert_eq!(pe.chars().count(), 50);
    }

    /// Regression guard for PR #322 review finding #1: two DIFFERENT messages
    /// from the same author with an identical timestamp must get DIFFERENT
    /// monitor dedup keys (keyed on the signature-derived id, not author:time),
    /// or they would flip-flop forever as spurious "edit" re-emissions. The same
    /// message yields a stable key.
    #[test]
    fn monitor_seen_key_distinct_for_same_author_and_time_different_content() {
        let sk = SigningKey::from_bytes(&[8u8; 32]);
        let owner = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let make = |text: &str| {
            let m = MessageV1 {
                room_owner: MemberId::from(owner),
                author: MemberId::from(&sk.verifying_key()),
                content: RoomMessageBody::public(text.to_string()),
                time: SystemTime::UNIX_EPOCH, // identical timestamp
            };
            AuthorizedMessageV1::new(m, &sk)
        };
        let a = make("first");
        let b = make("second");
        assert_ne!(
            monitor_seen_key(&a),
            monitor_seen_key(&b),
            "same author + identical timestamp but different content must not collide"
        );
        // Same message → stable key.
        assert_eq!(monitor_seen_key(&a), monitor_seen_key(&make("first")));
    }

    /// The monitor edit-detection: a key never seen is New; the same content is
    /// Unchanged; a changed content for a seen key is Edited.
    #[test]
    fn classify_seen_detects_new_unchanged_edited() {
        let mut seen: HashMap<String, String> = HashMap::new();
        assert_eq!(classify_seen(&seen, "k1", "hello"), EmitKind::New);
        seen.insert("k1".to_string(), "hello".to_string());
        assert_eq!(classify_seen(&seen, "k1", "hello"), EmitKind::Unchanged);
        assert_eq!(
            classify_seen(&seen, "k1", "hello, world"),
            EmitKind::Edited,
            "a changed effective content for a seen message is an edit"
        );
        assert_eq!(classify_seen(&seen, "k2", "other"), EmitKind::New);
    }

    /// Deletion is emitted only for a message the stream previously surfaced,
    /// and only once. A message never shown (not in `seen`) — e.g. deleted
    /// before the stream started — produces no deletion event. freenet/river#323.
    #[test]
    fn should_emit_deletion_only_for_seen_and_unreported() {
        let mut seen: HashMap<String, String> = HashMap::new();
        let mut emitted: HashSet<String> = HashSet::new();

        // Never surfaced → no deletion event.
        assert!(!should_emit_deletion(&seen, &emitted, "k1"));

        // Surfaced → emit once.
        seen.insert("k1".to_string(), "hi".to_string());
        assert!(should_emit_deletion(&seen, &emitted, "k1"));

        // Already reported → don't repeat.
        emitted.insert("k1".to_string());
        assert!(!should_emit_deletion(&seen, &emitted, "k1"));
    }
}
