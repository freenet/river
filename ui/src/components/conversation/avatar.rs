//! Initials avatars for message authors.
//!
//! The disc's colour is per viewer: [`crate::avatar_salt::avatar_hue`] under
//! the room's salt from [`crate::avatar_salt::salt_for_render`] (stored, or
//! computed from this viewer's read seed). A room with no salt gets a neutral
//! grey disc; there is deliberately no other source of colour (see
//! `avatar_salt.rs`).

use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomStateV1;

/// The disc's text: the initial of each of the first words of the display
/// nickname that have one, at most two glyphs, upper-cased; for a name with
/// no letter or digit, its first emoji; else "?".
///
/// Built on UAX #29 extended grapheme clusters, because a nickname is
/// untrusted text: it must not be able to put arbitrary text in the disc.
/// - A word's initial is its first cluster holding a letter or digit, so the
///   `(Owner)` in `Kara (Owner)` gives `O`. Leading format and bidi controls
///   and lone combining marks are skipped.
/// - The initial is that letter (upper-cased; `ß` gives `SS`, two glyphs)
///   and at most two more characters of its own cluster (combining marks, an
///   Indic vowel sign or conjunct), never a format character: a ZWJ or a
///   pile of marks cannot drag more text in.
/// - At most two glyphs in all: `ßa ßb` gives `SS`.
pub(crate) fn initials(name: &str) -> String {
    let mut glyphs: Vec<String> = Vec::new();
    for word in name.split_whitespace() {
        if glyphs.len() >= 2 {
            break;
        }
        glyphs.extend(word_initial(word));
    }
    glyphs.truncate(2);
    if !glyphs.is_empty() {
        return glyphs.concat();
    }
    pictograph(name).unwrap_or_else(|| "?".to_string())
}

/// Characters an initial keeps after its letter, from the letter's cluster.
const MAX_MARKS: usize = 2;
/// The longest emoji cluster shown (a ZWJ family is seven characters).
const MAX_PICTOGRAPH_CHARS: usize = 8;

/// The word's initial as the glyphs it draws (see [`initials`]): one per
/// letter of the upper-cased base, the marks on the last. Empty when the
/// word holds no letter or digit.
fn word_initial(word: &str) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;
    for cluster in word.graphemes(true) {
        let mut chars = cluster.chars();
        // A cluster's base is its first letter or digit that is not itself
        // a mark (a lone mark forms a cluster of its own).
        let Some(base) = chars
            .by_ref()
            .find(|c| c.is_alphanumeric() && !is_format(*c) && !extends_cluster(*c))
        else {
            continue;
        };
        let mut glyphs: Vec<String> = base.to_uppercase().map(String::from).collect();
        let last = glyphs.last_mut().expect("upper case is never empty");
        last.extend(chars.filter(|c| !is_format(*c)).take(MAX_MARKS));
        return glyphs;
    }
    Vec::new()
}

/// A name with no letter or digit: its first emoji cluster (a flag, a ZWJ
/// sequence, a skin tone), if it starts with one.
fn pictograph(name: &str) -> Option<String> {
    use unicode_segmentation::UnicodeSegmentation;
    let cluster = name.graphemes(true).find(|g| {
        g.chars()
            .next()
            .is_some_and(|c| !c.is_whitespace() && !is_format(c) && !extends_cluster(c))
    })?;
    let first = cluster.chars().next()?;
    is_pictographic(first).then(|| cluster.chars().take(MAX_PICTOGRAPH_CHARS).collect())
}

/// Whether `c` attaches to the character before it in a grapheme cluster
/// (UAX #29 Extend, ZWJ or SpacingMark: combining marks, Indic vowel signs,
/// viramas, variation selectors, skin tones), asked of the segmenter itself
/// so no property table is duplicated here.
fn extends_cluster(c: char) -> bool {
    use unicode_segmentation::UnicodeSegmentation;
    let mut probe = String::from("a");
    probe.push(c);
    probe.graphemes(true).count() == 1
}

/// Invisible characters a disc never shows: zero-width spaces and joiners,
/// bidi marks, embeddings, overrides and isolates, the word joiner family,
/// variation selectors, the BOM and the soft hyphen; and the blanks that
/// Unicode files as letters or symbols (the Hangul fillers, the blank
/// Braille pattern), which would draw an empty disc.
fn is_format(c: char) -> bool {
    matches!(c, '\u{115F}' | '\u{1160}' | '\u{2800}' | '\u{3164}' | '\u{FFA0}' | '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}' | '\u{E0100}'..='\u{E01EF}')
}

/// Emoji: the emoji planes (flags' regional indicators included), the
/// miscellaneous symbols and dingbats, and the few emoji elsewhere in the
/// BMP. Not arrows, box drawing, Braille or other symbols, which read as
/// noise in a disc.
fn is_pictographic(c: char) -> bool {
    matches!(c, '\u{1F000}'..='\u{1FAFF}' | '\u{2600}'..='\u{27BF}' | '\u{231A}' | '\u{231B}' | '\u{23E9}'..='\u{23F3}' | '\u{23F8}'..='\u{23FA}' | '\u{2B1B}' | '\u{2B1C}' | '\u{2B50}' | '\u{2B55}' | '\u{3297}' | '\u{3299}')
}

