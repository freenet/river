//! Exporting and importing your identity in a room (the web UI's "Export ID" and "Import ID").
//!
//! An identity export is river-core's [`IdentityExport`] in its armored text form
//! (`-----BEGIN RIVER IDENTITY-----`, base58 of CBOR, 64 characters a line). The web UI
//! (`ui/src/components/members.rs`), `riverctl` and this app all encode and decode it with the
//! same type, so a token made by any of them imports in the others. It holds the room's owner,
//! your private signing key for the room, your signed membership record and invite chain, your
//! last nickname record and nickname, the room's name and any room secrets your invitation
//! carried. It is not encrypted and has no passphrase: **it is the key.** Whoever has it can
//! write in the room as you. So nothing here logs it, the types that hold it print `<hidden>`,
//! and the screens warn in the web UI's words.
//!
//! **Export** is one room at a time, built from the stored room as the web UI builds it: the
//! cached membership record and chain, else a self-signed record for the owner, else the
//! record and chain the room state holds for you (refused without a chain that validates).
//!
//! **Import** checks the token before anything is written ([`parse_token`]: the encoding, that
//! the signing key is the member's, the signatures of the invite chain, that the record names
//! the room's owner), then writes the room's slot in the chat delegate's storage under the web
//! UI's generation compare-and-swap ([`super::work`]):
//! - A room the store does not hold (or holds as a tombstone, an explicit rejoin as in the web
//!   UI) is read from the network first, so the slot holds the room's real state, then stored
//!   whole ([`NewRoom`]), as a join stores one.
//! - A room the store holds is **replaced in place** ([`slot_with_identity`]), only when the
//!   user said to replace. Only the identity fields of the stored slot change; everything
//!   else the web UI keeps there (the room state, the read position, fields this app does not
//!   know) stays exactly as it was, edited as CBOR. The verbs are the web UI's
//!   `swap_room_identity_in_place`: a different identity replaces all of the identity
//!   fields; the same identity merges (keeps a newer nickname record, keeps your chosen
//!   nickname when the token has none, unions the secrets with the stored ones winning).
//!
//! After an import the key sweep ([`super::keys`]) makes the delegate's signing key for the room
//! the imported one, over whatever it held (the web UI's authoritative `migrate_signing_key`).
//! That intent is in memory and the sweep only sends when the session is idle: a restart between
//! the slot write and the sweep leaves the delegate on the old key until a later sweep, and the
//! web UI replaces a different key on its next load of the room. A member the room holds with no
//! nickname record gets one published (the web UI's heal, river#414) once the room has taken the
//! network's state.
//!
//! What this does not do, which the web UI does: it does not prune cached direct-message state
//! (this app has none yet), and it does not import a room it cannot read from the network (the
//! web UI stores an empty placeholder and syncs later).

use std::collections::BTreeMap;

use ciborium::Value;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::ContractKey;
use river_core::room_state::ChatRoomParametersV1;
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::identity::IdentityExport;
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::member_info::AuthorizedMemberInfo;
use river_core::room_state::privacy::SealedBytes;
use river_core::stored_rooms::{NewRoom, StoredRoomSlot, decode_room_slot, encode_new_room};

use super::room::LiveRoom;

/// The longest token this app reads. A real one is one to a few kilobytes (the signing key, a
/// membership record and a chain of them, a private room's secrets); decoding base58 takes
/// time that grows with the square of its length, so anything far above that is refused
/// rather than decoded.
pub const MAX_TOKEN_LEN: usize = 16 * 1024;

/// A pasted identity token. It is the user's signing key, so its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityToken(pub String);

impl std::fmt::Debug for IdentityToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IdentityToken(<hidden>)")
    }
}

/// What a token's own text may put on screen: nothing but plain printable characters, and a
/// bounded amount. The decoder's error text can quote the token.
fn plain(s: &str, max: usize) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out: String = clean.chars().take(max).collect();
    if clean.chars().count() > max {
        out.push('\u{2026}');
    }
    out
}

