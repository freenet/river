//! `@mention` autocomplete in the composer, as in the web UI (`ui/.../conversation/mention.rs`):
//! typing `@` at the start of a word lists the room's members, filtered as you type, and picking
//! one writes the token `@[name](rv:<id>)` that every client reads as a real mention.
//!
//! The popup is not stored: it is worked out from the composer's text and caret each time the
//! screen is drawn, so it closes by itself when the caret leaves the `@word`, when the text
//! changes under it, or when the room cannot be written to. Only the highlighted row and an
//! Escape are remembered.

use rune::{Key, KeyEvent};

use super::Chat;
use super::model::Member;

/// Most members listed, as the web UI's `MENTION_CANDIDATE_LIMIT`.
pub const LIMIT: usize = 8;

/// The open popup.
#[derive(Clone, Debug, PartialEq)]
pub struct Popup {
    /// Byte offset of the `@` in the composer text.
    pub anchor: usize,
    /// What is typed after the `@`.
    pub query: String,
    /// Members that match, best first.
    pub candidates: Vec<Candidate>,
    /// The highlighted row.
    pub selected: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub uid: String,
    pub id: u32,
    pub name: String,
    /// Another listed member has the same name (ignoring case), so the row shows the member id.
    pub duplicate: bool,
}

/// If the caret sits in an `@query` (an `@` at the start of a word, no whitespace between it and
/// the caret), the byte offset of the `@` and the query. An email-like `name@host` is not one.
pub fn detect(text: &str, caret: usize) -> Option<(usize, &str)> {
    if caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let prefix = &text[..caret];
    let mut at = None;
    for (i, c) in prefix.char_indices().rev() {
        if c == '@' {
            at = Some(i);
            break;
        }
        if c.is_whitespace() {
            return None;
        }
    }
    let at = at?;
    if at > 0 && !prefix[..at].chars().next_back()?.is_whitespace() {
        return None;
    }
    Some((at, &prefix[at + 1..]))
}

/// Who `query` offers: members other than you with a name of their own (one that is still a
/// name once its hidden characters are removed), those whose name starts with it first, then
/// those that contain it, each by name; at most `LIMIT`. Whether two share a name is judged over
/// all that match, not only those listed.
pub fn candidates(members: &[Member], me: u32, query: &str) -> Vec<Candidate> {
    let q = query.to_lowercase();
    // (lowercased name, shown name, member), the names worked out once.
    let mut named: Vec<(String, String, &Member)> = members
        .iter()
        .filter(|m| m.id != me && m.claimed)
        .filter_map(|m| {
            let shown = super::visual_fold::sanitize_display_name(&m.nickname);
            let wire = river_core::mention::sanitize_name(&m.nickname);
            (shown != super::visual_fold::UNNAMED && !wire.is_empty())
                .then(|| (shown.to_lowercase(), shown, m))
        })
        .filter(|(lower, ..)| lower.contains(&q))
        .collect();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (lower, ..) in &named {
        *counts.entry(lower.clone()).or_default() += 1;
    }
    // Starting with the query ranks first; the sort is stable.
    named.sort_by_cached_key(|(lower, ..)| (!lower.starts_with(&q), lower.clone()));
    named.truncate(LIMIT);
    named
        .into_iter()
        .map(|(lower, shown, m)| Candidate {
            uid: m.uid.clone(),
            id: m.id,
            duplicate: counts[&lower] > 1,
            name: shown,
        })
        .collect()
}

/// The token a mention of `member` is written as: what `river_core::mention::encode_mention`
/// writes (the member's short id is their `uid`).
pub fn token(uid: &str, name: &str) -> String {
    format!(
        "@[{}]({}{uid})",
        river_core::mention::sanitize_name(name),
        river_core::mention::REF_SCHEME
    )
}

impl Chat {
    /// Whether an Escape closed the popup for this word: the same `@`, and what is typed still
    /// starts with what was typed then.
    fn mention_closed(&self, anchor: usize, query: &str) -> bool {
        self.mention_off
            .as_ref()
            .is_some_and(|(a, q)| *a == anchor && query.starts_with(q.as_str()))
    }

    /// Forget an Escape once its word is gone. Run before each key, so by the key after the
    /// word was deleted or left, a new `@` at the same place opens the popup again.
    pub fn mention_sync(&mut self) {
        if self.mention_off.is_none() {
            return;
        }
        let c = &self.composer;
        let open = c.anchor == c.focus
            && detect(&c.text, c.focus).is_some_and(|(a, q)| self.mention_closed(a, q));
        if !open {
            self.mention_off = None;
        }
    }

    /// The popup the composer shows now, if any.
    pub fn mention_popup(&self) -> Option<Popup> {
        let room = self.actionable()?;
        let c = &self.composer;
        if !c.focused || c.anchor != c.focus || c.preedit().is_some() {
            return None;
        }
        let (anchor, query) = detect(&c.text, c.focus)?;
        if self.mention_closed(anchor, query) {
            return None;
        }
        let candidates = candidates(&room.members, room.me, query);
        if candidates.is_empty() {
            return None;
        }
        let selected = match &self.mention_sel {
            (q, i) if q == query => (*i).min(candidates.len() - 1),
            _ => 0,
        };
        Some(Popup {
            anchor,
            query: query.to_owned(),
            candidates,
            selected,
        })
    }

    /// Write the mention of the candidate with member id `uid` in place of the `@query`, and a
    /// space after.
    pub fn mention_pick_uid(&mut self, uid: &str) {
        let Some(p) = self.mention_popup() else {
            return;
        };
        let Some(c) = p.candidates.iter().find(|c| c.uid == uid) else {
            return;
        };
        let text = format!("{} ", token(&c.uid, &c.name));
        self.composer.set_selection(p.anchor, self.composer.focus);
        self.composer.insert(&text);
        self.focus_composer();
        self.mention_off = None;
    }

    /// Highlight row `index` (the pointer is over it).
    pub fn mention_highlight(&mut self, index: usize) {
        if let Some(p) = self.mention_popup() {
            self.mention_sel = (p.query, index);
        }
    }

    /// Keys while the popup is open: Up and Down move, Enter and Tab pick, Escape closes.
    /// Returns whether the key was used.
    pub fn mention_key(&mut self, ev: &KeyEvent) -> bool {
        let Some(p) = self.mention_popup() else {
            return false;
        };
        let n = p.candidates.len();
        // Only the bare key: with Ctrl, Alt or Cmd (word keys, Option+Enter's newline) or Shift
        // (a selection, a newline) it belongs to the composer.
        let m = ev.mods;
        let bare = !(m.shift || m.ctrl || m.alt || m.logo);
        match ev.key {
            Key::Down | Key::Up if bare => {
                let down = ev.key == Key::Down;
                let i = if down {
                    (p.selected + 1) % n
                } else {
                    (p.selected + n - 1) % n
                };
                self.mention_sel = (p.query, i);
            }
            Key::Escape => self.mention_off = Some((p.anchor, p.query)),
            Key::Enter | Key::Tab if bare => {
                let uid = p.candidates[p.selected].uid.clone();
                self.mention_pick_uid(&uid);
            }
            _ => return false,
        }
        true
    }
}
