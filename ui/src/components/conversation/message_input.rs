use dioxus::prelude::*;
use river_core::room_state::member::MemberId;
use wasm_bindgen::JsCast;

use super::emoji_picker::EmojiPicker;
use super::ReplyContext;

/// Maximum number of members shown in the @mention autocomplete dropdown.
const MENTION_CANDIDATE_LIMIT: usize = 8;

/// In-flight @mention autocomplete state, set while the caret sits inside an
/// `@query` token in the composer.
#[derive(Clone, PartialEq)]
struct MentionAutocomplete {
    /// Byte offset of the `@` in the current message text.
    anchor: usize,
    /// Byte offset just past the typed query (i.e. the caret).
    query_end: usize,
    /// Members matching the query, already truncated to the display limit.
    candidates: Vec<(MemberId, String)>,
    /// Index into `candidates` currently highlighted for keyboard selection.
    selected: usize,
}

fn auto_resize_message_input() {
    let Some(el) = get_message_textarea() else {
        return;
    };
    // Reset height to auto to measure scrollHeight correctly, then clamp to ~7 lines.
    el.style().set_property("height", "auto").ok();
    let new_height = el.scroll_height().min(168);
    el.style()
        .set_property("height", &format!("{}px", new_height))
        .ok();
}

fn get_message_textarea() -> Option<web_sys::HtmlTextAreaElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id("message-input")?
        .dyn_into::<web_sys::HtmlTextAreaElement>()
        .ok()
}

/// Read the textarea caret as a *byte* offset into `value`. `selection_start`
/// is a UTF-16 code-unit index (DOMString semantics), so it must be converted.
fn read_caret_byte(value: &str) -> Option<usize> {
    let el = get_message_textarea()?;
    let caret_u16 = el.selection_start().ok().flatten()? as usize;
    Some(utf16_to_byte_offset(value, caret_u16))
}

