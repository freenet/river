//! Banning a member, as the web UI's Ban button does it (`ui/src/components/members/
//! member_info_modal/ban_button.rs`, `ban_gate` in `ui/src/components/members.rs`).
//!
//! **Who may.** The owner, a strict invite-ancestor of the target, or a deputy whose grant covers
//! the target (`MembersV1::is_ban_authorized`), who sits no deeper in the invite tree than the room
//! counts bans from (`BansV1::issuer_within_depth_cap`), and whose ban would not remove themselves
//! (a ban also removes everyone the banned member invited, so banning yourself, or a member you
//! joined the room through, is refused: freenet/river#478).
//!
//! **What it publishes.** One `ChatRoomStateV1Delta`: the signed `UserBan` and, in a private room,
//! the rotated secret in the SAME delta, so the banned member is never in a window where the room
//! is banned but still sealed under the secret they hold. The rotation is built as the web UI's
//! `RoomData::rotate_secret` builds it: the next secret derived from the owner's key
//! (`derive_room_secret`), a signed version record, and a blob of every secret version for the
//! owner and each member who remains after the ban (never the banned member or anyone their ban
//! removes).
//!
//! **Where this differs from the web UI.** Only the owner holds the key a rotation is signed with.
//! The web UI lets a deputy or an ancestor ban in a private room and leaves the rotation to the
//! owner's chat delegate, which runs it when the member set changes. This app does not: a ban in
//! a private room that this computer cannot rotate with is refused, with the reason said, rather
//! than published as if it locked the member out. This is a stopgap until the delegate takes over
//! rotation (freenet/river#921).

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use river_core::room_state::ChatRoomStateV1Delta;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::member::{MemberId, MembersV1};
use river_core::room_state::privacy::{RoomCipherSpec, SecretVersion};
use river_core::room_state::secret::{
    AuthorizedSecretVersionRecord, SecretVersionRecordV1, SecretsDelta,
    build_rotation_encrypted_secrets,
};

use super::outbox::{Blocked, participation};
use super::room::LiveRoom;
use crate::chat::model::{BanOffer, BanPreview};

/// The web UI's `ISSUER_TOO_DEEP_REASON`.
pub const ISSUER_TOO_DEEP: &str = "Bans only take effect from members at most four invites below \
     the room owner (fewer in rooms with a very small member limit). You joined further down, so \
     a ban from you would have no effect. Ask a moderator closer to the owner.";

/// Why a deputy or an ancestor cannot ban in a private room from this app (the web UI can, and
/// leaves the secret rotation to the owner's chat delegate).
pub const PRIVATE_OWNER_ONLY: &str = "In a private room a ban also has to change the room's \
     secret, and only the owner's computer can do that. Ask the owner to ban this member.";

pub const NOT_A_MEMBER: &str = "That member is no longer in the room.";
pub const NOT_KEPT: &str = "The room didn't keep the ban. It may already hold as many bans as it \
     allows, or this ban would not take effect.";
pub const NO_SECRET_VERSION_LEFT: &str =
    "The room's secret has been changed as many times as the room can count.";

/// The web UI's `self_removing_ban_reason`: the reason a ban of `target` by the banner is
/// refused when it would remove the banner (`target` is the banner or one they joined through).
fn self_removing_reason(banner: MemberId, target: MemberId) -> &'static str {
    if banner == target {
        "You can't ban yourself. The ban would remove you, and everyone you invited, from the \
         room."
    } else {
        "You can't ban a member you joined the room through. A ban also removes everyone the \
         banned member invited, so this one would remove you, and everyone you invited, along \
         with them."
    }
}

/// What the Ban gate needs of one viewer, worked out once for however many members it is asked
/// about (a room of N members asks N times on every redraw).
struct Gate<'a> {
    room: &'a LiveRoom,
    owner: MemberId,
    viewer: MemberId,
    by_id: HashMap<MemberId, &'a river_core::room_state::member::AuthorizedMember>,
    /// The viewer and everyone they joined the room through: a ban of any of these removes the
    /// viewer too (the contract's removal set of the target contains the viewer exactly then).
    chain: HashSet<MemberId>,
    too_deep: bool,
}

