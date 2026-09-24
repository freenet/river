//! In-room direct-message UI (#243 Phase 2, #258 follow-ups).
//!
//! UX model: clicking a member opens the member-info modal, which has a
//! "Send Direct Message" button. That button opens this module's
//! [`DmThreadModal`]: a per-pair thread of decrypted DMs plus a composer.
//!
//! A second, primary entry point lives in the **left rail under Rooms**:
//! [`crate::components::room_list::dm_rail_section::DmRailSection`] lists
//! every open DM thread across ALL rooms the local user is in, with an
//! unread badge per thread. Clicking a row opens [`DmThreadModal`] for
//! that (room, peer). Replaces the earlier per-room inbox button in the
//! members panel (zorolin feedback, 2026-05-16).
//!
//! Persistence model: all message state lives in `ChatRoomStateV1`. This
//! module only adds *view* state — currently open thread, last-seen
//! timestamps per peer for unread tracking. Last-seen state is purely
//! in-memory; reloading the page seeds it from the room state (see
//! [`seed_dm_last_seen_if_needed`]) so previously-read DMs don't pop
//! back as unread on every page load.

mod dm_thread_modal;
mod invite_via_dm_picker_modal;

pub use dm_thread_modal::DmThreadModal;
pub use invite_via_dm_picker_modal::InviteViaDmPickerModal;

use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::chat_delegate::{HiddenDmThreadEntry, OutboundDmEntry};
use river_core::room_state::direct_messages::PurgeToken;
use river_core::room_state::member::MemberId;
use std::collections::HashMap;

/// Currently-open DM thread, addressed by (room_owner_vk, counterparty).
/// `None` means no DM modal is open.
pub static OPEN_DM_THREAD: GlobalSignal<Option<(VerifyingKey, MemberId)>> = Global::new(|| None);

/// Per-(room, peer) timestamp (unix seconds) of the most recent DM the local
/// user has actually viewed in [`DmThreadModal`]. Anything in
/// `room.direct_messages.messages` addressed to the local user with
/// `timestamp > last_seen` counts as unread.
pub static DM_LAST_SEEN: GlobalSignal<HashMap<(VerifyingKey, MemberId), u64>> =
    Global::new(HashMap::new);

/// Pure decision for [`mark_thread_read`]: would recording `up_to_ts`
/// actually advance the stored cutoff? `current` is the existing
/// `DM_LAST_SEEN` entry for the `(room, peer)` pair (`None` = no entry
/// yet, which reads as cutoff 0 everywhere else).
///
/// Split out because the answer gates whether `mark_thread_read`
/// touches the signal at all: `DmThreadModalBody` calls
/// `mark_thread_read` from its render body on every render while a
/// thread is open, and `with_mut` notifies subscribers even when the
/// mutation changed nothing — so an unconditional write turned an open
/// DM thread into a continuous write pulse on `DM_LAST_SEEN`, widening
/// the contention window that blanked the DM rail (issue #499).
/// Pinned by the `thread_read_needs_write_*` tests plus the wiring pin
/// `mark_thread_read_write_is_gated_pinned`.
pub(crate) fn thread_read_needs_write(current: Option<u64>, up_to_ts: u64) -> bool {
    up_to_ts > current.unwrap_or(0)
}

/// Mark every DM from `peer` in `room` as seen up to (and including) the
/// most recent inbound message timestamp known to the synchronizer.
pub fn mark_thread_read(room: VerifyingKey, peer: MemberId, up_to_ts: u64) {
    crate::util::defer(move || {
        // Skip the write when the stored cutoff would not advance —
        // `with_mut` notifies subscribers even for a no-op mutation and
        // this runs on every render of an open thread (issue #499
        // write-pulse). `try_peek` registers no subscription; it fails
        // ONLY while a live WRITE borrow exists on `DM_LAST_SEEN` — and
        // on that same synchronous stack `with_mut` (which is
        // `f(&mut *self.write())`, a panicking borrow) would be a
        // GUARANTEED panic. So a contended peek must SKIP, never fall
        // through to the write. Skipping is harmless: this function
        // re-fires on every render of the open thread and again on the
        // next inbound message, so a skipped advance self-heals on the
        // next clean pass. The `false` fallback below is load-bearing —
        // a `true` fallback is the panic path. Pinned by
        // `mark_thread_read_write_is_gated_pinned`.
        let needs_write = DM_LAST_SEEN
            .try_peek()
            .map(|seen| thread_read_needs_write(seen.get(&(room, peer)).copied(), up_to_ts))
            .unwrap_or(false);
        if !needs_write {
            return;
        }
        DM_LAST_SEEN.with_mut(|seen| {
            let entry = seen.entry((room, peer)).or_insert(0);
            if up_to_ts > *entry {
                *entry = up_to_ts;
            }
        });
    });
}

/// Open the DM thread modal for `(room, peer)`. Closes any other open
/// thread first.
pub fn open_dm_thread(room: VerifyingKey, peer: MemberId) {
    crate::util::defer(move || {
        *OPEN_DM_THREAD.write() = Some((room, peer));
    });
}

/// "Share an invite via DM…" picker state. When `Some((room, peer))`, the
/// [`InviteViaDmPickerModal`] is visible and offers to generate an invite
/// for ANOTHER room and pre-fill a DM to `peer` in `room` with the invite
/// URL. See issue #252.
pub static INVITE_VIA_DM_PICKER: GlobalSignal<Option<(VerifyingKey, MemberId)>> =
    Global::new(|| None);

/// Currently in-flight pick generated by [`InviteViaDmPickerModal`]'s row
/// click. Lives at module scope (not `use_signal` inside the picker)
/// because a watchdog task can outlive the picker's unmount on the
/// success path; reading a use_signal whose owning component has been
/// dropped panics in Dioxus. The `generation` field lets stale
/// watchdogs short-circuit immediately when a newer pick has taken over
/// (Codex P2 + Skeptical M1/L3 on PR #260).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct InvitePickInflight {
    /// Monotonic counter — every fresh row-click bumps this. Watchdogs
    /// capture it at scheduling time and no-op if it has moved on.
    pub generation: u64,
    /// Which candidate room the user picked, for the row spinner.
    pub room_vk: VerifyingKey,
}

pub static INVITE_VIA_DM_PICKER_INFLIGHT: GlobalSignal<Option<InvitePickInflight>> =
    Global::new(|| None);

/// Body to pre-fill into the DM composer the next time
/// [`DmThreadModal`] renders for the matching (room, peer). Used by the
/// "Share an invite via DM…" flow (#252) to drop an invite URL straight
/// into the recipient's thread composer.
///
/// Consumed on render: the thread body component drains this signal the
/// first time it matches its `(room, peer)` props, then clears it so a
/// second render doesn't reset what the user has subsequently typed.
pub static DM_DRAFT: GlobalSignal<Option<(VerifyingKey, MemberId, String)>> = Global::new(|| None);

