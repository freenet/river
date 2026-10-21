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

    /// Drop the record of every recipient that is not a member of the room
    /// in `state` (it left or was pruned, and its DMs went with it), so a
    /// later rejoin does not re-sign every token it ever purged. Only a
    /// state whose configuration the room owner signed is read; a
    /// placeholder prunes nothing. Returns whether anything was dropped (the
    /// caller then persists this record).
    pub fn prune(&mut self, room_owner_vk: &VerifyingKey, state: &ChatRoomStateV1) -> bool {
        if state.configuration.verify_signature(room_owner_vk).is_err() {
            return false;
        }
        let owner = MemberId::from(room_owner_vk);
        let members: std::collections::HashSet<MemberId> = state
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();
        let kept = |r: &MemberId| *r == owner || members.contains(r);
        let before = (self.purged.len(), self.healed_at.len());
        self.purged.retain(|(r, _)| kept(r));
        self.healed_at.retain(|(r, _)| kept(r));
        before != (self.purged.len(), self.healed_at.len())
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
    /// persists this record afterwards). The re-signed envelope holds at most
    /// the tombstone cap; recorded tokens past it are left out. `Some(Err)`
    /// when the envelope cannot be built; the version is recorded then too.
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
        // At most what the envelope still has room for: recorded tokens past
        // the tombstone cap are not all re-signed at once.
        let room = crate::room_state::direct_messages::MAX_PURGED_TOMBSTONES_PER_RECIPIENT
            .saturating_sub(held_tokens.len());
        let missing: Vec<PurgeToken> = self
            .purged
            .iter()
            .filter(|(r, t)| *r == me && !held_tokens.contains(t))
            .map(|(_, t)| *t)
            .take(room)
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

    /// A recipient that is no longer a member of the room (it left or was
    /// pruned, and its DMs went with it) has its record dropped, so a later
    /// rejoin does not re-sign every token it ever purged; a room whose
    /// configuration the owner did not sign (a placeholder) prunes nothing.
    #[test]
    fn the_record_of_a_recipient_who_left_is_dropped() {
        let r = room();
        let stranger = SigningKey::from_bytes(&[14u8; 32]);
        let sid = MemberId::from(&stranger.verifying_key());
        let bob = MemberId::from(&r.bob_sk.verifying_key());
        let mut rec = OwnPurges::default();
        rec.record(sid, [PurgeToken([5; 16])]);
        rec.record(bob, [PurgeToken([6; 16])]);
        assert!(!rec.prune(&r.params.owner, &ChatRoomStateV1::default()));
        assert!(rec.prune(&r.params.owner, &r.state));
        assert_eq!(rec.purged, vec![(bob, PurgeToken([6; 16]))]);
        assert!(
            !rec.prune(&r.params.owner, &r.state),
            "nothing more to drop"
        );
    }

    /// The room owner (never a member record) keeps its record, and the
    /// healed version of a recipient that left is dropped too (and reported
    /// as a change).
    #[test]
    fn a_prune_keeps_the_owner_and_drops_the_healed_version_of_one_who_left() {
        let r = room();
        let owner = MemberId::from(&r.owner_sk.verifying_key());
        let gone = MemberId::from(&SigningKey::from_bytes(&[2; 32]).verifying_key());
        let mut rec = OwnPurges::default();
        rec.record(owner, [PurgeToken([1; 16])]);
        assert!(!rec.prune(&r.params.owner, &r.state), "nothing to drop");
        assert_eq!(rec.purged, vec![(owner, PurgeToken([1; 16]))]);
        rec.healed_at = vec![(gone, 4)];
        assert!(rec.prune(&r.params.owner, &r.state));
        assert!(rec.healed_at.is_empty());
    }

    /// Recorded tokens beyond what one envelope may hold are not re-signed
    /// all at once: the re-signed envelope stays within the tombstone cap.
    #[test]
    fn a_heal_stays_within_the_tombstone_cap() {
        use crate::room_state::direct_messages::MAX_PURGED_TOMBSTONES_PER_RECIPIENT;
        let r = room();
        let bob = MemberId::from(&r.bob_sk.verifying_key());
        let mut rec = OwnPurges::default();
        rec.record(
            bob,
            (0..MAX_PURGED_TOMBSTONES_PER_RECIPIENT as u32 + 50).map(|i| {
                let mut t = [0u8; 16];
                t[..4].copy_from_slice(&i.to_le_bytes());
                PurgeToken(t)
            }),
        );
        let env = rec
            .heal(&r.bob_sk, &r.params.owner, &r.state)
            .expect("missing tokens")
            .expect("within the cap");
        assert_eq!(env.state.purged.len(), MAX_PURGED_TOMBSTONES_PER_RECIPIENT);
    }

    /// The guard is once per winning version, not once ever: a later winning
    /// envelope that omits a recorded token is re-signed again.
    #[test]
    fn a_later_winning_version_that_omits_a_token_is_re_signed_again() {
        let r = room();
        let bob = MemberId::from(&r.bob_sk.verifying_key());
        let (t1, t2, t3) = (
            PurgeToken([1; 16]),
            PurgeToken([2; 16]),
            PurgeToken([3; 16]),
        );
        let mut rec = OwnPurges::default();
        rec.record(bob, [t1]);
        let v1 = advance_recipient_purges(&r.bob_sk, &r.params.owner, None, [t2]).unwrap();
        let s1 = with_envelope(&r, &r.state, v1.clone());
        assert!(rec.heal(&r.bob_sk, &r.params.owner, &s1).is_some());
        assert!(rec.heal(&r.bob_sk, &r.params.owner, &s1).is_none());
        let v2 = advance_recipient_purges(&r.bob_sk, &r.params.owner, Some(&v1), [t3]).unwrap();
        let v3 = advance_recipient_purges(&r.bob_sk, &r.params.owner, Some(&v2), []).unwrap();
        let s3 = with_envelope(&r, &s1, v3);
        assert!(rec.heal(&r.bob_sk, &r.params.owner, &s3).is_some());
    }

    /// The room owner, a member without a record in `members`, re-signs its
    /// own purge too.
    #[test]
    fn the_room_owner_re_signs_its_own_purge() {
        let r = room();
        let owner = MemberId::from(&r.owner_sk.verifying_key());
        let mut rec = OwnPurges::default();
        rec.record(owner, [PurgeToken([1; 16])]);
        let winner =
            advance_recipient_purges(&r.owner_sk, &r.params.owner, None, [PurgeToken([2; 16])])
                .unwrap();
        let s = with_envelope(&r, &r.state, winner);
        assert!(rec.heal(&r.owner_sk, &r.params.owner, &s).is_some());
    }

    /// Two copies of the record merge to the union of the purges and the
    /// later healed version per recipient, in either order; the record stays
    /// sorted and free of duplicates.
    #[test]
    fn a_union_keeps_every_purge_and_the_later_healed_version() {
        let bob = MemberId::from(&SigningKey::from_bytes(&[12u8; 32]).verifying_key());
        for (x, y) in [(5, 3), (3, 5)] {
            let mut a = OwnPurges {
                healed_at: vec![(bob, x)],
                ..Default::default()
            };
            let b = OwnPurges {
                healed_at: vec![(bob, y)],
                ..Default::default()
            };
            a.union(&b);
            assert_eq!(a.healed_at, vec![(bob, 5)]);
        }
        let mut a = OwnPurges::default();
        a.record(bob, [PurgeToken([9; 16]), PurgeToken([1; 16])]);
        a.record(bob, [PurgeToken([9; 16])]);
        assert_eq!(
            a.purged,
            vec![(bob, PurgeToken([1; 16])), (bob, PurgeToken([9; 16]))]
        );
        let mut b = OwnPurges::default();
        b.record(bob, [PurgeToken([5; 16]), PurgeToken([9; 16])]);
        a.union(&b);
        assert_eq!(
            a.purged,
            vec![
                (bob, PurgeToken([1; 16])),
                (bob, PurgeToken([5; 16])),
                (bob, PurgeToken([9; 16]))
            ]
        );
    }

    /// A winning envelope already at the tombstone cap leaves no room: nothing
    /// is re-signed.
    #[test]
    fn a_full_envelope_is_not_re_signed() {
        use crate::room_state::direct_messages::MAX_PURGED_TOMBSTONES_PER_RECIPIENT;
        let r = room();
        let bob = MemberId::from(&r.bob_sk.verifying_key());
        let full: Vec<PurgeToken> = (0..MAX_PURGED_TOMBSTONES_PER_RECIPIENT as u32)
            .map(|i| {
                let mut t = [0u8; 16];
                t[..4].copy_from_slice(&i.to_le_bytes());
                PurgeToken(t)
            })
            .collect();
        let winner = advance_recipient_purges(&r.bob_sk, &r.params.owner, None, full).unwrap();
        let s = with_envelope(&r, &r.state, winner);
        let mut rec = OwnPurges::default();
        rec.record(bob, [PurgeToken([0xEE; 16])]);
        assert!(rec.heal(&r.bob_sk, &r.params.owner, &s).is_none());
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
