//! The ten wave dots that say a user action is still on its way, and where
//! they show.
//!
//! Two reasons light them, only while the socket is connected (connecting
//! and reconnecting are the connection pill's to explain; a mark made before
//! a reconnect shows again once connected):
//!
//! - **Joining a room** (an invitation whose join is pending or subscribing,
//!   `PENDING_INVITES`), anywhere. Its outcome is the join flow's; these dots
//!   never decide it. The one screen-reader announcement is for this reason
//!   ([`UserWaitStatus`]).
//! - **A room change on its way** (an edit, a delete or a reaction, marked in
//!   `app/user_waits.rs`), in that room only. Best effort: see that module.
//!   Sending a message or a reply lights nothing; the composer keeps its
//!   draft instead (`conversation/message_input.rs`).
//!
//! # One visible host
//!
//! The reason is shared; exactly one surface may paint it, chosen by
//! [`visible_surface`]: the frontmost covering action surface (an app modal,
//! the room's delete confirmation, the DM thread, the mobile message sheet),
//! else the mobile panel that replaces the chat, else the open room's dock
//! above the composer, else the Welcome screen's slot. This plan mounts the
//! room dock and the Welcome slot; a covered surface paints nothing here (its
//! own slot is mounted by the plan that tracks its actions), so covering a
//! room hides its dots at once rather than leaving them under a translucent
//! backdrop.
//!
//! Each host stays mounted and says two things: whether there is activity
//! (`data-active`, logical, which also drives the room spacer) and whether it
//! is the owner (`data-owner`). `main.css` paints only an owner with
//! activity: dots appear 300 ms after activity starts (also on a host
//! mounting with activity already under way, through `@starting-style`), fade
//! over 200 ms when it ends, and vanish at once when the host stops being the
//! owner, whatever its fade. A pending operation does not depend on any of
//! this: a host unmounting, or losing ownership, ends no work and certifies
//! nothing.
//!
//! The room spacer (`RoomWaitSpacer`, after `bottom-sentinel` in
//! `#chat-content`) makes room for the dock at the end of the history. It
//! opens with the dots and collapses once they have faded; only the room
//! dock's ownership opens it, and losing ownership starts its collapse.

use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::user_waits::USER_WAITS;
use crate::components::app::{
    MobileView, CREATE_ROOM_MODAL, CURRENT_ROOM, EDIT_ROOM_MODAL, MEMBER_INFO_MODAL, MOBILE_VIEW,
    NOTIFICATION_MODAL, PENDING_INVITES, SYNC_STATUS,
};
use crate::components::direct_messages::{INVITE_VIA_DM_PICKER, OPEN_DM_THREAD};
use crate::components::loading_dots::WaveDots;
use crate::components::members::Invitation;
use crate::util::signal_guard;
use dioxus::prelude::*;
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserWaitReason {
    JoiningRoom,
    SendingRoomChange,
}

impl UserWaitReason {
    fn attr(self) -> &'static str {
        match self {
            Self::JoiningRoom => "joining-room",
            Self::SendingRoomChange => "sending",
        }
    }
}

/// Joining outranks the room's own change; nothing shows unless connected.
fn reason(connected: bool, joining: bool, room_change: bool) -> Option<UserWaitReason> {
    if !connected {
        return None;
    }
    if joining {
        Some(UserWaitReason::JoiningRoom)
    } else if room_change {
        Some(UserWaitReason::SendingRoomChange)
    } else {
        None
    }
}

/// Where the shared dots may paint, frontmost first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    Modal,
    DmThread,
    MobileSheet,
    MobilePanel,
    Room,
    Welcome,
}

/// What covers the conversation now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Covering {
    /// An app modal or the room's delete confirmation.
    pub modal: bool,
    pub dm_thread: bool,
    /// The mobile message sheet (below 48rem).
    pub mobile_sheet: bool,
    /// Below 48rem, the Rooms or Members panel instead of the chat. (At
    /// 48rem and wider the Members panel sits beside the chat and covers
    /// nothing.)
    pub mobile_panel: bool,
}

