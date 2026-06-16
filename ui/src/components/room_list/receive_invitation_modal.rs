use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerMessage;
use crate::components::app::{PENDING_INVITES, ROOMS, SYNCHRONIZER};
use crate::components::members::Invitation;
use crate::invites::{PendingRoomJoin, PendingRoomStatus};
use crate::room_data::Rooms;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::{AuthorizedMember, MemberId};
use std::cell::RefCell;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsValue;

thread_local! {
    /// In-memory mirror of the processed-invitation list for read-after-write
    /// consistency within a single page load. The shell's `hash` handler uses
    /// `history.replaceState`, which by design does NOT fire `hashchange` or
    /// `popstate` (so the shell-bridge's own forwardHash listener doesn't
    /// loop). That means after we send a new hash, the iframe's own
    /// `window.location.hash` is NOT updated, and a subsequent
    /// `mark_invitation_processed` would re-read the stale, pre-update
    /// payload from `location.hash`, append a single fingerprint to the OLD
    /// list, and overwrite the parent's URL, losing the prior write. The
    /// cache breaks that lost-update race by serving as the in-process
    /// source of truth once we've initialised it from the URL hash.
    /// `None` until first read; populated lazily.
    static PROCESSED_CACHE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

const INVITATION_STORAGE_KEY: &str = "river_pending_invitation";
/// Sibling key to [`INVITATION_STORAGE_KEY`] holding the nickname the user
/// chose when they accepted the pending invitation. Persisting it lets a
/// reload mid-subscription auto-resume `accept_invitation` (re-populating the
/// in-memory `PENDING_INVITES` so the "Subscribing…" indicator returns)
/// instead of re-prompting for a nickname (#218). Only written once the user
/// has clicked Accept — a reload *before* Accept still shows the nickname
/// prompt, matching the fingerprint guard's "mark only on definitive action"
/// rule.
const INVITATION_NICKNAME_STORAGE_KEY: &str = "river_pending_invitation_nickname";
/// Fingerprint of the invitation the saved nickname belongs to. Without this
/// binding, a stale nickname from a previously-accepted invitation A could be
/// applied to a *different* invitation B that was opened (but not accepted)
/// and overwrote `INVITATION_STORAGE_KEY` before A's subscription cleared
/// storage — auto-accepting B with A's nickname (Codex review, PR #333). The
/// nickname is only returned when this fingerprint matches the recovered
/// invitation's fingerprint.
const INVITATION_NICKNAME_FP_STORAGE_KEY: &str = "river_pending_invitation_nickname_fp";
/// Prefix that identifies River's processed-invitation list inside the
/// top-level URL hash. Format: `#river-processed=fp1,fp2,fp3`.
///
/// We intentionally use the URL hash rather than localStorage because the
/// gateway iframe runs with `sandbox="allow-scripts allow-forms allow-popups"`
/// (no `allow-same-origin`). Opaque-origin documents cannot read or write
/// `localStorage`. `window.localStorage` throws `SecurityError`. The hash is
/// part of the top-level URL, persists across reload, and is propagated into
/// the iframe by the gateway shell on every load (see `SHELL_BRIDGE_JS` in
/// freenet-core's `path_handlers.rs`: `iframeSrc += location.hash`). The
/// iframe cannot rewrite the top-level URL itself, but the shell already
/// exposes a `{__freenet_shell__: true, type: 'hash', hash: '#...'}`
/// postMessage handler that does, so we use that to update the hash.
const PROCESSED_HASH_PREFIX: &str = "#river-processed=";
/// Cap on the number of remembered invitation fingerprints. With 16-byte
/// fingerprints (32 hex chars + 1 separator) this caps the hash payload at
/// roughly 1.1 KB, well under the 8 KiB shell-bridge slice and any
/// reasonable browser URL limit.
const MAX_PROCESSED_INVITATIONS: usize = 32;

/// Cross-component request to surface a [`ReceiveInvitationModal`] for an
/// invitation that did NOT arrive via the URL bar — currently used by the
/// in-app "Accept" button on a DM-delivered invite card (see
/// [`river_core::room_state::dm_body::DirectMessageBody::Invite`]). `App`
/// watches this signal via a `use_effect`, copies the invitation into its
/// local `receive_invitation` signal (the one wired into the modal's
/// `invitation` prop), and clears the global synchronously.
///
/// Why a separate signal rather than making the local `receive_invitation`
/// signal global: the local signal owns the "URL-bar invitation" lifecycle
/// (parse → localStorage save → modal display → user action → clear).
/// Splitting that lifecycle from the in-app trigger keeps the URL parse path
/// unchanged and avoids "two writers, one signal, fights over clear timing"
/// — the bridge effect in `App` is the only writer to the local signal.
pub static PRESENT_INVITATION_REQUEST: GlobalSignal<Option<Invitation>> = Global::new(|| None);

/// Surface the `ReceiveInvitationModal` for `inv` from anywhere in the app
/// (currently called by the DM-thread "Accept" button — see
/// [`river_core::room_state::dm_body::DirectMessageBody::Invite`]).
///
/// Mirrors the URL-bar flow: stashes the invitation in localStorage so a
/// mid-flow reload restores the modal, then asks `App` to display it. We do
/// NOT check `is_invitation_processed` here — the caller (the Accept button)
/// already had the user click intentionally, and if the modal pops up
/// already-acted-upon we'd rather show the "already a member" branch than
/// silently no-op.
pub fn present_invitation(inv: Invitation) {
    save_invitation_to_storage(&inv);
    crate::util::defer(move || {
        *PRESENT_INVITATION_REQUEST.write() = Some(inv);
    });
}

/// Save invitation to localStorage so it survives page reloads.
///
/// Clears any previously-saved nickname binding first: this is the single
/// chokepoint every fresh-invitation save path (URL bar, click interceptor,
/// DM-card present, and `accept_invitation` itself) goes through, so a stale
/// nickname from a previously-accepted invitation can never linger to be
/// applied to a different invitation that overwrites the invitation key. The
/// fingerprint binding in `load_invitation_nickname_from_storage` is the
/// authoritative guard; this clear is defense-in-depth that keeps storage
/// honest. `accept_invitation` re-saves the nickname immediately after, so its
/// own binding survives. (Codex review, PR #333.)
pub fn save_invitation_to_storage(invitation: &Invitation) {
    clear_invitation_nickname_from_storage();
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let encoded = invitation.to_encoded_string();
            if let Err(e) = storage.set_item(INVITATION_STORAGE_KEY, &encoded) {
                warn!("Failed to save invitation to localStorage: {:?}", e);
            }
        }
    }
}

/// Load invitation from localStorage (for recovery after page reload)
pub fn load_invitation_from_storage() -> Option<Invitation> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok()??;
    let encoded = storage.get_item(INVITATION_STORAGE_KEY).ok()??;
    Invitation::from_encoded_string(&encoded).ok()
}

/// Clear saved invitation (and any saved nickname + its fingerprint binding)
/// from localStorage.
pub fn clear_invitation_from_storage() {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item(INVITATION_STORAGE_KEY);
            let _ = storage.remove_item(INVITATION_NICKNAME_STORAGE_KEY);
            let _ = storage.remove_item(INVITATION_NICKNAME_FP_STORAGE_KEY);
        }
    }
}

/// Remove only the saved nickname binding, leaving the invitation artifact in
/// place. Called from `save_invitation_to_storage` so a stale nickname from a
/// previously accepted invitation can never auto-accept a different,
/// not-yet-accepted invitation that overwrote `INVITATION_STORAGE_KEY` (Codex
/// review, PR #333).
fn clear_invitation_nickname_from_storage() {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item(INVITATION_NICKNAME_STORAGE_KEY);
            let _ = storage.remove_item(INVITATION_NICKNAME_FP_STORAGE_KEY);
        }
    }
}

