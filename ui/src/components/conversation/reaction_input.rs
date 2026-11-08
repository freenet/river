//! Whether text typed into the mobile reaction field is one emoji.
//!
//! A reaction is one complete emoji sequence, kept byte for byte: the device
//! keyboard's emoji go in exactly as it produced them (variation selectors,
//! skin tones, ZWJ sequences, flags, keycaps and subdivision flags included),
//! and are never normalised. Surrounding whitespace is not part of the emoji
//! and is left out of the answer; nothing inside it changes.
//!
//! The data is pinned to **Unicode Emoji 16.0** (`emoji-data.txt`, 2024): the
//! `Emoji`, `Emoji_Presentation` and `Emoji_Modifier_Base` ranges below, with
//! the fixed code points of `Emoji_Modifier`, `Emoji_Component`, regional
//! indicators and tags. An emoji newer than that is reported as unknown
//! rather than guessed at. The sequence grammar is UTS #51's (ED-14 to ED-17):
//!
//! - a default-emoji character (`Emoji_Presentation`), optionally followed by
//!   VS16 (U+FE0F);
//! - a text-default emoji character followed by VS16 (a bare one, such as a
//!   digit, `©` or a plain `❤`, renders as text and is refused);
//! - a modifier sequence (`Emoji_Modifier_Base` + a skin tone);
//! - a keycap (`[0-9#*]`, an optional VS16, U+20E3);
//! - a flag (two regional indicators);
//! - a tag sequence (an emoji, tags U+E0020..U+E007E, U+E007F), the
//!   subdivision flags;
//! - a ZWJ sequence of the above (U+200D between elements). After the first
//!   element a text-default character may come without its VS16, as keyboards
//!   that send minimally-qualified sequences do.
//!
//! The whole input must also be a single grapheme cluster (UAX #29). This is a
//! UI check only: the contract and the reaction payload are unchanged and
//! accept any string, as they always have.

use unicode_segmentation::UnicodeSegmentation;

/// Why the field's text is not a reaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReactionInputError {
    /// Nothing, or only whitespace.
    Empty,
    /// Text that is not an emoji this version knows.
    NotEmoji,
    /// More than one emoji.
    Multiple,
    /// Longer than any one emoji ([`MAX_REACTION_BYTES`]).
    TooLong,
}

impl ReactionInputError {
    /// What the field says under it.
    pub(super) fn explanation(self) -> &'static str {
        match self {
            Self::Empty => "Type or paste one emoji.",
            Self::NotEmoji => {
                "That isn\u{2019}t an emoji River knows. Choose one from the palette."
            }
            Self::Multiple => "One emoji per reaction.",
            Self::TooLong => "That\u{2019}s longer than one emoji.",
        }
    }
}

/// The one emoji in `input`, as the exact bytes typed (surrounding whitespace
/// left out), or why it is not one.
pub(super) fn single_emoji(input: &str) -> Result<&str, ReactionInputError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ReactionInputError::Empty);
    }
    if trimmed.len() > MAX_REACTION_BYTES {
        return Err(ReactionInputError::TooLong);
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let mut at = 0;
    let mut sequences = 0;
    while at < chars.len() {
        match emoji_sequence_len(&chars[at..]) {
            Some(len) => {
                at += len;
                sequences += 1;
            }
            None => return Err(ReactionInputError::NotEmoji),
        }
    }
    if sequences > 1 || trimmed.graphemes(true).count() != 1 {
        return Err(ReactionInputError::Multiple);
    }
    Ok(trimmed)
}

/// The most bytes a reaction may have. The longest RGI sequence in Emoji 16.0
/// (a kiss or couple with two skin tones) is 35 bytes, a subdivision flag 28;
/// this leaves room for any of them and refuses pasted runs of text.
pub(super) const MAX_REACTION_BYTES: usize = 64;

/// The most elements a ZWJ sequence may join: the RGI maximum in Emoji 16.0
/// (families, kisses and couples join four).
const MAX_ZWJ_ELEMENTS: usize = 4;

/// The black flag, the only base of the subdivision flags (England,
/// Scotland, Wales: the RGI tag sequences).
const TAG_BASE: char = '\u{1F3F4}';

const VS16: char = '\u{FE0F}';
const ZWJ: char = '\u{200D}';
const KEYCAP: char = '\u{20E3}';
const TAG_END: char = '\u{E007F}';