/// Read and check a pasted token. The error is what the screen says (the web UI's
/// "Invalid token: ..."), bounded and with nothing the token could use to draw on the screen.
///
/// river-core's decoder checks that the signing key is the member's and the signatures of the
/// chain links whose inviter it can find, and skips the rest. That is not enough to replace a
/// working identity with, so this also requires what a real export always has: every record
/// names this room's owner, and the member's invite chain leads, link by link with every
/// signature checked, to the owner (or the member is the owner, self-signed).
pub fn parse_token(pasted: &str) -> Result<IdentityExport, String> {
    if pasted.trim().is_empty() {
        return Err("Paste an identity token.".into());
    }
    if pasted.len() > MAX_TOKEN_LEN {
        return Err("Invalid token: it is far longer than an identity token.".into());
    }
    let export = IdentityExport::from_armored_string(pasted)
        .map_err(|e| format!("Invalid token: {}", plain(&e, 160)))?;
    check_chain(&export).map_err(|why| format!("Invalid token: {why}"))?;
    Ok(export)
}

fn check_chain(export: &IdentityExport) -> Result<(), String> {
    verify_membership(export, None).map(|_| ())
}

/// That the member's record leads to the room's owner with every signature checked.
///
/// The web UI sets a member's record and invite chain separately, so a real token can hold a
/// record invited by a member and an EMPTY chain (the inviter was not in the member's local
/// state when they joined). Such a link is not refused: with `state` (the room as read from the
/// network, or as stored) the inviter must be a member of it whose own chain leads to the owner,
/// and the record's signature must verify against their key. Without `state` (a pasted token
/// before anything is read) an open end is allowed here and checked once the room is known.
pub fn verify_membership(
    export: &IdentityExport,
    state: Option<&ChatRoomStateV1>,
) -> Result<Vec<AuthorizedMember>, String> {
    let owner = export.room_owner;
    let owner_id = MemberId::from(&owner);
    let me = &export.authorized_member;
    if me.member.owner_member_id != owner_id
        || export
            .invite_chain
            .iter()
            .any(|m| m.member.owner_member_id != owner_id)
    {
        return Err("a membership record is for a different room owner.".into());
    }
    if me.member.member_vk != export.signing_key.verifying_key() {
        return Err("the signing key is not the member's.".into());
    }
    if export.invite_chain.len() > 64 {
        return Err("the invite chain is far longer than a room allows.".into());
    }
    let mut current = me;
    // The links walked so far, in order: what is stored as the member's invite chain.
    let mut walked: Vec<AuthorizedMember> = Vec::new();
    // At most one step per chain member, then the owner: a loop cannot run forever.
    for _ in 0..=export.invite_chain.len() {
        let inviter = current.member.invited_by;
        if inviter == owner_id {
            return current
                .verify_signature(&owner)
                .map(|_| walked)
                .map_err(|_| "a membership record is not signed by the room's owner.".to_owned());
        }
        if inviter == current.member.id() {
            return Err("a membership record names the member as its own inviter.".into());
        }
        if let Some(link) = export
            .invite_chain
            .iter()
            .find(|m| m.member.id() == inviter)
        {
            current
                .verify_signature(&link.member.member_vk)
                .map_err(|_| {
                    "an invite in the chain is not signed by the member who sent it.".to_owned()
                })?;
            walked.push(link.clone());
            current = link;
            continue;
        }
        // An open end: the inviter is in no chain link.
        let Some(state) = state else {
            return Ok(export.invite_chain.clone());
        };
        let params = ChatRoomParametersV1 { owner };
        let unshown = || {
            "its invite can't be checked against the room: the member who invited this identity \
             is not in the room's member list here."
                .to_owned()
        };
        let Some(record) = state
            .members
            .members
            .iter()
            .find(|m| m.member.id() == inviter)
        else {
            return Err(unshown());
        };
        current
            .verify_signature(&record.member.member_vk)
            .map_err(|_| {
                "an invite is not signed by the member the room lists as its inviter.".to_owned()
            })?;
        let above = state
            .members
            .get_invite_chain(record, &params)
            .map_err(|_| {
                "the inviter's own invite chain does not lead to the room's owner.".to_owned()
            })?;
        // What a rejoin needs if the room later prunes the member and the inviter: the inviter
        // and the records above them, verified here against the room as read.
        walked.push(record.clone());
        walked.extend(above);
        return Ok(walked);
    }
    Err("the invite chain does not lead to the room's owner.".into())
}

