//! Builds universes of real signed v2 records, named like the executable
//! model's universes so the families port one to one. Keys `k0` (the owner)
//! to `k7` are fixed; every time is in hours (a model time `t` is the
//! instant `t` hours after 2026-01-01).

use crate::room_state::ban::{AuthorizedUserBan, UserBan};
use crate::room_state::member::{AuthorizedMember, Member, MemberId};
use crate::room_state_v2::config::{ConfigurationV2, EPOCH_2026_SECS};
use crate::room_state_v2::encoding::{sign_payload, to_cbor, ByteBuf, Digest, Key, Sig};
use crate::room_state_v2::records::*;
use ed25519_dalek::SigningKey;
use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};

pub const NK: usize = 8;

pub fn sk(i: usize) -> SigningKey {
    SigningKey::from_bytes(&[i as u8 + 1; 32])
}

pub fn key(i: usize) -> Key {
    Key::from(&sk(i).verifying_key())
}

pub fn owner() -> Key {
    key(0)
}

pub fn secs(hours: u32) -> u64 {
    EPOCH_2026_SECS + hours as u64 * 3600
}

/// A model configuration, as the model names its fields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cfg {
    pub max_members: u32,
    pub max_msgs: u32,
    pub max_dms: u32,
    pub max_owner_bans: u32,
    pub max_deputy_bans: u32,
    pub max_per_deputy: u32,
    pub max_member_bans: u32,
    pub max_per_issuer: u32,
    pub max_depth: u32,
    pub max_links: u32,
    pub max_links_per_member: u32,
    pub max_children: u32,
    pub owner_children: u32,
    pub deputies: u8,
    pub protected: u8,
    pub window_open: bool,
    pub ban_window: u32,
}

pub fn base() -> Cfg {
    Cfg {
        max_members: 3,
        max_msgs: 1,
        max_dms: 1,
        max_owner_bans: 1,
        max_deputy_bans: 1,
        max_per_deputy: 1,
        max_member_bans: 1,
        max_per_issuer: 1,
        max_depth: 3,
        max_links: 9,
        max_links_per_member: 2,
        max_children: 2,
        owner_children: 9,
        deputies: 0,
        protected: 0,
        window_open: false,
        ban_window: 3,
    }
}

pub fn with(f: impl Fn(&mut Cfg)) -> Cfg {
    let mut c = base();
    f(&mut c);
    c
}

fn keys_of(mask: u8) -> Vec<Key> {
    let mut v: Vec<Key> = (1..NK).filter(|k| mask & (1 << k) != 0).map(key).collect();
    v.sort();
    v
}

impl Cfg {
    pub fn to_v2(self, version: u32) -> ConfigurationV2 {
        let mut c = ConfigurationV2::defaults(self.max_members);
        c.version = version;
        c.max_members = self.max_members;
        c.max_recent_messages = self.max_msgs;
        c.max_direct_messages = self.max_dms.max(1);
        c.max_owner_bans = self.max_owner_bans;
        c.max_deputy_bans = self.max_deputy_bans;
        c.max_bans_per_deputy = self.max_per_deputy;
        c.max_member_bans = self.max_member_bans;
        c.max_bans_per_issuer = self.max_per_issuer;
        c.max_chain_depth = self.max_depth;
        c.max_links = self.max_links;
        c.max_links_per_member = self.max_links_per_member;
        c.max_children_per_link = self.max_children;
        c.max_owner_children = self.owner_children;
        c.deputies = keys_of(self.deputies);
        c.protected = keys_of(self.protected);
        c.legacy_window_open = self.window_open;
        c.member_ban_activity_window = self.ban_window;
        c
    }
}

/// A ban target in model terms.
#[derive(Clone, Copy, Debug)]
pub enum T {
    Key(usize),
    Inv(u8),
    /// A v1 64-bit id; a real id matches one key, so a set of keys maps to
    /// the lowest one.
    Keys(u8),
}

pub struct Builder {
    pub recs: Vec<Record>,
    pub names: Vec<String>,
    map: HashMap<String, usize>,
    invs: HashMap<u8, (Parent, usize, u32, u32)>,
    inv_digest: HashMap<u8, (Digest, Key)>,
    fix: Vec<(usize, u32, u32, Vec<String>)>,
    fix_roster: Vec<(usize, u32, Vec<String>)>,
    fix_restore: Vec<(usize, String, u32)>,
}