/// How many chars of one whole emoji sequence `s` starts with, if it starts
/// with one.
fn emoji_sequence_len(s: &[char]) -> Option<usize> {
    // Keycap: [0-9#*] FE0F? 20E3.
    if matches!(s[0], '0'..='9' | '#' | '*') {
        let mut i = 1;
        if s.get(i) == Some(&VS16) {
            i += 1;
        }
        return (s.get(i) == Some(&KEYCAP)).then_some(i + 1);
    }
    // Flag: exactly two regional indicators. A lone one is no flag.
    if is_regional_indicator(s[0]) {
        return s
            .get(1)
            .copied()
            .filter(|&c| is_regional_indicator(c))
            .map(|_| 2);
    }
    // Tag sequence: the black flag alone, a subdivision id in tag letters
    // and digits (a region and its subdivision, 3 to 7 of them), the cancel
    // tag. Any other base, or other tag characters, is no flag.
    if s.get(1).copied().is_some_and(is_tag_spec) {
        if s[0] != TAG_BASE {
            return None;
        }
        let mut i = 1;
        while s.get(i).copied().is_some_and(is_tag_spec) {
            i += 1;
        }
        return ((4..=8).contains(&i) && s.get(i) == Some(&TAG_END)).then_some(i + 1);
    }
    let mut i = zwj_element_len(s, true)?;
    // ZWJ sequence: more elements after each joiner, up to the RGI maximum.
    let mut elements = 1;
    while s.get(i) == Some(&ZWJ) {
        elements += 1;
        if elements > MAX_ZWJ_ELEMENTS {
            return None;
        }
        i += 1;
        i += zwj_element_len(&s[i..], false)?;
    }
    Some(i)
}

