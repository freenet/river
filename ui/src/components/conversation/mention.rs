//! Reusable `@mention` autocomplete machinery shared by the new-message
//! composer (`message_input.rs`) and the inline message-edit form
//! (`conversation.rs::MessageGroupComponent`).
//!
//! Everything here is parameterised by the DOM `id` of the target
//! `<textarea>` so the same caret math, query detection, candidate filtering,
//! and dropdown rendering drive both call sites without duplication.

use dioxus::prelude::*;
use river_core::room_state::member::MemberId;
use wasm_bindgen::JsCast;

/// Maximum number of members shown in the @mention autocomplete dropdown.
pub const MENTION_CANDIDATE_LIMIT: usize = 8;

/// In-flight @mention autocomplete state, set while the caret sits inside an
/// `@query` token in a textarea.
#[derive(Clone, PartialEq)]
pub struct MentionAutocomplete {
    /// Byte offset of the `@` in the current text.
    pub anchor: usize,
    /// Byte offset just past the typed query (i.e. the caret).
    pub query_end: usize,
    /// Members matching the query, already truncated to the display limit.
    pub candidates: Vec<(MemberId, String)>,
    /// Index into `candidates` currently highlighted for keyboard selection.
    pub selected: usize,
}

/// Look up a `<textarea>` by its DOM `id`.
fn get_textarea(id: &str) -> Option<web_sys::HtmlTextAreaElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(id)?
        .dyn_into::<web_sys::HtmlTextAreaElement>()
        .ok()
}

/// Read the textarea caret as a *byte* offset into `value`. `selection_start`
/// is a UTF-16 code-unit index (DOMString semantics), so it must be converted.
fn read_caret_byte(id: &str, value: &str) -> Option<usize> {
    let el = get_textarea(id)?;
    let caret_u16 = el.selection_start().ok().flatten()? as usize;
    Some(utf16_to_byte_offset(value, caret_u16))
}

/// Focus the textarea and place the caret at byte offset `byte_off` in `value`.
fn set_caret(id: &str, value: &str, byte_off: usize) {
    let Some(el) = get_textarea(id) else {
        return;
    };
    let _ = el.focus();
    let off = byte_to_utf16_offset(value, byte_off);
    let _ = el.set_selection_range(off, off);
}

fn utf16_to_byte_offset(s: &str, utf16_off: usize) -> usize {
    let mut u16count = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        if u16count >= utf16_off {
            return byte_idx;
        }
        u16count += ch.len_utf16();
    }
    s.len()
}

fn byte_to_utf16_offset(s: &str, byte_off: usize) -> u32 {
    let mut u16count = 0u32;
    for (byte_idx, ch) in s.char_indices() {
        if byte_idx >= byte_off {
            break;
        }
        u16count += ch.len_utf16() as u32;
    }
    u16count
}

/// If the caret sits inside an `@query` token (a `@` at a word boundary with no
/// whitespace between it and the caret), return `(byte offset of '@', query)`.
fn detect_mention_query(value: &str, byte_caret: usize) -> Option<(usize, String)> {
    if byte_caret > value.len() || !value.is_char_boundary(byte_caret) {
        return None;
    }
    let prefix = &value[..byte_caret];
    // Walk back from the caret: the first `@` (with no intervening whitespace)
    // starts the query; any whitespace first means we're not in a token.
    let mut at = None;
    for (idx, ch) in prefix.char_indices().rev() {
        if ch == '@' {
            at = Some(idx);
            break;
        }
        if ch.is_whitespace() {
            return None;
        }
    }
    let at = at?;
    // The `@` must begin a word: at start-of-text or after whitespace. This
    // skips email-like `name@host` (the char before `@` is not whitespace).
    if at > 0 {
        let before = prefix[..at].chars().next_back()?;
        if !before.is_whitespace() {
            return None;
        }
    }
    Some((at, prefix[at + 1..].to_string()))
}

/// Rank members against a query: case-insensitive prefix matches first, then
/// substring matches, preserving the input order (caller pre-sorts by name).
/// An empty query lists everyone (up to `limit`).
fn filter_mention_candidates(
    members: &[(MemberId, String)],
    query: &str,
    limit: usize,
) -> Vec<(MemberId, String)> {
    let q = query.to_lowercase();
    let mut prefix = Vec::new();
    let mut substr = Vec::new();
    for (id, name) in members {
        let lname = name.to_lowercase();
        if q.is_empty() || lname.starts_with(&q) {
            prefix.push((*id, name.clone()));
        } else if lname.contains(&q) {
            substr.push((*id, name.clone()));
        }
    }
    prefix.extend(substr);
    prefix.truncate(limit);
    prefix
}

/// Lowercased nicknames that appear on more than one of the shown candidates.
/// Those rows look identical, so the dropdown shows a disambiguating member id
/// beside them. Case-insensitive, matching the resolver's ambiguity key (so the
/// picker and riverctl's send-time `@name` resolution agree on what "ambiguous"
/// means). Near-but-distinct names (e.g. "Alice"/"Alicia") are intentionally
/// NOT treated as collisions — they are already distinguishable by sight.
fn duplicate_candidate_names(
    candidates: &[(MemberId, String)],
) -> std::collections::HashSet<String> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (_, name) in candidates {
        *counts.entry(name.to_lowercase()).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(name, _)| name)
        .collect()
}

