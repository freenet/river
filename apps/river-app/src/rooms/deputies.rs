//! Which members show a deputy shield in one viewer's view, read from the room state exactly as
//! the web UI does (`deputy_badges_for_viewer` in `ui/src/components/members.rs`).
//!
//! A grant is a member's own signed record naming a deputy. The web UI reads only the canonical
//! record of each member, ignores a member naming themselves, never badges the owner, and shows
//! a badge only when an appointer matters to the viewer: the owner, the viewer, or someone who
//! invited the viewer (whose deputy could get the viewer banned). Everything else is not shown.
//!
//! Grant signatures are not re-verified here, as in the web UI: the stored room state is trusted
//! to have passed the contract's `verify`.

use std::collections::{HashMap, HashSet};

use river_core::room_state::member::{AuthorizedMember, MemberId, MembersV1};
use river_core::room_state::member_info::MemberInfoV1;

/// One deputy as the viewer sees them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Badge {
    /// Never empty. In the order the web UI lists them: by member id.
    pub deputized_by: Vec<MemberId>,
    /// Whether they can get the viewer removed. `None` for the viewer themselves.
    pub can_ban_viewer: Option<bool>,
}

/// Every member who shows a shield to `viewer`.
pub fn badges(
    members: &MembersV1,
    member_info: &MemberInfoV1,
    owner: MemberId,
    viewer: MemberId,
) -> HashMap<MemberId, Badge> {
    let deputizers_of = deputizers_of(member_info);
    let relevant = viewer_relevant(members, owner, viewer);
    let by_id = members.members_by_member_id();
    deputizers_of
        .keys()
        .filter_map(|&target| {
            badge(
                &by_id,
                member_info,
                &deputizers_of,
                &relevant,
                owner,
                viewer,
                target,
            )
            .map(|b| (target, b))
        })
        .collect()
}

/// For each deputy, who has made them one: from each member's canonical record, never a member
/// naming themselves, no repeats, appointers in id order.
fn deputizers_of(member_info: &MemberInfoV1) -> HashMap<MemberId, Vec<MemberId>> {
    let mut appointers: Vec<MemberId> = member_info
        .member_info
        .iter()
        .map(|i| i.member_info.member_id)
        .collect();
    appointers.sort_unstable();
    appointers.dedup();
    let mut map: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
    for appointer in appointers {
        let Some(record) = member_info.canonical(appointer) else {
            continue;
        };
        for deputy in &record.member_info.deputies {
            if *deputy == appointer {
                continue;
            }
            let entry = map.entry(*deputy).or_default();
            if !entry.contains(&appointer) {
                entry.push(appointer);
            }
        }
    }
    map
}

/// The owner (for everyone but the owner), the viewer's inviters up to the owner, and the viewer.
fn viewer_relevant(members: &MembersV1, owner: MemberId, viewer: MemberId) -> HashSet<MemberId> {
    let mut set = HashSet::new();
    if viewer != owner {
        set.insert(owner);
    }
    let invited_by: HashMap<MemberId, MemberId> = members
        .members
        .iter()
        .map(|m| (m.member.id(), m.member.invited_by))
        .collect();
    let mut seen = HashSet::from([viewer]);
    let mut at = invited_by.get(&viewer).copied();
    while let Some(c) = at {
        if !seen.insert(c) {
            break;
        }
        set.insert(c);
        if c == owner {
            break;
        }
        at = invited_by.get(&c).copied();
    }
    set.insert(viewer);
    set
}

