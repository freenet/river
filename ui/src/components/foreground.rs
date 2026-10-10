//! Whether a conversation is in the foreground: the tab is visible, its
//! history has layout, and no modal covers it. Following an arrival and the
//! read rule both ask this, for the room history and the DM thread alike
//! (`.claude/rules/history-scrolling.md`).
//!
//! Modals open from a mix of global and local signals, so none of those can
//! say whether one is open. Each modal root, and each popover attached to a
//! history row, mounts a [`ModalPresence`] instead: it registers the modal
//! while mounted and, when it unmounts, bumps [`FOREGROUND_CHANGED`] so the
//! read rule re-checks what is on screen. `every_modal_root_has_a_presence`
//! pins one in each root.

use dioxus::prelude::*;
use std::cell::RefCell;

thread_local! {
    /// The names of the mounted [`ModalPresence`]s. Plain, not a signal: read
    /// from render and raw DOM callbacks, which must not subscribe. A `Vec`, so
    /// a remount that mounts a name before the old instance drops keeps it open.
    static OPEN_MODALS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

/// Bumped when a conversation may have come back to the foreground: the tab
/// became visible, or a modal closed. The read rule's re-check effects
/// subscribe to it; nothing scrolls on it.
pub static FOREGROUND_CHANGED: GlobalSignal<u64> = Global::new(|| 0);

/// Record that a conversation may have come back to the foreground. Callers
/// are in a clean context (a `defer` closure or a raw DOM event handler that
/// defers), so this writes directly; the next value is computed before the
/// write guard.
pub fn note_foreground_changed() {
    let next = FOREGROUND_CHANGED.peek().wrapping_add(1);
    *FOREGROUND_CHANGED.write() = next;
}

/// Marks the modal it is mounted in as open for as long as it stays mounted.
/// Put it inside the modal's root, so it mounts and unmounts with the modal's
/// markup. Renders nothing.
#[component]
pub fn ModalPresence(name: &'static str) -> Element {
    use_hook(move || open_modal(name));
    use_drop(move || {
        close_modal(name);
        crate::util::defer(note_foreground_changed);
    });
    rsx! {}
}

fn open_modal(name: &'static str) {
    OPEN_MODALS.with(|open| open.borrow_mut().push(name));
}

fn close_modal(name: &'static str) {
    OPEN_MODALS.with(|open| {
        let mut open = open.borrow_mut();
        if let Some(i) = open.iter().position(|n| *n == name) {
            open.remove(i);
        }
    });
}

/// Is a modal open other than `except`? A DM thread passes its own name, so
/// only a modal over the thread counts; the room passes `None`.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn modal_open_except(except: Option<&str>) -> bool {
    OPEN_MODALS.with(|open| open.borrow().iter().any(|n| Some(*n) != except))
}

