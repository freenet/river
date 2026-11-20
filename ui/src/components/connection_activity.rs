//! Background activity shown alongside the local node's connection status.

use crate::components::app::chat_delegate::{
    pending_load_workers, RoomsLoadState, LOAD_WORKERS_CHANGED, ROOMS_LOAD_STATE,
};
use crate::components::app::freenet_api::{
    freenet_synchronizer::SynchronizerStatus, migration_journal,
};
use crate::components::app::sync_info::SYNC_INFO;
use crate::components::app::{PENDING_INVITES, ROOMS};
use crate::components::room_list::{
    restoring_more_rooms, room_list_display_state, RoomListDisplay,
};
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
    pub(crate) fn as_attr(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Reconnecting => "reconnecting",
            Self::LoadingRooms => "loading-rooms",
            Self::JoiningRoom => "joining-room",
            Self::SyncingRooms => "syncing-rooms",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting to Freenet",
            Self::Reconnecting => "Reconnecting to Freenet",
            Self::LoadingRooms => "Loading your rooms",
            Self::JoiningRoom => "Joining the room",
            Self::SyncingRooms => "Syncing rooms with the network",
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
struct RoomActivity {
    loading: bool,
    joining: bool,
    syncing: bool,
}

impl RoomActivity {
    fn reason(self, status: &SynchronizerStatus, sync_enabled: bool) -> Option<BackgroundReason> {
        use BackgroundReason::*;
        match status {
            SynchronizerStatus::Connecting => Some(Connecting),
            SynchronizerStatus::Disconnected => sync_enabled.then_some(Reconnecting),
            SynchronizerStatus::Error(_) => None,
            SynchronizerStatus::Connected if self.loading => Some(LoadingRooms),
            SynchronizerStatus::Connected if self.joining => Some(JoiningRoom),
            SynchronizerStatus::Connected if self.syncing => Some(SyncingRooms),
            SynchronizerStatus::Connected => None,
        }
    }
}

fn loading_rooms(state: RoomsLoadState, room_count: usize, workers: bool, walk: bool) -> bool {
    state == RoomsLoadState::Loading
        || room_list_display_state(state, room_count) == RoomListDisplay::Migrating
        || restoring_more_rooms(state, workers, room_count, walk)
}

/// Use the same socket snapshot as the label. Room updates that leave these
/// three facts alone do not need to rerender the pill.
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
        Some(RoomActivity {
            loading: loading_rooms(
                state,
                live_keys.len(),
                pending_load_workers() > 0,
                migration_journal::current_walk_may_restore_rooms(),
            ),
            joining,
            syncing,
        })
    });
    // A contended input keeps the mounted pill's last good room activity.
    // The socket status can still change independently during that render.
    let last_good = use_hook(|| Rc::new(Cell::new(RoomActivity::default())));
    signal_guard::anchor();
    match activity.try_read() {
        Ok(snapshot) => {
            if let Some(facts) = *snapshot {
                last_good.set(facts);
            }
        }
        Err(_) => signal_guard::schedule_nudge(),
    }
    last_good.get().reason(status, !cfg!(feature = "no-sync"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::app::freenet_api::error_report::{ErrorReport, LABEL_DISCONNECTED};

    #[test]
    fn pill_reasons_follow_connection_and_room_priority() {
        use BackgroundReason as R;
        use SynchronizerStatus as S;

        let all = RoomActivity {
            loading: true,
            joining: true,
            syncing: true,
        };
        let join_and_sync = RoomActivity {
            loading: false,
            ..all
        };
        let sync = RoomActivity {
            joining: false,
            ..join_and_sync
        };
        let error = S::Error(ErrorReport::new(LABEL_DISCONNECTED, "closed", "closed"));
        assert_eq!(all.reason(&S::Connecting, true), Some(R::Connecting));
        assert_eq!(all.reason(&S::Disconnected, true), Some(R::Reconnecting));
        assert_eq!(all.reason(&S::Disconnected, false), None);
        assert_eq!(all.reason(&error, true), None);
        assert_eq!(all.reason(&S::Connected, true), Some(R::LoadingRooms));
        assert_eq!(
            join_and_sync.reason(&S::Connected, true),
            Some(R::JoiningRoom)
        );
        assert_eq!(sync.reason(&S::Connected, true), Some(R::SyncingRooms));
        assert_eq!(RoomActivity::default().reason(&S::Connected, true), None);
    }

    #[test]
    fn pill_tracks_initial_load_and_actual_partial_restoration() {
        use RoomsLoadState::*;
        assert!(loading_rooms(Loading, 2, false, false));
        assert!(loading_rooms(Migrating, 0, false, false));
        assert!(loading_rooms(Migrating, 2, true, false));
        assert!(loading_rooms(Loaded, 2, false, true));
        assert!(!loading_rooms(Migrating, 2, false, false));
        assert!(!loading_rooms(Loaded, 0, false, false));
        assert!(!loading_rooms(LoadFailed, 0, false, false));
    }
}