/// Recompute the `@mention` dropdown state for textarea `id` from an `oninput`
/// event's new `value`. Sets `mention` to the matching candidates (or clears it
/// when the caret isn't inside an `@query` or nothing matches).
pub fn update_mention_from_input(
    id: &str,
    value: &str,
    members: &[(MemberId, String)],
    mut mention: Signal<Option<MentionAutocomplete>>,
) {
    match read_caret_byte(id, value).and_then(|caret| detect_mention_query(value, caret)) {
        Some((at, query)) => {
            let candidates = filter_mention_candidates(members, &query, MENTION_CANDIDATE_LIMIT);
            if candidates.is_empty() {
                mention.set(None);
            } else {
                mention.set(Some(MentionAutocomplete {
                    anchor: at,
                    query_end: at + 1 + query.len(),
                    candidates,
                    selected: 0,
                }));
            }
        }
        None => mention.set(None),
    }
}

/// Replace the `@query` span in textarea `id` with the chosen member's wire
/// token and a trailing space, then reposition the caret after the inserted
/// token. `after` runs in the same deferred tick as the caret reposition (used
/// e.g. to auto-resize the composer textarea); pass `|| {}` when nothing else
/// is needed.
pub fn apply_mention_selection(
    id: String,
    mut text: Signal<String>,
    mut mention: Signal<Option<MentionAutocomplete>>,
    idx: usize,
    after: impl FnOnce() + 'static,
) {
    let Some(m) = mention.peek().clone() else {
        return;
    };
    let Some((member_id, name)) = m.candidates.get(idx).cloned() else {
        return;
    };
    let value = text.peek().to_string();
    // Guard the stored offsets against a value that changed out from under us
    // (they are only valid for the value captured at the last keystroke).
    if m.anchor > m.query_end
        || m.query_end > value.len()
        || !value.is_char_boundary(m.anchor)
        || !value.is_char_boundary(m.query_end)
    {
        mention.set(None);
        return;
    }
    let token = river_core::mention::encode_mention(member_id, &name);
    let mut new_value = String::with_capacity(value.len() + token.len() + 1);
    new_value.push_str(&value[..m.anchor]);
    new_value.push_str(&token);
    new_value.push(' ');
    new_value.push_str(&value[m.query_end..]);
    let caret_byte = m.anchor + token.len() + 1;
    text.set(new_value.clone());
    mention.set(None);
    // The caret must be set after Dioxus flushes the new value to the DOM.
    crate::util::defer(move || {
        set_caret(&id, &new_value, caret_byte);
        after();
    });
}

/// Handle a textarea `onkeydown` event for `@mention` navigation in textarea
/// `id`. Returns `true` when the dropdown consumed the key (Arrow Up/Down to
/// move, Escape to dismiss, Enter/Tab to accept) so the caller can `return`
/// before its own Enter/Escape handling. Returns `false` (a no-op) when the
/// dropdown isn't open or the key isn't one it handles. `after` runs after an
/// accept, like `apply_mention_selection`.
pub fn handle_mention_keydown(
    id: &str,
    evt: &KeyboardEvent,
    text: Signal<String>,
    mut mention: Signal<Option<MentionAutocomplete>>,
    after: impl FnOnce() + 'static,
) -> bool {
    if mention.peek().is_none() {
        return false;
    }
    let key = evt.key();
    if key == Key::ArrowDown || key == Key::ArrowUp {
        evt.prevent_default();
        let down = key == Key::ArrowDown;
        mention.with_mut(|opt| {
            if let Some(m) = opt.as_mut() {
                let n = m.candidates.len();
                if n > 0 {
                    m.selected = if down {
                        (m.selected + 1) % n
                    } else {
                        (m.selected + n - 1) % n
                    };
                }
            }
        });
        return true;
    }
    if key == Key::Escape {
        evt.prevent_default();
        mention.set(None);
        return true;
    }
    if (key == Key::Enter || key == Key::Tab) && !evt.modifiers().shift() {
        evt.prevent_default();
        let idx = mention.peek().as_ref().map(|m| m.selected).unwrap_or(0);
        apply_mention_selection(id.to_string(), text, mention, idx, after);
        return true;
    }
    false
}

