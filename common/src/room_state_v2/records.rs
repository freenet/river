//! The v2 record kinds. A state is a set of these; `retain::f` decides which
//! of a set survive. Every signed v2 payload is signed over
//! [`signing_bytes`](super::encoding::signing_bytes) with its own tag, so the
//! owner key and the record kind are part of what is signed.

use super::config::ConfigurationV2;
use super::encoding::{
    compact_enum, compact_struct, digest_of, ByteBuf, Bytes32, Digest, Key, Nonce12, Sig, Token16,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Signing tags, one per signed payload type. Never reuse a value.
pub mod tag {
    pub const CONFIGURATION: u8 = 1;
    pub const CLOCK: u8 = 2;
    pub const CHECKPOINT: u8 = 3;
    pub const UPGRADE: u8 = 4;
    pub const ROSTER: u8 = 5;
    pub const SECRET_VERSION: u8 = 6;
    pub const BAN: u8 = 7;
    pub const INVITATION: u8 = 8;
    pub const ACCEPTANCE: u8 = 9;
    pub const ACTIVITY: u8 = 10;
    pub const RESTORE: u8 = 11;
    pub const MEMBER_INFO: u8 = 12;
    pub const MESSAGE: u8 = 13;
    pub const DIRECT_MESSAGE: u8 = 14;
    pub const PURGE: u8 = 15;
    pub const SECRET_BLOB: u8 = 16;
}

/// A payload with one signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed<P> {
    pub payload: P,
    pub sig: Sig,
}

impl<P: Serialize> Serialize for Signed<P> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        (&self.payload, &self.sig).serialize(s)
    }
}

impl<'de, P: Deserialize<'de>> Deserialize<'de> for Signed<P> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (payload, sig) = <(P, Sig)>::deserialize(d)?;
        Ok(Signed { payload, sig })
    }
}

compact_struct! {
    /// Owner-signed. The highest `hours` is the room's `Clock`.
    pub struct ClockP { pub hours: u32 }
}

compact_struct! {
    /// Owner-signed. Retires every link whose invitation clock (a legacy
    /// link: 0) is below `retire_before`, except the links in `keep`
    /// (sorted, no duplicates).
    pub struct CheckpointP { pub version: u32, pub retire_before: u32, pub keep: Vec<Digest> }
}

compact_struct! {
    /// Owner-signed pointer to the room's next contract.
    pub struct UpgradeP { pub version: u32, pub new_address: Bytes32 }
}

/// The record kinds a roster batch can attest. Encoded as its `u8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LegacyKind {
    Link = 0,
    Ban = 1,
    Activity = 2,
    MemberInfo = 3,
    Message = 4,
    DirectMessage = 5,
    Purge = 6,
    SecretBlob = 7,
}

impl Serialize for LegacyKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for LegacyKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let t = u8::deserialize(d)?;
        LEGACY_KINDS
            .get(t as usize)
            .copied()
            .ok_or_else(|| serde::de::Error::custom(format!("unknown legacy kind {t}")))
    }
}

pub const LEGACY_KINDS: [LegacyKind; 8] = [
    LegacyKind::Link,
    LegacyKind::Ban,
    LegacyKind::Activity,
    LegacyKind::MemberInfo,
    LegacyKind::Message,
    LegacyKind::DirectMessage,
    LegacyKind::Purge,
    LegacyKind::SecretBlob,
];

compact_struct! {
    /// One attested legacy record: its kind and record digest.
    pub struct RosterEntry { pub kind: LegacyKind, pub digest: Digest }
}

compact_struct! {
    /// Owner-signed batch of attested legacy records (non-empty, sorted, no
    /// duplicates).
    pub struct RosterP { pub seq: u32, pub batch: Vec<RosterEntry> }
}

compact_struct! {
    /// Owner-signed record of a room secret version (private rooms).
    pub struct SecretVersionP { pub version: u32, pub cipher: u8 }
}

compact_struct! {
    /// The invitation a ban names: its digest and its inviter key, so the
    /// ban's effect and its protection check read only the ban's bytes.
    pub struct InvitationTarget { pub invitation: Digest, pub inviter: Key }
}

compact_enum! {
    /// What a ban removes.
    pub enum BanTarget {
        0 => Key(Key),
        1 => Invitation(InvitationTarget),
        /// A v1 64-bit member id (legacy bans only): every key hashing to it.
        2 => LegacyId(i64),
    }
}

compact_struct! {
    /// A member-ban issuer: the issuing link and its member key (which signs).
    pub struct MemberIssuer { pub link: Digest, pub key: Key }
}

