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
//! `cli/src/pointer.rs`). It reloads rather than following the new key in
//! place for the same reason: a new generation can change the state format and
//! validation, and a delegate cannot be followed without the new build's
//! migration. A fresh load already does everything a re-key needs.
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
//! makes the same choice). A top-level refresh of the tab starts afresh, which
//! is the reader's own action, not a loop.
//!
//! 1. At most [`MAX_TRIES_PER_TARGET`] automatic reloads per [`Target`],
//!    spaced by [`RETRY_DELAYS_MS`] so a slow bundle has time to arrive. A
//!    target that is no newer in any artifact than the one already being
//!    retried counts as the same target, so a page that has so far heard only
//!    one of the two pointers cannot reset the count.
//! 2. At most [`MAX_AUTO_RELOADS_PER_WINDOW`] automatic reloads per
//!    [`AUTO_RELOAD_WINDOW_MS`], whatever the target.
//! 3. Past either cap, only a banner with a manual Reload.
//! 4. One reload per page: once a reload is under way the page only waits for
//!    it ([`Phase::Reloading`]); it never issues a second.
//!
//! A reload always fetches the newest bundle the node has, so a loop could
//! only persist if the newest build were itself broken, and the caps bound it
//! until the next publish replaces that build.
//!
//! # When to reload
//!
//! A hidden tab reloads after [`HIDDEN_DELAY_MIN_MS`] plus jitter, so every
//! open tab does not re-key at the same moment. A visible tab shows a banner
//! counting down [`COUNTDOWN_MS`] with "Reload now" and "Not now". Nothing
//! reloads within [`MIN_PAGE_AGE_MS`] of the page loading: until the node's
//! 30 s bundle-refresh window has passed, a reload is served the bundle this
//! page came from. No automatic reload happens while the reader is busy (see
//! `user_is_busy`) or within a few seconds of it, and pending delegate writes
//! are flushed first.
//!
//! # How it reloads
//!
//! The app runs in the node shell's sandboxed iframe, which may not navigate
//! the top window, and the shell refuses app-initiated reloads. It does honour
//! a same-contract `navigate`, which closes every WebSocket it proxied for the
//! old document before replacing it. That matters: the shim numbers sockets
//! from 1 in each document, so a socket left open by the old page would share
//! an id with the new page's and feed it stale notifications. A top-level
//! (unframed) dev build reloads itself. River's other self-navigation, on an
//! expired auth token, is freenet/river#522.
//!
//! The shell's `navigate` always pushes a history entry, so each reload adds
//! one, and Back reloads the previous frame path (with its older caps, which
//! takes a reader's press per round, so it cannot loop on its own). A
//! replacing `navigate` is freenet/freenet-core#5910.

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
pub(crate) const MAX_TRIES_PER_TARGET: u32 = 5;
/// The least wait before each retry for one target. A reload that landed on
/// the old bundle means the node does not hold the new one yet.
///
/// Sized from production telemetry over River UI publishes 30000397-403
/// (2026-10-12..16, 41 sampled deliveries of the ~1.2 MB web container):
/// median ~20 s, 71% within 60 s, 80% within 5 min, p90 ~10 min, max ~2 h.
/// The 100-byte pointer reached subscribers in under 0.5 s. The slow tail
/// looked like peers reconnecting rather than slow transfers. These delays
/// span ~67 minutes: past the p90 with room to spare, then the banner.
pub(crate) const RETRY_DELAYS_MS: [u64; 4] =
    [2 * 60 * 1000, 5 * 60 * 1000, 15 * 60 * 1000, 45 * 60 * 1000];
/// Most automatic reloads in one [`AUTO_RELOAD_WINDOW_MS`], whatever the
/// target. A backstop over the per-target bound.
pub(crate) const MAX_AUTO_RELOADS_PER_WINDOW: u32 = 6;
pub(crate) const AUTO_RELOAD_WINDOW_MS: u64 = 60 * 60 * 1000;

/// How long a visible tab's banner counts down before reloading.
pub(crate) const COUNTDOWN_MS: u64 = 15_000;
/// A hidden tab waits at least this long, plus jitter.
pub(crate) const HIDDEN_DELAY_MIN_MS: u64 = 30_000;
/// Upper bound of the random extra wait for a hidden tab.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) const HIDDEN_DELAY_JITTER_MS: u64 = 90_000;
/// No reload before the page is this old. It must exceed the node's 30 s
/// `CONTRACT_CACHE_REFRESH_TTL`: within that window the iframe path serves the
/// bundle it extracted last (this page's) without re-reading the container,
/// so an earlier reload is a try spent for nothing.
pub(crate) const MIN_PAGE_AGE_MS: u64 = 35_000;
/// A reload that has not replaced the page by now did not happen (an old node
/// without the shell's `navigate`, say). Fall back to the banner.
pub(crate) const RELOAD_STUCK_MS: u64 = 20_000;

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
        // `Resolved` also covers a different record at the build's OWN version
        // that wins the lower-code-hash tiebreak (the author signed twice at
        // one version). That is not a newer generation, and no build could
        // satisfy it, so only a strictly higher version counts.
        Some(Ok(PointerOutcome::Resolved(r))) if r.version() > watched.build_version() => {
            Verdict::Newer(r.version())
        }
        Some(Ok(PointerOutcome::Unchanged(_))) => Verdict::Current,
        _ => Verdict::NoSignal,
    }
}

/// The generation a reload is for: one version per artifact, written `r5d6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Target {
    pub room: u32,
    pub delegate: u32,
}

impl Target {
    fn encode(self) -> String {
        format!("r{}d{}", self.room, self.delegate)
    }

    fn parse(s: &str) -> Option<Self> {
        let (room, delegate) = s.strip_prefix('r')?.split_once('d')?;
        Some(Self {
            room: room.parse().ok()?,
            delegate: delegate.parse().ok()?,
        })
    }

    /// Nothing in `self` is newer than in `other`.
    fn within(self, other: Self) -> bool {
        self.room <= other.room && self.delegate <= other.delegate
    }

    fn max(self, other: Self) -> Self {
        Self {
            room: self.room.max(other.room),
            delegate: self.delegate.max(other.delegate),
        }
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

    /// The generation to reload for. An artifact the network has not moved,
    /// or not answered for yet, is named by this build's version (0 if
    /// unwatched). Such a partial target is never newer than the full one in
    /// that artifact, which is what lets [`cap_decision`] treat it as the same
    /// target.
    pub(crate) fn target(&self, watched: &[Watched]) -> Target {
        let version = |artifact: Artifact| {
            self.newer[artifact.index()].unwrap_or_else(|| {
                watched
                    .iter()
                    .find(|w| w.artifact == artifact)
                    .map_or(0, Watched::build_version)
            })
        };
        Target {
            room: version(Artifact::RoomContract),
            delegate: version(Artifact::ChatDelegate),
        }
    }
}

/// The reload caps carried in [`RELOAD_PARAM`]:
/// `<target>-<window start ms>-<reloads in window>-<tries for target>`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CapState {
    target: Target,
    window_start_ms: u64,
    count: u32,
    tries: u32,
}