/// Save the accepted nickname alongside the pending invitation so a reload
/// mid-subscription can auto-resume the join without re-prompting (#218).
/// `invitation_encoded` is the canonical `Invitation::to_encoded_string()`;
/// its fingerprint is stored too so the nickname is only ever applied back to
/// the same invitation it was chosen for.
pub fn save_invitation_nickname_to_storage(invitation_encoded: &str, nickname: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            if let Err(e) = storage.set_item(INVITATION_NICKNAME_STORAGE_KEY, nickname) {
                warn!(
                    "Failed to save invitation nickname to localStorage: {:?}",
                    e
                );
                return;
            }
            let fp = invitation_fingerprint(invitation_encoded);
            if let Err(e) = storage.set_item(INVITATION_NICKNAME_FP_STORAGE_KEY, &fp) {
                warn!(
                    "Failed to save invitation nickname fingerprint to localStorage: {:?}",
                    e
                );
                // Leave no half-written binding: drop the nickname too so the
                // resume path falls back to prompting rather than applying an
                // unbound nickname.
                let _ = storage.remove_item(INVITATION_NICKNAME_STORAGE_KEY);
            }
        }
    }
}

/// What the app should do with an invitation recovered from localStorage on
/// page load. Pure decision so the three-way branch is testable on the host
/// without a browser (the storage reads that feed it are wasm-only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveredInvitationAction {
    /// User had clicked Accept (a nickname was saved); re-run `accept_invitation`
    /// with this nickname to resume the in-flight subscription (#218).
    Resume { nickname: String },
    /// User already acted on this invitation in this browser and there is no
    /// in-flight join to resume; drop it and clear storage.
    Discard,
    /// User reloaded before deciding; re-open the modal at the nickname prompt.
    Prompt,
}

/// Decide what to do with a recovered invitation given the two persisted
/// signals: whether a nickname was saved alongside it (meaning the user had
/// clicked Accept), and whether the invitation's fingerprint is already in the
/// processed set.
///
/// A saved nickname takes precedence over the processed flag: a nickname is
/// only persisted once the user has clicked Accept, and the invitation +
/// nickname stay in storage until the room subscribes or the user dismisses
/// (both clear them together). So "invitation still in storage + nickname
/// present" is the authoritative "accepted but join not yet finished, resume
/// it" signal. Note the processed flag is NOT a reliable mid-flight signal:
/// `accept_invitation` does NOT mark the invitation processed (the mark now
/// happens only at terminal success, in `render_subscribed_state`, or on
/// dismiss), so a reload mid-subscription sees `already_processed == false`.
/// The `None if already_processed => Discard` arm therefore only fires for an
/// invitation that reached a terminal state in a prior session yet somehow
/// still has its artifact in storage — defense-in-depth, not the common path.
pub fn decide_recovered_invitation(
    saved_nickname: Option<String>,
    already_processed: bool,
) -> RecoveredInvitationAction {
    match saved_nickname {
        Some(nickname) => RecoveredInvitationAction::Resume { nickname },
        None if already_processed => RecoveredInvitationAction::Discard,
        None => RecoveredInvitationAction::Prompt,
    }
}

/// Whether a stored nickname-binding fingerprint belongs to `invitation_encoded`.
/// Extracted so the binding check is host-testable without a browser.
fn nickname_belongs_to_invitation(stored_fp: &str, invitation_encoded: &str) -> bool {
    stored_fp == invitation_fingerprint(invitation_encoded)
}

/// Check-and-set a one-shot flag: returns `true` exactly once (the first call),
/// `false` on every subsequent call. Used by the `App` body to fire the #218
/// auto-resume at most once per page load — the recovery block runs on every
/// re-render and the resume's side effects (re-send `AcceptInvitation`, reset
/// `PENDING_INVITES`) must NOT repeat, or they loop (Codex review, PR #333).
pub fn take_resume_once(fired: &std::cell::Cell<bool>) -> bool {
    if fired.get() {
        false
    } else {
        fired.set(true);
        true
    }
}

/// Load the saved nickname for `invitation_encoded`, if the user already
/// accepted (i.e. clicked Accept) *this* invitation before reloading. Returns
/// `None` — meaning "prompt for a nickname" — when no nickname is stored, the
/// stored nickname is blank, or the stored fingerprint does not match this
/// invitation (a stale nickname from a different invitation; Codex review,
/// PR #333).
pub fn load_invitation_nickname_from_storage(invitation_encoded: &str) -> Option<String> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok()??;
    let nickname = storage.get_item(INVITATION_NICKNAME_STORAGE_KEY).ok()??;
    let stored_fp = storage
        .get_item(INVITATION_NICKNAME_FP_STORAGE_KEY)
        .ok()??;
    // The nickname must belong to THIS invitation. Without the match, a
    // nickname saved for invitation A would be applied to a different
    // invitation B that overwrote the invitation key before A's join cleared
    // storage.
    if !nickname_belongs_to_invitation(&stored_fp, invitation_encoded) {
        return None;
    }
    // Treat an empty/whitespace-only stored value as "no nickname" so we fall
    // back to the prompt rather than auto-resuming with a blank nickname.
    if nickname.trim().is_empty() {
        None
    } else {
        Some(nickname)
    }
}

/// Short, stable identifier for an invitation, suitable for storage in a set.
/// 16 bytes of BLAKE3 over the encoded form gives a 2^128 collision space.
fn invitation_fingerprint(encoded: &str) -> String {
    let hash = blake3::hash(encoded.as_bytes());
    let bytes = &hash.as_bytes()[..16];
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Returns the current set of processed-invitation fingerprints, lazily
/// initialised from the iframe's URL hash on first call. See
/// `PROCESSED_CACHE` for why an in-memory mirror is required for
/// read-after-write consistency.
fn read_processed_list() -> Vec<String> {
    PROCESSED_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.is_none() {
            *cache = Some(read_processed_from_window_hash());
        }
        cache.as_ref().cloned().unwrap_or_default()
    })
}

/// Read the processed-invitation list straight from the browser. Used only
/// to seed `PROCESSED_CACHE` on first access. Returns an empty Vec if the
/// hash is missing, has the wrong prefix, or is malformed; never panics,
/// since the hash is user-influenced data.
#[cfg(target_arch = "wasm32")]
fn read_processed_from_window_hash() -> Vec<String> {
    let Some(window) = web_sys::window() else {
        return Vec::new();
    };
    let Ok(hash) = window.location().hash() else {
        return Vec::new();
    };
    parse_processed_hash(&hash)
}

/// Native fallback used by the host-runnable unit tests. `web_sys::window()`
/// panics on non-WASM targets (`cannot access imported statics`), so the
/// tests start from an empty seed and exercise the cache directly.
#[cfg(not(target_arch = "wasm32"))]
fn read_processed_from_window_hash() -> Vec<String> {
    Vec::new()
}

/// Pure helper for parsing the hash. Extracted so the hash format is
/// testable without a browser environment.
fn parse_processed_hash(hash: &str) -> Vec<String> {
    let Some(payload) = hash.strip_prefix(PROCESSED_HASH_PREFIX) else {
        return Vec::new();
    };
    payload
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Build the hash string for a list of fingerprints. Empty list collapses to
/// the empty string so callers can clear the hash entirely if desired.
fn build_processed_hash(list: &[String]) -> String {
    if list.is_empty() {
        String::new()
    } else {
        format!("{}{}", PROCESSED_HASH_PREFIX, list.join(","))
    }
}

/// Persist the processed-invitation list. Updates the in-memory cache
/// synchronously (so subsequent reads observe the write within the same
/// page load) and asks the gateway shell to update the top-level URL hash
/// via postMessage. The shell calls `history.replaceState` from its
/// same-origin context, which is the only way to influence the top-level
/// URL from inside the sandboxed iframe.
fn write_processed_list(list: &[String]) {
    PROCESSED_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(list.to_vec());
    });
    persist_processed_list(list);
}

