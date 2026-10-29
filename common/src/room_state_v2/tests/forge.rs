//! Forged copies of real records: one signature byte changed, everything
//! else as signed. A record with several signatures (a link's invitation,
//! invite-key and member signatures; a restore request's outer signature
//! and those of each link in its chain) gets one forgery per signature, the
//! others re-signed where the forged one is covered by them, so each
//! forgery fails exactly one check. A legacy record's forgery changes a
//! byte of the v1 signature inside its v1 bytes, which stay the canonical
//! encoding of the v1 type, so it decodes and fails only its signature.

use super::builder::{sk, NK};
use crate::room_state::ban::AuthorizedUserBan;
use crate::room_state::direct_messages::{AuthorizedDirectMessage, AuthorizedRecipientPurges};
use crate::room_state::member::AuthorizedMember;
use crate::room_state::member_info::AuthorizedMemberInfo;
use crate::room_state::message::AuthorizedMessageV1;
use crate::room_state::secret::AuthorizedEncryptedSecretForMember;
use crate::room_state_v2::encoding::{sign_payload, to_cbor, ByteBuf, Key, Sig};
use crate::room_state_v2::fastcbor;
use crate::room_state_v2::records::*;
use ed25519_dalek::{Signature, SigningKey};
use serde::{de::DeserializeOwned, Serialize};

fn flip(s: Sig) -> Sig {
    let mut s = s;
    s.0[0] ^= 1;
    s
}

fn flip_v1(s: &mut Signature) {
    let mut b = s.to_bytes();
    b[0] ^= 1;
    *s = Signature::from_bytes(&b);
}

/// The signing key of `k`: a builder key, or an invitation's one-time key.
fn secret_of(k: &Key) -> SigningKey {
    (0..NK)
        .map(sk)
        .chain((0..56u8).map(|i| SigningKey::from_bytes(&[200 + i; 32])))
        .find(|s| Key::from(&s.verifying_key()) == *k)
        .expect("a key the builder signs with")
}

/// The v1 bytes `b`, decoded as `T`, changed by `edit` and re-encoded.
fn reencode<T: Serialize + DeserializeOwned>(b: &[u8], edit: impl FnOnce(&mut T)) -> Vec<u8> {
    let mut v: T = ciborium::de::from_reader(b).expect("builder v1 bytes decode");
    edit(&mut v);
    let out = to_cbor(&v);
    assert!(
        fastcbor::from_canonical::<T>(&out).is_some(),
        "a forged v1 record must stay canonical"
    );
    out
}

/// `l` with a byte of its v1 signature changed; `kind` is its record kind.
fn forge_legacy(kind: u8, l: &Legacy) -> Legacy {
    let b = &l.v1.0;
    let v1 = match kind {
        8 => reencode::<AuthorizedUserBan>(b, |v| flip_v1(&mut v.signature)),
        10 => reencode::<AuthorizedMember>(b, |v| flip_v1(&mut v.signature)),
        12 | 17 => reencode::<AuthorizedMessageV1>(b, |v| flip_v1(&mut v.signature)),
        15 => reencode::<AuthorizedMemberInfo>(b, |v| flip_v1(&mut v.signature)),
        19 => reencode::<AuthorizedDirectMessage>(b, |v| flip_v1(&mut v.sender_signature)),
        21 => reencode::<AuthorizedRecipientPurges>(b, |v| flip_v1(&mut v.recipient_signature)),
        23 => {
            reencode::<AuthorizedEncryptedSecretForMember>(b, |v| flip_v1(&mut v.owner_signature))
        }
        _ => unreachable!("not a legacy kind: {kind}"),
    };
    Legacy {
        signer: l.signer,
        v1: ByteBuf(v1),
    }
}

fn chain_member(c: &ChainLink) -> Key {
    match c {
        ChainLink::Accepted(a) => a.acceptance.member,
        ChainLink::Legacy(l) => {
            let v1: AuthorizedMember = ciborium::de::from_reader(&l.v1.0[..]).unwrap();
            Key::from(&v1.member.member_vk)
        }
    }
}