/// Whether the room's own rule (the contract's `banned_member_ids`, as the composer uses) takes
/// the imported member out of `state`. A member merely absent from a stale or partial state is
/// not banned: only an enforced ban counts. The owner cannot be banned.
pub fn banned_in(state: &ChatRoomStateV1, export: &IdentityExport) -> bool {
    let vk = export.signing_key.verifying_key();
    if vk == export.room_owner {
        return false;
    }
    super::outbox::banned_by_state(
        state,
        &ChatRoomParametersV1 {
            owner: export.room_owner,
        },
        &vk,
        Some(&export.authorized_member),
        &export.invite_chain,
    )
}

/// What the screen says when the room has banned the imported identity.
pub const BANNED: &str =
    "This identity has been banned from this room, so importing it would not let you use the room.";

/// The nickname to publish again for the imported identity when the room holds none for it: the
/// one the token carries in plaintext, else the one in its record (when public).
pub fn nickname_of(export: &IdentityExport) -> Option<String> {
    export.self_nickname.clone().or_else(|| {
        match &export.member_info.as_ref()?.member_info.preferred_nickname {
            SealedBytes::Public { value } => Some(String::from_utf8_lossy(value).into_owned()),
            _ => None,
        }
    })
}

/// The room's key as the screens name it: base58 of the owner's verifying key.
pub fn room_key(owner: &VerifyingKey) -> String {
    bs58::encode(owner.to_bytes()).into_string()
}

/// The export token for `room`, as the web UI's export dialog builds it, or why there is none.
pub fn export_token(room: &LiveRoom) -> Result<String, String> {
    let stored = &room.stored;
    let Some(sk) = stored.self_sk.clone() else {
        // An export is the private key, so a room that keeps it elsewhere has nothing to export.
        return Err("Cannot export: the signing key for this room is not stored here.".into());
    };
    let vk = sk.verifying_key();
    let owner = stored.owner_vk;
    let resolved: Option<(AuthorizedMember, Vec<AuthorizedMember>)> =
        if let Some(am) = &stored.self_authorized_member {
            Some((am.clone(), stored.invite_chain.clone()))
        } else if vk == owner {
            let owner_id = MemberId::from(&owner);
            let member = Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: owner,
            };
            Some((AuthorizedMember::new(member, &sk), vec![]))
        } else {
            let params = ChatRoomParametersV1 { owner };
            stored
                .room_state
                .members
                .members
                .iter()
                .find(|m| m.member.member_vk == vk)
                .and_then(|m| {
                    // An export with a broken chain would fail the import's validation.
                    stored
                        .room_state
                        .members
                        .get_invite_chain(m, &params)
                        .ok()
                        .map(|chain| (m.clone(), chain))
                })
        };
    let Some((authorized_member, invite_chain)) = resolved else {
        return Err(
            "Cannot export: membership data not available. Try sending a message first.".into(),
        );
    };
    let room_name = room.unseal(&stored.room_state.configuration.configuration.display.name);
    let member_info = stored.self_member_info.clone().or_else(|| {
        stored
            .room_state
            .member_info
            .canonical(MemberId::from(&vk))
            .cloned()
    });
    let export = IdentityExport {
        room_owner: owner,
        signing_key: sk,
        authorized_member,
        invite_chain,
        member_info,
        room_name,
        self_nickname: stored.self_nickname.clone(),
        // `IdentityExport` keeps a `HashMap`; the store a `BTreeMap`.
        invitation_secrets: stored
            .invitation_secrets
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect(),
    };
    Ok(export.to_armored_string())
}