/// The `@mention` autocomplete dropdown. Renders nothing when `mention` is
/// `None`. Anchored `absolute bottom-full` so the caller must place it inside a
/// `position: relative` container that also holds the textarea. `on_pick` is
/// invoked with the chosen candidate index (use `apply_mention_selection`).
#[component]
pub fn MentionDropdown(
    mention: Signal<Option<MentionAutocomplete>>,
    on_pick: EventHandler<usize>,
) -> Element {
    // Snapshot dropdown state for rendering (drops the read guard before rsx).
    let view = mention.read().as_ref().map(|m| {
        // Lowercased nicknames shared by more than one shown candidate need a
        // disambiguating id, since the rows otherwise look identical.
        let dups = duplicate_candidate_names(&m.candidates);
        (m.candidates.clone(), m.selected, dups)
    });
    let Some((candidates, selected, dups)) = view else {
        return rsx! {};
    };
    rsx! {
        div {
            class: "absolute bottom-full left-0 mb-1 w-64 max-h-56 overflow-y-auto bg-panel border border-border rounded-xl shadow-lg z-50",
            role: "listbox",
            "aria-label": "Mention a member",
            for (i, (id, name)) in candidates.into_iter().enumerate() {
                button {
                    key: "{river_core::mention::member_id_to_hex(id)}",
                    r#type: "button",
                    role: "option",
                    "aria-selected": if i == selected { "true" } else { "false" },
                    class: format!(
                        "w-full text-left px-3 py-2 text-sm flex items-baseline justify-between gap-2 {}",
                        if i == selected { "bg-accent/15 text-accent" } else { "text-text hover:bg-surface" }
                    ),
                    // Use mousedown + preventDefault so the textarea
                    // doesn't blur before the selection is applied.
                    onmousedown: move |evt| {
                        evt.prevent_default();
                        on_pick.call(i);
                    },
                    span { class: "font-medium truncate min-w-0 flex-1", "@{name}" }
                    // Show the member id only when another shown candidate shares
                    // this nickname (case-insensitive), so identical-looking rows
                    // can be told apart.
                    if dups.contains(&name.to_lowercase()) {
                        span {
                            class: "text-xs text-text-muted font-mono shrink-0",
                            title: "Member ID (shown because another member shares this name)",
                            "{id}"
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(n: i64) -> MemberId {
        MemberId(freenet_scaffold::util::FastHash(n))
    }

    #[test]
    fn duplicate_candidate_names_flags_only_case_insensitive_collisions() {
        let cands = vec![
            (mid(1), "Alice".to_string()),
            (mid(2), "alice".to_string()), // collides with "Alice" (case-insensitive)
            (mid(3), "Alicia".to_string()), // near but distinct — NOT a collision
            (mid(4), "Bob".to_string()),   // unique
        ];
        let dups = duplicate_candidate_names(&cands);
        assert!(dups.contains("alice"), "case-insensitive duplicate flagged");
        assert!(!dups.contains("alicia"), "near-name must not be flagged");
        assert!(!dups.contains("bob"));
        assert_eq!(dups.len(), 1);
    }

    fn members() -> Vec<(MemberId, String)> {
        vec![
            (
                MemberId(freenet_scaffold::util::FastHash(1)),
                "Alice".to_string(),
            ),
            (
                MemberId(freenet_scaffold::util::FastHash(2)),
                "Albert".to_string(),
            ),
            (
                MemberId(freenet_scaffold::util::FastHash(3)),
                "Bob".to_string(),
            ),
        ]
    }

    #[test]
    fn detects_query_at_caret() {
        assert_eq!(
            detect_mention_query("hi @al", 6),
            Some((3, "al".to_string()))
        );
        // Caret at the bare '@'
        assert_eq!(detect_mention_query("@", 1), Some((0, String::new())));
        // '@' at start of text
        assert_eq!(detect_mention_query("@bo", 3), Some((0, "bo".to_string())));
    }

    #[test]
    fn no_query_when_not_in_token() {
        assert_eq!(detect_mention_query("hello world", 11), None);
        // whitespace between '@' and caret
        assert_eq!(detect_mention_query("@al bob", 7), None);
        // email-like: '@' not at a word boundary
        assert_eq!(detect_mention_query("name@host", 9), None);
    }

    #[test]
    fn filter_prefers_prefix_then_substring() {
        // "al" → Albert and Alice by prefix (input order preserved)
        let r = filter_mention_candidates(&members(), "al", 8);
        let names: Vec<_> = r.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, vec!["Alice", "Albert"]);
        // substring-only match
        let r = filter_mention_candidates(&members(), "ob", 8);
        assert_eq!(
            r.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(),
            vec!["Bob"]
        );
        // empty query lists everyone
        assert_eq!(filter_mention_candidates(&members(), "", 8).len(), 3);
        // limit is honoured
        assert_eq!(filter_mention_candidates(&members(), "", 1).len(), 1);
    }

    #[test]
    fn utf16_byte_offset_round_trips_with_multibyte() {
        // "é" is 2 bytes / 1 utf16 unit; "𝄞" is 4 bytes / 2 utf16 units.
        let s = "aé𝄞b";
        for (byte_off, _) in s.char_indices().chain(std::iter::once((s.len(), ' '))) {
            let u16 = byte_to_utf16_offset(s, byte_off);
            assert_eq!(utf16_to_byte_offset(s, u16 as usize), byte_off);
        }
    }

    #[test]
    fn detect_handles_multibyte_before_at() {
        // "é @bo" — the '@' is preceded by a space, query is "bo".
        let s = "é @bo";
        let caret = s.len();
        let (at, q) = detect_mention_query(s, caret).unwrap();
        assert_eq!(&s[at..at + 1], "@");
        assert_eq!(q, "bo");
    }
}
