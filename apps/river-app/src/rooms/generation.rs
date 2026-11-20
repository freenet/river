//! Noticing that River has moved to a newer chat delegate or room contract than the one this
//! build carries, and stopping every write when it has.
//!
//! The app carries River's chat delegate and room contract WASM, and their hashes are the keys
//! it reads and writes. When River re-keys either one, the web UI moves its users' rooms to the
//! new keys (its migration walk), and keeps re-reading the old delegate only for a top-up
//! window (14 days or 30 loads, `ui/.../migration_journal.rs`). An older desktop build would go
//! on registering and writing its own, older keys with no error: rooms it creates or joins,
//! messages it sends to a re-keyed room, and signing keys it stores would never reach the web
//! UI. So once River has moved on, this build writes nothing.
//!
//! # The signal
//!
//! River's pointer records (`pointer-records.toml`): two 100-byte contracts at fixed addresses
//! whose state, signed by River's author key, names the current code hash and a version that
//! rises on every re-key. The web UI watches the same two records to reload an open tab
//! (`ui/src/components/app/freenet_api/generation_watch.rs`), and this module judges a record
//! exactly as it does:
//!
//! - **The trust anchor** is the `author_verifying_key` in `pointer-records.toml`, compiled in
//!   here as the web UI's `build.rs` compiles it in there. Never anything from the network.
//! - **This build's own generation** is the record in that same file for each artifact,
//!   accepted only if its `code_hash` is the BLAKE3 of the WASM this build actually embeds
//!   ([`watched_pointers`], the web UI's `watched_pointers`). No version number is written
//!   anywhere by hand: the file is re-signed with every re-key, and CI's
//!   `check-pointer-freshness` fails when a record does not name the committed WASM. A build
//!   whose WASM does not match its record (a local rebuild) cannot say which generation it is,
//!   so that pointer is not watched, as in the web UI.
//! - **"Newer"** is decided by freenet-migrate's [`PointerResolver`], seeded with this build's
//!   record as its floor: only [`PointerOutcome::Resolved`] with a version strictly above this
//!   build's counts ([`evaluate`], the web UI's `evaluate`). The resolver verifies the
//!   signature over `(author_vk || app_id)` first, so a record that is unsigned, signed by
//!   another key, or for the other app is no signal. A record older than this build is
//!   `Stale`, its own record is `Unchanged`, a different record at its own version is
//!   `CompetingRecord`, and a withdrawal is `Withdrawn`: none of them stops anything.
//!
//! # What happens when it is newer
//!
//! The [`WriteGate`] closes, for good in this run, and [`super::socket::NodeSocket`] refuses
//! every request that is not a read ([`super::socket::is_read`]). That is the one place every
//! write passes through: delegate registration, the stored-room compare-and-swap, the signing
//! key store, room UPDATEs and the seed PUT all go out through it. The session also stops
//! asking for them and says why on screen; the gate is what holds if it ever did not. The
//! fetch thread judges each answer the moment it arrives, so the gate can close while the
//! session is in the middle of something (a slow read, a write it already decided on). A write
//! caught that way is refused at the socket, the connection ends, and the next one fails what
//! was waiting and says why: refused, never sent. Reading
//! goes on: the rooms already stored still show and still receive messages. Reading writes
//! nothing, so it cannot split anything, and a user who cannot update at once keeps their
//! history.
//!
//! The verified record is kept in the app's config directory, so a later start that cannot
//! reach the pointer still knows. It is checked again on load like any served record, so a file
//! someone else wrote cannot stop the app, and a build at or past that version ignores it.
//!
//! # When it cannot be read
//!
//! Each case, and whether the app goes on writing:
//!
//! | Case | Writes | Why |
//! |---|---|---|
//! | A verified record above this build | stop | River moved on; writing here splits the data. |
//! | This build's own record | go on | Current. |
//! | A higher version naming this build's own code hash | go on | Current: the author re-signed the same code, so nothing splits. The web UI reloads for it. |
//! | Older, forged, other app, malformed, competing, withdrawn | go on | No evidence of a newer River; the web UI does not act on these either. |
//! | Pointer unreachable, absent, or the node too slow | go on | A network fault must not stop a current app. Retried with backoff. |
//! | A newer record seen earlier (kept on disk) | stop | Known already; a blip does not undo it. |
//! | No verdict yet at startup | wait, then go on | Writes wait until every pointer gives a verdict, or for `first_wait`. Each answer is judged as it arrives, so one newer record stops writes at once even while the other pointer is silent. |
//!
//! Failing open on an unreachable pointer leaves a window: a build that has never seen the
//! newer record, on a node that cannot fetch it, writes. That is bounded: a re-key's pointer
//! reaches subscribers in under a second (telemetry in `generation_watch.rs`), it is checked
//! again every `pointer_recheck`, and writes in the first 14 days after a re-key are still
//! carried forward by the web UI's top-up. Failing closed would instead stop every current
//! app whenever the network is slow.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use freenet_migrate::{PointerFloor, PointerOutcome, PointerResolver};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::ContractInstanceId;