/// The verifying-key bytes of `member`, the input the hue is keyed on: the
/// room owner's key, or the member record's. `None` for an author the room no
/// longer lists (a removed or banned member), whose avatar stays neutral.
pub(crate) fn member_vk_bytes(
    state: &ChatRoomStateV1,
    owner_vk: &ed25519_dalek::VerifyingKey,
    member: MemberId,
) -> Option<[u8; 32]> {
    if MemberId::from(owner_vk) == member {
        return Some(owner_vk.to_bytes());
    }
    state
        .members
        .members
        .iter()
        .map(|m| &m.member.member_vk)
        .find(|vk| MemberId::from(*vk) == member)
        .map(|vk| vk.to_bytes())
}

/// The hue this viewer draws `member_vk`'s avatar with under `salt`: the
/// keyed hash's hue, quantised ([`crate::avatar_salt::avatar_hue_step`]).
pub(crate) fn display_hue(salt: &[u8; 32], member_vk: &[u8]) -> u16 {
    crate::avatar_salt::avatar_hue_step(crate::avatar_salt::avatar_hue(salt, member_vk))
}

/// The disc's inline style: its hue, or no style attribute at all (the
/// neutral disc). `None` rather than an empty string, so a disc that loses
/// its salt drops the attribute instead of keeping the old value.
pub(crate) fn avatar_style(hue: Option<u16>) -> Option<String> {
    hue.map(|h| format!("--avatar-hue: {h}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_words_give_two_initials() {
        assert_eq!(initials("Kara Martinez"), "KM");
        assert_eq!(initials("ben anderson (Member)"), "BA");
        assert_eq!(initials("  Xander   Golden  "), "XG");
    }

    #[test]
    fn a_word_without_letters_is_skipped() {
        assert_eq!(initials("Kara (Owner)"), "KO");
        assert_eq!(initials("— Priya"), "P");
    }

    #[test]
    fn one_word_gives_its_first_character() {
        assert_eq!(initials("dmitri"), "D");
        assert_eq!(initials("éva"), "É");
        // A decomposed accent stays with its letter.
        assert_eq!(initials("e\u{0301}va"), "E\u{0301}");
    }

    #[test]
    fn a_glyph_keeps_what_draws_with_it() {
        // A ZWJ family and a skin tone stay one emoji.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(initials(family), family);
        assert_eq!(initials("\u{1F44D}\u{1F3FD}"), "\u{1F44D}\u{1F3FD}");
        // A variation selector stays with its symbol.
        assert_eq!(initials("\u{2764}\u{FE0F}"), "\u{2764}\u{FE0F}");
        // A flag is one cluster of two regional indicators.
        assert_eq!(initials("\u{1F1FA}\u{1F1F8}"), "\u{1F1FA}\u{1F1F8}");
        // A letter wins over an emoji.
        assert_eq!(initials("\u{1F1FA}\u{1F1F8} Sam"), "S");
        // Devanagari: a vowel sign, and a conjunct through the virama.
        assert_eq!(
            initials("\u{0915}\u{093F}\u{0930}\u{0923}"),
            "\u{0915}\u{093F}"
        );
        assert_eq!(
            initials("\u{0915}\u{094D}\u{0937}\u{092E}\u{093E}"),
            "\u{0915}\u{094D}\u{0937}"
        );
        assert_eq!(
            initials("\u{0930}\u{093E}\u{091C} \u{0915}\u{0941}\u{092E}\u{093E}\u{0930}"),
            "\u{0930}\u{093E}\u{0915}\u{0941}"
        );
        // Tamil's virama does not form conjuncts: "ச்" alone.
        assert_eq!(
            initials("\u{0B9A}\u{0BCD}\u{0B9A}\u{0BC6}\u{0BB2}\u{0BCD}\u{0BB5}\u{0BAE}\u{0BCD}"),
            "\u{0B9A}\u{0BCD}"
        );
        assert_eq!(initials("\u{0B95}\u{0BBF}"), "\u{0B95}\u{0BBF}");
        // Sinhala letters are letters (සමන් gives ස; two words, two glyphs).
        assert_eq!(initials("\u{0DC3}\u{0DB8}\u{0DB1}\u{0DCA}"), "\u{0DC3}");
        assert_eq!(
            initials("\u{0DC3}\u{0DB8}\u{0DB1}\u{0DCA} \u{0D9A}\u{0DD4}\u{0DB8}\u{0DCF}\u{0DBB}"),
            "\u{0DC3}\u{0D9A}\u{0DD4}"
        );
        assert_eq!(
            initials("\u{0DC3}\u{0DD4}\u{0DB1}\u{0DD2}\u{0DBD}\u{0DCA}"),
            "\u{0DC3}\u{0DD4}"
        );
        // Hebrew points and Thai marks stay on their letter.
        assert_eq!(
            initials("\u{05E9}\u{05C1}\u{05B8}\u{05DC}\u{05D5}\u{05B9}\u{05DD}"),
            "\u{05E9}\u{05C1}\u{05B8}"
        );
        assert_eq!(
            initials("\u{0E19}\u{0E49}\u{0E33}"),
            "\u{0E19}\u{0E49}\u{0E33}"
        );
    }

    /// A nickname cannot write text into the disc: a ZWJ joins only emoji,
    /// a virama only same-script consonants, and marks are capped.
    #[test]
    fn a_name_cannot_smuggle_text_into_the_disc() {
        assert_eq!(initials("O\u{200D}W\u{200D}N\u{200D}E\u{200D}R"), "O");
        assert_eq!(initials("A\u{200D}d\u{200D}m\u{200D}i\u{200D}n x"), "AX");
        // A Devanagari virama does not join Latin letters.
        assert_eq!(initials("\u{0915}\u{094D}abc"), "\u{0915}\u{094D}");
        assert_eq!(initials("x\u{094D}yz"), "X\u{094D}");
        // Eight combining marks: the letter keeps two.
        let zalgo = format!("a{}", "\u{0301}".repeat(8));
        assert_eq!(initials(&zalgo), "A\u{0301}\u{0301}");
        // Format characters never reach the disc.
        assert_eq!(initials("a\u{200B}\u{0301}"), "A");
    }

    #[test]
    fn a_lone_mark_or_control_is_never_shown() {
        assert_eq!(initials("\u{0301}abc"), "A");
        assert_eq!(initials("\u{0301}\u{0302}"), "?");
        assert_eq!(initials("\u{093F}\u{0915}"), "\u{0915}");
        assert_eq!(initials("\u{200D}x"), "X");
        // Arabic harakat before the first letter.
        assert_eq!(initials("\u{064E}\u{0639}\u{0644}\u{064A}"), "\u{0639}");
        // Leading RLO, ZWSP, LRM.
        assert_eq!(initials("\u{202E}abc"), "A");
        assert_eq!(initials("\u{200B}bob"), "B");
        assert_eq!(initials("\u{200E}cat dog"), "CD");
        // Nothing but format characters.
        assert_eq!(initials("\u{200B}\u{200E}\u{202E}\u{2066}"), "?");
    }

    /// Characters that draw nothing, though Unicode files them as letters or
    /// symbols, are skipped like format characters; a name of nothing else
    /// gets "?". Arrows, box drawing and Braille are not emoji.
    #[test]
    fn blank_and_non_emoji_symbols_never_fill_the_disc() {
        for blank in [
            "\u{3164}",
            "\u{115F}\u{1160}",
            "\u{FFA0}",
            "\u{2800}\u{2800}",
        ] {
            assert_eq!(initials(blank), "?", "{blank:?}");
        }
        assert_eq!(initials("\u{3164} bob"), "B");
        assert_eq!(initials("\u{3164}x"), "X");
        for symbol in ["\u{2192}", "\u{2503}", "\u{2840}", "\u{2B05}"] {
            assert_eq!(initials(symbol), "?", "{symbol:?}");
        }
        assert_eq!(initials("\u{2B50}"), "\u{2B50}");
        assert_eq!(initials("\u{231A}"), "\u{231A}");
    }

    #[test]
    fn at_most_two_glyphs() {
        assert_eq!(initials("ß"), "SS");
        assert_eq!(initials("ßa ßb"), "SS");
        assert_eq!(initials("anna ßb"), "AS");
        assert_eq!(initials("Kara Martinez Lopez"), "KM");
        // ŉ upper-cases to an apostrophe and N.
        assert_eq!(initials("\u{0149}"), "\u{02BC}N");
    }

    #[test]
    fn an_empty_name_gives_a_question_mark() {
        assert_eq!(initials(""), "?");
        assert_eq!(initials("   "), "?");
    }

    /// The hues `message-rows.spec.ts` expects for the test hooks' fixed
    /// senders under `__riverTest.setAvatarSalt(1)` and `(2)` (a salt of
    /// that byte repeated). If the hash or the quantisation changes, the
    /// spec's numbers change with these.
    #[test]
    fn the_fixed_test_senders_hues_are_pinned() {
        for (seed, under_1, under_2) in [(0x5Au8, 288, 252), (0x6A, 144, 288), (0x7B, 108, 288)] {
            let vk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]).verifying_key();
            assert_eq!(
                display_hue(&[1; 32], vk.as_bytes()),
                under_1,
                "{seed:#x} under salt 1"
            );
            assert_eq!(
                display_hue(&[2; 32], vk.as_bytes()),
                under_2,
                "{seed:#x} under salt 2"
            );
        }
    }

    #[test]
    fn a_neutral_disc_carries_no_hue() {
        assert_eq!(avatar_style(None), None);
        assert_eq!(
            avatar_style(Some(250)).as_deref(),
            Some("--avatar-hue: 250")
        );
    }
}
