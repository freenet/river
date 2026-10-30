//! Legacy records: v1 records carried verbatim into a v2 state. Each wraps
//! the v1 bytes (which must be the canonical re-encoding of the v1 type) and
//! names its signer key, which must hash to the v1 id the record claims and
//! verify the v1 signature (strictly). Records whose v1 bytes name a room
//! owner must name this room's owner, so a v1 record cannot be replayed from
//! another room.
//!
//! `decode_*` checks everything but the signature; `verify` checks the
//! signature, so `f` can verify lazily in admission order.

use super::config::ceiling::{
    LEGACY_DM_CIPHERTEXT, LEGACY_MESSAGE_SIZE, LEGACY_PURGE_TOKENS, SECRET_BLOB_CIPHERTEXT,
};
use super::encoding::{legacy_id, verify_bytes, Key, Sig};
use super::fastcbor;
use super::records::{Legacy, Record};
use crate::room_state::ban::AuthorizedUserBan;
use crate::room_state::direct_messages::{
    build_direct_message_signed_bytes, build_recipient_purges_signed_bytes,
    AuthorizedDirectMessage, AuthorizedRecipientPurges, PurgeToken,
};
use crate::room_state::member::{AuthorizedMember, MemberId};
use crate::room_state::member_info::AuthorizedMemberInfo;
use crate::room_state::message::{AuthorizedMessageV1, RoomMessageBody};
use crate::room_state::secret::AuthorizedEncryptedSecretForMember;
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

/// The signed part of a decoded v1 record and its signature, kept from the
/// structural decode so the signature check, which `f` runs lazily in
/// admission order, does not decode the v1 bytes a second time.
pub struct V1Sig {
    inner: Inner,
    sig: ed25519_dalek::Signature,
}

enum Inner {
    Member(crate::room_state::member::Member),
    Ban(crate::room_state::ban::UserBan),
    Message(crate::room_state::message::MessageV1),
    MemberInfo(crate::room_state::member_info::MemberInfo),
    Dm(crate::room_state::direct_messages::DirectMessage),
    Purge(
        MemberId,
        crate::room_state::direct_messages::RecipientPurges,
    ),
    Blob(crate::room_state::secret::EncryptedSecretForMemberV1),
}

impl V1Sig {
    /// `verify_strict` of the v1 signature under `signer`.
    pub fn verify(&self, owner: &Key, signer: &Key) -> bool {
        let owner_vk = || VerifyingKey::from_bytes(&owner.0).ok();
        let msg = match &self.inner {
            Inner::Member(x) => fastcbor::to_vec(x),
            Inner::Ban(x) => fastcbor::to_vec(x),
            Inner::Message(x) => fastcbor::to_vec(x),
            Inner::MemberInfo(x) => fastcbor::to_vec(x),
            Inner::Blob(x) => fastcbor::to_vec(x),
            Inner::Dm(m) => {
                let Some(o) = owner_vk() else { return false };
                match build_direct_message_signed_bytes(
                    m.sender,
                    m.recipient,
                    &o,
                    m.timestamp,
                    &m.ciphertext,
                ) {
                    Ok(b) => b,
                    Err(_) => return false,
                }
            }
            Inner::Purge(r, state) => {
                let Some(o) = owner_vk() else { return false };
                match build_recipient_purges_signed_bytes(*r, &o, state) {
                    Ok(b) => b,
                    Err(_) => return false,
                }
            }
        };
        verify_bytes(signer, &msg, &sig_of(&self.sig))
    }
}

/// The longest array a legacy record may hold, per kind, refused at the
/// array's header before it is walked. A v1 signature is 64 elements. A
/// record over its kind's content bound (decision (m)) was dropped by `f`
/// anyway, after decoding it element by element. Member info also holds a
/// deputy list, and its nickname is over the bound well before this.
const SIGNATURE_ELEMENTS: u64 = 64;
const MEMBER_INFO_ELEMENTS: u64 = 1_000;