impl<'a> Gate<'a> {
    fn new(room: &'a LiveRoom, viewer: MemberId) -> Self {
        let state = &room.stored.room_state;
        let owner = MemberId::from(&room.stored.owner_vk);
        let by_id = state.members.members_by_member_id();
        // The chain is walked through the records the contract's removal set uses: the members
        // and the ban evidence (removed members a stored ban still needs). The contract keeps
        // every ancestor of a listed member as a member, so for a listed viewer the evidence
        // adds nothing today; walking it is what keeps the #478 self-removal rule right if a
        // link ever sits only in evidence.
        let pool = state.ban_evidence.lookup(&state.members, owner);
        let mut chain = HashSet::from([viewer]);
        let mut at = pool.get(&viewer).map(|m| m.member.invited_by);
        while let Some(c) = at {
            if !chain.insert(c) {
                break;
            }
            at = pool.get(&c).map(|m| m.member.invited_by);
        }
        let too_deep = !BansV1::issuer_within_depth_cap(
            viewer,
            &state.members,
            owner,
            state.configuration.configuration.max_members,
        );
        Gate {
            room,
            owner,
            viewer,
            by_id,
            chain,
            too_deep,
        }
    }

    fn offer(&self, target: MemberId) -> Option<BanOffer> {
        let state = &self.room.stored.room_state;
        if self.viewer == target || target == self.owner {
            return None;
        }
        if !MembersV1::is_ban_authorized(
            self.viewer,
            target,
            &self.by_id,
            &state.member_info,
            self.owner,
        ) {
            return None;
        }
        if self.too_deep {
            return Some(BanOffer::Withheld(ISSUER_TOO_DEEP.into()));
        }
        if self.chain.contains(&target) {
            return Some(BanOffer::Withheld(
                self_removing_reason(self.viewer, target).into(),
            ));
        }
        if self.room.is_private() && self.viewer != self.owner {
            return Some(BanOffer::Withheld(PRIVATE_OWNER_ONLY.into()));
        }
        Some(BanOffer::Allowed)
    }
}

/// Whether `viewer` is offered Ban for `target`, and if not, whether they are owed a reason
/// (the web UI's `ban_gate`): `None` when nothing is offered or said.
pub fn gate(room: &LiveRoom, viewer: MemberId, target: MemberId) -> Option<BanOffer> {
    Gate::new(room, viewer).offer(target)
}

/// Each member (by `Member::uid`) the viewer is offered Ban for, or told why not.
pub fn offers(room: &LiveRoom) -> HashMap<String, BanOffer> {
    let Some(vk) = room.stored.self_verifying_key() else {
        return HashMap::new();
    };
    let gate = Gate::new(room, MemberId::from(&vk));
    room.stored
        .room_state
        .members
        .members
        .iter()
        .filter_map(|m| {
            let id = m.member.id();
            gate.offer(id).map(|o| (id.to_string(), o))
        })
        .collect()
}

/// What an owner or moderator publishes in one update: a signed ban and/or, in a private room,
/// the rotated secret. A ban in a private room carries both; the owner's "Rotate" has only the
/// rotation.
#[derive(Clone)]
pub struct Moderation {
    pub ban: Option<AuthorizedUserBan>,
    /// `Some` in a private room: the next secret version, for who remains.
    pub secrets: Option<SecretsDelta>,
    /// The version the rotation makes current.
    pub new_version: Option<SecretVersion>,
    /// The names, as the room showed them when this was built, of members whose every ban the
    /// room drops to keep this one. The room keeps at most `max_user_bans` bans and decides
    /// which by authority (`BansV1::enforce_user_ban_cap`: the owner's bans first, up to half
    /// the cap, then by how high in the invite tree each ban's authority sits, newer first
    /// within a level), so at the cap a ban can lift others', and a moderator's ban the room
    /// ranks lowest is not kept at all.
    pub displaced: Vec<String>,
}

impl Moderation {
    /// The update that publishes it: the ban and the rotation together.
    pub fn delta(&self) -> ChatRoomStateV1Delta {
        ChatRoomStateV1Delta {
            bans: self.ban.clone().map(|b| vec![b]),
            secrets: self.secrets.clone(),
            ..Default::default()
        }
    }

    /// Whether `room` shows it done: the member is gone and the room enforces a ban on them,
    /// whoever signed it (an equivalent ban from the owner's other device counts), and (private)
    /// THIS rotation's record is the room's for its version. A rotation by anyone else, even to
    /// the same version, does not count: it may have given the member the new secret before
    /// they were banned, and only a rotation built after the ban leaves them out.
    pub fn held_in(&self, room: &LiveRoom) -> bool {
        let state = &room.stored.room_state;
        self.ban
            .as_ref()
            .is_none_or(|ban| banned_and_gone(room, ban.ban.banned_user))
            && self
                .secrets
                .as_ref()
                .and_then(|s| s.new_versions.first())
                .is_none_or(|ours| {
                    state
                        .secrets
                        .versions
                        .iter()
                        .any(|v| v.owner_signature == ours.owner_signature)
                })
    }
}