/// One element of a ZWJ sequence (also a whole sequence on its own): an emoji
/// character with its VS16 or skin tone. `first` refuses a bare text-default
/// character there, which would render as text.
fn zwj_element_len(s: &[char], first: bool) -> Option<usize> {
    let c = *s.first()?;
    if !is_emoji(c) || is_regional_indicator(c) || matches!(c, '0'..='9' | '#' | '*') {
        return None;
    }
    match s.get(1) {
        Some(&VS16) => Some(2),
        Some(&m) if is_modifier(m) && is_modifier_base(c) => Some(2),
        _ if is_presentation(c) || !first => Some(1),
        _ => None,
    }
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn is_modifier(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// A tag letter (a-z) or tag digit (0-9): what a subdivision id is spelled in.
fn is_tag_spec(c: char) -> bool {
    ('\u{E0061}'..='\u{E007A}').contains(&c) || ('\u{E0030}'..='\u{E0039}').contains(&c)
}

fn in_ranges(c: char, ranges: &[(u32, u32)]) -> bool {
    let c = c as u32;
    ranges
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn is_emoji(c: char) -> bool {
    in_ranges(c, EMOJI)
}

fn is_presentation(c: char) -> bool {
    in_ranges(c, EMOJI_PRESENTATION)
}

fn is_modifier_base(c: char) -> bool {
    in_ranges(c, EMOJI_MODIFIER_BASE)
}

/// `Emoji=Yes`, Unicode Emoji 16.0, adjacent ranges merged. Includes the
/// keycap bases and regional indicators, which the grammar handles first.
const EMOJI: &[(u32, u32)] = &[
    (0x0023, 0x0023),
    (0x002A, 0x002A),
    (0x0030, 0x0039),
    (0x00A9, 0x00A9),
    (0x00AE, 0x00AE),
    (0x203C, 0x203C),
    (0x2049, 0x2049),
    (0x2122, 0x2122),
    (0x2139, 0x2139),
    (0x2194, 0x2199),
    (0x21A9, 0x21AA),
    (0x231A, 0x231B),
    (0x2328, 0x2328),
    (0x23CF, 0x23CF),
    (0x23E9, 0x23F3),
    (0x23F8, 0x23FA),
    (0x24C2, 0x24C2),
    (0x25AA, 0x25AB),
    (0x25B6, 0x25B6),
    (0x25C0, 0x25C0),
    (0x25FB, 0x25FE),
    (0x2600, 0x2604),
    (0x260E, 0x260E),
    (0x2611, 0x2611),
    (0x2614, 0x2615),
    (0x2618, 0x2618),
    (0x261D, 0x261D),
    (0x2620, 0x2620),
    (0x2622, 0x2623),
    (0x2626, 0x2626),
    (0x262A, 0x262A),
    (0x262E, 0x262F),
    (0x2638, 0x263A),
    (0x2640, 0x2640),
    (0x2642, 0x2642),
    (0x2648, 0x2653),
    (0x265F, 0x2660),
    (0x2663, 0x2663),
    (0x2665, 0x2666),
    (0x2668, 0x2668),
    (0x267B, 0x267B),
    (0x267E, 0x267F),
    (0x2692, 0x2697),
    (0x2699, 0x2699),
    (0x269B, 0x269C),
    (0x26A0, 0x26A1),
    (0x26A7, 0x26A7),
    (0x26AA, 0x26AB),
    (0x26B0, 0x26B1),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x26C8, 0x26C8),
    (0x26CE, 0x26CF),
    (0x26D1, 0x26D1),
    (0x26D3, 0x26D4),
    (0x26E9, 0x26EA),
    (0x26F0, 0x26F5),
    (0x26F7, 0x26FA),
    (0x26FD, 0x26FD),
    (0x2702, 0x2702),
    (0x2705, 0x2705),
    (0x2708, 0x270D),
    (0x270F, 0x270F),
    (0x2712, 0x2712),
    (0x2714, 0x2714),
    (0x2716, 0x2716),
    (0x271D, 0x271D),
    (0x2721, 0x2721),
    (0x2728, 0x2728),
    (0x2733, 0x2734),
    (0x2744, 0x2744),
    (0x2747, 0x2747),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2763, 0x2764),
    (0x2795, 0x2797),
    (0x27A1, 0x27A1),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2934, 0x2935),
    (0x2B05, 0x2B07),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x3030, 0x3030),
    (0x303D, 0x303D),
    (0x3297, 0x3297),
    (0x3299, 0x3299),
    (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF),
    (0x1F170, 0x1F171),
    (0x1F17E, 0x1F17F),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F1E6, 0x1F1FF),
    (0x1F201, 0x1F202),
    (0x1F21A, 0x1F21A),
    (0x1F22F, 0x1F22F),
    (0x1F232, 0x1F23A),
    (0x1F250, 0x1F251),
    (0x1F300, 0x1F321),
    (0x1F324, 0x1F393),
    (0x1F396, 0x1F397),
    (0x1F399, 0x1F39B),
    (0x1F39E, 0x1F3F0),
    (0x1F3F3, 0x1F3F5),
    (0x1F3F7, 0x1F4FD),
    (0x1F4FF, 0x1F53D),
    (0x1F549, 0x1F54E),
    (0x1F550, 0x1F567),
    (0x1F56F, 0x1F570),
    (0x1F573, 0x1F57A),
    (0x1F587, 0x1F587),
    (0x1F58A, 0x1F58D),
    (0x1F590, 0x1F590),
    (0x1F595, 0x1F596),
    (0x1F5A4, 0x1F5A5),
    (0x1F5A8, 0x1F5A8),
    (0x1F5B1, 0x1F5B2),
    (0x1F5BC, 0x1F5BC),
    (0x1F5C2, 0x1F5C4),
    (0x1F5D1, 0x1F5D3),
    (0x1F5DC, 0x1F5DE),
    (0x1F5E1, 0x1F5E1),
    (0x1F5E3, 0x1F5E3),
    (0x1F5E8, 0x1F5E8),
    (0x1F5EF, 0x1F5EF),
    (0x1F5F3, 0x1F5F3),
    (0x1F5FA, 0x1F64F),
    (0x1F680, 0x1F6C5),
    (0x1F6CB, 0x1F6D2),
    (0x1F6D5, 0x1F6D7),
    (0x1F6DC, 0x1F6E5),
    (0x1F6E9, 0x1F6E9),
    (0x1F6EB, 0x1F6EC),
    (0x1F6F0, 0x1F6F0),
    (0x1F6F3, 0x1F6FC),
    (0x1F7E0, 0x1F7EB),
    (0x1F7F0, 0x1F7F0),
    (0x1F90C, 0x1F93A),
    (0x1F93C, 0x1F945),
    (0x1F947, 0x1F9FF),
    (0x1FA70, 0x1FA7C),
    (0x1FA80, 0x1FA89),
    (0x1FA8F, 0x1FAC6),
    (0x1FACE, 0x1FADC),
    (0x1FADF, 0x1FAE9),
    (0x1FAF0, 0x1FAF8),
];

