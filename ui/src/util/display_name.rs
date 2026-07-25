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
//! * Homoglyphs are **not** stripped, and must not be: a Cyrillic `а` is a
//!   legitimate letter, as is every character in a Chinese, Arabic or Greek
//!   name, and this function's job is to preserve them. Homoglyph
//!   impersonation is handled separately and non-destructively, by
//!   [`collision_key`] + `components::members::name_flags_for_viewer`, which
//!   MARK a confusable name rather than altering it. Detection can afford
//!   false positives; rewriting a name cannot.
//! * Combining marks are preserved, so a "Zalgo" nickname can still be ugly.
//!   It cannot forge a badge, which is what this module is for.
//! * Sanitising can CREATE a collision: `"B⭐ob"` and `"Bob"` render alike, as
//!   do two nicknames that differ only in stripped characters. Nicknames were
//!   never unique in River, so this grants no new capability, but identical
//!   rendered names are possible by construction. The specific vectors that
//!   are cheap to close ARE closed — invisible-but-not-whitespace characters
//!   are stripped, space-lookalikes are normalised, and a joiner is only kept
//!   between two non-ASCII letters — but a joiner inside a CJK name still
//!   clones it, and homoglyphs always will. Note that
//!   `conversation::mention::duplicate_candidate_names` compares name STRINGS,
//!   so it does not flag a pair that differs only in kept characters.
//!
//! ## What gets removed
//!
//! Emoji and pictographic symbols (the badge-forgery vector), plus two classes
//! that exist purely to deceive the reader:
//!
//! * **Invisible and blank characters** — zero-width space, bidi embedding and
//!   override controls, soft hyphen, interlinear annotation, the Hangul
//!   fillers, Braille blank. These either reorder the text around them
//!   (`Alice\u{202E}...`) or let two members render a pixel-identical name,
//!   which would defeat telling a moderator from an impersonator even with the
//!   badge itself correct.
//!
//!   `U+200C` ZWNJ and `U+200D` ZWJ are a special case. They look like emoji
//!   machinery, but they are orthography in Persian, Sinhala and Malayalam,
//!   and blanket-stripping them mangles real names. They are kept only where
//!   they can be doing that work — between two non-ASCII letters, or trailing
//!   one (Malayalam chillu ends a name) — and dropped everywhere else, which
//!   covers both the orphans the emoji strip leaves behind and the
//!   `"Bo\u{200D}b"` clone of `"Bob"`. Keeping them is safe because every
//!   emoji a joiner could assemble is itself removed.
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
        // Zero-width space, and the LTR/RTL marks.
        //
        // NOT U+200C ZWNJ or U+200D ZWJ. Those look like emoji machinery — ZWJ
        // is what joins 👩 + ZWJ + 💻 — but they are orthography in several
        // scripts, and stripping them mangles real names: Persian compounds
        // (`علی‌رضا` Alireza, `حسین‌زاده` Hosseinzadeh) need ZWNJ to keep the
        // preceding letter in its final form, Sinhala touching letters
        // (`සූර්‍ය` Surya) need ZWJ, and several Malayalam IMEs emit chillu as
        // consonant + virama + ZWJ. Keeping them is safe here because every
        // emoji a ZWJ could join is itself stripped, so the joiner has nothing
        // left to assemble.
        | 0x200B | 0x200E | 0x200F
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
        // The remaining invisible `Cf` formatting characters that no range
        // above covers, and that `char::is_control()` (which is `Cc` only)
        // misses. U+061C ARABIC LETTER MARK is a bidi control like U+200E/F;
        // U+00AD SOFT HYPHEN and U+180E MONGOLIAN VOWEL SEPARATOR render as
        // nothing; U+FFF9..U+FFFB are interlinear annotation anchors that hide
        // the text between them.
        | 0x00AD | 0x061C | 0x180E | 0xFFF9..=0xFFFB
        // Blank glyphs that are not whitespace, so the space collapse below
        // would not remove them: they let two members share a pixel-identical
        // rendered name (`Alice` vs `Alice\u{3164}`), which undermines the
        // "you can tell members apart by name" assumption the badge sits on.
        // U+2800 BRAILLE PATTERN BLANK, and the Hangul fillers.
        | 0x115F | 0x1160 | 0x2800 | 0x3164 | 0xFFA0
        // The rest of the Default_Ignorable characters, which render as
        // nothing. Without these a nickname made ENTIRELY of them is
        // non-empty (so it never becomes `UNNAMED`) yet renders blank, which
        // makes a message header look like a continuation of the group above
        // it — including a badged moderator's group.
        // U+034F combining grapheme joiner, the Mongolian free variation
        // selectors, the Khmer inherent vowels, U+2065, the Variation
        // Selectors Supplement (U+FE00..FE0F's big brother), and the
        // shorthand-format and musical beam/slur/phrase controls.
        | 0x034F | 0x180B..=0x180D | 0x180F | 0x17B4..=0x17B5 | 0x2065
        | 0xE0100..=0xE01EF | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        // Text-presentation symbols that read as a badge in the fonts that
        // carry them: ۞ (ornate star, present wherever Arabic renders), ٭,
        // ꙳, and the Phaistos shield. Plus Symbols for Legacy Computing and
        // its supplement, which contain an inverse check mark and stick
        // figures.
        | 0x066D | 0x06DE | 0xA673 | 0x101DB
        | 0x1FB00..=0x1FBFF | 0x1CC00..=0x1CEBF
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
///
/// Only runs of ASCII space are collapsed, and only interior ones. Non-ASCII
/// spaces are left alone: U+3000 IDEOGRAPHIC SPACE is the conventional
/// separator between a Japanese surname and given name (`山田　太郎`), and
/// normalising it to `' '` would quietly rewrite a real name.
pub fn sanitize_display_name(raw: &str) -> String {
    // 1. Remove the hidden characters. A hidden character that was itself
    //    whitespace becomes a space so words don't run together.
    let stripped: Vec<char> = raw
        .chars()
        .filter_map(|c| match (is_display_hidden(c), c.is_whitespace()) {
            (true, true) => Some(' '),
            (true, false) => None,
            (false, _) => Some(c),
        })
        .collect();

    // 2. Keep a joiner only where it can be doing orthographic work, which is
    //    between two NON-ASCII letters (Persian `علی‌رضا`, Sinhala `සූර්‍ය`,
    //    Malayalam chillu — which is consonant + virama + ZWJ and legitimately
    //    ENDS a name, so a trailing joiner after a non-ASCII letter is kept
    //    too). Everywhere else it is dropped, which covers two cases:
    //
    //    * Orphans left by step 1: `"Bob 👮🏽‍♀️"` strips down to `"Bob ‍"`.
    //    * The clone attack `"Bo\u{200D}b"`, which renders exactly like
    //      `"Bob"` in any Latin font. Latin script never needs a joiner, so
    //      requiring a non-ASCII neighbour costs nothing and closes it.
    //
    //    A joiner between two non-ASCII letters is still kept, so a CJK name
    //    can still be cloned this way. That is the residual documented in the
    //    module header, and it is the same shape as the homoglyph problem.
    let is_joiner = |c: char| c == '\u{200C}' || c == '\u{200D}';
    let joins_letters =
        |c: Option<&char>| c.is_some_and(|c| !c.is_ascii() && !c.is_whitespace() && !is_joiner(*c));
    let kept: Vec<char> = stripped
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            !is_joiner(**c)
                || (*i > 0
                    && joins_letters(stripped.get(i - 1))
                    // A trailing joiner is legitimate (Malayalam chillu), so
                    // "no next character" is allowed; a next character that is
                    // present must itself be a non-ASCII letter.
                    && stripped.get(i + 1).is_none_or(|n| joins_letters(Some(n))))
        })
        .map(|(_, c)| *c)
        .collect();

    // 3. Normalise the spaces that are visually IDENTICAL to U+0020 (NBSP and
    //    friends) down to it, then collapse runs. Normalising loses nothing a
    //    reader can see, and it closes the `"Alice\u{00A0}Smith"` clone of
    //    `"Alice Smith"`. U+3000 IDEOGRAPHIC SPACE is deliberately NOT in this
    //    set: it renders double-width, it is visibly different, and it is the
    //    conventional separator in a Japanese name (`山田　太郎`).
    let looks_like_a_plain_space =
        |c: char| matches!(u32::from(c), 0x00A0 | 0x2000..=0x200A | 0x202F | 0x205F);
    let mut collapsed = String::with_capacity(kept.len());
    let mut last_was_space = false;
    for c in kept {
        let c = if looks_like_a_plain_space(c) { ' ' } else { c };
        let is_space = c == ' ';
        if !(is_space && last_was_space) {
            collapsed.push(c);
        }
        last_was_space = is_space;
    }

    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        UNNAMED.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Fold a display name to a key for detecting members who LOOK like each