/// In-memory cache of outbound DM plaintext, keyed by
/// `(room_owner_vk, recipient, purge_token)`. Hydrated from the chat
/// delegate on app startup and re-written on every send / purge. Used
/// by [`DmThreadModal`] (and CLI/`riverctl dm list` via the equivalent
/// CLI cache) to render the sender's own outbound bubbles as plaintext
/// instead of "sent — ciphertext only". See issue freenet/river#256.
///
/// Miss on lookup → caller falls back to the placeholder, so DMs sent
/// under older clients (pre-#256) continue to render as ciphertext-only.
pub static OUTBOUND_DMS: GlobalSignal<OutboundDmsCache> = Global::new(OutboundDmsCache::default);

/// In-memory shape of the outbound-DM cache. The on-disk form is
/// `river_core::chat_delegate::OutboundDmStore` (a `Vec` for JSON safety
/// per the bug-prevention pattern); we hold a `HashMap` here for O(1)
/// render-time lookup.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OutboundDmsCache {
    pub by_token: HashMap<(VerifyingKey, MemberId, PurgeToken), OutboundDmEntry>,
}

impl OutboundDmsCache {
    pub fn get(
        &self,
        room: &VerifyingKey,
        recipient: &MemberId,
        token: &PurgeToken,
    ) -> Option<&OutboundDmEntry> {
        self.by_token.get(&(*room, *recipient, *token))
    }

    /// Insert an entry — exposed so unit tests can construct a
    /// populated cache without depending on the delegate I/O path.
    /// Production callers should go through
    /// `chat_delegate::save_outbound_dm` to also get per-pair cap
    /// eviction and a delegate save.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, entry: OutboundDmEntry) {
        let key = (
            ed25519_dalek::VerifyingKey::from_bytes(&entry.room_owner_vk)
                .expect("test entry has valid room VK"),
            entry.recipient,
            entry.purge_token,
        );
        self.by_token.insert(key, entry);
    }
}

/// Local view-state for issue freenet/river#261: per-`(room, peer)`
/// "hidden-at" cutoffs. A thread is hidden from
/// [`crate::components::room_list::dm_rail_section::DmRailSection`]
/// iff no message between the local user and `peer` in `room` is
/// strictly later than the recorded `hidden_at_ts`.
///
/// Persisted via the chat-delegate `OutboundDmStore.hidden_threads`
/// side-channel so a hide on device A propagates to device B (and
/// survives a reload on the same device). In-memory shape is a
/// `HashMap` keyed by `(room, peer)` for O(1) render-time lookup; the
/// on-disk shape is a `Vec` for JSON safety (see the "non-string map
/// keys" bug-prevention pattern).
pub static HIDDEN_DM_THREADS: GlobalSignal<HashMap<(VerifyingKey, MemberId), HiddenDmThreadEntry>> =
    Global::new(HashMap::new);

/// UI-side wrapper around `river_core::chat_delegate::is_thread_hidden`
/// that does the `VerifyingKey -> [u8; 32]` conversion at the boundary.
/// Kept as a thin shim so callers don't have to repeat the byte
/// conversion at every render site.
pub fn is_thread_hidden_for(
    hidden: &HashMap<(VerifyingKey, MemberId), HiddenDmThreadEntry>,
    room: &VerifyingKey,
    peer: MemberId,
    max_message_ts: u64,
) -> bool {
    hidden
        .get(&(*room, peer))
        .is_some_and(|h| max_message_ts <= h.hidden_at_ts)
}

/// Pure helper: decide how to render an outbound DM bubble given the
/// loaded plaintext cache.
///
/// `Ok(plaintext)` — render as user-supplied prose (markdown / linkify
/// pass). `Err(())` — render as the legacy `"sent — ciphertext only"`
/// placeholder (DM was sent before this cache shipped, or on a second
/// device whose cache hasn't hydrated yet, OR the cache entry is
/// missing for any other reason).
///
/// Pinned by `dm_outbound_lookup_returns_plaintext_on_hit` and
/// `dm_outbound_lookup_returns_err_on_miss` — issue freenet/river#256
/// regression coverage. Both `DmThreadModalBody` (UI) and the CLI
/// `execute_list` (via the same lookup tuple) MUST stay in agreement
/// with this helper.
pub fn lookup_outbound_plaintext(
    cache: &OutboundDmsCache,
    room: &VerifyingKey,
    recipient: &MemberId,
    token: &PurgeToken,
) -> Result<String, ()> {
    cache
        .get(room, recipient, token)
        .map(|entry| entry.plaintext.clone())
        .ok_or(())
}

/// Open the invite-via-DM picker for the given target peer in the current
/// room.
///
/// Refuses to open for `peer == self` even though all current callers gate
/// on that — defense in depth so a future shortcut doesn't strand a user
/// with a self-DM draft they can't send (Skeptical-review finding #3).
pub fn open_invite_via_dm_picker(current_room: VerifyingKey, peer: MemberId) {
    let Ok(rooms) = crate::components::app::ROOMS.try_read() else {
        return;
    };
    // Public half only. With no local identity the self-as-peer check simply
    // does not apply — the same fall-through as a room that isn't loaded.
    if let Some(self_id) = rooms
        .map
        .get(&current_room)
        .and_then(|room_data| room_data.self_member_id())
    {
        if self_id == peer {
            dioxus::logger::tracing::warn!(
                "open_invite_via_dm_picker: refusing to open for self-as-peer"
            );
            return;
        }
    }
    drop(rooms);
    crate::util::defer(move || {
        *INVITE_VIA_DM_PICKER.write() = Some((current_room, peer));
    });
}

/// Tracks whether the one-shot DM-last-seen seed has already run for this
/// session. Once set, [`seed_dm_last_seen_if_needed`] is a no-op — which is
/// what we want: if it kept running on every `ROOMS` update, every newly-
/// arrived inbound DM would seed itself and never surface as unread (Codex
/// P2 on #244 review pass 3).
static DM_LAST_SEEN_SEEDED: GlobalSignal<bool> = Global::new(|| false);

/// Result of [`send_structured_dm`] — surfaced to the caller so the
/// picker / DM modal can either close + toast on success or render an
/// inline error string. Mirrors the `ApplyOutcome` shape used inside
/// `dm_thread_modal.rs`, but kept as its own enum here because the
/// surface is callable from outside the modal and we don't want the
/// modal-specific names leaking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendDmOutcome {
    /// Delta applied locally, `mark_needs_sync` queued, outbound cache
    /// updated. The caller should clear its composer / close its modal.
    Sent,
    /// The room we tried to send in is no longer in `ROOMS` (unloaded).
    RoomGone,
    /// The recipient is not currently a member of the room.
    RecipientNotMember,
    /// Sender == recipient.
    SelfDm,
    /// The local user has been pruned from the room's member list AND
    /// no rejoin credentials are available. The contract would silent-
    /// drop the DM. See `dm_thread_modal.rs`'s `SilentDrop` arm for the
    /// full diagnostic — same root cause.
    SenderMissingRejoin,
    /// Body encoding failed (CBOR serialize error) or the resulting
    /// envelope exceeds `MAX_DM_CIPHERTEXT_BYTES`. Carries the error
    /// string from the underlying helper.
    BodyTooLargeOrEncodeFailed(String),
    /// `apply_delta` returned Err (signature / membership / tombstone /
    /// cap inside the contract layer). Deterministic — retrying byte-
    /// identical input gives the same result. Carries the diagnostic.
    DeltaFailed(String),
    /// `apply_delta` returned Ok but the message wasn't actually present
    /// in `direct_messages.messages` after the merge. Sender or
    /// recipient missing from members AND no rejoin bundle could fix
    /// it. See `ApplyOutcome::SilentDrop` in `dm_thread_modal.rs`.
    SilentDrop,
    /// This device does not hold the local user's key for this room, so the
    /// DM cannot be signed or sealed. Distinct from `SenderMissingRejoin`:
    /// the user may well be a member, we just cannot author as them here.
    IdentityUnavailable,
}

