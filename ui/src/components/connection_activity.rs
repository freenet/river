//! Background activity shown alongside the local node's connection status.

use crate::components::app::chat_delegate::{
    pending_load_workers, RoomsLoadState, LOAD_WORKERS_CHANGED, ROOMS_LOAD_STATE,
};
use crate::components::app::freenet_api::{
    freenet_synchronizer::SynchronizerStatus, migration_journal,
};
use crate::components::app::sync_info::SYNC_INFO;
use crate::components::app::{PENDING_INVITES, ROOMS};
use crate::util::signal_guard;
use dioxus::prelude::*;
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackgroundReason {
    Connecting,
    Reconnecting,
    LoadingRooms,
    JoiningRoom,
    SyncingRooms,
}

impl BackgroundReason {
    /// The `data-busy-reason` value and the tooltip text.
    pub(crate) fn text(self) -> (&'static str, &'static str) {
        match self {
            Self::Connecting => ("connecting", "Connecting to Freenet"),
            Self::Reconnecting => ("reconnecting", "Reconnecting to Freenet"),
            Self::LoadingRooms => ("loading-rooms", "Loading your rooms"),
            Self::JoiningRoom => ("joining-room", "Joining the room"),
            Self::SyncingRooms => ("syncing-rooms", "Syncing rooms with the network"),
        }
    }
}

/// Room work by priority: loading, then joining, then syncing.
fn room_reason(loading: bool, joining: bool, syncing: bool) -> Option<BackgroundReason> {
    use BackgroundReason::*;
    [
        (loading, LoadingRooms),
        (joining, JoiningRoom),
        (syncing, SyncingRooms),
    ]
    .into_iter()
    .find_map(|(on, reason)| on.then_some(reason))
}

/// The socket status outranks room work, which shows only while connected.
fn reason(
    status: &SynchronizerStatus,
    sync_enabled: bool,
    room: Option<BackgroundReason>,
) -> Option<BackgroundReason> {
    match status {
        SynchronizerStatus::Connecting => Some(BackgroundReason::Connecting),
        SynchronizerStatus::Disconnected => sync_enabled.then_some(BackgroundReason::Reconnecting),
        SynchronizerStatus::Error(_) => None,
        SynchronizerStatus::Connected => room,
    }
}

/// A restoring walk counts on its own, even when the current delegate listed
/// no rooms: the rail's "Restoring more rooms" note needs a room to sit under,
/// but the pill is the only sign that rooms may still arrive. A migration
/// counts while that list is still empty, and while a worker is still running
/// after rooms have appeared. Once those workers settle, cached rooms are idle.
fn loading_rooms(state: RoomsLoadState, room_count: usize, workers: bool, walk: bool) -> bool {
    walk || state == RoomsLoadState::Loading
        || (state == RoomsLoadState::Migrating && (room_count == 0 || workers))
}

/// Use the same socket snapshot as the label. Room updates that leave the room
/// reason alone do not need to rerender the pill. The memo yields `None` when an
/// input is contended, `Some(room reason)` otherwise.
pub(crate) fn use_background_reason(status: &SynchronizerStatus) -> Option<BackgroundReason> {
    let activity = use_memo(move || {
        signal_guard::anchor();
        // Worker and journal state live outside signals; these versions
        // subscribe the memo to changes even when ROOMS_LOAD_STATE stays put.
        if LOAD_WORKERS_CHANGED.try_read().is_err() {
            signal_guard::schedule_nudge();
            return None;
        }
        if migration_journal::JOURNAL_GATE_VERSION.try_read().is_err() {
            signal_guard::schedule_nudge();
            return None;
        }
        let Ok(state) = ROOMS_LOAD_STATE.try_read().map(|s| *s) else {
            signal_guard::schedule_nudge();
            return None;
        };
        // Snapshot only keys; release the room data before reading sync info.
        let Ok(live_keys) = ROOMS
            .try_read()
            .map(|r| r.map.keys().copied().collect::<Vec<_>>())
        else {
            signal_guard::schedule_nudge();
            return None;
        };
        let Ok(syncing) = SYNC_INFO
            .try_read()
            .map(|info| info.has_syncing_rooms(&live_keys))
        else {
            signal_guard::schedule_nudge();
            return None;
        };
        let Ok(joining) = PENDING_INVITES
            .try_read()
            .map(|invites| invites.any_in_progress())
        else {
            signal_guard::schedule_nudge();
            return None;
        };
        let loading = loading_rooms(
            state,
            live_keys.len(),
            pending_load_workers() > 0,
            migration_journal::current_walk_may_restore_rooms(),
        );
        Some(room_reason(loading, joining, syncing))
    });
    // A contended input keeps the mounted pill's last good room reason.
    // The socket status can still change independently during that render.
    let last_good = use_hook(|| Rc::new(Cell::new(None)));
    signal_guard::anchor();
    match activity.try_read() {
        Ok(snapshot) => {
            if let Some(room) = *snapshot {
                last_good.set(room);
            }
        }
        Err(_) => signal_guard::schedule_nudge(),
    }
    reason(status, !cfg!(feature = "no-sync"), last_good.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::app::freenet_api::error_report::{ErrorReport, LABEL_DISCONNECTED};

    #[test]
    fn pill_reasons_follow_connection_and_room_priority() {
        use BackgroundReason as R;
        use SynchronizerStatus as S;

        let error = S::Error(ErrorReport::new(LABEL_DISCONNECTED, "closed", "closed"));
        let all = room_reason(true, true, true);
        assert_eq!(all, Some(R::LoadingRooms));
        assert_eq!(reason(&S::Connecting, true, all), Some(R::Connecting));
        assert_eq!(reason(&S::Disconnected, true, all), Some(R::Reconnecting));
        assert_eq!(reason(&S::Disconnected, false, all), None);
        assert_eq!(reason(&error, true, all), None);
        assert_eq!(reason(&S::Connected, true, all), Some(R::LoadingRooms));
        assert_eq!(room_reason(false, true, true), Some(R::JoiningRoom));
        assert_eq!(room_reason(false, false, true), Some(R::SyncingRooms));
        assert_eq!(room_reason(false, false, false), None);
        assert_eq!(reason(&S::Connected, true, None), None);
    }

    #[test]
    fn pill_tracks_initial_load_and_actual_partial_restoration() {
        use RoomsLoadState::*;
        assert!(loading_rooms(Loading, 2, false, false));
        assert!(loading_rooms(Migrating, 0, false, false));
        assert!(loading_rooms(Migrating, 2, true, false));
        assert!(loading_rooms(Loaded, 2, false, true));
        assert!(!loading_rooms(Migrating, 2, false, false));
        assert!(loading_rooms(Loaded, 0, false, true));
        assert!(!loading_rooms(Loaded, 0, false, false));
        assert!(loading_rooms(LoadFailed, 0, false, true));
        assert!(!loading_rooms(LoadFailed, 0, false, false));
    }
}