compact_enum! {
    /// Who issued a v2 ban. The owner and a deputy sign with their own key;
    /// a member signs with the member key of the link it issues through.
    pub enum Issuer {
        0 => Owner(()),
        1 => Deputy(Key),
        2 => Member(MemberIssuer),
    }
}

compact_struct! {
    /// A v2 ban. `time` is seconds since the Unix epoch; `clock_ref` is the
    /// signer's view of `Clock` (hours since 2026-01-01).
    pub struct BanP {
        pub target: BanTarget,
        pub issuer: Issuer,
        pub time: u64,
        pub clock_ref: u32,
        pub reason: ByteBuf,
    }
}

compact_enum! {
    /// An invitation's parent: the owner, or the link of the inviter.
    pub enum Parent {
        0 => Owner(()),
        1 => Link(Digest),
    }
}

compact_struct! {
    /// Signed by `inviter`. `invite_key` is the one-time key K0 the
    /// invitation URL carries; `clock` is the inviter's view of `Clock`.
    pub struct InvitationP {
        pub inviter: Key,
        pub invite_key: Key,
        pub parent: Parent,
        pub seq: u32,
        pub clock: u32,
    }
}

compact_struct! {
    /// Signed by the one-time key and by the new member's key.
    pub struct AcceptanceP { pub invitation: Digest, pub member: Key }
}

compact_struct! {
    /// An accepted invitation: a link from the inviter to the member.
    pub struct AcceptedLink {
        pub invitation: Signed<InvitationP>,
        pub acceptance: AcceptanceP,
        pub invite_key_sig: Sig,
        pub member_sig: Sig,
    }
}

impl AcceptedLink {
    pub fn invitation_digest(&self) -> Digest {
        digest_of(&self.invitation)
    }
}

compact_struct! {
    /// A v1 record carried verbatim: `v1` must be the canonical CBOR of the
    /// v1 type, and `signer` the key that signed it and hashes to the v1 id
    /// the record claims.
    pub struct Legacy { pub signer: Key, pub v1: ByteBuf }
}

compact_struct! {
    /// Member-signed: the member was active at `hours` (since 2026-01-01).
    pub struct ActivityP { pub member: Key, pub hours: u32 }
}

compact_enum! {
    /// A link embedded in a restore request.
    pub enum ChainLink {
        0 => Accepted(AcceptedLink),
        1 => Legacy(Legacy),
    }
}

compact_struct! {
    /// Signed by the member of `chain[0]`: the retired link, then each
    /// retired ancestor, up to an admitted link or the owner.
    pub struct RestoreP { pub chain: Vec<ChainLink>, pub hours: u32, pub clock_ref: u32 }
}

compact_enum! {
    /// A value that is public or sealed under a room secret version.
    pub enum Sealed {
        0 => Public(ByteBuf),
        1 => Private(SealedPrivate),
    }
}

compact_struct! {
    pub struct SealedPrivate {
        pub ciphertext: ByteBuf,
        pub nonce: Nonce12,
        pub secret_version: u32,
        pub declared_len: u32,
    }
}

/// The AES-GCM tag a sealed value's ciphertext carries beyond its plaintext.
pub const AEAD_TAG: usize = 16;

impl Sealed {
    pub fn declared_len(&self) -> usize {
        match self {
            Sealed::Public(b) => b.0.len(),
            Sealed::Private(p) => p.declared_len as usize,
        }
    }

    /// Whether the ciphertext is no longer than the declared length plus the
    /// AEAD tag, so the declared length bounds the record's size.
    pub fn size_ok(&self) -> bool {
        match self {
            Sealed::Public(_) => true,
            Sealed::Private(p) => p.ciphertext.0.len() <= p.declared_len as usize + AEAD_TAG,
        }
    }
}

compact_struct! {
    /// Member-signed (the owner signs its own).
    pub struct MemberInfoP { pub member: Key, pub version: u32, pub nickname: Sealed }
}

compact_struct! {
    pub struct PublicBody { pub content_type: u32, pub content_version: u32, pub data: ByteBuf }
}

compact_struct! {
    pub struct PrivateBody {
        pub content_type: u32,
        pub content_version: u32,
        pub ciphertext: ByteBuf,
        pub nonce: Nonce12,
        pub secret_version: u32,
    }
}

compact_enum! {
    /// A message body. Actions (edit, react, delete, reply) are messages
    /// whose content names a target message id; the room never reads it.
    pub enum MessageBody {
        0 => Public(PublicBody),
        1 => Private(PrivateBody),
    }
}