impl Builder {
    pub fn new() -> Self {
        Builder {
            recs: vec![],
            names: vec![],
            map: HashMap::new(),
            invs: HashMap::new(),
            inv_digest: HashMap::new(),
            fix: vec![],
            fix_roster: vec![],
            fix_restore: vec![],
        }
    }

    fn push(&mut self, name: String, short: Option<&str>, r: Record) -> usize {
        let i = self.recs.len();
        if let Some(s) = short {
            self.map.insert(s.to_string(), i);
        }
        self.names.push(name);
        self.recs.push(r);
        i
    }

    fn link_member(&self, i: usize) -> usize {
        let k = match &self.recs[i] {
            Record::Link(a) => a.acceptance.member,
            Record::LegacyLink(l) => {
                let v1: AuthorizedMember = ciborium::de::from_reader(&l.v1.0[..]).unwrap();
                Key::from(&v1.member.member_vk)
            }
            _ => panic!(),
        };
        (0..NK).find(|&j| key(j) == k).unwrap()
    }

    pub fn invitation(&mut self, inv: u8, parent: &str, seq: u32, clock: u32) {
        let (p, inviter) = if parent == "O" {
            (Parent::Owner(()), 0)
        } else {
            let i = self.map[parent];
            (Parent::Link(self.recs[i].digest()), self.link_member(i))
        };
        self.invs.insert(inv, (p, inviter, seq, clock));
    }

    pub fn acc(&mut self, name: &str, inv: u8, member: usize) {
        let (parent, inviter, seq, clock) = self.invs[&inv].clone();
        let o = owner();
        // The one-time key is a function of the invitation id.
        let k0 = SigningKey::from_bytes(&[200 + inv; 32]);
        let ip = InvitationP {
            inviter: key(inviter),
            invite_key: Key::from(&k0.verifying_key()),
            parent,
            seq,
            clock,
        };
        let invitation = Signed {
            sig: sign_payload(tag::INVITATION, &o, &ip, &sk(inviter)),
            payload: ip,
        };
        let inv_d = crate::room_state_v2::encoding::digest_of(&invitation);
        self.inv_digest.insert(inv, (inv_d, key(inviter)));
        let acceptance = AcceptanceP {
            invitation: inv_d,
            member: key(member),
        };
        let link = AcceptedLink {
            invite_key_sig: sign_payload(tag::ACCEPTANCE, &o, &acceptance, &k0),
            member_sig: sign_payload(tag::ACCEPTANCE, &o, &acceptance, &sk(member)),
            invitation,
            acceptance,
        };
        self.push(
            format!("{name}[k{member} I{inv}<-k{inviter} s{seq} c{clock}]"),
            Some(name),
            Record::Link(link),
        );
    }

    pub fn leg(&mut self, name: &str, member: usize, inviter: usize) {
        let m = Member {
            owner_member_id: MemberId::from(&sk(0).verifying_key()),
            invited_by: MemberId::from(&sk(inviter).verifying_key()),
            member_vk: sk(member).verifying_key(),
        };
        let am = AuthorizedMember::new(m, &sk(inviter));
        self.push(
            format!("{name}[k{member} leg<-k{inviter}]"),
            Some(name),
            Record::LegacyLink(Legacy {
                signer: key(inviter),
                v1: ByteBuf(to_cbor(&am)),
            }),
        );
    }

    pub fn act(&mut self, member: usize, hours: u32) {
        let p = ActivityP {
            member: key(member),
            hours,
        };
        let r = Record::Activity(Signed {
            sig: sign_payload(tag::ACTIVITY, &owner(), &p, &sk(member)),
            payload: p,
        });
        self.push(format!("act(k{member}@{hours})"), None, r);
    }

    pub fn clock(&mut self, hours: u32) {
        let p = ClockP { hours };
        let r = Record::Clock(Signed {
            sig: sign_payload(tag::CLOCK, &owner(), &p, &sk(0)),
            payload: p,
        });
        self.push(format!("clock({hours})"), None, r);
    }

    pub fn cfg(&mut self, version: u32, cfg: Cfg) {
        let p = cfg.to_v2(version);
        let r = Record::Configuration(Signed {
            sig: sign_payload(tag::CONFIGURATION, &owner(), &p, &sk(0)),
            payload: p,
        });
        self.push(format!("cfg v{version}"), None, r);
    }

