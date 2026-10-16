//! Noticing, while a tab is open, that River re-keyed (freenet/river#580).
//!
//! An open tab runs the UI build it loaded, and that build has the room
//! contract and chat delegate WASM compiled in. When a publish re-keys either
//! one, the tab keeps using the old key:
//!
//! * **Room contract.** It stays subscribed to the old contract key while
//!   everyone on the new build posts under the new one. Its rooms look like
//!   they have gone quiet, and nothing it posts reaches the others.
//! * **Chat delegate.** It keeps writing to the old delegate after a new-build
//!   load has migrated from it, so changes to rooms the new build already
//!   knows (a leave, a re-invite's signing key, read markers) are stranded.
//!
//! The cure is a reload. The signal is River's own pointer records
//! (`pointer-records.toml`): two 100-byte contracts at fixed addresses whose
//! author-signed state names the current code hash and a version that rises on
//! every re-key. This module watches both and, when either names a generation
//! newer than this build's, reloads the app. riverctl's long-running
//! `message stream` does the same and exits to be restarted (freenet/river#694,
//! `cli/src/pointer.rs`).
//!
//! # Why the pointer and not the web container
//!
//! Watching the web container would also catch UI-only publishes, but every
//! publish would push the whole ~1.2 MB archive to every open tab just to say
//! "something changed". A UI-only publish strands nobody; the pointer is
//! 100 bytes and moves only on the re-keys that do.
//!
//! # What counts as "newer"
//!
//! Each check builds a [`PointerResolver`] seeded with THIS BUILD's own record
//! (version + code hash from `pointer-records.toml`, baked in by `build.rs`)
//! as its floor. Only [`PointerOutcome::Resolved`], a signature-verified record
//! strictly above that floor, means a newer generation exists. A stale peer, an
//! unreachable pointer, a competing record or a withdrawal is not a reason to
//! reload, and neither is a pointer that is BEHIND this build: the pointer is
//! published after the UI, so a fresh build routinely sees it lag.
//!
//! A build whose embedded WASM does not hash to its record (a local build that
//! rebuilt the WASM without re-signing) cannot place itself in the sequence,
//! so the watch is off for that pointer rather than guessing.
//!
//! # Loop safety
//!
//! A reload is only as good as what it loads. The pointer is 100 bytes and
//! the web container ~1.2 MB, so a node can learn of a re-key before it holds
//! the bundle that implements it, and the reloaded page then sees the same
//! newer pointer again. That is expected, and is retried; what must not happen
//! is retrying forever. The caps live in the iframe URL's [`RELOAD_PARAM`]
//! rather than in storage: storage in the sandboxed iframe is not reliable,
//! and a cap that silently resets fails open (the node shell's `_freload` cap
//! makes the same choice).
//!
//! 1. At most [`MAX_TRIES_PER_TARGET`] automatic reloads per target
//!    generation, spaced by [`RETRY_DELAYS_MS`] so a slow bundle has time to
//!    arrive.
//! 2. At most [`MAX_AUTO_RELOADS_PER_WINDOW`] automatic reloads per
//!    [`AUTO_RELOAD_WINDOW_MS`], whatever the target.
//! 3. Past either cap, only a banner with a manual Reload.
//!
//! A reload always fetches the newest bundle the node has, so a loop could
//! only persist if the newest build were itself broken, and the caps bound it
//! until the next publish replaces that build.
//!
//! # When to reload
//!
//! A hidden tab reloads after [`HIDDEN_DELAY_MIN_MS`] plus jitter. The floor
//! outlasts the node's 30 s web-bundle refresh window, and the jitter keeps
//! every open tab from re-keying at the same moment. A visible tab shows a
//! banner counting down [`COUNTDOWN_MS`] with "Reload now" and "Not now". No
//! automatic reload ever happens while the reader has unsent text.

use dioxus::logger::tracing::{info, warn};
use ed25519_dalek::VerifyingKey;
use freenet_migrate::{PointerFloor, PointerOutcome, PointerResolver};
use freenet_stdlib::prelude::{ContractInstanceId, UpdateData};
use std::sync::{LazyLock, Mutex};

/// One row of `pointer-records.toml`, as baked in by `build.rs`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BuildPointerRecord {
    pub app_id: &'static str,
    pub version: u32,
    pub code_hash: [u8; 32],
}

// Defines `POINTER_AUTHOR_VK: &str` and `POINTER_RECORDS: &[BuildPointerRecord]`.
include!(concat!(env!("OUT_DIR"), "/pointer_records.rs"));

/// Prefix of `author_verifying_key` in `pointer-records.toml` and FREENET.md.
const AUTHOR_VK_PREFIX: &str = "river:v1:vk:";

/// Query parameter carrying the reload caps across a reload.
pub(crate) const RELOAD_PARAM: &str = "_rgen";
/// The connection manager's auth-reload parameter, dropped on our reloads so
/// the URL does not accumulate both.
const AUTH_RELOAD_PARAM: &str = "_reload";

/// Most automatic reloads for one target generation.
pub(crate) const MAX_TRIES_PER_TARGET: u32 = 4;
/// The least wait before the 2nd, 3rd and 4th try for one target. A reload
/// that landed on the old bundle means the node does not hold the new one
/// yet; these spread the remaining tries over ~20 minutes of propagation.
pub(crate) const RETRY_DELAYS_MS: [u64; 3] = [2 * 60 * 1000, 5 * 60 * 1000, 15 * 60 * 1000];
/// Most automatic reloads in one [`AUTO_RELOAD_WINDOW_MS`], whatever the
/// target. A backstop over the per-target bound.
pub(crate) const MAX_AUTO_RELOADS_PER_WINDOW: u32 = 6;
pub(crate) const AUTO_RELOAD_WINDOW_MS: u64 = 60 * 60 * 1000;

/// How long a visible tab's banner counts down before reloading.
pub(crate) const COUNTDOWN_MS: u64 = 15_000;
/// A hidden tab waits at least this long. It must exceed the node's 30 s
/// `CONTRACT_CACHE_REFRESH_TTL`, after which the iframe path re-reads the web
/// container instead of serving the bundle it extracted last.
pub(crate) const HIDDEN_DELAY_MIN_MS: u64 = 30_000;
/// Upper bound of the random extra wait for a hidden tab.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) const HIDDEN_DELAY_JITTER_MS: u64 = 90_000;

/// The two artifacts whose re-key strands an open tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Artifact {
    RoomContract,
    ChatDelegate,
}

impl Artifact {
    pub(crate) const ALL: [Artifact; 2] = [Artifact::RoomContract, Artifact::ChatDelegate];

