//! Legacy records: v1 records carried verbatim into a v2 state. Each wraps
//! the v1 bytes (which must be the canonical re-encoding of the v1 type) and
//! names its signer key, which must hash to the v1 id the record claims and
//! verify the v1 signature (strictly). Records whose v1 bytes name a room
//! owner must name this room's owner, so a v1 record cannot be replayed from
//! another room.
//!
//! `decode_*` checks everything but the signature; `verify` checks the
//! signature, so `f` can verify lazily in admission order.

use super::encoding::{from_cbor_canonical, legacy_id, to_cbor, verify_bytes, Key, Sig};
use super::records::{Legacy, Record};
use crate::room_state::ban::AuthorizedUserBan;
use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use crate::room_state::direct_messages::{
    build_direct_message_signed_bytes, build_recipient_purges_signed_bytes,
    AuthorizedDirectMessage, AuthorizedRecipientPurges, PurgeToken,
};
use crate::room_state::member::{AuthorizedMember, MemberId};
use crate::room_state::member_info::AuthorizedMemberInfo;
use crate::room_state::message::{AuthorizedMessageV1, RoomMessageBody};
use crate::room_state::secret::AuthorizedEncryptedSecretForMember;
use crate::room_state::ChatRoomParametersV1;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold::util::FastHash;
use std::time::{SystemTime, UNIX_EPOCH};

