//! Initials avatars for message authors.
//!
//! The disc's colour is per viewer: [`crate::avatar_salt::avatar_hue`] under
//! the room's salt in [`crate::avatar_salt::AVATAR_SALTS`]. A room whose salt
//! is absent gets a neutral grey disc; there is deliberately no other source
//! of colour (see `avatar_salt.rs`).

use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomStateV1;

/// The disc's text: the first letter of each of the first two words of the
/// display nickname, upper-cased; the first character (with its combining
/// marks) if the name is one word; "?" if it has none.
///
/// A word's "letter" is its first alphanumeric character, so the `(Owner)`
/// in `Kara (Owner)` gives `O`, not `(`.
pub(crate) fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() >= 2 {
        let letters: String = words
            .iter()
            .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
            .take(2)
            .flat_map(char::to_uppercase)
            .collect();
        if !letters.is_empty() {
            return letters;
        }
    }
    let Some(word) = words.first() else {
        return "?".to_string();
    };
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return "?".to_string();
    };
    first
        .to_uppercase()
        .chain(chars.take_while(|c| is_combining_mark(*c)))
        .collect()
}

/// Combining diacritical marks, the part of a grapheme cluster a Latin,
/// Greek or Cyrillic name is likely to carry after its first letter.
fn is_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}' | '\u{20D0}'..='\u{20FF}' | '\u{FE20}'..='\u{FE2F}')
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
    fn an_empty_name_gives_a_question_mark() {
        assert_eq!(initials(""), "?");
        assert_eq!(initials("   "), "?");
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
