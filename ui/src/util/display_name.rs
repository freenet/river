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
//!   It cannot forge a badge, which is what this module is for. The one
//!   exception is general category **Me** (Enclosing_Mark), all thirteen of
//!   which are stripped: an Mn mark decorates the preceding character, but an
//!   Me mark draws a shape *around* it, so `A\u{20DD}` and `A\u{A670}` both
//!   render a circled A and rebuild by composition the enclosed-alphanumeric
//!   glyphs this module strips.
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
//!   The two characters kept in context — a joiner between non-ASCII letters,
//!   and an Ideographic Variation Selector after an ideograph — are the whole
//!   of that residual. `"李\u{E0100}小龍"` and `"李小龍"` render alike on a font
//!   with no IVD entry for the sequence, exactly as `"李\u{200D}小龍"` does. It
//!   is bounded: neither survives after an ASCII, Cyrillic or Hangul letter, so
//!   a Latin name cannot be cloned this way, and a font that DOES carry the IVD
//!   entry renders the two differently.
//!
//!   **This residual is mitigated, and the mitigation is load-bearing on the
//!   layering below.** `confusable.rs::skeleton` folds a name for
//!   impersonation detection by dropping every character [`is_display_hidden`]
//!   reports, and it consults that function DIRECTLY. Because the variation
//!   selectors stay inside `is_display_hidden`'s plane-14 range and their
//!   exception is applied on top, `skeleton` still folds `"李\u{E0100}小龍"` and
//!   `"李小龍"` to the same value, so the confusable warning fires on exactly
//!   this residual. Anyone tempted to "simplify" by moving the carve-out INTO
//!   [`is_display_hidden`] — punching a hole in the range rather than layering
//!   over it — would silently switch that warning off and make the residual
//!   undetectable. Do not.
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
//!
//!   The **Ideographic Variation Selectors** (`U+E0100..U+E01EF`) are the same
//!   kind of special case, from the other direction: they are
//!   Default_Ignorable by property, but they SELECT A GLYPH rather than
//!   rendering as nothing, and a Japanese family name routinely needs one
//!   (`辻󠄀` is `U+8FBB U+E0100`). Blanket-stripping them rewrote real names and
//!   locked those users out of saving a nickname at all, so they are judged in
//!   context too: kept directly after an ideograph, dropped everywhere else.
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

/// Shown for a member with no `member_info` record at all.
///
/// A constant rather than a literal at each fallback because it is a
/// CLASS-WIDE name: every member whose record has not synced yet renders as
/// this string simultaneously (see `.claude/rules/private-rooms.md` and the
/// `build_member_info_heal` path). Anything that treats a display name as
/// identifying — [`crate::components::members::impersonation_checker_for_viewer`]
/// above all — has to be able to name it, or it accuses a dozen members at once
/// during an ordinary sync gap.
pub const UNKNOWN_MEMBER: &str = "Unknown";

/// Shown next to a nickname `<input>` whose contents would not survive
/// [`sanitize_display_name`]. One wording, used by every nickname input, so a
/// user gets the same explanation wherever they hit it.
pub const EMOJI_REJECTION_MESSAGE: &str = "Nicknames can't contain emoji";