fn id(k: &Key) -> MemberId {
    MemberId(FastHash(legacy_id(k)))
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A sealed v1 value whose ciphertext is no longer than its declared
/// length plus the AEAD tag, so a declared length bounds its size.
fn sealed_v1_ok(s: &crate::room_state::privacy::SealedBytes) -> bool {
    match s {
        crate::room_state::privacy::SealedBytes::Public { .. } => true,
        crate::room_state::privacy::SealedBytes::Private {
            ciphertext,
            declared_len_bytes,
            ..
        } => ciphertext.len() <= *declared_len_bytes as usize + super::records::AEAD_TAG,
    }
}

fn sig_of(s: &ed25519_dalek::Signature) -> Sig {
    Sig(s.to_bytes())
}

pub struct LegacyConfig {
    pub configuration: Configuration,
    pub sig: Sig,
}

pub fn decode_config(owner: &Key, l: &Legacy) -> Option<LegacyConfig> {
    if l.signer != *owner {
        return None;
    }
    let v1: AuthorizedConfigurationV1 = from_cbor_canonical(&l.v1.0)?;
    let vk = VerifyingKey::from_bytes(&owner.0).ok()?;
    let params = ChatRoomParametersV1 { owner: vk };
    v1.configuration.check_values(&params).ok()?;
    let d = &v1.configuration.display;
    if !sealed_v1_ok(&d.name) || !d.description.as_ref().is_none_or(sealed_v1_ok) {
        return None;
    }
    Some(LegacyConfig {
        sig: sig_of(&v1.signature),
        configuration: v1.configuration,
    })
}

pub struct LegacyLinkView {
    pub member: Key,
    pub inviter: Key,
}

pub fn decode_link(owner: &Key, l: &Legacy) -> Option<LegacyLinkView> {
    let v1: AuthorizedMember = from_cbor_canonical(&l.v1.0)?;
    let m = &v1.member;
    if m.owner_member_id != id(owner) || m.invited_by != id(&l.signer) {
        return None;
    }
    Some(LegacyLinkView {
        member: Key::from(&m.member_vk),
        inviter: l.signer,
    })
}

pub struct LegacyBanView {
    pub target: i64,
    pub time: u64,
}

pub fn decode_ban(owner: &Key, l: &Legacy) -> Option<LegacyBanView> {
    let v1: AuthorizedUserBan = from_cbor_canonical(&l.v1.0)?;
    if v1.ban.owner_member_id != id(owner) || v1.banned_by != id(&l.signer) {
        return None;
    }
    Some(LegacyBanView {
        target: v1.ban.banned_user.0 .0,
        time: secs(v1.ban.banned_at),
    })
}

pub struct LegacyMessageView {
    pub sig: Sig,
    pub author: Key,
    pub time: u64,
    pub body: RoomMessageBody,
}

pub fn decode_message(owner: &Key, l: &Legacy) -> Option<LegacyMessageView> {
    let v1: AuthorizedMessageV1 = from_cbor_canonical(&l.v1.0)?;
    if v1.message.room_owner != id(owner) || v1.message.author != id(&l.signer) {
        return None;
    }
    Some(LegacyMessageView {
        sig: sig_of(&v1.signature),
        author: l.signer,
        time: secs(v1.message.time),
        body: v1.message.content,
    })
}

pub struct LegacyMemberInfoView {
    pub sig: Sig,
    pub member: Key,
    pub version: u32,
    pub declared_len: usize,
}

pub fn decode_member_info(_owner: &Key, l: &Legacy) -> Option<LegacyMemberInfoView> {
    let v1: AuthorizedMemberInfo = from_cbor_canonical(&l.v1.0)?;
    if v1.member_info.member_id != id(&l.signer)
        || !sealed_v1_ok(&v1.member_info.preferred_nickname)
    {
        return None;
    }
    Some(LegacyMemberInfoView {
        sig: sig_of(&v1.signature),
        member: l.signer,
        version: v1.member_info.version,
        declared_len: v1.member_info.preferred_nickname.declared_len(),
    })
}

pub struct LegacyDmView {
    pub sender: Key,
    pub sender_id: i64,
    pub recipient_id: i64,
    pub time: u64,
    pub ciphertext_len: usize,
    pub sig: Sig,
    pub token: [u8; 16],
}

pub fn decode_dm(_owner: &Key, l: &Legacy) -> Option<LegacyDmView> {
    let v1: AuthorizedDirectMessage = from_cbor_canonical(&l.v1.0)?;
    if v1.message.sender != id(&l.signer) {
        return None;
    }
    Some(LegacyDmView {
        sender: l.signer,
        sender_id: v1.message.sender.0 .0,
        recipient_id: v1.message.recipient.0 .0,
        time: v1.message.timestamp,
        ciphertext_len: v1.message.ciphertext.len(),
        sig: sig_of(&v1.sender_signature),
        token: PurgeToken::from_signature(&v1.sender_signature).0,
    })
}

pub struct LegacyPurgeView {
    pub sig: Sig,
    pub recipient_id: i64,
    pub version: u64,
    pub tokens: Vec<[u8; 16]>,
}

pub fn decode_purge(_owner: &Key, l: &Legacy) -> Option<LegacyPurgeView> {
    let v1: AuthorizedRecipientPurges = from_cbor_canonical(&l.v1.0)?;
    if v1.recipient_id != id(&l.signer) || v1.state.version == 0 {
        return None;
    }
    Some(LegacyPurgeView {
        sig: sig_of(&v1.recipient_signature),
        recipient_id: v1.recipient_id.0 .0,
        version: v1.state.version,
        tokens: v1.state.purged.iter().map(|t| t.0).collect(),
    })
}

pub struct LegacyBlobView {
    pub sig: Sig,
    pub recipient_id: i64,
    pub version: u32,
    pub ciphertext_len: usize,
}

pub fn decode_blob(owner: &Key, l: &Legacy) -> Option<LegacyBlobView> {
    if l.signer != *owner {
        return None;
    }
    let v1: AuthorizedEncryptedSecretForMember = from_cbor_canonical(&l.v1.0)?;
    Some(LegacyBlobView {
        sig: sig_of(&v1.owner_signature),
        recipient_id: v1.secret.member_id.0 .0,
        version: v1.secret.secret_version,
        ciphertext_len: v1.secret.ciphertext.len(),
    })
}

fn strict<T: serde::Serialize>(signer: &Key, inner: &T, sig: &ed25519_dalek::Signature) -> bool {
    verify_bytes(signer, &to_cbor(inner), &sig_of(sig))
}

/// The v1 signature of a legacy record, checked strictly under its named
/// signer. False for anything that does not decode.
pub fn verify(owner: &Key, rec: &Record) -> bool {
    let decoded = || -> Option<bool> {
        Some(match rec {
            Record::LegacyConfiguration(l) => {
                let v1: AuthorizedConfigurationV1 = from_cbor_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.configuration, &v1.signature)
            }
            Record::LegacyLink(l) => {
                let v1: AuthorizedMember = from_cbor_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.member, &v1.signature)
            }
            Record::LegacyBan(l) => {
                let v1: AuthorizedUserBan = from_cbor_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.ban, &v1.signature)
            }
            Record::LegacyActivity(l) | Record::LegacyMessage(l) => {
                let v1: AuthorizedMessageV1 = from_cbor_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.message, &v1.signature)
            }
            Record::LegacyMemberInfo(l) => {
                let v1: AuthorizedMemberInfo = from_cbor_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.member_info, &v1.signature)
            }
            Record::LegacyDirectMessage(l) => {
                let v1: AuthorizedDirectMessage = from_cbor_canonical(&l.v1.0)?;
                let owner_vk = VerifyingKey::from_bytes(&owner.0).ok()?;
                let m = &v1.message;
                let bytes = build_direct_message_signed_bytes(
                    m.sender,
                    m.recipient,
                    &owner_vk,
                    m.timestamp,
                    &m.ciphertext,
                )
                .ok()?;
                verify_bytes(&l.signer, &bytes, &sig_of(&v1.sender_signature))
            }
            Record::LegacyPurge(l) => {
                let v1: AuthorizedRecipientPurges = from_cbor_canonical(&l.v1.0)?;
                let owner_vk = VerifyingKey::from_bytes(&owner.0).ok()?;
                let bytes =
                    build_recipient_purges_signed_bytes(v1.recipient_id, &owner_vk, &v1.state)
                        .ok()?;
                verify_bytes(&l.signer, &bytes, &sig_of(&v1.recipient_signature))
            }
            Record::LegacySecretBlob(l) => {
                let v1: AuthorizedEncryptedSecretForMember = from_cbor_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.secret, &v1.owner_signature)
            }
            _ => return None,
        })
    };
    decoded().unwrap_or(false)
}
