//! Showing the user's own edits, deletes and reactions before their signatures
//! arrive (`outbox.rs`). The conversation's groups are built from `ROOMS`, which
//! holds only signed messages; this applies the pending actions over them, so a
//! reaction appears, an edit shows its new text and a delete removes the
//! message the moment the user acts. When the action commits, the outbox entry
//! goes and `ROOMS` gains the same change in the same tick, so nothing moves.

use super::markdown::message_to_html_with_mentions;
use super::outbox::PendingEffect;
use super::{DisplayItem, GroupedMessage};
use river_core::room_state::member::MemberId;
use std::collections::HashMap;

/// Apply `effects`, in order, as `self_id` making them.
pub(super) fn apply_pending_effects(
    items: &mut Vec<DisplayItem>,
    effects: &[PendingEffect],
    self_id: MemberId,
    member_names: &HashMap<MemberId, String>,
) {
    for effect in effects {
        match effect {
            PendingEffect::Delete { target } => {
                for item in items.iter_mut() {
                    if let DisplayItem::Messages(group) = item {
                        group.messages.retain(|m| m.message_id != *target);
                    }
                }
                items.retain(|item| match item {
                    DisplayItem::Messages(group) => !group.messages.is_empty(),
                    DisplayItem::Event(_) => true,
                });
            }
            PendingEffect::Edit { target, text } => {
                if let Some(m) = find(items, target) {
                    m.content_html =
                        message_to_html_with_mentions(text, member_names, Some(self_id));
                    m.content_text = text.clone();
                    m.edited = true;
                }
            }
            PendingEffect::React { target, emoji } => {
                if let Some(m) = find(items, target) {
                    let reactors = m.reactions.entry(emoji.clone()).or_default();
                    if !reactors.contains(&self_id) {
                        reactors.push(self_id);
                    }
                }
            }
            PendingEffect::Unreact { target, emoji } => {
                if let Some(m) = find(items, target) {
                    if let Some(reactors) = m.reactions.get_mut(emoji) {
                        reactors.retain(|r| *r != self_id);
                        if reactors.is_empty() {
                            m.reactions.remove(emoji);
                        }
                    }
                }
            }
        }
    }
}

fn find<'a>(
    items: &'a mut [DisplayItem],
    target: &river_core::room_state::message::MessageId,
) -> Option<&'a mut GroupedMessage> {
    items.iter_mut().find_map(|item| match item {
        DisplayItem::Messages(group) => group.messages.iter_mut().find(|m| m.message_id == *target),
        DisplayItem::Event(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{MessageGroup, ReplyStrip};
    use super::*;
    use chrono::{DateTime, Utc};
    use river_core::room_state::message::MessageId;

    fn id(n: u64) -> MessageId {
        MessageId(freenet_scaffold::util::FastHash(n as i64))
    }

    fn message(n: u64, text: &str) -> GroupedMessage {
        GroupedMessage {
            content_text: text.to_string(),
            content_html: text.to_string(),
            time: DateTime::<Utc>::UNIX_EPOCH,
            sent_ms: n as i64,
            time_clamped: false,
            id: format!("{n}"),
            message_id: id(n),
            edited: false,
            reactions: HashMap::new(),
            reply_strip: ReplyStrip::NotAReply,
            receive_delay_secs: None,
        }
    }

    fn items(messages: Vec<GroupedMessage>) -> Vec<DisplayItem> {
        vec![DisplayItem::Messages(MessageGroup {
            author_id: me(),
            author_name: "Me".to_string(),
            author_badge: None,
            author_impersonation: None,
            is_self: true,
            first_time: DateTime::<Utc>::UNIX_EPOCH,
            time_clamped: false,
            first_delay_secs: None,
            messages,
        })]
    }

    fn me() -> MemberId {
        MemberId(freenet_scaffold::util::FastHash(42))
    }

    fn only(items: &[DisplayItem]) -> &[GroupedMessage] {
        match &items[0] {
            DisplayItem::Messages(g) => &g.messages,
            DisplayItem::Event(_) => panic!("expected messages"),
        }
    }

    #[test]
    fn a_pending_reaction_edit_and_delete_show_over_the_committed_messages() {
        let mut shown = items(vec![message(1, "one"), message(2, "two")]);
        apply_pending_effects(
            &mut shown,
            &[
                PendingEffect::React {
                    target: id(1),
                    emoji: "👍".to_string(),
                },
                PendingEffect::Edit {
                    target: id(1),
                    text: "one, edited".to_string(),
                },
                PendingEffect::Delete { target: id(2) },
            ],
            me(),
            &HashMap::new(),
        );
        let messages = only(&shown);
        assert_eq!(messages.len(), 1, "the deleted message is gone");
        assert_eq!(messages[0].content_text, "one, edited");
        assert!(messages[0].edited);
        assert!(messages[0].content_html.contains("one, edited"));
        assert_eq!(messages[0].reactions["👍"], vec![me()]);
    }

    #[test]
    fn a_reaction_swap_moves_the_own_reaction_and_leaves_others() {
        let other = MemberId(freenet_scaffold::util::FastHash(7));
        let mut m = message(1, "one");
        m.reactions.insert("👍".to_string(), vec![other, me()]);
        let mut shown = items(vec![m]);
        apply_pending_effects(
            &mut shown,
            &[
                PendingEffect::Unreact {
                    target: id(1),
                    emoji: "👍".to_string(),
                },
                PendingEffect::React {
                    target: id(1),
                    emoji: "🎉".to_string(),
                },
            ],
            me(),
            &HashMap::new(),
        );
        let reactions = &only(&shown)[0].reactions;
        assert_eq!(reactions["👍"], vec![other]);
        assert_eq!(reactions["🎉"], vec![me()]);
    }

    #[test]
    fn removing_the_last_reaction_drops_its_chip_and_deleting_a_group_drops_it() {
        let mut m = message(1, "one");
        m.reactions.insert("👍".to_string(), vec![me()]);
        let mut shown = items(vec![m]);
        apply_pending_effects(
            &mut shown,
            &[PendingEffect::Unreact {
                target: id(1),
                emoji: "👍".to_string(),
            }],
            me(),
            &HashMap::new(),
        );
        assert!(only(&shown)[0].reactions.is_empty());
        apply_pending_effects(
            &mut shown,
            &[PendingEffect::Delete { target: id(1) }],
            me(),
            &HashMap::new(),
        );
        assert!(shown.is_empty(), "an emptied group is not rendered");
    }

    #[test]
    fn an_effect_on_a_message_not_shown_changes_nothing() {
        let mut shown = items(vec![message(1, "one")]);
        let before = shown.clone();
        apply_pending_effects(
            &mut shown,
            &[PendingEffect::Edit {
                target: id(9),
                text: "x".to_string(),
            }],
            me(),
            &HashMap::new(),
        );
        assert!(shown == before);
    }
}
