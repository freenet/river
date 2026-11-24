//! The rules behind the Discover Rooms view (freenet/river#975, R2 of #972):
//! what each state of [`DiscoverIndex`] shows, how search and tag chips narrow
//! the list, and how Atlas's text is attributed. The rendering is
//! `components::discover`; nothing here touches signals other than
//! [`DISCOVER_OPEN`], and nothing here writes `ROOMS`, the chat delegate or
//! browser storage (browsing on try.freenet.org must leave no trace).

use crate::discover_index::atlas::{Availability, RoomHit, TextSource};
use crate::discover_index::{DiscoverIndex, Unavailable};
use dioxus::prelude::*;

/// Whether the Discover view fills the main pane. Session-only.
pub static DISCOVER_OPEN: GlobalSignal<bool> = Global::new(|| false);

/// What the view shows for one state of the index.
#[derive(Debug, Clone, PartialEq)]
pub enum Pane<'a> {
    Loading,
    /// A real failure: distinct from `Empty`, and offers Retry.
    Error(&'static str),
    /// A verified index with no listed rooms.
    Empty,
    /// Rooms are listed but safe search lets none through.
    Hidden,
    List(&'a [RoomHit]),
}

pub const ERROR_MESSAGE: &str = "Discover is unavailable right now. Try again.";

pub fn pane(index: &DiscoverIndex) -> Pane<'_> {
    match index {
        DiscoverIndex::Loading => Pane::Loading,
        DiscoverIndex::Unavailable(reason) => Pane::Error(match reason {
            // Every reason reads the same to a user; the reasons stay distinct
            // in the index so tests and logs can tell them apart.
            Unavailable::PointerNeverPublished
            | Unavailable::PointerWithdrawn
            | Unavailable::PointerRolledBack
            | Unavailable::PointerCompeting
            | Unavailable::PointerUnreachable
            | Unavailable::PointerInvalid(_)
            | Unavailable::IndexNotFound
            | Unavailable::IndexInvalid(_)
            | Unavailable::BadAnchor => ERROR_MESSAGE,
        }),
        DiscoverIndex::Empty => Pane::Empty,
        DiscoverIndex::Hidden => Pane::Hidden,
        DiscoverIndex::Ready(hits) => Pane::List(hits),
    }
}

/// Who said a card's text. Never styled or worded as the owner's own.
pub fn attribution(source: Option<TextSource>) -> &'static str {
    match source {
        Some(TextSource::Curator) => "Curated by Atlas",
        Some(TextSource::Llm) => "Summarised by Atlas (automated)",
        None => "From Atlas",
    }
}

/// Anything but `Live` is a quiet note, never a hidden or "dead" room (Atlas's
/// `Unreachable` is not a takedown).
pub fn availability_note(a: Availability) -> Option<&'static str> {
    match a {
        Availability::Live => None,
        Availability::Unreachable | Availability::Stale | Availability::Unknown => {
            Some("may be unavailable")
        }
    }
}

/// The tags to offer as chips: each tag once (compared without case), most
/// listed first, then alphabetical.
pub fn tag_chips(hits: &[RoomHit]) -> Vec<String> {
    let mut counts: Vec<(String, String, usize)> = Vec::new();
    for tag in hits.iter().flat_map(|h| h.tags.iter()) {
        let lower = tag.to_lowercase();
        if lower.is_empty() {
            continue;
        }
        match counts.iter_mut().find(|(l, _, _)| *l == lower) {
            Some((_, _, n)) => *n += 1,
            None => counts.push((lower, tag.clone(), 1)),
        }
    }
    counts.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    counts.into_iter().map(|(_, shown, _)| shown).collect()
}

fn score(h: &RoomHit, q: &str) -> u8 {
    if h.title.to_lowercase().contains(q) {
        3
    } else if h.tags.iter().any(|t| t.to_lowercase().contains(q)) {
        2
    } else if h.snippet.to_lowercase().contains(q) {
        1
    } else {
        0
    }
}

