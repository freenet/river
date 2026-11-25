//! Presentation order for the production reaction projection. Replay never
//! supplies membership: an emoji is shown only when `MessagesV1::reactions`
//! says it is standing. Retained messages are already in canonical (time, id)
//! order, the same order the production action rebuild consumes.

use crate::components::app::notifications::extract_action_content;
use river_core::room_state::content::{ACTION_TYPE_REACTION, ACTION_TYPE_REMOVE_REACTION};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{MessageId, MessagesV1};
use river_core::SignedTime;
use std::collections::HashMap;

type EmojiRanks = HashMap<String, SignedTime>;
type ReactionRanks = HashMap<MessageId, EmojiRanks>;

/// Earliest still-standing add for each emoji. Replay only when order can
/// matter, once for the full retained history rather than once per row.
/// Recompute when secrets change as well as when messages change; no cache can
/// infer that from a message count or newest id.
///
/// Cost, measured 2026-11-18 (release native build, x86_64, every retained
/// message a reaction on one target): about 0.27 ms public and 0.56 ms private
/// at 200 retained messages, 1.4 / 2.7 ms at 1,000. It runs from the
/// conversation's `message_groups` memo, so when room data or receive times
/// change, not on every render.
pub(super) fn reaction_ranks_for(
    messages: &MessagesV1,
    secrets: &HashMap<u32, [u8; 32]>,
) -> ReactionRanks {
    if !messages
        .actions_state
        .reactions
        .values()
        .any(|r| r.len() > 1)
    {
        return HashMap::new();
    }
    // Only final, displayable targets need ranks. Using the production
    // projection here also excludes absent targets, deletes and action bodies.
    let targets: HashMap<_, _> = messages
        .display_messages()
        .filter(|m| !m.message.content.is_event())
        .filter_map(|m| {
            let id = m.id();
            messages
                .reactions(&id)
                .filter(|r| r.len() > 1)
                .map(|r| (id, r))
        })
        .collect();
    if targets.is_empty() {
        return HashMap::new();
    }
    let mut standing: HashMap<(MessageId, String, MemberId), SignedTime> = HashMap::new();
    for message in &messages.messages {
        let Some(action) = extract_action_content(&message.message.content, secrets) else {
            continue;
        };
        if !targets.contains_key(&action.target) {
            continue;
        }
        let Some(payload) = action.reaction_payload() else {
            continue;
        };
        let key = (action.target, payload.emoji, message.message.author);
        match action.action_type {
            // A duplicate does not restart an interval; another emoji by the
            // same member does not terminate this one. The contract allows it.
            ACTION_TYPE_REACTION => {
                standing.entry(key).or_insert(message.message.time);
            }
            ACTION_TYPE_REMOVE_REACTION => {
                standing.remove(&key);
            }
            _ => {}
        }
    }
    let mut ranks: ReactionRanks = HashMap::new();
    for ((target, emoji, reactor), time) in standing {
        // Membership/counts remain exclusively the production projection's.
        if !targets[&target]
            .get(&emoji)
            .is_some_and(|rs| rs.contains(&reactor))
        {
            continue;
        }
        ranks
            .entry(target)
            .or_default()
            .entry(emoji)
            .and_modify(|earliest| *earliest = (*earliest).min(time))
            .or_insert(time);
    }
    ranks
}

