//! Building a new room, an invitation to a room, and what joining a room from an invitation
//! sends and stores, the way the web UI builds each. Pure: the network and the chat delegate's
//! storage are [`super::work`]'s business.
//!
//! What this mirrors in the web UI:
//! - **A new room** (`RoomData::create_new_room_with_name` in `ui/src/room_data.rs`, called
//!   from `room_list/create_room_modal.rs`): the owner key is a fresh key and is the user's
//!   own; the first configuration is signed by it, with the name sealed under a fresh secret
//!   for a private room; the owner's nickname record (version 0) is signed by it too; a
//!   private room gets secret version 0, sealed to the owner (`encrypt_secret_for_member`)
//!   and signed. The room contract's key is the bundled room-contract WASM with the owner's
//!   parameters (`owner_vk_to_contract_key`), and the room reaches the network with the web
//!   UI's seed PUT (`seed_put_request` in `freenet_api/room_synchronizer.rs`): the WASM, the
//!   parameters and the normalized state, subscribing. Checked first with the room's own
//!   limits (`new_room_refusal`), as the dialog does.
//! - **An invitation** (`members/invitation_builder.rs::create_invitation`): a fresh invitee
//!   key, its membership signed by the inviting member's own key (the web UI asks the chat
//!   delegate to sign and falls back to the same key; Ed25519 is deterministic, so both give
//!   the same signature), and for a private room every secret the inviter holds, by version
//!   (`collect_invitation_secrets`). The link is the web app's URL with `?invitation=<code>`
//!   (`invite_member_modal.rs`).
//! - **Joining** (the pending-invite branch of `handle_get_response` in
//!   `freenet_api/response_handler/get_response.rs`): after reading the room from the network,
//!   the nickname is checked against the room's limit, sealed for a private room under the
//!   current secret (from the room, else from the invitation: `seal_invitee_nickname`) or left
//!   out when neither has it, and signed by the invitee; the invitee's membership, that record
//!   and a join event (`RoomMessageBody::join_event`) go in. The stored room keeps the
//!   invitation's credentials (`record_invite_credentials`), its invite chain, and the
//!   invitation's secrets up to the room's current version.
//!
//! Where joining differs: the web UI publishes the joined room with a whole-state PUT. This
//! app never PUTs a room it did not just create, so it sends the same additions as an UPDATE
//! delta, as riverctl's `accept_invitation_struct` (`cli/src/api.rs`) does: the membership,
//! the invite chain members the room lacks, the nickname record, the join event. Before
//! anything is sent, the delta is applied to the room as read, and a room that would not
//! admit the invitee or keep the join event (full, or the invitee banned) refuses the join,
//! as riverctl's `refuse_if_dropped` and the web UI's `RoomAtCapacity` do.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractKey, ContractWasmAPIVersion, Parameters,
    WrappedContract, WrappedState,
};
use river_core::ecies::{
    decrypt_secret_from_member_blob_raw, encrypt_secret_for_member, encrypt_with_symmetric_key,
    generate_room_secret, seal_bytes,
};
use river_core::invitation::Invitation;
use river_core::room_state::configuration::{
    AuthorizedConfigurationV1, Configuration, new_room_refusal,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, nickname_refusal};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::{
    PrivacyMode, RoomCipherSpec, RoomDisplayMetadata, SealedBytes,
};
use river_core::room_state::secret::{
    AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord, EncryptedSecretForMemberV1,
    SecretVersionRecordV1,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use river_core::stored_rooms::NewRoom;

use super::room::LiveRoom;

/// The room contract the web UI ships. A room's contract key is a hash of these bytes and the
/// owner's parameters, so this is the web UI's contract and key. A file read, not a crate
/// dependency: it cannot change the WASM. `apps.yml` runs when it changes.
const ROOM_CONTRACT_WASM: &[u8] =
    include_bytes!("../../../../ui/public/contracts/room_contract.wasm");

fn params_bytes(owner: &VerifyingKey) -> Vec<u8> {
    let mut out = Vec::new();
    // CBOR of a struct of one 32-byte key into a Vec does not fail.
    let _ = ciborium::ser::into_writer(&ChatRoomParametersV1 { owner: *owner }, &mut out);
    out
}

/// The room contract key of the room `owner` owns, as the web UI derives it
/// (`owner_vk_to_contract_key` in `ui/src/util.rs`).
pub fn contract_key(owner: &VerifyingKey) -> ContractKey {
    ContractKey::from_params_and_code(
        Parameters::from(params_bytes(owner)),
        &ContractCode::from(ROOM_CONTRACT_WASM),
    )
}

/// A one-line name a user typed is refused if it holds characters that draw nothing or
/// reorder text, as the web UI's dialogs refuse them (`contains_hidden_chars`).
fn hidden_char_refusal(what: &str, s: &str) -> Option<String> {
    s.chars()
        .any(super::room::is_hidden)
        .then(|| format!("The {what} has characters that don't show. Type it again."))
}

/// Why a new room with this name and nickname can't be made, if it can't: what the web UI's
/// create dialog checks before it creates a room.
pub fn create_refusal(name: &str, nickname: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("Give the room a name.".into());
    }
    if let Some(r) = hidden_char_refusal("room name", name) {
        return Some(r);
    }
    if let Some(r) = hidden_char_refusal("nickname", nickname) {
        return Some(r);
    }
    let params = ChatRoomParametersV1 {
        owner: SigningKey::from_bytes(&[1; 32]).verifying_key(),
    };
    new_room_refusal(&params, name, nickname)
}

