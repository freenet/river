//! Display-time sanitisation of member nicknames.
//!
//! River shows a 🛡 shield next to members who hold deputy (moderator)
//! authority over the viewer. A nickname is attacker-controlled bytes from
//! the member's own signed `MemberInfoV1.preferred_nickname`, so a member who
//! calls themselves `Alice 🛡` renders a shield that River never granted —
//! a moderator-impersonation ("fake badge") attack. The same trick works for
//! `👑` (room owner), `⭐` (you) and every other glyph River uses as a badge.
//!
//! The fix is **render-time**, not input-time. `riverctl` writes `member_info`
//! straight to the contract and never touches the UI's input validation, so
//! any rule enforced only in the nickname `<input>` is trivially bypassed.
//! The nickname `<input>` *also* rejects emoji (see
//! `member_info_modal::nickname_field`), but that is a UX affordance so honest
//! users get told why their characters vanish — the security boundary is
//! [`sanitize_display_name`], applied at every point where a nickname becomes
//! display text.
//!
//! ## Scope, honestly stated
//!
//! * This **hides** emoji, it does not prevent storage. A CLI user can still
//!   write `Alice 🛡` into the contract; River just never renders the shield.
//!   Enforcing the rule in the room contract would change the contract WASM
//!   and re-key every live room, which is not worth it for a display concern.
//! * Homoglyphs are **not** addressed and cannot be, without breaking the
//!   non-Latin names this function deliberately preserves. A Cyrillic `а` is a
//!   legitimate letter; so is every character in a Chinese, Arabic or Greek
//!   name. Confusable-script detection is a different problem with a different
//!   (much worse) false-positive profile.
//! * Combining marks are preserved, so a "Zalgo" nickname can still be ugly.
//!   It cannot forge a badge, which is what this module is for.
//!
//! ## What gets removed
//!
//! Emoji and pictographic symbols (the badge-forgery vector), plus two classes
//! that exist purely to deceive the reader:
//!
//! * **Invisible formatting** — zero-width joiners/spaces, bidi embedding and
//!   override controls, word joiners. These let a nickname reorder or hide
//!   text around itself (`Alice\u{202E}...`), and ZWJ is how multi-codepoint
//!   emoji sequences are assembled in the first place.
//! * **Private-use area** codepoints. Fonts are free to map these to any glyph
//!   at all (Nerd Fonts map a large PUA range to icons, including shields), so
//!   a PUA nickname renders as a badge on any machine with such a font
//!   installed. River *also* uses `U+E000`/`U+E001` as internal sentinels in
//!   [`crate::components::conversation::message_to_html_with_mentions`], so
//!   stripping PUA additionally keeps a nickname from smuggling a sentinel
//!   into the mention-chip substitution.
//!
//! Everything else is kept. Chinese, Japanese, Korean, Arabic, Hebrew,
//! Cyrillic, Greek, Devanagari and accented Latin names pass through
//! byte-identical, as do ordinary punctuation and CJK punctuation — see the
//! `real_names_in_other_scripts_are_untouched` test, which is the guard
//! against a rule that mangles real people's names (worse than the problem it
//! solves).

use river_core::room_state::privacy::SealedBytes;
use std::collections::HashMap;

/// Rendered in place of a nickname that is empty once sanitised (e.g. the
/// nickname was nothing but emoji). Distinct from `"Unknown"`, which callers
/// already use for "no `member_info` record at all", so the two cases stay
/// distinguishable in the UI.
pub const UNNAMED: &str = "Unnamed";

/// Shown next to a nickname `<input>` whose contents would not survive
/// [`sanitize_display_name`]. One wording, used by every nickname input, so a
/// user gets the same explanation wherever they hit it.
pub const EMOJI_REJECTION_MESSAGE: &str = "Nicknames can't contain emoji";