/// Order only the emojis the production projection exposes. Missing history
/// or unavailable secrets have unknown rank, after known ranks, tied by emoji.
pub(super) fn order_reactions(
    reactions: &HashMap<String, Vec<MemberId>>,
    ranks: Option<&EmojiRanks>,
) -> Vec<String> {
    let mut emojis: Vec<_> = reactions.keys().cloned().collect();
    emojis.sort_by_cached_key(|e| {
        let r = ranks.and_then(|m| m.get(e)).copied();
        (r.is_none(), r, e.clone())
    });
    emojis
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::conversation::seal::SealInput;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::content::{ActionContentV1, CONTENT_TYPE_ACTION};
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use std::time::{Duration, UNIX_EPOCH};

    const SECRET: [u8; 32] = [12; 32];
    const SECRET_VERSION: u32 = 7;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }
    fn member(n: u8) -> MemberId {
        MemberId::from(&key(n).verifying_key())
    }
    fn signed(author: u8, second: u64, content: RoomMessageBody) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: member(1),
                author: member(author),
                time: (UNIX_EPOCH + Duration::from_secs(second)).into(),
                content,
            },
            &key(author),
        )
    }
    fn text() -> AuthorizedMessageV1 {
        signed(1, 0, RoomMessageBody::public("target".into()))
    }
    fn action(author: u8, second: u64, content: ActionContentV1) -> AuthorizedMessageV1 {
        signed(
            author,
            second,
            RoomMessageBody::public_raw(CONTENT_TYPE_ACTION, 1, content.encode()),
        )
    }
    fn add(author: u8, second: u64, target: &MessageId, emoji: &str) -> AuthorizedMessageV1 {
        action(
            author,
            second,
            ActionContentV1::reaction(target.clone(), emoji.into()),
        )
    }
    fn remove(author: u8, second: u64, target: &MessageId, emoji: &str) -> AuthorizedMessageV1 {
        action(
            author,
            second,
            ActionContentV1::remove_reaction(target.clone(), emoji.into()),
        )
    }
    fn rebuild(messages: Vec<AuthorizedMessageV1>, secrets: &HashMap<u32, [u8; 32]>) -> MessagesV1 {
        let mut state = MessagesV1 {
            messages,
            ..Default::default()
        };
        state.messages.sort_by_key(|m| (m.message.time, m.id()));
        let decrypted = state
            .messages
            .iter()
            .filter_map(|m| {
                matches!(m.message.content, RoomMessageBody::Private { .. })
                    .then(|| {
                        extract_action_content(&m.message.content, secrets)
                            .map(|a| (m.id(), a.encode()))
                    })
                    .flatten()
            })
            .collect();
        state.rebuild_actions_state_with_decrypted(&decrypted);
        state
    }
    fn shown(
        state: &MessagesV1,
        secrets: &HashMap<u32, [u8; 32]>,
        target: &MessageId,
    ) -> Vec<String> {
        let ranks = reaction_ranks_for(state, secrets);
        order_reactions(
            state.reactions(target).unwrap_or(&HashMap::new()),
            ranks.get(target),
        )
    }
    fn names(es: &[&str]) -> Vec<String> {
        es.iter().map(|e| (*e).to_string()).collect()
    }
    /// The same history with every action sealed under `SECRET` and signed
    /// again by its own fixture author (keys 2..10) at its own time.
    fn seal_actions(public: &MessagesV1) -> Vec<AuthorizedMessageV1> {
        public
            .messages
            .iter()
            .map(|m| {
                let Some(action) = extract_action_content(&m.message.content, &HashMap::new())
                else {
                    return m.clone();
                };
                let author = (2..10)
                    .find(|n| member(*n) == m.message.author)
                    .expect("fixture actions are signed by keys 2..10");
                signed(
                    author,
                    m.message
                        .time
                        .duration_since(UNIX_EPOCH.into())
                        .unwrap()
                        .as_secs(),
                    SealInput::Action(action).seal(&SECRET, SECRET_VERSION),
                )
            })
            .collect()
    }

    #[test]
    fn earliest_standing_interval_orders_and_duplicate_adds_are_idempotent() {
        let target = text();
        let id = target.id();
        let secrets = HashMap::new();
        let mut history = vec![
            target,
            add(2, 1, &id, "z"),
            add(3, 2, &id, "a"),
            add(2, 3, &id, "z"),
        ];
        let state = rebuild(history.clone(), &secrets);
        assert_eq!(shown(&state, &secrets, &id), names(&["z", "a"]));
        assert_eq!(state.reactions(&id).unwrap()["z"], vec![member(2)]);
        history.push(remove(2, 4, &id, "z"));
        history.push(add(2, 5, &id, "z"));
        let state = rebuild(history, &secrets);
        assert_eq!(shown(&state, &secrets, &id), names(&["a", "z"]));
        assert_eq!(state.reactions(&id).unwrap().len(), 2);
    }

    #[test]
    fn leaving_the_earliest_reactor_exposes_the_next_interval_and_preserves_all_memberships() {
        let target = text();
        let id = target.id();
        let secrets = HashMap::new();
        let mut history = vec![
            target,
            add(2, 1, &id, "z"),
            add(3, 2, &id, "a"),
            add(4, 3, &id, "z"),
        ];
        let before = rebuild(history.clone(), &secrets);
        assert_eq!(shown(&before, &secrets, &id), names(&["z", "a"]));
        assert_eq!(
            before.reactions(&id).unwrap()["z"],
            vec![member(2), member(4)]
        );
        history.push(remove(2, 4, &id, "z"));
        // An authorized member can stand on several emojis: do not enforce
        // the sender UI's one-emoji policy in this projection.
        history.push(add(3, 5, &id, "b"));
        let after = rebuild(history, &secrets);
        assert_eq!(shown(&after, &secrets, &id), names(&["a", "z", "b"]));
        let projected = after.reactions(&id).unwrap();
        assert_eq!(projected["z"], vec![member(4)]);
        assert_eq!(projected["a"], vec![member(3)]);
        assert_eq!(projected["b"], vec![member(3)]);
    }

    #[test]
    fn equal_time_adds_tie_by_emoji_between_earlier_and_later_ranks() {
        let target = text();
        let id = target.id();
        let secrets = HashMap::new();
        let state = rebuild(
            vec![
                target,
                add(2, 2, &id, "z"),
                add(3, 2, &id, "a"),
                add(4, 1, &id, "y"),
                add(5, 3, &id, "b"),
            ],
            &secrets,
        );
        // Only "a" and "z" share a time, from different authors, so their
        // order is the emoji tie-break whatever their message ids are.
        assert_eq!(shown(&state, &secrets, &id), names(&["y", "a", "z", "b"]));
    }

    #[test]
    fn same_time_remove_then_add_replays_in_canonical_message_id_order() {
        let target = text();
        let id = target.id();
        let secrets = HashMap::new();
        // An id hashes a deterministic Ed25519 signature, so this bounded
        // search picks the same pair on every run: one member's remove and
        // add of "z" at one time, where the remove's id sorts first.
        let (second, removal, addition) = (1..=64)
            .map(|second| {
                (
                    second,
                    remove(2, second, &id, "z"),
                    add(2, second, &id, "z"),
                )
            })
            .find(|(_, removal, addition)| removal.id() < addition.id())
            .expect("a second in 1..=64 whose remove id sorts before its add id");
        let state = rebuild(
            vec![
                target,
                // Given add first: only the (time, id) sort puts the remove first.
                addition.clone(),
                removal.clone(),
                add(3, second + 1, &id, "a"),
                add(4, second + 2, &id, "b"),
            ],
            &secrets,
        );
        let position = |wanted: &AuthorizedMessageV1| {
            state
                .messages
                .iter()
                .position(|m| m.id() == wanted.id())
                .unwrap()
        };
        assert_eq!(removal.message.time, addition.message.time);
        assert!(
            position(&removal) < position(&addition),
            "premise: the retained history holds the remove before the add"
        );
        assert_eq!(
            state.reactions(&id).unwrap()["z"],
            vec![member(2)],
            "premise: production replays the remove first, so the add stands"
        );
        // The add's interval is the oldest. A replay taking the pair in the
        // other order ends it, leaving "z" unranked and last.
        assert_eq!(shown(&state, &secrets, &id), names(&["z", "a", "b"]));
    }

    #[test]
    fn invalid_targets_and_malformed_actions_gain_no_rank_and_a_deleted_target_loses_its_ranks() {
        let target = text();
        let id = target.id();
        let secrets = HashMap::new();
        let at = |second| SignedTime::new(second, 0).unwrap();
        let first = add(2, 5, &id, "z");
        let absent = MessageId(freenet_scaffold::util::FastHash(88));
        let malformed = ActionContentV1 {
            action_type: ACTION_TYPE_REACTION,
            target: id.clone(),
            payload: vec![255],
        };
        let mut history = vec![
            target,
            // Everything not aimed at `id` reuses the live emojis and authors
            // at other times, so a rank or a removal leaking from an absent
            // target or from an action would change the ranks below.
            add(3, 1, &absent, "a"),
            action(3, 2, malformed),
            first.clone(),
            add(3, 6, &first.id(), "a"),
            add(3, 7, &id, "a"),
            remove(2, 8, &absent, "z"),
        ];
        let live = rebuild(history.clone(), &secrets);
        assert_eq!(
            live.actions_state.reactions.len(),
            1,
            "premise: production projects reactions for the live target only"
        );
        assert_eq!(
            reaction_ranks_for(&live, &secrets),
            HashMap::from([(
                id.clone(),
                HashMap::from([("z".to_string(), at(5)), ("a".to_string(), at(7))]),
            )]),
            "only the live target is ranked, by its own standing adds"
        );
        assert_eq!(shown(&live, &secrets, &id), names(&["z", "a"]));

        history.push(action(1, 9, ActionContentV1::delete(id.clone())));
        let deleted = rebuild(history, &secrets);
        assert!(deleted.is_deleted(&id));
        assert_eq!(
            deleted.reactions(&id),
            live.reactions(&id),
            "premise: the projection keeps a deleted message's reactions, so \
             only the displayable-target gate can drop its ranks"
        );
        assert!(
            reaction_ranks_for(&deleted, &secrets).is_empty(),
            "a deleted message keeps no ranks"
        );
    }

    #[test]
    fn retained_gaps_and_unavailable_secrets_sort_unknown_last_without_inventing_membership() {
        let target = text();
        let id = target.id();
        let secrets = HashMap::new();
        let state = rebuild(
            vec![target, add(2, 2, &id, "z"), add(3, 3, &id, "a")],
            &secrets,
        );
        let mut gap = state.clone();
        // Model a projection held while some retained action bytes cannot be
        // replayed. It is authoritative even when the order cannot be known.
        gap.messages.retain(|m| {
            !m.message.content.is_action()
                || m.message.time != (UNIX_EPOCH + Duration::from_secs(3)).into()
        });
        assert_eq!(shown(&gap, &secrets, &id), names(&["z", "a"]));
        assert_eq!(gap.reactions(&id), state.reactions(&id));
        let mut no_adds = state.clone();
        no_adds.messages.retain(|m| !m.message.content.is_action());
        assert_eq!(shown(&no_adds, &secrets, &id), names(&["a", "z"]));
        let rebuilt = rebuild(no_adds.messages, &secrets);
        assert!(
            shown(&rebuilt, &secrets, &id).is_empty(),
            "retention cannot resurrect absent projection entries"
        );
    }

    #[test]
    fn private_replay_matches_public_and_refreshes_when_secrets_arrive() {
        let target = text();
        let id = target.id();
        let public = rebuild(
            vec![
                target.clone(),
                add(2, 1, &id, "z"),
                add(3, 2, &id, "a"),
                add(2, 3, &id, "z"),
            ],
            &HashMap::new(),
        );
        let sealed = seal_actions(&public);
        let secrets = HashMap::from([(SECRET_VERSION, SECRET)]);
        // Before the secret arrives nothing is projected, so nothing shows.
        let unavailable = rebuild(sealed.clone(), &HashMap::new());
        assert!(shown(&unavailable, &HashMap::new(), &id).is_empty());
        // Once it arrives, the same sealed history matches the public one.
        let private = rebuild(sealed, &secrets);
        assert_eq!(private.reactions(&id), public.reactions(&id));
        assert_eq!(shown(&private, &secrets, &id), names(&["z", "a"]));
        // A projection held while replay lacks the secret keeps its
        // membership, with every rank unknown.
        assert_eq!(shown(&private, &HashMap::new(), &id), names(&["a", "z"]));
    }

    #[test]
    fn no_competing_emojis_need_no_replay() {
        let target = text();
        let id = target.id();
        let state = rebuild(
            vec![target, add(2, 1, &id, "z"), add(3, 2, &id, "z")],
            &HashMap::new(),
        );
        assert!(reaction_ranks_for(&state, &HashMap::new()).is_empty());
        assert_eq!(shown(&state, &HashMap::new(), &id), names(&["z"]));
    }

    #[test]
    fn grouping_carries_standing_order_into_the_rendered_reaction_projection() {
        use super::super::{group_messages, reactions_by_emoji, DisplayItem, MessageClock};
        use crate::components::app::receive_times::ReceiveTimes;
        use crate::util::confusable::ImpersonationChecker;
        let target = text();
        let id = target.id();
        let state = rebuild(
            vec![target, add(2, 1, &id, "z"), add(3, 2, &id, "a")],
            &HashMap::new(),
        );
        let groups = group_messages(
            &state,
            &Default::default(),
            Some(member(2)),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &ImpersonationChecker::default(),
            member(1),
            MessageClock {
                receive_times: &ReceiveTimes::default(),
                fallback_now: chrono::Utc::now(),
            },
        );
        let DisplayItem::Messages(group) = &groups[0] else {
            panic!("message group");
        };
        let message = &group.messages[0];
        assert_eq!(message.reaction_order, names(&["z", "a"]));
        let reactions = reactions_by_emoji(
            &message.reactions,
            &message.reaction_order,
            Some(member(2)),
            &HashMap::new(),
            &ImpersonationChecker::default(),
            member(1),
            &HashMap::new(),
        );
        assert_eq!(
            reactions
                .iter()
                .map(|r| r.emoji.clone())
                .collect::<Vec<_>>(),
            names(&["z", "a"])
        );
    }
}