/// Compose, locally-apply, and queue for network sync a single direct
/// message with a structured `DirectMessageBody` (Text or Invite). This
/// is the canonical send-a-DM entry point for callers that need the
/// structured form — currently:
///
/// * [`crate::components::direct_messages::invite_via_dm_picker_modal`]
///   for the in-app "Share an invite via DM…" flow, which sends an
///   `Invite` variant directly (no `DM_DRAFT` URL paste).
///
/// `dm_thread_modal.rs::do_send` will move to this helper in a follow-
/// up commit — for now it still inlines the same logic for `Text` bodies.
/// The equivalence has been hand-verified but is not yet pinned by a
/// dedicated integration test; do not refactor either path without
/// re-checking the wire-byte equality.
///
/// Returns an outcome the caller renders inline (no panicking, no
/// `expect`). Side effects on `Sent`: writes to `ROOMS`, calls
/// `mark_needs_sync`, calls `unhide_dm_thread`, persists outbound
/// plaintext into the delegate-backed cache.
///
/// **Plaintext stored in the outbound cache.** The cache key is
/// `(room, recipient, purge_token)`; the value is the user-facing
/// rendering of the body. For `Text { text }` that's just `text`; for
/// `Invite { personal_message, .. }` we store a JSON-encoded summary so
/// the sender's own list view doesn't lose the structured shape. The
/// recipient ignores this entirely — they decode the wire body itself.
pub async fn send_structured_dm(
    room: VerifyingKey,
    peer: MemberId,
    body: river_core::room_state::dm_body::DirectMessageBody,
) -> SendDmOutcome {
    use crate::components::app::chat_delegate::{save_outbound_dm, unhide_dm_thread};
    use crate::components::app::{mark_needs_sync, ROOMS};
    use freenet_scaffold::ComposableState;
    use river_core::room_state::direct_messages::{compose_direct_message, DirectMessagesDelta};
    use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};

    // Snapshot what we need from ROOMS. The pre-flight reads go
    // through `defer` because this function is called from
    // `safe_spawn_local` contexts (e.g. the invite-via-DM picker's
    // `drive_send`), where the Dioxus runtime ISN'T on the call stack
    // — a bare `ROOMS.try_read()` here would panic with "Must be
    // called from inside a Dioxus runtime" (Codex P1 finding on PR
    // #278). `defer` pushes the captured runtime + root scope before
    // running the closure, so GlobalSignal access is safe inside.
    //
    // We funnel the result through a oneshot channel so the caller
    // can `await` us synchronously. The closure inside `defer` is
    // synchronous (runs in a `setTimeout(0)` macrotask).
    struct PreflightSnapshot {
        self_sk: ed25519_dalek::SigningKey,
        self_id: MemberId,
        peer_vk: VerifyingKey,
        rejoin_members: Option<river_core::room_state::member::MembersDelta>,
        rejoin_member_info: Option<Vec<river_core::room_state::member_info::AuthorizedMemberInfo>>,
    }
    // Boxed `Ready` variant — `PreflightSnapshot` contains a `SigningKey`
    // and `VerifyingKey` plus two `Option<...>` rejoin fields, well over
    // 100 bytes. The `Reject` variant is just a small enum, so the
    // unboxed enum would carry ~200 bytes regardless of which variant
    // is active (clippy::large_enum_variant). Box the larger variant so
    // both fit in a discriminant + pointer.
    enum PreflightOutcome {
        Ready(Box<PreflightSnapshot>),
        Reject(SendDmOutcome),
    }
    let (preflight_tx, preflight_rx) = futures::channel::oneshot::channel::<PreflightOutcome>();
    crate::util::defer(move || {
        let outcome = (|| {
            let Some(room_data) = ROOMS
                .try_read()
                .ok()
                .and_then(|r| r.map.get(&room).cloned())
            else {
                return PreflightOutcome::Reject(SendDmOutcome::RoomGone);
            };
            // A DM is signed AND sealed with the local key, so an absent one
            // is a hard reject rather than a degraded send.
            let Some(self_sk) = room_data.signing_key().cloned() else {
                return PreflightOutcome::Reject(SendDmOutcome::IdentityUnavailable);
            };
            let self_id: MemberId = (&self_sk.verifying_key()).into();
            let owner_id = MemberId::from(&room);
            if self_id == peer {
                return PreflightOutcome::Reject(SendDmOutcome::SelfDm);
            }
            let peer_vk = if peer == owner_id {
                room
            } else {
                match room_data
                    .room_state
                    .members
                    .members
                    .iter()
                    .find(|m| m.member.id() == peer)
                    .map(|m| m.member.member_vk)
                {
                    Some(vk) => vk,
                    None => return PreflightOutcome::Reject(SendDmOutcome::RecipientNotMember),
                }
            };
            // NO per-pair cap guard — the contract's cap is newest-N now, so a
            // send at the cap succeeds (evicting the pair's oldest) instead of
            // being silently dropped. See `dm.rs`'s note for the full rationale.
            // Rejoin bundle: matches `dm_thread_modal.rs::do_send`.
            // Bug #1 (Ivvor, 2026-05-16) — pruned-but-invited senders
            // silently fail without this.
            let (rejoin_members, rejoin_member_info) = room_data.build_rejoin_delta();
            let self_in_members = self_id == owner_id
                || room_data
                    .room_state
                    .members
                    .members
                    .iter()
                    .any(|m| m.member.id() == self_id);
            if !self_in_members && rejoin_members.is_none() {
                return PreflightOutcome::Reject(SendDmOutcome::SenderMissingRejoin);
            }
            PreflightOutcome::Ready(Box::new(PreflightSnapshot {
                self_sk,
                self_id,
                peer_vk,
                rejoin_members,
                rejoin_member_info,
            }))
        })();
        let _ = preflight_tx.send(outcome);
    });

    let snapshot = match preflight_rx.await {
        Ok(PreflightOutcome::Ready(s)) => *s,
        Ok(PreflightOutcome::Reject(r)) => return r,
        Err(_) => {
            return SendDmOutcome::DeltaFailed(
                "deferred preflight aborted before completion".into(),
            );
        }
    };
    let PreflightSnapshot {
        self_sk,
        self_id,
        peer_vk,
        rejoin_members,
        rejoin_member_info,
    } = snapshot;

    // Encode the body and capture the plaintext-summary BEFORE moving it.
    let body_bytes = match river_core::room_state::dm_body::encode_body(&body) {
        Ok(b) => b,
        Err(e) => return SendDmOutcome::BodyTooLargeOrEncodeFailed(e),
    };
    let plaintext_summary = summarise_body_for_outbound_cache(&body);

    let now = unix_now();
    let auth = match compose_direct_message(&self_sk, &peer_vk, &room, now, now, &body_bytes) {
        Ok(a) => a,
        Err(e) => return SendDmOutcome::BodyTooLargeOrEncodeFailed(e),
    };

    let purge_token = auth.purge_token();
    let dm_timestamp = auth.message.timestamp;
    let auth_sig = auth.sender_signature;

    let delta = ChatRoomStateV1Delta {
        members: rejoin_members,
        member_info: rejoin_member_info,
        direct_messages: Some(DirectMessagesDelta {
            new_messages: vec![auth],
            advanced_purges: vec![],
        }),
        ..Default::default()
    };
    let params = ChatRoomParametersV1 { owner: room };

    // Apply the delta under a write-lock on ROOMS. We `await` the
    // `apply_delta` result via a oneshot channel because Dioxus signal
    // writes must be deferred (AGENTS.md "Dioxus WASM Signal Safety
    // Rules"). The picker awaits us and reacts to the outcome.
    let (tx, rx) = futures::channel::oneshot::channel::<SendDmOutcome>();
    let plaintext_for_cache = plaintext_summary.clone();
    crate::util::defer(move || {
        let outcome = ROOMS.with_mut(|rooms| {
            let Some(rd) = rooms.map.get_mut(&room) else {
                return SendDmOutcome::RoomGone;
            };
            let parent = rd.room_state.clone();
            if let Err(e) = rd.room_state.apply_delta(&parent, &params, &Some(delta)) {
                return SendDmOutcome::DeltaFailed(format!("{:?}", e));
            }
            // Verify the DM actually landed (defence-in-depth against
            // contract-side silent drop).
            let landed = rd
                .room_state
                .direct_messages
                .messages
                .iter()
                .any(|m| m.sender_signature == auth_sig);
            if !landed {
                return SendDmOutcome::SilentDrop;
            }
            // #310: apply_delta's MessagesV1 step re-runs the public-only
            // rebuild_actions_state, dropping private edits/reactions.
            // Re-derive them with decryption. No-op on public rooms.
            rd.rebuild_private_actions_state();
            SendDmOutcome::Sent
        });

        if matches!(outcome, SendDmOutcome::Sent) {
            mark_needs_sync(room);
            save_outbound_dm(
                room,
                self_id,
                peer,
                purge_token,
                dm_timestamp,
                plaintext_for_cache,
            );
            unhide_dm_thread(room, peer);
            // Scroll an open thread to the message we just sent, exactly as
            // the thread's own composer does. Without this, an invitation
            // sent from the DM thread produced NO feedback at all for a user
            // who had scrolled up: the picker's success banner lives inside
            // the picker and is torn down in the same defer block that closes
            // it, so it renders for zero frames, and the thread simply looked
            // unchanged. This send adds a message, so a fresh bubble mounts
            // and re-runs the auto-scroll effect — the condition
            // `note_outbound_send`'s docs require of any caller.
            dm_thread_modal::note_outbound_send();
        }
        let _ = tx.send(outcome);
    });

    // Wait for the deferred work to land. Defer schedules via
    // setTimeout(0), so this is one macrotask away.
    rx.await.unwrap_or(SendDmOutcome::DeltaFailed(
        "deferred send aborted before completion".into(),
    ))
}

