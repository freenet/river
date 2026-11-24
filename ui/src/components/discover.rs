//! The Discover Rooms main-pane view (freenet/river#975, R2 of #972): Atlas's
//! public room index as cards. Rules and tests: [`crate::discover_view`]; the
//! index itself: [`crate::discover_index`]. Browsing writes nothing: no
//! `ROOMS`, no delegate, no browser storage.

use crate::components::app::{MobileView, MOBILE_VIEW};
use crate::components::icons::{line_icon, SEARCH_ICON_PATH};
use crate::discover_index::atlas::RoomHit;
use crate::discover_index::{self, DISCOVER_INDEX};
use crate::discover_view::{
    attribution, availability_note, narrow, pane, tag_chips, Pane, DISCOVER_OPEN,
};
use dioxus::prelude::*;

/// Show the Discover view in the main pane (also on a phone) and start
/// reading the index. Safe to call from an event handler.
pub fn open_discover() {
    crate::util::defer(|| {
        // Leave a read-only room being viewed; this also resets the flag.
        crate::public_room::close();
        crate::util::defer(|| {
            *DISCOVER_OPEN.write() = true;
            *MOBILE_VIEW.write() = MobileView::Chat;
        });
    });
}

fn close_discover() {
    crate::util::defer(|| {
        if DISCOVER_OPEN.try_read().is_ok_and(|open| *open) {
            *DISCOVER_OPEN.write() = false;
        }
    });
}

fn open_card(owner_key: String) {
    crate::util::defer(move || {
        *DISCOVER_OPEN.write() = false;
        crate::public_room::open_from_discover(&owner_key);
        *MOBILE_VIEW.write() = MobileView::Chat;
    });
}

fn retry() {
    // Closing first makes the next open start over from the pointer.
    discover_index::close();
    discover_index::open();
}