/// A room made here, ready to store and to publish. No `Debug`: it holds the owner's key.
pub struct NewRoomPlan {
    pub room: NewRoom,
    /// The seed PUT's state: the room's state, normalized.
    pub state: Vec<u8>,
}

impl NewRoomPlan {
    /// The room's name, for the screen.
    pub fn name(&self) -> String {
        LiveRoom::new(super::work::stored(&self.room)).name()
    }

    /// The seed PUT (the web UI's `seed_put_request`).
    pub fn put(&self) -> freenet_stdlib::client_api::ContractRequest<'static> {
        let container = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
            Arc::new(ContractCode::from(ROOM_CONTRACT_WASM)),
            Parameters::from(params_bytes(&self.room.owner_vk)),
        )));
        freenet_stdlib::client_api::ContractRequest::Put {
            contract: container,
            state: WrappedState::new(self.state.clone()),
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
        }
    }
}

/// Build a new room owned by `owner` (a fresh key), named `name`, with the owner's `nickname`.
/// The web UI's `create_new_room_with_name`.
pub fn new_room(
    owner: SigningKey,
    name: &str,
    nickname: &str,
    private: bool,
) -> Result<NewRoomPlan, String> {
    if let Some(reason) = create_refusal(name, nickname) {
        return Err(reason);
    }
    let owner_vk = owner.verifying_key();
    let owner_id = MemberId::from(&owner_vk);
    let mut state = ChatRoomStateV1::default();
    let secret = private.then(generate_room_secret);
    if let Some(secret) = &secret {
        let (ciphertext, nonce, ephemeral) = encrypt_secret_for_member(secret, &owner_vk);
        state
            .secrets
            .versions
            .push(AuthorizedSecretVersionRecord::new(
                SecretVersionRecordV1 {
                    version: 0,
                    cipher_spec: RoomCipherSpec::Aes256Gcm,
                    created_at: SystemTime::now(),
                },
                &owner,
            ));
        state
            .secrets
            .encrypted_secrets
            .push(AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: owner_id,
                    secret_version: 0,
                    ciphertext,
                    nonce,
                    sender_ephemeral_public_key: ephemeral.to_bytes(),
                    provider: owner_id,
                },
                &owner,
            ));
        state.secrets.current_version = 0;
    }
    // Sealed as the web UI seals them here: under version 0, declaring the plaintext length.
    let seal = |text: &str| match &secret {
        Some(secret) => {
            let (ciphertext, nonce) = encrypt_with_symmetric_key(secret, text.as_bytes());
            SealedBytes::Private {
                ciphertext,
                nonce,
                secret_version: 0,
                declared_len_bytes: text.len() as u32,
            }
        }
        None => SealedBytes::public(text.as_bytes().to_vec()),
    };
    let config = Configuration {
        owner_member_id: owner_id,
        privacy_mode: if private {
            PrivacyMode::Private
        } else {
            PrivacyMode::Public
        },
        display: RoomDisplayMetadata {
            name: seal(name),
            description: None,
        },
        ..Configuration::default()
    };
    state.configuration = AuthorizedConfigurationV1::new(config, &owner);
    state
        .member_info
        .member_info
        .push(AuthorizedMemberInfo::new(
            MemberInfo {
                member_id: owner_id,
                version: 0,
                preferred_nickname: seal(nickname),
                deputies: Vec::new(),
            },
            &owner,
        ));
    let params = ChatRoomParametersV1 { owner: owner_vk };
    let state = state.normalized(&params)?;
    state
        .verify(&state, &params)
        .map_err(|e| format!("The new room is not valid: {e}"))?;
    let mut put_state = Vec::new();
    ciborium::ser::into_writer(&state, &mut put_state).map_err(|e| e.to_string())?;
    Ok(NewRoomPlan {
        room: NewRoom {
            owner_vk,
            room_state: state,
            self_vk: Some(owner_vk),
            self_sk: Some(owner),
            contract_key: contract_key(&owner_vk),
            last_read_message_id: None,
            // The owner needs none of these (the web UI leaves them empty for an owner).
            self_authorized_member: None,
            invite_chain: Vec::new(),
            self_member_info: None,
            self_nickname: None,
            invitation_secrets: BTreeMap::new(),
        },
        state: put_state,
    })
}

