//! Banning a newcomer (a provisional member) from a message's menu.
//!
//! A newcomer who was admitted by proof of work, and has not been invited,
//! has no member record, so the member panel cannot ban them. Their messages
//! are the only handle, which is why the ban lives on the message menu (the
//! desktop action cluster and the mobile sheet).
//!
//! Who may issue the ban is decided by the contract, in `provisional_step`
//! (P1, `common/src/room_state/provisional.rs`): the owner signs with the
//! owner key, and an owner deputy signs with the key of its member record.
//! [`can_ban_newcomer`] mirrors that rule to decide what to offer, and
//! [`ban_verifies`] calls the contract's own signature check on the ban that
//! was built, so the two cannot drift unnoticed.

use crate::components::app::{mark_needs_sync, ROOMS};
use crate::components::toast::show_toast;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::member::MemberId;
use river_core::room_state::provisional::{
    AuthorizedProvisionalBan, ProvisionalBan, ProvisionalDelta,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::SystemTime;

/// The newcomer a ban is being confirmed for: the room, the target, and the
/// name shown in the dialog. Set by the menu, read by [`NewcomerBanDialog`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingNewcomerBan {
    pub(crate) room: VerifyingKey,
    pub(crate) target: MemberId,
    pub(crate) name: String,
}

pub(crate) static PENDING_NEWCOMER_BAN: GlobalSignal<Option<PendingNewcomerBan>> =
    Global::new(|| None);

/// Ask for confirmation before banning `target`.
pub(crate) fn request_ban(room: VerifyingKey, target: MemberId, name: String) {
    crate::util::defer(move || {
        *PENDING_NEWCOMER_BAN.write() = Some(PendingNewcomerBan { room, target, name });
    });
}

/// Whether `issuer_vk` is a key the contract accepts as a provisional-ban
/// issuer in `state`: the owner's key, or the key of an owner deputy's member
/// record.
pub(crate) fn is_ban_issuer(
    state: &ChatRoomStateV1,
    owner_vk: &VerifyingKey,
    issuer_vk: &VerifyingKey,
) -> bool {
    let owner_id = MemberId::from(owner_vk);
    let issuer_id = MemberId::from(issuer_vk);
    if issuer_id == owner_id {
        return issuer_vk == owner_vk;
    }
    state
        .members
        .members_by_member_id()
        .get(&issuer_id)
        .is_some_and(|record| {
            record.member.member_vk == *issuer_vk
                && state.member_info.deputies_of(owner_id).contains(&issuer_id)
        })
}

/// Whether the menu offers "Ban newcomer" on a message by `author`: the
/// reader may issue the ban, and the author is a provisional member (an entry,
/// no member record) who is not the reader, the owner, or already banned.
pub(crate) fn can_ban_newcomer(
    state: &ChatRoomStateV1,
    owner_vk: &VerifyingKey,
    reader_vk: Option<&VerifyingKey>,
    author: MemberId,
) -> bool {
    let Some(reader_vk) = reader_vk else {
        return false;
    };
    if !is_ban_issuer(state, owner_vk, reader_vk)
        || author == MemberId::from(reader_vk)
        || author == MemberId::from(owner_vk)
    {
        return false;
    }
    if state.members.members_by_member_id().contains_key(&author) {
        return false;
    }
    let is_provisional = state
        .provisional
        .entries
        .iter()
        .any(|e| MemberId::from(&e.provisional.member_vk) == author);
    let already_banned = state
        .provisional
        .bans
        .iter()
        .any(|b| b.ban.target == author);
    is_provisional && !already_banned
}

/// The signed ban of `target` by the holder of `signing_key`.
pub(crate) fn build_ban(
    owner_vk: &VerifyingKey,
    signing_key: &SigningKey,
    target: MemberId,
    now: SystemTime,
) -> AuthorizedProvisionalBan {
    AuthorizedProvisionalBan::new(
        owner_vk,
        ProvisionalBan {
            banned_by: MemberId::from(&signing_key.verifying_key()),
            banned_at: now,
            target,
        },
        signing_key,
    )
}

/// The contract's own check of `ban`'s signature against the issuer's key.
#[cfg(test)]
pub(crate) fn ban_verifies(
    ban: &AuthorizedProvisionalBan,
    owner_vk: &VerifyingKey,
    issuer_vk: &VerifyingKey,
) -> bool {
    ban.verify_signature(owner_vk, issuer_vk).is_ok()
}

/// What happened to a ban once it was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BanOutcome {
    /// The merged state holds the ban.
    Stands,
    /// The merged state does not hold it (for example, cut by the ban cap).
    NotKept,
}