/// `hits` narrowed to those matching `text` (over title, snippet and tags) and
/// carrying every selected tag. The input order (featured first) is kept;
/// with text, better matches (title, then tag, then snippet) come first.
pub fn narrow(hits: &[RoomHit], text: &str, tags: &[String]) -> Vec<RoomHit> {
    let q = text.trim().to_lowercase();
    let wanted: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();
    let mut scored: Vec<(u8, &RoomHit)> = hits
        .iter()
        .filter(|h| {
            wanted
                .iter()
                .all(|w| h.tags.iter().any(|t| t.to_lowercase() == *w))
        })
        .filter_map(|h| {
            if q.is_empty() {
                Some((0, h))
            } else {
                let s = score(h, &q);
                (s > 0).then_some((s, h))
            }
        })
        .collect();
    // Stable: equal scores keep Atlas's order.
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, h)| h.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(title: &str, snippet: &str, tags: &[&str], featured: bool) -> RoomHit {
        RoomHit {
            owner_key: title.to_string(),
            subject_id: title.to_string(),
            title: title.to_string(),
            snippet: snippet.to_string(),
            snippet_source: Some(TextSource::Curator),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            featured,
            added_at: 0,
            availability: Availability::Live,
            last_verified_at: None,
            landing: Some("general".into()),
            has_adult_sections: Some(false),
            config_version: None,
        }
    }

    #[test]
    fn every_failure_is_an_error_and_never_empty() {
        for reason in [
            Unavailable::PointerNeverPublished,
            Unavailable::PointerWithdrawn,
            Unavailable::PointerRolledBack,
            Unavailable::PointerCompeting,
            Unavailable::PointerUnreachable,
            Unavailable::PointerInvalid("x".into()),
            Unavailable::IndexNotFound,
            Unavailable::IndexInvalid("x".into()),
            Unavailable::BadAnchor,
        ] {
            let failed = DiscoverIndex::Unavailable(reason);
            let p = pane(&failed);
            assert_eq!(p, Pane::Error(ERROR_MESSAGE));
            assert_ne!(p, pane(&DiscoverIndex::Empty));
        }
    }

    #[test]
    fn the_other_states_map_one_to_one() {
        assert_eq!(pane(&DiscoverIndex::Loading), Pane::Loading);
        assert_eq!(pane(&DiscoverIndex::Empty), Pane::Empty);
        assert_eq!(pane(&DiscoverIndex::Hidden), Pane::Hidden);
        let hits = vec![hit("a", "", &[], false)];
        assert_eq!(pane(&DiscoverIndex::Ready(hits.clone())), Pane::List(&hits));
    }

    #[test]
    fn attribution_never_reads_as_the_owner() {
        assert_eq!(attribution(Some(TextSource::Curator)), "Curated by Atlas");
        assert_eq!(
            attribution(Some(TextSource::Llm)),
            "Summarised by Atlas (automated)"
        );
        assert_eq!(attribution(None), "From Atlas");
    }

    #[test]
    fn only_a_live_room_goes_without_the_note() {
        assert_eq!(availability_note(Availability::Live), None);
        for a in [
            Availability::Unreachable,
            Availability::Stale,
            Availability::Unknown,
        ] {
            let note = availability_note(a).unwrap();
            assert_eq!(note, "may be unavailable");
            assert!(!note.to_lowercase().contains("dead"));
        }
    }

    #[test]
    fn narrowing_matches_title_snippet_and_tags_without_case() {
        let hits = vec![
            hit("Rust Lounge", "chat about code", &["dev"], false),
            hit("Gardeners", "plants and RUST on tools", &["hobby"], false),
            hit("Cooking", "recipes", &["Rusty"], false),
            hit("Music", "songs", &["art"], false),
        ];
        let got: Vec<_> = narrow(&hits, "rust", &[])
            .into_iter()
            .map(|h| h.title)
            .collect();
        // Title match first, then tag, then snippet.
        assert_eq!(got, ["Rust Lounge", "Cooking", "Gardeners"]);
        assert!(narrow(&hits, "zzz", &[]).is_empty());
        assert_eq!(narrow(&hits, "  ", &[]).len(), 4);
    }

    #[test]
    fn selected_tags_must_all_be_present_and_order_is_kept() {
        let hits = vec![
            hit("A", "", &["x", "y"], true),
            hit("B", "", &["x"], false),
            hit("C", "", &["X", "y"], false),
        ];
        let got = |tags: &[&str]| -> Vec<String> {
            narrow(
                &hits,
                "",
                &tags.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            )
            .into_iter()
            .map(|h| h.title)
            .collect()
        };
        assert_eq!(got(&["x"]), ["A", "B", "C"]);
        assert_eq!(got(&["x", "y"]), ["A", "C"]);
        assert!(got(&["nope"]).is_empty());
    }

    #[test]
    fn chips_list_each_tag_once_most_common_first() {
        let hits = vec![
            hit("A", "", &["Chat", "dev"], false),
            hit("B", "", &["chat", "art"], false),
            hit("C", "", &["dev", "chat", ""], false),
        ];
        assert_eq!(tag_chips(&hits), ["Chat", "dev", "art"]);
    }

    /// Browsing must leave no trace on the hosted page: nothing in the view
    /// logic or the component may write the rooms map, the chat delegate or
    /// browser storage.
    #[test]
    fn discover_never_writes_rooms_the_delegate_or_storage() {
        for (name, src) in [
            ("discover_view.rs", include_str!("discover_view.rs")),
            (
                "components/discover.rs",
                include_str!("components/discover.rs"),
            ),
        ] {
            // Only the code before the test module counts.
            let code = src.split("#[cfg(test)]").next().unwrap();
            for banned in [
                "ROOMS.write",
                "ROOMS.with_mut",
                "chat_delegate",
                "save_rooms",
                "localStorage",
                "local_storage",
                "sessionStorage",
                "set_item",
            ] {
                assert!(!code.contains(banned), "{name} must not use {banned}");
            }
        }
    }
}