use super::grant::GrantToken;
use super::socket::NodeSocket;

/// River's pointer records, as committed. CI checks each names the committed WASM.
const RECORDS_TOML: &str = include_str!("../../../../pointer-records.toml");

/// Prefix of `author_verifying_key` in `pointer-records.toml` and FREENET.md.
const AUTHOR_VK_PREFIX: &str = "river:v1:vk:";

/// Where River's desktop releases are.
pub const RELEASES_URL: &str = "https://github.com/freenet/river/releases";

/// What a message refused for this reason says. It keeps its Retry, which fails the same way
/// until the app is updated, so the text does not promise more.
pub const UPDATE_REQUIRED: &str = "Not sent: a newer version of River is out, and this version \
     of the app can no longer send. Update this app from github.com/freenet/river/releases.";

/// What a create or join refused for this reason says. It has no Try again.
pub const UPDATE_REQUIRED_WORK: &str = "A newer version of River is out, and this version of \
     the app can no longer create or join rooms. Update this app from \
     github.com/freenet/river/releases, then create or join the room there.";

/// What the composer says once River has moved on.
pub const UPDATE_REQUIRED_SEND: &str = "A newer version of River is out. Update this app to \
     send messages here; until then it only shows your rooms.";

/// What the thread says once River has moved on, about deleting someone's messages.
pub const UPDATE_REQUIRED_PURGE: &str = "A newer version of River is out. Update this app to \
     delete messages here; until then it only shows your rooms.";

/// What the thread says once River has moved on, about archiving a conversation: the change is
/// saved with the other things River keeps, and this build saves nothing then.
pub const UPDATE_REQUIRED_ARCHIVE: &str = "A newer version of River is out. Update this app to \
     archive conversations; until then it only shows your rooms.";

/// The two artifacts whose re-key strands this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Artifact {
    RoomContract,
    ChatDelegate,
}

impl Artifact {
    pub const ALL: [Artifact; 2] = [Artifact::RoomContract, Artifact::ChatDelegate];

    /// The pointer's app id, as in `pointer-records.toml`.
    pub fn app_id(self) -> &'static str {
        match self {
            Artifact::RoomContract => "river.room-contract",
            Artifact::ChatDelegate => "river.chat-delegate",
        }
    }

    /// The WASM this build carries.
    pub fn embedded_wasm(self) -> &'static [u8] {
        match self {
            Artifact::RoomContract => super::create::ROOM_CONTRACT_WASM,
            Artifact::ChatDelegate => super::delegate::CHAT_DELEGATE_WASM,
        }
    }

    fn index(self) -> usize {
        match self {
            Artifact::RoomContract => 0,
            Artifact::ChatDelegate => 1,
        }
    }
}

/// One `[[record]]` of a pointer records file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRecord {
    pub app_id: String,
    pub version: u32,
    pub code_hash: [u8; 32],
}

