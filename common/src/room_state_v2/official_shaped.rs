//! A generated v1 River room shaped like the "Official" room (as of
//! 2026-10-23), with a known owner key and real signatures, for exercising
//! the v1 to v2 owner conversion at realistic scale (tests, corpora and the
//! measurement tool). Not used by the contract.
//!
//! Everything derives from `seed`: keys from a BLAKE3 hash of
//! `(seed, role, index)`, all other choices from a small seeded generator, so
//! one seed always gives byte-identical CBOR.
//!
//! Shape at scale 1.0:
//! - 169 current members: the owner invites the bot and 2 others; the bot
//!   invites 160 members (5 of them its deputies); one of the bot's invitees
//!   invites 4 members and one of the owner's invitees invites 2, so the
//!   deepest invite chain is 3 below the owner.
//! - 400 bans: 200 by the owner and 200 by the 5 deputies (134, 30, 20, 10, 6).
//!   Targets are not current members. 380 are bare generated keys; 20 are
//!   former members invited by the bot, banned by a deputy, whose
//!   `AuthorizedMember` records are held as ban evidence.
//! - 190 member info records: one per current member, one for the owner, and
//!   one per evidenced former member (v1 accepts a record only for those).
//! - 200 messages from 60 authors, 300 direct messages, 19 purge envelopes.

use crate::room_state::ban::{AuthorizedUserBan, UserBan};
use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use crate::room_state::direct_messages::{
    sign_direct_message, sign_recipient_purges, PurgeToken, RecipientPurges,
};
use crate::room_state::member::{AuthorizedMember, Member, MemberId};
use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use crate::room_state::ChatRoomStateV1;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};

/// A v1 room shaped like the Official room, plus the keys a test needs.
pub struct OfficialShaped {
    pub owner: SigningKey,
    pub v1: ChatRoomStateV1,
    /// The member that invites most members and appoints the deputies.
    pub bot: VerifyingKey,
    /// The bot's deputies (members), in key order.
    pub deputies: Vec<VerifyingKey>,
}

/// 2026-09-21 00:00 UTC; records are spread over the 25 days after it.
const T0: u64 = 1_790_000_000;
const SPAN: u64 = 25 * 86_400;

fn key(seed: u64, role: &str, index: usize) -> SigningKey {
    let mut h = blake3::Hasher::new();
    h.update(&seed.to_le_bytes());
    h.update(role.as_bytes());
    h.update(&(index as u64).to_le_bytes());
    SigningKey::from_bytes(h.finalize().as_bytes())
}

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
    fn time(&mut self) -> u64 {
        T0 + self.next() % SPAN
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, self.below(i + 1));
        }
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

struct Mem {
    sk: SigningKey,
    vk: VerifyingKey,
    id: MemberId,
}

fn mem(sk: SigningKey) -> Mem {
    let vk = sk.verifying_key();
    Mem {
        id: MemberId::from(&vk),
        sk,
        vk,
    }
}