#[cfg(target_arch = "wasm32")]
fn persist_processed_list(list: &[String]) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let parent = match window.parent() {
        Ok(Some(parent)) => parent,
        _ => {
            // No parent window: probably running in a standalone dx-serve dev
            // server, not inside the gateway shell. Mutate our own hash
            // directly so behaviour is consistent across deployments.
            let _ = window.location().set_hash(&build_processed_hash(list));
            return;
        }
    };
    // `Window::eq` on web-sys delegates to `JsValue` referential equality,
    // which matches the JS `parent === window` test for the top-level
    // window. If parent === self we have no shell to talk to and fall back
    // to the dx-serve path.
    if parent == window {
        let _ = window.location().set_hash(&build_processed_hash(list));
        return;
    }

    let new_hash = build_processed_hash(list);
    // The shell's hash handler requires a leading '#'. When clearing, send
    // '#' rather than '' so the shell collapses the hash to empty rather
    // than rejecting the message.
    let hash_for_shell = if new_hash.is_empty() {
        "#".to_string()
    } else {
        new_hash
    };

    let msg = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &msg,
        &JsValue::from_str("__freenet_shell__"),
        &JsValue::TRUE,
    );
    let _ = js_sys::Reflect::set(&msg, &JsValue::from_str("type"), &JsValue::from_str("hash"));
    let _ = js_sys::Reflect::set(
        &msg,
        &JsValue::from_str("hash"),
        &JsValue::from_str(&hash_for_shell),
    );
    // Wildcard target origin: a sandboxed iframe does not know the parent's
    // origin (it has its own opaque origin and the parent could be any
    // gateway). The shell-bridge filters by sender identity
    // (`event.source !== iframe.contentWindow`) rather than `event.origin`,
    // so `'*'` is correct here and not a security regression.
    if let Err(e) = parent.post_message(&msg, "*") {
        warn!("Failed to postMessage processed-invitation hash: {:?}", e);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn persist_processed_list(_list: &[String]) {
    // Host tests only exercise the in-memory cache; the browser-side
    // postMessage path is exercised by the manual Playwright suite documented
    // in the PR.
}

/// Append `fingerprint` to `list`, deduplicating and capping length.
/// Returns `Some(new_list)` if anything changed, `None` if already present.
fn append_fingerprint(
    mut list: Vec<String>,
    fingerprint: String,
    cap: usize,
) -> Option<Vec<String>> {
    if list.contains(&fingerprint) {
        return None;
    }
    list.push(fingerprint);
    if list.len() > cap {
        let drop = list.len() - cap;
        list.drain(0..drop);
    }
    Some(list)
}

/// Record that the user has acted (Accept or any dismiss) on an invitation
/// so a subsequent reload of the same `?invitation=...` URL does not
/// re-prompt for a nickname. The fingerprint is appended to River's slice of
/// the top-level URL hash via the gateway shell's postMessage bridge. See
/// the `PROCESSED_HASH_PREFIX` constant for why localStorage is unsuitable.
///
/// Called on definitive user actions only (Accept, Decline, Cancel, Close,
/// Dismiss-on-error). Recording on URL parse instead would mean a user who
/// reloads before deciding could never re-open the modal, which is a UX
/// regression compared to pre-PR behaviour.
pub fn mark_invitation_processed(encoded: &str) {
    let fingerprint = invitation_fingerprint(encoded);
    let list = read_processed_list();
    if let Some(new_list) = append_fingerprint(list, fingerprint, MAX_PROCESSED_INVITATIONS) {
        write_processed_list(&new_list);
    }
}

/// Returns true if `encoded` matches an invitation the user has previously
/// accepted or dismissed in this top-level page session. Used by the URL
/// parser to skip stale `?invitation=...` params that the iframe cannot
/// strip itself (because `history.replaceState` requires same-origin and the
/// iframe runs in an opaque origin).
pub fn is_invitation_processed(encoded: &str) -> bool {
    let fingerprint = invitation_fingerprint(encoded);
    read_processed_list().iter().any(|f| f == &fingerprint)
}

/// Dismiss the modal *persistently*: append the invitation's fingerprint to
/// the top-level URL hash (via the shell postMessage bridge), clear it from
/// `INVITATION_STORAGE_KEY`, and close the modal. Use this for every
/// user-initiated dismiss (Decline, Cancel, Close, Dismiss-on-error).
/// Without the fingerprint mark, a reload of the same `?invitation=...` URL
/// would re-surface the modal; the iframe cannot strip its own URL because
/// it runs in an opaque origin.
fn dismiss_invitation_persistently(inv: &Invitation, mut invitation: Signal<Option<Invitation>>) {
    mark_invitation_processed(&inv.to_encoded_string());
    clear_invitation_from_storage();
    invitation.set(None);
}

/// Main component for the invitation modal
#[component]
pub fn ReceiveInvitationModal(invitation: Signal<Option<Invitation>>) -> Element {
    // No event listener needed — PENDING_INVITES is a GlobalSignal.
    // When get_response.rs sets status to Subscribed, this component
    // re-renders via render_invitation_content reading PENDING_INVITES.

    // Don't render anything if there's no invitation
    let inv_data = invitation.read().as_ref().cloned();
    if inv_data.is_none() {
        return rsx! {};
    }

    rsx! {
        // Modal backdrop - no click dismiss to prevent accidental invitation loss
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center",
            // Overlay (non-dismissable)
            div {
                class: "absolute inset-0 bg-black/50",
            }
            // Modal content
            div {
                "data-testid": "receive-invitation-modal",
                class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border",
                div {
                    class: "p-6",
                    h1 { class: "text-xl font-semibold text-text mb-4", "Invitation Received" }
                    {render_invitation_content(inv_data.unwrap(), invitation)}
                }
            }
        }
    }
}

/// Renders the content of the invitation modal based on the invitation data
fn render_invitation_content(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    // Clone the status to release the read guard before any branch can mutate
    let status = {
        let pending_invites = PENDING_INVITES.read();
        pending_invites
            .map
            .get(&inv.room)
            .map(|join| join.status.clone())
    };

    match status {
        Some(PendingRoomStatus::PendingSubscription) => render_pending_subscription_state(),
        Some(PendingRoomStatus::Subscribing) => render_subscribing_state(),
        Some(PendingRoomStatus::Error(e)) => render_error_state(&e, &inv, invitation),
        Some(PendingRoomStatus::Subscribed) => {
            // Room subscribed and retrieved successfully, close modal
            render_subscribed_state(&inv, invitation)
        }
        None => render_invitation_options(inv, invitation),
    }
}

/// Renders the state when waiting to subscribe to room data
fn render_pending_subscription_state() -> Element {
    rsx! {
        div {
            class: "text-center py-4",
            p { class: "mb-4 text-text", "Preparing to subscribe to room..." }
            div { class: "w-full h-2 bg-surface rounded-full overflow-hidden",
                div { class: "h-full bg-accent animate-pulse w-1/2" }
            }
        }
    }
}

/// Renders the loading state when subscribing to room data
fn render_subscribing_state() -> Element {
    rsx! {
        div {
            class: "text-center py-4",
            p { class: "mb-4 text-text", "Subscribing to room..." }
            div { class: "w-full h-2 bg-surface rounded-full overflow-hidden",
                div { class: "h-full bg-blue-500 animate-pulse w-2/3" }
            }
        }
    }
}