/// Unicode general category `Cf` (Format): every character whose whole job is
/// to be invisible and change how neighbouring text is laid out.
///
/// **Generated from Unicode 15.0 data, not hand-listed.** The list above grew
/// one codepoint at a time as each was noticed, and the result had the exact
/// signature of that process: U+06DE was in it but U+06DD was not; U+0488/U+0489
/// were covered but U+0890/U+0891 were not. Thirteen `Cf` codepoints survived
/// BOTH the sanitiser and the confusable fold — U+0600..U+0605, U+06DD, U+070F,
/// U+0890, U+0891, U+08E2, U+110BD, U+110CD and U+13430..U+1343F — and because
/// `is_display_hidden` also gates [`sanitize_display_name`], they survived into
/// the RENDERED nickname. `Ian\u{070F} Clarke` is not merely a fold miss; it is
/// a pixel-identical clone of another member's displayed name.
///
/// Regenerate by sweeping `0..0x110000` for `unicodedata.category(chr(cp)) ==
/// 'Cf'` and collapsing to ranges; 170 codepoints in 21 ranges as of Unicode
/// 15.0. `every_format_character_is_hidden` pins a representative of each range.
///
/// ## The other invisible categories
///
/// * `Cs` (surrogates) cannot exist in a Rust `char`, so there is nothing to do.
/// * `Co` (private use) is covered by the PUA ranges in [`is_display_hidden`].
/// * `Cn` (unassigned) is deliberately NOT covered. An unassigned codepoint
///   renders as a visible replacement box, not as nothing, so it does not clone
///   anyone's name — and the set SHRINKS with every Unicode release, so a frozen
///   `Cn` table would start stripping newly-assigned letters out of real names.
///   That is the one direction of error this module must not have.
///
/// ## The two exceptions, which are the same ones as everywhere else
///
/// U+200C ZWNJ and U+200D ZWJ are `Cf` and are deliberately NOT reported. They
/// are orthography in Persian, Sinhala and Malayalam, and stripping them at
/// RENDER time mangles real names — the reasoning is on the `0x200B` entry
/// below. `crate::util::confusable::skeleton` drops them anyway, because
/// comparison is not rendering.
fn is_format_control(c: char) -> bool {
    if c == '\u{200C}' || c == '\u{200D}' {
        return false;
    }
    matches!(u32::from(c),
        0x00AD                  // SOFT HYPHEN
        | 0x0600..=0x0605       // ARABIC NUMBER SIGN..ARABIC NUMBER MARK ABOVE
        | 0x061C                // ARABIC LETTER MARK
        | 0x06DD                // ARABIC END OF AYAH
        | 0x070F                // SYRIAC ABBREVIATION MARK
        | 0x0890..=0x0891       // ARABIC POUND/PIASTRE MARK ABOVE
        | 0x08E2                // ARABIC DISPUTED END OF AYAH
        | 0x180E                // MONGOLIAN VOWEL SEPARATOR
        | 0x200B..=0x200F       // ZWSP, ZWNJ, ZWJ, LRM, RLM (joiners excepted above)
        | 0x202A..=0x202E       // bidi embedding / override
        | 0x2060..=0x2064       // word joiner, invisible operators
        | 0x2066..=0x206F       // bidi isolates, deprecated formatting
        | 0xFEFF                // ZERO WIDTH NO-BREAK SPACE / BOM
        | 0xFFF9..=0xFFFB       // interlinear annotation anchors
        | 0x110BD | 0x110CD     // KAITHI NUMBER SIGN, ...ABOVE
        | 0x13430..=0x1343F     // Egyptian hieroglyph format controls
        | 0x1BCA0..=0x1BCA3     // shorthand format controls
        | 0x1D173..=0x1D17A     // musical beam/slur/phrase controls
        | 0xE0001               // LANGUAGE TAG
        | 0xE0020..=0xE007F     // TAG SPACE..CANCEL TAG
    )
}

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
    // Every Unicode FORMAT character, by category rather than by whichever ones
    // someone happened to hit. See [`is_format_control`].
    if is_format_control(c) {
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
        // Combining Diacritical Marks for Symbols. Includes the keycap
        // assembler (`1️⃣`) but also the ENCLOSING marks — U+20DD circle,
        // U+20DE square, U+20E0 circle-backslash, U+20E4 triangle — which
        // rebuild by composition the very glyphs `0x2460..=0x24FF` is stripped
        // for: `A\u{20DD}` is Ⓐ, `!\u{20E4}` reads as ⚠. The block has no
        // letter content.
        | 0x20D0..=0x20F0
        // The rest of Unicode general category Me (Enclosing_Mark). Me draws a
        // shape AROUND the preceding character, so every one of them composes
        // a badge the same way U+20DD does: `A\u{A670}` renders a circled A,
        // which is byte-for-byte the `A\u{20DD}` attack above. Me has thirteen
        // members; the block above covers seven and these are the other six.
        // U+0488/U+0489 are the Cyrillic hundred-thousands and millions signs,
        // U+A670..U+A672 the ten-millions family, U+1ABE the parentheses
        // overlay. Stripping a whole general category is the exception to the
        // "combining marks are preserved" rule in the module header: Mn marks
        // decorate a letter, Me marks enclose it, and only the latter can draw
        // a badge.
        | 0x0488..=0x0489 | 0x1ABE | 0xA670..=0xA672
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
        | 0x00AD | 0x061C | 0x180E | 0xFFF0..=0xFFFB
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
        | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        // Text-presentation symbols that read as a badge in the fonts that
        // carry them: ۞ (ornate star, present wherever Arabic renders), ٭,
        // ꙳, and the Phaistos shield. Plus Symbols for Legacy Computing and
        // its supplement, which contain an inverse check mark and stick
        // figures.
        | 0x066D | 0x06DE | 0xA673
        // Aegean/Phaistos: picking out only the shield (U+101DB) left the
        // helmet, tiara, rosette and — the one that matters — U+10102 AEGEAN
        // CHECK MARK, which renders as ✓ wherever Noto Sans Symbols is
        // installed (stock Ubuntu/Fedora).
        | 0x10100..=0x101FC
        // Halfwidth clones of the geometric shapes stripped above.
        | 0xFFED..=0xFFEE
        | 0x1FB00..=0x1FBFF | 0x1CC00..=0x1CEBF
        // Private Use Area (BMP). Font-defined glyphs, and River's own
        // mention sentinels live at U+E000/U+E001.
        | 0xE000..=0xF8FF
        // The emoji planes: Mahjong/Domino/Cards, Enclosed Alphanumeric
        // Supplement (regional-indicator flags), Miscellaneous Symbols and
        // Pictographs, Emoticons, Transport, Supplemental Symbols and
        // Pictographs, Symbols and Pictographs Extended-A. 🛡 is U+1F6E1.
        | 0x1F000..=0x1FAFF
        // Plane 14's Default_Ignorable range. `E0000..E0FFF` is the whole set
        // Unicode marks ignorable in that plane, and the rest of plane 14
        // (`E1000..EFFFF`) is not ignorable and is left alone. Naming only the
        // tag block (`E0000..E007F`, the flag-sequence assembler 🏴󠁧󠁢󠁳󠁣󠁴󠁿) left
        // ~3,700 invisible characters through, each of which clones another
        // member's rendered name.
        //
        // The Ideographic Variation Selectors (`E0100..E01EF`) are INSIDE this
        // range on purpose, even though they are the one part of it that is
        // legitimate in a name. Their exception is applied ON TOP, in context,
        // by [`sanitize_display_name`] and [`contains_hidden_chars`] — NOT by
        // punching a hole here. TWO separate things depend on that.
        //
        // First, `confusable.rs::skeleton` folds names for impersonation
        // detection by calling THIS function directly. Keeping the selectors
        // inside the range is what lets it fold `"李\u{E0100}小龍"` and
        // `"李小龍"` together, so the confusable warning covers the residual
        // the in-context exception deliberately leaves open (module header).
        // Punching a hole here would switch that warning off silently.
        //
        // Second, two complementary hand-written ranges drift:
        // narrowing the carve-out by one codepoint leaves a character that is
        // neither stripped nor judged, so it survives verbatim in every name
        // while rendering as nothing. Layering makes that gap impossible, and
        // `no_plane_14_codepoint_escapes_both_the_strip_and_the_carve_out`
        // fails if anyone splits it again.
        | 0xE0000..=0xE0FFF
        // Supplementary Private Use Areas A and B.
        | 0xF0000..=0xFFFFD
        | 0x100000..=0x10FFFD
    )
}