/// Whether `c` must never appear in rendered display text.
///
/// Ranges are Unicode *blocks* rather than the `Emoji` character property:
/// River has no Unicode-property dependency and the UI's wasm bundle size is
/// a standing concern, so a table of block ranges is the right trade. Blocks
/// are slightly broader than the emoji property (they also catch arrows,
/// geometric shapes and dingbats), which is the safe direction — those are
/// symbols, not letters, and none of them belong in a person's name.
pub fn is_display_hidden(c: char) -> bool {
    // Control characters (C0/C1). A newline or NUL in a nickname is never
    // legitimate and breaks layout.
    if c.is_control() {
        return true;
    }

    matches!(u32::from(c),
        // Symbols that Latin-1 inherited and that render as emoji: © ®
        0x00A9 | 0x00AE
        // ‼ ⁉
        | 0x203C | 0x2049
        // Zero-width space/non-joiner/joiner, LTR/RTL marks. ZWJ is how
        // multi-codepoint emoji sequences are built.
        | 0x200B..=0x200F
        // Line/paragraph separators.
        | 0x2028..=0x2029
        // Bidi embedding / override controls (the `\u{202E}` reversal trick).
        | 0x202A..=0x202E
        // Word joiner, invisible operators, bidi isolates.
        | 0x2060..=0x2064 | 0x2066..=0x206F
        // ™ ℹ
        | 0x2122 | 0x2139
        // Arrows.
        | 0x2190..=0x21FF
        // Miscellaneous Technical (⌚ ⌛ ⏰ …).
        | 0x2300..=0x23FF
        // Enclosed Alphanumerics (① Ⓐ …).
        | 0x2460..=0x24FF
        // Geometric Shapes, Miscellaneous Symbols (☀ ⚔ ⛨ …), Dingbats
        // (✅ ❌ ❤ …) — one contiguous run.
        | 0x25A0..=0x27BF
        // Supplemental Arrows-B.
        | 0x2900..=0x297F
        // Miscellaneous Symbols and Arrows (⬛ ⭐ …).
        | 0x2B00..=0x2BFF
        // Combining enclosing keycap (the `1️⃣` assembler).
        | 0x20E3
        // 〰 〽 and the two emoji-presented enclosed ideographs ㊗ ㊙. The
        // rest of the CJK punctuation and Enclosed CJK blocks is untouched.
        | 0x3030 | 0x303D | 0x3297 | 0x3299
        // Variation selectors — VS16 is what turns a text-presentation
        // character into its emoji glyph.
        | 0xFE00..=0xFE0F
        // Zero-width no-break space / BOM.
        | 0xFEFF
        // Private Use Area (BMP). Font-defined glyphs, and River's own
        // mention sentinels live at U+E000/U+E001.
        | 0xE000..=0xF8FF
        // The emoji planes: Mahjong/Domino/Cards, Enclosed Alphanumeric
        // Supplement (regional-indicator flags), Miscellaneous Symbols and
        // Pictographs, Emoticons, Transport, Supplemental Symbols and
        // Pictographs, Symbols and Pictographs Extended-A. 🛡 is U+1F6E1.
        | 0x1F000..=0x1FAFF
        // Tags — the flag-sequence assembler (🏴󠁧󠁢󠁳󠁣󠁴󠁿).
        | 0xE0000..=0xE007F
        // Supplementary Private Use Areas A and B.
        | 0xF0000..=0xFFFFD
        | 0x100000..=0x10FFFD
    )
}

/// Whether `s` contains anything [`sanitize_display_name`] would remove.
///
/// Drives the nickname `<input>`'s "Nicknames can't contain emoji" message —
/// UX only. Never rely on this for safety: the render-time strip is the
/// boundary, because `riverctl` never runs this code.
pub fn contains_hidden_chars(s: &str) -> bool {
    s.chars().any(is_display_hidden)
}

/// Strip everything [`is_display_hidden`] rejects and tidy the result.
///
/// Removing a character can leave a double space (`"Alice 🛡 Smith"`), so
/// internal whitespace runs are collapsed to one space and the result is
/// trimmed. A name that is empty afterwards becomes [`UNNAMED`] rather than a
/// blank author line.
///
/// A removed character that was itself whitespace (a newline, a paragraph
/// separator) becomes a space rather than vanishing, so `"Alice\nBob"` stays
/// two words instead of collapsing to `"AliceBob"`. Zero-width characters are
/// not whitespace, so `"A\u{200B}lice"` correctly rejoins as `"Alice"`.
pub fn sanitize_display_name(raw: &str) -> String {
    let stripped: String = raw
        .chars()
        .filter_map(|c| match (is_display_hidden(c), c.is_whitespace()) {
            (true, true) => Some(' '),
            (true, false) => None,
            (false, _) => Some(c),
        })
        .collect();

    // Collapse whitespace runs left behind by the removal. `split_whitespace`
    // handles every Unicode space, and the join normalises exotic spaces to a
    // plain one — deliberate, since a nickname has no use for U+2007.
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.is_empty() {
        UNNAMED.to_string()
    } else {
        collapsed
    }
}