impl CapState {
    fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.splitn(4, '-');
        let target = Target::parse(parts.next()?)?;
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
            self.target.encode(),
            self.window_start_ms,
            self.count,
            self.tries
        )
    }

    /// The target to record and the tries already spent on it. A target no
    /// newer in any artifact than the one carried is the same target: a page
    /// that so far heard one pointer (`r2d1`) is still retrying `r2d2`. Only a
    /// strictly newer artifact starts the count again.
    fn target_and_tries(existing: Option<&Self>, target: Target) -> (Target, u32) {
        match existing {
            Some(c) if target.within(c.target) => (c.target, c.tries),
            Some(c) => (c.target.max(target), 0),
            None => (target, 0),
        }
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
pub(crate) fn cap_decision(existing: Option<&str>, target: Target, now_ms: u64) -> CapDecision {
    let existing = existing.and_then(CapState::parse);
    let (target, tries) = CapState::target_and_tries(existing.as_ref(), target);
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
            target,
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
pub(crate) fn manual_param(existing: Option<&str>, target: Target, now_ms: u64) -> String {
    let existing = existing.and_then(CapState::parse);
    let (target, tries) = CapState::target_and_tries(existing.as_ref(), target);
    let (window_start_ms, count) = CapState::live_window(existing.as_ref(), now_ms);
    CapState {
        target,
        window_start_ms,
        count,
        tries,
    }
    .encode()
}

/// The URL to reload to: this document's own path and query, with
/// [`RELOAD_PARAM`] replaced and the auth-reload marker dropped, and `hash`.
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

/// The fragment to reload with. The iframe's own `location.hash` is stale once
/// River has recorded a processed invitation (the shell updates the top-level
/// URL with `replaceState`, which never reaches the iframe; see
/// `PROCESSED_CACHE` in `receive_invitation_modal.rs`). Reloading with it
/// would replay an invitation the reader already handled, so the in-memory
/// list wins whenever it has anything.
pub(crate) fn reload_fragment(location_hash: &str, processed_hash: &str) -> String {
    if processed_hash.is_empty() {
        location_hash.to_string()
    } else {
        processed_hash.to_string()
    }
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
    /// The reader said "Not now". It sticks for the page: no countdown while
    /// visible. Once the tab has been hidden long enough (`hidden_due_ms`) it
    /// reloads; showing the tab again cancels that wait, not the snooze.
    Snoozed {
        not_before_ms: u64,
        hidden_due_ms: Option<u64>,
    },
    /// A cap refused an automatic reload. Banner with a manual Reload only.
    ManualOnly,
    /// A reload is under way: pending writes are being flushed, then the
    /// reload is posted at `posted_ms`. Nothing else is ever issued from here.
    /// A posted reload that has not replaced the page after
    /// [`RELOAD_STUCK_MS`] did not happen.
    Reloading { posted_ms: Option<u64> },
    /// A posted reload never replaced the page (an old shell without
    /// `navigate`, a cancelled frame load). Asking again would fail the same
    /// way, so the banner asks the reader to refresh the tab.
    ReloadFailed,
}

impl Phase {
    /// When the next automatic reload could happen, if one is planned.
    pub(crate) fn due_ms(self) -> Option<u64> {
        match self {
            Phase::Hidden { due_ms, .. } | Phase::Countdown { due_ms, .. } => Some(due_ms),
            Phase::Snoozed { hidden_due_ms, .. } => hidden_due_ms,
            _ => None,
        }
    }
}

/// What the page looks like at one tick.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Inputs {
    pub now_ms: u64,
    /// Time since this document started loading.
    pub page_age_ms: u64,
    pub visible: bool,
    /// The reader is in the middle of something a reload would lose: unsent
    /// text, an edit, an open dialog, an invitation being accepted, or was
    /// within the last few seconds.
    pub busy: bool,
    /// Uniform in `0..=HIDDEN_DELAY_JITTER_MS`.
    pub jitter_ms: u64,
}

/// The earliest a reload may happen: past the cap's delay, and past
/// [`MIN_PAGE_AGE_MS`] of page age.
fn earliest(i: Inputs, min_delay_ms: u64) -> u64 {
    i.now_ms + min_delay_ms.max(MIN_PAGE_AGE_MS.saturating_sub(i.page_age_ms))
}

fn countdown(i: Inputs, not_before_ms: u64) -> Phase {
    Phase::Countdown {
        due_ms: (i.now_ms + COUNTDOWN_MS).max(not_before_ms),
        not_before_ms,
    }
}

fn hidden_due(i: Inputs, not_before_ms: u64) -> u64 {
    (i.now_ms + HIDDEN_DELAY_MIN_MS + i.jitter_ms).max(not_before_ms)
}

fn hidden_wait(i: Inputs, not_before_ms: u64) -> Phase {
    Phase::Hidden {
        due_ms: hidden_due(i, not_before_ms),
        not_before_ms,
    }
}

/// The phase a newer generation starts, once the caps allow a reload no
/// sooner than `min_delay_ms` from now.
pub(crate) fn start(i: Inputs, min_delay_ms: u64) -> Phase {
    let not_before_ms = earliest(i, min_delay_ms);
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
/// partial target, the second the real one, which may be a retry owed a
/// backoff. So a running plan takes the later of its own wait and the new
/// delay, and never gets shorter.
pub(crate) fn replan(phase: Phase, decision: &CapDecision, i: Inputs) -> Phase {
    if let Phase::Reloading { .. } | Phase::ReloadFailed = phase {
        return phase;
    }
    let CapDecision::Allow { min_delay_ms, .. } = decision else {
        return Phase::ManualOnly;
    };
    let floor = earliest(i, *min_delay_ms);
    match phase {
        // ManualOnly too: a newer target is a new decision, and this one allows.
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
        Phase::Snoozed {
            not_before_ms,
            hidden_due_ms,
        } => {
            let not_before_ms = not_before_ms.max(floor);
            Phase::Snoozed {
                not_before_ms,
                hidden_due_ms: hidden_due_ms.map(|d| d.max(not_before_ms)),
            }
        }
        Phase::Reloading { .. } | Phase::ReloadFailed => phase,
    }
}

/// The reload fell due and the caps were asked: the phase to move to, and the
/// [`RELOAD_PARAM`] value to reload with if allowed. From here the page only
/// waits for the reload; the stuck clock starts when it is posted.
pub(crate) fn on_reload_due(decision: CapDecision) -> (Phase, Option<String>) {
    match decision {
        CapDecision::Allow { param, .. } => (Phase::Reloading { posted_ms: None }, Some(param)),
        CapDecision::TriesExhausted | CapDecision::RateLimited => (Phase::ManualOnly, None),
    }
}

/// Advance the plan one tick. Returns the next phase and whether to reload now.
///
/// Never reloads while `busy`: the reload waits for the reader.
pub(crate) fn step(phase: Phase, i: Inputs) -> (Phase, bool) {
    match phase {
        Phase::Idle | Phase::ManualOnly | Phase::ReloadFailed => (phase, false),
        Phase::Reloading {
            posted_ms: Some(posted),
        } if i.now_ms.saturating_sub(posted) >= RELOAD_STUCK_MS => (Phase::ReloadFailed, false),
        Phase::Reloading { .. } => (phase, false),
        Phase::Snoozed { not_before_ms, .. } if i.visible => (
            Phase::Snoozed {
                not_before_ms,
                hidden_due_ms: None,
            },
            false,
        ),
        Phase::Snoozed {
            not_before_ms,
            hidden_due_ms: None,
        } => (
            Phase::Snoozed {
                not_before_ms,
                hidden_due_ms: Some(hidden_due(i, not_before_ms)),
            },
            false,
        ),
        Phase::Snoozed {
            hidden_due_ms: Some(due_ms),
            ..
        } => (phase, i.now_ms >= due_ms && !i.busy),
        Phase::Hidden { not_before_ms, .. } if i.visible => (countdown(i, not_before_ms), false),
        Phase::Countdown { not_before_ms, .. } if !i.visible => {
            (hidden_wait(i, not_before_ms), false)
        }
        Phase::Hidden { due_ms, .. } | Phase::Countdown { due_ms, .. } => {
            (phase, i.now_ms >= due_ms && !i.busy)
        }
    }
}

/// What the banner shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// "Reloading in N s", with Reload now / Not now.
    Countdown { secs: u64 },
    /// The countdown ran out while the reader is busy (or the connection is
    /// down).
    WaitingForReader,
    /// No automatic reload is coming; Reload only.
    Manual,
    /// A reload is under way.
    Reloading,
    /// A reload did not happen; only refreshing the tab will.
    RefreshTab,
}

/// The banner for a phase. A hidden tab needs none; it gets one on the tick
/// that sees it visible again.
pub(crate) fn notice(phase: Phase, i: Inputs) -> Option<Notice> {
    match phase {
        Phase::Idle | Phase::Hidden { .. } => None,
        Phase::Reloading { .. } => Some(Notice::Reloading),
        Phase::ReloadFailed => Some(Notice::RefreshTab),
        Phase::Countdown { due_ms, .. } if i.now_ms >= due_ms && i.busy => {
            Some(Notice::WaitingForReader)
        }
        Phase::Countdown { due_ms, .. } => Some(Notice::Countdown {
            secs: due_ms.saturating_sub(i.now_ms).div_ceil(1000),
        }),
        Phase::Snoozed { .. } | Phase::ManualOnly => Some(Notice::Manual),
    }
}

/// Everything this module remembers for the page load.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WatchState {
    pub observed: Observed,
    pub phase: Phase,
}