/// Whether `c` is an Ideographic Variation Selector (VS17..VS256).
///
/// These sit inside plane 14's Default_Ignorable range but are the one part of
/// it that is NOT invisible: they SELECT A GLYPH. Japanese family names are
/// routinely spelled with one — `辻` has the variant `辻󠄀` (U+8FBB U+E0100), and
/// `邊`/`邉`, `﨑`, `髙` work the same way — and any font with an IVS table
/// (Source Han, Noto CJK) renders the selected form. Stripping them blanket-
/// wise rewrote real names, and because [`contains_hidden_chars`] gates the
/// nickname `<input>`, it also told those users their own name "can't contain
/// emoji" and refused to save it.
///
/// So they are judged in context instead, exactly like `U+200C`/`U+200D`: kept
/// where they can be doing the work they exist for, dropped everywhere else.
fn is_variation_selector_supplement(c: char) -> bool {
    matches!(u32::from(c), 0xE0100..=0xE01EF)
}

/// The ideographs a variation selector may legitimately follow. These are the
/// blocks the Ideographic Variation Database actually registers sequences for;
/// after anything else a selector is invisible filler.
fn is_ideograph(c: char) -> bool {
    matches!(u32::from(c),
        // CJK Unified Ideographs Extension A, and the main block.
        0x3400..=0x4DBF | 0x4E00..=0x9FFF
        // CJK Compatibility Ideographs (where `﨑` lives).
        | 0xF900..=0xFAFF
        // Extensions B onwards, plus the compatibility supplement.
        | 0x20000..=0x323AF
    )
}