    pub(crate) fn app_id(self) -> &'static str {
        match self {
            Artifact::RoomContract => "river.room-contract",
            Artifact::ChatDelegate => "river.chat-delegate",
        }
    }

    fn embedded_wasm(self) -> &'static [u8] {
        match self {
            Artifact::RoomContract => crate::constants::ROOM_CONTRACT_WASM,
            Artifact::ChatDelegate => crate::constants::CHAT_DELEGATE_WASM,
        }
    }

    fn index(self) -> usize {
        match self {
            Artifact::RoomContract => 0,
            Artifact::ChatDelegate => 1,
        }
    }
}

/// A pointer this build can place itself against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Watched {
    pub artifact: Artifact,
    pub pointer_id: ContractInstanceId,
    author_vk: VerifyingKey,
    build_floor: PointerFloor,
}

impl Watched {
    pub(crate) fn build_version(&self) -> u32 {
        self.build_floor.version()
    }
}

/// Decode `river:v1:vk:<base58>` into a verifying key.
fn decode_author_vk(s: &str) -> Option<VerifyingKey> {
    let b58 = s.strip_prefix(AUTHOR_VK_PREFIX)?;
    let bytes: [u8; 32] = bs58::decode(b58).into_vec().ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Which pointers this build can watch: those whose record names the BLAKE3
/// of the WASM this build actually embeds (`embedded`).
pub(crate) fn watched_pointers(
    author: &str,
    records: &[BuildPointerRecord],
    embedded: impl Fn(Artifact) -> &'static [u8],
) -> Vec<Watched> {
    let Some(author_vk) = decode_author_vk(author) else {
        warn!("generation watch off: pointer author key {author:?} does not decode");
        return Vec::new();
    };
    Artifact::ALL
        .into_iter()
        .filter_map(|artifact| {
            let app_id = artifact.app_id();
            let Some(record) = records.iter().find(|r| r.app_id == app_id) else {
                warn!("generation watch off for {app_id}: no pointer record");
                return None;
            };
            if *blake3::hash(embedded(artifact)).as_bytes() != record.code_hash {
                // A local build whose WASM was rebuilt without re-signing. It
                // cannot say which generation it is, so it must not judge
                // whether the network is ahead of it.
                warn!(
                    "generation watch off for {app_id}: embedded WASM does not match \
                     pointer record v{}",
                    record.version
                );
                return None;
            }
            let build_floor = PointerFloor::at(record.version, record.code_hash).ok()?;
            let pointer_id =
                freenet_migrate::pointer_contract_id(&author_vk, app_id.as_bytes()).ok()?;
            Some(Watched {
                artifact,
                pointer_id,
                author_vk,
                build_floor,
            })
        })
        .collect()
}

static WATCHED: LazyLock<Vec<Watched>> = LazyLock::new(|| {
    let watched = watched_pointers(POINTER_AUTHOR_VK, POINTER_RECORDS, Artifact::embedded_wasm);
    for w in &watched {
        info!(
            "generation watch: {} at build v{} (pointer {})",
            w.artifact.app_id(),
            w.build_version(),
            w.pointer_id
        );
    }
    watched
});

/// What one pointer record says about this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The network names this build's own generation.
    Current,
    /// A verified record names a newer generation than this build.
    Newer(u32),
    /// Nothing that should change what this tab does.
    NoSignal,
}

/// Judge one served pointer state against this build.
pub(crate) fn evaluate(watched: &Watched, state: &[u8]) -> Verdict {
    let Ok(mut resolver) = PointerResolver::new(
        &watched.author_vk,
        watched.artifact.app_id().as_bytes(),
        watched.build_floor,
    ) else {
        return Verdict::NoSignal;
    };
    // Arms the resolver's event gate; the GET itself already happened.
    let _ = resolver.next_action();
    if !resolver.on_response(watched.pointer_id, state) {
        return Verdict::NoSignal;
    }
    match resolver.take_outcome() {
        Some(Ok(PointerOutcome::Resolved(r))) => Verdict::Newer(r.version()),
        Some(Ok(PointerOutcome::Unchanged(_))) => Verdict::Current,
        _ => Verdict::NoSignal,
    }
}

/// The newest generation seen per artifact this page load, or `None` while
/// the network has named nothing newer than this build.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Observed {
    newer: [Option<u32>; 2],
}

impl Observed {
    /// Record a verdict. Returns whether the target moved.
    pub(crate) fn record(&mut self, artifact: Artifact, verdict: Verdict) -> bool {
        let Verdict::Newer(version) = verdict else {
            return false;
        };
        let slot = &mut self.newer[artifact.index()];
        if slot.is_some_and(|seen| seen >= version) {
            return false;
        }
        *slot = Some(version);
        true
    }

    pub(crate) fn is_stale(&self) -> bool {
        self.newer.iter().any(Option::is_some)
    }

    /// Identifies the generation a reload is for, e.g. `r5d5`. An artifact the
    /// network has not moved is named by this build's version (0 if unwatched).
    pub(crate) fn target_key(&self, watched: &[Watched]) -> String {
        let version = |artifact: Artifact| {
            self.newer[artifact.index()].unwrap_or_else(|| {
                watched
                    .iter()
                    .find(|w| w.artifact == artifact)
                    .map_or(0, Watched::build_version)
            })
        };
        format!(
            "r{}d{}",
            version(Artifact::RoomContract),
            version(Artifact::ChatDelegate)
        )
    }
}

static OBSERVED: LazyLock<Mutex<Observed>> = LazyLock::new(Default::default);

/// The reload caps carried in [`RELOAD_PARAM`]:
/// `<target>-<window start ms>-<reloads in window>-<tries for target>`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CapState {
    target: String,
    window_start_ms: u64,
    count: u32,
    tries: u32,
}

impl CapState {
    fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.splitn(4, '-');
        let target = parts.next()?.to_string();
        let window_start_ms = parts.next()?.parse().ok()?;
        let count = parts.next()?.parse().ok()?;
        let tries = parts.next()?.parse().ok()?;
        Some(Self {
            target,
            window_start_ms,
            count,
            tries,
        })
    }

    fn encode(&self) -> String {
        format!(
            "{}-{}-{}-{}",
            self.target, self.window_start_ms, self.count, self.tries
        )
    }

    /// Tries already spent on `target` by the reloads that led to this page.
    fn tries_for(existing: Option<&Self>, target: &str) -> u32 {
        existing
            .filter(|c| c.target == target)
            .map_or(0, |c| c.tries)
    }

    /// The `(window start, count)` still in force at `now_ms`, or a fresh
    /// window. A start in the future or past the window is not trusted.
    fn live_window(existing: Option<&Self>, now_ms: u64) -> (u64, u32) {
        match existing {
            Some(c)
                if c.window_start_ms <= now_ms
                    && now_ms - c.window_start_ms < AUTO_RELOAD_WINDOW_MS =>
            {
                (c.window_start_ms, c.count)
            }
            _ => (now_ms, 0),
        }
    }
}

