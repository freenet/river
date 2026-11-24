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