/// Whether the variation selector at `chars[i]` can be selecting a glyph, i.e.
/// it directly follows an ideograph.
///
/// Anywhere else it renders as nothing and clones the surrounding name, so it
/// is dropped for the same reason an orphaned joiner is. `"Ian\u{E0100}"` is a
/// clone of `"Ian"`; `"辻\u{E0100}"` is a person's name.
fn selects_an_ideograph_variant(chars: &[char], i: usize) -> bool {
    i > 0 && chars.get(i - 1).copied().is_some_and(is_ideograph)
}

/// Whether `s` contains anything [`sanitize_display_name`] would remove.
///
/// Drives the nickname `<input>`'s "Nicknames can't contain emoji" message —
/// UX only. Never rely on this for safety: the render-time strip is the
/// boundary, because `riverctl` never runs this code.
///
/// Position-sensitive for the same characters [`sanitize_display_name`] judges
/// in context, so it cannot reject a name the sanitiser would have kept: a
/// variation selector after an ideograph is a legitimate Japanese name and is
/// NOT flagged, while the same selector after `n` is invisible filler and is.
pub fn contains_hidden_chars(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    chars.iter().enumerate().any(|(i, c)| {
        if is_variation_selector_supplement(*c) {
            return !selects_an_ideograph_variant(&chars, i);
        }
        is_display_hidden(*c)
    })
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
    //
    //    A variation selector is judged here rather than by the blanket strip,
    //    because whether it is a Japanese name or invisible filler depends on
    //    what precedes it. Same decision, same inputs as
    //    [`contains_hidden_chars`], so the input check and the render strip
    //    cannot disagree about a given name.
    let raw_chars: Vec<char> = raw.chars().collect();
    let stripped: Vec<char> = raw_chars
        .iter()
        .enumerate()
        .filter_map(|(i, &c)| {
            if is_variation_selector_supplement(c) {
                return selects_an_ideograph_variant(&raw_chars, i).then_some(c);
            }
            match (is_display_hidden(c), c.is_whitespace()) {
                (true, true) => Some(' '),
                (true, false) => None,
                (false, _) => Some(c),
            }
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
                    // A joiner is legitimate at the end of a WORD, not just at
                    // the end of the string: Malayalam legacy chillu is
                    // consonant + virama + ZWJ, so `"മോഹന\u{0D4D}\u{200D} കുമാർ"`
                    // (Mohan Kumar) carries one mid-name, and word-final ZWNJ
                    // does the same in Persian and Kurdish. Requiring
                    // end-of-string dropped it and degraded the chillu `ൻ` to
                    // `ന്`, showing a chandrakkala that is not part of the name.
                    // So: no next character, a non-ASCII letter, or whitespace.
                    //
                    // This does not widen the Latin clone surface — the
                    // PRECEDING character must still be a non-ASCII letter, so
                    // `"Bo\u{200D}b"` stays closed. It does add word-final
                    // positions to the non-ASCII residual already documented in
                    // the module header (an interior joiner in a CJK name), so
                    // no new capability, just more places for the same one.
                    && stripped
                        .get(i + 1)
                        .is_none_or(|n| joins_letters(Some(n)) || n.is_whitespace()))
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

    /// Every Unicode `Cf` FORMAT character must be hidden, because
    /// `is_display_hidden` gates the sanitiser: one that survives is not merely
    /// a fold miss, it renders as nothing inside the nickname and clones
    /// another member's displayed name exactly.
    ///
    /// One representative from each of the 21 ranges (the whole range where it
    /// is short), so a range dropped from [`is_format_control`] fails here.
    /// The specific codepoints called out below are the ones that survived
    /// BOTH this sanitiser and the confusable fold before the list was
    /// generated rather than hand-extended.
    #[test]
    fn every_format_character_is_hidden() {
        for cp in [
            0x00ADu32, 0x0600, 0x0601, 0x0602, 0x0603, 0x0604, 0x0605, 0x061C, 0x06DD, 0x070F,
            0x0890, 0x0891, 0x08E2, 0x180E, 0x200B, 0x200E, 0x200F, 0x202A, 0x202E, 0x2060, 0x2064,
            0x2066, 0x206F, 0xFEFF, 0xFFF9, 0xFFFB, 0x110BD, 0x110CD, 0x13430, 0x1343F, 0x1BCA0,
            0x1BCA3, 0x1D173, 0x1D17A, 0xE0001, 0xE0020, 0xE007F,
        ] {
            let c = char::from_u32(cp).expect("a valid codepoint");
            assert!(
                is_display_hidden(c),
                "U+{cp:04X} is a Cf format character and renders as nothing, so \
                 it must never survive into a displayed nickname"
            );
        }

        // The two deliberate exceptions survive, exactly as before: they are
        // orthography in Persian, Sinhala and Malayalam.
        assert!(!is_display_hidden('\u{200C}'));
        assert!(!is_display_hidden('\u{200D}'));

        // The end-to-end property: a name carrying one of these must not
        // render identically to the plain one. Both of these used to.
        for hidden in ['\u{070F}', '\u{13437}', '\u{06DD}', '\u{0890}', '\u{110BD}'] {
            let spoofed = format!("Ian{hidden} Clarke");
            assert!(
                contains_hidden_chars(&spoofed),
                "{spoofed:?} must be reported as containing hidden characters"
            );
            assert_eq!(
                sanitize_display_name(&spoofed),
                "Ian Clarke",
                "{spoofed:?} rendered as a pixel-identical clone of `Ian Clarke`"
            );
        }
    }

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

    /// Every Default_Ignorable character renders as nothing, so any one of
    /// them clones another member's rendered name — the exact capability an
    /// impersonation campaign wants. The first pass named only the ranges it
    /// happened to think of and left ~3,700 through, including all of plane 14
    /// and `U+FFF0..U+FFF8`. These are NOT riverctl-only: `contains_hidden_chars`
    /// gates the nickname input, so a miss means the UI accepts them too.
    #[test]
    fn default_ignorable_characters_cannot_clone_a_name() {
        for c in [
            '\u{FFF0}',
            '\u{FFF4}',
            '\u{FFF8}', // the FFFx reserved-DI gap
            '\u{E0001}',
            '\u{E0080}',
            '\u{E00FF}', // plane 14 outside the tag block
            '\u{E01F0}',
            '\u{E0FFF}', // plane 14 above the variation selectors
            '\u{E0100}', // variation selectors supplement
        ] {
            let cloned = format!("Ian{c} Clarke");
            assert_eq!(
                sanitize_display_name(&cloned),
                "Ian Clarke",
                "U+{:04X} survived and clones another member's name",
                u32::from(c)
            );
            assert!(
                contains_hidden_chars(&cloned),
                "U+{:04X} is not rejected at the nickname input",
                u32::from(c)
            );
            // Alone, it must not pass as a name at all.
            assert_eq!(sanitize_display_name(&c.to_string()), UNNAMED);
        }
    }

    /// Enclosing combining marks rebuild by composition the badge-shaped
    /// glyphs the enclosed-alphanumeric block is stripped for, and the Aegean
    /// check mark renders as ✓ on a stock Linux desktop.
    #[test]
    fn composed_and_stray_badge_glyphs_are_stripped() {
        for (label, name, want) in [
            ("enclosing circle (Ⓐ)", "Mod A\u{20DD}", "Mod A"),
            ("enclosing square", "Mod A\u{20DE}", "Mod A"),
            ("enclosing triangle (⚠-ish)", "Mod !\u{20E4}", "Mod !"),
            ("Aegean check mark", "Mod \u{10102}", "Mod"),
            ("halfwidth black square", "Mod \u{FFED}", "Mod"),
        ] {
            let out = sanitize_display_name(name);
            assert_eq!(out, want, "{label}: got {out:?}");
        }
    }

    /// Both ends of every range widened here, plus the two codepoints the
    /// widening comments name by hand (`U+20E0`, `U+20E3`).
    ///
    /// The tests above sample INTERIOR points, which catches a deleted range
    /// but not a range narrowed by one, and off-by-one is the likelier edit.
    /// Every entry below is a codepoint whose loss reopens a real vector: a
    /// combining mark that rebuilds a badge by composition, an interlinear
    /// annotation anchor that hides the text between two of them, or an
    /// invisible plane-14 codepoint that clones another member's name.
    #[test]
    fn widened_range_endpoints_are_hidden() {
        for (label, c) in [
            // Combining Diacritical Marks for Symbols, 0x20D0..=0x20F0.
            ("U+20D0 combining left harpoon above", '\u{20D0}'),
            ("U+20E0 combining enclosing circle backslash", '\u{20E0}'),
            ("U+20E3 combining enclosing keycap", '\u{20E3}'),
            ("U+20F0 combining asterisk above", '\u{20F0}'),
            // Halfwidth geometric shapes, 0xFFED..=0xFFEE.
            ("U+FFED halfwidth black square", '\u{FFED}'),
            ("U+FFEE halfwidth white circle", '\u{FFEE}'),
            // Reserved Default_Ignorables + interlinear annotation,
            // 0xFFF0..=0xFFFB.
            ("U+FFF0 reserved Default_Ignorable", '\u{FFF0}'),
            ("U+FFFB interlinear annotation terminator", '\u{FFFB}'),
            // Aegean Numbers + Phaistos Disc, 0x10100..=0x101FC.
            ("U+10100 aegean word separator line", '\u{10100}'),
            ("U+101FC phaistos disc sign wavy band", '\u{101FC}'),
            // Plane 14's Default_Ignorable range, 0xE0000..=0xE0FFF, plus
            // 0xE01F0..=0xE0FFF, the two codepoints flanking the
            // variation-selector carve-out layered on top of it.
            ("U+E0000 reserved tag codepoint", '\u{E0000}'),
            (
                "U+E00FF reserved, just below the IVS carve-out",
                '\u{E00FF}',
            ),
            (
                "U+E01F0 reserved, just above the IVS carve-out",
                '\u{E01F0}',
            ),
            ("U+E0FFF reserved Default_Ignorable", '\u{E0FFF}'),
        ] {
            assert!(
                is_display_hidden(c),
                "{label} is not stripped, so it can forge a badge or clone a name"
            );
            let cloned = format!("Ian{c} Clarke");
            assert_eq!(
                sanitize_display_name(&cloned),
                "Ian Clarke",
                "{label} survived sanitisation"
            );
            assert!(
                contains_hidden_chars(&cloned),
                "{label} is not rejected at the nickname input"
            );
        }
    }

    /// The upper constraint on every range widened here.
    ///
    /// Endpoint and interior assertions only pin a range from BELOW: they all
    /// still pass if a future edit widens it further, which is how a strip rule
    /// starts mangling real names. Widening `0xFFF0..=0xFFFB` to swallow
    /// U+FFFD REPLACEMENT CHARACTER, or plane 14 to `0xEFFFF`, passes every
    /// other test in this module.
    #[test]
    fn characters_just_outside_the_widened_ranges_stay_visible() {
        for (label, c) in [
            // Either side of 0x20D0..=0x20F0. Reserved codepoints render as a
            // tofu box, so they are visible and cannot clone a name.
            (
                "U+20CF reserved, below the symbol combining marks",
                '\u{20CF}',
            ),
            (
                "U+20F1 reserved, above the symbol combining marks",
                '\u{20F1}',
            ),
            // Below 0xFFED..=0xFFEE.
            ("U+FFEC halfwidth downwards arrow", '\u{FFEC}'),
            // The one-codepoint gap between 0xFFED..=0xFFEE and 0xFFF0..=0xFFFB,
            // so this pins the top of one range and the bottom of the other.
            ("U+FFEF reserved", '\u{FFEF}'),
            // Above 0xFFF0..=0xFFFB. U+FFFD is what `String::from_utf8_lossy`
            // emits, and `display_nickname`'s undecryptable fallback goes
            // through it, so stripping it would blank a whole nickname.
            ("U+FFFC object replacement character", '\u{FFFC}'),
            ("U+FFFD replacement character", '\u{FFFD}'),
            // Either side of 0x10100..=0x101FC.
            ("U+100FF reserved, below Aegean Numbers", '\u{100FF}'),
            (
                "U+101FD phaistos disc combining oblique stroke",
                '\u{101FD}',
            ),
            // Either side of 0xE0000..=0xE0FFF. Unicode's Default_Ignorable set
            // for plane 14 ends at E0FFF; E1000 and up are ordinary reserved
            // codepoints, and plane 13 below it is unassigned throughout.
            ("U+DFFFF reserved, below plane 14", '\u{DFFFF}'),
            ("U+E1000 reserved, above plane 14's ignorables", '\u{E1000}'),
            ("U+EFFFF reserved, top of plane 14", '\u{EFFFF}'),
        ] {
            assert!(
                !is_display_hidden(c),
                "{label} is treated as hidden, so a widened range is now \
                 stripping characters that are not invisible"
            );
            let name = format!("Ian{c} Clarke");
            assert_eq!(
                sanitize_display_name(&name),
                name,
                "{label} was stripped out of a name"
            );
        }
    }

    /// Unicode general category Me (Enclosing_Mark) in FULL.
    ///
    /// Every Me character draws a shape around the character before it, so
    /// each one composes a badge exactly the way `U+20DD` does. Adding the
    /// `0x20D0..=0x20F0` block caught seven of the thirteen and left six —
    /// including `U+A670`, which sits immediately before the `U+A673` this
    /// module already stripped, and which renders `"Mod A\u{A670}"` as a
    /// circled A: byte-for-byte the attack the block was added to stop.
    #[test]
    fn every_enclosing_mark_is_stripped() {
        // The complete Me category, verified against the Unicode 15.0
        // character database rather than assembled from memory: these thirteen
        // are every codepoint with `category == "Me"`. If a later revision adds
        // a fourteenth, this list is the thing to update.
        const ENCLOSING_MARKS: &[(char, &str)] = &[
            ('\u{0488}', "COMBINING CYRILLIC HUNDRED THOUSANDS SIGN"),
            ('\u{0489}', "COMBINING CYRILLIC MILLIONS SIGN"),
            ('\u{1ABE}', "COMBINING PARENTHESES OVERLAY"),
            ('\u{20DD}', "COMBINING ENCLOSING CIRCLE"),
            ('\u{20DE}', "COMBINING ENCLOSING SQUARE"),
            ('\u{20DF}', "COMBINING ENCLOSING DIAMOND"),
            ('\u{20E0}', "COMBINING ENCLOSING CIRCLE BACKSLASH"),
            ('\u{20E2}', "COMBINING ENCLOSING SCREEN"),
            ('\u{20E3}', "COMBINING ENCLOSING KEYCAP"),
            ('\u{20E4}', "COMBINING ENCLOSING UPWARD POINTING TRIANGLE"),
            ('\u{A670}', "COMBINING CYRILLIC TEN MILLIONS SIGN"),
            ('\u{A671}', "COMBINING CYRILLIC HUNDRED MILLIONS SIGN"),
            ('\u{A672}', "COMBINING CYRILLIC THOUSAND MILLIONS SIGN"),
        ];
        for (mark, name) in ENCLOSING_MARKS {
            assert!(
                is_display_hidden(*mark),
                "U+{:04X} {name} is not stripped, so `A{mark}` composes a badge",
                u32::from(*mark)
            );
            // The concrete attack: a circled/enclosed capital, rebuilt from a
            // letter the strip cannot remove plus a mark it must.
            assert_eq!(
                sanitize_display_name(&format!("Mod A{mark}")),
                "Mod A",
                "U+{:04X} {name} survived and encloses the letter before it",
                u32::from(*mark)
            );
            assert!(
                contains_hidden_chars(&format!("Mod A{mark}")),
                "U+{:04X} {name} is not rejected at the nickname input",
                u32::from(*mark)
            );
        }
    }

    /// Malayalam legacy chillu is `consonant + virama + ZWJ` and ends a WORD,
    /// not only a string. Permitting the trailing joiner solely at
    /// end-of-string kept the single-token name working (which is all the
    /// original test covered) while silently breaking the ordinary two-word
    /// form: the ZWJ was dropped, degrading the chillu `ൻ` to `ന്` and showing
    /// a chandrakkala that is not part of the name.
    #[test]
    fn word_final_joiners_survive_in_multi_word_names() {
        for (label, name) in [
            (
                "Malayalam chillu before a space",
                "മോഹന\u{0D4D}\u{200D} കുമാർ",
            ),
            ("Malayalam chillu at end of string", "മോഹന\u{0D4D}\u{200D}"),
            ("Persian word-final ZWNJ", "علی\u{200C} رضا"),
            (
                "two chillus, two words",
                "മോഹന\u{0D4D}\u{200D} കുമാരന\u{0D4D}\u{200D}",
            ),
        ] {
            assert_eq!(
                sanitize_display_name(name),
                name,
                "{label}: sanitisation dropped a joiner that is part of the name"
            );
            assert!(
                !contains_hidden_chars(name),
                "{label}: the nickname input rejected a legitimate name"
            );
        }

        // The Latin clone surface must stay closed: the PRECEDING character is
        // still required to be a non-ASCII letter, so a joiner next to ASCII
        // is dropped no matter what follows it.
        assert_eq!(sanitize_display_name("Bo\u{200D}b"), "Bob");
        assert_eq!(sanitize_display_name("Bob\u{200D} Smith"), "Bob Smith");
        assert_eq!(sanitize_display_name("Alice \u{200D} Bob"), "Alice Bob");
    }

    /// The Ideographic Variation Selectors are Default_Ignorable by property
    /// but DO select a glyph, and Japanese family names need them. Blanket-
    /// stripping them rewrote real names, and because `contains_hidden_chars`
    /// gates the nickname `<input>`, it also told those users their own name
    /// "can't contain emoji" and refused to save it.
    #[test]
    fn ideographic_variation_sequences_are_preserved() {
        for (label, name) in [
            ("辻 with VS17 (the canonical IVS example)", "辻\u{E0100}"),
            ("邊 variant", "邊\u{E0101}"),
            ("﨑 (compatibility ideograph) variant", "﨑\u{E0100}"),
            ("髙 variant in a full name", "髙\u{E0100}橋 太郎"),
            ("VS256, the top of the range", "辻\u{E01EF}"),
        ] {
            assert_eq!(
                sanitize_display_name(name),
                name,
                "{label}: sanitisation rewrote an ideographic variation sequence"
            );
            assert!(
                !contains_hidden_chars(name),
                "{label}: the nickname input refused a legitimate Japanese name"
            );
        }
    }

    /// No codepoint in plane 14's Default_Ignorable range may fall through BOTH
    /// the blanket strip and the context-judged variation-selector carve-out.
    ///
    /// The two used to be complementary hand-written ranges
    /// (`E0000..E00FF | E01F0..E0FFF` in [`is_display_hidden`], `E0100..E01EF`
    /// in [`is_variation_selector_supplement`]). Mutation testing showed that
    /// narrowing the carve-out by ONE codepoint left `U+E01EF` in neither: not
    /// stripped, not judged, kept verbatim in every name while rendering as
    /// nothing — a free clone vector, and every other test still passed. The
    /// ranges are now layered rather than complementary, so the gap is
    /// structurally impossible; this sweep is what fails if anyone splits them
    /// again.
    #[test]
    fn no_plane_14_codepoint_escapes_both_the_strip_and_the_carve_out() {
        for cp in 0xE0000u32..=0xE0FFF {
            let c = char::from_u32(cp).expect("all of plane 14 is valid scalars");
            assert!(
                is_display_hidden(c) || is_variation_selector_supplement(c),
                "U+{cp:04X} is neither stripped nor context-judged, so it \
                 survives verbatim while rendering as nothing"
            );
            // The decision that matters: after a non-ideograph, every one of
            // them must go, whichever of the two paths handles it.
            let cloned = format!("Ian{c} Clarke");
            assert_eq!(
                sanitize_display_name(&cloned),
                "Ian Clarke",
                "U+{cp:04X} survived after an ASCII letter and clones a name"
            );
            assert!(
                contains_hidden_chars(&cloned),
                "U+{cp:04X} is not rejected at the nickname input"
            );
        }
    }

    /// The other half of the IVS carve-out. A selector that is NOT after an
    /// ideograph selects nothing, renders as nothing, and clones the name it
    /// sits in — so it must still be dropped AND still be rejected at the
    /// input. Without this the carve-out would just be a hole.
    #[test]
    fn variation_selectors_not_after_an_ideograph_are_still_dropped() {
        for (label, raw, want) in [
            ("after an ASCII letter", "Ian\u{E0100} Clarke", "Ian Clarke"),
            ("after a space", "Ian \u{E0100}Clarke", "Ian Clarke"),
            ("after Cyrillic (not an ideograph)", "Иван\u{E0100}", "Иван"),
            (
                "after Hangul (not an ideograph)",
                "김민준\u{E0100}",
                "김민준",
            ),
            ("at the very start", "\u{E0100}Ian", "Ian"),
            (
                "doubled after an ideograph",
                "辻\u{E0100}\u{E0100}",
                "辻\u{E0100}",
            ),
        ] {
            assert_eq!(
                sanitize_display_name(raw),
                want,
                "{label}: a non-selecting variation selector survived and clones a name"
            );
            assert!(
                contains_hidden_chars(raw),
                "{label}: a non-selecting variation selector is not rejected at the input"
            );
        }
        // Alone it is not a name at all.
        assert_eq!(sanitize_display_name("\u{E0100}"), UNNAMED);
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