/// A pointer records file: the author key and the records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Records {
    pub author: String,
    pub records: Vec<BuildRecord>,
}

impl Records {
    /// The records this build was made with.
    pub fn shipped() -> Result<Records, String> {
        Records::parse(RECORDS_TOML)
    }

    /// Read a pointer records file, as the web UI's `build.rs` reads it.
    pub fn parse(text: &str) -> Result<Records, String> {
        let doc: toml::Table = text.parse().map_err(|e| format!("not TOML: {e}"))?;
        let author = doc
            .get("author_verifying_key")
            .and_then(|v| v.as_str())
            .ok_or("no author_verifying_key")?
            .to_owned();
        let mut records = Vec::new();
        for record in doc
            .get("record")
            .and_then(|v| v.as_array())
            .ok_or("no [[record]] entries")?
        {
            let app_id = record
                .get("app_id")
                .and_then(|v| v.as_str())
                .ok_or("a record has no app_id")?
                .to_owned();
            let version = record
                .get("version")
                .and_then(|v| v.as_integer())
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| format!("{app_id}: version must be a u32"))?;
            let hex = record
                .get("code_hash")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("{app_id}: no code_hash"))?;
            let code_hash =
                hex32(hex).ok_or_else(|| format!("{app_id}: code_hash is not 64 hex digits"))?;
            records.push(BuildRecord {
                app_id,
                version,
                code_hash,
            });
        }
        Ok(Records { author, records })
    }
}

fn hex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A pointer this build can place itself against.
#[derive(Debug, Clone, Copy)]
pub struct Watched {
    pub artifact: Artifact,
    pub pointer_id: ContractInstanceId,
    author_vk: VerifyingKey,
    build_floor: PointerFloor,
}

impl Watched {
    pub fn build_version(&self) -> u32 {
        self.build_floor.version()
    }
}