/// Marks a cached outbound summary as an invitation rather than ordinary
/// text. The sender cannot decrypt their own outbound DM (it is ECIES-sealed
/// to the recipient), so this cached string is the ONLY thing their own bubble
/// can be rendered from — this prefix is what lets the renderer tell a sent
/// invitation from an ordinary message.
pub(crate) const OUTBOUND_INVITE_SENTINEL: &str = "[Invitation]";

/// The parts of a cached outbound invite summary, recovered for rendering.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OutboundInviteSummary {
    /// Everything the sender wrote alongside the invitation, verbatim.
    pub(crate) note: Option<String>,
}

/// Compute the plaintext string we cache for the sender's own outbound
/// bubble rendering. For `Text` bodies that's just the text; for `Invite`
/// it's a human-readable summary so the sender's list view doesn't
/// render a blank "Invite (binary payload)" placeholder. The recipient
/// decodes the wire body directly — they never look at this string.
///
/// **This format is deliberately unchanged, and is not a data channel.**
/// The obvious improvement — recording the invited ROOM's name here, so the
/// sender's bubble can name it the way the recipient's card does — was tried
/// and reverted, because this one `String` is a shared display field:
/// `riverctl dm list` prints it as a single line per message
/// (`cli/src/commands/dm.rs`), so a second line there becomes an orphan row
/// with no index, direction or timestamp; and re-parsing a richer format back
/// out mangles legacy entries, whose personal message could itself contain
/// newlines. Naming the room properly needs structured storage in
/// `OutboundDmEntry`, which lives in `river-core` and therefore re-keys the
/// chat delegate — a migration, not a UI change. Tracked separately; do not
/// re-encode fields into this string.
fn summarise_body_for_outbound_cache(
    body: &river_core::room_state::dm_body::DirectMessageBody,
) -> String {
    use river_core::room_state::dm_body::DirectMessageBody;
    match body {
        DirectMessageBody::Text { text } => text.clone(),
        DirectMessageBody::Invite(payload) => match &payload.personal_message {
            Some(msg) if !msg.trim().is_empty() => {
                format!("{} {}", OUTBOUND_INVITE_SENTINEL, msg.trim())
            }
            _ => OUTBOUND_INVITE_SENTINEL.to_string(),
        },
    }
}

/// Recover the parts of a cached outbound invite summary, or `None` when the
/// string is ordinary message text.
///
/// **Known false positive, deliberately accepted:** a message the user TYPED
/// beginning with `"[Invitation]"` is cached verbatim and so parses as one.
/// The sentinel predates this and is the only signal available — the cache
/// stores a bare `String`, with no body-kind field to consult (see
/// [`summarise_body_for_outbound_cache`] for why adding one is a migration).
/// The cost is bounded to a mislabelled card in the sender's OWN thread: the
/// note below carries the rest of the text through verbatim, so nothing the
/// user wrote is hidden, and the recipient always sees the true body. Pinned
/// by `text_that_looks_like_an_invite_keeps_the_users_words`.
pub(crate) fn parse_outbound_invite_summary(summary: &str) -> Option<OutboundInviteSummary> {
    let rest = summary.strip_prefix(OUTBOUND_INVITE_SENTINEL)?;
    // Trim only the separating space; keep the sender's own line breaks, which
    // a legacy personal message may contain.
    let note = rest.trim();
    Some(OutboundInviteSummary {
        note: (!note.is_empty()).then(|| note.to_string()),
    })
}