/// Focus the textarea and place the caret at byte offset `byte_off` in `value`.
fn set_caret(value: &str, byte_off: usize) {
    let Some(el) = get_message_textarea() else {
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

/// Replace the `@query` span with the chosen member's wire token and a trailing
/// space, then reposition the caret after the inserted token.
fn apply_mention_selection(
    mut message_text: Signal<String>,
    mut mention: Signal<Option<MentionAutocomplete>>,
    idx: usize,
) {
    let Some(m) = mention.peek().clone() else {
        return;
    };
    let Some((id, name)) = m.candidates.get(idx).cloned() else {
        return;
    };
    let value = message_text.peek().to_string();
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
    let token = river_core::mention::encode_mention(id, &name);
    let mut new_value = String::with_capacity(value.len() + token.len() + 1);
    new_value.push_str(&value[..m.anchor]);
    new_value.push_str(&token);
    new_value.push(' ');
    new_value.push_str(&value[m.query_end..]);
    let caret_byte = m.anchor + token.len() + 1;
    message_text.set(new_value.clone());
    mention.set(None);
    // The caret must be set after Dioxus flushes the new value to the DOM.
    crate::util::defer(move || {
        set_caret(&new_value, caret_byte);
        auto_resize_message_input();
    });
}

/// Message input component that owns its own state.
/// This isolates keystroke handling from the parent component,
/// preventing expensive re-renders of the message list on each keystroke.
#[component]
pub fn MessageInput(
    handle_send_message: EventHandler<(String, Option<ReplyContext>)>,
    replying_to: Signal<Option<ReplyContext>>,
    on_request_edit_last: EventHandler<()>,
    /// Maximum message content size in bytes (from room configuration).
    max_message_size: usize,
    /// Mentionable members (id, current nickname), excluding self, sorted by
    /// name. Drives the `@` autocomplete. Changes only when membership changes,
    /// so it does not affect keystroke-level re-rendering.
    members: Vec<(MemberId, String)>,
) -> Element {
    // Own the message state locally - keystrokes only re-render this component
    let mut message_text = use_signal(String::new);
    let mut show_emoji_picker = use_signal(|| false);
    let mut mention = use_signal(|| None as Option<MentionAutocomplete>);

    let mut send_message = move || {
        let text = message_text.peek().to_string();
        if !text.is_empty() && text.len() <= max_message_size {
            let reply_ctx = replying_to.peek().clone();
            message_text.set(String::new());
            replying_to.set(None);
            mention.set(None);
            handle_send_message.call((text, reply_ctx));
            // Defer resize so it runs after Dioxus flushes the cleared value to the DOM;
            // measuring scrollHeight synchronously here still sees the pre-send content.
            crate::util::defer(auto_resize_message_input);
        }
    };

    // Handle emoji selection - insert at end of message
    let handle_emoji_select = move |emoji: String| {
        let current = message_text.peek().to_string();
        message_text.set(format!("{}{}", current, emoji));
    };

    // Snapshot dropdown state for rendering (drops the read guard before rsx).
    let mention_view = mention.read().as_ref().map(|m| {
        // Lowercased nicknames shared by more than one shown candidate need a
        // disambiguating id, since the rows otherwise look identical.
        let dups = duplicate_candidate_names(&m.candidates);
        (m.candidates.clone(), m.selected, dups)
    });

    rsx! {
        // Backdrop for emoji picker - outside the message bar to avoid z-index issues
        if show_emoji_picker() {
            div {
                class: "fixed inset-0 z-40",
                onclick: move |_| show_emoji_picker.set(false),
            }
        }
        // Backdrop for the @mention dropdown: a click anywhere outside the
        // message bar dismisses it (the bar sits at z-50, above this z-40
        // layer, so clicks inside the textarea/buttons are unaffected).
        if mention.read().is_some() {
            div {
                class: "fixed inset-0 z-40",
                // Defer the signal mutation off the event tick per the Dioxus
                // WASM signal-safety rule (.claude/rules/dioxus-signal-safety.md).
                onclick: move |_| crate::util::defer(move || mention.set(None)),
            }
        }
        div { class: "flex-shrink-0 border-t border-border bg-panel relative z-50",
            div { class: "max-w-4xl mx-auto px-4 py-3",
                // Reply preview strip
                {
                    let reply = replying_to.read();
                    if let Some(ctx) = reply.as_ref() {
                        let author = ctx.author_name.clone();
                        let preview = ctx.content_preview.clone();
                        rsx! {
                            div { class: "flex items-center gap-2 mb-2 px-3 py-1.5 bg-surface border-l-2 border-accent rounded text-sm text-text-muted",
                                span { class: "flex-1 truncate",
                                    span { class: "font-medium", "\u{21a9} @{author}: " }
                                    "{preview}"
                                }
                                button {
                                    class: "text-text-muted hover:text-text transition-colors flex-shrink-0",
                                    title: "Cancel reply",
                                    onclick: move |_| replying_to.set(None),
                                    "\u{00d7}"
                                }
                            }
                        }
                    } else {
                        rsx! {}
                    }
                }
                form {
                    class: "flex gap-3 items-end",
                    onsubmit: move |evt| {
                        evt.prevent_default();
                        send_message();
                    },
                    // Emoji picker button and popup
                    div { class: "relative self-center",
                        button {
                            r#type: "button",
                            class: "p-2.5 rounded-xl hover:bg-surface transition-colors",
                            title: "Insert emoji",
                            onclick: move |_| show_emoji_picker.set(!show_emoji_picker()),
                            span {
                                class: "text-lg",
                                style: "filter: grayscale(100%); opacity: 0.6;",
                                "🙂"
                            }
                        }
                        // Emoji picker popup (appears above the button)
                        if show_emoji_picker() {
                            div {
                                class: "absolute bottom-full left-0 mb-2",
                                EmojiPicker {
                                    on_select: handle_emoji_select,
                                    on_close: move |_| show_emoji_picker.set(false),
                                }
                            }
                        }
                    }
                    // Textarea with optional byte counter. `relative` anchors the
                    // @mention autocomplete dropdown.
                    div { class: "flex-1 flex flex-col relative",
                        // @mention autocomplete dropdown (floats above the textarea)
                        if let Some((candidates, selected, dups)) = mention_view {
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
                                            apply_mention_selection(message_text, mention, i);
                                        },
                                        span { class: "font-medium truncate min-w-0 flex-1", "@{name}" }
                                        // Show the member id only when another shown
                                        // candidate shares this nickname (case-insensitive),
                                        // so identical-looking rows can be told apart.
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
                        textarea {
                            id: "message-input",
                            class: "w-full px-4 py-2.5 bg-surface border border-border rounded-xl text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent/50 focus:border-accent transition-colors resize-none min-h-[44px] overflow-y-auto",
                            style: "max-height: 168px;",
                            placeholder: "Type your message...",
                            value: "{message_text}",
                            rows: "1",
                            oninput: move |evt| {
                                let value = evt.value().to_string();
                                message_text.set(value.clone());
                                auto_resize_message_input();
                                // Detect / update the @mention autocomplete.
                                match read_caret_byte(&value)
                                    .and_then(|caret| detect_mention_query(&value, caret))
                                {
                                    Some((at, query)) => {
                                        let candidates = filter_mention_candidates(
                                            &members, &query, MENTION_CANDIDATE_LIMIT,
                                        );
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
                            },
                            onkeydown: move |evt| {
                                let key = evt.key();
                                // @mention navigation takes precedence while the dropdown is open.
                                if mention.peek().is_some() {
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
                                        return;
                                    }
                                    if key == Key::Escape {
                                        evt.prevent_default();
                                        mention.set(None);
                                        return;
                                    }
                                    if (key == Key::Enter || key == Key::Tab) && !evt.modifiers().shift() {
                                        evt.prevent_default();
                                        let idx = mention.peek().as_ref().map(|m| m.selected).unwrap_or(0);
                                        apply_mention_selection(message_text, mention, idx);
                                        return;
                                    }
                                }
                                // Enter without Shift sends the message
                                // Shift+Enter creates a new line (default textarea behavior)
                                if key == Key::Enter && !evt.modifiers().shift() {
                                    evt.prevent_default();
                                    send_message();
                                }
                                // Up arrow in empty input: edit last sent message
                                if key == Key::ArrowUp && message_text.peek().is_empty() {
                                    evt.prevent_default();
                                    on_request_edit_last.call(());
                                }
                            }
                        }
                        // Byte counter — shown when approaching the limit (>80%)
                        {
                            let text_bytes = message_text.read().len();
                            let threshold = max_message_size * 4 / 5; // 80%
                            if text_bytes > threshold {
                                let over = text_bytes > max_message_size;
                                if over {
                                    rsx! {
                                        div { class: "text-xs text-right mt-1 pr-1 text-red-600 dark:text-red-400 font-medium",
                                            "Message too long \u{2014} {text_bytes}/{max_message_size} bytes"
                                        }
                                    }
                                } else {
                                    rsx! {
                                        div { class: "text-xs text-right mt-1 pr-1 text-text-muted",
                                            "{text_bytes}/{max_message_size}"
                                        }
                                    }
                                }
                            } else {
                                rsx! {}
                            }
                        }
                    }
                    {
                        let text_bytes = message_text.read().len();
                        let over_limit = text_bytes > max_message_size;
                        let btn_class = if over_limit {
                            "px-5 py-2.5 bg-gray-400 dark:bg-gray-600 text-white font-medium rounded-xl opacity-50 cursor-not-allowed"
                        } else {
                            "px-5 py-2.5 bg-accent hover:bg-accent-hover text-white font-medium rounded-xl transition-colors cursor-pointer"
                        };
                        let tooltip = if over_limit {
                            format!("Message exceeds the {} byte limit", max_message_size)
                        } else {
                            String::new()
                        };
                        rsx! {
                            button {
                                r#type: "button",
                                class: "{btn_class}",
                                disabled: over_limit,
                                title: "{tooltip}",
                                // Use explicit onclick instead of form submit — on iOS Safari,
                                // tapping a submit button while the keyboard is visible causes
                                // the keyboard to dismiss first, which triggers a viewport resize
                                // that cancels the click→submit event chain.
                                onclick: move |_| send_message(),
                                "Send"
                            }
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