pub(crate) fn visible_surface(covering: Covering, room_open: bool) -> Surface {
    if covering.modal {
        Surface::Modal
    } else if covering.dm_thread {
        Surface::DmThread
    } else if covering.mobile_sheet {
        Surface::MobileSheet
    } else if covering.mobile_panel {
        Surface::MobilePanel
    } else if room_open {
        Surface::Room
    } else {
        Surface::Welcome
    }
}

/// The modals whose open state is a component's own signal rather than a
/// global, provided by `App` so the hosts can see them.
#[derive(Clone, Copy)]
pub(crate) struct LocalModals {
    pub receive_invitation: Signal<Option<Invitation>>,
    pub invite_member: Signal<bool>,
    pub export_identity: Signal<bool>,
}

/// What covers the conversation, read fallibly: `None` when an input is
/// contended (and a nudge is scheduled; the caller has anchored).
fn covering_now(local_modal: bool) -> Option<Covering> {
    let local_modals = match try_consume_context::<LocalModals>() {
        Some(m) => {
            m.receive_invitation.try_read().ok()?.is_some()
                || *m.invite_member.try_read().ok()?
                || *m.export_identity.try_read().ok()?
        }
        None => false,
    };
    let app_modal = local_modals
        || EDIT_ROOM_MODAL.try_read().ok()?.room.is_some()
        || NOTIFICATION_MODAL.try_read().ok()?.room.is_some()
        || MEMBER_INFO_MODAL.try_read().ok()?.member.is_some()
        || CREATE_ROOM_MODAL.try_read().ok()?.show
        || INVITE_VIA_DM_PICKER.try_read().ok()?.is_some();
    let dm_thread = OPEN_DM_THREAD.try_read().ok()?.is_some();
    let mobile_panel = crate::components::conversation::narrow_window_for_render()
        && *MOBILE_VIEW.try_read().ok()? != MobileView::Chat;
    Some(Covering {
        modal: app_modal || local_modal,
        dm_thread,
        mobile_sheet: false,
        mobile_panel,
    })
}

/// The surface that owns the dots now. `delete_confirmation` and
/// `mobile_sheet` are what only the conversation knows.
fn surface_for_render(
    last_good: &Rc<Cell<Option<Covering>>>,
    delete_confirmation: bool,
    mobile_sheet: bool,
    room_open: bool,
) -> Surface {
    signal_guard::anchor();
    let covering = match covering_now(delete_confirmation) {
        Some(covering) => {
            last_good.set(Some(covering));
            covering
        }
        None => {
            signal_guard::schedule_nudge();
            // As last seen; never seen: assume covered, so nothing paints.
            last_good.get().unwrap_or(Covering {
                modal: true,
                ..Covering::default()
            })
        }
    };
    visible_surface(
        Covering {
            mobile_sheet,
            ..covering
        },
        room_open,
    )
}

/// The current reason, shared by every host. A contended input keeps the
/// last good reason for this mounted host.
pub(crate) fn use_user_wait_reason() -> Option<UserWaitReason> {
    let snapshot = use_memo(move || {
        signal_guard::anchor();
        let Ok(connected) = SYNC_STATUS
            .try_read()
            .map(|s| matches!(*s, SynchronizerStatus::Connected))
        else {
            signal_guard::schedule_nudge();
            return None;
        };
        let Ok(joining) = PENDING_INVITES.try_read().map(|p| p.any_in_progress()) else {
            signal_guard::schedule_nudge();
            return None;
        };
        let room = CURRENT_ROOM.read().owner_key;
        let Ok(room_change) = USER_WAITS
            .try_read()
            .map(|w| room.is_some_and(|room| w.active(&room).is_some()))
        else {
            signal_guard::schedule_nudge();
            return None;
        };
        Some(reason(connected, joining, room_change))
    });
    let last_good = use_hook(|| Rc::new(Cell::new(None)));
    signal_guard::anchor();
    match snapshot.try_read() {
        Ok(read) => {
            if let Some(reason) = *read {
                last_good.set(reason);
            }
        }
        Err(_) => signal_guard::schedule_nudge(),
    }
    last_good.get()
}