/// Whether `target` is no longer listed and the room enforces a ban on them (the contract's own
/// decision, `MembersV1::banned_member_ids`: a ban that is inert, because its issuer is itself
/// removed or the cap refused it, does not count, and neither does a member only pruned).
pub fn banned_and_gone(room: &LiveRoom, target: MemberId) -> bool {
    let state = &room.stored.room_state;
    !state
        .members
        .members
        .iter()
        .any(|m| m.member.id() == target)
        && state
            .members
            .banned_member_ids(
                &state.ban_evidence,
                &state.bans,
                &state.member_info,
                &room.params(),
            )
            .contains(&target)
}

/// Build and sign the ban of `target` by this room's identity, stamped `time`, against `room`
/// as it is now. Refuses what the web UI's Ban button would not offer, and what this app cannot
/// finish (a private room's rotation by anyone but the owner).
pub fn build(room: &LiveRoom, target: MemberId, time: SystemTime) -> Result<Moderation, String> {
    let time = crate::signed_time::whole_millis(time);
    participation(room).map_err(|b| b.reason().to_owned())?;
    let sk = room
        .stored
        .self_sk
        .as_ref()
        .ok_or_else(|| Blocked::NoKey.reason().to_owned())?;
    let me = MemberId::from(&sk.verifying_key());
    let owner = MemberId::from(&room.stored.owner_vk);
    if !room
        .stored
        .room_state
        .members
        .members
        .iter()
        .any(|m| m.member.id() == target)
    {
        return Err(NOT_A_MEMBER.into());
    }
    match gate(room, me, target) {
        Some(BanOffer::Allowed) => {}
        Some(BanOffer::Withheld(why)) => return Err(why),
        None => return Err("You can't ban this member.".into()),
    }
    let ban = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner,
            banned_at: time.into(),
            banned_user: target,
        },
        me,
        sk,
    );
    let mut built = Moderation {
        ban: Some(ban.clone()),
        secrets: None,
        new_version: None,
        displaced: Vec::new(),
    };
    // The room as the ban leaves it: who remains is who gets the new secret.
    let mut after = room.clone();
    after
        .apply_delta(built.delta())
        .map_err(|e| format!("The room refused it: {e}"))?;
    if !after
        .stored
        .room_state
        .bans
        .0
        .iter()
        .any(|b| b.signature == ban.signature)
    {
        return Err(NOT_KEPT.into());
    }
    let me_name = Some(me);
    built.displaced = displaced(room, &after)
        .into_iter()
        .map(|id| room.nickname(id, me_name))
        .collect();
    if room.is_private() {
        let (secrets, version) = rotation(room, &after, sk, time)?;
        built.secrets = Some(secrets);
        built.new_version = Some(version);
    }
    Ok(built)
}

/// The members every one of whose bans `room` holds and `after` no longer does: the ban cap
/// drops them. A member who still has a kept ban is not lifted.
fn displaced(room: &LiveRoom, after: &LiveRoom) -> Vec<MemberId> {
    lost_targets(
        &room.stored.room_state.bans.0,
        &after.stored.room_state.bans.0,
    )
}

fn lost_targets(before: &[AuthorizedUserBan], kept: &[AuthorizedUserBan]) -> Vec<MemberId> {
    let kept_sigs: HashSet<[u8; 64]> = kept.iter().map(|b| b.signature.to_bytes()).collect();
    let still_banned: HashSet<MemberId> = kept.iter().map(|b| b.ban.banned_user).collect();
    let mut out: Vec<MemberId> = Vec::new();
    for b in before {
        let target = b.ban.banned_user;
        if !kept_sigs.contains(&b.signature.to_bytes())
            && !still_banned.contains(&target)
            && !out.contains(&target)
        {
            out.push(target);
        }
    }
    out
}

/// What banning `target` would do to the room's other bans: the real contract apply of a ban
/// from this member on a copy of `room` (which the caller gives with the bans still on their
/// way applied), read the way [`build`] reads it. Nothing is changed; nothing is signed with
/// more than a throwaway time.
pub fn preview(room: &LiveRoom, target: MemberId) -> BanPreview {
    let state = &room.stored.room_state;
    let Some(sk) = room.stored.self_sk.as_ref() else {
        return BanPreview::default();
    };
    // Room for one more ban: nothing is lifted and it is kept.
    if state.bans.0.len() < state.configuration.configuration.max_user_bans {
        return BanPreview::default();
    }
    let me = MemberId::from(&sk.verifying_key());
    let ban = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: MemberId::from(&room.stored.owner_vk),
            banned_at: crate::signed_time::now().into(),
            banned_user: target,
        },
        me,
        sk,
    );
    let signature = ban.signature;
    let mut after = room.clone();
    let applied = after.apply_delta(ChatRoomStateV1Delta {
        bans: Some(vec![ban]),
        ..Default::default()
    });
    let kept = applied.is_ok()
        && after
            .stored
            .room_state
            .bans
            .0
            .iter()
            .any(|b| b.signature == signature);
    if !kept {
        return BanPreview {
            lifts: Vec::new(),
            refused: true,
        };
    }
    BanPreview {
        lifts: displaced(room, &after)
            .into_iter()
            .map(|id| room.nickname(id, Some(me)))
            .collect(),
        refused: false,
    }
}