#[component]
pub fn DiscoverPane() -> Element {
    let mut query = use_signal(String::new);
    let mut selected = use_signal(Vec::<String>::new);

    // Read the index while the pane is up; stop retrying when it goes.
    use_hook(|| crate::util::defer(discover_index::open));
    use_drop(discover_index::close);

    let index = match DISCOVER_INDEX.try_read() {
        Ok(v) => v.clone(),
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            return rsx! { div { class: "flex-1" } };
        }
    };

    rsx! {
        div {
            class: "flex-1 flex flex-col min-w-0 bg-bg",
            "data-testid": "discover",
            div { class: "flex items-center justify-between gap-3 px-4 md:px-6 h-[60px] flex-shrink-0 border-b border-border",
                h1 { class: "text-lg font-semibold text-text", "Discover rooms" }
                button {
                    r#type: "button",
                    "data-testid": "discover-close",
                    class: "flex-shrink-0 px-3 py-2 bg-surface hover:bg-surface-hover border border-border rounded-lg text-text-muted hover:text-text text-sm transition-colors",
                    onclick: move |_| close_discover(),
                    "Close"
                }
            }
            div { class: "flex-1 overflow-y-auto",
                div { class: "max-w-3xl mx-auto w-full px-4 md:px-6 py-4",
                    match pane(&index) {
                        Pane::Loading => rsx! {
                            Notice {
                                testid: "discover-loading",
                                title: "Looking for public rooms",
                                body: "Asking the network. This can take a few seconds.",
                            }
                        },
                        Pane::Error(message) => rsx! {
                            Notice { testid: "discover-error", title: message, body: "" ,
                                button {
                                    r#type: "button",
                                    "data-testid": "discover-retry",
                                    class: "px-3 py-2 bg-surface hover:bg-surface-hover border border-border rounded-lg text-text text-sm transition-colors",
                                    onclick: move |_| retry(),
                                    "Retry"
                                }
                            }
                        },
                        Pane::Empty => rsx! {
                            Notice {
                                testid: "discover-empty",
                                title: "Public rooms will appear here soon.",
                                body: "Have a link to a room? Open it to read and join.",
                            }
                        },
                        Pane::Hidden => rsx! {
                            Notice {
                                testid: "discover-hidden",
                                title: "All listed rooms are hidden by safe search.",
                                body: "Have a link to a room? Open it to read and join.",
                            }
                        },
                        Pane::List(hits) => {
                            let chips = tag_chips(hits);
                            let shown = narrow(hits, &query.read(), &selected.read());
                            rsx! {
                                div { class: "relative mb-3",
                                    span { class: "absolute left-3 top-1/2 -translate-y-1/2 text-text-muted pointer-events-none",
                                        {line_icon(SEARCH_ICON_PATH, 15)}
                                    }
                                    input {
                                        r#type: "search",
                                        "data-testid": "discover-search",
                                        "aria-label": "Search rooms",
                                        placeholder: "Search rooms",
                                        class: "w-full pl-9 pr-3 py-2 rounded-lg bg-surface border border-border text-text text-sm",
                                        value: "{query}",
                                        oninput: move |e| query.set(e.value()),
                                    }
                                }
                                if !chips.is_empty() {
                                    div { class: "flex flex-wrap gap-1.5 mb-4", "data-testid": "discover-tags",
                                        for tag in chips {
                                            {
                                                let on = selected.read().iter().any(|t| t.eq_ignore_ascii_case(&tag));
                                                let label = tag.clone();
                                                rsx! {
                                                    button {
                                                        key: "{tag}",
                                                        r#type: "button",
                                                        "data-testid": "discover-tag",
                                                        "aria-pressed": "{on}",
                                                        class: if on {
                                                            "px-2.5 py-1 rounded-full text-xs border border-link bg-accent-soft text-link"
                                                        } else {
                                                            "px-2.5 py-1 rounded-full text-xs border border-border bg-surface text-text-muted hover:text-text"
                                                        },
                                                        onclick: move |_| {
                                                            let mut sel = selected.write();
                                                            if let Some(i) = sel.iter().position(|t| t.eq_ignore_ascii_case(&label)) {
                                                                sel.remove(i);
                                                            } else {
                                                                sel.push(label.clone());
                                                            }
                                                        },
                                                        "{tag}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if shown.is_empty() {
                                    p { class: "text-sm text-text-muted py-6 text-center", "data-testid": "discover-no-match",
                                        "No rooms match your search."
                                    }
                                } else {
                                    ul { class: "space-y-3", "data-testid": "discover-list",
                                        for hit in shown {
                                            Card { key: "{hit.owner_key}", hit }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Notice(
    testid: &'static str,
    title: &'static str,
    body: &'static str,
    children: Element,
) -> Element {
    rsx! {
        div {
            class: "py-12 text-center",
            "data-testid": "{testid}",
            h2 { class: "text-base font-semibold text-text mb-2", "{title}" }
            if !body.is_empty() {
                p { class: "text-sm text-text-muted mb-4", "{body}" }
            }
            div { class: "flex justify-center gap-2", {children} }
        }
    }
}

#[component]
fn Card(hit: RoomHit) -> Element {
    let note = availability_note(hit.availability);
    let owner_key = hit.owner_key.clone();
    rsx! {
        li {
            button {
                r#type: "button",
                "data-testid": "discover-card",
                "data-owner": "{hit.owner_key}",
                class: "w-full text-left p-4 rounded-xl bg-surface hover:bg-surface-hover border border-border transition-colors",
                onclick: move |_| open_card(owner_key.clone()),
                div { class: "flex items-start justify-between gap-2",
                    div { class: "min-w-0",
                        div { class: "text-base font-semibold text-text break-words", "data-testid": "discover-card-title", "{hit.title}" }
                        div { class: "text-[11px] text-text-muted", "Name as Atlas listed it" }
                    }
                    div { class: "flex flex-col items-end gap-1 flex-shrink-0",
                        if hit.featured {
                            span { class: "px-2 py-0.5 rounded-full text-[11px] bg-accent-soft text-link", "data-testid": "discover-card-featured", "Featured" }
                        }
                        if let Some(note) = note {
                            span { class: "text-[11px] text-text-muted", "data-testid": "discover-card-unavailable", "{note}" }
                        }
                    }
                }
                if !hit.snippet.is_empty() {
                    p { class: "mt-2 text-sm text-text break-words", "data-testid": "discover-card-snippet", "{hit.snippet}" }
                    p { class: "mt-1 text-[11px] italic text-text-muted", "data-testid": "discover-card-attribution",
                        "{attribution(hit.snippet_source)}"
                    }
                }
                if !hit.tags.is_empty() {
                    div { class: "mt-2 flex flex-wrap gap-1",
                        for tag in hit.tags.iter() {
                            span { key: "{tag}", class: "px-2 py-0.5 rounded-full text-[11px] border border-border text-text-muted", "{tag}" }
                        }
                    }
                }
            }
        }
    }
}