/// Whether a legacy record is well formed apart from the array limits above:
/// the conversion report tells "too large" from "undecodable" with it.
pub fn decodes_past_the_limits(r: &Record) -> bool {
    match r {
        Record::LegacyLink(l) => fastcbor::from_canonical::<AuthorizedMember>(&l.v1.0).is_some(),
        Record::LegacyBan(l) => fastcbor::from_canonical::<AuthorizedUserBan>(&l.v1.0).is_some(),
        Record::LegacyActivity(l) | Record::LegacyMessage(l) => {
            fastcbor::from_canonical::<AuthorizedMessageV1>(&l.v1.0).is_some()
        }
        Record::LegacyMemberInfo(l) => {
            fastcbor::from_canonical::<AuthorizedMemberInfo>(&l.v1.0).is_some()
        }
        Record::LegacyDirectMessage(l) => {
            fastcbor::from_canonical::<AuthorizedDirectMessage>(&l.v1.0).is_some()
        }
        Record::LegacyPurge(l) => {
            fastcbor::from_canonical::<AuthorizedRecipientPurges>(&l.v1.0).is_some()
        }
        Record::LegacySecretBlob(l) => {
            fastcbor::from_canonical::<AuthorizedEncryptedSecretForMember>(&l.v1.0).is_some()
        }
        _ => false,
    }
}

pub struct LegacyLinkView {
    pub v1: Option<V1Sig>,
    pub member: Key,
    pub inviter: Key,
}

pub fn decode_link(owner: &Key, l: &Legacy) -> Option<LegacyLinkView> {
    let v1: AuthorizedMember = fastcbor::from_canonical_within(&l.v1.0, SIGNATURE_ELEMENTS)?;
    let m = &v1.member;
    if m.owner_member_id != id(owner) || m.invited_by != id(&l.signer) {
        return None;
    }
    Some(LegacyLinkView {
        member: Key::from(&m.member_vk),
        inviter: l.signer,
        v1: Some(V1Sig {
            inner: Inner::Member(v1.member),
            sig: v1.signature,
        }),
    })
}

pub struct LegacyBanView {
    pub v1: Option<V1Sig>,
    pub target: i64,
    pub time: u64,
}

pub fn decode_ban(owner: &Key, l: &Legacy) -> Option<LegacyBanView> {
    let v1: AuthorizedUserBan = fastcbor::from_canonical_within(&l.v1.0, SIGNATURE_ELEMENTS)?;
    if v1.ban.owner_member_id != id(owner) || v1.banned_by != id(&l.signer) {
        return None;
    }
    Some(LegacyBanView {
        target: v1.ban.banned_user.0 .0,
        time: secs(v1.ban.banned_at),
        v1: Some(V1Sig {
            inner: Inner::Ban(v1.ban),
            sig: v1.signature,
        }),
    })
}

pub struct LegacyMessageView {
    pub v1: Option<V1Sig>,
    pub sig: Sig,
    pub author: Key,
    pub time: u64,
    pub body: RoomMessageBody,
}

pub fn decode_message(owner: &Key, l: &Legacy) -> Option<LegacyMessageView> {
    let v1: AuthorizedMessageV1 =
        fastcbor::from_canonical_within(&l.v1.0, LEGACY_MESSAGE_SIZE as u64)?;
    if v1.message.room_owner != id(owner) || v1.message.author != id(&l.signer) {
        return None;
    }
    Some(LegacyMessageView {
        sig: sig_of(&v1.signature),
        author: l.signer,
        time: secs(v1.message.time),
        body: v1.message.content.clone(),
        v1: Some(V1Sig {
            inner: Inner::Message(v1.message),
            sig: v1.signature,
        }),
    })
}

pub struct LegacyMemberInfoView {
    pub v1: Option<V1Sig>,
    /// The v1 deputy list, by v1 id.
    pub deputies: Vec<i64>,
    pub sig: Sig,
    pub member: Key,
    pub version: u32,
    pub declared_len: usize,
}

pub fn decode_member_info(_owner: &Key, l: &Legacy) -> Option<LegacyMemberInfoView> {
    let v1: AuthorizedMemberInfo = fastcbor::from_canonical_within(&l.v1.0, MEMBER_INFO_ELEMENTS)?;
    if v1.member_info.member_id != id(&l.signer)
        || v1.member_info.deputies.len() > crate::room_state::member_info::MAX_DEPUTIES
        || !sealed_v1_ok(&v1.member_info.preferred_nickname)
    {
        return None;
    }
    Some(LegacyMemberInfoView {
        sig: sig_of(&v1.signature),
        member: l.signer,
        version: v1.member_info.version,
        declared_len: v1.member_info.preferred_nickname.declared_len(),
        deputies: v1.member_info.deputies.iter().map(|d| d.0 .0).collect(),
        v1: Some(V1Sig {
            inner: Inner::MemberInfo(v1.member_info),
            sig: v1.signature,
        }),
    })
}