/// Whether an automatic reload for `target` may happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CapDecision {
    /// Reload no sooner than `min_delay_ms` from now, carrying this
    /// [`RELOAD_PARAM`] value. The delay is non-zero on a retry: this page was
    /// loaded by a reload for the same target and is still behind, so the node
    /// does not hold the new bundle yet.
    Allow { param: String, min_delay_ms: u64 },
    /// [`MAX_TRIES_PER_TARGET`] reloads for this target all landed on an old
    /// bundle. Stop; the banner remains.
    TriesExhausted,
    /// Too many automatic reloads in the window.
    RateLimited,
}

/// Decide an automatic reload from the [`RELOAD_PARAM`] this page loaded with.
///
/// A malformed value (reachable only by hand-editing the URL) starts afresh.
/// The bound still holds, because every reload writes a well-formed value that
/// the next load reads back.
pub(crate) fn cap_decision(existing: Option<&str>, target: &str, now_ms: u64) -> CapDecision {
    let existing = existing.and_then(CapState::parse);
    let tries = CapState::tries_for(existing.as_ref(), target);
    if tries >= MAX_TRIES_PER_TARGET {
        return CapDecision::TriesExhausted;
    }
    let (window_start_ms, count) = CapState::live_window(existing.as_ref(), now_ms);
    if count >= MAX_AUTO_RELOADS_PER_WINDOW {
        return CapDecision::RateLimited;
    }
    let min_delay_ms = match tries {
        0 => 0,
        n => RETRY_DELAYS_MS[(n as usize - 1).min(RETRY_DELAYS_MS.len() - 1)],
    };
    CapDecision::Allow {
        param: CapState {
            target: target.to_string(),
            window_start_ms,
            count: count + 1,
            tries: tries + 1,
        }
        .encode(),
        min_delay_ms,
    }
}

/// The [`RELOAD_PARAM`] value for a reload the reader asked for. It spends
/// neither budget, because a reader clicking Reload is not a loop. It keeps
/// the target's tries, so automatic retries after it carry on where they were.
pub(crate) fn manual_param(existing: Option<&str>, target: &str, now_ms: u64) -> String {
    let existing = existing.and_then(CapState::parse);
    let (window_start_ms, count) = CapState::live_window(existing.as_ref(), now_ms);
    CapState {
        target: target.to_string(),
        window_start_ms,
        count,
        tries: CapState::tries_for(existing.as_ref(), target),
    }
    .encode()
}

/// The URL to reload to: this document's own path, query and fragment, with
/// [`RELOAD_PARAM`] replaced and the auth-reload marker dropped.
///
/// Keeping the query matters: the node serves the app into the shell's iframe
/// at `?__sandbox=1`, and a reload without it would load the shell inside the
/// iframe.
pub(crate) fn reload_href(pathname: &str, search: &str, hash: &str, param: &str) -> String {
    let mut pairs: Vec<&str> = search
        .trim_start_matches('?')
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("");
            !pair.is_empty() && key != RELOAD_PARAM && key != AUTH_RELOAD_PARAM
        })
        .collect();
    let ours = format!("{RELOAD_PARAM}={param}");
    pairs.push(&ours);
    format!("{pathname}?{}{hash}", pairs.join("&"))
}

/// Read [`RELOAD_PARAM`] out of a `location.search` string.
pub(crate) fn reload_param_of(search: &str) -> Option<&str> {
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix(RELOAD_PARAM)?.strip_prefix('='))
}

/// Where the reload plan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Nothing newer seen.
    Idle,
    /// Hidden tab: reload at `due_ms`, never before `not_before_ms`.
    Hidden { due_ms: u64, not_before_ms: u64 },
    /// Visible tab: banner counting down to `due_ms`, never before
    /// `not_before_ms`.
    Countdown { due_ms: u64, not_before_ms: u64 },
    /// The reader said "Not now". No countdown while visible; an automatic
    /// reload resumes once the tab is hidden.
    Snoozed { not_before_ms: u64 },
    /// A cap refused an automatic reload. Banner with a manual Reload only.
    ManualOnly,
}

/// What the page looks like at one tick.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Inputs {
    pub now_ms: u64,
    pub visible: bool,
    /// Unsent text in a composer, or a message being edited.
    pub composing: bool,
    /// Uniform in `0..=HIDDEN_DELAY_JITTER_MS`.
    pub jitter_ms: u64,
}

fn countdown(i: Inputs, not_before_ms: u64) -> Phase {
    Phase::Countdown {
        due_ms: (i.now_ms + COUNTDOWN_MS).max(not_before_ms),
        not_before_ms,
    }
}

fn hidden_wait(i: Inputs, not_before_ms: u64) -> Phase {
    Phase::Hidden {
        due_ms: (i.now_ms + HIDDEN_DELAY_MIN_MS + i.jitter_ms).max(not_before_ms),
        not_before_ms,
    }
}

/// The phase a newer generation starts, once the caps allow a reload no
/// sooner than `min_delay_ms` from now.
pub(crate) fn start(i: Inputs, min_delay_ms: u64) -> Phase {
    let not_before_ms = i.now_ms + min_delay_ms;
    if i.visible {
        countdown(i, not_before_ms)
    } else {
        hidden_wait(i, not_before_ms)
    }
}

/// Re-plan after the target moved, from the caps' decision for the NEW target.
///
/// Called on every move, not only the first. Both pointers usually move
/// together, and their answers arrive milliseconds apart: the first names a
/// partial target (`r2d1`) that has spent no tries, so it plans no delay, and
/// the second names the real one (`r2d2`), which may be a retry owed a
/// backoff. Keeping the first plan would retry every countdown and spend every
/// try in under a minute. So a running plan takes the later of its own wait
/// and the new delay, and never gets shorter.
pub(crate) fn replan(phase: Phase, decision: &CapDecision, i: Inputs) -> Phase {
    let CapDecision::Allow { min_delay_ms, .. } = decision else {
        return Phase::ManualOnly;
    };
    let floor = i.now_ms + min_delay_ms;
    match phase {
        // ManualOnly too: a new target is a new decision, and this one allows.
        Phase::Idle | Phase::ManualOnly => start(i, *min_delay_ms),
        Phase::Countdown {
            due_ms,
            not_before_ms,
        } => {
            let not_before_ms = not_before_ms.max(floor);
            Phase::Countdown {
                due_ms: due_ms.max(not_before_ms),
                not_before_ms,
            }
        }
        Phase::Hidden {
            due_ms,
            not_before_ms,
        } => {
            let not_before_ms = not_before_ms.max(floor);
            Phase::Hidden {
                due_ms: due_ms.max(not_before_ms),
                not_before_ms,
            }
        }
        Phase::Snoozed { not_before_ms } => Phase::Snoozed {
            not_before_ms: not_before_ms.max(floor),
        },
    }
}