/// Decode `river:v1:vk:<base58>` into a verifying key.
fn decode_author_vk(s: &str) -> Option<VerifyingKey> {
    let b58 = s.strip_prefix(AUTHOR_VK_PREFIX)?;
    let bytes: [u8; 32] = bs58::decode(b58).into_vec().ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Which pointers this build can watch: those whose record names the BLAKE3 of the WASM this
/// build actually embeds (`embedded`). The web UI's `watched_pointers`.
pub fn watched_pointers(
    records: &Records,
    embedded: impl Fn(Artifact) -> &'static [u8],
) -> Vec<Watched> {
    let Some(author_vk) = decode_author_vk(&records.author) else {
        eprintln!(
            "River: not watching for a newer River: the pointer author key {:?} does not decode",
            records.author
        );
        return Vec::new();
    };
    Artifact::ALL
        .into_iter()
        .filter_map(|artifact| {
            let app_id = artifact.app_id();
            let Some(record) = records.records.iter().find(|r| r.app_id == app_id) else {
                eprintln!("River: not watching {app_id}: no pointer record");
                return None;
            };
            if *blake3::hash(embedded(artifact)).as_bytes() != record.code_hash {
                // A build whose WASM was rebuilt without re-signing. It cannot say which
                // generation it is, so it must not judge whether the network is ahead of it.
                eprintln!(
                    "River: not watching {app_id}: the WASM this build carries is not the one \
                     its pointer record v{} names",
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

/// What one pointer record says about this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The network names this build's own generation.
    Current,
    /// A verified record names a newer generation than this build.
    Newer(u32),
    /// Nothing that should change what this app does.
    NoSignal,
}

/// Judge one served pointer state against this build. The web UI's `evaluate`, with one
/// difference: a higher version that names this build's OWN code hash is not newer here.
pub fn evaluate(watched: &Watched, state: &[u8]) -> Verdict {
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
        // `Resolved` also covers a different record at the build's OWN version that wins the
        // lower-code-hash tiebreak (the author signed twice at one version). That is not a
        // newer generation, and no build could satisfy it, so only a strictly higher version
        // counts.
        //
        // A higher version naming the WASM this build carries is not a re-key (the author
        // re-signed the same code). The web UI reloads for it, which costs a reload. Here it
        // would stop every write for good, though writing to that code splits nothing.
        Some(Ok(PointerOutcome::Resolved(r)))
            if r.version() > watched.build_version()
                && Some(r.code_hash()) != watched.build_floor.code_hash() =>
        {
            Verdict::Newer(r.version())
        }
        // The author re-signed this build's own code at a higher version: current, so it
        // settles the check like this build's own record does.
        Some(Ok(PointerOutcome::Resolved(r)))
            if r.version() > watched.build_version()
                && Some(r.code_hash()) == watched.build_floor.code_hash() =>
        {
            Verdict::Current
        }
        Some(Ok(PointerOutcome::Unchanged(_))) => Verdict::Current,
        _ => Verdict::NoSignal,
    }
}

/// What a fetch of one pointer found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// The state the node served.
    State(Vec<u8>),
    /// The node answered that it has no such contract.
    Absent,
    /// No answer, or an error.
    Unreachable,
}

/// Fetches pointer states from the node. The session's own connection matches answers by kind,
/// so a fetch has a connection of its own ([`NodePointers`]); tests supply their own.
pub trait PointerSource: Send + Sync {
    /// Fetch each of `ids` from the node at `authority`, waiting at most `wait` in all, and
    /// hand each answer to `deliver` as it arrives. A pointer that does not answer is not
    /// delivered. One fast newer record must not wait behind a silent pointer: it closes the
    /// gate the moment it is judged.
    fn fetch(
        &self,
        authority: &str,
        token: &GrantToken,
        ids: &[ContractInstanceId],
        wait: Duration,
        deliver: &dyn Fn(ContractInstanceId, Fetch),
    );
}

/// Fetches pointers on a connection of its own, with the app's grant: a GET for each, without
/// the contract's code or a subscription (the 100-byte record is the whole answer), as the web
/// UI sends it.
pub struct NodePointers;

impl PointerSource for NodePointers {
    fn fetch(
        &self,
        authority: &str,
        token: &GrantToken,
        ids: &[ContractInstanceId],
        wait: Duration,
        deliver: &dyn Fn(ContractInstanceId, Fetch),
    ) {
        // This connection only reads, so its gate never needs closing.
        let Ok(mut socket) = NodeSocket::connect(authority, token, &WriteGate::default()) else {
            return;
        };
        let deadline = Instant::now() + wait;
        for id in ids {
            let get = ContractRequest::Get {
                key: *id,
                return_contract_code: false,
                subscribe: false,
                blocking_subscribe: false,
            };
            if socket.send(&ClientRequest::ContractOp(get)).is_err() {
                return;
            }
        }
        let mut open: Vec<ContractInstanceId> = ids.to_vec();
        while !open.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || socket.set_read_timeout(left).is_err() {
                return;
            }
            let (id, fetch) = match socket.recv() {
                Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                    key,
                    state,
                    ..
                }))) => (*key.id(), Fetch::State(state.as_ref().to_vec())),
                Ok(Ok(HostResponse::ContractResponse(ContractResponse::NotFound {
                    instance_id,
                }))) => (instance_id, Fetch::Absent),
                // An error, or anything else: what it names stays unanswered.
                Ok(_) => continue,
                Err(_) => return,
            };
            if let Some(i) = open.iter().position(|o| *o == id) {
                open.swap_remove(i);
                deliver(id, fetch);
            }
        }
    }
}

/// What a check under way sends back to the watch (its answers are judged where they arrive).
enum Event {
    /// The fetch is over; what did not answer learned nothing.
    Done,
}

/// Closed once River has moved on; every [`NodeSocket`] holds one and refuses writes while it
/// is closed. Cloning shares it.
#[derive(Clone, Default, Debug)]
pub struct WriteGate(Arc<AtomicBool>);