/// The room an import adds, built from the room as the network has it (`state`, read under
/// `contract_key`). Refused if that is not the room the token is for.
pub fn new_room(
    export: &IdentityExport,
    state: &ChatRoomStateV1,
    contract_key: ContractKey,
) -> Result<NewRoom, super::create::JoinRefusal> {
    use super::create::JoinRefusal;
    let owner_id = MemberId::from(&export.room_owner);
    if state.configuration.configuration.owner_member_id != owner_id {
        return Err(JoinRefusal::final_(
            "Freenet gave back a room that isn't the one this identity is for.",
        ));
    }
    let vk = export.signing_key.verifying_key();
    let chain = verify_membership(export, Some(state))
        .map_err(|why| JoinRefusal::final_(format!("Invalid token: {why}")))?;
    // The room as read may be a stale copy: a ban seen in it is worth reading again.
    if banned_in(state, export) {
        return Err(JoinRefusal::for_now(BANNED));
    }
    Ok(NewRoom {
        owner_vk: export.room_owner,
        room_state: state.clone(),
        self_sk: Some(export.signing_key.clone()),
        self_vk: Some(vk),
        contract_key,
        last_read_message_id: None,
        self_authorized_member: Some(export.authorized_member.clone()),
        invite_chain: chain,
        self_member_info: export.member_info.clone(),
        self_nickname: export.self_nickname.clone(),
        invitation_secrets: export
            .invitation_secrets
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect(),
    })
}

/// What an import does with a room's key, given what it holds now.
#[derive(Debug, PartialEq, Eq)]
pub enum ImportWrite {
    /// Store these bytes at the generation just read.
    Store(Vec<u8>),
    /// Already as the import would leave it (an earlier store whose answer was lost).
    Unchanged,
    /// Not allowed, and why. Trying again does not change it.
    Refuse(String),
}

/// The write that adds the imported `room` where the key holds nothing, or a tombstone (an
/// explicit rejoin). Never over a room that is there with another identity: that is what
/// replace is for, and the user has to say so. The same identity already there is merged as
/// the web UI merges a re-import ([`slot_with_identity`]).
pub fn add_room(current: Option<&[u8]>, room: &NewRoom, export: &IdentityExport) -> ImportWrite {
    let store = || match encode_new_room(room) {
        Ok(bytes) => ImportWrite::Store(bytes),
        Err(e) => ImportWrite::Refuse(e),
    };
    let Some(bytes) = current.filter(|b| !b.is_empty()) else {
        return store();
    };
    match decode_room_slot(bytes) {
        Ok(StoredRoomSlot::Tombstone) => store(),
        Ok(StoredRoomSlot::Present(stored)) => {
            let mine = room.self_verifying_key();
            if mine.is_some() && stored.self_verifying_key() == mine {
                slot_with_identity(current, export).unwrap_or_else(ImportWrite::Refuse)
            } else {
                ImportWrite::Refuse(
                    "You already have this room. Import again and choose to replace your \
                     identity in it."
                        .into(),
                )
            }
        }
        Err(e) => ImportWrite::Refuse(format!(
            "River's storage holds something for this room that this app can't read ({e}), so it \
             was left as it is."
        )),
    }
}

fn cbor<T: serde::Serialize>(v: &T) -> Result<Value, String> {
    Value::serialized(v).map_err(|e| format!("room slot: {e}"))
}

fn set_field(entries: &mut Vec<(Value, Value)>, name: &str, value: Value) {
    match entries.iter_mut().find(|(k, _)| k.as_text() == Some(name)) {
        Some((_, v)) => *v = value,
        None => entries.push((Value::Text(name.into()), value)),
    }
}

