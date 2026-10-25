//! Initials avatars for message authors.
//!
//! The disc's colour is per viewer: [`crate::avatar_salt::avatar_hue`] under
//! the room's salt in [`crate::avatar_salt::AVATAR_SALTS`]. A room whose salt
//! is absent gets a neutral grey disc; there is deliberately no other source
//! of colour (see `avatar_salt.rs`).

use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomStateV1;

/// The disc's text: the first letter of each of the first two words of the
/// display nickname, upper-cased; the first character if the name is one
/// word; "?" if it has none. At most two visible glyphs: an upper-casing that
/// expands (`ß` to `SS`) counts each letter.
///
/// A word's "letter" is its first alphanumeric character, so the `(Owner)`
/// in `Kara (Owner)` gives `O`, not `(`. A character keeps what a reader sees
/// as part of it (combining marks, an Indic vowel sign or conjunct, an emoji's
/// skin tone, variation selector or ZWJ sequence; see [`first_glyphs`]), and
/// a mark with no letter before it is never shown alone.
pub(crate) fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() >= 2 {
        let letters: String = words
            .iter()
            .flat_map(|w| first_glyphs(w, true))
            .take(2)
            .collect();
        if !letters.is_empty() {
            return letters;
        }
    }
    let one: String = words
        .first()
        .map(|w| first_glyphs(w, false))
        .unwrap_or_default()
        .into_iter()
        .take(2)
        .collect();
    if one.is_empty() {
        "?".to_string()
    } else {
        one
    }
}

/// The word's first character, upper-cased, as the glyphs it shows: the base
/// character's upper case (one glyph per letter, `ß` gives two) with what
/// extends it attached to the last. `alphanumeric` skips to the first
/// alphanumeric base; otherwise any base will do. Leading marks are skipped,
/// so a lone mark is never the result.
///
/// A close approximation of an extended grapheme cluster, without a
/// segmentation table: extenders are combining marks, variation selectors
/// and emoji modifiers ([`extends_glyph`]); a ZWJ or an Indic virama also
/// takes the character after it.
fn first_glyphs(word: &str, alphanumeric: bool) -> Vec<String> {
    let mut chars = word.chars().peekable();
    let base = loop {
        let c = chars.next();
        match c {
            None => return Vec::new(),
            Some(c) if extends_glyph(c) || c == ZWJ => continue,
            Some(c) if alphanumeric && !c.is_alphanumeric() => continue,
            Some(c) => break c,
        }
    };
    let mut glyphs: Vec<String> = base.to_uppercase().map(String::from).collect();
    let last = glyphs.last_mut().expect("upper case is never empty");
    while let Some(&c) = chars.peek() {
        if c == ZWJ || is_virama(c) {
            last.push(c);
            chars.next();
            if let Some(joined) = chars.next() {
                last.push(joined);
            }
        } else if extends_glyph(c) {
            last.push(c);
            chars.next();
        } else {
            break;
        }
    }
    glyphs
}

const ZWJ: char = '\u{200D}';

/// Characters drawn as part of the character before them: combining marks
/// (Latin, Greek, Cyrillic, symbols), variation selectors, emoji skin-tone
/// modifiers, and the Indic blocks' dependent signs (vowel signs, nukta,
/// candrabindu, anusvara, visarga), which sit at the same offsets in every
/// block from Devanagari to Sinhala.
fn extends_glyph(c: char) -> bool {
    if matches!(c, '\u{0300}'..='\u{036F}' | '\u{0483}'..='\u{0489}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}' | '\u{20D0}'..='\u{20FF}' | '\u{FE20}'..='\u{FE2F}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}' | '\u{1F3FB}'..='\u{1F3FF}')
    {
        return true;
    }
    match indic_offset(c) {
        Some(o) => matches!(o, 0x00..=0x03 | 0x3A..=0x3C | 0x3E..=0x4F | 0x51..=0x57 | 0x62..=0x63),
        None => false,
    }
}

/// An Indic virama, which joins the next consonant into a conjunct.
fn is_virama(c: char) -> bool {
    indic_offset(c) == Some(0x4D)
}

/// `c`'s offset in its Indic block (Devanagari U+0900 to Sinhala U+0DFF, 128
/// code points each), if it is in one.
fn indic_offset(c: char) -> Option<u32> {
    let c = c as u32;
    (0x0900..=0x0DFF).contains(&c).then_some(c & 0x7F)
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
        // Tamil (a different block, the same offsets).
        assert_eq!(initials("\u{0B95}\u{0BBF}"), "\u{0B95}\u{0BBF}");
    }

    #[test]
    fn a_lone_mark_is_never_shown() {
        assert_eq!(initials("\u{0301}abc"), "A");
        assert_eq!(initials("\u{0301}\u{0302}"), "?");
        assert_eq!(initials("\u{093F}\u{0915}"), "\u{0915}");
        assert_eq!(initials("\u{200D}x"), "X");
    }

    #[test]
    fn at_most_two_glyphs() {
        assert_eq!(initials("ß"), "SS");
        assert_eq!(initials("ßa ßb"), "SS");
        assert_eq!(initials("anna ßb"), "AS");
        assert_eq!(initials("Kara Martinez Lopez"), "KM");
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