/// `Emoji_Presentation=Yes`, Unicode Emoji 16.0, adjacent ranges merged.
const EMOJI_PRESENTATION: &[(u32, u32)] = &[
    (0x231A, 0x231B),
    (0x23E9, 0x23EC),
    (0x23F0, 0x23F0),
    (0x23F3, 0x23F3),
    (0x25FD, 0x25FE),
    (0x2614, 0x2615),
    (0x2648, 0x2653),
    (0x267F, 0x267F),
    (0x2693, 0x2693),
    (0x26A1, 0x26A1),
    (0x26AA, 0x26AB),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x26CE, 0x26CE),
    (0x26D4, 0x26D4),
    (0x26EA, 0x26EA),
    (0x26F2, 0x26F3),
    (0x26F5, 0x26F5),
    (0x26FA, 0x26FA),
    (0x26FD, 0x26FD),
    (0x2705, 0x2705),
    (0x270A, 0x270B),
    (0x2728, 0x2728),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2795, 0x2797),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F1E6, 0x1F1FF),
    (0x1F201, 0x1F201),
    (0x1F21A, 0x1F21A),
    (0x1F22F, 0x1F22F),
    (0x1F232, 0x1F236),
    (0x1F238, 0x1F23A),
    (0x1F250, 0x1F251),
    (0x1F300, 0x1F320),
    (0x1F32D, 0x1F335),
    (0x1F337, 0x1F37C),
    (0x1F37E, 0x1F393),
    (0x1F3A0, 0x1F3CA),
    (0x1F3CF, 0x1F3D3),
    (0x1F3E0, 0x1F3F0),
    (0x1F3F4, 0x1F3F4),
    (0x1F3F8, 0x1F43E),
    (0x1F440, 0x1F440),
    (0x1F442, 0x1F4FC),
    (0x1F4FF, 0x1F53D),
    (0x1F54B, 0x1F54E),
    (0x1F550, 0x1F567),
    (0x1F57A, 0x1F57A),
    (0x1F595, 0x1F596),
    (0x1F5A4, 0x1F5A4),
    (0x1F5FB, 0x1F64F),
    (0x1F680, 0x1F6C5),
    (0x1F6CC, 0x1F6CC),
    (0x1F6D0, 0x1F6D2),
    (0x1F6D5, 0x1F6D7),
    (0x1F6DC, 0x1F6DF),
    (0x1F6EB, 0x1F6EC),
    (0x1F6F4, 0x1F6FC),
    (0x1F7E0, 0x1F7EB),
    (0x1F7F0, 0x1F7F0),
    (0x1F90C, 0x1F93A),
    (0x1F93C, 0x1F945),
    (0x1F947, 0x1F9FF),
    (0x1FA70, 0x1FA7C),
    (0x1FA80, 0x1FA89),
    (0x1FA8F, 0x1FAC6),
    (0x1FACE, 0x1FADC),
    (0x1FADF, 0x1FAE9),
    (0x1FAF0, 0x1FAF8),
];

/// `Emoji_Modifier_Base=Yes`, Unicode Emoji 16.0, adjacent ranges merged.
const EMOJI_MODIFIER_BASE: &[(u32, u32)] = &[
    (0x261D, 0x261D),
    (0x26F9, 0x26F9),
    (0x270A, 0x270D),
    (0x1F385, 0x1F385),
    (0x1F3C2, 0x1F3C4),
    (0x1F3C7, 0x1F3C7),
    (0x1F3CA, 0x1F3CC),
    (0x1F442, 0x1F443),
    (0x1F446, 0x1F450),
    (0x1F466, 0x1F478),
    (0x1F47C, 0x1F47C),
    (0x1F481, 0x1F483),
    (0x1F485, 0x1F487),
    (0x1F48F, 0x1F48F),
    (0x1F491, 0x1F491),
    (0x1F4AA, 0x1F4AA),
    (0x1F574, 0x1F575),
    (0x1F57A, 0x1F57A),
    (0x1F590, 0x1F590),
    (0x1F595, 0x1F596),
    (0x1F645, 0x1F647),
    (0x1F64B, 0x1F64F),
    (0x1F6A3, 0x1F6A3),
    (0x1F6B4, 0x1F6B6),
    (0x1F6C0, 0x1F6C0),
    (0x1F6CC, 0x1F6CC),
    (0x1F90C, 0x1F90C),
    (0x1F90F, 0x1F90F),
    (0x1F918, 0x1F91F),
    (0x1F926, 0x1F926),
    (0x1F930, 0x1F939),
    (0x1F93C, 0x1F93E),
    (0x1F977, 0x1F977),
    (0x1F9B5, 0x1F9B6),
    (0x1F9B8, 0x1F9B9),
    (0x1F9BB, 0x1F9BB),
    (0x1F9CD, 0x1F9CF),
    (0x1F9D1, 0x1F9DD),
    (0x1FAC3, 0x1FAC5),
    (0x1FAF0, 0x1FAF8),
];