/// Advance the plan one tick. Returns the next phase and whether to reload now.
///
/// Never reloads while `composing`: the reload waits for the draft to go.
pub(crate) fn step(phase: Phase, i: Inputs) -> (Phase, bool) {
    match phase {
        Phase::Idle | Phase::ManualOnly => (phase, false),
        Phase::Snoozed { .. } if i.visible => (phase, false),
        Phase::Snoozed { not_before_ms } => (hidden_wait(i, not_before_ms), false),
        Phase::Hidden { not_before_ms, .. } if i.visible => (countdown(i, not_before_ms), false),
        Phase::Countdown { not_before_ms, .. } if !i.visible => {
            (hidden_wait(i, not_before_ms), false)
        }
        Phase::Hidden { due_ms, .. } | Phase::Countdown { due_ms, .. } => {
            (phase, i.now_ms >= due_ms && !i.composing)
        }
    }
}

/// What the banner shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// "Reloading in N s", with Reload now / Not now.
    Countdown { secs: u64 },
    /// The countdown ran out while the reader has unsent text.
    WaitingForDraft,
    /// No automatic reload is coming; Reload only.
    Manual,
}

/// The banner for a phase. A hidden tab needs none; it gets one on the tick
/// that sees it visible again.
pub(crate) fn notice(phase: Phase, i: Inputs) -> Option<Notice> {
    match phase {
        Phase::Idle | Phase::Hidden { .. } => None,
        Phase::Countdown { due_ms, .. } if i.now_ms >= due_ms && i.composing => {
            Some(Notice::WaitingForDraft)
        }
        Phase::Countdown { due_ms, .. } => Some(Notice::Countdown {
            secs: due_ms.saturating_sub(i.now_ms).div_ceil(1000),
        }),
        Phase::Snoozed { .. } | Phase::ManualOnly => Some(Notice::Manual),
    }
}

/// Route a GET response. Returns `true` if `id` is a watched pointer, in
/// which case the caller must not treat it as a room.
pub fn deliver_state(id: &ContractInstanceId, state: &[u8]) -> bool {
    let Some(watched) = WATCHED.iter().find(|w| &w.pointer_id == id) else {
        return false;
    };
    let verdict = evaluate(watched, state);
    let app_id = watched.artifact.app_id();
    match verdict {
        Verdict::Current => info!(
            "generation watch: {app_id} is current (v{})",
            watched.build_version()
        ),
        Verdict::Newer(v) => warn!(
            "generation watch: {app_id} moved to v{v}; this build is v{}",
            watched.build_version()
        ),
        Verdict::NoSignal => info!("generation watch: {app_id}: no usable record in response"),
    }
    let moved = OBSERVED
        .lock()
        .map(|mut o| o.record(watched.artifact, verdict))
        .unwrap_or(false);
    if moved {
        runtime::on_newer_generation();
    }
    true
}

/// Route an update notification. Returns `true` if `id` is a watched pointer.
///
/// The pointer contract's delta is the whole 100-byte record, so a `Delta`
/// is judged exactly like a `State`.
pub fn deliver_update(id: &ContractInstanceId, update: &UpdateData) -> bool {
    if !is_watched(id) {
        return false;
    }
    match update {
        UpdateData::State(state) => deliver_state(id, state.as_ref()),
        UpdateData::Delta(delta) => deliver_state(id, delta.as_ref()),
        UpdateData::StateAndDelta { state, .. } => deliver_state(id, state.as_ref()),
        _ => true,
    }
}

/// Whether `id` is one of the watched pointers.
pub fn is_watched(id: &ContractInstanceId) -> bool {
    WATCHED.iter().any(|w| &w.pointer_id == id)
}

pub use runtime::{on_connected, on_page_visible, reload_now, snooze, UPDATE_NOTICE};

#[cfg(target_arch = "wasm32")]
mod runtime {
    use super::*;
    use crate::components::app::WEB_API;
    use dioxus::prelude::*;
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;
    use wasm_bindgen::JsCast;

    /// What the update banner shows, if anything.
    pub static UPDATE_NOTICE: GlobalSignal<Option<Notice>> = Signal::global(|| None);

    static PLAN: Mutex<Phase> = Mutex::new(Phase::Idle);
    static DRIVER_RUNNING: AtomicBool = AtomicBool::new(false);
    static BACKSTOP_RUNNING: AtomicBool = AtomicBool::new(false);
    static LAST_CHECK_MS: AtomicU64 = AtomicU64::new(0);

    /// Backstop re-check interval. The subscription is the primary signal;
    /// this catches a notification dropped on a full channel.
    const BACKSTOP_INTERVAL_MS: f64 = 15.0 * 60.0 * 1000.0;
    /// Least time between visibility-triggered re-checks.
    const VISIBLE_RECHECK_MIN_MS: u64 = 2 * 60 * 1000;
    /// Driver tick.
    const TICK: Duration = Duration::from_secs(1);

    fn now_ms() -> u64 {
        js_sys::Date::now() as u64
    }

    fn jitter_ms() -> u64 {
        (js_sys::Math::random() * HIDDEN_DELAY_JITTER_MS as f64) as u64
    }

    fn location() -> Option<web_sys::Location> {
        web_sys::window().map(|w| w.location())
    }

    fn existing_param() -> Option<String> {
        let search = location()?.search().ok()?;
        reload_param_of(&search).map(str::to_string)
    }

    fn target_key() -> Option<String> {
        let observed = *OBSERVED.lock().ok()?;
        observed.is_stale().then(|| observed.target_key(&WATCHED))
    }