impl Default for WatchState {
    fn default() -> Self {
        Self {
            observed: Observed::default(),
            phase: Phase::Idle,
        }
    }
}

static STATE: LazyLock<Mutex<WatchState>> = LazyLock::new(Default::default);

/// Handle one served pointer state: judge it, and re-plan if the target moved.
/// Returns `None` if `id` is not a watched pointer.
///
/// Pure over its inputs so the whole path from response to plan is testable;
/// [`deliver_state`] supplies the live values.
pub(crate) fn on_pointer_state(
    watched: &[Watched],
    state: &mut WatchState,
    id: &ContractInstanceId,
    bytes: &[u8],
    reload_param: Option<&str>,
    i: Inputs,
) -> Option<Verdict> {
    let w = watched.iter().find(|w| &w.pointer_id == id)?;
    let verdict = evaluate(w, bytes);
    let app_id = w.artifact.app_id();
    match verdict {
        Verdict::Current => info!(
            "generation watch: {app_id} is current (v{})",
            w.build_version()
        ),
        Verdict::Newer(v) => warn!(
            "generation watch: {app_id} moved to v{v}; this build is v{}",
            w.build_version()
        ),
        Verdict::NoSignal => info!("generation watch: {app_id}: no usable record in response"),
    }
    if state.observed.record(w.artifact, verdict) {
        let target = state.observed.target(watched);
        let decision = cap_decision(reload_param, target, i.now_ms);
        match &decision {
            CapDecision::Allow { min_delay_ms, .. } => info!(
                "generation watch: reload planned for {} (no sooner than {min_delay_ms} ms)",
                target.encode()
            ),
            refused => warn!(
                "generation watch: automatic reload for {} refused ({refused:?}); banner only",
                target.encode()
            ),
        }
        state.phase = replan(state.phase, &decision, i);
    }
    Some(verdict)
}

/// The record bytes an update notification carries. The pointer contract's
/// delta is the whole 100-byte record, so a `Delta` is judged like a `State`.
pub(crate) fn update_bytes<'a>(update: &'a UpdateData<'_>) -> Option<&'a [u8]> {
    match update {
        UpdateData::State(state) => Some(state.as_ref()),
        UpdateData::Delta(delta) => Some(delta.as_ref()),
        UpdateData::StateAndDelta { state, .. } => Some(state.as_ref()),
        _ => None,
    }
}

/// Route a GET response. Returns `true` if `id` is a watched pointer, in
/// which case the caller must not treat it as a room.
pub fn deliver_state(id: &ContractInstanceId, bytes: &[u8]) -> bool {
    if !is_watched(id) {
        return false;
    }
    let i = runtime::current_inputs();
    let param = runtime::current_reload_param();
    let planned = match STATE.lock() {
        Ok(mut state) => {
            on_pointer_state(&WATCHED, &mut state, id, bytes, param.as_deref(), i);
            state.phase != Phase::Idle
        }
        Err(_) => false,
    };
    if planned {
        runtime::ensure_driver();
    }
    true
}

/// Route an update notification. Returns `true` if `id` is a watched pointer.
pub fn deliver_update(id: &ContractInstanceId, update: &UpdateData) -> bool {
    if !is_watched(id) {
        return false;
    }
    if let Some(bytes) = update_bytes(update) {
        deliver_state(id, bytes);
    }
    true
}

/// Whether `id` is one of the watched pointers.
pub fn is_watched(id: &ContractInstanceId) -> bool {
    WATCHED.iter().any(|w| &w.pointer_id == id)
}

pub use runtime::{
    is_reloading, note_user_activity, on_connected, on_page_visible, reload_now, snooze,
    UPDATE_NOTICE,
};

#[cfg(target_arch = "wasm32")]
mod runtime {
    use super::super::freenet_synchronizer::{SynchronizerMessage, SynchronizerStatus};
    use super::*;
    use crate::components::app::{NEEDS_SYNC, PENDING_INVITES, SYNC_STATUS, WEB_API};
    use crate::invites::PendingRoomStatus;
    use dioxus::prelude::*;
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;
    use wasm_bindgen::{JsCast, JsValue};

    /// What the update banner shows, if anything.
    pub static UPDATE_NOTICE: GlobalSignal<Option<Notice>> = Signal::global(|| None);

    static DRIVER_RUNNING: AtomicBool = AtomicBool::new(false);
    static BACKSTOP_RUNNING: AtomicBool = AtomicBool::new(false);
    static RELOADING: AtomicBool = AtomicBool::new(false);
    static LAST_CHECK_MS: AtomicU64 = AtomicU64::new(0);
    static LAST_BUSY_MS: AtomicU64 = AtomicU64::new(0);

    /// Backstop re-check interval. The subscription is the primary signal;
    /// this catches a notification dropped on a full channel.
    const BACKSTOP_INTERVAL_MS: f64 = 15.0 * 60.0 * 1000.0;
    /// Least time between visibility-triggered re-checks.
    const VISIBLE_RECHECK_MIN_MS: u64 = 2 * 60 * 1000;
    /// The reader counts as busy for this long after the last busy sign, so a
    /// message just sent has time to leave before the page goes.
    const SETTLE_MS: u64 = 5_000;
    /// Longest a reload waits for pending writes before it goes anyway.
    const FLUSH_TIMEOUT_MS: u64 = 5_000;

    fn now_ms() -> u64 {
        js_sys::Date::now() as u64
    }

    fn jitter_ms() -> u64 {
        (js_sys::Math::random() * HIDDEN_DELAY_JITTER_MS as f64) as u64
    }

    fn page_age_ms() -> u64 {
        web_sys::window()
            .and_then(|w| w.performance())
            .map_or(u64::MAX, |p| p.now() as u64)
    }

    fn location() -> Option<web_sys::Location> {
        web_sys::window().map(|w| w.location())
    }

    pub(super) fn current_reload_param() -> Option<String> {
        let search = location()?.search().ok()?;
        reload_param_of(&search).map(str::to_string)
    }

    /// Whether a reload is under way. The synchronizer does not reconnect
    /// then: a socket opened by the departing page would outlive it in the
    /// shell.
    pub fn is_reloading() -> bool {
        RELOADING.load(Ordering::SeqCst)
    }

    /// Whether the reader is in the middle of something a reload would lose.
    ///
    /// Deliberately broad. Every modal in this app renders a full-screen
    /// `fixed inset-0` overlay only while it is open, so any visible overlay
    /// means a dialog, picker or menu is up (with whatever is typed in it).
    /// Beyond that: unsent text in the room composer, a message being edited,
    /// any focused text field with something in it, and an invitation being
    /// accepted.
    fn user_is_busy() -> bool {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return false;
        };
        let value_of = |el: &web_sys::Element| {
            el.dyn_ref::<web_sys::HtmlTextAreaElement>()
                .map(|t| t.value())
                .or_else(|| el.dyn_ref::<web_sys::HtmlInputElement>().map(|i| i.value()))
        };
        let has_text = |el: &web_sys::Element| value_of(el).is_some_and(|v| !v.trim().is_empty());