#[cfg(test)]
mod tests {
    use super::*;
    use ReactionInputError::*;

    /// Accepted inputs come back byte for byte.
    fn accepts(input: &str) {
        assert_eq!(
            single_emoji(input),
            Ok(input),
            "{input:?} {:x?}",
            input.chars().map(|c| c as u32).collect::<Vec<_>>()
        );
    }

    fn refuses(input: &str, why: ReactionInputError) {
        assert_eq!(single_emoji(input), Err(why), "{input:?}");
    }

    #[test]
    fn the_tables_are_sorted_and_disjoint() {
        for table in [EMOJI, EMOJI_PRESENTATION, EMOJI_MODIFIER_BASE] {
            for pair in table.windows(2) {
                assert!(pair[0].0 <= pair[0].1 && pair[0].1 < pair[1].0, "{pair:x?}");
            }
        }
        // Every presentation and modifier-base character is an emoji.
        for table in [EMOJI_PRESENTATION, EMOJI_MODIFIER_BASE] {
            for &(lo, hi) in table {
                for c in lo..=hi {
                    let c = char::from_u32(c).unwrap();
                    assert!(is_emoji(c), "{:x}", c as u32);
                }
            }
        }
    }

    #[test]
    fn default_emoji_characters() {
        accepts("👍");
        accepts("😂");
        accepts("🔥");
        accepts("⌚");
        accepts("🦰"); // a hair component on its own
        accepts("🏽"); // a skin tone on its own is an emoji character
    }

    /// The Emoji 16.0 additions, which an older table would refuse.
    #[test]
    fn emoji_16_additions() {
        for added in [
            "\u{1FAE9}",
            "\u{1FA89}",
            "\u{1FA8F}",
            "\u{1FABE}",
            "\u{1FAC6}",
            "\u{1FADC}",
            "\u{1FADF}",
        ] {
            accepts(added);
        }
        // Flag: Sark, new in 16.0, is a regional-indicator pair.
        accepts("\u{1F1E8}\u{1F1F6}");
        // One past each 16.0 block end is not assigned in 16.0.
        refuses("\u{1FAEA}", NotEmoji);
        refuses("\u{1FA8A}", NotEmoji);
    }

    #[test]
    fn a_variation_selector_is_kept_exactly() {
        // Text-default characters with VS16, as keyboards send them.
        accepts("❤\u{FE0F}");
        accepts("☺\u{FE0F}");
        accepts("\u{00A9}\u{FE0F}");
        // A redundant VS16 on a default-emoji character is kept, not removed.
        accepts("👍\u{FE0F}");
        // Without it a text-default character is text.
        refuses("❤", NotEmoji);
        refuses("\u{00A9}", NotEmoji);
        // VS15 asks for text.
        refuses("❤\u{FE0E}", NotEmoji);
    }

    #[test]
    fn modifier_sequences() {
        accepts("👍🏽");
        accepts("👋🏿");
        accepts("\u{1FAF1}\u{1F3FB}"); // rightwards hand, light
                                       // A skin tone after a character that takes none is two emoji.
        refuses("😂🏽", Multiple);
    }

    #[test]
    fn zwj_sequences() {
        accepts("👨\u{200D}👩\u{200D}👧\u{200D}👦"); // family
        accepts("🧑\u{200D}⚕\u{FE0F}"); // health worker
        accepts("🏳\u{FE0F}\u{200D}🌈"); // rainbow flag
        accepts("❤\u{FE0F}\u{200D}🔥"); // heart on fire
        accepts("🧑🏽\u{200D}🚀"); // astronaut, medium skin tone
        accepts("👩🏻\u{200D}\u{2764}\u{FE0F}\u{200D}👨🏿"); // couple with heart
        accepts("🏃\u{200D}♂"); // minimally qualified: no VS16 on the sign
                                // Emoji 16.0's new direction-facing ZWJ sequences.
        accepts("🏃\u{200D}\u{27A1}\u{FE0F}");
        // A dangling joiner is not a sequence.
        refuses("👨\u{200D}", NotEmoji);
        refuses("\u{200D}👨", NotEmoji);
        refuses("a\u{200D}👨", NotEmoji);
    }

