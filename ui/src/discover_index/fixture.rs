//! Discover fixtures for the example build (no network). The session runs for
//! real: it is built with fixture keys, asks for the pointer and then the
//! index, and is answered with a signed pointer record and a signed index
//! state, which go through the real decode, verify and safe-search pipeline.
//! So a test of "hidden rooms are absent", Retry, timers or the monotonicity
//! guard exercises the production code, not a hand-made list.

use super::atlas::fixtures::{audience, build, encode, owner_b58, params, room, sk, verify_status};
use super::atlas::{Locator, Verification};
use super::{index_id, Session, POINTER_APP_ID};
use dioxus::prelude::*;
use ed25519_dalek::Signer;
use freenet_migrate::{PointerFloor, PointerResolver};
use freenet_stdlib::prelude::ContractInstanceId;
use std::cell::RefCell;

thread_local! {
    static KIND: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// What the index is served as: "ready", "hidden", "empty", "unavailable"; any
/// other value leaves every request unanswered.
pub fn set_kind(kind: &str) {
    KIND.with(|k| *k.borrow_mut() = Some(kind.to_string()));
}

fn pointer_sk() -> ed25519_dalek::SigningKey {
    sk(5)
}

const INDEX_HASH: [u8; 32] = [7; 32];

/// A session wired to the fixture's own keys instead of Atlas's.
pub fn session() -> Option<Session> {
    Session::new(pointer_sk().verifying_key(), POINTER_APP_ID, params())
}

fn pointer_id() -> Option<ContractInstanceId> {
    PointerResolver::new(
        &pointer_sk().verifying_key(),
        POINTER_APP_ID,
        PointerFloor::never_resolved(),
    )
    .ok()
    .map(|r| r.pointer_id())
}

fn pointer_record() -> Vec<u8> {
    let sk = pointer_sk();
    let pp = freenet_migrate::pointer_params(&sk.verifying_key(), POINTER_APP_ID).unwrap();
    let msg = freenet_migrate::pointer_signing_message(&pp, 1, &INDEX_HASH);
    freenet_migrate::PointerRecord {
        version: 1,
        code_hash: INDEX_HASH,
        signature: sk.sign(&msg).to_bytes(),
    }
    .encode()
    .to_vec()
}

/// The bytes a GET of `id` is answered with, or `None` for no answer.
pub fn reply(id: &ContractInstanceId) -> Option<Vec<u8>> {
    let kind = KIND.with(|k| k.borrow().clone())?;
    if Some(*id) == pointer_id() {
        return matches!(kind.as_str(), "ready" | "hidden" | "empty" | "unavailable")
            .then(pointer_record);
    }
    if *id != index_id(&INDEX_HASH, &params()) {
        return None;
    }
    match kind.as_str() {
        "ready" => Some(encode(&build(entries()).state)),
        "hidden" => Some(encode(
            &build(entries().into_iter().skip(4).collect()).state,
        )),
        "empty" => Some(encode(&build(vec![]).state)),
        // Fails verification: a real failure, not an empty list.
        "unavailable" => Some(b"not an index state".to_vec()),
        _ => None,
    }
}

fn owner_of(kind: &str) -> String {
    match kind {
        // Listed and public now.
        "public" | "private" => crate::example_data::viewable_fixtures()
            .iter()
            .find(|(k, _, _)| *k == kind)
            .map(|(_, vk, _)| bs58::encode(vk.to_bytes()).into_string())
            .unwrap_or_default(),
        // A room the user is already in (the example list's "Team Chat Room").
        "member" => crate::components::app::ROOMS
            .try_read()
            .ok()
            .and_then(|rooms| {
                rooms
                    .map
                    .iter()
                    .find(|(_, r)| r.display_name() == "Team Chat Room")
                    .map(|(vk, _)| bs58::encode(vk.to_bytes()).into_string())
            })
            .unwrap_or_default(),
        // Well formed, held by nobody.
        _ => bs58::encode(
            ed25519_dalek::SigningKey::from_bytes(&[0x53; 32])
                .verifying_key()
                .to_bytes(),
        )
        .into_string(),
    }
}

fn ext(source: &str) -> Option<std::collections::BTreeMap<String, String>> {
    Some([("ss".to_string(), source.to_string())].into())
}

/// Entries: public rooms (one listed though now private, one the reader is
/// already in), one unreachable, and three that safe search must hide
/// (unclassified, adult, malformed).
fn entries() -> Vec<super::atlas::IndexEntry> {
    let mut public = room(1, 1, "Atlas Open Room", owner_of("public"));
    public.snippet = "Open conversation about decentralised software.".into();
    public.tags = vec!["chat".into(), "software".into()];
    public.featured = true;
    public.added_at = 30;
    public.ext = ext("c");

    let mut private = room(2, 1, "Atlas Quiet Room", owner_of("private"));
    private.snippet = "Gardening and slow living.".into();
    private.tags = vec!["hobby".into(), "chat".into()];
    private.added_at = 20;
    private.ext = ext("l");

    let mut unreachable = room(3, 1, "Atlas Faraway Room", owner_of("missing"));
    unreachable.snippet = "Local history from long ago.".into();
    unreachable.tags = vec!["history".into()];
    unreachable.added_at = 10;
    unreachable.ext = ext("c");
    unreachable.verified = Some(Verification {
        last_verified_at: 1,
        status: verify_status("unreachable"),
    });

    let mut member = room(7, 1, "Atlas Team Room", owner_of("member"));
    member.snippet = "A room the reader already belongs to.".into();
    member.tags = vec!["team".into()];
    member.added_at = 5;
    member.ext = ext("c");

    let mut unclassified = room(4, 1, "Hidden Unclassified Room", owner_b58(0x61));
    unclassified.class = None;
    let mut adult = room(5, 1, "Hidden Adult Room", owner_b58(0x62));
    if let Some(c) = adult.class.as_mut() {
        c.landing = audience("adult");
    }
    let mut malformed = room(6, 1, "Hidden Malformed Room", String::new());
    malformed.locator = Locator::External {
        url: "https://example.invalid/".into(),
    };
    vec![
        public,
        private,
        unreachable,
        member,
        unclassified,
        adult,
        malformed,
    ]
}
