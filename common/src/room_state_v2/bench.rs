//! Builds the largest valid state for a set of caps, for the work and size
//! measurements the design requires (validate and merge of the largest valid
//! state, and of the union of two, on the WASM build). Every record is at
//! its maximum size and every cap is full. Two variants share the owner and
//! the L1 records and differ everywhere else, so their union holds twice the
//! records `f` must choose between.

use super::config::{ceiling, ConfigurationV2, EPOCH_2026_SECS};
use super::encoding::{sign_payload, ByteBuf, Bytes32, Digest, Key, Nonce12, Token16};
use super::records::*;
use super::retain::Verifier;
use super::state::{normalize, RoomStateV2};
use ed25519_dalek::SigningKey;

/// The caps of the configuration the largest state is built under, plus the
/// length of each restore request's chain.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub max_members: u32,
    pub max_links: u32,
    pub max_owner_children: u32,
    pub max_children_per_link: u32,
    pub max_owner_bans: u32,
    pub max_deputy_bans: u32,
    pub max_bans_per_deputy: u32,
    pub max_member_bans: u32,
    pub max_bans_per_issuer: u32,
    pub max_secret_versions: u32,
    pub max_secret_blobs: u32,
    pub max_recent_messages: u32,
    pub max_message_size: u32,
    pub max_nickname_size: u32,
    pub max_direct_messages: u32,
    pub restore_requests: usize,
    pub restore_chain: usize,
    pub purge_tokens: usize,
    /// DM ciphertext bytes.
    pub dm_bytes: usize,
    /// Members with a purge envelope (the cap is one per kept member).
    pub purges: usize,
}

impl Caps {
    /// Every cap at its placeholder ceiling.
    pub fn ceilings() -> Self {
        use ceiling::*;
        Caps {
            max_members: MAX_MEMBERS,
            max_links: MAX_LINKS,
            max_owner_children: MAX_CHILDREN_PER_LINK,
            max_children_per_link: MAX_CHILDREN_PER_LINK,
            max_owner_bans: MAX_OWNER_BANS,
            max_deputy_bans: MAX_DEPUTY_BANS,
            max_bans_per_deputy: MAX_BANS_PER_DEPUTY,
            max_member_bans: MAX_MEMBER_BANS,
            max_bans_per_issuer: MAX_BANS_PER_ISSUER,
            max_secret_versions: MAX_SECRET_VERSIONS,
            max_secret_blobs: MAX_SECRET_BLOBS,
            max_recent_messages: MAX_RECENT_MESSAGES,
            max_message_size: MAX_MESSAGE_SIZE,
            max_nickname_size: MAX_NICKNAME_SIZE,
            max_direct_messages: MAX_DIRECT_MESSAGES,
            restore_requests: RESTORE_REQUESTS,
            restore_chain: MAX_CHAIN_DEPTH as usize,
            purge_tokens: PURGE_TOKENS,
            dm_bytes: DM_CIPHERTEXT,
            purges: MAX_MEMBERS as usize + 1,
        }
    }

    /// The Official room's current contents in v2 terms (the inventory of
    /// 2026-10-23: 166 members, 200 owner and 200 deputy bans, 200 messages
    /// of up to 1,000 bytes, 300 DMs averaging 630 bytes, 18 purge envelopes,
    /// a public room), with the restore requests and checkpoint a year of
    /// maintenance would add.
    pub fn official_room() -> Self {
        Caps {
            max_members: 166,
            max_links: 166,
            max_owner_children: 166,
            max_children_per_link: 4_000,
            max_owner_bans: 200,
            max_deputy_bans: 200,
            max_bans_per_deputy: 134,
            max_member_bans: 1,
            max_bans_per_issuer: 1,
            max_secret_versions: 0,
            max_secret_blobs: 0,
            max_recent_messages: 200,
            max_message_size: 1_000,
            max_nickname_size: 50,
            max_direct_messages: 300,
            restore_requests: 10,
            restore_chain: 3,
            purge_tokens: 50,
            dm_bytes: 630,
            purges: 18,
        }
    }