/// Renders the error state when room retrieval fails
fn render_error_state(
    error: &str,
    inv: &Invitation,
    invitation: Signal<Option<Invitation>>,
) -> Element {
    let room_key = inv.room; // Copy type, avoid clone
    let inv_for_dismiss = inv.clone();

    rsx! {
        div {
            class: "bg-red-500/10 border border-red-500/20 rounded-lg p-4",
            p { class: "mb-4 text-red-400", "Failed to retrieve room: {error}" }
            div {
                class: "flex gap-3",
                button {
                    class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white font-medium rounded-lg transition-colors",
                    onmounted: move |cx| {
                        let element = cx.data();
                        wasm_bindgen_futures::spawn_local(async move {
                            let _ = element.set_focus(true).await;
                        });
                    },
                    onclick: move |_| {
                        // Reset to PendingSubscription so the synchronizer retries
                        PENDING_INVITES.with_mut(|pending| {
                            if let Some(join) = pending.map.get_mut(&room_key) {
                                join.status = PendingRoomStatus::PendingSubscription;
                            }
                        });
                    },
                    "Retry"
                }
                button {
                    class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                    onclick: move |_| {
                        PENDING_INVITES.write().map.remove(&room_key);
                        dismiss_invitation_persistently(&inv_for_dismiss, invitation);
                    },
                    "Dismiss"
                }
            }
        }
    }
}

/// Renders the state when room is successfully subscribed and retrieved.
/// Cleans up the invitation and returns empty to dismiss the modal.
fn render_subscribed_state(
    inv: &Invitation,
    mut invitation: Signal<Option<Invitation>>,
) -> Element {
    let room_key = inv.room;
    // Mark the invitation processed NOW — at terminal success — rather than up
    // front in `accept_invitation`. This is the gravestone fix: marking on
    // accept meant a join that never completed (e.g. the room-contract GET
    // failing intermittently, freenet-core #4345) was suppressed forever on
    // reload, with no way to retry inside the sandboxed gateway iframe where
    // the localStorage auto-resume (#218) is dead (`window.localStorage` throws
    // `SecurityError` in the opaque origin, #219). Marking here means:
    //   * a SUCCESSFUL join is suppressed on reload (the URL still carries
    //     `?invitation=...` because the iframe can't `replaceState`; the hash
    //     fingerprint stops it re-prompting — #215 / #216), and
    //   * an accepted-then-LEFT room stays suppressed too (this success mark
    //     was written before the user left, and leaving never clears it —
    //     #279), while
    //   * an accept whose join never finished is NEVER marked, so it
    //     re-surfaces on reload and the user can retry.
    // The mark goes straight onto the durable, iframe-safe top-level URL hash,
    // so it works synchronously on the next load with no dependency on the
    // chat-delegate ROOMS hydration finishing first.
    //
    // Idempotent across the several renders this state may produce before the
    // deferred `invitation.set(None)` below closes the modal:
    // `mark_invitation_processed` -> `append_fingerprint` dedups, so a repeat
    // mark is a no-op (no hash spam, no redundant postMessage).
    mark_invitation_processed(&inv.to_encoded_string());
    // Defer signal mutations to avoid RefCell panics during render.
    // The modal renders one empty frame before cleanup runs — acceptable
    // since we return rsx! {} immediately.
    clear_invitation_from_storage();
    crate::util::defer(move || {
        PENDING_INVITES.with_mut(|pending| {
            pending.map.remove(&room_key);
        });
        invitation.set(None);
        info!(
            "Invitation accepted, closing modal for {:?}",
            MemberId::from(room_key)
        );
    });
    rsx! {}
}

/// Renders the invitation options based on the user's membership status
fn render_invitation_options(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    let Ok(rooms) = ROOMS.try_read() else {
        return rsx! {};
    };
    let (current_key_is_member, invited_member_exists) = check_membership_status(&inv, &rooms);
    drop(rooms);

    if current_key_is_member {
        render_already_member(inv, invitation)
    } else if invited_member_exists {
        render_restore_access_option(inv, invitation)
    } else {
        render_new_invitation(inv, invitation)
    }
}

/// True if `vk` is the room owner or already present in `members`.
fn vk_is_room_member(
    owner_vk: &VerifyingKey,
    members: &[AuthorizedMember],
    vk: &VerifyingKey,
) -> bool {
    vk == owner_vk || members.iter().any(|m| &m.member.member_vk == vk)
}

/// Pure core of [`check_membership_status`], split out so it can be unit-tested
/// without constructing a full `RoomData` (whose `contract_key` needs the
/// room-contract WASM).
///
/// `current_key_is_member` is true when the user is ALREADY in this room —
/// either under the invitation's embedded key, OR (the case that matters for a
/// re-accept) under the per-room identity `self_vk` they already hold for this
/// room. Every accepted invitation carries a freshly generated
/// `invitee_signing_key`, so that key is never itself a member yet; checking
/// only it let a user re-accept an invite to a room they were already in and
/// join a SECOND time under a new key — orphaning their original membership and
/// making the original impossible to remove (freenet/river#365).
///
/// Note `current_key_is_member` takes precedence over `invited_member_exists`
/// in the dispatcher: a user who already holds a working `self_vk` membership
/// routes to "already a member" even if the invitation re-invites a DIFFERENT
/// existing member's key (the restore-access trigger). That is intentional — a
/// user with a working identity has no need to claim another member's slot. The
/// restore-access branch still fires for the case it is meant for: a lost
/// `self_vk` that is not itself a member (see
/// `restore_access_invitation_still_detected`).
fn membership_status(
    owner_vk: &VerifyingKey,
    members: &[AuthorizedMember],
    self_vk: &VerifyingKey,
    invitation_key_vk: &VerifyingKey,
    invitation_invitee_vk: &VerifyingKey,
) -> (bool, bool) {
    let current_key_is_member = vk_is_room_member(owner_vk, members, invitation_key_vk)
        || vk_is_room_member(owner_vk, members, self_vk);
    let invited_member_exists = members
        .iter()
        .any(|m| &m.member.member_vk == invitation_invitee_vk);
    (current_key_is_member, invited_member_exists)
}

/// Checks the membership status of the user in the room
fn check_membership_status(inv: &Invitation, current_rooms: &Rooms) -> (bool, bool) {
    if let Some(room_data) = current_rooms.map.get(&inv.room) {
        membership_status(
            &room_data.owner_vk,
            &room_data.room_state.members.members,
            &room_data.self_sk.verifying_key(),
            &inv.invitee_signing_key.verifying_key(),
            &inv.invitee.member.member_vk,
        )
    } else {
        (false, false)
    }
}

/// Renders the UI when the user is already a member of the room.
/// Closing this modal must mark the invitation processed so a reload (with
/// the URL parameter still present) doesn't re-open it.
fn render_already_member(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    rsx! {
        p { class: "text-text mb-4", "You are already a member of this room with your current key." }
        button {
            class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white font-medium rounded-lg transition-colors",
            onmounted: move |cx| {
                let element = cx.data();
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = element.set_focus(true).await;
                });
            },
            onclick: move |_| {
                dismiss_invitation_persistently(&inv, invitation);
            },
            "Close"
        }
    }
}

/// Renders the UI for restoring access to an existing member
fn render_restore_access_option(
    inv: Invitation,
    invitation: Signal<Option<Invitation>>,
) -> Element {
    rsx! {
        p { class: "text-text mb-2", "This invitation is for a member that already exists in the room." }
        p { class: "text-text-muted mb-4", "If you lost access to your previous key, you can use this invitation to restore access with your current key." }
        div {
            class: "flex gap-3",
            button {
                class: "px-4 py-2 bg-yellow-500 hover:bg-yellow-600 text-white font-medium rounded-lg transition-colors",
                onmounted: move |cx| {
                    let element = cx.data();
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = element.set_focus(true).await;
                    });
                },
                onclick: {
                    let room = inv.room;
                    let member_vk = inv.invitee.member.member_vk;
                    let inv_for_restore = inv.clone();
                    let inv_for_dismiss = inv.clone();

                    move |_| {
                        // Defer signal mutations to a clean execution context to
                        // prevent RefCell re-entrant borrow panics.
                        let inv_clone = inv_for_restore.invitee.clone();
                        crate::util::defer(move || {
                            ROOMS.with_mut(|rooms| {
                                if let Some(room_data) = rooms.map.get_mut(&room) {
                                    room_data.restore_member_access(
                                        member_vk,
                                        inv_clone,
                                    );
                                }
                            });
                            crate::components::app::mark_needs_sync(room);
                        });
                        dismiss_invitation_persistently(&inv_for_dismiss, invitation);
                    }
                },
                "Restore Access"
            }
            button {
                class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                onclick: {
                    let inv_for_cancel = inv.clone();
                    move |_| {
                        dismiss_invitation_persistently(&inv_for_cancel, invitation);
                    }
                },
                "Cancel"
            }
        }
    }
}