        if let Ok(Some(el)) = document.query_selector("#message-input") {
            if has_text(&el) {
                return true;
            }
        }
        if matches!(document.query_selector("[id^=\"edit-msg-\"]"), Ok(Some(_))) {
            return true;
        }
        if let Ok(overlays) = document.query_selector_all(".fixed.inset-0") {
            for i in 0..overlays.length() {
                let visible = overlays
                    .item(i)
                    .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
                    // `display: none` gives zero size; a shown overlay
                    // covers the viewport.
                    .is_some_and(|el| el.client_width() > 0 || el.client_height() > 0);
                if visible {
                    return true;
                }
            }
        }
        if let Some(active) = document.active_element() {
            if has_text(&active) {
                return true;
            }
        }
        // While disconnected, what the reader just did may not have left yet,
        // and a reload would not make anything arrive either. Wait. (Only the
        // two down states: `Error(..)` is also set on a live socket, e.g. for
        // an unsupported operation, and must not hold a reload forever.)
        let down = SYNC_STATUS
            .try_read()
            .map(|s| {
                matches!(
                    *s,
                    SynchronizerStatus::Disconnected | SynchronizerStatus::Connecting
                )
            })
            .unwrap_or(true);
        if down {
            return true;
        }
        // An invitation being accepted. Only the in-flight states: a finished
        // or failed entry lingers until its dialog is closed, and an open
        // dialog is already caught above. Unreadable means "cannot tell":
        // treat as busy, it only delays.
        PENDING_INVITES
            .try_read()
            .map(|p| {
                p.map.values().any(|join| {
                    matches!(
                        join.status,
                        PendingRoomStatus::PendingSubscription | PendingRoomStatus::Subscribing
                    )
                })
            })
            .unwrap_or(true)
    }

    /// The reader just did something that queues a sync (sent a message,
    /// reacted, edited). Busy is otherwise sampled once a tick, which a quick
    /// "ok⏎" can fall between; this starts the settle period at once.
    pub fn note_user_activity() {
        LAST_BUSY_MS.store(now_ms(), Ordering::SeqCst);
    }

    pub(super) fn current_inputs() -> Inputs {
        let now = now_ms();
        if user_is_busy() {
            LAST_BUSY_MS.store(now, Ordering::SeqCst);
        }
        let last_busy = LAST_BUSY_MS.load(Ordering::SeqCst);
        Inputs {
            now_ms: now,
            page_age_ms: page_age_ms(),
            visible: crate::components::app::document_title::get_visibility_state(),
            busy: last_busy != 0 && now.saturating_sub(last_busy) < SETTLE_MS,
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
        if let Ok(mut s) = STATE.lock() {
            s.phase = phase;
        }
    }

    fn current_target() -> Option<Target> {
        let state = *STATE.lock().ok()?;
        state
            .observed
            .is_stale()
            .then(|| state.observed.target(&WATCHED))
    }

    fn is_framed() -> bool {
        let Some(window) = web_sys::window() else {
            return false;
        };
        match window.parent() {
            Ok(Some(parent)) => !js_sys::Object::is(parent.as_ref(), window.as_ref()),
            _ => false,
        }
    }

    /// Hand the reload to the shell (framed) or do it (top level). Returns
    /// whether it was issued.
    fn navigate(param: &str) -> bool {
        let Some(location) = location() else {
            return false;
        };
        let hash = reload_fragment(
            &location.hash().unwrap_or_default(),
            &crate::components::room_list::receive_invitation_modal::processed_hash_for_reload(),
        );
        let href = reload_href(
            &location.pathname().unwrap_or_default(),
            &location.search().unwrap_or_default(),
            &hash,
            param,
        );
        info!("generation watch: reloading to {href}");
        if is_framed() {
            let msg = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&msg, &"__freenet_shell__".into(), &JsValue::TRUE);
            let _ = js_sys::Reflect::set(&msg, &"type".into(), &"navigate".into());
            let _ = js_sys::Reflect::set(&msg, &"href".into(), &href.as_str().into());
            let posted = web_sys::window()
                .and_then(|w| w.parent().ok().flatten())
                .map(|parent| parent.post_message(&msg, "*").is_ok())
                .unwrap_or(false);
            if !posted {
                warn!("generation watch: could not reach the shell to reload");
            }
            posted
        } else {
            match location.replace(&href) {
                Ok(()) => true,
                Err(e) => {
                    warn!("generation watch: reload failed: {e:?}");
                    false
                }
            }
        }
    }

    /// A reload did not happen. Let the synchronizer reconnect again, and ask
    /// it to: a Connect it skipped while the reload was under way is gone, and
    /// a connection lost meanwhile would otherwise stay lost. (Connect is a
    /// no-op on a live connection.)
    fn abandon_reload() {
        if !RELOADING.swap(false, Ordering::SeqCst) {
            return;
        }
        warn!("generation watch: reload did not happen; reconnecting");
        if let Ok(sync) = crate::components::app::SYNCHRONIZER.try_read() {
            let _ = sync
                .get_message_sender()
                .unbounded_send(SynchronizerMessage::Connect);
        }
    }

    fn still_reloading() -> bool {
        matches!(
            STATE.lock().map(|s| s.phase),
            Ok(Phase::Reloading { posted_ms: None })
        )
    }

    /// Let pending writes out, then reload. Waits at most
    /// [`FLUSH_TIMEOUT_MS`] for room syncs to drain and the delegate to
    /// acknowledge the flush; a reload that never happens strands the tab,
    /// which is worse than a snapshot a few minutes stale.
    async fn reload_after_flush(param: String) {
        RELOADING.store(true, Ordering::SeqCst);
        let deadline = now_ms() + FLUSH_TIMEOUT_MS;
        while now_ms() < deadline && NEEDS_SYNC.try_read().map_or(true, |s| !s.is_empty()) {
            crate::util::sleep(Duration::from_millis(250)).await;
        }
        let remaining = deadline.saturating_sub(now_ms()).max(500);
        let flush = Box::pin(crate::components::app::chat_delegate::flush_rooms_to_delegate());
        let timeout = Box::pin(crate::util::sleep(Duration::from_millis(remaining)));
        if let futures::future::Either::Right(_) = futures::future::select(flush, timeout).await {
            warn!("generation watch: delegate flush did not finish in time; reloading anyway");
        }
        // The plan may have moved on while we waited; never post a stray one.
        if !still_reloading() {
            abandon_reload();
            return;
        }
        if navigate(&param) {
            // The stuck clock starts now that the reload is posted.
            set_phase(Phase::Reloading {
                posted_ms: Some(now_ms()),
            });
        } else {
            set_phase(Phase::ReloadFailed);
            set_notice(Some(Notice::RefreshTab));
            abandon_reload();
        }
    }

    pub(super) fn ensure_driver() {
        if !DRIVER_RUNNING.swap(true, Ordering::SeqCst) {
            crate::util::safe_spawn_local(drive());
        }
    }

    async fn drive() {
        loop {
            let i = current_inputs();
            let Ok((next, reload)) = STATE.lock().map(|mut s| {
                let (next, reload) = step(s.phase, i);
                s.phase = next;
                (next, reload)
            }) else {
                break;
            };
            if next == Phase::ReloadFailed {
                abandon_reload();
            }
            set_notice(notice(next, i));
            if reload {
                let decision = current_target()
                    .map(|t| cap_decision(current_reload_param().as_deref(), t, i.now_ms))
                    .unwrap_or(CapDecision::RateLimited);
                if !matches!(decision, CapDecision::Allow { .. }) {
                    warn!("generation watch: automatic reload refused ({decision:?}); banner only");
                }
                // One reload per page: from here the page only waits.
                let (phase, param) = on_reload_due(decision);
                set_phase(phase);
                set_notice(notice(phase, i));
                if let Some(param) = param {
                    crate::util::safe_spawn_local(reload_after_flush(param));
                }
            }
            let phase = STATE.lock().map(|s| s.phase).unwrap_or(Phase::Idle);
            if matches!(phase, Phase::Idle | Phase::ManualOnly | Phase::ReloadFailed) {
                break;
            }
            // Tick fast only when something is about to happen.
            let soon = phase
                .due_ms()
                .is_none_or(|due| due.saturating_sub(i.now_ms) <= 60_000);
            let tick = if soon { 1 } else { 5 };
            crate::util::sleep(Duration::from_secs(tick)).await;
        }
        DRIVER_RUNNING.store(false, Ordering::SeqCst);
    }

    /// The banner's Reload button.
    pub fn reload_now() {
        // A click racing the automatic reload, or a double click: one reload.
        let Ok(mut state) = STATE.lock() else {
            return;
        };
        if is_reloading() || matches!(state.phase, Phase::Reloading { .. } | Phase::ReloadFailed) {
            return;
        }
        if !state.observed.is_stale() {
            return;
        }
        let target = state.observed.target(&WATCHED);
        state.phase = Phase::Reloading { posted_ms: None };
        drop(state);
        let param = manual_param(current_reload_param().as_deref(), target, now_ms());
        set_notice(Some(Notice::Reloading));
        ensure_driver();
        crate::util::safe_spawn_local(reload_after_flush(param));
    }

    /// The banner's "Not now" button.
    pub fn snooze() {
        let snoozed = STATE.lock().is_ok_and(|mut s| {
            if let Phase::Countdown { not_before_ms, .. } | Phase::Hidden { not_before_ms, .. } =
                s.phase
            {
                s.phase = Phase::Snoozed {
                    not_before_ms,
                    hidden_due_ms: None,
                };
                true
            } else {
                false
            }
        });
        if snoozed {
            set_notice(Some(Notice::Manual));
        }
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
    use super::{Inputs, Notice};
    use dioxus::prelude::*;

    pub static UPDATE_NOTICE: GlobalSignal<Option<Notice>> = Signal::global(|| None);

    pub(super) fn current_inputs() -> Inputs {
        Inputs {
            now_ms: 0,
            page_age_ms: 0,
            visible: true,
            busy: false,
            jitter_ms: 0,
        }
    }
    pub(super) fn current_reload_param() -> Option<String> {
        None
    }
    pub(super) fn ensure_driver() {}
    pub fn is_reloading() -> bool {
        false
    }
    pub fn note_user_activity() {}
    pub fn on_connected() {}
    pub fn on_page_visible() {}
    pub fn reload_now() {}
    pub fn snooze() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page old enough that [`MIN_PAGE_AGE_MS`] does not bind.
    const OLD: u64 = 10 * MIN_PAGE_AGE_MS;

    fn visible(now_ms: u64) -> Inputs {
        Inputs {
            now_ms,
            page_age_ms: OLD,
            visible: true,
            busy: false,
            jitter_ms: 0,
        }
    }

    fn hidden(now_ms: u64) -> Inputs {
        Inputs {
            visible: false,
            ..visible(now_ms)
        }
    }

    fn t(room: u32, delegate: u32) -> Target {
        Target { room, delegate }
    }

    // ---- build identity --------------------------------------------------

    /// The shipped build must be able to watch both pointers. If this fails,
    /// `pointer-records.toml` no longer names the WASM the UI embeds, and the
    /// UI would never notice a re-key. (Fails by design when the tests are
    /// built with `RIVER_POINTER_RECORDS` pointing at test records.)
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

    fn test_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
    }

    /// Both pointers watched at build version 1, signed by a test key.
    fn test_watched() -> Vec<Watched> {
        let author = format!(
            "{AUTHOR_VK_PREFIX}{}",
            bs58::encode(test_key().verifying_key().as_bytes()).into_string()
        );
        let room: &'static [u8] = b"room contract v1";
        let delegate: &'static [u8] = b"chat delegate v1";
        let records = [
            BuildPointerRecord {
                app_id: "river.room-contract",
                version: 1,
                code_hash: *blake3::hash(room).as_bytes(),
            },
            BuildPointerRecord {
                app_id: "river.chat-delegate",
                version: 1,
                code_hash: *blake3::hash(delegate).as_bytes(),
            },
        ];
        let watched = watched_pointers(&author, &records, |a| match a {
            Artifact::RoomContract => room,
            Artifact::ChatDelegate => delegate,
        });
        assert_eq!(watched.len(), 2);
        watched
    }

    fn room_state(version: u32) -> Vec<u8> {
        let code_hash = if version == 1 {
            *blake3::hash(b"room contract v1").as_bytes()
        } else {
            [version as u8; 32]
        };
        signed_state(&test_key(), "river.room-contract", version, code_hash)
    }

    fn delegate_state(version: u32) -> Vec<u8> {
        let code_hash = if version == 1 {
            *blake3::hash(b"chat delegate v1").as_bytes()
        } else {
            [0x40 + version as u8; 32]
        };
        signed_state(&test_key(), "river.chat-delegate", version, code_hash)
    }

    #[test]
    fn same_record_as_the_build_is_current() {
        let w = test_watched()[0];
        assert_eq!(evaluate(&w, &room_state(1)), Verdict::Current);
    }

    #[test]
    fn a_newer_signed_record_is_newer() {
        let w = test_watched()[0];
        assert_eq!(evaluate(&w, &room_state(5)), Verdict::Newer(5));
    }

    /// The pointer is published after the UI, so a fresh build sees it lag.
    /// That must never read as a reason to reload (it would reload into the
    /// same build forever, bounded only by the caps).
    #[test]
    fn a_pointer_behind_the_build_is_no_signal() {
        let sk = test_key();
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
        let w = watched_pointers(&author, &records, |_| wasm)[0];
        let behind = signed_state(&sk, "river.room-contract", 3, [0x33; 32]);
        assert_eq!(evaluate(&w, &behind), Verdict::NoSignal);
    }

    #[test]
    fn unsigned_or_foreign_records_are_no_signal() {
        let w = test_watched()[0];
        let stranger = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let forged = signed_state(&stranger, "river.room-contract", 9, [0x99; 32]);
        assert_eq!(evaluate(&w, &forged), Verdict::NoSignal);
        let other_app = signed_state(&test_key(), "river.chat-delegate", 9, [0x99; 32]);
        assert_eq!(evaluate(&w, &other_app), Verdict::NoSignal);
        assert_eq!(evaluate(&w, &[]), Verdict::NoSignal);
        assert_eq!(evaluate(&w, &[0u8; 7]), Verdict::NoSignal);
    }

    /// Only the author can sign two records at one version. Whichever side of
    /// the tiebreak the rival lands on, it is not a newer generation, and a
    /// reload could never satisfy it.
    #[test]
    fn a_competing_record_at_the_build_version_is_no_signal() {
        let w = test_watched()[0];
        // One rival below the build's hash (wins the tiebreak), one above.
        for rival_hash in [[0x01; 32], [0xFF; 32]] {
            let rival = signed_state(&test_key(), "river.room-contract", 1, rival_hash);
            assert_eq!(evaluate(&w, &rival), Verdict::NoSignal, "{rival_hash:?}");
        }
    }

    #[test]
    fn a_withdrawal_is_no_signal() {
        let w = test_watched()[0];
        let state = signed_state(&test_key(), "river.room-contract", 5, [0u8; 32]);
        assert_eq!(evaluate(&w, &state), Verdict::NoSignal);
    }

    // ---- targets ---------------------------------------------------------

    #[test]
    fn targets_round_trip_and_compare_per_artifact() {
        assert_eq!(Target::parse("r5d6"), Some(t(5, 6)));
        assert_eq!(t(5, 6).encode(), "r5d6");
        for junk in ["", "r", "5d6", "r5", "r5d", "rxd6", "r5d6x"] {
            assert_eq!(Target::parse(junk), None, "{junk:?}");
        }
        assert!(t(2, 1).within(t(2, 2)));
        assert!(t(2, 2).within(t(2, 2)));
        assert!(!t(3, 1).within(t(2, 2)));
        assert_eq!(t(3, 1).max(t(2, 2)), t(3, 2));
    }

    #[test]
    fn observed_tracks_the_newest_version_and_names_the_target() {
        let watched = test_watched();
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
        assert_eq!(o.target(&watched), t(5, 1), "delegate named by the build");
        assert!(o.record(Artifact::ChatDelegate, Verdict::Newer(6)));
        assert_eq!(o.target(&watched), t(5, 6));
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
        assert_eq!(
            cap_decision(None, t(5, 5), 1_000),
            allow("r5d5-1000-1-1", 0)
        );
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
            match cap_decision(param.as_deref(), t(5, 5), 1_000 + u64::from(i)) {
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
        let mut expected = vec![0];
        expected.extend(RETRY_DELAYS_MS);
        assert_eq!(delays, expected);
        assert_eq!(
            cap_decision(param.as_deref(), t(5, 5), 2_000),
            CapDecision::TriesExhausted
        );
        // Not even a fresh hour revives a target whose tries are spent.
        assert_eq!(
            cap_decision(
                param.as_deref(),
                t(5, 5),
                1_000 + 10 * AUTO_RELOAD_WINDOW_MS
            ),
            CapDecision::TriesExhausted
        );
    }

    /// Regression (review of #792): tries were keyed on the exact target
    /// string, so a page that so far heard only the room pointer (`r2d1`) had
    /// spent no tries even when it was the third reload for `r2d2`. If the
    /// delegate answer was late or lost, alternate loads reset the count and
    /// only the hourly ceiling held, renewed every hour.
    #[test]
    fn a_partial_target_cannot_reset_the_tries() {
        let mut param: Option<String> = None;
        let mut tries = 0;
        // Loads alternate between hearing both pointers and only the room one.
        for load in 0.. {
            let target = if load % 2 == 0 { t(2, 2) } else { t(2, 1) };
            match cap_decision(param.as_deref(), target, 1_000 + load) {
                CapDecision::Allow { param: p, .. } => {
                    assert!(p.starts_with("r2d2-"), "the full target is kept: {p}");
                    param = Some(p);
                    tries += 1;
                }
                CapDecision::TriesExhausted => break,
                other => panic!("load {load}: {other:?}"),
            }
            assert!(load < 50, "never exhausted");
        }
        assert_eq!(tries, MAX_TRIES_PER_TARGET);
    }

    #[test]
    fn a_newer_artifact_starts_its_own_tries() {
        let after_two = "r5d5-1000-2-2";
        assert_eq!(
            cap_decision(Some(after_two), t(5, 6), 2_000),
            allow("r5d6-1000-3-1", 0)
        );
        // Newer in one artifact, behind in the other: the record keeps the max.
        assert_eq!(
            cap_decision(Some(after_two), t(6, 1), 2_000),
            allow("r6d5-1000-3-1", 0)
        );
    }

    #[test]
    fn the_hourly_ceiling_holds_across_targets() {
        let mut param: Option<String> = None;
        for i in 0..MAX_AUTO_RELOADS_PER_WINDOW {
            match cap_decision(param.as_deref(), t(10 + i, 5), 1_000 + u64::from(i)) {
                CapDecision::Allow { param: p, .. } => param = Some(p),
                other => panic!("reload {i} refused: {other:?}"),
            }
        }
        assert_eq!(
            cap_decision(param.as_deref(), t(99, 5), 2_000),
            CapDecision::RateLimited
        );
        // A fresh window once the old one has passed.
        assert!(matches!(
            cap_decision(param.as_deref(), t(99, 5), 1_000 + AUTO_RELOAD_WINDOW_MS),
            CapDecision::Allow { .. }
        ));
    }

    #[test]
    fn a_future_or_malformed_window_starts_fresh() {
        assert_eq!(
            cap_decision(Some("r5d5-99999-6-1"), t(6, 5), 1_000),
            allow("r6d5-1000-1-1", 0)
        );
        for junk in ["garbage", "r5d5-1-1", "r5d5-x-1-1", "x5d5-1-1-1", ""] {
            assert_eq!(
                cap_decision(Some(junk), t(6, 5), 1_000),
                allow("r6d5-1000-1-1", 0),
                "{junk:?}"
            );
        }
    }

    #[test]
    fn a_manual_reload_spends_no_budget() {
        // Tries and the window count are carried, not incremented.
        assert_eq!(
            manual_param(Some("r6d5-1000-2-2"), t(6, 5), 2_000),
            "r6d5-1000-2-2"
        );
        // A newer target has no tries yet, so the page it loads may still
        // retry automatically from the start.
        let param = manual_param(Some("r5d5-1000-2-2"), t(6, 5), 2_000);
        assert_eq!(param, "r6d5-1000-2-0");
        assert_eq!(
            cap_decision(Some(&param), t(6, 5), 3_000),
            allow("r6d5-1000-3-1", 0)
        );
    }

    /// The tries outlast the measured p90 (~10 min) of web-container delivery
    /// with a wide margin; see `RETRY_DELAYS_MS`.
    #[test]
    fn retries_span_web_container_propagation() {
        let span: u64 = RETRY_DELAYS_MS.iter().sum();
        assert!(span >= 60 * 60 * 1000, "retries span only {span} ms");
        assert_eq!(RETRY_DELAYS_MS.len() as u32, MAX_TRIES_PER_TARGET - 1);
        assert!(
            RETRY_DELAYS_MS.windows(2).all(|w| w[0] <= w[1]),
            "backoff must not shrink"
        );
    }

    // ---- URL -------------------------------------------------------------

    #[test]
    fn reload_href_keeps_the_sandbox_query_and_fragment() {
        assert_eq!(
            reload_href(
                "/v1/contract/web/abc/",
                "?__sandbox=1&invitation=xyz",
                "#river-processed=a",
                "r5d5-1-1-1"
            ),
            "/v1/contract/web/abc/?__sandbox=1&invitation=xyz&_rgen=r5d5-1-1-1#river-processed=a"
        );
    }

    #[test]
    fn reload_href_replaces_its_own_markers() {
        assert_eq!(
            reload_href(
                "/p/",
                "?__sandbox=1&_rgen=r4d4-1-1-1&_reload=123",
                "",
                "r5d5-1-2-1"
            ),
            "/p/?__sandbox=1&_rgen=r5d5-1-2-1"
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

    /// Regression (review of #792): the iframe's own hash is stale after an
    /// invitation is processed, so reloading with it replays that invitation.
    #[test]
    fn the_reload_fragment_comes_from_the_processed_list() {
        assert_eq!(
            reload_fragment("#river-processed=old", "#river-processed=old,new"),
            "#river-processed=old,new"
        );
        assert_eq!(reload_fragment("#something", ""), "#something");
        assert_eq!(reload_fragment("", ""), "");
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
    fn a_hidden_tab_waits_then_reloads() {
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
        assert_eq!(notice(phase, i), None);
        assert_eq!(step(phase, hidden(due - 1)), (phase, false));
        assert_eq!(step(phase, hidden(due)), (phase, true));
    }

    /// Review of #792: within the node's 30 s bundle-refresh window, a reload
    /// is served the bundle this page came from. A fresh page must not spend a
    /// try there, visible or hidden.
    #[test]
    fn a_fresh_page_waits_out_the_node_refresh_window() {
        assert!(MIN_PAGE_AGE_MS > 30_000);
        for base in [visible(1_000), hidden(1_000)] {
            let i = Inputs {
                page_age_ms: 2_000,
                ..base
            };
            let due = start(i, 0).due_ms().unwrap();
            assert!(
                due >= 1_000 + MIN_PAGE_AGE_MS - 2_000,
                "reload at {due} for a page {} ms old",
                i.page_age_ms
            );
        }
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
            hidden_due_ms: None,
        };
        let (resumed, _) = step(snoozed, hidden(30));
        assert_eq!(step(resumed, hidden(delay - 1)), (resumed, false));
        assert_eq!(step(resumed, hidden(delay)), (resumed, true));
    }

    #[test]
    fn never_reloads_while_the_reader_is_busy() {
        let phase = Phase::Countdown {
            due_ms: 10,
            not_before_ms: 0,
        };
        let busy = Inputs {
            busy: true,
            ..visible(1_000_000)
        };
        assert_eq!(step(phase, busy), (phase, false));
        assert_eq!(notice(phase, busy), Some(Notice::WaitingForReader));
        assert_eq!(step(phase, visible(1_000_001)), (phase, true));

        let phase = Phase::Hidden {
            due_ms: 10,
            not_before_ms: 0,
        };
        let busy = Inputs {
            busy: true,
            ..hidden(1_000_000)
        };
        assert_eq!(step(phase, busy), (phase, false));

        let phase = Phase::Snoozed {
            not_before_ms: 0,
            hidden_due_ms: Some(10),
        };
        assert_eq!(step(phase, busy), (phase, false));
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

    /// Review of #792: "Not now" used to become a plain hidden wait on the
    /// first tab switch, and the countdown came back on the next.
    #[test]
    fn not_now_sticks_across_tab_switches() {
        let snoozed = Phase::Snoozed {
            not_before_ms: 0,
            hidden_due_ms: None,
        };
        assert_eq!(step(snoozed, visible(5)), (snoozed, false));
        assert_eq!(notice(snoozed, visible(5)), Some(Notice::Manual));

        let (away, reload) = step(snoozed, hidden(5));
        assert!(!reload);
        assert_eq!(
            away,
            Phase::Snoozed {
                not_before_ms: 0,
                hidden_due_ms: Some(5 + HIDDEN_DELAY_MIN_MS)
            }
        );
        // Back before the hidden wait ran out: still snoozed, no countdown.
        let (back, reload) = step(away, visible(10));
        assert!(!reload);
        assert_eq!(back, snoozed);
        assert_eq!(notice(back, visible(10)), Some(Notice::Manual));
        // Away long enough: it reloads.
        let (away, _) = step(back, hidden(20));
        assert_eq!(step(away, hidden(20 + HIDDEN_DELAY_MIN_MS)).1, true);
    }

    /// Review of #792: the driver re-issued the reload every tick until the
    /// page went, each one aborting the navigation before it. Once reloading,
    /// nothing else is ever issued; a reload that never lands falls back to
    /// the banner.
    #[test]
    fn a_page_reloads_at_most_once() {
        let posted = Phase::Reloading {
            posted_ms: Some(100),
        };
        for now in [100, 1_000, 100 + RELOAD_STUCK_MS - 1] {
            for i in [visible(now), hidden(now)] {
                assert_eq!(step(posted, i), (posted, false));
            }
        }
        assert_eq!(notice(posted, visible(200)), Some(Notice::Reloading));
        // A newer target arriving mid-reload does not restart anything.
        let allowed = cap_decision(None, t(9, 9), 200);
        assert_eq!(replan(posted, &allowed, visible(200)), posted);
    }

    /// Review of #792: the stuck clock must not run while pending writes are
    /// still being flushed (a throttled hidden tab can take far longer than
    /// [`RELOAD_STUCK_MS`] there), or the page gives up and then posts the
    /// reload anyway.
    #[test]
    fn the_stuck_clock_starts_when_the_reload_is_posted() {
        let flushing = Phase::Reloading { posted_ms: None };
        assert_eq!(step(flushing, hidden(u64::MAX / 2)), (flushing, false));
        let posted = Phase::Reloading {
            posted_ms: Some(1_000),
        };
        assert_eq!(
            step(posted, visible(1_000 + RELOAD_STUCK_MS)),
            (Phase::ReloadFailed, false)
        );
    }

    /// A reload that did not happen ends in a phase that never reloads, keeps
    /// out a newer plan (the same reload would fail the same way), and asks
    /// the reader to refresh the tab.
    #[test]
    fn a_failed_reload_asks_for_a_refresh_and_stays_put() {
        for i in [visible(5), hidden(u64::MAX / 2)] {
            assert_eq!(step(Phase::ReloadFailed, i), (Phase::ReloadFailed, false));
        }
        assert_eq!(
            notice(Phase::ReloadFailed, visible(0)),
            Some(Notice::RefreshTab)
        );
        let allowed = cap_decision(None, t(9, 9), 200);
        assert_eq!(
            replan(Phase::ReloadFailed, &allowed, visible(200)),
            Phase::ReloadFailed
        );
    }

    /// The driver's one decision when a reload falls due: reload once and wait
    /// (stuck clock not yet running), or fall back to the banner.
    #[test]
    fn a_due_reload_moves_to_waiting_or_to_the_banner() {
        let allowed = cap_decision(None, t(2, 2), 0);
        let CapDecision::Allow { param, .. } = allowed.clone() else {
            unreachable!()
        };
        assert_eq!(
            on_reload_due(allowed),
            (Phase::Reloading { posted_ms: None }, Some(param))
        );
        for refused in [CapDecision::TriesExhausted, CapDecision::RateLimited] {
            assert_eq!(on_reload_due(refused), (Phase::ManualOnly, None));
        }
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

    #[test]
    fn replan_never_shortens_a_wait() {
        let long = Phase::Hidden {
            due_ms: 900_000,
            not_before_ms: 600_000,
        };
        let no_delay = cap_decision(None, t(9, 9), 1_000);
        assert_eq!(replan(long, &no_delay, hidden(1_000)), long);
        let snoozed = Phase::Snoozed {
            not_before_ms: 600_000,
            hidden_due_ms: Some(700_000),
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
        let allowed = cap_decision(None, t(3, 3), 50);
        assert_eq!(
            replan(Phase::ManualOnly, &allowed, visible(50)),
            start(visible(50), 0)
        );
    }

    // ---- routing: response to plan -------------------------------------

    /// Regression, found end to end and pinned at the call site (review of
    /// #792): both pointers move, the reload lands on the old build, and the
    /// reloaded page hears the room pointer just before the delegate's. The
    /// partial target must not plan away the retry backoff the full one is
    /// owed.
    #[test]
    fn the_second_pointer_answer_keeps_the_retry_backoff() {
        let watched = test_watched();
        let (room_id, delegate_id) = (watched[0].pointer_id, watched[1].pointer_id);
        let landed_with = "r2d2-1000-1-1"; // one try already spent on r2d2
        let mut state = WatchState::default();

        let v = on_pointer_state(
            &watched,
            &mut state,
            &room_id,
            &room_state(2),
            Some(landed_with),
            visible(5_000),
        );
        assert_eq!(v, Some(Verdict::Newer(2)));
        let after_room = state.phase.due_ms().expect("a plan");
        assert!(
            after_room >= 5_000 + RETRY_DELAYS_MS[0],
            "the partial target r2d1 already owes r2d2's backoff: due {after_room}"
        );

        on_pointer_state(
            &watched,
            &mut state,
            &delegate_id,
            &delegate_state(2),
            Some(landed_with),
            visible(5_007),
        );
        let due = state.phase.due_ms().expect("a plan");
        assert!(due >= 5_007 + RETRY_DELAYS_MS[0], "backoff lost: due {due}");
        assert_eq!(step(state.phase, visible(5_000 + COUNTDOWN_MS)).1, false);
    }

    /// Every target move re-plans, not only the first. A page that has spent
    /// its tries on `r2d2` goes manual on the room answer; when the delegate
    /// answer then names a NEWER generation, that is a new decision and must
    /// plan a reload again.
    #[test]
    fn a_newer_generation_after_exhaustion_plans_again() {
        let watched = test_watched();
        let (room_id, delegate_id) = (watched[0].pointer_id, watched[1].pointer_id);
        let exhausted = format!("r2d2-1000-5-{MAX_TRIES_PER_TARGET}");
        let mut state = WatchState::default();

        on_pointer_state(
            &watched,
            &mut state,
            &room_id,
            &room_state(2),
            Some(&exhausted),
            visible(5_000),
        );
        assert_eq!(state.phase, Phase::ManualOnly);

        on_pointer_state(
            &watched,
            &mut state,
            &delegate_id,
            &delegate_state(3),
            Some(&exhausted),
            visible(5_010),
        );
        assert!(
            matches!(state.phase, Phase::Countdown { .. }),
            "a newer delegate generation must plan again, got {:?}",
            state.phase
        );
    }

    #[test]
    fn routing_ignores_what_it_does_not_watch_and_what_says_nothing() {
        let watched = test_watched();
        let mut state = WatchState::default();
        let stranger = ContractInstanceId::new([3u8; 32]);
        assert_eq!(
            on_pointer_state(
                &watched,
                &mut state,
                &stranger,
                &room_state(9),
                None,
                visible(0)
            ),
            None
        );
        let room_id = watched[0].pointer_id;
        for bytes in [room_state(1), Vec::new(), vec![1, 2, 3]] {
            on_pointer_state(&watched, &mut state, &room_id, &bytes, None, visible(0));
        }
        assert_eq!(state.phase, Phase::Idle);
        assert!(!state.observed.is_stale());

        let forged = signed_state(
            &ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]),
            "river.room-contract",
            9,
            [9; 32],
        );
        assert_eq!(
            on_pointer_state(&watched, &mut state, &room_id, &forged, None, visible(0)),
            Some(Verdict::NoSignal)
        );
        assert_eq!(state.phase, Phase::Idle);
    }

    /// The hooks' return values are what keeps a pointer response out of the
    /// room logic (and a room response in it). Pin them against the real
    /// watched pointers of this build.
    #[test]
    fn deliver_claims_exactly_the_watched_pointers() {
        use freenet_stdlib::prelude::{State, StateDelta};
        let stranger = ContractInstanceId::new([5u8; 32]);
        assert!(!deliver_state(&stranger, &[1, 2, 3]));
        assert!(!deliver_update(
            &stranger,
            &UpdateData::State(State::from(vec![1]))
        ));
        assert!(!is_watched(&stranger));

        let pointer = WATCHED.first().expect("shipped build watches").pointer_id;
        assert!(is_watched(&pointer));
        // Claimed even when there is nothing usable in it.
        assert!(deliver_state(&pointer, &[]));
        assert!(deliver_update(
            &pointer,
            &UpdateData::Delta(StateDelta::from(vec![0u8; 7]))
        ));
        assert!(deliver_update(
            &pointer,
            &UpdateData::RelatedState {
                related_to: ContractInstanceId::new([1; 32]),
                state: State::from(Vec::new()),
            }
        ));
        // ...and garbage plans nothing.
        assert!(!STATE.lock().unwrap().observed.is_stale());
    }

    #[test]
    fn every_carrying_update_variant_yields_the_record() {
        use freenet_stdlib::prelude::{State, StateDelta};
        let bytes = room_state(3);
        let state = UpdateData::State(State::from(bytes.clone()));
        let delta = UpdateData::Delta(StateDelta::from(bytes.clone()));
        let both = UpdateData::StateAndDelta {
            state: State::from(bytes.clone()),
            delta: StateDelta::from(Vec::new()),
        };
        for update in [&state, &delta, &both] {
            assert_eq!(update_bytes(update), Some(bytes.as_slice()));
        }
        let related = UpdateData::RelatedState {
            related_to: ContractInstanceId::new([1; 32]),
            state: State::from(bytes.clone()),
        };
        assert_eq!(update_bytes(&related), None);
    }

    // ---- wiring ----------------------------------------------------------

    /// Whether the block opened right after `needle` (the hook's own
    /// `if ... { }`) leaves the handler, i.e. contains a `return` or
    /// `continue`. Scoped to that block by brace matching, so a `return`
    /// belonging to the NEXT statement cannot satisfy it.
    fn hook_block_exits(src: &str, needle: &str) -> bool {
        hook_block(src, needle)
            .is_some_and(|block| block.contains("return") || block.contains("continue"))
    }

    /// The text of the block opened right after `needle`, brace-matched.
    fn hook_block<'a>(src: &'a str, needle: &str) -> Option<&'a str> {
        let at = src.find(needle)?;
        let open = at + src[at..].find('{')?;
        let mut depth = 0usize;
        for (i, c) in src[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&src[open..open + i]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Each hook must run BEFORE the room lookup and RETURN when it claims the
    /// id, or a pointer response is treated as an unknown room.
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
        assert!(hook_block_exits(&get, "generation_watch::deliver_state("));
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
        assert!(hook_block_exits(&get, "generation_watch::is_watched("));

        let upd = src(include_str!("response_handler/update_notification.rs"));
        let upd_fn = upd.find("pub fn handle_update_notification").unwrap();
        let body = &upd[upd_fn..];
        let upd_hook = body
            .find("generation_watch::deliver_update(")
            .expect("update hook");
        let upd_lookup = body.find("get_owner_vk_for_instance_id").unwrap();
        assert!(upd_hook < upd_lookup);
        assert!(hook_block_exits(body, "generation_watch::deliver_update("));

        let sub = src(include_str!("response_handler/subscribe_response.rs"));
        let sub_hook = sub
            .find("generation_watch::is_watched(")
            .expect("subscribe hook");
        let sub_lookup = sub.find("get_owner_vk_for_instance_id").unwrap();
        assert!(sub_hook < sub_lookup);
        assert!(hook_block_exits(&sub, "generation_watch::is_watched("));

        let dispatch = src(include_str!("response_handler.rs"));
        assert!(
            dispatch.contains("subscribed && !is_pointer"),
            "a pointer subscribe must not trigger the room re-GET"
        );

        let sync = src(include_str!("freenet_synchronizer.rs"));
        assert!(sync.contains("generation_watch::on_connected()"));
        assert!(sync.contains("generation_watch::on_page_visible()"));
        let connect = sync
            .find("SynchronizerMessage::Connect =>")
            .expect("Connect arm");
        assert!(
            hook_block_exits(&sync[connect..], "generation_watch::is_reloading()"),
            "Connect must not reconnect while a reload is under way"
        );
        assert!(
            hook_block(&sync[connect..], "generation_watch::is_reloading()")
                .is_some_and(|b| b.contains("note_connect_attempt()")),
            "a Connect skipped during a reload must still clear the reconnect latch, \
             or a reload that never happens leaves the tab disconnected"
        );
    }

    /// `user_is_busy` finds the composers by these hooks. Renaming one would
    /// fail OPEN (reload over unsent text) without anything else noticing.
    #[test]
    fn busy_detection_selectors_still_exist() {
        let src = |s: &str| crate::util::strip_comments(s);
        assert!(src(include_str!("../../conversation/message_input.rs"))
            .contains("id: \"message-input\""));
        assert!(src(include_str!("../../conversation.rs")).contains("\"edit-msg-{"));
        for (file, body) in [
            (
                "invite_via_dm_picker_modal.rs",
                include_str!("../../direct_messages/invite_via_dm_picker_modal.rs"),
            ),
            (
                "dm_thread_modal.rs",
                include_str!("../../direct_messages/dm_thread_modal.rs"),
            ),
            (
                "edit_room_modal.rs",
                include_str!("../../room_list/edit_room_modal.rs"),
            ),
            (
                "create_room_modal.rs",
                include_str!("../../room_list/create_room_modal.rs"),
            ),
            (
                "join_with_code_modal.rs",
                include_str!("../../room_list/join_with_code_modal.rs"),
            ),
            (
                "invite_member_modal.rs",
                include_str!("../../members/invite_member_modal.rs"),
            ),
            (
                "member_info_modal.rs",
                include_str!("../../members/member_info_modal.rs"),
            ),
            (
                "ban_button.rs",
                include_str!("../../members/member_info_modal/ban_button.rs"),
            ),
            ("members.rs", include_str!("../../members.rs")),
            (
                "notification_modal.rs",
                include_str!("../../room_list/notification_modal.rs"),
            ),
            (
                "receive_invitation_modal.rs",
                include_str!("../../room_list/receive_invitation_modal.rs"),
            ),
            (
                "message_input.rs",
                include_str!("../../conversation/message_input.rs"),
            ),
            ("conversation.rs", include_str!("../../conversation.rs")),
        ] {
            assert!(
                src(body).contains("fixed inset-0"),
                "{file} no longer renders a `fixed inset-0` overlay, so an open \
                 dialog there would not count as busy"
            );
        }
    }

    /// A quick send between driver ticks is only seen through this call.
    #[test]
    fn user_actions_start_the_settle_period() {
        let app = crate::util::strip_comments(include_str!("../../app.rs"));
        let at = app.find("pub fn mark_needs_sync").expect("mark_needs_sync");
        let body = &app[at..at + app[at..].find("\n}").expect("fn end")];
        assert!(
            body.contains("generation_watch::note_user_activity()"),
            "mark_needs_sync must note user activity for the reload settle period"
        );
    }
}