/// What is said when a ban lifted others': the members and why.
pub fn lifted_sentence(names: &[String], max_bans: usize) -> String {
    format!(
        "This will lift the ban on {} because the room keeps at most {max_bans} bans.",
        names.join(", ")
    )
}

/// What the confirmation says when the room would not keep the ban itself.
pub const REFUSED_SENTENCE: &str = "The room already holds as many bans as it keeps, and they outrank this one, so it would not keep this ban.";

/// What is said once a ban has lifted others'.
pub fn lifted_notice(names: &[String], max_bans: usize) -> String {
    format!(
        "The room keeps at most {max_bans} bans, so the ban on {} was lifted.",
        names.join(", ")
    )
}

/// The next secret version, as the web UI's `RoomData::rotate_secret` builds it, for the owner
/// and every member `after` still lists and no enforced ban removes: the version above the one
/// `room` holds, derived from the owner's key, a signed record, and a blob of every version
/// each of them lacks. `after` is `room` with the ban (if any) applied.
fn rotation(
    room: &LiveRoom,
    after: &LiveRoom,
    sk: &ed25519_dalek::SigningKey,
    time: SystemTime,
) -> Result<(SecretsDelta, SecretVersion), String> {
    let owner = MemberId::from(&room.stored.owner_vk);
    let state = &after.stored.room_state;
    let banned = state.members.banned_member_ids(
        &state.ban_evidence,
        &state.bans,
        &state.member_info,
        &after.params(),
    );
    let remaining: Vec<(MemberId, ed25519_dalek::VerifyingKey)> = state
        .members
        .members
        .iter()
        .map(|m| (m.member.id(), m.member.member_vk))
        .filter(|(id, _)| !banned.contains(id) && *id != owner)
        .collect();
    let current = room.stored.room_state.secrets.current_version;
    let new_version = current
        .checked_add(1)
        .ok_or_else(|| NO_SECRET_VERSION_LEFT.to_owned())?;
    let new_secret = river_core::key_derivation::derive_room_secret(
        &sk.to_bytes(),
        &room.stored.owner_vk,
        new_version,
    );
    let record = AuthorizedSecretVersionRecord::new(
        SecretVersionRecordV1 {
            version: new_version,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: time.into(),
        },
        sk,
    );
    let blobs = build_rotation_encrypted_secrets(
        sk,
        &room.stored.owner_vk,
        owner,
        new_version,
        &new_secret,
        &remaining,
        &room.stored.room_state.secrets.encrypted_secrets,
    )?;
    Ok((
        SecretsDelta {
            current_version: Some(new_version),
            new_versions: vec![record],
            new_encrypted_secrets: blobs,
        },
        new_version,
    ))
}

pub const NOT_PRIVATE: &str = "Only a private room has a secret to rotate.";
pub const NOT_OWNER: &str = "Only the room's owner can rotate its secret.";

/// The owner's "Rotate": the next secret version for everyone the room still holds, in a private
/// room, with no ban (the web UI's Rotate button).
pub fn build_rotation(room: &LiveRoom, time: SystemTime) -> Result<Moderation, String> {
    let time = crate::signed_time::whole_millis(time);
    participation(room).map_err(|b| b.reason().to_owned())?;
    let sk = room
        .stored
        .self_sk
        .as_ref()
        .ok_or_else(|| Blocked::NoKey.reason().to_owned())?;
    if !room.is_private() {
        return Err(NOT_PRIVATE.into());
    }
    if sk.verifying_key() != room.stored.owner_vk {
        return Err(NOT_OWNER.into());
    }
    let (secrets, version) = rotation(room, room, sk, time)?;
    let built = Moderation {
        ban: None,
        secrets: Some(secrets),
        new_version: Some(version),
        displaced: Vec::new(),
    };
    // The contract must take it, as for a ban.
    let mut after = room.clone();
    after
        .apply_delta(built.delta())
        .map_err(|e| format!("The room refused it: {e}"))?;
    if !built.held_in(&after) {
        return Err("The room didn't take the new secret.".into());
    }
    Ok(built)
}

#[cfg(test)]
#[path = "bans_tests.rs"]
pub(in crate::rooms) mod tests;