/// The tab is visible, `container` has layout (a hidden mobile panel measures
/// 0), and no modal but `except` is open.
#[cfg(target_arch = "wasm32")]
pub fn in_foreground(except: Option<&str>, container: &web_sys::Element) -> bool {
    crate::components::app::document_title::get_visibility_state()
        && container.client_height() > 0
        && !modal_open_except(except)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::Lexeme;

    fn reset() {
        OPEN_MODALS.with(|open| open.borrow_mut().clear());
    }

    #[test]
    fn the_registry_tracks_open_modals_by_name() {
        reset();
        assert!(!modal_open_except(None));
        open_modal("member-info");
        assert!(modal_open_except(None));
        close_modal("member-info");
        assert!(!modal_open_except(None));
    }

    #[test]
    fn a_thread_is_covered_only_by_a_modal_other_than_itself() {
        reset();
        open_modal("dm-thread");
        assert!(!modal_open_except(Some("dm-thread")));
        assert!(modal_open_except(None), "the thread covers the room");
        open_modal("dm-confirm-delete");
        assert!(modal_open_except(Some("dm-thread")));
        close_modal("dm-confirm-delete");
        close_modal("dm-thread");
    }

    #[test]
    fn a_name_mounted_twice_and_dropped_once_stays_open() {
        reset();
        open_modal("dm-thread");
        open_modal("dm-thread");
        close_modal("dm-thread");
        assert!(modal_open_except(None));
        close_modal("dm-thread");
        assert!(!modal_open_except(None));
    }

    /// Every modal root, and every popover attached to a history row.
    /// Updating this count is the point of failure when a modal is added.
    const MODAL_ROOTS: usize = 16;

    /// Where each modal root or row popover starts: a `class:` literal holding
    /// both `fixed inset-0` and `z-50`, an `"aria-modal": "true"`, or one of
    /// the row popovers' test ids.
    fn modal_markers(src: &str, kinds: &[Lexeme]) -> Vec<usize> {
        // A marker is code, or a literal opened from code.
        let in_code = |i: usize| i > 0 && kinds[i - 1] == Lexeme::Code;
        let mut at = Vec::new();
        for (i, _) in src.match_indices("class: \"").filter(|&(i, _)| in_code(i)) {
            let start = i + "class: \"".len();
            let end = src[start..].find('"').map_or(src.len(), |e| start + e);
            let class = &src[start..end];
            if class.contains("fixed inset-0") && class.contains("z-50") {
                at.push(i);
            }
        }
        for needle in [
            "\"aria-modal\": \"true\"",
            "\"data-testid\": \"message-action-menu\"",
            "\"data-testid\": \"emoji-picker\"",
        ] {
            at.extend(
                src.match_indices(needle)
                    .map(|(i, _)| i)
                    .filter(|&i| in_code(i)),
            );
        }
        at
    }

    /// The `{ … }` of the rsx element enclosing byte `at`, as byte offsets of
    /// its two braces. Braces in literals do not count.
    fn enclosing_block(src: &str, kinds: &[Lexeme], at: usize) -> (usize, usize) {
        let b = src.as_bytes();
        let mut depth = 0usize;
        let mut open = at;
        while open > 0 {
            open -= 1;
            if kinds[open] != Lexeme::Code {
                continue;
            }
            match b[open] {
                b'}' => depth += 1,
                b'{' if depth == 0 => break,
                b'{' => depth -= 1,
                _ => {}
            }
        }
        let mut depth = 0usize;
        let mut close = open;
        for (i, &c) in b.iter().enumerate().skip(open) {
            if kinds[i] != Lexeme::Code {
                continue;
            }
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        (open, close)
    }

    /// Does the block mount a `ModalPresence` as a direct child, not only
    /// inside a nested element (the DM thread nests its confirmation)?
    fn has_direct_presence(src: &str, kinds: &[Lexeme], (open, close): (usize, usize)) -> bool {
        let b = src.as_bytes();
        let mut depth = 0usize;
        for i in open..=close {
            if kinds[i] != Lexeme::Code {
                continue;
            }
            match b[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ if depth == 1 && src[i..].starts_with("ModalPresence {") => return true,
                _ => {}
            }
        }
        false
    }

    #[test]
    fn every_modal_root_has_a_presence() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut roots = 0;
        let mut missing = Vec::new();
        for path in crate::util::source_scan::rust_files(&src_dir) {
            let raw = std::fs::read_to_string(&path).expect("readable source file");
            let src = crate::util::source_scan::production_only(&raw);
            let kinds = crate::util::lex(&src);
            for at in modal_markers(&src, &kinds) {
                roots += 1;
                if !has_direct_presence(&src, &kinds, enclosing_block(&src, &kinds, at)) {
                    let line = src[..at].matches('\n').count() + 1;
                    let rel = path.strip_prefix(&src_dir).unwrap_or(&path).display();
                    missing.push(format!("{rel}:~{line}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "modal roots without a `ModalPresence` (arrivals would scroll, and \
             the history be read, behind them): {missing:#?}"
        );
        assert_eq!(
            roots, MODAL_ROOTS,
            "the scan found a different number of modal roots; a new modal needs \
             a `ModalPresence`, and a removed one an updated count"
        );
    }
}