/// Decrypt a member's sealed nickname and sanitise it for display.
///
/// **The single choke point for turning `preferred_nickname` into display
/// text.** Every UI surface that shows a nickname — message author lines, the
/// member list, the member-info modal, reply previews, mention chips, DM
/// threads and rail, notifications — routes through here, and
/// [`tests::nickname_render_paths_go_through_display_nickname`] pins that so a
/// new surface can't quietly reintroduce the hole by inlining the unseal.
///
/// Falls back to the sealed value's lossy rendering when decryption fails
/// (a private room whose secret hasn't synced yet), matching what every call
/// site did before this helper existed — and sanitising that path too, since
/// `SealedBytes::Public` returns the raw bytes verbatim.
pub fn display_nickname(sealed: &SealedBytes, secrets: &HashMap<u32, [u8; 32]>) -> String {
    let raw = match crate::util::ecies::unseal_bytes_with_secrets(sealed, secrets) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(_) => sealed.to_string_lossy(),
    };
    sanitize_display_name(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The badge glyphs River itself renders. A nickname able to display any
    /// of these can impersonate a moderator, the room owner, or "you".
    const BADGE_GLYPHS: &[&str] = &["🛡", "👑", "⭐", "🔑", "🎪", "✅", "⚠", "🔰", "⚔"];

    #[test]
    fn badge_glyphs_cannot_survive_a_nickname() {
        for glyph in BADGE_GLYPHS {
            let name = format!("Alice {glyph}");
            let out = sanitize_display_name(&name);
            assert_eq!(out, "Alice", "{glyph} survived sanitisation as {out:?}");
            assert!(
                contains_hidden_chars(&name),
                "{glyph} not flagged as hidden"
            );
        }
    }

    #[test]
    fn shield_with_variation_selector_is_stripped() {
        // U+1F6E1 U+FE0F — the emoji-presentation form, which is what a
        // phone keyboard actually inserts.
        assert_eq!(sanitize_display_name("Mod\u{1F6E1}\u{FE0F}"), "Mod");
    }

    #[test]
    fn zwj_sequences_and_skin_tones_are_stripped() {
        // 👮🏽‍♀️ = U+1F46E U+1F3FD ZWJ U+2640 U+FE0F
        let officer = "Bob \u{1F46E}\u{1F3FD}\u{200D}\u{2640}\u{FE0F}";
        assert_eq!(sanitize_display_name(officer), "Bob");
    }

    #[test]
    fn flag_sequences_are_stripped() {
        // Regional indicators (🇬🇧) and tag sequences (🏴󠁧󠁢󠁳󠁣󠁴󠁿).
        assert_eq!(sanitize_display_name("Kim \u{1F1EC}\u{1F1E7}"), "Kim");
        let scotland = "Ada \u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}";
        assert_eq!(sanitize_display_name(scotland), "Ada");
    }

    #[test]
    fn bidi_overrides_and_zero_width_chars_are_stripped() {
        // RLO can visually reverse the text that follows it, letting a
        // nickname reorder the badge/name/timestamp row around itself.
        assert_eq!(sanitize_display_name("Alice\u{202E}bob"), "Alicebob");
        assert_eq!(sanitize_display_name("A\u{200B}l\u{FEFF}ice"), "Alice");
        assert!(contains_hidden_chars("Alice\u{202E}"));
    }

    #[test]
    fn private_use_area_is_stripped() {
        // Nerd Fonts and similar map PUA to icon glyphs (shields included),
        // and U+E000/U+E001 are River's own mention sentinels.
        assert_eq!(sanitize_display_name("Eve \u{E000}\u{E001}"), "Eve");
        assert_eq!(sanitize_display_name("Eve \u{F0FF}"), "Eve");
        assert_eq!(sanitize_display_name("Eve \u{100001}"), "Eve");
    }

    #[test]
    fn control_characters_are_stripped() {
        assert_eq!(sanitize_display_name("Alice\nBob"), "Alice Bob");
        assert_eq!(sanitize_display_name("Alice\u{0}"), "Alice");
    }

    /// The guard against a rule that mangles real people's names. Every name
    /// here must pass through completely unchanged.
    #[test]
    fn real_names_in_other_scripts_are_untouched() {
        for name in [
            "李小龍",                  // Chinese
            "さくら 田中",             // Japanese (kana + kanji)
            "김민준",                  // Korean
            "محمد عبد الله",           // Arabic
            "דָּוִד",                     // Hebrew with niqqud (combining marks)
            "Иван Петров",             // Cyrillic
            "Γιώργος Παπαδόπουλος",    // Greek
            "अमिताभ बच्चन",             // Devanagari
            "François Müller",         // accented Latin
            "Ægir Þórsson",            // Icelandic
            "Nguyễn Thị Hương",        // Vietnamese
            "José Ñuñez",              // Spanish
            "O'Brien-Smith Jr.",       // punctuation
            "Anne-Marie (Ann)",        // more punctuation
            "user_42",                 // underscores/digits
            "山田 太郎、はじめまして", // CJK punctuation 、
        ] {
            assert_eq!(
                sanitize_display_name(name),
                name,
                "sanitisation altered a legitimate name: {name:?}"
            );
            assert!(
                !contains_hidden_chars(name),
                "legitimate name flagged as containing emoji: {name:?}"
            );
        }
    }

    #[test]
    fn generated_default_handles_are_untouched() {
        // Every handle `crate::nickname` can produce must survive verbatim,
        // or new members would render as a mangled name.
        for first in crate::nickname::FIRST_NAMES {
            for last in crate::nickname::LAST_NAMES {
                let handle = format!("{first} {last}");
                assert_eq!(sanitize_display_name(&handle), handle);
            }
        }
    }

    #[test]
    fn whitespace_left_by_stripping_is_collapsed() {
        assert_eq!(sanitize_display_name("Alice 🛡 Smith"), "Alice Smith");
        assert_eq!(sanitize_display_name("  Alice  "), "Alice");
    }

    #[test]
    fn all_emoji_nickname_falls_back_to_placeholder() {
        assert_eq!(sanitize_display_name("🛡👑⭐"), UNNAMED);
        assert_eq!(sanitize_display_name(""), UNNAMED);
        assert_eq!(sanitize_display_name("   "), UNNAMED);
    }

    #[test]
    fn display_nickname_sanitises_public_and_undecryptable_values() {
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();
        let public = SealedBytes::public("Mallory 🛡".as_bytes().to_vec());
        assert_eq!(display_nickname(&public, &secrets), "Mallory");

        // A private value with no secret available falls back to
        // `to_string_lossy` — a synthetic placeholder, which must also come
        // back sanitised rather than bypassing the strip.
        let private = SealedBytes::private(vec![1, 2, 3], [0u8; 12], 7, 3);
        let out = display_nickname(&private, &secrets);
        assert!(
            !contains_hidden_chars(&out),
            "fallback path bypassed the strip"
        );
    }

    /// **The regression gate.** Render-time stripping only closes the hole if
    /// EVERY surface goes through [`display_nickname`]; one surface that
    /// inlines the old `unseal_bytes_with_secrets(&…preferred_nickname)` /
    /// `preferred_nickname.to_string_lossy()` pattern reopens it, and that is
    /// exactly the mistake a future feature is most likely to make (it is what
    /// twelve call sites looked like before this module existed).
    ///
    /// So: scan the whole UI source tree and fail if any production code
    /// outside this module turns `preferred_nickname` into a display string by
    /// hand. Walking the tree rather than listing files means a NEW component
    /// is covered the day it is written.
    ///
    /// If this fires on a genuinely non-display use of the field, route it
    /// through [`display_nickname`] anyway or move it below `#[cfg(test)]` —
    /// do not weaken the scan.
    #[test]
    fn nickname_render_paths_go_through_display_nickname() {
        use std::path::{Path, PathBuf};

        fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("readable source dir") {
                let path = entry.expect("readable dir entry").path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        assert!(files.len() > 20, "source walk found suspiciously few files");

        let mut offenders: Vec<String> = Vec::new();
        for path in files {
            // This module is the one place allowed to do it.
            if path.ends_with("util/display_name.rs") {
                continue;
            }
            // Deliberately scans the WHOLE file, test modules included. The
            // obvious refinement — cut at the first `#[cfg(test)]`, as
            // `members.rs`'s own pins do — silently disarmed this test:
            // `conversation.rs` has a `#[cfg(test)]` helper at line ~770, so
            // the cut hid 2,800 lines including a real offender. No test
            // fixture in this crate needs the pattern anyway.
            let production = std::fs::read_to_string(&path).expect("readable source file");
            let rel = path
                .strip_prefix(&src)
                .unwrap_or(&path)
                .display()
                .to_string();

            if production.contains("preferred_nickname.to_string_lossy()") {
                offenders.push(format!("{rel}: preferred_nickname.to_string_lossy()"));
            }
            // An unseal whose argument is the nickname field. The window is
            // generous because rustfmt often breaks the call across lines.
            let mut rest = production.as_str();
            while let Some(idx) = rest.find("unseal_bytes_with_secrets(") {
                let after = &rest[idx..];
                let window = &after[..after.len().min(160)];
                if window.contains("preferred_nickname") {
                    offenders.push(format!("{rel}: unseal of preferred_nickname"));
                }
                rest = &after[1..];
            }
        }

        assert!(
            offenders.is_empty(),
            "these UI surfaces turn a nickname into display text without \
             `crate::util::display_name::display_nickname`, so an emoji \
             nickname can forge a 🛡 deputy badge there:\n  {}",
            offenders.join("\n  ")
        );
    }

    #[test]
    fn ascii_is_never_touched() {
        // Cheap total check that the block table has no accidental hole in
        // the printable-ASCII range.
        for byte in 0x20u8..0x7F {
            let c = byte as char;
            assert!(!is_display_hidden(c), "ASCII {c:?} treated as hidden");
        }
    }
}
