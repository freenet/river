//! Display order for a message's reaction chips: by when each emoji's earliest
//! still-standing reaction was added, so a new emoji appends its chip at the
//! end instead of slotting in alphabetically.
//!
//! `MessagesV1::reactions` (river-core) carries membership but no order, so the
//! order is re-derived here by replaying the reaction actions. It lives in the
//! UI because river-core is compiled into the room-contract WASM, and any edit
//! there re-keys the contract.

use crate::components::app::notifications::extract_action_content;
use river_core::room_state::content::{ACTION_TYPE_REACTION, ACTION_TYPE_REMOVE_REACTION};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{MessageId, MessagesV1};
use std::collections::HashMap;
use std::time::SystemTime;

/// One reaction add or removal, in the order the actions replay.
struct ReactionEvent {
    target: MessageId,
    emoji: String,
    actor: MemberId,
    time: SystemTime,
    removed: bool,
}

/// For each message, when each emoji's earliest standing reaction was added.
type FirstReactionTimes = HashMap<MessageId, HashMap<String, SystemTime>>;

/// Replay `events` and keep, per (message, emoji), the add time of the
/// earliest reaction still standing at the end. A reaction removed and later
/// re-added counts from the re-add.
fn first_reaction_times(events: impl IntoIterator<Item = ReactionEvent>) -> FirstReactionTimes {
    let mut standing: HashMap<(MessageId, String, MemberId), SystemTime> = HashMap::new();
    for ev in events {
        let key = (ev.target, ev.emoji, ev.actor);
        if ev.removed {
            standing.remove(&key);
        } else {
            // Idempotent, like the core replay: a repeat add keeps the first.
            standing.entry(key).or_insert(ev.time);
        }
    }
    let mut firsts = FirstReactionTimes::new();
    for ((target, emoji, _), time) in standing {
        let first = firsts
            .entry(target)
            .or_default()
            .entry(emoji)
            .or_insert(time);
        *first = (*first).min(time);
    }
    firsts
}

/// The reaction add/remove actions in `messages_state`, in replay order.
/// Private actions are decrypted with `secrets`; one that cannot be is
/// skipped, exactly as the core replay skips it.
fn reaction_events<'a>(
    messages_state: &'a MessagesV1,
    secrets: &'a HashMap<u32, [u8; 32]>,
) -> impl Iterator<Item = ReactionEvent> + 'a {
    messages_state.messages.iter().filter_map(move |m| {
        let action = extract_action_content(&m.message.content, secrets)?;
        let removed = match action.action_type {
            ACTION_TYPE_REACTION => false,
            ACTION_TYPE_REMOVE_REACTION => true,
            _ => return None,
        };
        let emoji = action.reaction_payload()?.emoji;
        Some(ReactionEvent {
            target: action.target,
            emoji,
            actor: m.message.author,
            time: m.message.time,
            removed,
        })
    })
}

/// [`first_reaction_times`] for every message in `messages_state`.
///
/// Order only matters on a message with two or more distinct emojis, so the
/// replay — and, in a private room, a decrypt per action message — is skipped
/// when there is none.
pub(super) fn reaction_order_for(
    messages_state: &MessagesV1,
    secrets: &HashMap<u32, [u8; 32]>,
) -> FirstReactionTimes {
    let any_multi = messages_state
        .actions_state
        .reactions
        .values()
        .any(|by_emoji| by_emoji.len() > 1);
    if !any_multi {
        return HashMap::new();
    }
    first_reaction_times(reaction_events(messages_state, secrets))
}