    /// A candidate set of ceilings sized from the WASM measurement (see the
    /// measurement tool's README): about 3,900 signatures and 3 MB at most.
    pub fn proposed() -> Self {
        Caps {
            max_members: 300,
            max_links: 400,
            max_owner_children: 400,
            max_children_per_link: 400,
            max_owner_bans: 250,
            max_deputy_bans: 250,
            max_bans_per_deputy: 150,
            max_member_bans: 50,
            max_bans_per_issuer: 10,
            max_secret_versions: 32,
            max_secret_blobs: 400,
            max_recent_messages: 200,
            max_message_size: 2_000,
            max_nickname_size: 100,
            max_direct_messages: 300,
            restore_requests: 10,
            restore_chain: 8,
            purge_tokens: 200,
            dm_bytes: 4_096,
            purges: 301,
        }
    }

    /// Every count divided by `d` (sizes kept), for scaling measurements.
    pub fn scaled(self, d: u32) -> Self {
        let s = |x: u32| (x / d).max(1);
        let su = |x: usize| (x / d as usize).max(1);
        Caps {
            max_members: s(self.max_members),
            max_links: s(self.max_links),
            max_owner_children: s(self.max_owner_children),
            max_children_per_link: self.max_children_per_link,
            max_owner_bans: s(self.max_owner_bans),
            max_deputy_bans: s(self.max_deputy_bans),
            max_bans_per_deputy: s(self.max_bans_per_deputy),
            max_member_bans: s(self.max_member_bans),
            max_bans_per_issuer: self.max_bans_per_issuer,
            max_secret_versions: self.max_secret_versions,
            max_secret_blobs: s(self.max_secret_blobs),
            max_recent_messages: s(self.max_recent_messages),
            max_message_size: self.max_message_size,
            max_nickname_size: self.max_nickname_size,
            max_direct_messages: s(self.max_direct_messages),
            restore_requests: su(self.restore_requests),
            restore_chain: self.restore_chain,
            purge_tokens: self.purge_tokens,
            dm_bytes: self.dm_bytes,
            purges: su(self.purges),
        }
    }
}

/// A deterministic key for (domain, variant, index).
fn sk(domain: u8, variant: u8, i: u32) -> SigningKey {
    let mut seed = [0u8; 32];
    seed[0] = domain;
    seed[1] = variant;
    seed[2..6].copy_from_slice(&i.to_le_bytes());
    seed[31] = 0x5a;
    SigningKey::from_bytes(&seed)
}

fn k(s: &SigningKey) -> Key {
    Key::from(&s.verifying_key())
}

pub fn owner_key() -> SigningKey {
    sk(0, 0, 0)
}

fn bytes(n: usize, fill: u8) -> ByteBuf {
    ByteBuf(vec![fill; n])
}