fn unix_now() -> u64 {
    use std::time::UNIX_EPOCH;
    crate::util::get_current_system_time()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pure helper: compute the max inbound DM timestamp per `(room, peer)` in
/// `rooms`. Split from the signal-touching wrapper so it's unit-testable.
pub(crate) fn compute_dm_last_seen(
    rooms: &crate::room_data::Rooms,
) -> HashMap<(VerifyingKey, MemberId), u64> {
    let mut updates: HashMap<(VerifyingKey, MemberId), u64> = HashMap::new();
    for (owner_vk, room_data) in &rooms.map {
        // Without the local identity we cannot tell inbound DMs from outbound
        // ones, so this room contributes no last-seen entries.
        let Some(self_id) = room_data.self_member_id() else {
            continue;
        };
        for msg in &room_data.room_state.direct_messages.messages {
            if msg.message.recipient != self_id {
                continue;
            }
            let key = (*owner_vk, msg.message.sender);
            let entry = updates.entry(key).or_insert(0);
            if msg.message.timestamp > *entry {
                *entry = msg.message.timestamp;
            }
        }
    }
    updates
}

/// Re-seed [`DM_LAST_SEEN`] for a single room after an identity swap
/// (freenet/river#414 round-10 P2).
///
/// `DM_LAST_SEEN` is keyed only by `(room, peer)`, so the OLD identity's
/// last-seen cutoffs would carry over to the NEW identity's threads and wrongly
/// suppress unread badges for messages the new identity has never seen. This
/// drops the room's stale cutoffs and re-seeds them from the NEW identity's
/// inbound DMs (`compute_dm_last_seen` keys on `room_data.self_sk`, which is now
/// the swapped-in identity) — the same "don't flood history as unread" seed
/// logic `seed_dm_last_seen_if_needed` uses, scoped to one room. Reads the
/// CURRENT (post-swap) rooms snapshot, so the caller must run it AFTER the swap
/// has updated `self_sk`. Signal-safe: the `DM_LAST_SEEN` mutation is deferred.
pub fn reseed_dm_last_seen_for_room(owner_vk: VerifyingKey) {
    let updates = {
        let Ok(rooms) = crate::components::app::ROOMS.try_read() else {
            return;
        };
        compute_dm_last_seen(&rooms)
    };
    crate::util::defer(move || {
        DM_LAST_SEEN.with_mut(|seen| {
            apply_dm_last_seen_reseed(seen, owner_vk, &updates);
        });
    });
}

/// Pure re-seed step for [`reseed_dm_last_seen_for_room`]: drop every existing
/// last-seen cutoff for `owner_vk` (the replaced identity's) and set the new
/// identity's from `updates` (already computed by `compute_dm_last_seen`, so
/// keyed on the swapped-in `self_sk`), leaving other rooms untouched. Pure so
/// the re-seed is unit-testable without the `ROOMS`/`DM_LAST_SEEN` signals.
pub(crate) fn apply_dm_last_seen_reseed(
    seen: &mut HashMap<(VerifyingKey, MemberId), u64>,
    owner_vk: VerifyingKey,
    updates: &HashMap<(VerifyingKey, MemberId), u64>,
) {
    seen.retain(|(room, _), _| *room != owner_vk);
    for ((room, peer), ts) in updates {
        if *room == owner_vk {
            seen.insert((*room, *peer), *ts);
        }
    }
}

/// Initialise [`DM_LAST_SEEN`] from current room state so previously-existing
/// inbound DMs don't show up as "unread" every time the page reloads.
///
/// `DM_LAST_SEEN` is in-memory only — that's an explicit limitation of the
/// first cut, documented in the module header. Without this seeding step,
/// every reload would mark every DM ever received as unread until the user
/// opened each thread, which is much noisier than the room-message badge
/// (which is durable via `last_read_message_id`).
///
/// **Subscription semantics.** The intended caller is a `use_effect` that
/// subscribes to [`crate::components::app::ROOMS`] so it fires the FIRST
/// time `ROOMS` hydrates from the delegate (it's empty on synchronous
/// app-component first render). The internal `DM_LAST_SEEN_SEEDED` flag
/// then makes every subsequent call a no-op: if we re-seeded on every
/// `ROOMS` update, a newly-arrived inbound DM would advance the cutoff to
/// its own timestamp and never appear as unread (Codex review #3 caught
/// this).
///
/// Per-`(room, peer)` last-seen is set to the maximum inbound timestamp
/// in that thread; anything newer than the current state still counts as
/// unread.
pub fn seed_dm_last_seen_if_needed() {
    // Cheap early-exit: if we've already seeded once, do nothing.
    if let Ok(g) = DM_LAST_SEEN_SEEDED.try_read() {
        if *g {
            return;
        }
    } else {
        return;
    };

    let Ok(rooms) = crate::components::app::ROOMS.try_read() else {
        return;
    };
    if rooms.map.is_empty() {
        // ROOMS hasn't hydrated yet; wait for the next ROOMS change.
        return;
    }
    let updates = compute_dm_last_seen(&rooms);
    drop(rooms);

    // Latch the seeded flag synchronously so any parallel re-run of this
    // effect (before the deferred write hits) immediately early-exits.
    // Doing the latch BEFORE the deferred write also avoids the
    // one-render-frame "every historical DM looks unread" window the
    // skeptical reviewer (#258 M3) flagged — consumers reading
    // DM_LAST_SEEN_SEEDED see "seeded, write in flight" rather than
    // "not seeded".
    //
    // Safety: a same-tick re-entry doesn't lose the seed because we
    // already computed `updates` from the just-read rooms snapshot and
    // captured it into the defer closure. The flag latch and the write
    // are conceptually one operation.
    crate::util::defer(move || {
        *DM_LAST_SEEN_SEEDED.write() = true;
        DM_LAST_SEEN.with_mut(|seen| {
            for (key, ts) in updates {
                let entry = seen.entry(key).or_insert(0);
                if ts > *entry {
                    *entry = ts;
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{
        parse_outbound_invite_summary, summarise_body_for_outbound_cache, OutboundInviteSummary,
    };
    use river_core::room_state::dm_body::{DirectMessageBody, InvitePayload};

    fn invite(personal_message: Option<&str>) -> DirectMessageBody {
        DirectMessageBody::Invite(Box::new(InvitePayload {
            room_owner_vk: ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]).verifying_key(),
            invitation_payload: vec![1, 2, 3],
            personal_message: personal_message.map(str::to_string),
        }))
    }

    /// A sent invitation must be recognisable in the sender's own thread —
    /// it is the only signal their bubble has, since the outbound copy is
    /// sealed to the recipient.
    #[test]
    fn a_sent_invite_is_recognisable_in_the_senders_own_thread() {
        let bare = summarise_body_for_outbound_cache(&invite(None));
        assert_eq!(
            parse_outbound_invite_summary(&bare),
            Some(OutboundInviteSummary { note: None })
        );

        let with_note = summarise_body_for_outbound_cache(&invite(Some("come say hi")));
        assert_eq!(
            parse_outbound_invite_summary(&with_note),
            Some(OutboundInviteSummary {
                note: Some("come say hi".to_string()),
            })
        );
    }

    /// The cached summary is ALSO what `riverctl dm list` prints, one line
    /// per message, so it must stay single-line for a single-line note. A
    /// previous revision of this branch put the note on a second line and
    /// turned every invite into an orphan row there, with no index,
    /// direction or timestamp.
    #[test]
    fn a_sent_invite_summary_stays_one_line() {
        let summary = summarise_body_for_outbound_cache(&invite(Some("come say hi")));
        assert!(
            !summary.contains('\n'),
            "the cached summary gained a newline: {summary:?} - riverctl prints \
             this field as one line per message"
        );
    }

    /// A LEGACY personal message could itself contain newlines (the picker's
    /// field is a textarea). Those entries are already in users' delegate
    /// storage and cannot be rewritten, so the note must come back whole —
    /// an earlier revision of this branch returned only the text after the
    /// first newline, silently dropping everything before it.
    #[test]
    fn a_multi_line_note_survives_the_round_trip_intact() {
        let summary = summarise_body_for_outbound_cache(&invite(Some("Hey!\n\nThing on Friday.")));
        assert_eq!(
            parse_outbound_invite_summary(&summary),
            Some(OutboundInviteSummary {
                note: Some("Hey!\n\nThing on Friday.".to_string()),
            }),
            "a multi-line note must not lose the text before its first newline"
        );
    }

    /// The accepted false positive, pinned so it stays a known cost rather
    /// than a surprise — and pinned on the property that BOUNDS it: whatever
    /// the user typed still reaches the screen.
    ///
    /// The previous version of this test was named
    /// `ordinary_text_is_not_mistaken_for_an_invite` and asserted only that
    /// the summariser caches text verbatim. It never fed that output back
    /// into the parser, which is the call that misfires — so it was green
    /// while its name claimed the opposite of the truth.
    #[test]
    fn text_that_looks_like_an_invite_keeps_the_users_words() {
        let typed = "[Invitation] to the pub at 7";
        let body = DirectMessageBody::Text {
            text: typed.to_string(),
        };
        let cached = summarise_body_for_outbound_cache(&body);
        assert_eq!(cached, typed, "text bodies are cached verbatim");

        let parsed = parse_outbound_invite_summary(&cached)
            .expect("this DOES parse as an invite - the accepted false positive");
        assert_eq!(
            parsed.note.as_deref(),
            Some("to the pub at 7"),
            "the user's words must survive into the card, so a mislabelled \
             bubble still shows what they actually wrote"
        );
    }

    /// Ordinary text without the sentinel must never render as an invite.
    #[test]
    fn ordinary_text_does_not_parse_as_an_invite() {
        for text in [
            "hello there",
            "I sent you an [Invitation] earlier",
            "",
            "invitation",
            " [Invitation] leading space",
        ] {
            assert_eq!(
                parse_outbound_invite_summary(text),
                None,
                "{text:?} was parsed as an invite summary"
            );
        }
    }

    /// Issue freenet/river#526: the archive clock is INBOUND-only, so an
    /// outbound send no longer revives an archived thread through the
    /// timestamp filter. The unconditional `unhide_dm_thread` call in
    /// `send_structured_dm` is now the ONLY mechanism that does, which makes it
    /// load-bearing rather than belt-and-braces.
    ///
    /// Routing it through the cutoff-gated `unhide_dm_thread_if_dm_is_newer`
    /// - the "consistency" refactor - would make replying stop reviving
    /// archived threads entirely, with a green suite: at that moment the
    /// thread's inbound clock is by construction at or below the cutoff.
    #[test]
    fn outbound_send_keeps_the_unconditional_unhide() {
        let raw = include_str!("direct_messages.rs");
        let cut = raw
            .find("mod tests {")
            .expect("this file must have a `mod tests {`");
        let src: String = raw[..cut].chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !src.contains(&format!("{}{}", "unhide_dm_thread_if_dm_is_newer", "(")),
            "the outbound send must NOT use the cutoff-gated unhide (#526) - \
             replying would silently stop reviving archived threads."
        );
        assert_eq!(
            src.matches(&format!("{}{}", "unhide_dm_thread", "("))
                .count(),
            1,
            "the outbound send must keep calling the unconditional \
             unhide_dm_thread (#526); it is the only remaining mechanism that \
             revives an archived thread on reply."
        );
    }
    /// The DM send path in this file must NOT carry a client-side per-pair cap
    /// guard.
    ///
    /// It used to block at `MAX_DM_MESSAGES_PER_PAIR` because the contract's
    /// cap was first-come-wins and silently discarded the arrival. The cap is
    /// newest-N now, so a send at the cap succeeds (evicting the pair's oldest).
    /// Re-adding a block here would leave the user unable to send for a purely
    /// client-side reason and make the contract fix invisible — and this is the
    /// UI, so that is nearly the whole population.
    ///
    /// Source-scrape rather than behavioural: the send path needs a Dioxus
    /// runtime and a live node. Three deliberate choices:
    ///
    /// * **Whole non-test body, not one function.** The CLI's equivalent pin was
    ///   first written scoped to `execute_send` while the guard actually lived
    ///   in `deliver_dm`, so it passed while the guard sat untouched. Scope-by-
    ///   function is how these go vacuous.
    /// * **Cut at `mod tests`** so this comment and the assertion below cannot
    ///   satisfy their own check. Verified: this file has exactly ONE
    ///   `mod tests`, and it is the real module — the cut point resolving
    ///   correctly is a property of the file, not a guarantee of the technique.
    /// * **BOTH symbols**, not just `pair_message_count`. Neither has any
    ///   legitimate non-test use left in this file, so keying on
    ///   `MAX_DM_MESSAGES_PER_PAIR` too also catches a hand-rolled
    ///   `.filter(..).count() >= MAX_DM_MESSAGES_PER_PAIR`, which a
    ///   symbol-only pin would miss. (The CLI pin cannot do this — it uses the
    ///   constant legitimately for outbound-cache pruning.)
    ///
    /// Residual limitation: a guard that inlines the literal 100 and counts by
    /// hand still slips past. That is tolerated; every realistic re-add goes
    /// through one of these two symbols.
    #[test]
    fn dm_send_path_has_no_client_side_pair_cap_guard() {
        let src = include_str!("direct_messages.rs");
        let body = &src[..src.find("mod tests").unwrap_or(src.len())];
        for symbol in ["pair_message_count(", "MAX_DM_MESSAGES_PER_PAIR"] {
            assert!(
                !body.contains(symbol),
                "direct_messages.rs references `{symbol}` outside its test module, which means a \
                 client-side per-pair DM cap guard has been re-added. The contract \
                 evicts the pair's oldest DM to admit a newer one now, so blocking \
                 the send here makes that fix invisible to UI users."
            );
        }
    }

    /// Issue #499 write-pulse: `mark_thread_read` is called from
    /// `DmThreadModalBody`'s render body on every render while a thread
    /// is open, and `with_mut` notifies subscribers even when the
    /// mutation is a no-op — so the `with_mut` MUST stay gated on
    /// `thread_read_needs_write`. Source-scrape (the function needs a
    /// Dioxus runtime to exercise): match whitespace-stripped source so
    /// rustfmt reflowing can't fake a failure; cut at `mod tests` (this
    /// file has exactly one) so these needles can't satisfy their own
    /// check; bound the segment by the NEXT function head so a gate
    /// elsewhere in the file can't vacuously pass.
    #[test]
    fn mark_thread_read_write_is_gated_pinned() {
        let src = include_str!("direct_messages.rs");
        let body = &src[..src.find("mod tests").unwrap_or(src.len())];
        let stripped: String = body.chars().filter(|c| !c.is_whitespace()).collect();

        let start = stripped
            .find("pubfnmark_thread_read")
            .expect("mark_thread_read not found");
        let end = stripped[start..]
            .find("pubfnopen_dm_thread")
            .map(|i| start + i)
            .expect("open_dm_thread must follow mark_thread_read");
        let seg = &stripped[start..end];

        let gate = seg
            .find("if!needs_write{return;}")
            .expect("mark_thread_read must early-return when the cutoff would not advance");
        let decide = seg
            .find("thread_read_needs_write(")
            .expect("mark_thread_read must decide via thread_read_needs_write");
        let write = seg
            .find(".with_mut(")
            .expect("mark_thread_read must still perform the gated write");
        assert!(
            decide < gate && gate < write,
            "mark_thread_read's with_mut must come AFTER the needs-write gate \
             (decide at {decide}, gate at {gate}, write at {write}) — an ungated \
             with_mut notifies DM rail subscribers on every render of an open \
             thread (issue #499 write-pulse)"
        );

        // The contended-peek fallback must be FALSE (skip the write).
        // `try_peek` fails only while a live WRITE borrow exists on
        // DM_LAST_SEEN, and on that same synchronous stack `with_mut`
        // is a panicking borrow — so a `true` fallback converts every
        // contended peek into a guaranteed RefCell panic. `false` is
        // safe: mark_thread_read re-fires on every render of the open
        // thread, so a skipped advance self-heals on the next clean
        // pass. (Needles built with concat! so this comment and the
        // assertion literals cannot drift into matching themselves —
        // they sit inside the cut anyway, belt and braces.)
        let fallback_false = concat!(".unwrap_or(", "false)");
        let fallback_true = concat!(".unwrap_or(", "true)");
        assert!(
            seg.contains(fallback_false),
            "mark_thread_read's try_peek fallback must be `false` (skip on \
             contention) — a contended peek means a live write borrow, and \
             falling through to with_mut on that stack panics"
        );
        assert!(
            !seg.contains(fallback_true),
            "mark_thread_read must NOT fall back to `true` on a contended \
             try_peek — that is the guaranteed-panic path (with_mut on a \
             signal whose write borrow is live)"
        );
    }

    /// Decision table for the issue #499 write-pulse gate.
    #[test]
    fn thread_read_needs_write_only_when_cutoff_advances() {
        // No stored entry: any positive timestamp advances the cutoff…
        assert!(thread_read_needs_write(None, 1));
        // …but a zero timestamp does not (absent reads as 0 everywhere).
        assert!(!thread_read_needs_write(None, 0));
        // Equal to the stored cutoff: no-op — this is the exact case the
        // render body hits on every re-render while a thread is open.
        assert!(!thread_read_needs_write(Some(100), 100));
        // Older than the stored cutoff: no-op.
        assert!(!thread_read_needs_write(Some(100), 99));
        // Strictly newer: write.
        assert!(thread_read_needs_write(Some(100), 101));
    }

    use super::*;
    use crate::room_data::{RoomData, Rooms};
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use freenet_stdlib::prelude::{ContractCode, ContractKey, Parameters};
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::direct_messages::sign_direct_message;
    use river_core::room_state::member::{AuthorizedMember, Member, MembersV1};
    use river_core::ChatRoomStateV1;

    fn empty_rooms() -> Rooms {
        Rooms {
            map: std::collections::HashMap::new(),
            current_room_key: None,
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
            removed_rooms: std::collections::HashSet::new(),
            room_order: Vec::new(),
        }
    }

    fn fixed_sk(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// Build a `Rooms` with one room owned by `owner_sk`, with
    /// `self_sk` as the local user enrolled as a member.
    fn make_rooms(owner_sk: &SigningKey, self_sk: &SigningKey, other_sks: &[&SigningKey]) -> Rooms {
        let owner_vk = owner_sk.verifying_key();
        let owner_id: MemberId = (&owner_vk).into();
        let mut members: Vec<AuthorizedMember> = vec![AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: self_sk.verifying_key(),
            },
            owner_sk,
        )];
        for s in other_sks {
            members.push(AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: s.verifying_key(),
                },
                owner_sk,
            ));
        }
        let auth_config = AuthorizedConfigurationV1::new(Configuration::default(), owner_sk);
        let state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 { members },
            ..Default::default()
        };
        let mut rooms = empty_rooms();
        let contract_code = ContractCode::from(crate::constants::ROOM_CONTRACT_WASM);
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(crate::util::to_cbor_vec(
                &river_core::room_state::ChatRoomParametersV1 { owner: owner_vk },
            )),
            &contract_code,
        );
        rooms.map.insert(
            owner_vk,
            RoomData {
                owner_vk,
                room_state: state,
                self_sk: Some(self_sk.clone()),
                self_vk: None,
                contract_key,
                last_read_message_id: None,
                secrets: std::collections::HashMap::new(),
                current_secret_version: None,
                last_secret_rotation: None,
                key_migrated_to_delegate: false,
                self_authorized_member: None,
                invite_chain: Vec::new(),
                self_member_info: None,
                self_nickname: None,
                previous_contract_key: None,
                invitation_secrets: std::collections::BTreeMap::new(),
            },
        );
        rooms
    }

    fn push_dm(
        rooms: &mut Rooms,
        room_owner_vk: &VerifyingKey,
        sender_sk: &SigningKey,
        recipient_vk: &VerifyingKey,
        ts: u64,
    ) {
        let auth = sign_direct_message(
            sender_sk,
            (&sender_sk.verifying_key()).into(),
            recipient_vk.into(),
            room_owner_vk,
            ts,
            b"opaque".to_vec(),
        )
        .expect("sign_direct_message");
        rooms
            .map
            .get_mut(room_owner_vk)
            .unwrap()
            .room_state
            .direct_messages
            .messages
            .push(auth);
    }

    #[test]
    fn compute_dm_last_seen_returns_empty_for_empty_rooms() {
        let updates = compute_dm_last_seen(&empty_rooms());
        assert!(updates.is_empty());
    }

    /// freenet/river#414 (Codex round-10 P2): on an identity swap, the room's
    /// stale last-seen cutoffs (the OLD identity's) must be dropped and replaced
    /// by the NEW identity's, so unread badges reflect the new identity's actual
    /// seen-state — while OTHER rooms are untouched.
    #[test]
    fn apply_dm_last_seen_reseed_replaces_room_only() {
        let room_a = fixed_sk(1).verifying_key();
        let room_b = fixed_sk(2).verifying_key();
        let old_peer: MemberId = (&fixed_sk(3).verifying_key()).into();
        let new_peer: MemberId = (&fixed_sk(4).verifying_key()).into();
        let other_peer: MemberId = (&fixed_sk(5).verifying_key()).into();

        let mut seen: HashMap<(VerifyingKey, MemberId), u64> = HashMap::new();
        // Room A: the OLD identity's cutoffs.
        seen.insert((room_a, old_peer), 100);
        // Room B: an unrelated room's cutoff (must survive).
        seen.insert((room_b, other_peer), 50);

        // The NEW identity's computed last-seen for room A only.
        let mut updates: HashMap<(VerifyingKey, MemberId), u64> = HashMap::new();
        updates.insert((room_a, new_peer), 200);
        // (compute_dm_last_seen also returns other rooms; the helper must ignore them.)
        updates.insert((room_b, other_peer), 999);

        apply_dm_last_seen_reseed(&mut seen, room_a, &updates);

        // Room A's OLD-identity cutoff is gone; the NEW identity's is seeded.
        assert!(
            !seen.contains_key(&(room_a, old_peer)),
            "the replaced identity's last-seen must be dropped"
        );
        assert_eq!(seen.get(&(room_a, new_peer)), Some(&200));
        // Room B is untouched (NOT overwritten by the ignored update).
        assert_eq!(seen.get(&(room_b, other_peer)), Some(&50));
    }

    #[test]
    fn compute_dm_last_seen_only_counts_inbound_to_self() {
        let owner = fixed_sk(1);
        let me = fixed_sk(2);
        let alice = fixed_sk(3);
        let bob = fixed_sk(4);
        let owner_vk = owner.verifying_key();
        let mut rooms = make_rooms(&owner, &me, &[&alice, &bob]);

        // Alice -> me at ts 100; Bob -> me at ts 200; me -> Alice at ts 250
        // (outbound; must NOT contribute).
        push_dm(&mut rooms, &owner_vk, &alice, &me.verifying_key(), 100);
        push_dm(&mut rooms, &owner_vk, &bob, &me.verifying_key(), 200);
        push_dm(&mut rooms, &owner_vk, &me, &alice.verifying_key(), 250);

        let updates = compute_dm_last_seen(&rooms);
        let alice_id: MemberId = (&alice.verifying_key()).into();
        let bob_id: MemberId = (&bob.verifying_key()).into();
        assert_eq!(updates.get(&(owner_vk, alice_id)), Some(&100));
        assert_eq!(updates.get(&(owner_vk, bob_id)), Some(&200));
        // Outbound DM: must NOT seed against my own self_id.
        let me_id: MemberId = (&me.verifying_key()).into();
        assert!(!updates.contains_key(&(owner_vk, me_id)));
    }

    #[test]
    fn compute_dm_last_seen_takes_max_per_peer() {
        let owner = fixed_sk(11);
        let me = fixed_sk(12);
        let alice = fixed_sk(13);
        let owner_vk = owner.verifying_key();
        let mut rooms = make_rooms(&owner, &me, &[&alice]);

        // Three DMs from Alice; the helper must pick the max.
        push_dm(&mut rooms, &owner_vk, &alice, &me.verifying_key(), 100);
        push_dm(&mut rooms, &owner_vk, &alice, &me.verifying_key(), 1_000);
        push_dm(&mut rooms, &owner_vk, &alice, &me.verifying_key(), 500);

        let updates = compute_dm_last_seen(&rooms);
        let alice_id: MemberId = (&alice.verifying_key()).into();
        assert_eq!(updates.get(&(owner_vk, alice_id)), Some(&1_000));
    }

    fn sample_outbound_entry(
        room_vk: VerifyingKey,
        recipient: MemberId,
        token: PurgeToken,
        plaintext: &str,
    ) -> OutboundDmEntry {
        OutboundDmEntry {
            room_owner_vk: room_vk.to_bytes(),
            sender: MemberId::from(&fixed_sk(99).verifying_key()),
            recipient,
            purge_token: token,
            timestamp: 1_700_000_000,
            plaintext: plaintext.to_string(),
        }
    }

    /// Issue freenet/river#256 regression: a cache hit on
    /// `(room, recipient, purge_token)` MUST return the original
    /// plaintext so the sender's own outbound bubble renders as
    /// markdown prose instead of the "sent — ciphertext only"
    /// placeholder. Pins the load-bearing lookup tuple — any future
    /// refactor that drops a field from the key (or swaps key order)
    /// fails this test.
    #[test]
    fn dm_outbound_lookup_returns_plaintext_on_hit() {
        let room_vk = fixed_sk(1).verifying_key();
        let recipient: MemberId = (&fixed_sk(2).verifying_key()).into();
        let token = PurgeToken([0x42; 16]);

        let mut cache = OutboundDmsCache::default();
        cache.insert_for_test(sample_outbound_entry(room_vk, recipient, token, "hello!"));

        let resolved = lookup_outbound_plaintext(&cache, &room_vk, &recipient, &token);
        assert_eq!(resolved, Ok("hello!".to_string()));
    }

    /// Issue freenet/river#256 regression: a cache miss MUST surface
    /// as `Err(())` so the caller falls back to the legacy
    /// `"sent — ciphertext only"` placeholder. Three miss-scenarios
    /// pin all three components of the lookup tuple are load-bearing.
    #[test]
    fn dm_outbound_lookup_returns_err_on_miss() {
        let room_vk = fixed_sk(1).verifying_key();
        let other_room_vk = fixed_sk(7).verifying_key();
        let recipient: MemberId = (&fixed_sk(2).verifying_key()).into();
        let other_recipient: MemberId = (&fixed_sk(3).verifying_key()).into();
        let token = PurgeToken([0x42; 16]);
        let other_token = PurgeToken([0xff; 16]);

        let mut cache = OutboundDmsCache::default();
        cache.insert_for_test(sample_outbound_entry(room_vk, recipient, token, "hello!"));

        // Wrong room.
        assert_eq!(
            lookup_outbound_plaintext(&cache, &other_room_vk, &recipient, &token),
            Err(())
        );
        // Wrong recipient.
        assert_eq!(
            lookup_outbound_plaintext(&cache, &room_vk, &other_recipient, &token),
            Err(())
        );
        // Wrong token.
        assert_eq!(
            lookup_outbound_plaintext(&cache, &room_vk, &recipient, &other_token),
            Err(())
        );
        // Empty cache.
        let empty = OutboundDmsCache::default();
        assert_eq!(
            lookup_outbound_plaintext(&empty, &room_vk, &recipient, &token),
            Err(())
        );
    }
}
