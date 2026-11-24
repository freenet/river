//! The admission proof-of-work solver, run off the page's main thread
//! (freenet/river#1023).
//!
//! The page loads its own wasm module into one or more Web Workers (the same
//! `river-ui` JS glue the page runs, which starts [`worker_main`] instead of
//! the app when it finds itself in a worker). The page sends [`ToWorker`]
//! messages and reads [`FromWorker`] messages; both travel as CBOR bytes, so
//! the protocol and the solver's behaviour ([`WorkerCore`]) are tested natively.
//!
//! The page never waits on a worker for correctness: if workers cannot start
//! (a browser or container that refuses them) or one fails, the caller falls
//! back to solving on the main thread.

use ed25519_dalek::VerifyingKey;
use river_core::room_state::admission::{PowRequirement, PowSolver, PowStep};
use serde::{Deserialize, Serialize};

/// What the page tells a worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToWorker {
    /// Search nonces `start, start + stride, ...` for `owner`'s room.
    Start {
        owner: [u8; 32],
        member: [u8; 32],
        epoch: u32,
        bits: u8,
        start: u64,
        stride: u64,
        /// Begin without hashing until `Pause(false)`.
        paused: bool,
    },
    /// The room now asks for `bits` in `epoch`. A new epoch restarts the
    /// search; the same epoch continues. Resumes a worker that had reported a
    /// nonce the page did not accept.
    Retarget {
        epoch: u32,
        bits: u8,
    },
    Pause(bool),
    /// Stop hashing and drop the search.
    Stop,
}

/// What a worker tells the page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromWorker {
    /// The worker's module is loaded and it takes messages.
    Ready,
    /// Nonces this worker has tried since its search (re)started.
    Progress { tried: u64 },
    /// A nonce that meets the target the worker was last given.
    Found { nonce: u64 },
    /// The worker cannot continue.
    Failed(String),
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(msg, &mut out).expect("a message encodes into memory");
    out
}

pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, String> {
    ciborium::from_reader(bytes).map_err(|e| e.to_string())
}

/// The solver and what the page last told it, with no browser in it.
#[derive(Default)]
pub struct WorkerCore {
    solver: Option<PowSolver>,
    paused: bool,
    found: bool,
}

impl WorkerCore {
    /// Apply a message from the page. `Some` is something to send back.
    pub fn handle(&mut self, msg: ToWorker) -> Option<FromWorker> {
        match msg {
            ToWorker::Start {
                owner,
                member,
                epoch,
                bits,
                start,
                stride,
                paused,
            } => {
                let (Ok(owner), Ok(member)) = (
                    VerifyingKey::from_bytes(&owner),
                    VerifyingKey::from_bytes(&member),
                ) else {
                    self.solver = None;
                    return Some(FromWorker::Failed("not a valid key".into()));
                };
                self.solver = Some(
                    PowSolver::new(&owner, &member, &requirement(epoch, bits), bits)
                        .with_partition(start, stride),
                );
                self.paused = paused;
                self.found = false;
                None
            }
            ToWorker::Retarget { epoch, bits } => {
                if let Some(solver) = self.solver.as_mut() {
                    solver.retarget(&requirement(epoch, bits), bits);
                    self.found = false;
                }
                None
            }
            ToWorker::Pause(p) => {
                self.paused = p;
                None
            }
            ToWorker::Stop => {
                self.solver = None;
                None
            }
        }
    }

    /// Whether [`run`](Self::run) has anything to do.
    pub fn has_work(&self) -> bool {
        self.solver.is_some() && !self.paused && !self.found
    }

    /// Try up to `budget` nonces. `Found` once, then idle until a retarget.
    pub fn run(&mut self, budget: u64) -> Option<FromWorker> {
        if !self.has_work() {
            return None;
        }
        let solver = self.solver.as_mut()?;
        Some(match solver.step(budget) {
            PowStep::Found { nonce, .. } => {
                self.found = true;
                FromWorker::Found { nonce }
            }
            PowStep::Progress { tried, .. } => FromWorker::Progress { tried },
        })
    }
}

fn requirement(epoch: u32, bits: u8) -> PowRequirement {
    PowRequirement {
        difficulty_bits: bits,
        epoch,
        expected_hashes: 2f64.powi(bits as i32),
    }
}