/// The largest state of `caps`, variant `v` (0 or 1). Returns the state as
/// `f` normalizes it, and the number of records `f` dropped (0 when the
/// build is a valid state as built).
pub fn largest_state(caps: Caps, v: u8) -> (RoomStateV2, usize) {
    let osk = owner_key();
    let o = k(&osk);
    let clock_hours: u32 = 24 * 300;
    let mut recs: Vec<Record> = vec![];

    // Deputies: enough to hold every deputy ban at the per-deputy limit.
    let n_dep = caps
        .max_deputy_bans
        .div_ceil(caps.max_bans_per_deputy)
        .min(ceiling::AUTHORITY_KEYS as u32);
    let mut deputies: Vec<Key> = (0..n_dep).map(|i| k(&sk(1, 0, i))).collect();
    deputies.sort();
    let mut protected: Vec<Key> = (0..ceiling::AUTHORITY_KEYS as u32)
        .map(|i| k(&sk(2, 0, i)))
        .collect();
    protected.sort();

    let mut cfg = ConfigurationV2::defaults(caps.max_members);
    cfg.version = 1;
    let private = caps.max_secret_versions > 0;
    cfg.privacy_mode = private as u8;
    cfg.max_room_name = ceiling::MAX_ROOM_NAME;
    cfg.max_room_description = ceiling::MAX_ROOM_DESCRIPTION;
    let seal = |n: usize| {
        if private {
            sealed(n)
        } else {
            Sealed::Public(bytes(n, b'n'))
        }
    };
    cfg.display_name = seal(ceiling::MAX_ROOM_NAME as usize);
    cfg.display_description = Some(seal(ceiling::MAX_ROOM_DESCRIPTION as usize));
    cfg.max_members = caps.max_members;
    cfg.max_links = caps.max_links;
    cfg.max_links_per_member = ceiling::MAX_LINKS_PER_MEMBER;
    cfg.max_owner_children = caps.max_owner_children;
    cfg.max_children_per_link = caps.max_children_per_link;
    cfg.max_chain_depth = ceiling::MAX_CHAIN_DEPTH;
    cfg.max_owner_bans = caps.max_owner_bans;
    cfg.max_deputy_bans = caps.max_deputy_bans;
    cfg.max_bans_per_deputy = caps.max_bans_per_deputy;
    cfg.max_member_bans = caps.max_member_bans;
    cfg.max_bans_per_issuer = caps.max_bans_per_issuer;
    cfg.max_secret_versions = caps.max_secret_versions;
    cfg.max_secret_blobs = caps.max_secret_blobs;
    cfg.max_recent_messages = caps.max_recent_messages;
    cfg.max_message_size = caps.max_message_size;
    cfg.max_nickname_size = caps.max_nickname_size;
    cfg.max_direct_messages = caps.max_direct_messages;
    cfg.deputies = deputies.clone();
    cfg.protected = protected;
    cfg.legacy_window_open = false;
    cfg.legacy_window_closed_at = clock_hours;
    let mut exceptions: Vec<(Digest, u32)> = (0..ceiling::CHILD_EXCEPTIONS as u32)
        .map(|i| (Bytes32([i as u8; 32]), 1))
        .collect();
    exceptions.sort();
    exceptions.dedup_by(|a, b| a.0 == b.0);
    cfg.child_exceptions = exceptions;
    recs.push(Record::Configuration(Signed {
        sig: sign_payload(tag::CONFIGURATION, &o, &cfg, &osk),
        payload: cfg.clone(),
    }));
    let p = ClockP { hours: clock_hours };
    recs.push(Record::Clock(Signed {
        sig: sign_payload(tag::CLOCK, &o, &p, &osk),
        payload: p,
    }));
    let p = UpgradeP {
        version: 1,
        new_address: Bytes32([7; 32]),
    };
    recs.push(Record::Upgrade(Signed {
        sig: sign_payload(tag::UPGRADE, &o, &p, &osk),
        payload: p,
    }));
    for ver in 1..=caps.max_secret_versions {
        let p = SecretVersionP {
            version: ver,
            cipher: 0,
        };
        recs.push(Record::SecretVersion(Signed {
            sig: sign_payload(tag::SECRET_VERSION, &o, &p, &osk),
            payload: p,
        }));
    }

    // Owner and deputy bans on keys that are never linked.
    for i in 0..caps.max_owner_bans {
        recs.push(ban(&o, &osk, Issuer::Owner(()), k(&sk(3, v, i)), i, v));
    }
    for i in 0..caps.max_deputy_bans {
        let d = (i / caps.max_bans_per_deputy) as usize % deputies.len().max(1);
        let dsk = (0..n_dep)
            .map(|j| sk(1, 0, j))
            .find(|s| k(s) == deputies[d])
            .unwrap();
        recs.push(ban(
            &o,
            &dsk,
            Issuer::Deputy(deputies[d]),
            k(&sk(4, v, i)),
            i,
            v,
        ));
    }

    // Links: owner children first (they become the members), the rest as
    // children of those, each with its own key.
    let n1 = caps.max_owner_children.min(caps.max_links);
    let mut links: Vec<(AcceptedLink, SigningKey)> = vec![];
    for i in 0..n1 {
        let msk = sk(5, 0, i);
        links.push((accepted(&o, &osk, Parent::Owner(()), i, &msk, i), msk));
    }
    let mut depth1: Vec<Digest> = links
        .iter()
        .map(|(l, _)| Record::Link(l.clone()).digest())
        .collect();
    let mut i = 0u32;
    while (links.len() as u32) < caps.max_links {
        let parent = (i / caps.max_children_per_link) as usize % depth1.len();
        let (_, psk) = &links[parent];
        let psk = psk.clone();
        let msk = sk(6, v, i);
        let l = accepted(&o, &psk, Parent::Link(depth1[parent]), i, &msk, 10_000 + i);
        links.push((l, msk));
        i += 1;
    }
    depth1.truncate(n1 as usize);
    for (l, msk) in &links {
        recs.push(Record::Link(l.clone()));
        let p = ActivityP {
            member: k(msk),
            hours: clock_hours - v as u32,
        };
        recs.push(Record::Activity(Signed {
            sig: sign_payload(tag::ACTIVITY, &o, &p, msk),
            payload: p,
        }));
    }
    let members: Vec<&SigningKey> = links
        .iter()
        .take(caps.max_members as usize)
        .map(|x| &x.1)
        .collect();

    // Member bans: issuers are the first members, all active.
    for i in 0..caps.max_member_bans {
        let issuer = (i / caps.max_bans_per_issuer) as usize % links.len();
        let (l, isk) = &links[issuer];
        let issuer = Issuer::Member(MemberIssuer {
            link: Record::Link(l.clone()).digest(),
            key: k(isk),
        });
        recs.push(ban(&o, isk, issuer, k(&sk(7, v, i)), i, v));
    }

    // Restore requests: chains of links that are not admitted, from the
    // retired link up to a link the owner invited.
    for r in 0..caps.restore_requests as u32 {
        let mut chain: Vec<ChainLink> = vec![];
        let mut parent = Parent::Owner(());
        let mut inviter = osk.clone();
        for j in 0..caps.restore_chain as u32 {
            let ck = sk(8, v, r * 64 + j);
            let l = accepted(&o, &inviter, parent.clone(), j, &ck, 1_000_000 + r * 64 + j);
            parent = Parent::Link(Record::Link(l.clone()).digest());
            inviter = ck;
            chain.push(ChainLink::Accepted(l));
        }
        chain.reverse();
        let p = RestoreP {
            chain,
            hours: clock_hours,
            clock_ref: clock_hours,
        };
        recs.push(Record::Restore(Signed {
            sig: sign_payload(tag::RESTORE, &o, &p, &inviter),
            payload: p,
        }));
    }

    // Member info for every member and the owner.
    for m in members.iter().copied().chain(std::iter::once(&osk)) {
        let p = MemberInfoP {
            member: k(m),
            version: 1 + v as u32,
            nickname: seal(caps.max_nickname_size as usize),
        };
        recs.push(Record::MemberInfo(Signed {
            sig: sign_payload(tag::MEMBER_INFO, &o, &p, m),
            payload: p,
        }));
    }

    // Messages, private, at the size limit.
    for i in 0..caps.max_recent_messages {
        let a = members[i as usize % members.len()];
        let p = MessageP {
            author: k(a),
            time: EPOCH_2026_SECS + clock_hours as u64 * 3600 - i as u64 * 60 - v as u64,
            clock_ref: clock_hours,
            body: if private {
                MessageBody::Private(PrivateBody {
                    content_type: 1,
                    content_version: 1,
                    ciphertext: bytes(caps.max_message_size as usize, 0xee),
                    nonce: Nonce12([v; 12]),
                    secret_version: 1,
                })
            } else {
                MessageBody::Public(PublicBody {
                    content_type: 1,
                    content_version: 1,
                    data: bytes(caps.max_message_size as usize, 0xee),
                })
            },
        };
        recs.push(Record::Message(Signed {
            sig: sign_payload(tag::MESSAGE, &o, &p, a),
            payload: p,
        }));
    }

    // DMs at the ciphertext limit, spread so no pair reaches its cap.
    for i in 0..caps.max_direct_messages {
        let a = members[i as usize % members.len()];
        let b = members[(i as usize + 1) % members.len()];
        let b = if k(a) == k(b) { &osk } else { b };
        let p = DirectMessageP {
            sender: k(a),
            recipient: k(b),
            time: EPOCH_2026_SECS + clock_hours as u64 * 3600 - i as u64 * 60 - v as u64,
            clock_ref: clock_hours,
            ciphertext: bytes(caps.dm_bytes, 0xdd),
        };
        recs.push(Record::DirectMessage(Signed {
            sig: sign_payload(tag::DIRECT_MESSAGE, &o, &p, a),
            payload: p,
        }));
    }

    // Purge envelopes listing tokens no DM has.
    for (j, m) in members
        .iter()
        .copied()
        .chain(std::iter::once(&osk))
        .take(caps.purges)
        .enumerate()
    {
        let mut purged: Vec<Token16> = (0..caps.purge_tokens as u32)
            .map(|t| {
                let mut b = [0u8; 16];
                b[..4].copy_from_slice(&t.to_le_bytes());
                b[4..8].copy_from_slice(&(j as u32).to_le_bytes());
                b[8] = v;
                Token16(b)
            })
            .collect();
        purged.sort();
        let p = PurgeP {
            recipient: k(m),
            version: 1 + v as u64,
            purged,
        };
        recs.push(Record::Purge(Signed {
            sig: sign_payload(tag::PURGE, &o, &p, m),
            payload: p,
        }));
    }

    // Secret blobs: members times the newest versions.
    for i in 0..caps.max_secret_blobs * private as u32 {
        let m = members[i as usize % members.len()];
        let ver = caps.max_secret_versions - (i / members.len() as u32) % caps.max_secret_versions;
        let p = SecretBlobP {
            recipient: k(m),
            version: ver,
            ciphertext: bytes(ceiling::SECRET_BLOB_CIPHERTEXT, 0xbb + v),
            nonce: Nonce12([v; 12]),
            ephemeral_key: Bytes32([v; 32]),
        };
        recs.push(Record::SecretBlob(Signed {
            sig: sign_payload(tag::SECRET_BLOB, &o, &p, &osk),
            payload: p,
        }));
    }

    // The checkpoint keeps every link (it retires nothing).
    let mut keep: Vec<Digest> = links
        .iter()
        .map(|(l, _)| Record::Link(l.clone()).digest())
        .collect();
    keep.sort();
    keep.truncate(caps.max_links as usize);
    let p = CheckpointP {
        version: 1,
        retire_before: 0,
        keep,
    };
    recs.push(Record::Checkpoint(Signed {
        sig: sign_payload(tag::CHECKPOINT, &o, &p, &osk),
        payload: p,
    }));

    // Roster batches attesting digests (no legacy record is present).
    let mut entries: Vec<RosterEntry> = vec![];
    for kind in LEGACY_KINDS {
        for j in 0..super::retain::roster_ceiling(kind) as u32 {
            let mut d = [0u8; 32];
            d[0] = kind as u8;
            d[1..5].copy_from_slice(&j.to_le_bytes());
            entries.push(RosterEntry {
                kind,
                digest: Bytes32(d),
            });
        }
    }
    entries.sort_by_key(|e| (e.kind, e.digest));
    for (seq, batch) in entries.chunks(ceiling::ROSTER_BATCH_ENTRIES).enumerate() {
        let p = RosterP {
            seq: seq as u32,
            batch: batch.to_vec(),
        };
        recs.push(Record::Roster(Signed {
            sig: sign_payload(tag::ROSTER, &o, &p, &osk),
            payload: p,
        }));
    }

    let n = recs.len();
    let (s, _) = normalize(&o, recs, &mut Verifier::new());
    let dropped = n - s.records.len();
    (s, dropped)
}