    fn target(&self, t: T) -> (BanTarget, String) {
        match t {
            T::Key(k) => (BanTarget::Key(key(k)), format!("k{k}")),
            T::Keys(m) => {
                let k = (1..NK).find(|k| m & (1 << k) != 0).unwrap();
                (
                    BanTarget::LegacyId(crate::room_state_v2::encoding::legacy_id(&key(k))),
                    format!("id(k{k})"),
                )
            }
            T::Inv(v) => {
                let (d, k) = self.inv_digest[&v];
                (
                    BanTarget::Invitation(InvitationTarget {
                        invitation: d,
                        inviter: k,
                    }),
                    format!("I{v}"),
                )
            }
        }
    }

    fn ban(
        &mut self,
        name: String,
        short: &str,
        signer: usize,
        issuer: Issuer,
        t: T,
        time: u32,
        cref: u32,
    ) {
        let (target, _) = self.target(t);
        let p = BanP {
            target,
            issuer,
            time: secs(time),
            clock_ref: cref,
            reason: ByteBuf(vec![]),
        };
        let r = Record::Ban(Signed {
            sig: sign_payload(tag::BAN, &owner(), &p, &sk(signer)),
            payload: p,
        });
        self.push(name, Some(short), r);
    }

    fn legacy_ban(&mut self, name: String, short: &str, signer: usize, target: i64, time: u32) {
        let ub = UserBan {
            owner_member_id: MemberId::from(&sk(0).verifying_key()),
            banned_at: UNIX_EPOCH + Duration::from_secs(secs(time)),
            banned_user: MemberId(freenet_scaffold::util::FastHash(target)),
        };
        let ab =
            AuthorizedUserBan::new(ub, MemberId::from(&sk(signer).verifying_key()), &sk(signer));
        self.push(
            name,
            Some(short),
            Record::LegacyBan(Legacy {
                signer: key(signer),
                v1: ByteBuf(to_cbor(&ab)),
            }),
        );
    }

    fn id_of(&self, t: T) -> i64 {
        match self.target(t).0 {
            BanTarget::Key(k) => crate::room_state_v2::encoding::legacy_id(&k),
            BanTarget::LegacyId(x) => x,
            BanTarget::Invitation(_) => panic!("a legacy ban cannot target an invitation"),
        }
    }

    pub fn oban(&mut self, name: &str, t: T, time: u32, cref: u32) {
        let tn = self.target(t).1;
        let label = format!("{name}:oban({tn}@{time}r{cref})");
        if let T::Keys(_) = t {
            let id = self.id_of(t);
            self.legacy_ban(label, name, 0, id, time);
        } else {
            self.ban(label, name, 0, Issuer::Owner(()), t, time, cref);
        }
    }

    pub fn dban(&mut self, name: &str, deputy: usize, t: T, time: u32, cref: u32) {
        let tn = self.target(t).1;
        let label = format!("{name}:dban(k{deputy}->{tn}@{time}r{cref})");
        if let T::Keys(_) = t {
            let id = self.id_of(t);
            self.legacy_ban(label, name, deputy, id, time);
        } else {
            self.ban(
                label,
                name,
                deputy,
                Issuer::Deputy(key(deputy)),
                t,
                time,
                cref,
            );
        }
    }

    /// A member ban through link `issuer`. A legacy one names its signer
    /// only, so its issuer is the signer's canonical link.
    pub fn mban(&mut self, name: &str, issuer: &str, t: T, time: u32, cref: u32, legacy: bool) {
        let l = self.map[issuer];
        let k = self.link_member(l);
        let tn = self.target(t).1;
        let lg = if legacy { "legacy " } else { "" };
        let label = format!("{name}:{lg}mban({issuer}->{tn}@{time}r{cref})");
        if legacy {
            let id = self.id_of(t);
            self.legacy_ban(label, name, k, id, time);
        } else {
            let d = self.recs[l].digest();
            self.ban(
                label,
                name,
                k,
                Issuer::Member(MemberIssuer {
                    link: d,
                    key: key(k),
                }),
                t,
                time,
                cref,
            );
        }
    }