/// `current` (a stored `Present` slot) with `export`'s identity in place of the stored one,
/// everything else in the slot kept as CBOR, as the web UI's `swap_room_identity_in_place`
/// does. `Unchanged` when the slot already holds exactly that. A slot that is absent, a
/// tombstone, another room's, or unreadable is refused and left alone.
pub fn slot_with_identity(
    current: Option<&[u8]>,
    export: &IdentityExport,
) -> Result<ImportWrite, String> {
    let gone = || {
        Ok(ImportWrite::Refuse(
            "This room is no longer in River's storage on this computer, so there is no \
             identity to replace. Import it again to add it."
                .into(),
        ))
    };
    let Some(bytes) = current.filter(|b| !b.is_empty()) else {
        return gone();
    };
    let stored = match decode_room_slot(bytes) {
        Ok(StoredRoomSlot::Present(r)) => r,
        Ok(StoredRoomSlot::Tombstone) => return gone(),
        Err(e) => {
            return Ok(ImportWrite::Refuse(format!(
                "River's storage holds something for this room that this app can't read ({e}), \
                 so it was left as it is."
            )));
        }
    };
    if stored.owner_vk != export.room_owner {
        return Ok(ImportWrite::Refuse(
            "River's storage holds a different room under this one's name, so it was left as it \
             is."
            .into(),
        ));
    }
    let imported_vk = export.signing_key.verifying_key();
    let same = stored.self_verifying_key() == Some(imported_vk);
    // The same identity the slot already holds a record for needs no new proof of membership: a
    // re-import only merges, and the inviter may have been pruned from the stored state since.
    let has_record = same
        && stored
            .self_authorized_member
            .as_ref()
            .is_some_and(|m| m.member.member_vk == imported_vk);
    let chain = if has_record {
        export.invite_chain.clone()
    } else {
        match verify_membership(export, Some(&stored.room_state)) {
            Ok(chain) => chain,
            Err(why) => return Ok(ImportWrite::Refuse(format!("Invalid token: {why}"))),
        }
    };
    if banned_in(&stored.room_state, export) {
        return Ok(ImportWrite::Refuse(BANNED.into()));
    }
    let Ok(Value::Map(mut outer)) = ciborium::de::from_reader::<Value, _>(bytes) else {
        return Ok(ImportWrite::Refuse("room slot: not a map".into()));
    };
    let Some(Value::Map(fields)) = outer
        .iter_mut()
        .find(|(k, _)| k.as_text() == Some("Present"))
        .map(|(_, v)| v)
    else {
        return Ok(ImportWrite::Refuse("room slot: not a present room".into()));
    };

    // The key itself: always the imported one, with its public half.
    set_field(fields, "self_sk", cbor(&Some(&export.signing_key))?);
    set_field(fields, "self_vk", cbor(&Some(imported_vk))?);
    let secrets: BTreeMap<u32, [u8; 32]> = export
        .invitation_secrets
        .iter()
        .map(|(k, v)| (*k, *v))
        .collect();
    if !same {
        // A different identity: all of the identity metadata is the imported one's, and the old
        // identity's invitation secrets are not carried to the new one.
        set_field(
            fields,
            "self_authorized_member",
            cbor(&Some(&export.authorized_member))?,
        );
        set_field(fields, "invite_chain", cbor(&chain)?);
        set_field(fields, "self_member_info", cbor(&export.member_info)?);
        set_field(fields, "self_nickname", cbor(&export.self_nickname)?);
        set_field(fields, "invitation_secrets", cbor(&secrets)?);
    } else {
        // The same identity again (a stale or older token): what the token lacks or has older
        // never erases what the slot has.
        if stored.self_authorized_member.is_none() {
            set_field(
                fields,
                "self_authorized_member",
                cbor(&Some(&export.authorized_member))?,
            );
            set_field(fields, "invite_chain", cbor(&chain)?);
        }
        let newer: Option<AuthorizedMemberInfo> =
            match (stored.self_member_info.clone(), export.member_info.clone()) {
                (Some(l), Some(i)) if i.member_info.version > l.member_info.version => Some(i),
                (Some(l), _) => Some(l),
                (None, i) => i,
            };
        set_field(fields, "self_member_info", cbor(&newer)?);
        if export.self_nickname.is_some() {
            set_field(fields, "self_nickname", cbor(&export.self_nickname)?);
        }
        let mut union = stored.invitation_secrets.clone();
        for (v, s) in secrets {
            union.entry(v).or_insert(s);
        }
        set_field(fields, "invitation_secrets", cbor(&union)?);
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(outer), &mut out)
        .map_err(|e| format!("room slot: {e}"))?;
    Ok(if out == bytes {
        ImportWrite::Unchanged
    } else {
        ImportWrite::Store(out)
    })
}

#[cfg(test)]
#[path = "identity_tests.rs"]
pub(super) mod tests;
