//! Best-effort feedback that a room change the user made is on its way to
//! the network: the room's wait dots (`components/user_wait_dots.rs`).
//!
//! An edit or a delete that the local room has admitted
//! (`outbox_commit::commit_ready_now`) marks its room here. The mark ends at
//! the room contract's next UPDATE reply or keyed UPDATE error, or after
//! [`FEEDBACK_CAP_MS`], whichever comes first. A message or reply from the
//! composer marks nothing: the composer keeps its draft until the room admits
//! it, and its sync goes on in the background. A reaction marks nothing
//! either: its sync stays out of sight.
//!
//! **This is not an acknowledgment of the action.** An UPDATE reply names a
//! contract, not a request, so any reply for the room ends the mark: an
//! earlier action's late reply, or a background sync's, can end a later
//! action's feedback early. A later eligible action marks the room again.
//! There is no request ledger behind this, by design: the protocol has no
//! per-request correlation to match one against.
//!
//! Nothing here certifies that anything was saved or sent. The generation
//! reload's busy and flush checks (`generation_watch.rs`) read the outbox and
//! `NEEDS_SYNC` directly; a mark, its reply, or its expiry neither delays nor
//! permits a reload, and expiry changes no saved state, toast or retry.
//!
//! A reconnect clears nothing: a mark made before it shows again once the
//! socket is connected, until its reply or its cap. Typed UPDATE errors are
//! observed in the socket's own callback, before they are flattened to text,
//! and only for the current connection attempt; an UPDATE reply is observed
//! where the synchronizer handles it, which does not yet carry its socket's
//! attempt (freenet/river#852).

use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientError, ContractError, ErrorKind, RequestError};
use freenet_stdlib::prelude::{ContractInstanceId, ContractKey};
use std::collections::HashMap;

/// The longest a room's wait dots show without a reply.
const FEEDBACK_CAP_MS: u64 = 20_000;

/// Prefix of freenet-core's synthesized UPDATE failure, which names no
/// contract (`operations/update/op_ctx_task.rs`: an infrastructure error).
// The socket callback that reads it is wasm-only.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const UNKEYED_UPDATE_FAILURE: &str = "UPDATE failed";

/// Local UI state, never a contract summary, so a `HashMap` is fine.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct UserWaits {
    /// When each waiting room was last marked.
    rooms: HashMap<VerifyingKey, u64>,
}

impl UserWaits {
    /// Marks `room` as waiting since `now_ms`; a later action restamps it.
    fn mark(&mut self, room: VerifyingKey, now_ms: u64) {
        self.rooms.insert(room, now_ms);
    }

    pub fn is_waiting(&self, room: &VerifyingKey) -> bool {
        self.rooms.contains_key(room)
    }

    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn is_empty(&self) -> bool {
        self.rooms.is_empty()
    }

    #[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
    pub fn room_count(&self) -> usize {
        self.rooms.len()
    }

    /// Ends `room`'s mark.
    fn complete(&mut self, room: &VerifyingKey) {
        self.rooms.remove(room);
    }

    /// Ends every room's mark: an UPDATE failure that names no contract.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn complete_all(&mut self) {
        self.rooms.clear();
    }

    /// Ends the marks at or past their cap.
    fn sweep(&mut self, now_ms: u64) {
        self.rooms
            .retain(|_, started| now_ms.saturating_sub(*started) < FEEDBACK_CAP_MS);
    }

    /// When the next mark reaches its cap.
    fn next_deadline(&self) -> Option<u64> {
        self.rooms
            .values()
            .map(|started| started.saturating_add(FEEDBACK_CAP_MS))
            .min()
    }
}

/// An UPDATE failure, as the socket reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) enum UpdateFailure {
    Keyed(ContractInstanceId),
    Unkeyed,
}

/// The UPDATE failure `error` reports, if it is one. Read from the error's
/// structure: a delegate error, a GET or PUT failure, an auth error or some
/// other operation error whose text merely mentions an update is not one.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn update_failure(error: &ClientError) -> Option<UpdateFailure> {
    match error.kind() {
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Update {
            key, ..
        })) => Some(UpdateFailure::Keyed(*key.id())),
        ErrorKind::OperationError { cause } if cause.starts_with(UNKEYED_UPDATE_FAILURE) => {
            Some(UpdateFailure::Unkeyed)
        }
        _ => None,
    }
}

pub(crate) static USER_WAITS: GlobalSignal<UserWaits> = Global::new(UserWaits::default);

fn now_ms() -> u64 {
    crate::util::get_current_system_time()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// An edit or a delete has been applied to `room` locally: show its room's
/// wait dots. Its sync is the caller's to queue, synchronously
/// (`outbox_commit::commit_ready_now`); only this feedback is deferred.
pub(crate) fn mark_user_change(room: VerifyingKey) {
    let now = now_ms();
    crate::util::defer(move || {
        USER_WAITS.with_mut(|w| w.mark(room, now));
        arm_sweep();
    });
}

/// A reply to an UPDATE of `key` arrived: end its room's feedback.
pub(crate) fn note_update_response(key: ContractKey) {
    crate::util::defer(move || end_for_contract(*key.id()));
}

/// The current connection reported an UPDATE failure.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn note_update_failure(failure: UpdateFailure) {
    crate::util::defer(move || match failure {
        UpdateFailure::Keyed(id) => end_for_contract(id),
        UpdateFailure::Unkeyed => {
            if !USER_WAITS.peek().is_empty() {
                USER_WAITS.with_mut(|w| w.complete_all());
            }
        }
    });
}