    pub fn checkpoint(&mut self, version: u32, retire_before: u32, keep: &[&str]) {
        let i = self.push(
            format!(
                "ckpt v{version}(retire<{retire_before} keep[{}])",
                keep.join(",")
            ),
            None,
            Record::Clock(Signed {
                payload: ClockP { hours: 0 },
                sig: Sig([0; 64]),
            }),
        );
        self.fix.push((
            i,
            version,
            retire_before,
            keep.iter().map(|s| s.to_string()).collect(),
        ));
    }

    pub fn restore(&mut self, link: &str, hours: u32) {
        let i = self.push(
            format!("restore({link}@{hours})"),
            None,
            Record::Clock(Signed {
                payload: ClockP { hours: 0 },
                sig: Sig([0; 64]),
            }),
        );
        self.fix_restore.push((i, link.to_string(), hours));
    }

    pub fn roster(&mut self, seq: u32, links: &[&str]) {
        let i = self.push(
            format!("roster s{seq}[{}]", links.join(",")),
            None,
            Record::Clock(Signed {
                payload: ClockP { hours: 0 },
                sig: Sig([0; 64]),
            }),
        );
        self.fix_roster
            .push((i, seq, links.iter().map(|s| s.to_string()).collect()));
    }

    pub fn msg(&mut self, author: usize, time: u32, cref: u32) {
        let p = MessageP {
            author: key(author),
            time: secs(time),
            clock_ref: cref,
            body: MessageBody::Public(PublicBody {
                content_type: 1,
                content_version: 1,
                data: ByteBuf(vec![time as u8]),
            }),
        };
        let r = Record::Message(Signed {
            sig: sign_payload(tag::MESSAGE, &owner(), &p, &sk(author)),
            payload: p,
        });
        let a = if author == 0 {
            "O".into()
        } else {
            format!("k{author}")
        };
        self.push(format!("msg({a}@{time}r{cref})"), None, r);
    }

    pub fn dm(&mut self, a: usize, b: usize, time: u32, cref: u32) {
        let p = DirectMessageP {
            sender: key(a),
            recipient: key(b),
            time: secs(time),
            clock_ref: cref,
            ciphertext: ByteBuf(vec![1, 2, 3]),
        };
        let r = Record::DirectMessage(Signed {
            sig: sign_payload(tag::DIRECT_MESSAGE, &owner(), &p, &sk(a)),
            payload: p,
        });
        self.push(format!("dm(k{a},k{b}@{time}r{cref})"), None, r);
    }

    /// Resolves records that name other records by their final digests.
    pub fn finish(mut self) -> Builder {
        let o = owner();
        for (i, version, retire_before, names) in std::mem::take(&mut self.fix) {
            let mut keep: Vec<Digest> = names
                .iter()
                .map(|n| self.recs[self.map[n]].digest())
                .collect();
            keep.sort();
            keep.dedup();
            let p = CheckpointP {
                version,
                retire_before,
                keep,
            };
            self.recs[i] = Record::Checkpoint(Signed {
                sig: sign_payload(tag::CHECKPOINT, &o, &p, &sk(0)),
                payload: p,
            });
        }
        for (i, seq, names) in std::mem::take(&mut self.fix_roster) {
            let mut batch: Vec<RosterEntry> = names
                .iter()
                .map(|n| {
                    let r = &self.recs[self.map[n]];
                    RosterEntry {
                        kind: r.legacy_kind().expect("a roster lists legacy records"),
                        digest: r.digest(),
                    }
                })
                .collect();
            batch.sort_by(|a, b| (a.kind, a.digest).cmp(&(b.kind, b.digest)));
            let p = RosterP { seq, batch };
            self.recs[i] = Record::Roster(Signed {
                sig: sign_payload(tag::ROSTER, &o, &p, &sk(0)),
                payload: p,
            });
        }
        for (i, link, hours) in std::mem::take(&mut self.fix_restore) {
            let l = self.map[&link];
            let member = self.link_member(l);
            let cl = match &self.recs[l] {
                Record::Link(a) => ChainLink::Accepted(a.clone()),
                Record::LegacyLink(x) => ChainLink::Legacy(x.clone()),
                _ => unreachable!(),
            };
            let p = RestoreP {
                chain: vec![cl],
                hours,
                clock_ref: 0,
            };
            self.recs[i] = Record::Restore(Signed {
                sig: sign_payload(tag::RESTORE, &o, &p, &sk(member)),
                payload: p,
            });
        }
        self
    }
}