fn sealed(declared: usize) -> Sealed {
    Sealed::Private(SealedPrivate {
        ciphertext: bytes(declared + AEAD_TAG, 0xcc),
        nonce: Nonce12([1; 12]),
        secret_version: 1,
        declared_len: declared as u32,
    })
}

fn ban(o: &Key, signer: &SigningKey, issuer: Issuer, target: Key, i: u32, v: u8) -> Record {
    let p = BanP {
        target: BanTarget::Key(target),
        issuer,
        time: EPOCH_2026_SECS + 3600 * 24 * 299 - i as u64 - v as u64,
        clock_ref: 24 * 300,
        reason: bytes(ceiling::BAN_REASON, 0xab),
    };
    Record::Ban(Signed {
        sig: sign_payload(tag::BAN, o, &p, signer),
        payload: p,
    })
}

fn accepted(
    o: &Key,
    inviter: &SigningKey,
    parent: Parent,
    seq: u32,
    member: &SigningKey,
    salt: u32,
) -> AcceptedLink {
    let k0 = sk(9, 0, salt);
    let ip = InvitationP {
        inviter: k(inviter),
        invite_key: k(&k0),
        parent,
        seq,
        clock: 0,
    };
    let invitation = Signed {
        sig: sign_payload(tag::INVITATION, o, &ip, inviter),
        payload: ip,
    };
    let acceptance = AcceptanceP {
        invitation: super::encoding::digest_of(&invitation),
        member: k(member),
    };
    AcceptedLink {
        invite_key_sig: sign_payload(tag::ACCEPTANCE, o, &acceptance, &k0),
        member_sig: sign_payload(tag::ACCEPTANCE, o, &acceptance, member),
        invitation,
        acceptance,
    }
}