/// The link's forgeries: its invitation signature changed (as is, so the
/// acceptance no longer names the invitation, and with the acceptance
/// re-signed over the forged invitation's digest, so only the inviter's
/// signature fails), its invite-key signature changed, its member signature
/// changed.
fn forge_link(o: &Key, a: &AcceptedLink) -> Vec<(&'static str, AcceptedLink)> {
    let mut bad_inv = a.clone();
    bad_inv.invitation.sig = flip(a.invitation.sig);
    let mut resigned = bad_inv.clone();
    resigned.acceptance.invitation = resigned.invitation_digest();
    let invite_sk = secret_of(&resigned.invitation.payload.invite_key);
    let member_sk = secret_of(&resigned.acceptance.member);
    resigned.invite_key_sig = sign_payload(tag::ACCEPTANCE, o, &resigned.acceptance, &invite_sk);
    resigned.member_sig = sign_payload(tag::ACCEPTANCE, o, &resigned.acceptance, &member_sk);
    let mut bad_ik = a.clone();
    bad_ik.invite_key_sig = flip(a.invite_key_sig);
    let mut bad_m = a.clone();
    bad_m.member_sig = flip(a.member_sig);
    vec![
        ("inviter sig", bad_inv),
        ("inviter sig, re-accepted", resigned),
        ("invite-key sig", bad_ik),
        ("member sig", bad_m),
    ]
}

fn signed<P: Clone>(s: &Signed<P>) -> Signed<P> {
    Signed {
        payload: s.payload.clone(),
        sig: flip(s.sig),
    }
}

/// Every forgery of `r`, each named by the signature it changes.
pub fn forgeries(o: &Key, r: &Record) -> Vec<(String, Record)> {
    let one = |what: &str, r: Record| vec![(what.to_string(), r)];
    match r {
        Record::Configuration(s) => one("sig", Record::Configuration(signed(s))),
        Record::Clock(s) => one("sig", Record::Clock(signed(s))),
        Record::Checkpoint(s) => one("sig", Record::Checkpoint(signed(s))),
        Record::Upgrade(s) => one("sig", Record::Upgrade(signed(s))),
        Record::Roster(s) => one("sig", Record::Roster(signed(s))),
        Record::SecretVersion(s) => one("sig", Record::SecretVersion(signed(s))),
        Record::Ban(s) => one("sig", Record::Ban(signed(s))),
        Record::Activity(s) => one("sig", Record::Activity(signed(s))),
        Record::MemberInfo(s) => one("sig", Record::MemberInfo(signed(s))),
        Record::Message(s) => one("sig", Record::Message(signed(s))),
        Record::DirectMessage(s) => one("sig", Record::DirectMessage(signed(s))),
        Record::Purge(s) => one("sig", Record::Purge(signed(s))),
        Record::SecretBlob(s) => one("sig", Record::SecretBlob(signed(s))),
        Record::Link(a) => forge_link(o, a)
            .into_iter()
            .map(|(w, l)| (w.to_string(), Record::Link(l)))
            .collect(),
        Record::Restore(s) => {
            let mut out = one("outer sig", Record::Restore(signed(s)));
            let signer = secret_of(&chain_member(&s.payload.chain[0]));
            for i in 0..s.payload.chain.len() {
                let mut p = s.payload.clone();
                p.chain[i] = match &p.chain[i] {
                    ChainLink::Accepted(a) => {
                        let mut a = a.clone();
                        a.member_sig = flip(a.member_sig);
                        ChainLink::Accepted(a)
                    }
                    ChainLink::Legacy(l) => ChainLink::Legacy(forge_legacy(10, l)),
                };
                let sig = sign_payload(tag::RESTORE, o, &p, &signer);
                out.push((
                    format!("chain[{i}] sig"),
                    Record::Restore(Signed { payload: p, sig }),
                ));
            }
            out
        }
        Record::LegacyBan(l)
        | Record::LegacyLink(l)
        | Record::LegacyActivity(l)
        | Record::LegacyMemberInfo(l)
        | Record::LegacyMessage(l)
        | Record::LegacyDirectMessage(l)
        | Record::LegacyPurge(l)
        | Record::LegacySecretBlob(l) => {
            let f = forge_legacy(r.kind(), l);
            let rec = match r {
                Record::LegacyBan(_) => Record::LegacyBan(f),
                Record::LegacyLink(_) => Record::LegacyLink(f),
                Record::LegacyActivity(_) => Record::LegacyActivity(f),
                Record::LegacyMemberInfo(_) => Record::LegacyMemberInfo(f),
                Record::LegacyMessage(_) => Record::LegacyMessage(f),
                Record::LegacyDirectMessage(_) => Record::LegacyDirectMessage(f),
                Record::LegacyPurge(_) => Record::LegacyPurge(f),
                _ => Record::LegacySecretBlob(f),
            };
            one("v1 sig", rec)
        }
    }
}
