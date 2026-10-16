//! A client's record of the DM purges it signed, and the purge envelope it
//! re-signs when the room's winning envelope for its own recipient id omits
//! one of them.
//!
//! A member's purge envelope is one record per recipient, resolved by rank
//! (the higher version, then the greater signature digest). Two devices of
//! one member that each purge at the same version both sign a version-`v`
//! envelope; one wins, and the other's tokens are no longer tombstoned. The
//! device whose tokens are missing re-signs, at the next version, the union
//! of the winning envelope and the purges it recorded
//! ([`OwnPurges::heal`], built with
//! [`advance_recipient_purges`](crate::room_state::direct_messages::advance_recipient_purges)
//! so the UI and riverctl sign byte-identical envelopes).
//!
//! It re-signs only when a recorded token is missing, and at most once per
//! winning version it observes, so two devices never trade envelopes
//! without end.

use crate::room_state::direct_messages::{
    advance_recipient_purges, AuthorizedRecipientPurges, PurgeToken,
};
use crate::room_state::member::MemberId;
use crate::room_state::ChatRoomStateV1;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// The DM purges a client signed in one room, per recipient id it signed
/// them as, and the winning envelope version it last re-signed against.
///
/// Both lists are kept sorted and free of duplicates. Vectors of pairs,
/// not maps, so the record also serializes as JSON (riverctl's store).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnPurges {
    /// `(recipient, token)` for every DM this client purged.
    #[serde(default)]
    pub purged: Vec<(MemberId, PurgeToken)>,
    /// `(recipient, version)`: the version of the winning envelope this
    /// client last re-signed against for that recipient.
    #[serde(default)]
    pub healed_at: Vec<(MemberId, u64)>,
}

impl OwnPurges {
    /// Record that this client purged `tokens` as `recipient`.
    pub fn record(&mut self, recipient: MemberId, tokens: impl IntoIterator<Item = PurgeToken>) {
        self.purged
            .extend(tokens.into_iter().map(|t| (recipient, t)));
        self.purged.sort();
        self.purged.dedup();
    }

    /// Fold in another copy of this record (another tab's, or an older
    /// saved copy): the union of the purges, and the later healed version
    /// per recipient.
    pub fn union(&mut self, other: &OwnPurges) {
        self.purged.extend(other.purged.iter().copied());
        self.purged.sort();
        self.purged.dedup();
        for &(recipient, version) in &other.healed_at {
            self.set_healed_at_max(recipient, version);
        }
    }

    fn healed_at(&self, recipient: MemberId) -> Option<u64> {
        self.healed_at
            .iter()
            .find(|(r, _)| *r == recipient)
            .map(|(_, v)| *v)
    }

    fn set_healed_at_max(&mut self, recipient: MemberId, version: u64) {
        match self.healed_at.iter_mut().find(|(r, _)| *r == recipient) {
            Some(entry) => entry.1 = entry.1.max(version),
            None => {
                self.healed_at.push((recipient, version));
                self.healed_at.sort();
            }
        }
    }