/// An invitation to `room` from the user, and the link that carries it. `web_app` is the web
/// app's address on the user's node (`http://<authority>/v1/contract/web/<id>/`).
pub fn invitation(room: &LiveRoom, web_app: &str) -> Result<(Invitation, String), String> {
    let Some(inviter) = &room.stored.self_sk else {
        return Err(
            "This computer doesn't hold your key for this room, so it can't make an invitation."
                .into(),
        );
    };
    let invitee = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
    let member = Member {
        owner_member_id: MemberId::from(&room.stored.owner_vk),
        invited_by: MemberId::from(&inviter.verifying_key()),
        member_vk: invitee.verifying_key(),
    };
    let invitation = Invitation {
        room: room.stored.owner_vk,
        invitee_signing_key: invitee,
        invitee: AuthorizedMember::new(member, inviter),
        room_secrets: if room.is_private() {
            room.secrets_by_version()
        } else {
            Vec::new()
        },
    };
    let code = invitation.to_encoded_string()?;
    let link = format!("{web_app}?invitation={code}");
    Ok((invitation, link))
}

/// What joining a room sends and stores. No `Debug`: it holds the invitee's key.
pub struct JoinPlan {
    pub room: NewRoom,
    pub delta: ChatRoomStateV1Delta,
}

/// Why `nickname` can't be used to join, before the room is read.
pub fn join_refusal(nickname: &str) -> Option<String> {
    if nickname.trim().is_empty() {
        return Some("Choose a nickname for this room.".into());
    }
    if nickname.len() > river_core::room_state::member_info::NICKNAME_CEILING {
        return Some("That nickname is too long.".into());
    }
    hidden_char_refusal("nickname", nickname)
}

/// The room's current secret sealed to the member holding `sk`, from the room itself
/// (the web UI's `current_secret_from_state`).
fn current_secret_from_state(state: &ChatRoomStateV1, sk: &SigningKey) -> Option<([u8; 32], u32)> {
    let me = MemberId::from(&sk.verifying_key());
    let version = state.secrets.current_version;
    let blob = state
        .secrets
        .encrypted_secrets
        .iter()
        .find(|s| s.secret.member_id == me && s.secret.secret_version == version)?;
    let secret = decrypt_secret_from_member_blob_raw(
        &blob.secret.ciphertext,
        &blob.secret.nonce,
        &blob.secret.sender_ephemeral_public_key,
        sk,
    )
    .ok()?;
    Some((secret, version))
}