/// other, for the duplicate-name and impersonation markers in
/// `components::members`.
///
/// Deliberately far more aggressive than any name comparison used for
/// enforcement. The automated impersonator ban must match near-exactly,
/// because a false positive there bans an innocent person; a marker in the UI
/// costs nothing when it is occasionally wrong, so it should catch the whole
/// class the ban deliberately will not. **Do not "harmonise" the two.**
///
/// The fold, in order: [`sanitize_display_name`] (emoji, invisibles, space
/// lookalikes), lowercase, map confusable characters onto one ASCII
/// representative, then drop everything that is not alphanumeric. So
/// `Ian Clarke`, `IAN CLARKE`, `Ian_Clarke`, `IanClarke`, `lan Clarke` (with a
/// lowercase L) and `Ian Clarkе` (with a Cyrillic е) all fold to `ianclarke`.
///
/// Returns an empty string for a name with no alphanumeric content; callers
/// must treat that as "no key" rather than as a match, or every such member
/// would collide with every other.
pub fn collision_key(name: &str) -> String {
    sanitize_display_name(name)
        .to_lowercase()
        .chars()
        .map(fold_confusable)
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Map a lowercased character onto the ASCII letter it is confusable with.
///
/// Covers the Cyrillic and Greek lookalikes that make a homoglyph attack work,
/// plus the two digit/letter pairs that are confusable in essentially every
/// font (`0`/`o`, `1`/`l`/`i`). Deliberately NOT full leetspeak: folding `3`
/// into `e` would make `bob3` collide with `bobe`, which is surprising for a
/// marker that is meant to be quiet unless something is wrong.
fn fold_confusable(c: char) -> char {
    match c {
        'а' | 'α' => 'a',
        'ь' | 'ъ' => 'b',
        'с' | 'ϲ' => 'c',
        'ԁ' => 'd',
        'е' | 'ε' | 'э' => 'e',
        'ɡ' | 'ց' => 'g',
        'һ' | 'հ' => 'h',
        // `l` and `1` fold into `i`: `lan Clarke` / `1an Clarke` are the
        // classic impersonations of `Ian Clarke`.
        'l' | '1' | 'ı' | 'і' | 'ⅰ' | 'ⅼ' | 'ӏ' | 'ǀ' => 'i',
        'ј' | 'ȷ' => 'j',
        'κ' | 'к' => 'k',
        'м' | 'ｍ' => 'm',
        'п' | 'ո' => 'n',
        'о' | 'ο' | '0' | 'օ' => 'o',
        'р' | 'ρ' => 'p',
        'ԛ' => 'q',
        'ѕ' | 'ș' => 's',
        'т' | 'τ' => 't',
        'υ' | 'ս' => 'u',
        'ν' | 'ѵ' => 'v',
        'ԝ' | 'ա' => 'w',
        'х' | 'χ' => 'x',
        'у' | 'γ' | 'ү' => 'y',
        'ᴢ' => 'z',
        other => other,
    }
}

/// Whether two [`collision_key`]s are close enough that one could be mistaken
/// for the other: equal, or one edit apart.
///
/// The edit-distance tier only applies from four characters up, so short
/// handles (`bob` / `rob`) do not all flag each other. Empty keys never match:
/// a name with no alphanumeric content is "no key", not "matches everything".
pub fn keys_are_confusable(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    if a.chars().count() < 4 || b.chars().count() < 4 {
        return false;
    }
    within_one_edit(a, b)
}

/// Levenshtein distance <= 1, without building a matrix.
fn within_one_edit(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (long, short) = if a.len() >= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if long.len() - short.len() > 1 {
        return false;
    }

    let mut i = 0;
    let mut j = 0;
    let mut edited = false;
    while i < long.len() && j < short.len() {
        if long[i] == short[j] {
            i += 1;
            j += 1;
            continue;
        }
        if edited {
            return false;
        }
        edited = true;
        if long.len() == short.len() {
            // Substitution: advance both.
            i += 1;
            j += 1;
        } else {
            // Deletion from the longer side only.
            i += 1;
        }
    }
    // Any unconsumed tail must be the single remaining edit.
    edited || (long.len() - i) + (short.len() - j) <= 1
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
        // 👮🏽‍♀️ = U+1F46E U+1F3FD ZWJ U+2640 U+FE0F. Every component is
        // hidden; the ZWJ is kept by `is_display_hidden` (it is orthography in
        // other scripts) and then dropped as an orphan, so nothing is left.
        let officer = "Bob \u{1F46E}\u{1F3FD}\u{200D}\u{2640}\u{FE0F}";
        assert_eq!(sanitize_display_name(officer), "Bob");
        assert!(!sanitize_display_name(officer).contains('\u{200D}'));

        // A joiner alone, or beside a space, is also an orphan.
        assert_eq!(sanitize_display_name("\u{200D}Alice"), "Alice");
        assert_eq!(sanitize_display_name("Alice\u{200D}"), "Alice");
        assert_eq!(sanitize_display_name("Alice \u{200D} Bob"), "Alice Bob");
    }

    /// U+200C ZWNJ and U+200D ZWJ look like emoji machinery but are letters in
    /// effect in Persian, Sinhala and Malayalam. Stripping them mangles real
    /// names — and, because `contains_hidden_chars` gates three inputs, it
    /// would have locked those users out of creating a room or accepting an
    /// invitation while telling them their name contained emoji.
    #[test]
    fn joiners_between_letters_are_preserved() {
        for name in [
            "علی\u{200C}رضا",   // Alireza (Persian compound given name)
            "حسین\u{200C}زاده", // Hosseinzadeh
            "නික\u{200D}නම",     // Sinhala touching letters
            "മോഹ\u{200D}ൻ",     // Malayalam chillu (consonant + virama + ZWJ)
        ] {
            assert_eq!(
                sanitize_display_name(name),
                name,
                "sanitisation broke a name whose joiner is orthography: {name:?}"
            );
            assert!(
                !contains_hidden_chars(name),
                "a name with an interior joiner must not be rejected at input: {name:?}"
            );
        }
    }

    /// Blank-but-not-whitespace characters let two members render a
    /// pixel-identical name, which defeats telling a moderator from an
    /// impersonator even with the badge correct.
    #[test]
    fn invisible_and_blank_characters_are_stripped() {
        for (label, blank) in [
            ("HANGUL FILLER", '\u{3164}'),
            ("HANGUL CHOSEONG FILLER", '\u{115F}'),
            ("HANGUL JUNGSEONG FILLER", '\u{1160}'),
            ("HALFWIDTH HANGUL FILLER", '\u{FFA0}'),
            ("BRAILLE PATTERN BLANK", '\u{2800}'),
            ("SOFT HYPHEN", '\u{00AD}'),
            ("ARABIC LETTER MARK", '\u{061C}'),
            ("MONGOLIAN VOWEL SEPARATOR", '\u{180E}'),
            ("INTERLINEAR ANNOTATION ANCHOR", '\u{FFF9}'),
        ] {
            let name = format!("Alice{blank}");
            assert_eq!(
                sanitize_display_name(&name),
                "Alice",
                "{label} survived and can clone another member's name"
            );
            assert!(contains_hidden_chars(&name), "{label} not flagged");
        }
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
            "山田\u{3000}太郎",        // U+3000, the Japanese name separator
            "佐々木",                  // U+3005 iteration mark
            "สมชาย ใจดี",               // Thai
            "Արամ Խաչատրյան",          // Armenian
            "გიორგი ბერიძე",           // Georgian
            "ኃይሌ ገብረሥላሴ",              // Ethiopic
            "ᏣᎳᎩ ᎠᏰᎵ",                 // Cherokee
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

    /// Spaces that render IDENTICALLY to U+0020 are normalised to it, so a
    /// nickname cannot clone another member's rendered name by swapping one
    /// in. U+3000 is not one of them: it is double-width, visibly different,
    /// and the conventional separator in a Japanese name.
    #[test]
    fn space_lookalikes_are_normalised_but_ideographic_space_is_not() {
        for lookalike in ['\u{00A0}', '\u{2002}', '\u{2007}', '\u{202F}', '\u{205F}'] {
            assert_eq!(
                sanitize_display_name(&format!("Jean{lookalike}Luc")),
                "Jean Luc",
                "U+{:04X} can clone a name that uses a plain space",
                u32::from(lookalike)
            );
        }
        assert_eq!(
            sanitize_display_name("山田\u{3000}太郎"),
            "山田\u{3000}太郎",
            "the ideographic space is part of the name, not a lookalike"
        );
    }

    /// A joiner between two ASCII letters is invisible, so `"Bo\u{200D}b"`
    /// renders exactly like `"Bob"` — a pixel-perfect clone of another
    /// member's name that no font distinguishes. Latin script never needs a
    /// joiner, so it is dropped there; a Malayalam chillu, which legitimately
    /// ENDS a name as consonant + virama + ZWJ, is kept.
    #[test]
    fn joiners_are_dropped_where_they_only_clone() {
        assert_eq!(sanitize_display_name("Bo\u{200D}b"), "Bob");
        assert_eq!(sanitize_display_name("Ali\u{200C}ce Smith"), "Alice Smith");
        // Non-ASCII on one side only is still Latin-adjacent: drop.
        assert_eq!(sanitize_display_name("Ali\u{200D}سce"), "Aliسce");
        // Malayalam legacy chillu at the end of a name: keep.
        let mohan = "മോഹന\u{0D4D}\u{200D}";
        assert_eq!(sanitize_display_name(mohan), mohan);
    }

    #[test]
    fn all_emoji_nickname_falls_back_to_placeholder() {
        assert_eq!(sanitize_display_name("🛡👑⭐"), UNNAMED);
        assert_eq!(sanitize_display_name(""), UNNAMED);
        assert_eq!(sanitize_display_name("   "), UNNAMED);
    }

    /// A nickname that renders BLANK but is not whitespace would otherwise
    /// skip the `UNNAMED` placeholder and produce a nameless message header,
    /// which reads as a continuation of the group above it — including a
    /// badged moderator's group.
    #[test]
    fn nicknames_that_render_blank_become_unnamed() {
        for blank in [
            "\u{1BCA0}",        // shorthand format letter overlap
            "\u{1D173}",        // musical symbol begin beam
            "\u{E0100}",        // variation selector-17
            "\u{180B}",         // Mongolian free variation selector one
            "\u{034F}",         // combining grapheme joiner
            "\u{2065}",         // unassigned Default_Ignorable
            "\u{3164}\u{2800}", // Hangul filler + Braille blank
            "\u{200D}",         // a lone joiner
        ] {
            assert_eq!(
                sanitize_display_name(blank),
                UNNAMED,
                "a nickname rendering blank must not pass as a name: {blank:?}"
            );
        }
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
    /// Three shapes are flagged. The third exists because the first two missed
    /// a live bug: `BanButton { nickname: …preferred_nickname.clone() }` handed
    /// a `SealedBytes` to a `String` prop, which the Dioxus props derive
    /// silently converts via `Display` (i.e. `to_string_lossy`) — no unseal, no
    /// sanitise, and neither literal pattern present. That coercion is a
    /// repeatable footgun in this codebase, so it gets its own check.
    ///
    /// If this fires on a genuinely non-display use of the field, route it
    /// through [`display_nickname`] anyway. Do NOT weaken the scan, and note
    /// that moving code into a test module does not help: the walk reads whole
    /// files on purpose (see the comment on `source` below).
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
            let source = std::fs::read_to_string(&path).expect("readable source file");
            let production = source.as_str();
            let rel = path
                .strip_prefix(&src)
                .unwrap_or(&path)
                .display()
                .to_string();

            if production.contains("preferred_nickname.to_string_lossy()") {
                offenders.push(format!("{rel}: preferred_nickname.to_string_lossy()"));
            }

            // A `SealedBytes` handed to a name-shaped component prop. Matches
            // `nickname:` / `name:` / `author:` (but not the struct field
            // `preferred_nickname:`, which is how a record is BUILT) followed
            // closely by the field.
            for prop in ["nickname:", "name:", "author:"] {
                let mut rest = production;
                let mut consumed = 0usize;
                while let Some(idx) = rest.find(prop) {
                    let abs = consumed + idx;
                    let preceded_by_ident = production[..abs]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_');
                    if !preceded_by_ident {
                        let window: String = production[abs..].chars().take(120).collect();
                        if window.contains("preferred_nickname") {
                            offenders.push(format!(
                                "{rel}: `{prop}` prop fed from preferred_nickname \
                                 (Dioxus converts a String prop via Display, so this \
                                 bypasses both the unseal and the sanitiser)"
                            ));
                        }
                    }
                    rest = &rest[idx + prop.len()..];
                    consumed = abs + prop.len();
                }
            }
            // An unseal whose argument is the nickname field. The window is
            // generous because rustfmt often breaks the call across lines.
            let mut rest = production;
            while let Some(idx) = rest.find("unseal_bytes_with_secrets(") {
                let after = &rest[idx..];
                // Take CHARS, not bytes: these files are full of multi-byte
                // characters (em-dashes and emoji in comments), and a byte
                // slice would panic on a char boundary and be blamed on
                // whichever unrelated change happened to move the text.
                let window: String = after.chars().take(160).collect();
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

    /// The real campaign this exists for: six accounts named "Ian Clarke" in
    /// the Official room, plus the near-miss variants an exact-match ban
    /// deliberately will not touch.
    #[test]
    fn impersonation_variants_fold_together() {
        let real = collision_key("Ian Clarke");
        assert_eq!(real, "ianciarke", "the fold is not what the cases assume");

        for variant in [
            "Ian Clarke",
            "IAN CLARKE",
            "ian clarke",
            "Ian_Clarke",
            "IanClarke",
            "Ian  Clarke",
            "  Ian Clarke  ",
            "lan Clarke",           // lowercase L for I
            "1an Clarke",           // digit one for I
            "Ian Clarkе",           // Cyrillic е
            "Іan Clarke",           // Cyrillic І
            "Ian Cl\u{0430}rke",    // Cyrillic а
            "Ian Clarke\u{200B}",   // zero-width space
            "Ian Clarke \u{1F6E1}", // the emoji-badge spoof
        ] {
            assert_eq!(
                collision_key(variant),
                real,
                "{variant:?} must fold onto the real name's key"
            );
        }

        // One edit away: caught by the warning tier, not by exact folding.
        for near in ["Ian Clark", "Ian Clarkes", "Ian Carke"] {
            let k = collision_key(near);
            assert_ne!(k, real, "{near:?} should not fold exactly");
            assert!(
                keys_are_confusable(&k, &real),
                "{near:?} must still be flagged as confusable"
            );
        }
    }

    /// Warning fatigue is the failure mode: if this fires on ordinary names
    /// nobody reads it. Unrelated members must not be confusable.
    #[test]
    fn unrelated_names_are_not_confusable() {
        let pairs = [
            ("Ian Clarke", "Nacho Duart"),
            ("Ian Clarke", "Hector Santos"),
            ("Alice", "Bob"),
            ("Cipher Daylight", "Neon Falcon"),
            ("李小龍", "王小明"),
            ("Иван Петров", "Ivan Sokolov"),
        ];
        for (a, b) in pairs {
            assert!(
                !keys_are_confusable(&collision_key(a), &collision_key(b)),
                "{a:?} and {b:?} must not be flagged as confusable"
            );
        }

        // Short handles differing by one character are NOT flagged, or every
        // three-letter name in the room would warn about every other.
        assert!(!keys_are_confusable(
            &collision_key("Bob"),
            &collision_key("Rob")
        ));
        // ...but from four characters up, one edit is enough.
        assert!(keys_are_confusable(
            &collision_key("Bobby"),
            &collision_key("Robby")
        ));
    }

    /// A key with no alphanumeric content is "no key". If it matched, every
    /// such member would be flagged as impersonating every privileged member.
    #[test]
    fn empty_keys_never_match() {
        assert_eq!(collision_key("---"), "");
        assert_eq!(collision_key("!?"), "");
        assert!(!keys_are_confusable("", ""));
        assert!(!keys_are_confusable("", "ianclarke"));
        assert!(!keys_are_confusable("ianclarke", ""));

        // A name that sanitises to the placeholder keys on the PLACEHOLDER,
        // not on nothing — several such members really do render the same
        // name, so they should be told apart by id like any other collision.
        assert_eq!(collision_key("🛡👑"), collision_key(UNNAMED));
        assert!(keys_are_confusable(
            &collision_key("🛡👑"),
            &collision_key(UNNAMED)
        ));
    }

    #[test]
    fn within_one_edit_boundaries() {
        assert!(within_one_edit("abcd", "abcd")); // 0 edits
        assert!(within_one_edit("abcd", "abcde")); // insertion at end
        assert!(within_one_edit("abcd", "xabcd")); // insertion at start
        assert!(within_one_edit("abcd", "abxd")); // substitution
        assert!(within_one_edit("abcde", "abde")); // deletion in the middle
        assert!(!within_one_edit("abcd", "abxy")); // 2 substitutions
        assert!(!within_one_edit("abcd", "abcdef")); // 2 insertions
        assert!(!within_one_edit("abcd", "dcba"));
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