    #[test]
    fn flags() {
        accepts("🇫🇷");
        accepts("🇺🇳");
        refuses("🇫", NotEmoji);
        refuses("🇫🇷🇩🇪", Multiple);
    }

    #[test]
    fn keycaps() {
        accepts("1\u{FE0F}\u{20E3}");
        accepts("#\u{FE0F}\u{20E3}");
        accepts("*\u{20E3}");
        refuses("1", NotEmoji);
        refuses("#", NotEmoji);
        refuses("1\u{FE0F}", NotEmoji);
    }

    #[test]
    fn subdivision_flags() {
        // England, Scotland, Wales.
        accepts("🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}");
        accepts("🏴\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}");
        accepts("🏴\u{E0067}\u{E0062}\u{E0077}\u{E006C}\u{E0073}\u{E007F}");
        // No cancel tag, or nothing to tag.
        refuses("🏴\u{E0067}\u{E0062}", NotEmoji);
        refuses("\u{E0067}\u{E007F}", NotEmoji);
        // Only the black flag takes tags, and only letters and digits.
        refuses(
            "😀\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}",
            NotEmoji,
        );
        refuses(
            "🏳\u{FE0F}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}",
            NotEmoji,
        );
        refuses(
            "🏴\u{E0047}\u{E0042}\u{E0045}\u{E004E}\u{E0047}\u{E007F}",
            NotEmoji,
        ); // capitals
        refuses("🏴\u{E0020}\u{E0067}\u{E0062}\u{E007F}", NotEmoji); // tag space
                                                                     // A subdivision id is 3 to 7 tags.
        refuses("🏴\u{E0067}\u{E0062}\u{E007F}", NotEmoji);
        refuses(&format!("🏴{}\u{E007F}", "\u{E0061}".repeat(8)), NotEmoji);
    }

    /// The length and ZWJ caps: every RGI sequence fits, longer runs do not.
    #[test]
    fn length_and_join_caps() {
        // The longest RGI sequences (35 bytes) fit.
        let kiss = "👩🏻\u{200D}\u{2764}\u{FE0F}\u{200D}💋\u{200D}👨🏼";
        assert_eq!(kiss.len(), 35);
        accepts(kiss);
        accepts("👨\u{200D}👩\u{200D}👧\u{200D}👦");
        // A fifth element joined is past the RGI maximum.
        refuses("👨\u{200D}👩\u{200D}👧\u{200D}👦\u{200D}👶", NotEmoji);
        // Past 64 bytes, whatever it is.
        refuses(&"👍".repeat(17), TooLong);
        refuses(&"a".repeat(65), TooLong);
        assert_eq!(
            single_emoji(&format!("  {kiss}  ")),
            Ok(kiss),
            "whitespace is not counted"
        );
    }

    #[test]
    fn text_and_emptiness_are_refused() {
        refuses("", Empty);
        refuses("   ", Empty);
        refuses("\n\t", Empty);
        refuses("a", NotEmoji);
        refuses("ok", NotEmoji);
        refuses("7", NotEmoji);
        refuses("é", NotEmoji);
        refuses("👍a", NotEmoji);
        refuses("a👍", NotEmoji);
        refuses("👍 👍", NotEmoji);
    }

    #[test]
    fn several_emoji_are_refused() {
        refuses("👍👍", Multiple);
        refuses("😂🔥", Multiple);
        refuses("❤\u{FE0F}👍", Multiple);
    }

    /// Surrounding whitespace is not part of the emoji; the emoji's own
    /// bytes come back unchanged.
    #[test]
    fn surrounding_whitespace_is_left_out() {
        assert_eq!(single_emoji(" ❤\u{FE0F} "), Ok("❤\u{FE0F}"));
        assert_eq!(single_emoji("👍\n"), Ok("👍"));
    }

    /// Every palette choice is accepted as typed, so the field and the
    /// palette agree.
    #[test]
    fn every_palette_choice_is_one_emoji() {
        for emoji in super::super::emoji_picker::FREQUENT_EMOJIS {
            accepts(emoji);
        }
    }
}