/// Renders the UI for a new invitation
fn render_new_invitation(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    // Clone the invitation for the closures
    let inv_for_accept = inv.clone();
    let inv_for_enter = inv.clone();

    // Generate a default handle from the member's key (deterministic —
    // same key always yields the same handle).
    let default_nickname =
        crate::nickname::generate_default_nickname(&inv.invitee.member.member_vk);

    // Create a signal for the nickname
    let mut nickname = use_signal(|| default_nickname);

    rsx! {
        p { class: "text-text mb-2", "You have been invited to join a new room." }
        p { class: "text-text-muted mb-4", "Choose a nickname to use in this room:" }

        div { class: "mb-4",
            input {
                "data-testid": "receive-invitation-nickname-input",
                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent",
                r#type: "text",
                value: "{nickname}",
                onmounted: move |cx| {
                    let element = cx.data();
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = element.set_focus(true).await;
                    });
                },
                oninput: move |evt| nickname.set(evt.value().clone()),
                onkeydown: move |evt: KeyboardEvent| {
                    if evt.key() == Key::Enter && !nickname.read().trim().is_empty() {
                        evt.prevent_default();
                        accept_invitation(inv_for_enter.clone(), nickname.read().clone());
                    }
                },
                placeholder: "Your preferred nickname"
            }
        }

        p { class: "text-text mb-4", "Would you like to accept the invitation?" }
        div {
            class: "flex gap-3",
            button {
                "data-testid": "receive-invitation-accept-button",
                class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white font-medium rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                disabled: nickname.read().trim().is_empty(),
                onclick: move |_| {
                    accept_invitation(inv_for_accept.clone(), nickname.read().clone());
                },
                "Accept"
            }
            button {
                "data-testid": "receive-invitation-decline-button",
                class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                onclick: {
                    let inv_for_decline = inv.clone();
                    move |_| {
                        dismiss_invitation_persistently(&inv_for_decline, invitation);
                    }
                },
                "Decline"
            }
        }
    }
}