/// From a deferred closure only.
fn end_for_contract(id: ContractInstanceId) {
    let room = crate::components::app::ROOMS
        .peek()
        .map
        .iter()
        .find(|(_, room)| *room.contract_key.id() == id)
        .map(|(owner, _)| *owner);
    if let Some(room) = room {
        // Checked first so a reply for a room with no mark writes nothing.
        if USER_WAITS.peek().is_waiting(&room) {
            USER_WAITS.with_mut(|w| w.complete(&room));
        }
    }
}

thread_local! {
    static SWEEP_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One sweep at a time, sleeping until the earliest cap. From a deferred
/// closure only.
fn arm_sweep() {
    if SWEEP_ARMED.with(|armed| armed.replace(true)) {
        return;
    }
    let Some(deadline) = USER_WAITS.peek().next_deadline() else {
        SWEEP_ARMED.with(|armed| armed.set(false));
        return;
    };
    let wait = deadline.saturating_sub(now_ms());
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(std::time::Duration::from_millis(wait)).await;
        crate::util::defer(|| {
            SWEEP_ARMED.with(|armed| armed.set(false));
            let now = now_ms();
            let expired = {
                let waits = USER_WAITS.peek();
                waits.next_deadline().is_some_and(|d| d <= now)
            };
            if expired {
                USER_WAITS.with_mut(|w| w.sweep(now));
            }
            arm_sweep();
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use std::borrow::Cow;

    fn room(n: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[n; 32]).verifying_key()
    }

    #[test]
    fn rooms_are_independent_and_a_later_action_restamps() {
        let mut waits = UserWaits::default();
        waits.mark(room(1), 1_000);
        waits.mark(room(2), 5_000);
        waits.complete(&room(2));
        assert!(waits.is_waiting(&room(1)));
        assert!(!waits.is_waiting(&room(2)));
        waits.complete(&room(2));
        assert!(
            waits.is_waiting(&room(1)),
            "ending a room with no mark ends nothing else"
        );

        waits.mark(room(1), 15_000);
        waits.sweep(1_000 + FEEDBACK_CAP_MS);
        assert!(waits.is_waiting(&room(1)), "restamped at 15s");
        assert_eq!(waits.next_deadline(), Some(15_000 + FEEDBACK_CAP_MS));
    }

    /// A starts, B starts in the same room, A's reply arrives: the reply
    /// names the room's contract, not an action, so it ends the room's
    /// feedback for both. Best effort, as the module says.
    #[test]
    fn an_earlier_actions_reply_ends_a_later_ones_feedback() {
        let mut waits = UserWaits::default();
        waits.mark(room(1), 1_000);
        waits.mark(room(1), 2_000);
        waits.mark(room(2), 2_000);
        waits.complete(&room(1));
        assert!(!waits.is_waiting(&room(1)));
        assert!(
            waits.is_waiting(&room(2)),
            "another room's reply ends nothing here"
        );
        assert_eq!(waits.next_deadline(), Some(2_000 + FEEDBACK_CAP_MS));
    }

    #[test]
    fn an_unkeyed_failure_ends_every_room() {
        let mut waits = UserWaits::default();
        waits.complete_all();
        assert_eq!(waits, UserWaits::default());
        waits.mark(room(1), 0);
        waits.mark(room(2), 0);
        waits.complete_all();
        assert!(waits.is_empty());
        assert_eq!(waits.next_deadline(), None);
    }

    #[test]
    fn the_cap_ends_a_mark_at_exactly_twenty_seconds() {
        let mut waits = UserWaits::default();
        waits.mark(room(1), 1_000);
        waits.sweep(1_000 + FEEDBACK_CAP_MS - 1);
        assert!(waits.is_waiting(&room(1)));
        assert_eq!(waits.next_deadline(), Some(1_000 + FEEDBACK_CAP_MS));
        waits.sweep(1_000 + FEEDBACK_CAP_MS);
        assert!(!waits.is_waiting(&room(1)));
        assert_eq!(waits.next_deadline(), None);
        // A clock that went backwards keeps the mark until its cap.
        waits.mark(room(1), 50_000);
        waits.sweep(10_000);
        assert!(waits.is_waiting(&room(1)));
        assert_eq!(waits.next_deadline(), Some(50_000 + FEEDBACK_CAP_MS));
    }

    #[test]
    fn only_update_failures_count() {
        let key = ContractKey::from_id_and_code(
            ContractInstanceId::new([7u8; 32]),
            freenet_stdlib::prelude::CodeHash::new([8u8; 32]),
        );
        let err = |kind: ErrorKind| ClientError::from(kind);
        let cause = |s: &'static str| Cow::Borrowed(s);
        assert_eq!(
            update_failure(&err(ErrorKind::RequestError(RequestError::ContractError(
                ContractError::Update {
                    key,
                    cause: cause("refused")
                }
            )))),
            Some(UpdateFailure::Keyed(*key.id()))
        );
        assert_eq!(
            update_failure(&err(ErrorKind::OperationError {
                cause: cause("UPDATE failed: timed out")
            })),
            Some(UpdateFailure::Unkeyed)
        );
        for other in [
            ErrorKind::RequestError(RequestError::ContractError(ContractError::Get {
                key,
                cause: cause("update failed"),
            })),
            ErrorKind::RequestError(RequestError::ContractError(ContractError::Put {
                key,
                cause: cause("UPDATE failed"),
            })),
            ErrorKind::OperationError {
                cause: cause("the update was refused"),
            },
            ErrorKind::Unhandled {
                cause: cause("UPDATE failed"),
            },
            ErrorKind::Disconnect,
        ] {
            assert_eq!(update_failure(&err(other.clone())), None, "{other:?}");
        }
    }
}