/// `reactions` as chips render them: earliest first reaction first, emoji as
/// the tie-break so the order is deterministic, untimed emojis last.
pub(super) fn order_reactions(
    reactions: &HashMap<String, Vec<MemberId>>,
    first_times: Option<&HashMap<String, SystemTime>>,
) -> Vec<(String, Vec<MemberId>)> {
    let time_key = |emoji: &str| {
        let time = first_times.and_then(|t| t.get(emoji)).copied();
        (time.is_none(), time)
    };
    let mut ordered: Vec<(String, Vec<MemberId>)> = reactions
        .iter()
        .map(|(emoji, reactors)| (emoji.clone(), reactors.clone()))
        .collect();
    ordered.sort_by(|(a, _), (b, _)| (time_key(a), a).cmp(&(time_key(b), b)));
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ecies::{decrypt_with_symmetric_key, encrypt_with_symmetric_key};
    use ed25519_dalek::SigningKey;
    use freenet_scaffold::util::FastHash;
    use river_core::room_state::content::ActionContentV1;
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use std::time::Duration;

    const THUMBS: &str = "👍";
    const HEART: &str = "❤️";
    const PARTY: &str = "🎉";

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn member(n: i64) -> MemberId {
        MemberId(FastHash(n))
    }

    fn target() -> MessageId {
        MessageId(FastHash(42))
    }

    fn add(emoji: &str, who: i64, secs: u64) -> ReactionEvent {
        ReactionEvent {
            target: target(),
            emoji: emoji.to_string(),
            actor: member(who),
            time: at(secs),
            removed: false,
        }
    }

    fn remove(emoji: &str, who: i64, secs: u64) -> ReactionEvent {
        ReactionEvent {
            removed: true,
            ..add(emoji, who, secs)
        }
    }

    fn order_of(events: Vec<ReactionEvent>) -> Vec<String> {
        let times = first_reaction_times(events);
        let reactions: HashMap<String, Vec<MemberId>> = times[&target()]
            .keys()
            .map(|e| (e.clone(), vec![]))
            .collect();
        order_reactions(&reactions, times.get(&target()))
            .into_iter()
            .map(|(emoji, _)| emoji)
            .collect()
    }

    #[test]
    fn reactions_order_by_first_reaction_not_by_emoji() {
        // Byte order would put ❤️ (U+2764) before 👍 (U+1F44D).
        assert!(HEART < THUMBS);
        assert_eq!(
            order_of(vec![add(THUMBS, 1, 10), add(HEART, 2, 20)]),
            [THUMBS, HEART]
        );
    }

    #[test]
    fn another_reactor_joining_does_not_move_an_emoji() {
        assert_eq!(
            order_of(vec![
                add(THUMBS, 1, 10),
                add(HEART, 2, 20),
                add(THUMBS, 3, 30)
            ]),
            [THUMBS, HEART]
        );
    }

    #[test]
    fn a_repeated_add_keeps_the_first_time() {
        assert_eq!(
            order_of(vec![
                add(THUMBS, 1, 10),
                add(HEART, 2, 20),
                add(THUMBS, 1, 30)
            ]),
            [THUMBS, HEART]
        );
    }

    #[test]
    fn a_removed_then_readded_reaction_counts_from_the_readd() {
        assert_eq!(
            order_of(vec![
                add(THUMBS, 1, 10),
                add(HEART, 2, 20),
                remove(THUMBS, 1, 30),
                add(THUMBS, 1, 40),
            ]),
            [HEART, THUMBS]
        );
    }

    #[test]
    fn the_earliest_reactor_leaving_falls_back_to_the_next_standing_one() {
        let events = vec![
            add(THUMBS, 1, 10),
            add(HEART, 2, 20),
            add(THUMBS, 3, 30),
            remove(THUMBS, 1, 40),
        ];
        let times = first_reaction_times(events);
        assert_eq!(times[&target()][THUMBS], at(30));
    }

    #[test]
    fn a_removal_only_drops_the_removers_own_reaction() {
        let times = first_reaction_times(vec![add(THUMBS, 1, 10), remove(THUMBS, 2, 20)]);
        assert_eq!(times[&target()][THUMBS], at(10));
    }

    #[test]
    fn equal_times_tie_break_by_emoji() {
        assert_eq!(
            order_of(vec![add(THUMBS, 1, 10), add(HEART, 2, 10)]),
            [HEART, THUMBS]
        );
    }

    // --- Against real signed room state -----------------------------------

    fn signed(sk: &SigningKey, content: RoomMessageBody, secs: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: member(0),
                author: MemberId::from(&sk.verifying_key()),
                time: at(secs),
                content,
            },
            sk,
        )
    }

    fn private_reaction(secret: &[u8; 32], target: MessageId, emoji: &str) -> RoomMessageBody {
        let bytes = ActionContentV1::reaction(target, emoji.to_string()).encode();
        let (ciphertext, nonce) = encrypt_with_symmetric_key(secret, &bytes);
        RoomMessageBody::private_action(ciphertext, nonce, 1)
    }

    /// End to end over `MessagesV1`: a public reaction, a private one, and the
    /// core replay's membership. Also pins that the order follows the core's
    /// semantics rather than drifting from them: the emoji set it orders is
    /// exactly the one `reactions()` reports.
    #[test]
    fn orders_public_and_private_reactions_from_room_state() {
        let alice = SigningKey::from_bytes(&[1; 32]);
        let bob = SigningKey::from_bytes(&[2; 32]);
        let carol = SigningKey::from_bytes(&[3; 32]);
        let secret = [9u8; 32];

        let text = signed(&alice, RoomMessageBody::public("hello".to_string()), 1);
        let id = text.id();
        let messages = vec![
            text,
            signed(
                &bob,
                RoomMessageBody::reaction(id.clone(), THUMBS.into()),
                10,
            ),
            signed(&carol, private_reaction(&secret, id.clone(), PARTY), 20),
            signed(
                &alice,
                RoomMessageBody::reaction(id.clone(), HEART.into()),
                30,
            ),
        ];
        let decrypted: HashMap<MessageId, Vec<u8>> = messages
            .iter()
            .filter_map(|m| match &m.message.content {
                RoomMessageBody::Private {
                    ciphertext, nonce, ..
                } => decrypt_with_symmetric_key(&secret, ciphertext, nonce)
                    .ok()
                    .map(|p| (m.id(), p)),
                _ => None,
            })
            .collect();
        let mut state = MessagesV1 {
            messages,
            actions_state: Default::default(),
        };
        state.rebuild_actions_state_with_decrypted(&decrypted);
        let reactions = state.reactions(&id).expect("reactions").clone();

        let order = |secrets: &HashMap<u32, [u8; 32]>| -> Vec<String> {
            let times = reaction_order_for(&state, secrets);
            order_reactions(&reactions, times.get(&id))
                .into_iter()
                .map(|(emoji, _)| emoji)
                .collect()
        };

        assert_eq!(order(&HashMap::from([(1, secret)])), [THUMBS, PARTY, HEART]);
        // Without the secret the private reaction has no time: it still
        // renders, after the timed ones.
        assert_eq!(order(&HashMap::new()), [THUMBS, HEART, PARTY]);
    }
}