/// Handles the invitation acceptance process.
///
/// `pub(crate)` so the reload-recovery path in `app.rs` can auto-resume a
/// subscription that was in flight when the page was reloaded (#218), reusing
/// the exact same accept flow the Accept button uses.
pub(crate) fn accept_invitation(inv: Invitation, nickname: String) {
    // Guard against re-accepting an invite to a room the user is ALREADY in.
    // Each invitation carries a freshly generated `invitee_signing_key`, so a
    // second accept would add another member entry (a new MemberId) for the
    // same human AND overwrite the user's existing per-room `self_sk` on the
    // GET response — orphaning the original membership, which the user could
    // then never remove (freenet/river#365). This guard covers BOTH the modal
    // Accept button and the #218 localStorage auto-resume, the two paths that
    // reach `accept_invitation`. It is best-effort by design — it only fires
    // when `ROOMS` is readable AND already holds this room; on a cold/locked
    // `ROOMS` it falls through to the normal join (reverting to the prior
    // behavior for that rare case — a deeper structural backstop in the
    // GET handler is tracked as freenet/river#367). The modal dispatcher
    // (`render_invitation_options` → `check_membership_status`) is a
    // complementary gate that routes an already-member to the "already a
    // member" branch whenever the modal is shown. A genuine rejoin after
    // leaving is unaffected: `leave_room` drops the room from `ROOMS`, so
    // `self_sk` is no longer a member and this guard does not fire.
    if let Ok(rooms) = ROOMS.try_read() {
        if let Some(room_data) = rooms.map.get(&inv.room) {
            let already_member = vk_is_room_member(
                &room_data.owner_vk,
                &room_data.room_state.members.members,
                &room_data.self_sk.verifying_key(),
            );
            if already_member {
                drop(rooms);
                info!(
                    "Ignoring invitation accept for room {:?}: already a member under existing key",
                    MemberId::from(inv.room)
                );
                // Drop the stale pending invitation so the #218 auto-resume
                // doesn't keep re-firing a join that is moot (the user is
                // already in). We deliberately do NOT mark the invitation
                // processed here: that is the up-front gravestone the #356 fix
                // removed (and the `accept_invitation_does_not_mark_processed_up_front`
                // pin test forbids). The URL-driven modal still shows the
                // "already a member" branch and its dismiss is the terminal
                // path; clearing storage only stops the silent auto-resume.
                clear_invitation_from_storage();
                return;
            }
        }
    }

    // NOTE: we deliberately do NOT mark this invitation processed here.
    //
    // Marking on accept (the old behaviour) turned the processed-set into a
    // permanent gravestone: an accept whose room-contract GET never completed
    // (intermittently, e.g. freenet-core #4345's large multi-fragment GET) was
    // suppressed forever on reload, and the localStorage auto-resume (#218) is
    // dead inside the sandboxed gateway iframe (`window.localStorage` throws
    // `SecurityError` in the opaque origin, #219) — so the user was stuck
    // unless they hand-edited the top-level URL hash. Real users hit exactly
    // this on the official Freenet River room.
    //
    // The invitation is instead marked processed at TERMINAL SUCCESS, in
    // `render_subscribed_state` (and on any dismiss, via
    // `dismiss_invitation_persistently`). A join that never finishes is never
    // marked, so it re-surfaces on the next load and the user can retry. We
    // still persist the invitation + chosen nickname to localStorage below so
    // a mid-flight reload auto-resumes WHERE localStorage is available (#218,
    // dev mode); in the iframe that persistence is a no-op and the re-surfaced
    // modal (the URL still carries `?invitation=...`) is the retry path.
    let room_owner = inv.room;
    let authorized_member = inv.invitee.clone();
    let invitee_signing_key = inv.invitee_signing_key.clone();
    let room_secrets = inv.room_secrets.clone();

    // Use the user-provided nickname
    let nickname = if nickname.trim().is_empty() {
        // Fallback to generated handle if somehow empty
        crate::nickname::generate_default_nickname(&authorized_member.member.member_vk)
    } else {
        nickname
    };

    // Persist the chosen nickname alongside the pending invitation so a reload
    // before the room arrives auto-resumes the subscription with this nickname
    // instead of re-prompting (#218). `clear_invitation_from_storage` removes
    // all three keys together once the room is subscribed or the invitation is
    // dismissed. We keep the raw invitation in storage too — the resume path
    // needs both the invitation artifact and the nickname. The nickname is
    // fingerprint-bound to THIS invitation so it can't be applied to a
    // different one that later overwrites the invitation key.
    let encoded = inv.to_encoded_string();
    save_invitation_to_storage(&inv);
    save_invitation_nickname_to_storage(&encoded, &nickname);

    info!(
        "Adding room to pending invites: {:?}",
        MemberId::from(room_owner)
    );

    // Add to pending invites
    PENDING_INVITES.with_mut(|pending_invites| {
        pending_invites.map.insert(
            room_owner,
            PendingRoomJoin {
                authorized_member: authorized_member.clone(),
                invitee_signing_key: invitee_signing_key.clone(),
                preferred_nickname: nickname.clone(),
                status: PendingRoomStatus::PendingSubscription,
                subscribing_since: None,
                retry_count: 0,
                room_secrets: room_secrets.clone(),
            },
        );
    });

    info!("Requesting room state for invitation");

    // Send the AcceptInvitation message directly without spawn_local
    let result = SYNCHRONIZER
        .write()
        .get_message_sender()
        .unbounded_send(SynchronizerMessage::AcceptInvitation {
            owner_vk: room_owner,
            authorized_member: Box::new(authorized_member),
            invitee_signing_key: Box::new(invitee_signing_key),
            nickname,
        })
        .map_err(|e| format!("Failed to send message: {}", e));

    match result {
        Ok(_) => {
            info!("Successfully requested room state for invitation");
        }
        Err(e) => {
            // Log detailed error information
            error!("Failed to request room state for invitation: {}", e);
            error!(
                "Error details: invitation for room with owner key: {:?}",
                MemberId::from(room_owner)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::member::Member;

    /// Build an `AuthorizedMember` invited directly by the owner.
    fn owner_invited_member(owner_sk: &SigningKey, member_vk: VerifyingKey) -> AuthorizedMember {
        let owner_vk = owner_sk.verifying_key();
        let member = Member {
            owner_member_id: owner_vk.into(),
            invited_by: owner_vk.into(),
            member_vk,
        };
        AuthorizedMember::new(member, owner_sk)
    }

    #[test]
    fn vk_is_room_member_matches_owner_and_members() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let member_sk = SigningKey::generate(&mut rng);
        let stranger_sk = SigningKey::generate(&mut rng);
        let members = vec![owner_invited_member(&owner_sk, member_sk.verifying_key())];

        assert!(
            vk_is_room_member(&owner_vk, &members, &owner_vk),
            "owner counts as a member"
        );
        assert!(
            vk_is_room_member(&owner_vk, &members, &member_sk.verifying_key()),
            "listed member is a member"
        );
        assert!(
            !vk_is_room_member(&owner_vk, &members, &stranger_sk.verifying_key()),
            "unrelated key is not a member"
        );
    }

    #[test]
    fn reaccept_while_already_member_reports_already_member() {
        // The exact bug (freenet/river#365): the user is already in the room
        // under their existing per-room identity (`self_sk`). A NEW invitation
        // to the same room carries a freshly generated key that is not yet a
        // member. The old check looked only at the invitation key and so fell
        // through to a full second join. `membership_status` must instead see
        // the existing identity and report "already a member".
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        let self_sk = SigningKey::generate(&mut rng);
        let members = vec![owner_invited_member(&owner_sk, self_sk.verifying_key())];

        // Fresh invitation key — never yet a member.
        let fresh_invite_sk = SigningKey::generate(&mut rng);
        let invited_vk = fresh_invite_sk.verifying_key();

        let (current_key_is_member, invited_member_exists) = membership_status(
            &owner_vk,
            &members,
            &self_sk.verifying_key(),
            &fresh_invite_sk.verifying_key(),
            &invited_vk,
        );

        assert!(
            current_key_is_member,
            "user already in the room (via self_sk) must be reported as already a member"
        );
        assert!(
            !invited_member_exists,
            "the fresh invitation key is not yet a member, so this is not a restore-access case"
        );
    }

    #[test]
    fn genuine_new_join_is_not_reported_as_member() {
        // The room exists locally but the user's existing identity is NOT a
        // member (e.g. a room they can see but have not joined). A fresh invite
        // must still flow to the normal new-join path.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        // Existing self_sk that is not in the (empty) member list.
        let self_sk = SigningKey::generate(&mut rng);
        let members: Vec<AuthorizedMember> = vec![];

        let fresh_invite_sk = SigningKey::generate(&mut rng);
        let invited_vk = fresh_invite_sk.verifying_key();

        let (current_key_is_member, invited_member_exists) = membership_status(
            &owner_vk,
            &members,
            &self_sk.verifying_key(),
            &fresh_invite_sk.verifying_key(),
            &invited_vk,
        );

        assert!(
            !current_key_is_member,
            "a user who is not yet a member must not be treated as already joined"
        );
        assert!(!invited_member_exists);
    }

    #[test]
    fn restore_access_invitation_still_detected() {
        // An invitation whose invitee key already exists in the room (the user
        // lost their key and wants to restore access) must still report
        // `invited_member_exists`, and — because the local `self_sk` is some
        // unrelated/lost key not in the room — NOT `current_key_is_member`,
        // so the dispatcher routes to the restore-access branch.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        let existing_member_sk = SigningKey::generate(&mut rng);
        let members = vec![owner_invited_member(
            &owner_sk,
            existing_member_sk.verifying_key(),
        )];

        // The invitation re-invites that same existing member key, but the
        // user's currently-held key is a different (lost) one not in the room.
        let lost_self_sk = SigningKey::generate(&mut rng);
        let invitation_key_sk = SigningKey::generate(&mut rng);

        let (current_key_is_member, invited_member_exists) = membership_status(
            &owner_vk,
            &members,
            &lost_self_sk.verifying_key(),
            &invitation_key_sk.verifying_key(),
            &existing_member_sk.verifying_key(),
        );

        assert!(
            !current_key_is_member,
            "the user's currently-held key is not in the room"
        );
        assert!(
            invited_member_exists,
            "the invitation's invitee key is an existing member → restore-access case"
        );
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let a = invitation_fingerprint("invitation-code-xyz");
        let b = invitation_fingerprint("invitation-code-xyz");
        assert_eq!(a, b, "same input must hash to same fingerprint");
    }

    #[test]
    fn fingerprint_distinguishes_inputs() {
        let a = invitation_fingerprint("invitation-a");
        let b = invitation_fingerprint("invitation-b");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_is_compact_hex() {
        let fp = invitation_fingerprint("anything");
        assert_eq!(fp.len(), 32, "16 bytes hex-encoded = 32 chars");
        assert!(
            fp.chars().all(|c| c.is_ascii_hexdigit()),
            "fingerprint must be hex"
        );
    }

    #[test]
    fn append_dedups() {
        let list = vec!["a".to_string(), "b".to_string()];
        assert!(
            append_fingerprint(list, "a".to_string(), 64).is_none(),
            "duplicate should return None to skip the write"
        );
    }

    #[test]
    fn append_evicts_fifo_when_at_cap() {
        let list = vec!["1".to_string(), "2".to_string(), "3".to_string()];
        let result = append_fingerprint(list, "4".to_string(), 3).expect("change occurred");
        assert_eq!(
            result,
            vec!["2".to_string(), "3".to_string(), "4".to_string()],
            "oldest entry should be evicted to make room"
        );
    }

    #[test]
    fn append_evicts_multiple_when_above_cap() {
        // Simulates raising the cap or pre-existing oversized state
        let list: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        let result = append_fingerprint(list, "new".to_string(), 3).expect("change occurred");
        assert_eq!(result.len(), 3);
        assert_eq!(result.last().unwrap(), "new");
        assert_eq!(result[0], "8");
    }

    #[test]
    fn append_evicts_at_actual_production_cap() {
        // Guards against off-by-one regressions in the eviction math at the
        // configured MAX_PROCESSED_INVITATIONS boundary.
        let mut list: Vec<String> = (0..MAX_PROCESSED_INVITATIONS)
            .map(|i| i.to_string())
            .collect();
        assert_eq!(list.len(), MAX_PROCESSED_INVITATIONS);
        list = append_fingerprint(list, "new".to_string(), MAX_PROCESSED_INVITATIONS)
            .expect("change occurred");
        assert_eq!(list.len(), MAX_PROCESSED_INVITATIONS);
        assert_eq!(list.last().unwrap(), "new");
        assert_eq!(list[0], "1", "oldest (\"0\") should have been evicted");
    }

    /// `Invitation::to_encoded_string` must round-trip byte-for-byte. The
    /// fingerprint dedup compares hashes of the canonical form, so two calls
    /// to `to_encoded_string` for the same `Invitation` (one at URL parse
    /// time, one at dismiss time) must produce the same bytes; otherwise
    /// `is_invitation_processed` will not recognize a previously-marked
    /// invitation across a page reload, defeating the fix.
    #[test]
    fn invitation_round_trip_is_byte_stable() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        use river_core::room_state::member::{AuthorizedMember, Member};

        let owner_sk = SigningKey::generate(&mut OsRng);
        let invitee_sk = SigningKey::generate(&mut OsRng);

        let member = Member {
            owner_member_id: owner_sk.verifying_key().into(),
            invited_by: owner_sk.verifying_key().into(),
            member_vk: invitee_sk.verifying_key(),
        };
        let authorized = AuthorizedMember::new(member, &owner_sk);

        let inv = Invitation {
            room: owner_sk.verifying_key(),
            invitee_signing_key: invitee_sk,
            invitee: authorized,
            room_secrets: Vec::new(),
        };

        let first = inv.to_encoded_string();
        let second = inv.to_encoded_string();
        assert_eq!(
            first, second,
            "to_encoded_string must be deterministic across calls"
        );

        // Round-trip: encode -> decode -> encode must produce the same bytes.
        let decoded = Invitation::from_encoded_string(&first)
            .expect("our own encoded form must decode cleanly");
        let re_encoded = decoded.to_encoded_string();
        assert_eq!(
            first, re_encoded,
            "encode->decode->encode must be byte-stable; otherwise the fingerprint dedup breaks across reloads"
        );

        // And the fingerprints must match too.
        assert_eq!(
            invitation_fingerprint(&first),
            invitation_fingerprint(&re_encoded),
            "fingerprints of round-tripped encodings must be equal"
        );
    }

    #[test]
    fn parse_hash_returns_empty_for_unrelated_hashes() {
        assert!(parse_processed_hash("").is_empty());
        assert!(parse_processed_hash("#").is_empty());
        assert!(parse_processed_hash("#some-other-anchor").is_empty());
        assert!(
            parse_processed_hash("#river-processed").is_empty(),
            "hash without '=' should not be misinterpreted as having entries"
        );
    }

    #[test]
    fn parse_hash_recovers_fingerprints() {
        let parsed = parse_processed_hash("#river-processed=abc,def,123");
        assert_eq!(parsed, vec!["abc", "def", "123"]);
    }

    #[test]
    fn parse_hash_filters_empty_entries() {
        // Defensive: trailing/double commas mustn't yield empty fingerprints.
        let parsed = parse_processed_hash("#river-processed=a,,b,");
        assert_eq!(parsed, vec!["a", "b"]);
    }

    #[test]
    fn build_hash_round_trips_through_parse() {
        let original: Vec<String> = vec!["fp1".into(), "fp2".into(), "fp3".into()];
        let hash = build_processed_hash(&original);
        assert!(hash.starts_with(PROCESSED_HASH_PREFIX));
        assert_eq!(parse_processed_hash(&hash), original);
    }

    #[test]
    fn build_hash_is_empty_for_empty_list() {
        assert_eq!(build_processed_hash(&[]), "");
    }

    #[test]
    fn cache_provides_read_after_write_consistency() {
        // Regression test for the lost-update race that would otherwise
        // happen if `read_processed_list` always re-read from
        // `window.location.hash`. The shell's `replaceState` does NOT fire
        // `hashchange`, so the iframe's own `location.hash` does not update
        // after a postMessage write. Without the in-memory cache, two
        // back-to-back marks would each see an empty list and the second
        // would clobber the first.
        PROCESSED_CACHE.with(|c| *c.borrow_mut() = None);

        write_processed_list(&["fp1".to_string()]);
        assert_eq!(
            read_processed_list(),
            vec!["fp1".to_string()],
            "second read must observe the first write"
        );

        // Simulate two consecutive marks: each builds on what the prior
        // call wrote, instead of overwriting it.
        mark_invitation_processed("invitation-A");
        mark_invitation_processed("invitation-B");
        let after = read_processed_list();
        assert_eq!(after.len(), 3, "all three writes must be present");
        assert_eq!(after[0], "fp1");
        assert!(after[1..].iter().all(|fp| fp.len() == 32));

        PROCESSED_CACHE.with(|c| *c.borrow_mut() = None);
    }

    // ---- #218: auto-resume subscription on reload mid-invitation-flow ----

    #[test]
    fn recovered_invitation_with_nickname_resumes() {
        // The user clicked Accept (nickname saved) and reloaded before the
        // room arrived. Even though `accept_invitation` already marked the
        // invitation processed, we must RESUME (not Discard) — otherwise the
        // user is dropped with no "Subscribing…" feedback, which is exactly
        // the bug #218 fixes.
        let action = decide_recovered_invitation(
            Some("Alice".to_string()),
            /* already_processed */ true,
        );
        assert_eq!(
            action,
            RecoveredInvitationAction::Resume {
                nickname: "Alice".to_string()
            },
            "saved nickname must take precedence over the processed flag"
        );
    }

    #[test]
    fn recovered_invitation_with_nickname_resumes_even_when_not_processed() {
        // Defensive: a nickname present but processed-flag somehow absent
        // (e.g. the top-level hash was lost) must still resume, never prompt.
        let action = decide_recovered_invitation(Some("Bob".to_string()), false);
        assert_eq!(
            action,
            RecoveredInvitationAction::Resume {
                nickname: "Bob".to_string()
            }
        );
    }

    #[test]
    fn recovered_invitation_processed_without_nickname_discards() {
        // No nickname → the user accepted-then-left or dismissed in a prior
        // session; there is no in-flight join to resume. Discard.
        let action = decide_recovered_invitation(None, true);
        assert_eq!(action, RecoveredInvitationAction::Discard);
    }

    #[test]
    fn recovered_invitation_not_processed_without_nickname_prompts() {
        // The user reloaded before deciding: no nickname, not yet acted on.
        // Re-open the modal at the nickname prompt (pre-#218 behaviour).
        let action = decide_recovered_invitation(None, false);
        assert_eq!(action, RecoveredInvitationAction::Prompt);
    }

    #[test]
    fn nickname_binding_matches_only_its_own_invitation() {
        // Regression for the Codex-review P2: a nickname saved for invitation A
        // must NOT be applied to a different invitation B that overwrote the
        // invitation key. The binding stores fp(A); recovering B compares
        // against fp(B) and rejects.
        let inv_a = "encoded-invitation-A";
        let inv_b = "encoded-invitation-B";
        let stored_fp_for_a = invitation_fingerprint(inv_a);

        assert!(
            nickname_belongs_to_invitation(&stored_fp_for_a, inv_a),
            "nickname saved for A must match A"
        );
        assert!(
            !nickname_belongs_to_invitation(&stored_fp_for_a, inv_b),
            "nickname saved for A must NOT match a different invitation B"
        );
    }

    #[test]
    fn resume_fires_at_most_once_across_many_renders() {
        // Regression for the Codex-review P1: the recovery block runs in the
        // `App` body on EVERY render, and the persisted nickname stays until
        // the join completes. Without the one-shot guard, each render would
        // re-fire the resume (re-send AcceptInvitation, reset PENDING_INVITES),
        // looping. Simulate many renders and assert the side effect fires once.
        let fired = std::cell::Cell::new(false);
        let mut side_effects = 0;
        for _ in 0..100 {
            // Each iteration is a render where the Resume action is selected.
            assert_eq!(
                decide_recovered_invitation(Some("Alice".to_string()), true),
                RecoveredInvitationAction::Resume {
                    nickname: "Alice".to_string()
                }
            );
            if take_resume_once(&fired) {
                side_effects += 1;
            }
        }
        assert_eq!(
            side_effects, 1,
            "auto-resume must fire exactly once across many renders"
        );
    }

    #[test]
    fn hash_payload_at_cap_fits_under_url_limit() {
        // Sanity-check the URL budget: 32 fingerprints (16 bytes hex = 32
        // chars each) plus separators plus the prefix must stay well under
        // the 8192-byte slice the shell-bridge applies to incoming hashes.
        let list: Vec<String> = (0..MAX_PROCESSED_INVITATIONS)
            .map(|i| format!("{:032x}", i))
            .collect();
        let hash = build_processed_hash(&list);
        assert!(
            hash.len() < 4096,
            "hash should stay compact: {}",
            hash.len()
        );
        assert!(hash.len() > PROCESSED_HASH_PREFIX.len());
    }

    // ---- invite-gravestone fix: mark on terminal success, not on accept ----
    //
    // The fix moves `mark_invitation_processed` out of `accept_invitation`
    // (up front) and into `render_subscribed_state` (terminal success).
    // `dismiss_invitation_persistently` still marks on any dismiss. The
    // processed-set lives in the durable, iframe-safe top-level URL hash and is
    // host-testable via `PROCESSED_CACHE`, so we drive it directly to assert
    // the four required outcomes. The actual call-site relocation is pinned by
    // the two source-grep tests below (the rsx-returning render fns can't run
    // without a Dioxus runtime).

    /// (a) Accepted but the join never completed → the invitation was NEVER
    /// marked processed, so the gate re-surfaces it and the user can retry.
    /// This is the bug the fix repairs (the old code marked it up front, which
    /// permanently suppressed it on reload in the sandboxed iframe).
    #[test]
    fn accepted_but_incomplete_join_is_not_suppressed() {
        PROCESSED_CACHE.with(|c| *c.borrow_mut() = Some(Vec::new()));
        // accept_invitation no longer marks; render_subscribed_state never ran.
        assert!(
            !is_invitation_processed("invitation-incomplete"),
            "an accepted-but-incomplete join must remain retryable on reload"
        );
        PROCESSED_CACHE.with(|c| *c.borrow_mut() = None);
    }

    /// (b) Accepted and the join COMPLETED → `render_subscribed_state` marks it
    /// processed, so a reload (URL still carries `?invitation=...`, which the
    /// iframe can't strip) is suppressed synchronously without re-prompting for
    /// a nickname (#215 / #216). We exercise the mark the success path makes.
    #[test]
    fn successful_join_suppresses_on_reload() {
        PROCESSED_CACHE.with(|c| *c.borrow_mut() = Some(Vec::new()));
        // The load-bearing op render_subscribed_state performs at success:
        mark_invitation_processed("invitation-joined");
        assert!(
            is_invitation_processed("invitation-joined"),
            "a successfully joined invite must be suppressed on reload (#215/#216)"
        );
        PROCESSED_CACHE.with(|c| *c.borrow_mut() = None);
    }

    /// (c) Declined / Cancelled / Closed → `dismiss_invitation_persistently`
    /// marks it processed → suppressed on reload (#279).
    #[test]
    fn dismissed_invite_suppresses_on_reload() {
        PROCESSED_CACHE.with(|c| *c.borrow_mut() = Some(Vec::new()));
        // The load-bearing op dismiss_invitation_persistently performs:
        mark_invitation_processed("invitation-declined");
        assert!(
            is_invitation_processed("invitation-declined"),
            "a dismissed invite must be suppressed on reload (#279)"
        );
        PROCESSED_CACHE.with(|c| *c.borrow_mut() = None);
    }

    /// (d) Accepted, joined, then LEFT → the success mark was written before
    /// the user left, and leaving never clears it, so the invite stays
    /// suppressed on reload (#279). Leaving a room does NOT touch the
    /// processed-set, so the already-present success mark is what suppresses.
    #[test]
    fn accepted_then_left_stays_suppressed_on_reload() {
        PROCESSED_CACHE.with(|c| *c.borrow_mut() = Some(Vec::new()));
        // Success path marked it when the join completed:
        mark_invitation_processed("invitation-left");
        // ... user later leaves the room (leave_room touches ROOMS only, NOT
        // the processed-set), so the mark survives:
        assert!(
            is_invitation_processed("invitation-left"),
            "an accepted-then-left room must stay suppressed on reload (#279)"
        );
        PROCESSED_CACHE.with(|c| *c.borrow_mut() = None);
    }

    /// Source-grep pin: `accept_invitation` must NOT mark the invitation
    /// processed (that was the gravestone). A future refactor that re-adds the
    /// up-front mark would reintroduce the bug and must update this test.
    #[test]
    fn accept_invitation_does_not_mark_processed_up_front() {
        let src = include_str!("receive_invitation_modal.rs");
        let accept_fn = src
            .split_once("pub(crate) fn accept_invitation(")
            .expect("accept_invitation must exist")
            .1;
        // Bound the search to the function body (up to the next top-level fn /
        // test module). The body ends before `#[cfg(test)]`.
        let accept_body = accept_fn
            .split_once("#[cfg(test)]")
            .map(|(body, _)| body)
            .unwrap_or(accept_fn);
        assert!(
            !accept_body.contains("mark_invitation_processed("),
            "accept_invitation must NOT call mark_invitation_processed — marking \
             on accept is the gravestone that suppresses a failed join forever \
             in the sandboxed iframe. Mark on terminal success instead."
        );
    }

    /// Source-grep pin: `accept_invitation` must guard against re-accepting an
    /// invite to a room the user is already in, BEFORE it creates the pending
    /// join. The #218 auto-resume calls `accept_invitation` directly, bypassing
    /// the modal's `check_membership_status` routing, so without this early-bail
    /// it would add a duplicate membership and clobber `self_sk`
    /// (freenet/river#365). A refactor that drops the guard or moves it after
    /// the `PENDING_INVITES` insert would re-regress #365 for the auto-resume
    /// path. (The pure decision is covered by
    /// `reaccept_while_already_member_reports_already_member`; this pins that
    /// the accept path actually USES it, since the guard reads the `ROOMS`
    /// signal and can't run under a host unit test.)
    #[test]
    fn accept_invitation_guards_against_reaccept_before_join() {
        let src = include_str!("receive_invitation_modal.rs");
        let accept_fn = src
            .split_once("pub(crate) fn accept_invitation(")
            .expect("accept_invitation must exist")
            .1;
        let accept_body = accept_fn
            .split_once("#[cfg(test)]")
            .map(|(body, _)| body)
            .unwrap_or(accept_fn);

        let guard_at = accept_body.find("vk_is_room_member(").expect(
            "accept_invitation must call vk_is_room_member to detect an existing \
             membership (freenet/river#365)",
        );
        let join_at = accept_body
            .find("PENDING_INVITES.with_mut(")
            .expect("accept_invitation must create the pending join via PENDING_INVITES");
        assert!(
            guard_at < join_at,
            "the already-member guard must run BEFORE the pending-join insert so \
             a re-accept early-bails instead of creating a duplicate membership \
             (freenet/river#365)."
        );
        assert!(
            accept_body[..join_at].contains("return;"),
            "the already-member guard must early-`return` before the join \
             (freenet/river#365)."
        );
    }

    /// Source-grep pin: the terminal-success render (`render_subscribed_state`)
    /// MUST mark the invitation processed, so a successful join is suppressed
    /// on reload (#215/#216) and the accepted-then-left case stays suppressed
    /// (#279). This is the relocation target for the mark removed from
    /// `accept_invitation`.
    #[test]
    fn render_subscribed_state_marks_processed() {
        let src = include_str!("receive_invitation_modal.rs");
        let sub_fn = src
            .split_once("fn render_subscribed_state(")
            .expect("render_subscribed_state must exist")
            .1;
        // Body ends at the start of the next fn.
        let sub_body = sub_fn
            .split_once("\nfn ")
            .map(|(body, _)| body)
            .unwrap_or(sub_fn);
        assert!(
            sub_body.contains("mark_invitation_processed(&inv.to_encoded_string())"),
            "render_subscribed_state must mark the invitation processed at \
             terminal success (the relocation target of the gravestone fix)."
        );
    }
}