    /// The envelope to publish when `state`'s winning purge envelope for
    /// `recipient_sk`'s id omits a token this client recorded as that
    /// recipient: the winning envelope's tokens plus the missing ones, at the
    /// next version. `None` when nothing is missing, when the recipient is
    /// not a member of the room, or when this client already re-signed
    /// against the winning envelope's version (recorded here, so the caller
    /// persists this record afterwards). `Some(Err)` when the envelope
    /// cannot be built (the tombstone cap); the version is recorded then too.
    pub fn heal(
        &mut self,
        recipient_sk: &SigningKey,
        room_owner_vk: &VerifyingKey,
        state: &ChatRoomStateV1,
    ) -> Option<Result<AuthorizedRecipientPurges, String>> {
        let me = MemberId::from(&recipient_sk.verifying_key());
        let is_member = me == MemberId::from(room_owner_vk)
            || state.members.members.iter().any(|m| m.member.id() == me);
        if !is_member {
            return None;
        }
        let held = state
            .direct_messages
            .purges
            .iter()
            .find(|p| p.recipient_id == me);
        let held_tokens: &[PurgeToken] = held.map_or(&[], |p| &p.state.purged);
        let missing: Vec<PurgeToken> = self
            .purged
            .iter()
            .filter(|(r, t)| *r == me && !held_tokens.contains(t))
            .map(|(_, t)| *t)
            .collect();
        if missing.is_empty() {
            return None;
        }
        let observed = held.map_or(0, |p| p.state.version);
        if self.healed_at(me) == Some(observed) {
            return None;
        }
        self.set_healed_at_max(me, observed);
        Some(advance_recipient_purges(
            recipient_sk,
            room_owner_vk,
            held,
            missing,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::direct_messages::{advance_recipient_purges, DirectMessagesDelta};
    use crate::room_state::member::{AuthorizedMember, Member, MembersV1};
    use crate::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
    use freenet_scaffold::ComposableState;

    struct Room {
        owner_sk: SigningKey,
        bob_sk: SigningKey,
        params: ChatRoomParametersV1,
        state: ChatRoomStateV1,
    }

    fn room() -> Room {
        let owner_sk = SigningKey::from_bytes(&[11u8; 32]);
        let bob_sk = SigningKey::from_bytes(&[12u8; 32]);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let bob = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: bob_sk.verifying_key(),
            },
            &owner_sk,
        );
        let mut state = ChatRoomStateV1::default();
        state.configuration.configuration.owner_member_id = owner_id;
        state.configuration = crate::room_state::configuration::AuthorizedConfigurationV1::new(
            state.configuration.configuration.clone(),
            &owner_sk,
        );
        state.members = MembersV1 { members: vec![bob] };
        Room {
            params: ChatRoomParametersV1 {
                owner: owner_sk.verifying_key(),
            },
            owner_sk,
            bob_sk,
            state,
        }
    }

    fn with_envelope(
        r: &Room,
        base: &ChatRoomStateV1,
        env: AuthorizedRecipientPurges,
    ) -> ChatRoomStateV1 {
        let mut s = base.clone();
        s.apply_delta(
            base,
            &r.params,
            &Some(ChatRoomStateV1Delta {
                direct_messages: Some(DirectMessagesDelta {
                    new_messages: vec![],
                    advanced_purges: vec![env],
                }),
                ..Default::default()
            }),
        )
        .expect("apply envelope");
        s
    }

    fn merged(r: &Room, a: &ChatRoomStateV1, b: &ChatRoomStateV1) -> ChatRoomStateV1 {
        let mut m = a.clone();
        m.merge(a, &r.params, b).expect("merge");
        m
    }

    fn tombstones(s: &ChatRoomStateV1, who: MemberId) -> Vec<PurgeToken> {
        s.direct_messages
            .purges
            .iter()
            .find(|p| p.recipient_id == who)
            .map(|p| p.state.purged.clone())
            .unwrap_or_default()
    }

    /// Two devices of one member purge different DMs at the same version;
    /// one envelope wins. The device whose purge lost re-signs the union on
    /// its next sync, and both devices converge with both DMs tombstoned.
    #[test]
    fn the_losing_device_re_signs_the_union_and_both_converge() {
        let r = room();
        let bob = MemberId::from(&r.bob_sk.verifying_key());
        let (t1, t2) = (PurgeToken([1; 16]), PurgeToken([2; 16]));
        let env_x = advance_recipient_purges(&r.bob_sk, &r.params.owner, None, [t1]).unwrap();
        let env_y = advance_recipient_purges(&r.bob_sk, &r.params.owner, None, [t2]).unwrap();
        let (mut rec_x, mut rec_y) = (OwnPurges::default(), OwnPurges::default());
        rec_x.record(bob, [t1]);
        rec_y.record(bob, [t2]);
        let x = with_envelope(&r, &r.state, env_x);
        let y = with_envelope(&r, &r.state, env_y);

        // Each device's next sync merges the other's state.
        let x_after = merged(&r, &x, &y);
        let y_after = merged(&r, &y, &x);
        assert_eq!(tombstones(&x_after, bob), tombstones(&y_after, bob));
        let winner = tombstones(&x_after, bob);
        assert_eq!(winner.len(), 1, "premise: one envelope won");
        let (mut loser_rec, loser_state, mut winner_rec, winner_state) = if winner == vec![t1] {
            (rec_y, y_after, rec_x, x_after)
        } else {
            (rec_x, x_after, rec_y, y_after)
        };

        assert!(winner_rec
            .heal(&r.bob_sk, &r.params.owner, &winner_state)
            .is_none());
        let heal = loser_rec
            .heal(&r.bob_sk, &r.params.owner, &loser_state)
            .expect("a recorded token is missing")
            .expect("envelope");
        assert_eq!(heal.state.version, 2);
        let loser_healed = with_envelope(&r, &loser_state, heal);
        let converged_l = merged(&r, &loser_healed, &winner_state);
        let converged_w = merged(&r, &winner_state, &loser_healed);
        assert_eq!(tombstones(&converged_l, bob), vec![t1, t2]);
        assert_eq!(tombstones(&converged_w, bob), vec![t1, t2]);
        assert!(loser_rec
            .heal(&r.bob_sk, &r.params.owner, &converged_l)
            .is_none());
        assert!(winner_rec
            .heal(&r.bob_sk, &r.params.owner, &converged_w)
            .is_none());
    }

    /// No loop: nothing is re-signed while every recorded token is held, and
    /// at most once per winning version observed, even if the re-signed
    /// envelope never lands.
    #[test]
    fn a_heal_is_signed_at_most_once_per_observed_version() {
        let r = room();
        let bob = MemberId::from(&r.bob_sk.verifying_key());
        let (t1, t2) = (PurgeToken([1; 16]), PurgeToken([2; 16]));
        let mut rec = OwnPurges::default();
        rec.record(bob, [t1]);
        let held = advance_recipient_purges(&r.bob_sk, &r.params.owner, None, [t1]).unwrap();
        let state = with_envelope(&r, &r.state, held);
        assert!(
            rec.heal(&r.bob_sk, &r.params.owner, &state).is_none(),
            "nothing missing"
        );

        let other = advance_recipient_purges(&r.bob_sk, &r.params.owner, None, [t2]).unwrap();
        let lost = with_envelope(&r, &r.state, other);
        assert!(rec.heal(&r.bob_sk, &r.params.owner, &lost).is_some());
        assert!(
            rec.heal(&r.bob_sk, &r.params.owner, &lost).is_none(),
            "not again at the same observed version"
        );
        // Another copy of the record (another tab) carries the healed version.
        let mut tab = OwnPurges::default();
        tab.record(bob, [t1]);
        tab.union(&rec);
        assert!(tab.heal(&r.bob_sk, &r.params.owner, &lost).is_none());
    }

    /// Only the recipient a token was recorded as re-signs it, and only
    /// while a member of the room.
    #[test]
    fn a_heal_is_signed_only_as_the_recorded_recipient_and_member() {
        let r = room();
        let bob = MemberId::from(&r.bob_sk.verifying_key());
        let mut rec = OwnPurges::default();
        rec.record(bob, [PurgeToken([3; 16])]);
        assert!(rec.heal(&r.owner_sk, &r.params.owner, &r.state).is_none());
        let stranger = SigningKey::from_bytes(&[13u8; 32]);
        let mut rec2 = OwnPurges::default();
        rec2.record(
            MemberId::from(&stranger.verifying_key()),
            [PurgeToken([3; 16])],
        );
        assert!(rec2.heal(&stranger, &r.params.owner, &r.state).is_none());
        assert!(rec.heal(&r.bob_sk, &r.params.owner, &r.state).is_some());
    }
}