    /// Unsent text in the room composer or a DM thread, or a message edit.
    fn user_is_composing() -> bool {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return false;
        };
        let has_text = |el: &web_sys::Element| {
            el.dyn_ref::<web_sys::HtmlTextAreaElement>()
                .map(|t| t.value())
                .or_else(|| el.dyn_ref::<web_sys::HtmlInputElement>().map(|i| i.value()))
                .is_some_and(|v| !v.trim().is_empty())
        };
        if let Ok(Some(el)) = document.query_selector("#message-input") {
            if has_text(&el) {
                return true;
            }
        }
        if let Ok(list) = document.query_selector_all("[data-testid=\"dm-thread-modal\"] textarea")
        {
            for i in 0..list.length() {
                if let Some(el) = list
                    .item(i)
                    .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
                {
                    if has_text(&el) {
                        return true;
                    }
                }
            }
        }
        matches!(document.query_selector("[id^=\"edit-msg-\"]"), Ok(Some(_)))
    }

    fn inputs() -> Inputs {
        Inputs {
            now_ms: now_ms(),
            visible: crate::components::app::document_title::get_visibility_state(),
            composing: user_is_composing(),
            jitter_ms: jitter_ms(),
        }
    }

    fn set_notice(next: Option<Notice>) {
        crate::util::defer(move || {
            if UPDATE_NOTICE.try_read().map(|n| *n != next).unwrap_or(true) {
                *UPDATE_NOTICE.write() = next;
            }
        });
    }

    fn set_phase(phase: Phase) {
        if let Ok(mut p) = PLAN.lock() {
            *p = phase;
        }
    }

    fn navigate(param: &str) {
        let Some(location) = location() else { return };
        let href = reload_href(
            &location.pathname().unwrap_or_default(),
            &location.search().unwrap_or_default(),
            &location.hash().unwrap_or_default(),
            param,
        );
        info!("generation watch: reloading to {href}");
        if let Err(e) = location.replace(&href) {
            warn!("generation watch: reload failed: {e:?}");
        }
    }

    /// An automatic reload, if the caps allow it; otherwise fall back to the
    /// manual banner.
    fn auto_reload(target: &str) {
        match cap_decision(existing_param().as_deref(), target, now_ms()) {
            CapDecision::Allow { param, .. } => navigate(&param),
            refused => {
                warn!("generation watch: automatic reload refused ({refused:?}); banner only");
                set_phase(Phase::ManualOnly);
                set_notice(Some(Notice::Manual));
            }
        }
    }

    /// A newer generation was just seen.
    pub(super) fn on_newer_generation() {
        let Some(target) = target_key() else { return };
        let decision = cap_decision(existing_param().as_deref(), &target, now_ms());
        match &decision {
            CapDecision::Allow { min_delay_ms, .. } => info!(
                "generation watch: reload planned for {target} (no sooner than {min_delay_ms} ms)"
            ),
            refused => warn!(
                "generation watch: automatic reload for {target} refused ({refused:?}); banner only"
            ),
        }
        if let Ok(mut phase) = PLAN.lock() {
            *phase = replan(*phase, &decision, inputs());
        }
        if !DRIVER_RUNNING.swap(true, Ordering::SeqCst) {
            crate::util::safe_spawn_local(drive());
        }
    }

    async fn drive() {
        loop {
            let i = inputs();
            let Ok(current) = PLAN.lock().map(|p| *p) else {
                break;
            };
            let (next, reload) = step(current, i);
            set_phase(next);
            set_notice(notice(next, i));
            if reload {
                if let Some(target) = target_key() {
                    auto_reload(&target);
                }
            }
            if matches!(next, Phase::Idle | Phase::ManualOnly) && !reload {
                break;
            }
            crate::util::sleep(TICK).await;
        }
        DRIVER_RUNNING.store(false, Ordering::SeqCst);
    }

    /// The banner's Reload button.
    pub fn reload_now() {
        let Some(target) = target_key() else { return };
        navigate(&manual_param(
            existing_param().as_deref(),
            &target,
            now_ms(),
        ));
    }

    /// The banner's "Not now" button.
    pub fn snooze() {
        if let Ok(mut phase) = PLAN.lock() {
            if let Phase::Countdown { not_before_ms, .. } | Phase::Hidden { not_before_ms, .. } =
                *phase
            {
                *phase = Phase::Snoozed { not_before_ms };
            }
        }
        set_notice(Some(Notice::Manual));
    }

    fn send_get(id: ContractInstanceId, subscribe: bool) {
        crate::util::safe_spawn_local(async move {
            // No code: the 100-byte record is the whole answer, and the
            // pointer's own WASM is ~130 KB.
            let request = ContractRequest::Get {
                key: id,
                return_contract_code: false,
                subscribe,
                blocking_subscribe: false,
            };
            if let Some(web_api) = WEB_API.write().as_mut() {
                if let Err(e) = web_api.send(ClientRequest::ContractOp(request)).await {
                    warn!("generation watch: pointer GET failed to send: {e}");
                }
            }
        });
    }

    fn check_all(subscribe: bool) {
        LAST_CHECK_MS.store(now_ms(), Ordering::SeqCst);
        for w in WATCHED.iter() {
            send_get(w.pointer_id, subscribe);
        }
    }

    /// After every (re)connect: GET + subscribe. A subscription does not
    /// survive a WebSocket reconnect, so this runs each time.
    pub fn on_connected() {
        if WATCHED.is_empty() {
            return;
        }
        check_all(true);
        if !BACKSTOP_RUNNING.swap(true, Ordering::SeqCst) {
            crate::util::safe_spawn_local(async {
                loop {
                    let jitter = 0.8 + 0.4 * js_sys::Math::random();
                    crate::util::sleep(Duration::from_millis(
                        (BACKSTOP_INTERVAL_MS * jitter) as u64,
                    ))
                    .await;
                    check_all(false);
                }
            });
        }
    }

    /// The page became visible with a live connection: re-check, at most
    /// every [`VISIBLE_RECHECK_MIN_MS`].
    pub fn on_page_visible() {
        if WATCHED.is_empty() {
            return;
        }
        let last = LAST_CHECK_MS.load(Ordering::SeqCst);
        if now_ms().saturating_sub(last) >= VISIBLE_RECHECK_MIN_MS {
            check_all(false);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod runtime {
    use super::Notice;
    use dioxus::prelude::*;

    pub static UPDATE_NOTICE: GlobalSignal<Option<Notice>> = Signal::global(|| None);

    pub(super) fn on_newer_generation() {}
    pub fn on_connected() {}
    pub fn on_page_visible() {}
    pub fn reload_now() {}
    pub fn snooze() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(now_ms: u64) -> Inputs {
        Inputs {
            now_ms,
            visible: true,
            composing: false,
            jitter_ms: 0,
        }
    }

    fn hidden(now_ms: u64) -> Inputs {
        Inputs {
            visible: false,
            ..visible(now_ms)
        }
    }

    // ---- build identity --------------------------------------------------

    /// The shipped build must be able to watch both pointers. If this fails,
    /// `pointer-records.toml` no longer names the WASM the UI embeds, and the
    /// UI would never notice a re-key.
    #[test]
    fn shipped_build_watches_both_pointers() {
        let watched = watched_pointers(POINTER_AUTHOR_VK, POINTER_RECORDS, Artifact::embedded_wasm);
        let apps: Vec<_> = watched.iter().map(|w| w.artifact).collect();
        assert_eq!(apps, Artifact::ALL);
    }

    /// The address the UI derives must be the one the records file pins, which
    /// `scripts/check-pointer-freshness.sh` also checks for the signer.
    #[test]
    fn pointer_ids_match_the_records_file() {
        let toml_text = include_str!("../../../../../pointer-records.toml");
        let watched = watched_pointers(POINTER_AUTHOR_VK, POINTER_RECORDS, Artifact::embedded_wasm);
        for w in &watched {
            let id = w.pointer_id.to_string();
            assert!(
                toml_text.contains(&format!("pointer_key = \"{id}\"")),
                "{} derives pointer {id}, which pointer-records.toml does not pin",
                w.artifact.app_id()
            );
        }
    }

    #[test]
    fn mismatched_embedded_wasm_disables_only_that_pointer() {
        let watched = watched_pointers(POINTER_AUTHOR_VK, POINTER_RECORDS, |a| match a {
            Artifact::RoomContract => b"a locally rebuilt contract",
            Artifact::ChatDelegate => Artifact::ChatDelegate.embedded_wasm(),
        });
        let apps: Vec<_> = watched.iter().map(|w| w.artifact).collect();
        assert_eq!(apps, vec![Artifact::ChatDelegate]);
    }

    #[test]
    fn undecodable_author_key_disables_the_watch() {
        assert!(
            watched_pointers("river:v1:vk:nope", POINTER_RECORDS, Artifact::embedded_wasm)
                .is_empty()
        );
        assert!(watched_pointers("", POINTER_RECORDS, Artifact::embedded_wasm).is_empty());
    }

    // ---- verdicts --------------------------------------------------------

    fn signed_state(
        sk: &ed25519_dalek::SigningKey,
        app_id: &str,
        version: u32,
        code_hash: [u8; 32],
    ) -> Vec<u8> {
        use ed25519_dalek::Signer;
        let params = freenet_migrate::pointer_params(&sk.verifying_key(), app_id.as_bytes())
            .expect("valid params");
        let msg = freenet_migrate::pointer_signing_message(&params, version, &code_hash);
        freenet_migrate::PointerRecord {
            version,
            code_hash,
            signature: sk.sign(&msg).to_bytes(),
        }
        .encode()
        .to_vec()
    }

    /// A watched room-contract pointer at build version 4, signed by a test key.
    fn test_watch() -> (ed25519_dalek::SigningKey, Watched, [u8; 32]) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let author = format!(
            "{AUTHOR_VK_PREFIX}{}",
            bs58::encode(sk.verifying_key().as_bytes()).into_string()
        );
        let wasm: &'static [u8] = b"room contract v4";
        let records = [BuildPointerRecord {
            app_id: "river.room-contract",
            version: 4,
            code_hash: *blake3::hash(wasm).as_bytes(),
        }];
        let watched = watched_pointers(&author, &records, |_| wasm);
        assert_eq!(watched.len(), 1);
        (sk, watched[0], records[0].code_hash)
    }

    #[test]
    fn same_record_as_the_build_is_current() {
        let (sk, w, hash) = test_watch();
        let state = signed_state(&sk, "river.room-contract", 4, hash);
        assert_eq!(evaluate(&w, &state), Verdict::Current);
    }

    #[test]
    fn a_newer_signed_record_is_newer() {
        let (sk, w, _) = test_watch();
        let state = signed_state(&sk, "river.room-contract", 5, [0x55; 32]);
        assert_eq!(evaluate(&w, &state), Verdict::Newer(5));
    }

    /// The pointer is published after the UI, so a fresh build sees it lag.
    /// That must never read as a reason to reload (it would reload into the
    /// same build forever, bounded only by the caps).
    #[test]
    fn a_pointer_behind_the_build_is_no_signal() {
        let (sk, w, _) = test_watch();
        let state = signed_state(&sk, "river.room-contract", 3, [0x33; 32]);
        assert_eq!(evaluate(&w, &state), Verdict::NoSignal);
    }

    #[test]
    fn unsigned_or_foreign_records_are_no_signal() {
        let (_, w, _) = test_watch();
        let stranger = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let forged = signed_state(&stranger, "river.room-contract", 9, [0x99; 32]);
        assert_eq!(evaluate(&w, &forged), Verdict::NoSignal);
        let (sk, _, _) = test_watch();
        let other_app = signed_state(&sk, "river.chat-delegate", 9, [0x99; 32]);
        assert_eq!(evaluate(&w, &other_app), Verdict::NoSignal);
        assert_eq!(evaluate(&w, &[]), Verdict::NoSignal);
        assert_eq!(evaluate(&w, &[0u8; 7]), Verdict::NoSignal);
    }

    #[test]
    fn a_withdrawal_is_no_signal() {
        let (sk, w, _) = test_watch();
        let state = signed_state(&sk, "river.room-contract", 5, [0u8; 32]);
        assert_eq!(evaluate(&w, &state), Verdict::NoSignal);
    }

    // ---- observed target -------------------------------------------------

    #[test]
    fn observed_tracks_the_newest_version_and_names_the_target() {
        let (_, w, _) = test_watch();
        let mut o = Observed::default();
        assert!(!o.is_stale());
        assert!(!o.record(Artifact::RoomContract, Verdict::Current));
        assert!(!o.record(Artifact::RoomContract, Verdict::NoSignal));
        assert!(o.record(Artifact::RoomContract, Verdict::Newer(5)));
        assert!(o.is_stale());
        assert!(
            !o.record(Artifact::RoomContract, Verdict::Newer(5)),
            "same version"
        );
        assert!(
            !o.record(Artifact::RoomContract, Verdict::Newer(4)),
            "older"
        );
        assert_eq!(o.target_key(&[w]), "r5d0");
        assert!(o.record(Artifact::ChatDelegate, Verdict::Newer(6)));
        assert_eq!(o.target_key(&[w]), "r5d6");
    }

    // ---- caps ------------------------------------------------------------

    fn allow(param: &str, min_delay_ms: u64) -> CapDecision {
        CapDecision::Allow {
            param: param.into(),
            min_delay_ms,
        }
    }

    #[test]
    fn first_reload_is_immediate_and_counted() {
        assert_eq!(cap_decision(None, "r5d5", 1_000), allow("r5d5-1000-1-1", 0));
    }

    /// A reload that lands on the old bundle (the node has the 100-byte
    /// pointer but not yet the 1.2 MB web container) sees the same newer
    /// pointer again. It retries, later each time, and then stops: the loop
    /// this module must not have is the one that never ends.
    #[test]
    fn a_target_is_retried_with_backoff_then_given_up() {
        let mut param: Option<String> = None;
        let mut delays = Vec::new();
        for i in 0..MAX_TRIES_PER_TARGET {
            match cap_decision(param.as_deref(), "r5d5", 1_000 + u64::from(i)) {
                CapDecision::Allow {
                    param: p,
                    min_delay_ms,
                } => {
                    param = Some(p);
                    delays.push(min_delay_ms);
                }
                other => panic!("try {i} refused: {other:?}"),
            }
        }
        assert_eq!(
            delays,
            [
                0,
                RETRY_DELAYS_MS[0],
                RETRY_DELAYS_MS[1],
                RETRY_DELAYS_MS[2]
            ]
        );
        assert_eq!(
            cap_decision(param.as_deref(), "r5d5", 2_000),
            CapDecision::TriesExhausted
        );
        // Not even a fresh hour revives a target whose tries are spent.
        assert_eq!(
            cap_decision(param.as_deref(), "r5d5", 1_000 + 10 * AUTO_RELOAD_WINDOW_MS),
            CapDecision::TriesExhausted
        );
    }

    /// The tries cover at least the ~20 minutes a slow web container may take
    /// to reach a node after the pointer did.
    #[test]
    fn retries_span_web_container_propagation() {
        let span: u64 = RETRY_DELAYS_MS.iter().sum();
        assert!(span >= 20 * 60 * 1000, "retries span only {span} ms");
        assert!(
            RETRY_DELAYS_MS.windows(2).all(|w| w[0] <= w[1]),
            "backoff must not shrink"
        );
    }

    #[test]
    fn a_new_target_starts_its_own_tries() {
        let after_two = "r5d5-1000-2-2";
        assert_eq!(
            cap_decision(Some(after_two), "r5d6", 2_000),
            allow("r5d6-1000-3-1", 0)
        );
    }

    #[test]
    fn the_hourly_ceiling_holds_across_targets() {
        let mut param: Option<String> = None;
        for i in 0..MAX_AUTO_RELOADS_PER_WINDOW {
            let target = format!("r{}d5", 10 + i);
            match cap_decision(param.as_deref(), &target, 1_000 + u64::from(i)) {
                CapDecision::Allow { param: p, .. } => param = Some(p),
                other => panic!("reload {i} refused: {other:?}"),
            }
        }
        assert_eq!(
            cap_decision(param.as_deref(), "r99d5", 2_000),
            CapDecision::RateLimited
        );
        // A fresh window once the old one has passed.
        assert!(matches!(
            cap_decision(param.as_deref(), "r99d5", 1_000 + AUTO_RELOAD_WINDOW_MS),
            CapDecision::Allow { .. }
        ));
    }

    #[test]
    fn a_future_or_malformed_window_starts_fresh() {
        assert_eq!(
            cap_decision(Some("r5d5-99999-6-1"), "r6d5", 1_000),
            allow("r6d5-1000-1-1", 0)
        );
        for junk in ["garbage", "r5d5-1-1", "r5d5-x-1-1", ""] {
            assert_eq!(
                cap_decision(Some(junk), "r6d5", 1_000),
                allow("r6d5-1000-1-1", 0)
            );
        }
    }

    #[test]
    fn a_manual_reload_spends_no_budget() {
        // Tries and the window count are carried, not incremented.
        assert_eq!(
            manual_param(Some("r6d5-1000-2-2"), "r6d5", 2_000),
            "r6d5-1000-2-2"
        );
        // A target with no tries yet keeps zero, so the page it loads may
        // still retry automatically from the start.
        let param = manual_param(Some("r5d5-1000-2-2"), "r6d5", 2_000);
        assert_eq!(param, "r6d5-1000-2-0");
        assert_eq!(
            cap_decision(Some(&param), "r6d5", 3_000),
            allow("r6d5-1000-3-1", 0)
        );
    }

    // ---- URL -------------------------------------------------------------

    #[test]
    fn reload_href_keeps_the_sandbox_query_and_fragment() {
        assert_eq!(
            reload_href(
                "/v1/contract/web/abc/",
                "?__sandbox=1&invitation=xyz",
                "#room",
                "r5d5-1-1"
            ),
            "/v1/contract/web/abc/?__sandbox=1&invitation=xyz&_rgen=r5d5-1-1#room"
        );
    }

    #[test]
    fn reload_href_replaces_its_own_markers() {
        assert_eq!(
            reload_href(
                "/p/",
                "?__sandbox=1&_rgen=r4d4-1-1&_reload=123",
                "",
                "r5d5-1-2"
            ),
            "/p/?__sandbox=1&_rgen=r5d5-1-2"
        );
        assert_eq!(reload_href("/p/", "", "", "x-1-1"), "/p/?_rgen=x-1-1");
    }

    #[test]
    fn reload_param_round_trips_through_the_url() {
        let href = reload_href("/p/", "?__sandbox=1", "", "r5d5-10-1-1");
        let search = &href[href.find('?').unwrap()..];
        assert_eq!(reload_param_of(search), Some("r5d5-10-1-1"));
        assert_eq!(reload_param_of("?__sandbox=1"), None);
        assert_eq!(reload_param_of("?_rgenx=1"), None);
    }

    // ---- timing ----------------------------------------------------------

    #[test]
    fn a_visible_tab_counts_down_then_reloads() {
        let phase = start(visible(0), 0);
        assert_eq!(
            phase,
            Phase::Countdown {
                due_ms: COUNTDOWN_MS,
                not_before_ms: 0
            }
        );
        assert_eq!(
            notice(phase, visible(0)),
            Some(Notice::Countdown { secs: 15 })
        );
        assert_eq!(step(phase, visible(COUNTDOWN_MS - 1)), (phase, false));
        assert_eq!(step(phase, visible(COUNTDOWN_MS)), (phase, true));
    }

    #[test]
    fn a_hidden_tab_waits_past_the_node_refresh_window() {
        let i = Inputs {
            jitter_ms: 500,
            ..hidden(0)
        };
        let phase = start(i, 0);
        let due = HIDDEN_DELAY_MIN_MS + 500;
        assert_eq!(
            phase,
            Phase::Hidden {
                due_ms: due,
                not_before_ms: 0
            }
        );
        assert!(
            due > 30_000,
            "must outlast the node's 30 s bundle refresh TTL"
        );
        assert_eq!(notice(phase, i), None);
        assert_eq!(step(phase, hidden(due - 1)), (phase, false));
        assert_eq!(step(phase, hidden(due)), (phase, true));
    }

    /// A retry's delay holds whatever the tab does: visible, hidden, snoozed.
    #[test]
    fn a_retry_delay_survives_every_transition() {
        let delay = RETRY_DELAYS_MS[1];
        let phase = start(visible(0), delay);
        assert_eq!(
            notice(phase, visible(0)),
            Some(Notice::Countdown { secs: delay / 1000 })
        );
        assert_eq!(step(phase, visible(delay - 1)), (phase, false));

        let (hidden_phase, _) = step(phase, hidden(10));
        assert_eq!(step(hidden_phase, hidden(delay - 1)), (hidden_phase, false));
        let (back, _) = step(hidden_phase, visible(20));
        assert_eq!(step(back, visible(delay - 1)), (back, false));

        let snoozed = Phase::Snoozed {
            not_before_ms: delay,
        };
        let (resumed, _) = step(snoozed, hidden(30));
        assert_eq!(step(resumed, hidden(delay - 1)), (resumed, false));
        assert_eq!(step(resumed, hidden(delay)), (resumed, true));
    }

    #[test]
    fn never_reloads_over_a_draft() {
        let phase = Phase::Countdown {
            due_ms: 10,
            not_before_ms: 0,
        };
        let typing = Inputs {
            composing: true,
            ..visible(1_000_000)
        };
        assert_eq!(step(phase, typing), (phase, false));
        assert_eq!(notice(phase, typing), Some(Notice::WaitingForDraft));
        assert_eq!(step(phase, visible(1_000_001)), (phase, true));

        let phase = Phase::Hidden {
            due_ms: 10,
            not_before_ms: 0,
        };
        let typing = Inputs {
            composing: true,
            ..hidden(1_000_000)
        };
        assert_eq!(step(phase, typing), (phase, false));
    }

    #[test]
    fn switching_visibility_restarts_the_right_clock() {
        let hidden_phase = Phase::Hidden {
            due_ms: 999_999,
            not_before_ms: 0,
        };
        assert_eq!(
            step(hidden_phase, visible(100)),
            (
                Phase::Countdown {
                    due_ms: 100 + COUNTDOWN_MS,
                    not_before_ms: 0
                },
                false
            )
        );
        let counting = Phase::Countdown {
            due_ms: 5,
            not_before_ms: 0,
        };
        assert_eq!(
            step(counting, hidden(100)),
            (
                Phase::Hidden {
                    due_ms: 100 + HIDDEN_DELAY_MIN_MS,
                    not_before_ms: 0
                },
                false
            )
        );
    }

    #[test]
    fn not_now_holds_while_visible_and_resumes_when_hidden() {
        let snoozed = Phase::Snoozed { not_before_ms: 0 };
        assert_eq!(step(snoozed, visible(5)), (snoozed, false));
        assert_eq!(notice(snoozed, visible(5)), Some(Notice::Manual));
        assert_eq!(
            step(snoozed, hidden(5)),
            (
                Phase::Hidden {
                    due_ms: 5 + HIDDEN_DELAY_MIN_MS,
                    not_before_ms: 0
                },
                false
            )
        );
    }

    /// Regression, found end to end: both pointers move, the reload lands on
    /// the old build, and the reloaded page hears the room pointer ~7 ms
    /// before the delegate's. The partial target `r2d1` has spent no tries
    /// (they were recorded against `r2d2`), so it plans no delay; unless the
    /// second answer re-plans, every retry fires after the 15 s countdown and
    /// all four are gone in under a minute.
    #[test]
    fn the_second_pointer_answer_restores_the_retry_backoff() {
        let (_, w, _) = test_watch();
        let delegate = Watched {
            artifact: Artifact::ChatDelegate,
            ..w
        };
        let watched = [w, delegate];
        let landed_with = "r2d2-1000-1-1"; // one try already spent on r2d2
        let now = 5_000;

        let mut observed = Observed::default();
        assert!(observed.record(Artifact::RoomContract, Verdict::Newer(2)));
        let partial = observed.target_key(&watched);
        assert_eq!(partial, "r2d4", "delegate still at the build's version");
        let first = cap_decision(Some(landed_with), &partial, now);
        let phase = replan(Phase::Idle, &first, visible(now));
        assert_eq!(
            phase,
            start(visible(now), 0),
            "partial target: no delay yet"
        );

        assert!(observed.record(Artifact::ChatDelegate, Verdict::Newer(2)));
        let full = observed.target_key(&watched);
        assert_eq!(full, "r2d2");
        let second = cap_decision(Some(landed_with), &full, now + 7);
        assert!(matches!(
            second,
            CapDecision::Allow { min_delay_ms, .. } if min_delay_ms == RETRY_DELAYS_MS[0]
        ));
        let phase = replan(phase, &second, visible(now + 7));
        let Phase::Countdown { due_ms, .. } = phase else {
            panic!("expected a countdown, got {phase:?}");
        };
        assert!(
            due_ms >= now + 7 + RETRY_DELAYS_MS[0],
            "backoff lost: due {due_ms}"
        );
        assert_eq!(step(phase, visible(now + COUNTDOWN_MS)), (phase, false));
    }

    #[test]
    fn replan_never_shortens_a_wait() {
        let long = Phase::Hidden {
            due_ms: 900_000,
            not_before_ms: 600_000,
        };
        let no_delay = cap_decision(None, "r9d9", 1_000);
        assert_eq!(replan(long, &no_delay, hidden(1_000)), long);
        let snoozed = Phase::Snoozed {
            not_before_ms: 600_000,
        };
        assert_eq!(replan(snoozed, &no_delay, visible(1_000)), snoozed);
    }

    #[test]
    fn replan_refusal_is_manual_and_a_new_allowed_target_resumes() {
        let running = start(visible(0), 0);
        assert_eq!(
            replan(running, &CapDecision::RateLimited, visible(0)),
            Phase::ManualOnly
        );
        assert_eq!(
            replan(running, &CapDecision::TriesExhausted, visible(0)),
            Phase::ManualOnly
        );
        let allowed = cap_decision(None, "r3d3", 50);
        assert_eq!(
            replan(Phase::ManualOnly, &allowed, visible(50)),
            start(visible(50), 0)
        );
    }

    #[test]
    fn manual_only_and_idle_never_reload() {
        for phase in [Phase::Idle, Phase::ManualOnly] {
            for i in [visible(u64::MAX / 2), hidden(u64::MAX / 2)] {
                assert_eq!(step(phase, i), (phase, false));
            }
        }
        assert_eq!(notice(Phase::Idle, visible(0)), None);
        assert_eq!(notice(Phase::ManualOnly, visible(0)), Some(Notice::Manual));
    }

    // ---- wiring ----------------------------------------------------------

    /// Each hook must run BEFORE the room lookup, or a pointer response is
    /// logged as an unknown contract and dropped.
    #[test]
    fn response_hooks_are_wired_before_room_lookup() {
        let src = |s: &str| crate::util::strip_comments(s);
        let get = src(include_str!("response_handler/get_response.rs"));
        let get_hook = get
            .find("generation_watch::deliver_state(")
            .expect("GET hook");
        let probe = get.find("deliver_probe_response(").expect("probe hook");
        assert!(
            get_hook < probe,
            "pointer GET hook must precede the probe hook"
        );
        let nf_hook = get
            .find("generation_watch::is_watched(")
            .expect("NotFound hook");
        let nf_probe = get
            .find("deliver_probe_absent(")
            .expect("NotFound probe hook");
        assert!(
            nf_hook < nf_probe,
            "pointer NotFound hook must precede the probe hook"
        );

        let upd = src(include_str!("response_handler/update_notification.rs"));
        let upd_fn = upd.find("pub fn handle_update_notification").unwrap();
        let upd_hook = upd[upd_fn..]
            .find("generation_watch::deliver_update(")
            .expect("update hook");
        let upd_lookup = upd[upd_fn..].find("get_owner_vk_for_instance_id").unwrap();
        assert!(upd_hook < upd_lookup);

        let sub = src(include_str!("response_handler/subscribe_response.rs"));
        let sub_hook = sub
            .find("generation_watch::is_watched(")
            .expect("subscribe hook");
        let sub_lookup = sub.find("get_owner_vk_for_instance_id").unwrap();
        assert!(sub_hook < sub_lookup);

        let sync = src(include_str!("freenet_synchronizer.rs"));
        assert!(sync.contains("generation_watch::on_connected()"));
        assert!(sync.contains("generation_watch::on_page_visible()"));
    }
}