/// One host: decorative, always mounted where it is used.
#[component]
fn UserWaitDots(surface: &'static str, owner: bool, reason: Option<UserWaitReason>) -> Element {
    rsx! {
        div {
            class: "user-wait-dots",
            "data-testid": "user-wait-dots",
            "data-surface": surface,
            "data-owner": if owner { "true" } else { "false" },
            "data-active": if reason.is_some() { "true" } else { "false" },
            "data-reason": reason.map(UserWaitReason::attr).unwrap_or(""),
            "aria-hidden": "true",
            WaveDots { test_id: "user-wait-wave" }
        }
    }
}

/// The open room's host: 12px above the composer, across the conversation
/// column, taking no hits, under Latest.
#[component]
pub(crate) fn RoomWaitDock(delete_confirmation: bool, mobile_sheet: bool) -> Element {
    let reason = use_user_wait_reason();
    let last_good = use_hook(|| Rc::new(Cell::new(None)));
    let owner =
        surface_for_render(&last_good, delete_confirmation, mobile_sheet, true) == Surface::Room;
    rsx! {
        div { class: "user-wait-dock",
            div { class: "conversation-column px-3 md:px-5",
                UserWaitDots { surface: "room", owner, reason }
            }
        }
    }
}

/// The room history's clearance for the dock, after `bottom-sentinel`: no
/// row key, no anchor key, so neither the read rule nor reading positions
/// ever count it.
#[component]
pub(crate) fn RoomWaitSpacer(delete_confirmation: bool, mobile_sheet: bool) -> Element {
    let reason = use_user_wait_reason();
    let last_good = use_hook(|| Rc::new(Cell::new(None)));
    let owner =
        surface_for_render(&last_good, delete_confirmation, mobile_sheet, true) == Surface::Room;
    let active = owner && reason.is_some();
    rsx! {
        div {
            id: "user-wait-spacer",
            class: "user-wait-spacer",
            "data-testid": "user-wait-spacer",
            "data-active": if active { "true" } else { "false" },
            "aria-hidden": "true",
        }
    }
}

/// The Welcome screen's host, in a slot of fixed height below its text.
#[component]
pub(crate) fn WelcomeWaitSlot() -> Element {
    let reason = use_user_wait_reason();
    let last_good = use_hook(|| Rc::new(Cell::new(None)));
    let owner = surface_for_render(&last_good, false, false, false) == Surface::Welcome;
    rsx! {
        div { class: "user-wait-welcome-slot",
            UserWaitDots { surface: "welcome", owner, reason }
        }
    }
}

/// The one live region: announces a join, and stays quiet for room changes,
/// which repeat with every edit or reaction.
#[component]
pub(crate) fn UserWaitStatus() -> Element {
    let reason = use_user_wait_reason();
    let text = match reason {
        Some(UserWaitReason::JoiningRoom) => "Joining room",
        _ => "",
    };
    rsx! {
        div {
            id: "user-wait-status",
            "data-testid": "user-wait-status",
            role: "status",
            "aria-live": "polite",
            class: "sr-only",
            "{text}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_show_only_while_connected_and_joining_comes_first() {
        use UserWaitReason::*;
        assert_eq!(reason(false, true, true), None);
        assert_eq!(reason(true, true, true), Some(JoiningRoom));
        assert_eq!(reason(true, false, true), Some(SendingRoomChange));
        assert_eq!(reason(true, false, false), None);
    }

    #[test]
    fn the_frontmost_surface_owns_the_dots() {
        let none = Covering::default();
        assert_eq!(visible_surface(none, true), Surface::Room);
        assert_eq!(visible_surface(none, false), Surface::Welcome);
        let all = Covering {
            modal: true,
            dm_thread: true,
            mobile_sheet: true,
            mobile_panel: true,
        };
        assert_eq!(visible_surface(all, true), Surface::Modal);
        let dm = Covering {
            modal: false,
            ..all
        };
        assert_eq!(visible_surface(dm, true), Surface::DmThread);
        let sheet = Covering {
            dm_thread: false,
            ..dm
        };
        assert_eq!(visible_surface(sheet, true), Surface::MobileSheet);
        let panel = Covering {
            mobile_sheet: false,
            ..sheet
        };
        assert_eq!(visible_surface(panel, true), Surface::MobilePanel);
        assert_eq!(visible_surface(panel, false), Surface::MobilePanel);
    }
}