#[cfg(target_arch = "wasm32")]
pub use browser::{worker_count, worker_main, Pool};

#[cfg(target_arch = "wasm32")]
mod browser {
    use super::*;
    use futures::channel::mpsc;
    use futures::StreamExt;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{DedicatedWorkerGlobalScope, MessageEvent, Worker};

    /// Nonces tried between two looks at the clock.
    const BUDGET: u64 = 2048;
    /// Milliseconds a worker hashes before it lets its own messages in.
    const SLICE_MS: f64 = 20.0;
    /// Milliseconds between two progress messages.
    const PROGRESS_MS: f64 = 200.0;
    /// How long a worker may take to load its module before the page gives up
    /// on workers (a phone fetching and compiling the wasm).
    const READY_TIMEOUT_MS: u32 = 30_000;
    /// Most workers one join starts.
    const MAX_WORKERS: usize = 4;

    /// Workers to start: one fewer than the cores (the page keeps one), at
    /// least one, at most [`MAX_WORKERS`].
    pub fn worker_count() -> usize {
        let cores = web_sys::window()
            .map(|w| w.navigator().hardware_concurrency())
            .unwrap_or(1.0);
        (cores as usize).saturating_sub(1).clamp(1, MAX_WORKERS)
    }

    /// Entered from `main` when the module finds itself in a worker.
    pub fn worker_main() {
        let Ok(scope) = js_sys::global().dyn_into::<DedicatedWorkerGlobalScope>() else {
            return;
        };
        let core = Rc::new(RefCell::new(WorkerCore::default()));
        let running = Rc::new(Cell::new(false));
        let on_message = {
            let (core, running, scope) = (core.clone(), running.clone(), scope.clone());
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                let Ok(bytes) = event.data().dyn_into::<js_sys::Uint8Array>() else {
                    return;
                };
                let reply = match decode::<ToWorker>(&bytes.to_vec()) {
                    Ok(msg) => core.borrow_mut().handle(msg),
                    Err(e) => Some(FromWorker::Failed(format!("bad message: {e}"))),
                };
                if let Some(reply) = reply {
                    post(&scope, &reply);
                }
                if !running.get() && core.borrow().has_work() {
                    running.set(true);
                    let (core, running, scope) = (core.clone(), running.clone(), scope.clone());
                    wasm_bindgen_futures::spawn_local(async move {
                        drive(&core, &scope).await;
                        running.set(false);
                    });
                }
            })
        };
        scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
        // The page sends nothing until it hears this: a message sent before the
        // handler above exists is lost.
        post(&scope, &FromWorker::Ready);
    }

    fn post(scope: &DedicatedWorkerGlobalScope, msg: &FromWorker) {
        let _ = scope.post_message(&js_sys::Uint8Array::from(encode(msg).as_slice()));
    }

    /// Hash in slices, letting this worker's own messages (retarget, pause,
    /// stop) in between, until there is nothing left to do.
    async fn drive(core: &Rc<RefCell<WorkerCore>>, scope: &DedicatedWorkerGlobalScope) {
        let mut last_progress = js_sys::Date::now();
        while core.borrow().has_work() {
            let began = js_sys::Date::now();
            let mut out = None;
            while js_sys::Date::now() - began < SLICE_MS {
                out = core.borrow_mut().run(BUDGET);
                if matches!(out, Some(FromWorker::Found { .. }) | None) {
                    break;
                }
            }
            match out {
                Some(found @ FromWorker::Found { .. }) => post(scope, &found),
                Some(progress) if js_sys::Date::now() - last_progress >= PROGRESS_MS => {
                    last_progress = js_sys::Date::now();
                    post(scope, &progress);
                }
                _ => {}
            }
            crate::util::yield_now().await;
        }
    }

    /// Workers running the solver, and the channel their messages arrive on.
    /// Dropping the pool stops them.
    pub struct Pool {
        workers: Vec<Worker>,
        rx: mpsc::UnboundedReceiver<(usize, FromWorker)>,
        blob_url: String,
        _handlers: Vec<Closure<dyn FnMut(MessageEvent)>>,
        _error_handlers: Vec<Closure<dyn FnMut(web_sys::Event)>>,
    }

    impl Drop for Pool {
        fn drop(&mut self) {
            for w in &self.workers {
                w.terminate();
            }
            let _ = web_sys::Url::revoke_object_url(&self.blob_url);
        }
    }

    /// The page's own wasm-bindgen glue: the module script the app was loaded
    /// from.
    fn glue_url() -> Option<String> {
        let document = web_sys::window()?.document()?;
        let scripts = document
            .query_selector_all("script[type=module][src]")
            .ok()?;
        (0..scripts.length())
            .filter_map(|i| {
                scripts
                    .item(i)?
                    .dyn_into::<web_sys::HtmlScriptElement>()
                    .ok()
            })
            .map(|s| s.src())
            .find(|src| {
                let name = src.rsplit('/').next().unwrap_or("");
                name.starts_with("river-ui") && name.ends_with(".js")
            })
    }

    impl Pool {
        /// Start `n` workers and wait until all have loaded. `None` if any
        /// cannot start or loading takes longer than [`READY_TIMEOUT_MS`].
        pub async fn spawn(n: usize) -> Option<Pool> {
            let document = web_sys::window()?.document()?;
            let glue = glue_url()?;
            let base = document.base_uri().ok().flatten()?;
            // A worker made from a blob has a `blob:` base, against which the
            // glue's own root-relative fetch of the wasm cannot resolve, so
            // fetches in the worker resolve against the page's base instead.
            // The worker is a classic one that `import()`s the glue: in the
            // sandboxed iframe the container serves the app in, a module worker
            // made from a blob fails to start, and one made straight from the
            // glue's URL is a cross-origin worker and is refused.
            let script = format!(
                "const base = {base};\n\
                 const nativeFetch = self.fetch.bind(self);\n\
                 self.fetch = (input, init) => nativeFetch(typeof input === 'string' ? new URL(input, base).href : input, init);\n\
                 import({glue});\n",
                base = js_string(&base),
                glue = js_string(&glue),
            );
            let parts = js_sys::Array::of1(&JsValue::from_str(&script));
            let options = web_sys::BlobPropertyBag::new();
            options.set_type("text/javascript");
            let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options).ok()?;
            let blob_url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;

            let (tx, rx) = mpsc::unbounded();
            let mut pool = Pool {
                workers: Vec::new(),
                rx,
                blob_url: blob_url.clone(),
                _handlers: Vec::new(),
                _error_handlers: Vec::new(),
            };
            for i in 0..n {
                let worker = Worker::new(&blob_url).ok()?;
                let sender = tx.clone();
                let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
                    let msg = match e.data().dyn_into::<js_sys::Uint8Array>() {
                        Ok(bytes) => decode::<FromWorker>(&bytes.to_vec())
                            .unwrap_or_else(|e| FromWorker::Failed(format!("bad reply: {e}"))),
                        Err(_) => FromWorker::Failed("reply was not bytes".into()),
                    };
                    let _ = sender.unbounded_send((i, msg));
                });
                worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
                let sender = tx.clone();
                let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    let _ = sender.unbounded_send((i, FromWorker::Failed("worker error".into())));
                });
                worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
                pool.workers.push(worker);
                pool._handlers.push(on_message);
                pool._error_handlers.push(on_error);
            }
            drop(tx);
            let mut ready = 0;
            while ready < n {
                match pool.next(READY_TIMEOUT_MS).await {
                    Some((_, FromWorker::Ready)) => ready += 1,
                    _ => return None,
                }
            }
            Some(pool)
        }

        pub fn len(&self) -> usize {
            self.workers.len()
        }

        fn send(&self, worker: &Worker, msg: &ToWorker) {
            let _ = worker.post_message(&js_sys::Uint8Array::from(encode(msg).as_slice()));
        }

        /// Give each worker its share of the nonce space.
        pub fn start(
            &self,
            owner: &VerifyingKey,
            member: &VerifyingKey,
            req: &PowRequirement,
            paused: bool,
        ) {
            let stride = self.workers.len() as u64;
            for (i, w) in self.workers.iter().enumerate() {
                self.send(
                    w,
                    &ToWorker::Start {
                        owner: owner.to_bytes(),
                        member: member.to_bytes(),
                        epoch: req.epoch,
                        bits: req.difficulty_bits,
                        start: i as u64,
                        stride,
                        paused,
                    },
                );
            }
        }

        pub fn retarget(&self, req: &PowRequirement) {
            for w in &self.workers {
                self.send(
                    w,
                    &ToWorker::Retarget {
                        epoch: req.epoch,
                        bits: req.difficulty_bits,
                    },
                );
            }
        }

        pub fn pause(&self, paused: bool) {
            for w in &self.workers {
                self.send(w, &ToWorker::Pause(paused));
            }
        }

        /// The next message from any worker, or `None` after `timeout_ms`.
        pub async fn next(&mut self, timeout_ms: u32) -> Option<(usize, FromWorker)> {
            let timeout = Box::pin(crate::util::sleep(std::time::Duration::from_millis(
                timeout_ms as u64,
            )));
            match futures::future::select(self.rx.next(), timeout).await {
                futures::future::Either::Left((msg, _)) => msg,
                futures::future::Either::Right(_) => None,
            }
        }
    }

    /// A JavaScript string literal for `s`.
    fn js_string(s: &str) -> String {
        js_sys::JSON::stringify(&JsValue::from_str(s))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "\"\"".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::provisional::{leading_zero_bits, pow_hash};

    fn keys() -> ([u8; 32], [u8; 32]) {
        (
            SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes(),
            SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes(),
        )
    }

    fn start(epoch: u32, bits: u8, start: u64, stride: u64, paused: bool) -> ToWorker {
        let (owner, member) = keys();
        ToWorker::Start {
            owner,
            member,
            epoch,
            bits,
            start,
            stride,
            paused,
        }
    }

    /// Drive a core to its first `Found`, returning the nonce.
    fn solve(core: &mut WorkerCore) -> u64 {
        loop {
            if let Some(FromWorker::Found { nonce }) = core.run(1000) {
                return nonce;
            }
            assert!(core.has_work(), "the core stopped before it found a nonce");
        }
    }

    fn meets(epoch: u32, nonce: u64, bits: u8) -> bool {
        let (owner, member) = keys();
        let h = pow_hash(
            &VerifyingKey::from_bytes(&owner).unwrap(),
            &VerifyingKey::from_bytes(&member).unwrap(),
            epoch,
            nonce,
        );
        leading_zero_bits(&h) >= bits as u32
    }

    #[test]
    fn every_message_round_trips_through_bytes() {
        for msg in [
            start(3, 12, 1, 4, true),
            ToWorker::Retarget { epoch: 4, bits: 13 },
            ToWorker::Pause(true),
            ToWorker::Stop,
        ] {
            assert_eq!(decode::<ToWorker>(&encode(&msg)).unwrap(), msg);
        }
        for msg in [
            FromWorker::Ready,
            FromWorker::Progress { tried: u64::MAX },
            FromWorker::Found { nonce: u64::MAX },
            FromWorker::Failed("no".into()),
        ] {
            assert_eq!(decode::<FromWorker>(&encode(&msg)).unwrap(), msg);
        }
        assert!(
            decode::<ToWorker>(&[0xff, 0x00]).is_err(),
            "garbage is refused"
        );
    }

    #[test]
    fn a_started_core_finds_a_nonce_that_meets_the_target() {
        let mut core = WorkerCore::default();
        assert!(!core.has_work(), "idle before Start");
        assert_eq!(core.handle(start(5, 10, 0, 1, false)), None);
        let nonce = solve(&mut core);
        assert!(meets(5, nonce, 10));
        assert!(!core.has_work(), "Found stops the search");
        assert_eq!(core.run(1000), None);
    }

    #[test]
    fn a_paused_core_does_no_work_until_resumed() {
        let mut core = WorkerCore::default();
        core.handle(start(5, 6, 0, 1, true));
        assert!(!core.has_work());
        assert_eq!(core.run(1_000_000), None, "paused hashes nothing");
        core.handle(ToWorker::Pause(false));
        assert!(meets(5, solve(&mut core), 6));
        core.handle(ToWorker::Pause(true));
        core.handle(ToWorker::Pause(false));
        assert!(!core.has_work(), "pausing again does not undo a Found");
    }

    #[test]
    fn stop_drops_the_search_and_a_restart_begins_again() {
        let mut core = WorkerCore::default();
        core.handle(start(5, 20, 0, 1, false));
        assert!(matches!(
            core.run(10),
            Some(FromWorker::Progress { tried: 10 })
        ));
        core.handle(ToWorker::Stop);
        assert!(!core.has_work());
        core.handle(start(5, 4, 0, 1, false));
        assert!(matches!(
            core.run(1),
            Some(FromWorker::Progress { tried: 1 } | FromWorker::Found { .. })
        ));
    }

    #[test]
    fn partitions_search_disjoint_nonces_that_cover_the_space() {
        // With stride n, worker i only returns nonces congruent to i mod n.
        for i in 0..3u64 {
            let mut core = WorkerCore::default();
            core.handle(start(1, 8, i, 3, false));
            assert_eq!(solve(&mut core) % 3, i);
        }
    }

    #[test]
    fn a_retarget_to_more_bits_keeps_the_work_and_rejects_the_old_nonce() {
        let mut core = WorkerCore::default();
        core.handle(start(2, 4, 0, 1, false));
        let first = solve(&mut core);
        assert!(meets(2, first, 4));
        // The page found the nonce too weak for a newly raised target and
        // retargets: the worker resumes past it and returns one that meets 9.
        core.handle(ToWorker::Retarget { epoch: 2, bits: 9 });
        assert!(
            core.has_work(),
            "a retarget resumes a worker that had found"
        );
        let second = solve(&mut core);
        assert!(second > first, "same epoch continues past what was tried");
        assert!(meets(2, second, 9));
    }

    #[test]
    fn a_new_epoch_restarts_the_search_from_the_start_of_its_partition() {
        let mut core = WorkerCore::default();
        core.handle(start(2, 30, 1, 2, false));
        assert!(matches!(
            core.run(500),
            Some(FromWorker::Progress { tried: 500 })
        ));
        core.handle(ToWorker::Retarget { epoch: 3, bits: 30 });
        assert!(
            matches!(core.run(7), Some(FromWorker::Progress { tried: 7 })),
            "a new epoch counts from zero"
        );
    }

    #[test]
    fn a_retarget_to_the_same_target_does_not_restart_the_count() {
        let mut core = WorkerCore::default();
        core.handle(start(2, 30, 0, 1, false));
        core.run(500);
        core.handle(ToWorker::Retarget { epoch: 2, bits: 30 });
        assert!(matches!(
            core.run(1),
            Some(FromWorker::Progress { tried: 501 })
        ));
    }

    #[test]
    fn messages_before_a_start_are_harmless_and_a_bad_key_is_reported() {
        let mut core = WorkerCore::default();
        assert_eq!(core.handle(ToWorker::Retarget { epoch: 1, bits: 5 }), None);
        assert_eq!(core.handle(ToWorker::Pause(false)), None);
        assert!(!core.has_work());
        // 32 bytes that are not a point on the curve.
        let not_a_key = (0..=255u8)
            .map(|b| [b; 32])
            .find(|k| VerifyingKey::from_bytes(k).is_err())
            .expect("some repeated byte is not a key");
        let bad = ToWorker::Start {
            owner: not_a_key,
            member: not_a_key,
            epoch: 0,
            bits: 4,
            start: 0,
            stride: 1,
            paused: false,
        };
        assert!(matches!(core.handle(bad), Some(FromWorker::Failed(_))));
        assert!(!core.has_work());
    }

    /// The page and the worker must agree on the hash: a worker's nonce is
    /// accepted by the same function the contract uses.
    #[test]
    fn a_worker_nonce_equals_what_the_main_thread_solver_finds() {
        use river_core::room_state::admission::PowSolver;
        let (owner, member) = keys();
        let (o, m) = (
            VerifyingKey::from_bytes(&owner).unwrap(),
            VerifyingKey::from_bytes(&member).unwrap(),
        );
        let mut direct = PowSolver::new(&o, &m, &requirement(6, 8), 8);
        let PowStep::Found { nonce: want, .. } = direct.step(u64::MAX / 2) else {
            panic!("found")
        };
        let mut core = WorkerCore::default();
        core.handle(start(6, 8, 0, 1, false));
        assert_eq!(solve(&mut core), want);
    }
}
