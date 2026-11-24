//! Discover fixtures for the example build (no network): a signed index state
//! pushed through the real decode, verify and safe-search pipeline, so a test
//! of "hidden rooms are absent" exercises the filter and not a hand-made list.

use super::atlas::fixtures::{audience, build, encode, owner_b58, params, room, verify_status};
use super::atlas::{Locator, Verification};
use super::{read_state, DiscoverIndex, Unavailable};
use std::cell::RefCell;

thread_local! {
    static ANSWER: RefCell<Option<DiscoverIndex>> = const { RefCell::new(None) };
}

/// What the next index request is answered with; `None` leaves it unanswered
/// (the view stays on "Loading").
pub fn set_answer(view: Option<DiscoverIndex>) {
    ANSWER.with(|a| *a.borrow_mut() = view);
}

pub fn answer() -> Option<DiscoverIndex> {
    ANSWER.with(|a| a.borrow().clone())
}

fn owner_of(kind: &str) -> String {
    match kind {
        // Listed and public now.
        "public" | "private" => crate::example_data::viewable_fixtures()
            .iter()
            .find(|(k, _, _)| *k == kind)
            .map(|(_, vk, _)| bs58::encode(vk.to_bytes()).into_string())
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

/// Entries: two public rooms (one listed though now private), one unreachable,
/// and three that safe search must hide (unclassified, adult, malformed).
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
    vec![public, private, unreachable, unclassified, adult, malformed]
}

fn view_of(entries: Vec<super::atlas::IndexEntry>) -> DiscoverIndex {
    let built = build(entries);
    read_state(&encode(&built.state), &params())
        .expect("fixture state reads")
        .0
}

/// `kind`: "ready", "hidden" (listed rooms, none visible), "empty",
/// "unavailable" (a real failure), or "loading" (never answered).
pub fn view(kind: &str) -> Option<DiscoverIndex> {
    match kind {
        "ready" => Some(view_of(entries())),
        "hidden" => Some(view_of(entries().into_iter().skip(3).collect())),
        "empty" => Some(view_of(vec![])),
        "unavailable" => Some(DiscoverIndex::Unavailable(Unavailable::IndexNotFound)),
        _ => None,
    }
}