/// Join the room `state` (as read from the network under `contract_key`) with `invitation`,
/// as `nickname`, at `now`.
pub fn join(
    invitation: &Invitation,
    nickname: &str,
    state: &ChatRoomStateV1,
    contract_key: ContractKey,
    now: SystemTime,
) -> Result<JoinPlan, String> {
    if let Some(reason) = join_refusal(nickname) {
        return Err(reason);
    }
    let owner_vk = invitation.room;
    let params = ChatRoomParametersV1 { owner: owner_vk };
    let owner_id = params.owner_id();
    let config = &state.configuration.configuration;
    if config.owner_member_id != owner_id {
        return Err("Freenet gave back a room that isn't the one this invitation is for.".into());
    }
    if let Some(reason) = nickname_refusal(nickname.len(), config) {
        return Err(reason);
    }
    let sk = &invitation.invitee_signing_key;
    let me = MemberId::from(&sk.verifying_key());
    // The invitation's secrets the room has reached (the web UI drops later ones).
    let invitation_secrets: BTreeMap<u32, [u8; 32]> = invitation
        .room_secrets
        .iter()
        .filter(|(v, _)| *v <= state.secrets.current_version)
        .copied()
        .collect();
    let sealed = if config.privacy_mode == PrivacyMode::Private {
        current_secret_from_state(state, sk)
            .or_else(|| {
                let v = state.secrets.current_version;
                invitation_secrets.get(&v).map(|s| (*s, v))
            })
            .map(|(secret, version)| seal_bytes(nickname.as_bytes(), &secret, version))
    } else {
        Some(SealedBytes::public(nickname.as_bytes().to_vec()))
    };
    let member_info = sealed.map(|preferred_nickname| {
        AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: me,
                version: 0,
                preferred_nickname,
                deputies: Vec::new(),
            },
            sk,
        )
    });
    let invite_chain = state
        .members
        .get_invite_chain(&invitation.invitee, &params)
        .unwrap_or_default();
    let present: std::collections::HashSet<MemberId> = state
        .members
        .members
        .iter()
        .map(|m| m.member.id())
        .collect();
    let already = me == owner_id || present.contains(&me);
    let mut adding = Vec::new();
    if !already {
        if state.members.members.len() >= config.max_members {
            return Err("This room is full.".into());
        }
        adding.push(invitation.invitee.clone());
    }
    for m in &invite_chain {
        if !present.contains(&m.member.id()) {
            adding.push(m.clone());
        }
    }
    let join_event = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: me,
            content: RoomMessageBody::join_event(),
            time: now,
        },
        sk,
    );
    let join_id = join_event.id();
    let delta = ChatRoomStateV1Delta {
        recent_messages: Some(vec![join_event]),
        members: (!adding.is_empty()).then(|| MembersDelta::new(adding)),
        member_info: member_info.clone().map(|i| vec![i]),
        ..Default::default()
    };
    let mut joined = state.clone();
    joined
        .apply_delta(&ChatRoomStateV1::default(), &params, &Some(delta.clone()))
        .map_err(|e| format!("The room won't take you: {e}"))?;
    let admitted = me == owner_id || joined.members.members.iter().any(|m| m.member.id() == me);
    let kept = joined
        .recent_messages
        .messages
        .iter()
        .any(|m| m.id() == join_id);
    if !admitted || !kept {
        return Err(
            "The room won't take you. You may have been banned, or the invitation may no longer \
             be valid."
                .into(),
        );
    }
    Ok(JoinPlan {
        room: NewRoom {
            owner_vk,
            room_state: joined,
            self_vk: Some(sk.verifying_key()),
            self_sk: Some(sk.clone()),
            contract_key,
            last_read_message_id: None,
            self_authorized_member: Some(invitation.invitee.clone()),
            invite_chain,
            self_member_info: member_info,
            self_nickname: Some(nickname.to_owned()),
            invitation_secrets,
        },
        delta,
    })
}

#[cfg(test)]
#[path = "create_tests.rs"]
mod tests;