impl WriteGate {
    /// Whether writes are refused.
    pub fn is_closed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Refuse writes from now on. Never opened again.
    pub(crate) fn close(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// How often, and how patiently, the pointers are checked. Part of the session's `Pace`.
#[derive(Clone, Debug)]
pub struct CheckPace {
    /// How long writes wait for the first answer before going on without it.
    pub first_wait: Duration,
    /// How long one fetch may take in all.
    pub fetch_wait: Duration,
    /// How often the pointers are checked again after an answer.
    pub recheck: Duration,
    /// After a check that learned nothing, the first wait before the next, doubling up to
    /// `recheck`.
    pub retry_first: Duration,
    /// Until every pointer has answered with a verdict, and within `first_wait`, how soon a
    /// check that learned nothing is asked again. A node that has just started answers "not
    /// found" at once, before it has asked the network.
    pub first_retry: Duration,
}

impl Default for CheckPace {
    fn default() -> Self {
        CheckPace {
            first_wait: Duration::from_secs(10),
            fetch_wait: Duration::from_secs(30),
            recheck: Duration::from_secs(15 * 60),
            retry_first: Duration::from_secs(30),
            first_retry: Duration::from_secs(1),
        }
    }
}

/// When the next check is due after one, and the misses in a row after it. `answered`: every
/// pointer gave a verdict. `settled`: the first check is settled (see [`Watch::writes_allowed`]).
/// A miss before then is asked again after `first_retry`; after, after `retry_first`, doubling
/// up to `recheck`. A full answer resets the misses and waits `recheck`.
pub fn next_wait(pace: &CheckPace, misses: u32, answered: bool, settled: bool) -> (Duration, u32) {
    if answered {
        return (pace.recheck, 0);
    }
    if !settled {
        return (pace.first_retry.min(pace.recheck), misses);
    }
    let wait = pace
        .retry_first
        .saturating_mul(1 << misses.min(16))
        .min(pace.recheck);
    (wait, misses.saturating_add(1))
}

/// File name of a kept newer record, per app id.
fn kept_name(artifact: Artifact) -> String {
    format!("newer-river.{}", artifact.app_id())
}

/// Watches River's pointer records and holds the [`WriteGate`]. The link keeps one for its
/// whole life, so what one session learned holds for the next.
pub struct Watch {
    source: Arc<dyn PointerSource>,
    shared: Arc<Shared>,
}

/// What a fetch thread judges into as each answer arrives, and the session reads: the gate
/// closes the moment a newer record is received, whatever the session thread is doing (a
/// slow read of the stored rooms included).
pub struct Shared {
    watched: Vec<Watched>,
    gate: WriteGate,
    /// Where a newer record is kept, the app's config directory.
    dir: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl std::ops::Deref for Watch {
    type Target = Shared;
    fn deref(&self) -> &Shared {
        &self.shared
    }
}

#[derive(Default)]
struct Inner {
    /// The newest version seen above this build, per artifact.
    newer: [Option<u32>; 2],
    /// When the first check was started, and whether one has finished.
    first_started: Option<Instant>,
    first_done: bool,
    in_flight: Option<Receiver<Event>>,
    /// Whether [`Watch::poll`] has reported the gate closed.
    stale_reported: bool,
    /// The pointers that gave a verdict in the check under way.
    heard: Vec<ContractInstanceId>,
    /// When the next check is due; `None` is now.
    next: Option<Instant>,
    /// Checks in a row that learned nothing.
    misses: u32,
}

impl Watch {
    /// The watch this build ships with: River's own records, fetched from the node.
    pub fn shipped(dir: Option<&Path>) -> Watch {
        let records = match Records::shipped() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("River: not watching for a newer River: pointer-records.toml: {e}");
                Records {
                    author: String::new(),
                    records: Vec::new(),
                }
            }
        };
        Watch::new(
            watched_pointers(&records, Artifact::embedded_wasm),
            Arc::new(NodePointers),
            dir,
        )
    }

    /// A watch over `watched`, fetching through `source`, keeping what it learns in `dir`.
    pub fn new(watched: Vec<Watched>, source: Arc<dyn PointerSource>, dir: Option<&Path>) -> Watch {
        let watch = Watch {
            source,
            shared: Arc::new(Shared {
                watched,
                gate: WriteGate::default(),
                dir: dir.map(Path::to_owned),
                inner: Mutex::new(Inner::default()),
            }),
        };
        watch.load_kept();
        watch
    }
}

impl Shared {
    /// The gate every connection of this link holds.
    pub fn gate(&self) -> &WriteGate {
        &self.gate
    }