/// Whether `ban` is in the merged `state`.
pub(crate) fn outcome(state: &ChatRoomStateV1, ban: &AuthorizedProvisionalBan) -> BanOutcome {
    if state.provisional.bans.iter().any(|b| b.ban == ban.ban) {
        BanOutcome::Stands
    } else {
        BanOutcome::NotKept
    }
}

/// The text shown for each outcome.
pub(crate) fn outcome_message(name: &str, outcome: BanOutcome) -> String {
    match outcome {
        BanOutcome::Stands => format!("Banned newcomer {name}."),
        BanOutcome::NotKept => format!(
            "The ban of {name} was not kept. The room holds a limited number of newcomer \
             bans, and this one was not among them."
        ),
    }
}

/// Sign and apply the ban, tell the user what came of it, and sync it.
fn execute(pending: PendingNewcomerBan) {
    let PendingNewcomerBan { room, target, name } = pending;
    crate::util::defer(move || {
        let mut result: Option<BanOutcome> = None;
        ROOMS.with_mut(|rooms| {
            let Some(room_data) = rooms.map.get_mut(&room) else {
                return;
            };
            let Some(sk) = room_data.signing_key().cloned() else {
                warn!("Cannot ban newcomer: no local signing key for this room");
                return;
            };
            // The action boundary, as `BanButton` has: the menu hides the
            // item, but this function trusts its arguments, so it checks again.
            if !can_ban_newcomer(
                &room_data.room_state,
                &room,
                Some(&sk.verifying_key()),
                target,
            ) {
                warn!("Refusing a newcomer ban the room would not accept");
                return;
            }
            let ban = build_ban(&room, &sk, target, crate::util::get_current_system_time());
            let before = room_data.room_state.clone();
            let delta = ChatRoomStateV1Delta {
                provisional: Some(ProvisionalDelta {
                    entries: vec![],
                    bans: vec![ban.clone()],
                }),
                ..Default::default()
            };
            match room_data.room_state.apply_delta(
                &before,
                &ChatRoomParametersV1 { owner: room },
                &Some(delta),
            ) {
                Ok(()) => {
                    result = Some(outcome(&room_data.room_state, &ban));
                    info!("Applied newcomer ban for {:?}: {:?}", target, result);
                }
                Err(e) => error!("Failed to apply newcomer ban: {:?}", e),
            }
        });
        match result {
            Some(o) => {
                show_toast(outcome_message(&name, o), None);
                if o == BanOutcome::Stands {
                    mark_needs_sync(room);
                }
            }
            None => show_toast(format!("Could not ban {name}."), None),
        }
    });
}