pub struct LegacyDmView {
    pub v1: Option<V1Sig>,
    pub sender: Key,
    pub sender_id: i64,
    pub recipient_id: i64,
    pub time: u64,
    pub ciphertext_len: usize,
    pub sig: Sig,
    pub token: [u8; 16],
}

pub fn decode_dm(_owner: &Key, l: &Legacy) -> Option<LegacyDmView> {
    let v1: AuthorizedDirectMessage =
        fastcbor::from_canonical_within(&l.v1.0, LEGACY_DM_CIPHERTEXT as u64)?;
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
        v1: Some(V1Sig {
            inner: Inner::Dm(v1.message),
            sig: v1.sender_signature,
        }),
    })
}

pub struct LegacyPurgeView {
    pub v1: Option<V1Sig>,
    pub sig: Sig,
    pub recipient_id: i64,
    pub version: u64,
    pub tokens: Vec<[u8; 16]>,
}

pub fn decode_purge(_owner: &Key, l: &Legacy) -> Option<LegacyPurgeView> {
    let v1: AuthorizedRecipientPurges =
        fastcbor::from_canonical_within(&l.v1.0, LEGACY_PURGE_TOKENS as u64)?;
    if v1.recipient_id != id(&l.signer) || v1.state.version == 0 {
        return None;
    }
    Some(LegacyPurgeView {
        sig: sig_of(&v1.recipient_signature),
        recipient_id: v1.recipient_id.0 .0,
        version: v1.state.version,
        tokens: v1.state.purged.iter().map(|t| t.0).collect(),
        v1: Some(V1Sig {
            inner: Inner::Purge(v1.recipient_id, v1.state),
            sig: v1.recipient_signature,
        }),
    })
}

pub struct LegacyBlobView {
    pub v1: Option<V1Sig>,
    pub sig: Sig,
    pub recipient_id: i64,
    pub version: u32,
    pub ciphertext_len: usize,
}

pub fn decode_blob(owner: &Key, l: &Legacy) -> Option<LegacyBlobView> {
    if l.signer != *owner {
        return None;
    }
    let v1: AuthorizedEncryptedSecretForMember =
        fastcbor::from_canonical_within(&l.v1.0, SECRET_BLOB_CIPHERTEXT as u64)?;
    Some(LegacyBlobView {
        sig: sig_of(&v1.owner_signature),
        recipient_id: v1.secret.member_id.0 .0,
        version: v1.secret.secret_version,
        ciphertext_len: v1.secret.ciphertext.len(),
        v1: Some(V1Sig {
            inner: Inner::Blob(v1.secret),
            sig: v1.owner_signature,
        }),
    })
}

fn strict<T: serde::Serialize>(signer: &Key, inner: &T, sig: &ed25519_dalek::Signature) -> bool {
    verify_bytes(signer, &fastcbor::to_vec(inner), &sig_of(sig))
}

/// The v1 signature of a legacy record, checked strictly under its named
/// signer. False for anything that does not decode.
pub fn verify(owner: &Key, rec: &Record) -> bool {
    let decoded = || -> Option<bool> {
        Some(match rec {
            Record::LegacyLink(l) => {
                let v1: AuthorizedMember = fastcbor::from_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.member, &v1.signature)
            }
            Record::LegacyBan(l) => {
                let v1: AuthorizedUserBan = fastcbor::from_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.ban, &v1.signature)
            }
            Record::LegacyActivity(l) | Record::LegacyMessage(l) => {
                let v1: AuthorizedMessageV1 = fastcbor::from_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.message, &v1.signature)
            }
            Record::LegacyMemberInfo(l) => {
                let v1: AuthorizedMemberInfo = fastcbor::from_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.member_info, &v1.signature)
            }
            Record::LegacyDirectMessage(l) => {
                let v1: AuthorizedDirectMessage = fastcbor::from_canonical(&l.v1.0)?;
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
                let v1: AuthorizedRecipientPurges = fastcbor::from_canonical(&l.v1.0)?;
                let owner_vk = VerifyingKey::from_bytes(&owner.0).ok()?;
                let bytes =
                    build_recipient_purges_signed_bytes(v1.recipient_id, &owner_vk, &v1.state)
                        .ok()?;
                verify_bytes(&l.signer, &bytes, &sig_of(&v1.recipient_signature))
            }
            Record::LegacySecretBlob(l) => {
                let v1: AuthorizedEncryptedSecretForMember = fastcbor::from_canonical(&l.v1.0)?;
                strict(&l.signer, &v1.secret, &v1.owner_signature)
            }
            _ => return None,
        })
    };
    decoded().unwrap_or(false)
}