fn badge(
    by_id: &HashMap<MemberId, &AuthorizedMember>,
    member_info: &MemberInfoV1,
    deputizers_of: &HashMap<MemberId, Vec<MemberId>>,
    relevant: &HashSet<MemberId>,
    owner: MemberId,
    viewer: MemberId,
    target: MemberId,
) -> Option<Badge> {
    // The owner's authority is not delegated, and nothing stops a member listing the owner.
    if target == owner {
        return None;
    }
    let deputized_by: Vec<MemberId> = deputizers_of
        .get(&target)?
        .iter()
        .copied()
        .filter(|a| relevant.contains(a))
        .collect();
    if deputized_by.is_empty() {
        return None;
    }
    // A ban reaches the banned member's whole invite subtree, so what can remove the viewer is
    // whatever can ban the viewer or anyone above them.
    let can_ban_viewer = (target != viewer).then(|| {
        relevant
            .iter()
            .any(|&victim| MembersV1::is_ban_authorized(target, victim, by_id, member_info, owner))
    });
    Some(Badge {
        deputized_by,
        can_ban_viewer,
    })
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;
    use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use river_core::room_state::privacy::SealedBytes;

    use crate::chat::model::{Appointer, Role, Room};
    use crate::rooms::room::LiveRoom;
    use crate::rooms::room_tests::Fixture;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    fn id(k: &SigningKey) -> MemberId {
        MemberId::from(&k.verifying_key())
    }

    /// `who`'s own record, at `version`, naming `deputies`.
    fn record(
        who: &SigningKey,
        name: &str,
        version: u32,
        deputies: &[&SigningKey],
    ) -> AuthorizedMemberInfo {
        AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: id(who),
                version,
                preferred_nickname: SealedBytes::public(name.as_bytes().to_vec()),
                deputies: deputies.iter().map(|d| id(d)).collect(),
            },
            who,
        )
    }

    /// A public room: owner Olive, you (Mia, key 2) invited by her, Dee (3) invited by Olive,
    /// Eve (4) invited by Olive, and Finn (5) invited by you. `grants` are extra records.
    fn view(grants: Vec<AuthorizedMemberInfo>, with_identity: bool) -> Room {
        view_with(None, &[], grants, with_identity)
    }

    /// [`view`], with you invited by `me_by` instead of the owner when given, and `extra`
    /// (member, inviter) pairs added, each with a nickname of their own (their key's number).
    fn view_with(
        me_by: Option<&SigningKey>,
        extra: &[(&SigningKey, &SigningKey)],
        grants: Vec<AuthorizedMemberInfo>,
        with_identity: bool,
    ) -> Room {
        let f = Fixture::new();
        let mut state = f.private_state();
        state.members.members.clear();
        state.member_info.member_info.clear();
        let (dee, eve, finn) = (key(3), key(4), key(5));
        let mut pairs = vec![
            (&f.me, me_by.unwrap_or(&f.owner)),
            (&dee, &f.owner),
            (&eve, &f.owner),
            (&finn, &f.me),
        ];
        pairs.extend_from_slice(extra);
        for (k, by) in pairs {
            state.members.members.push(AuthorizedMember::new(
                Member {
                    owner_member_id: f.owner_id(),
                    invited_by: id(by),
                    member_vk: k.verifying_key(),
                },
                by,
            ));
        }
        for (k, name) in [
            (&f.me, "Mia"),
            (&dee, "Dee"),
            (&eve, "Eve"),
            (&finn, "Finn"),
        ] {
            state.member_info.member_info.push(record(k, name, 0, &[]));
        }
        for (k, _) in extra {
            let name = format!("Extra{}", k.to_bytes()[0]);
            state.member_info.member_info.push(record(k, &name, 0, &[]));
        }
        state.member_info.member_info.extend(grants);
        let mut stored = f.stored(state);
        if !with_identity {
            stored.self_sk = None;
        }
        LiveRoom::new(stored).view(&|_| 0)
    }

    fn named<'a>(room: &'a Room, name: &str) -> &'a crate::chat::model::Member {
        room.members.iter().find(|m| m.nickname == name).unwrap()
    }

    #[test]
    fn a_deputy_of_the_owner_shows_as_one_and_can_ban_you() {
        let f = Fixture::new();
        let room = view(vec![record(&f.owner, "Olive", 1, &[&key(3)])], true);
        let dee = named(&room, "Dee");
        assert_eq!(dee.role, Role::Deputy);
        assert_eq!(dee.deputized_by, [Appointer::Owner]);
        assert_eq!(dee.can_ban_you, Some(true));
        assert_eq!(
            dee.deputy_tooltip().unwrap(),
            "Deputy (appointed by the room owner). Can ban you."
        );
        // Nobody else changes: the owner stays the owner, the rest are plain members.
        assert_eq!(named(&room, "Olive").role, Role::Owner);
        assert_eq!(named(&room, "Eve").role, Role::Member);
        assert!(named(&room, "Eve").deputy_tooltip().is_none());
    }

    #[test]
    fn a_grant_that_was_revoked_does_not_show() {
        let f = Fixture::new();
        // Version 1 grants, version 2 (the canonical one) takes it back.
        let room = view(
            vec![
                record(&f.owner, "Olive", 1, &[&key(3)]),
                record(&f.owner, "Olive", 2, &[]),
            ],
            true,
        );
        assert_eq!(named(&room, "Dee").role, Role::Member);
        assert!(named(&room, "Dee").deputized_by.is_empty());
    }

    #[test]
    fn grants_that_do_not_count_in_your_view_do_not_show() {
        let f = Fixture::new();
        let room = view(
            vec![
                // Eve is neither you, the owner, nor someone who invited you.
                record(&key(4), "Eve", 1, &[&key(3)]),
                // Finn names himself, which is no grant.
                record(&key(5), "Finn", 1, &[&key(5)]),
                // Naming the owner paints nothing on the owner.
                record(&key(3), "Dee", 1, &[&f.owner]),
            ],
            true,
        );
        for m in &room.members {
            assert!(m.deputized_by.is_empty(), "{}", m.nickname);
            assert_ne!(m.role, Role::Deputy, "{}", m.nickname);
        }
    }

    #[test]
    fn a_deputy_you_appointed_shows_but_cannot_ban_you() {
        let f = Fixture::new();
        let room = view(vec![record(&f.me, "Mia", 1, &[&key(5)])], true);
        let finn = named(&room, "Finn");
        assert_eq!(finn.role, Role::Deputy);
        assert_eq!(finn.deputized_by, [Appointer::You]);
        assert_eq!(finn.can_ban_you, Some(false));
        assert_eq!(
            finn.deputy_tooltip().unwrap(),
            "Deputy (appointed by you). Cannot ban you."
        );
    }

    #[test]
    fn nobody_is_a_deputy_without_a_local_identity() {
        let f = Fixture::new();
        let room = view(vec![record(&f.owner, "Olive", 1, &[&key(3)])], false);
        assert!(room.members.iter().all(|m| m.role != Role::Deputy));
    }

    #[test]
    fn a_grant_by_someone_who_is_not_a_member_does_not_show() {
        // Key 6 has a record but is not in the members list, so is nobody's ancestor.
        let room = view(vec![record(&key(6), "Ghost", 1, &[&key(3)])], true);
        assert_eq!(named(&room, "Dee").role, Role::Member);
    }

    #[test]
    fn a_deputy_of_someone_who_invited_you_shows_and_can_ban_you() {
        let gus = key(6);
        let f = Fixture::new();
        // Gus invited you, so his deputy could get you banned; Dee is invited by the owner.
        let room = view_with(
            Some(&gus),
            &[(&gus, &f.owner)],
            vec![record(&gus, "Gus", 1, &[&key(3)])],
            true,
        );
        let dee = named(&room, "Dee");
        assert_eq!(dee.role, Role::Deputy);
        let gus_id = room
            .members
            .iter()
            .find(|m| m.nickname == "Gus")
            .unwrap()
            .id;
        assert_eq!(dee.deputized_by, [Appointer::Member(gus_id)]);
        // is_ban_authorized: a deputy of Gus covers Gus's subtree, which holds you.
        assert_eq!(dee.can_ban_you, Some(true));
        assert_eq!(
            dee.deputy_tooltip().unwrap(),
            "Deputy (appointed by another member). Can ban you."
        );
    }

    #[test]
    fn an_invite_cycle_ends() {
        let (gus, hal, f) = (key(6), key(7), Fixture::new());
        // You were invited by Gus; Gus and Hal each claim the other invited them.
        let room = view_with(
            Some(&gus),
            &[(&gus, &hal), (&hal, &gus)],
            vec![
                record(&gus, "Gus", 1, &[&key(3)]),
                record(&hal, "Hal", 1, &[&key(5)]),
            ],
            true,
        );
        assert_eq!(named(&room, "Dee").role, Role::Deputy);
        assert_eq!(named(&room, "Finn").role, Role::Deputy);
        let _ = f;
    }

    #[test]
    fn two_records_at_one_version_are_settled_by_the_rooms_canonical_choice() {
        use river_core::room_state::member_info::MemberInfoV1;
        let f = Fixture::new();
        let grant = record(&f.owner, "Olive", 1, &[&key(3)]);
        let revoke = record(&f.owner, "Olive", 1, &[]);
        let mut info = MemberInfoV1::default();
        info.member_info.push(grant.clone());
        info.member_info.push(revoke.clone());
        // Whichever the room's own selector keeps, in either arrival order.
        let granted = !info.deputies_of(f.owner_id()).is_empty();
        for pair in [vec![grant.clone(), revoke.clone()], vec![revoke, grant]] {
            let room = view(pair, true);
            let role = if granted { Role::Deputy } else { Role::Member };
            assert_eq!(named(&room, "Dee").role, role);
        }
    }
}