/// The confirmation dialog for [`request_ban`], rendered once by the
/// conversation.
#[component]
pub(crate) fn NewcomerBanDialog() -> Element {
    let pending = PENDING_NEWCOMER_BAN.read().clone();
    let Some(p) = pending else {
        return rsx! {};
    };
    let name = p.name.clone();
    let cancel = move || crate::util::defer(move || *PENDING_NEWCOMER_BAN.write() = None);
    rsx! {
        div {
            "data-testid": "newcomer-ban-dialog",
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: move |_| cancel(),
            div {
                class: "bg-panel rounded-xl shadow-xl border border-border p-6 max-w-sm mx-4",
                onclick: move |e| e.stop_propagation(),
                h3 { class: "text-[16px] font-semibold text-text mb-2", "Ban newcomer?" }
                p { class: "text-text-muted text-[13px] mb-4",
                    "Ban "
                    span { class: "font-semibold", "{name}" }
                    " from this room? They are removed from the room and cannot rejoin with this key."
                }
                div { class: "flex gap-3 justify-end",
                    button {
                        "data-testid": "newcomer-ban-cancel",
                        class: "px-4 py-2 rounded-lg text-sm font-semibold text-text-secondary hover:text-text hover:bg-surface transition-colors",
                        onclick: move |_| cancel(),
                        "Cancel"
                    }
                    button {
                        "data-testid": "newcomer-ban-confirm",
                        class: "px-4 py-2 rounded-lg bg-danger hover:bg-danger-hover text-white transition-colors",
                        onclick: move |_| {
                            let p = p.clone();
                            crate::util::defer(move || {
                                *PENDING_NEWCOMER_BAN.write() = None;
                                execute(p);
                            });
                        },
                        "Ban newcomer"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::member::{AuthorizedMember, Member};
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use river_core::room_state::provisional::{
        AdmissionEvidence, AuthorizedProvisionalMember, ProvisionalMember,
    };

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    fn id(n: u8) -> MemberId {
        MemberId::from(&key(n).verifying_key())
    }

    fn member(owner: &SigningKey, sk: &SigningKey) -> AuthorizedMember {
        AuthorizedMember::new(
            Member {
                owner_member_id: MemberId::from(&owner.verifying_key()),
                invited_by: MemberId::from(&owner.verifying_key()),
                member_vk: sk.verifying_key(),
            },
            owner,
        )
    }

    fn entry(sk: &SigningKey) -> AuthorizedProvisionalMember {
        AuthorizedProvisionalMember {
            provisional: ProvisionalMember {
                member_vk: sk.verifying_key(),
                evidence: AdmissionEvidence::Pow { nonce: 0 },
            },
            signature: sk.sign(b"test"),
        }
    }

    /// Owner 1, deputy 2, ordinary member 3, full-member author 4, newcomer 5.
    fn state() -> ChatRoomStateV1 {
        let owner = key(1);
        let mut s = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: MemberId::from(&owner.verifying_key()),
                    ..Default::default()
                },
                &owner,
            ),
            ..Default::default()
        };
        for n in [2, 3, 4] {
            s.members.members.push(member(&owner, &key(n)));
        }
        let mut owner_info = MemberInfo::new_public(id(1), 1, "o".into());
        owner_info.deputies = vec![id(2)];
        s.member_info
            .member_info
            .push(AuthorizedMemberInfo::new(owner_info, &owner));
        s.provisional.entries.push(entry(&key(5)));
        s
    }

    fn can(s: &ChatRoomStateV1, reader: u8, author: u8) -> bool {
        can_ban_newcomer(
            s,
            &key(1).verifying_key(),
            Some(&key(reader).verifying_key()),
            id(author),
        )
    }

    #[test]
    fn owner_and_a_deputy_are_offered_the_ban_on_a_newcomer() {
        let s = state();
        assert!(can(&s, 1, 5), "owner");
        assert!(can(&s, 2, 5), "deputy");
    }

    #[test]
    fn an_ordinary_member_or_a_reader_with_no_key_is_not_offered_it() {
        let s = state();
        assert!(!can(&s, 3, 5), "ordinary member");
        assert!(!can(&s, 5, 5), "a newcomer");
        assert!(!can_ban_newcomer(&s, &key(1).verifying_key(), None, id(5)));
    }

    #[test]
    fn it_is_never_offered_for_a_full_member_yourself_the_owner_or_a_stranger() {
        let s = state();
        assert!(!can(&s, 1, 4), "full member");
        assert!(!can(&s, 2, 2), "yourself");
        assert!(!can(&s, 2, 1), "the owner");
        assert!(!can(&s, 1, 9), "an author with no entry");
    }

    #[test]
    fn it_is_not_offered_again_once_the_ban_stands() {
        let mut s = state();
        let ban = build_ban(&key(1).verifying_key(), &key(1), id(5), SystemTime::now());
        s.provisional.bans.push(ban);
        assert!(!can(&s, 1, 5));
    }

    #[test]
    fn a_key_that_is_not_the_owner_or_a_deputy_record_is_not_an_issuer() {
        let s = state();
        assert!(!is_ban_issuer(
            &s,
            &key(1).verifying_key(),
            &key(7).verifying_key()
        ));
        assert!(!is_ban_issuer(
            &s,
            &key(1).verifying_key(),
            &key(3).verifying_key()
        ));
    }

    #[test]
    fn the_built_ban_verifies_under_the_contract_check_for_owner_and_deputy() {
        let owner_vk = key(1).verifying_key();
        for issuer in [1u8, 2] {
            let ban = build_ban(&owner_vk, &key(issuer), id(5), SystemTime::now());
            assert!(ban_verifies(&ban, &owner_vk, &key(issuer).verifying_key()));
            assert_eq!(ban.ban.target, id(5));
            assert_eq!(ban.ban.banned_by, id(issuer));
            assert!(
                !ban_verifies(&ban, &owner_vk, &key(3).verifying_key()),
                "another key does not verify it"
            );
        }
    }

    #[test]
    fn outcome_says_whether_the_merged_state_holds_the_ban() {
        let mut s = state();
        let ban = build_ban(&key(1).verifying_key(), &key(1), id(5), SystemTime::now());
        assert_eq!(outcome(&s, &ban), BanOutcome::NotKept);
        s.provisional.bans.push(ban.clone());
        assert_eq!(outcome(&s, &ban), BanOutcome::Stands);
        assert!(outcome_message("N", BanOutcome::NotKept).contains("not kept"));
        assert!(!outcome_message("N", BanOutcome::Stands).contains("not kept"));
    }
}