impl MessageBody {
    pub fn content_len(&self) -> usize {
        match self {
            MessageBody::Public(b) => b.data.0.len(),
            MessageBody::Private(b) => b.ciphertext.0.len(),
        }
    }
}

compact_struct! {
    /// Author-signed. `time` in seconds since the Unix epoch.
    pub struct MessageP { pub author: Key, pub time: u64, pub clock_ref: u32, pub body: MessageBody }
}

compact_struct! {
    /// Sender-signed; `ciphertext` is sealed to the recipient.
    pub struct DirectMessageP {
        pub sender: Key,
        pub recipient: Key,
        pub time: u64,
        pub clock_ref: u32,
        pub ciphertext: ByteBuf,
    }
}

compact_struct! {
    /// Recipient-signed list of purged DM tokens (sorted, no duplicates).
    pub struct PurgeP { pub recipient: Key, pub version: u64, pub purged: Vec<Token16> }
}

compact_struct! {
    /// Owner-signed room secret sealed to one member.
    pub struct SecretBlobP {
        pub recipient: Key,
        pub version: u32,
        pub ciphertext: ByteBuf,
        pub nonce: Nonce12,
        pub ephemeral_key: Bytes32,
    }
}

compact_enum! {
    /// A v2 state record.
    pub enum Record {
        0 => Configuration(Signed<ConfigurationV2>),
        // Tag 1 is reserved: it held a v1 owner configuration while rooms
        // whose owner never converts were to migrate (dropped, decision (p)).
        2 => Clock(Signed<ClockP>),
        3 => Checkpoint(Signed<CheckpointP>),
        4 => Upgrade(Signed<UpgradeP>),
        5 => Roster(Signed<RosterP>),
        6 => SecretVersion(Signed<SecretVersionP>),
        7 => Ban(Signed<BanP>),
        8 => LegacyBan(Legacy),
        9 => Link(AcceptedLink),
        10 => LegacyLink(Legacy),
        11 => Activity(Signed<ActivityP>),
        12 => LegacyActivity(Legacy),
        13 => Restore(Signed<RestoreP>),
        14 => MemberInfo(Signed<MemberInfoP>),
        15 => LegacyMemberInfo(Legacy),
        16 => Message(Signed<MessageP>),
        17 => LegacyMessage(Legacy),
        18 => DirectMessage(Signed<DirectMessageP>),
        19 => LegacyDirectMessage(Legacy),
        20 => Purge(Signed<PurgeP>),
        21 => LegacyPurge(Legacy),
        22 => SecretBlob(Signed<SecretBlobP>),
        23 => LegacySecretBlob(Legacy),
    }
}

/// A record's kind, as the summary groups records.
pub type Kind = u8;

impl Record {
    pub fn kind(&self) -> Kind {
        match self {
            Record::Configuration(_) => 0,
            Record::Clock(_) => 2,
            Record::Checkpoint(_) => 3,
            Record::Upgrade(_) => 4,
            Record::Roster(_) => 5,
            Record::SecretVersion(_) => 6,
            Record::Ban(_) => 7,
            Record::LegacyBan(_) => 8,
            Record::Link(_) => 9,
            Record::LegacyLink(_) => 10,
            Record::Activity(_) => 11,
            Record::LegacyActivity(_) => 12,
            Record::Restore(_) => 13,
            Record::MemberInfo(_) => 14,
            Record::LegacyMemberInfo(_) => 15,
            Record::Message(_) => 16,
            Record::LegacyMessage(_) => 17,
            Record::DirectMessage(_) => 18,
            Record::LegacyDirectMessage(_) => 19,
            Record::Purge(_) => 20,
            Record::LegacyPurge(_) => 21,
            Record::SecretBlob(_) => 22,
            Record::LegacySecretBlob(_) => 23,
        }
    }

    /// The roster kind of a legacy record, if it is one the roster attests.
    pub fn legacy_kind(&self) -> Option<LegacyKind> {
        Some(match self {
            Record::LegacyLink(_) => LegacyKind::Link,
            Record::LegacyBan(_) => LegacyKind::Ban,
            Record::LegacyActivity(_) => LegacyKind::Activity,
            Record::LegacyMemberInfo(_) => LegacyKind::MemberInfo,
            Record::LegacyMessage(_) => LegacyKind::Message,
            Record::LegacyDirectMessage(_) => LegacyKind::DirectMessage,
            Record::LegacyPurge(_) => LegacyKind::Purge,
            Record::LegacySecretBlob(_) => LegacyKind::SecretBlob,
            _ => return None,
        })
    }

    /// BLAKE3-256 of the record's canonical bytes: its identity everywhere
    /// (summary, delta, link digest, message id, roster).
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}