    /// Whether River has moved on: nothing is written any more.
    pub fn is_stale(&self) -> bool {
        self.gate.is_closed()
    }

    pub fn watched(&self) -> &[Watched] {
        &self.watched
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A newer record kept by an earlier run, checked again as if just served.
    fn load_kept(&self) {
        let Some(dir) = &self.dir else { return };
        for w in &self.watched {
            let path = dir.join(kept_name(w.artifact));
            // A record is 100 bytes; anything longer is not one, and is not read whole.
            let Ok(bytes) = read_capped(&path, freenet_migrate::POINTER_STATE_LEN + 1) else {
                continue;
            };
            match evaluate(w, &bytes) {
                Verdict::Newer(v) => {
                    eprintln!(
                        "River: {} moved to v{v} (seen earlier); this build is v{}",
                        w.artifact.app_id(),
                        w.build_version()
                    );
                    self.record(w, v);
                }
                // This build is that version or newer (the app was updated), or the file is
                // not a record River signed: it says nothing.
                _ => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    fn record(&self, w: &Watched, version: u32) {
        let mut inner = self.inner();
        let slot = &mut inner.newer[w.artifact.index()];
        if slot.is_none_or(|seen| seen < version) {
            *slot = Some(version);
        }
        self.gate.close();
    }

    fn keep(&self, w: &Watched, bytes: &[u8]) {
        let Some(dir) = &self.dir else { return };
        let path = dir.join(kept_name(w.artifact));
        let tmp = dir.join(format!(
            ".{}.{}.tmp",
            kept_name(w.artifact),
            std::process::id()
        ));
        let written = std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("River: couldn't keep the newer River's pointer record: {e}");
        }
    }

    /// Judge one pointer's answer as it arrives. True when it closed the gate.
    fn judge_one(&self, id: ContractInstanceId, fetch: &Fetch) -> bool {
        let Some(w) = self.watched.iter().find(|w| w.pointer_id == id) else {
            return false;
        };
        let Fetch::State(bytes) = fetch else {
            return false;
        };
        let was_stale = self.is_stale();
        // On the fetch thread: if this panics, the thread ends, the session sees the fetch end
        // without saying so, and the answer counts as no signal.
        let verdict = evaluate(w, bytes);
        if let Verdict::Newer(version) = verdict {
            // The gate first: logging and keeping the record can block (a stderr nobody
            // drains, a hung config directory), and writes must not go out meanwhile.
            self.record(w, version);
            eprintln!(
                "River: {} moved to v{version}; this build is v{}. It no longer writes.",
                w.artifact.app_id(),
                w.build_version()
            );
            self.keep(w, bytes);
        }
        if verdict != Verdict::NoSignal {
            let mut inner = self.inner();
            if !inner.heard.contains(&id) {
                inner.heard.push(id);
            }
        }
        !was_stale && self.is_stale()
    }

    /// A check is over: schedule the next.
    fn finish(&self, pace: &CheckPace, now: Instant) {
        let all = self.all_heard();
        // Settled only by a verdict from every pointer (or by `first_wait` running out, in
        // `settled`): a fast "not found" or error from a node that has not asked the network
        // yet must not let the first writes out.
        let settled = self.settled(pace, now) || all;
        let mut inner = self.inner();
        if all {
            inner.first_done = true;
        }
        inner.heard.clear();
        let (wait, misses) = next_wait(pace, inner.misses, all, settled);
        inner.misses = misses;
        inner.next = Some(now + wait);
    }

    fn all_heard(&self) -> bool {
        let inner = self.inner();
        self.watched
            .iter()
            .all(|w| inner.heard.contains(&w.pointer_id))
    }

    /// Whether there is an answer to act on: River is known to have moved on (no check is
    /// started then), every pointer has answered with a verdict, or the first check has had
    /// its `first_wait`.
    pub fn settled(&self, pace: &CheckPace, now: Instant) -> bool {
        if self.is_stale() || self.watched.is_empty() || self.all_heard() {
            return true;
        }
        let inner = self.inner();
        inner.first_done
            || inner
                .first_started
                .is_some_and(|at| now.saturating_duration_since(at) >= pace.first_wait)
    }
}

impl Watch {
    /// Take the answer of a check under way, and start one if one is due. Never blocks. True
    /// when this closed the gate.
    pub fn poll(&self, authority: &str, token: &GrantToken, pace: &CheckPace) -> bool {
        if self.watched.is_empty() {
            return false;
        }
        let now = Instant::now();
        // Answers were judged by the fetch thread as they arrived; only the end of a check
        // comes this way.
        let over = {
            let mut inner = self.inner();
            let over = inner.in_flight.as_ref().is_some_and(|rx| {
                // Over, or the fetch ended without saying so (it panicked).
                matches!(
                    rx.try_recv(),
                    Ok(Event::Done) | Err(TryRecvError::Disconnected)
                )
            });
            if over {
                inner.in_flight = None;
            }
            over
        };
        if over {
            self.finish(pace, now);
        }
        let closed = self.is_stale() && !std::mem::replace(&mut self.inner().stale_reported, true);
        // Once River has moved on there is nothing more to learn in this run.
        if self.is_stale() {
            return closed;
        }
        let mut inner = self.inner();
        if inner.in_flight.is_some() || inner.next.is_some_and(|at| now < at) {
            return closed;
        }
        let (tx, rx) = channel();
        let source = self.source.clone();
        let shared = self.shared.clone();
        let ids: Vec<ContractInstanceId> = self.watched.iter().map(|w| w.pointer_id).collect();
        let (authority, token, wait) = (authority.to_owned(), token.clone(), pace.fetch_wait);
        std::thread::spawn(move || {
            // Judged here, as each answer arrives: a newer record closes the gate at once.
            let deliver = |id, fetch: Fetch| {
                shared.judge_one(id, &fetch);
            };
            source.fetch(&authority, &token, &ids, wait, &deliver);
            let _ = tx.send(Event::Done);
        });
        inner.in_flight = Some(rx);
        inner.first_started.get_or_insert(now);
        closed
    }

    /// Wait, polling, until [`Self::settled`]. For tests; the session never blocks on it.
    #[cfg(test)]
    pub fn settle(
        &self,
        authority: &str,
        token: &GrantToken,
        pace: &CheckPace,
        stop: &dyn Fn() -> bool,
    ) -> bool {
        let mut closed = self.poll(authority, token, pace);
        while !self.watched.is_empty() && !self.settled(pace, Instant::now()) && !stop() {
            std::thread::sleep(Duration::from_millis(10));
            closed |= self.poll(authority, token, pace);
        }
        closed
    }

    /// Whether the app may write now: River has not moved on, and the first check has
    /// answered or had its `first_wait`. A watch with nothing to watch never holds writes.
    pub fn writes_allowed(&self, pace: &CheckPace) -> bool {
        !self.is_stale() && (self.watched.is_empty() || self.settled(pace, Instant::now()))
    }
}

/// Read at most `cap` bytes of `path`.
fn read_capped(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(cap as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
#[path = "generation_tests.rs"]
pub(crate) mod tests;