fn st(secs: u64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Build the fixture. `scale` 1.0 is the Official room's size; smaller
/// values shrink every count proportionally (at least 1 of each kind).
#[allow(clippy::field_reassign_with_default)]
pub fn official_shaped(seed: u64, scale: f64) -> OfficialShaped {
    let n = |full: usize| -> usize { ((full as f64 * scale).round() as usize).max(1) };
    let mut rng = Rng(seed ^ 0xA5A5_5A5A_1234_5678);
    let owner = mem(key(seed, "owner", 0));
    let bot = mem(key(seed, "bot", 0));

    // ---- members ----
    let n_deputies = n(5);
    let n_bot_invitees = n(160).max(n_deputies + 1);
    let owner_invitees: Vec<Mem> = (0..n(2))
        .map(|i| mem(key(seed, "owner_invitee", i)))
        .collect();
    let bot_invitees: Vec<Mem> = (0..n_bot_invitees)
        .map(|i| mem(key(seed, "bot_invitee", i)))
        .collect();
    // Deputies are the first bot invitees; present them in key order.
    let mut deputy_idx: Vec<usize> = (0..n_deputies).collect();
    deputy_idx.sort_by_key(|&i| bot_invitees[i].vk.to_bytes());
    let deputies: Vec<&Mem> = deputy_idx.iter().map(|&i| &bot_invitees[i]).collect();
    let deep: Vec<Mem> = (0..n(4)).map(|i| mem(key(seed, "deep", i))).collect();
    let mid: Vec<Mem> = (0..n(2)).map(|i| mem(key(seed, "mid", i))).collect();

    let link = |m: &Mem, inviter: &Mem| {
        AuthorizedMember::new(
            Member {
                owner_member_id: owner.id,
                invited_by: inviter.id,
                member_vk: m.vk,
            },
            &inviter.sk,
        )
    };
    let mut s = ChatRoomStateV1::default();
    s.configuration = AuthorizedConfigurationV1::new(
        Configuration {
            owner_member_id: owner.id,
            configuration_version: 3,
            max_members: 400,
            max_user_bans: 400,
            max_recent_messages: 200,
            max_message_size: 1000,
            max_nickname_size: 50,
            ..Configuration::default()
        },
        &owner.sk,
    );
    let members = &mut s.members.members;
    members.push(link(&bot, &owner));
    for m in &owner_invitees {
        members.push(link(m, &owner));
    }
    for m in &bot_invitees {
        members.push(link(m, &bot));
    }
    // Depth 3: owner -> bot -> bot_invitees[n_deputies] -> deep.
    for m in &deep {
        members.push(link(m, &bot_invitees[n_deputies]));
    }
    // Depth 2: owner -> owner_invitees[0] -> mid.
    for m in &mid {
        members.push(link(m, &owner_invitees[0]));
    }

    // Every current member, in a stable order.
    let mut all: Vec<&Mem> = vec![&bot];
    all.extend(owner_invitees.iter());
    all.extend(bot_invitees.iter());
    all.extend(deep.iter());
    all.extend(mid.iter());

    // ---- bans ----
    let weights = [134usize, 30, 20, 10, 6];
    let mut deputy_counts: Vec<usize> = (0..n_deputies)
        .map(|i| (((weights[i % 5] as f64) * scale).round() as usize).max(1))
        .collect();
    if scale >= 1.0 {
        deputy_counts = weights[..n_deputies].to_vec();
    }
    let n_deputy_bans: usize = deputy_counts.iter().sum();
    let n_owner_bans = n(200);
    let n_ex = n(20).min(n_deputy_bans);
    let ex: Vec<Mem> = (0..n_ex).map(|i| mem(key(seed, "ex", i))).collect();
    let ban = |issuer: &Mem, target: MemberId, at: u64| {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner.id,
                banned_at: st(at),
                banned_user: target,
            },
            issuer.id,
            &issuer.sk,
        )
    };
    for i in 0..n_owner_bans {
        let t = mem(key(seed, "owner_ban_target", i));
        s.bans.0.push(ban(&owner, t.id, rng.time()));
    }
    let mut k = 0usize;
    for (d, count) in deputy_counts.iter().enumerate() {
        for _ in 0..*count {
            let target = if k < n_ex {
                ex[k].id
            } else {
                mem(key(seed, "deputy_ban_target", k)).id
            };
            s.bans.0.push(ban(deputies[d], target, rng.time()));
            k += 1;
        }
    }
    for e in &ex {
        s.ban_evidence.members.push(link(e, &bot));
    }

    // ---- member info ----
    let nickname = |rng: &mut Rng, i: usize| {
        let mut name = format!("user{i}-");
        let want = rng.range(8, 50);
        while name.len() < want {
            name.push((b'a' + rng.below(26) as u8) as char);
        }
        name
    };
    let mut owner_info = MemberInfo::new_public(owner.id, 1, "Freenet".into());
    owner_info.deputies = Vec::new();
    s.member_info
        .member_info
        .push(AuthorizedMemberInfo::new(owner_info, &owner.sk));
    let with_info: Vec<&Mem> = all.iter().copied().chain(ex.iter()).collect();
    for (i, m) in with_info.into_iter().enumerate() {
        let mut info = MemberInfo::new_public(m.id, 1 + (i % 3) as u32, nickname(&mut rng, i));
        if m.id == bot.id {
            info.preferred_nickname =
                MemberInfo::new_public(m.id, 1, "River Marshal".into()).preferred_nickname;
            info.deputies = deputies.iter().map(|d| d.id).collect();
        }
        s.member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(info, &m.sk));
    }

    // ---- messages ----
    let n_authors = n(60).max(2).min(all.len() + 1);
    let n_messages = n(200).max(n_authors);
    let mut others: Vec<usize> = (1..all.len()).collect();
    rng.shuffle(&mut others);
    // Author slots: the owner, the bot, then others.
    let mut authors: Vec<&Mem> = vec![&owner, &bot];
    authors.extend(others.iter().take(n_authors - 2).map(|&i| all[i]));
    for i in 0..n_messages {
        // Each author posts once first; the rest are skewed to early authors.
        let a = if i < authors.len() {
            i
        } else {
            let r = rng.below(authors.len());
            rng.below(r + 1)
        };
        let body = if rng.below(25) == 0 {
            // Text near the limit; the encoded content (what v1 bounds by
            // `max_message_size`) adds a few bytes.
            let len = rng.range(900, 980);
            format!("long{i}-{}", "x".repeat(len - format!("long{i}-").len()))
        } else {
            let len = rng.range(8, 90);
            format!("m{i} {}", "word ".repeat(len / 5))
        };
        let m = MessageV1 {
            room_owner: owner.id,
            author: authors[a].id,
            time: st(T0 + (i as u64) * SPAN / n_messages as u64 + rng.next() % 600),
            content: RoomMessageBody::public(body),
        };
        s.recent_messages
            .messages
            .push(AuthorizedMessageV1::new(m, &authors[a].sk));
    }

    // ---- direct messages ----
    let pool_len = n(45).max(2).min(all.len());
    let mut pool: Vec<usize> = (0..all.len()).collect();
    rng.shuffle(&mut pool);
    pool.truncate(pool_len);
    let mut pair_count: HashMap<(usize, usize), usize> = HashMap::new();
    let mut received: HashMap<usize, usize> = HashMap::new();
    for _ in 0..n(300) {
        let (a, b) = loop {
            let a = pool[rng.below(pool.len())];
            let b = pool[rng.below(pool.len())];
            if a != b && pair_count.get(&(a, b)).copied().unwrap_or(0) < 100 {
                break (a, b);
            }
        };
        *pair_count.entry((a, b)).or_default() += 1;
        let len = rng.range(64, 1200);
        let ct = rng.bytes(len);
        let dm = sign_direct_message(&all[a].sk, all[a].id, all[b].id, &owner.vk, rng.time(), ct)
            .expect("dm");
        *received.entry(b).or_default() += 1;
        s.direct_messages.messages.push(dm);
    }

    // ---- purges ----
    let mut recipients: Vec<usize> = received.keys().copied().collect();
    recipients.sort();
    rng.shuffle(&mut recipients);
    recipients.truncate(n(19));
    for (j, &r) in recipients.iter().enumerate() {
        let want = match j {
            0 => 1,
            1 => 40,
            _ => rng.range(1, 40),
        };
        // v1 refuses a purge whose token matches a DM still in the room, so
        // the tokens are arbitrary 16-byte values (DMs already removed).
        let purged = (0..want)
            .map(|_| PurgeToken(rng.bytes(16).try_into().unwrap()))
            .collect();
        let p = sign_recipient_purges(
            &all[r].sk,
            all[r].id,
            &owner.vk,
            RecipientPurges {
                version: 1 + (j as u64 % 3),
                purged,
            },
        )
        .expect("purges");
        s.direct_messages.purges.push(p);
    }

    OfficialShaped {
        bot: bot.vk,
        deputies: deputies.iter().map(|d| d.vk).collect(),
        owner: owner.sk,
        v1: s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::ChatRoomParametersV1;
    use freenet_scaffold::ComposableState;

    fn verify_v1(f: &OfficialShaped) -> Result<(), String> {
        let p = ChatRoomParametersV1 {
            owner: f.owner.verifying_key(),
        };
        f.v1.verify(&f.v1, &p)
    }

    fn cbor(f: &OfficialShaped) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(&f.v1, &mut out).unwrap();
        out
    }

    #[test]
    fn official_shaped_verifies_under_v1_at_full_and_tenth_scale() {
        for scale in [1.0, 0.1] {
            let f = official_shaped(7, scale);
            verify_v1(&f).unwrap_or_else(|e| panic!("scale {scale}: {e}"));
        }
    }

    #[test]
    fn official_shaped_counts_at_full_scale() {
        let t = std::time::Instant::now();
        let f = official_shaped(7, 1.0);
        let built = t.elapsed();
        println!("official_shaped(7, 1.0) built in {built:?}");
        assert!(built < Duration::from_secs(5), "{built:?}");
        let v1 = &f.v1;
        let owner_id = MemberId::from(&f.owner.verifying_key());
        let bot_id = MemberId::from(&f.bot);
        let members: HashMap<MemberId, &AuthorizedMember> = v1
            .members
            .members
            .iter()
            .map(|m| (m.member.id(), m))
            .collect();
        assert_eq!(members.len(), 169);
        let invited_by = |id: MemberId| {
            members
                .values()
                .filter(|m| m.member.invited_by == id)
                .count()
        };
        assert_eq!(invited_by(owner_id), 3);
        assert_eq!(invited_by(bot_id), 160);
        let depth = |mut id: MemberId| {
            let mut d = 0;
            while id != owner_id {
                id = members[&id].member.invited_by;
                d += 1;
            }
            d
        };
        assert_eq!(members.keys().map(|&i| depth(i)).max(), Some(3));

        assert_eq!(f.deputies.len(), 5);
        assert!(f
            .deputies
            .windows(2)
            .all(|w| w[0].to_bytes() < w[1].to_bytes()));
        let deputy_ids: Vec<MemberId> = f.deputies.iter().map(MemberId::from).collect();
        assert!(deputy_ids
            .iter()
            .all(|d| members[d].member.invited_by == bot_id));
        let bot_info = v1
            .member_info
            .member_info
            .iter()
            .find(|i| i.member_info.member_id == bot_id)
            .unwrap();
        let mut listed = bot_info.member_info.deputies.clone();
        let mut want = deputy_ids.clone();
        listed.sort();
        want.sort();
        assert_eq!(listed, want);
        let owner_info = v1
            .member_info
            .member_info
            .iter()
            .find(|i| i.member_info.member_id == owner_id)
            .unwrap();
        assert!(owner_info.member_info.deputies.is_empty());

        assert_eq!(v1.bans.0.len(), 400);
        let by = |id: MemberId| v1.bans.0.iter().filter(|b| b.banned_by == id).count();
        assert_eq!(by(owner_id), 200);
        let per_deputy: Vec<usize> = deputy_ids.iter().map(|&d| by(d)).collect();
        assert_eq!(per_deputy.iter().sum::<usize>(), 200);
        assert_eq!(per_deputy.iter().copied().max(), Some(134));
        assert!(v1
            .bans
            .0
            .iter()
            .all(|b| !members.contains_key(&b.ban.banned_user)));
        assert_eq!(v1.ban_evidence.members.len(), 20);

        assert_eq!(v1.member_info.member_info.len(), 190);
        assert!(v1.member_info.member_info.iter().all(|i| matches!(
            &i.member_info.preferred_nickname,
            crate::room_state::privacy::SealedBytes::Public { value } if value.len() <= 50
        )));

        assert_eq!(v1.recent_messages.messages.len(), 200);
        let authors: std::collections::HashSet<_> = v1
            .recent_messages
            .messages
            .iter()
            .map(|m| m.message.author)
            .collect();
        assert_eq!(authors.len(), 60);
        assert!(authors.contains(&owner_id) && authors.contains(&bot_id));

        let dms = &v1.direct_messages.messages;
        assert_eq!(dms.len(), 300);
        let total: usize = dms.iter().map(|d| d.message.ciphertext.len()).sum();
        let avg = total / dms.len();
        println!("dm average ciphertext {avg} bytes");
        assert!((600..=660).contains(&avg), "{avg}");
        assert!(dms.iter().all(|d| d.message.ciphertext.len() <= 2560));
        let purges = &v1.direct_messages.purges;
        assert_eq!(purges.len(), 19);
        assert!(purges
            .iter()
            .all(|p| (1..=40).contains(&p.state.purged.len())));
        assert!(v1.secrets.versions.is_empty() && v1.secrets.encrypted_secrets.is_empty());
        assert_eq!(v1.configuration.configuration.configuration_version, 3);
    }

    #[test]
    fn official_shaped_is_deterministic() {
        let a = cbor(&official_shaped(11, 1.0));
        let b = cbor(&official_shaped(11, 1.0));
        assert_eq!(a, b);
        assert_ne!(a, cbor(&official_shaped(12, 1.0)));
    }
}
