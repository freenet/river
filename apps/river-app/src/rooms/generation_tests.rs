use super::*;
use ed25519_dalek::{Signer, SigningKey};
use std::sync::atomic::AtomicUsize;

// ---- build identity --------------------------------------------------------------------

/// The shipped build must watch both pointers. If this fails, `pointer-records.toml` no
/// longer names the WASM this app carries, and the app would never notice a re-key.
#[test]
fn shipped_build_watches_both_pointers() {
    let watched = watched_pointers(&Records::shipped().unwrap(), Artifact::embedded_wasm);
    let apps: Vec<_> = watched.iter().map(|w| w.artifact).collect();
    assert_eq!(apps, Artifact::ALL);
}

/// This build's generation is the records file's, for the WASM it carries: no number is kept
/// anywhere else.
#[test]
fn the_build_generation_is_the_records_files() {
    let records = Records::shipped().unwrap();
    for w in watched_pointers(&records, Artifact::embedded_wasm) {
        let record = records
            .records
            .iter()
            .find(|r| r.app_id == w.artifact.app_id())
            .unwrap();
        assert_eq!(w.build_version(), record.version);
        assert_eq!(
            *blake3::hash(w.artifact.embedded_wasm()).as_bytes(),
            record.code_hash
        );
    }
}

/// The address the app derives must be the one the records file pins, which
/// `scripts/check-pointer-freshness.sh` also checks.
#[test]
fn pointer_ids_match_the_records_file() {
    for w in watched_pointers(&Records::shipped().unwrap(), Artifact::embedded_wasm) {
        let id = w.pointer_id.to_string();
        assert!(
            RECORDS_TOML.contains(&format!("pointer_key = \"{id}\"")),
            "{} derives pointer {id}, which pointer-records.toml does not pin",
            w.artifact.app_id()
        );
    }
}

/// The author key is the one FREENET.md publishes, as the web UI's is.
#[test]
fn the_author_key_is_freenet_mds() {
    let freenet_md = include_str!("../../../../FREENET.md");
    let author = Records::shipped().unwrap().author;
    assert!(author.starts_with(AUTHOR_VK_PREFIX));
    assert!(
        freenet_md.contains(&author),
        "{author} is not in FREENET.md"
    );
}

#[test]
fn mismatched_embedded_wasm_disables_only_that_pointer() {
    let watched = watched_pointers(&Records::shipped().unwrap(), |a| match a {
        Artifact::RoomContract => b"a locally rebuilt contract",
        Artifact::ChatDelegate => Artifact::ChatDelegate.embedded_wasm(),
    });
    let apps: Vec<_> = watched.iter().map(|w| w.artifact).collect();
    assert_eq!(apps, vec![Artifact::ChatDelegate]);
}

#[test]
fn undecodable_author_key_disables_the_watch() {
    for author in ["river:v1:vk:nope", ""] {
        let records = Records {
            author: author.into(),
            ..Records::shipped().unwrap()
        };
        assert!(watched_pointers(&records, Artifact::embedded_wasm).is_empty());
    }
}

#[test]
fn a_records_file_without_what_it_needs_is_refused() {
    assert!(Records::parse("not = [toml").is_err());
    assert!(Records::parse("[[record]]\napp_id = \"x\"").is_err());
    let bad_hash =
        "author_verifying_key = \"k\"\n[[record]]\napp_id = \"x\"\nversion = 1\ncode_hash = \"ab\"";
    assert!(Records::parse(bad_hash).is_err());
    let bad_version =
        "author_verifying_key = \"k\"\n[[record]]\napp_id = \"x\"\nversion = -1\ncode_hash = \"\"";
    assert!(Records::parse(bad_version).is_err());
}

// ---- golden vectors: River's own signed records --------------------------------------------

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The `state` River signed for `app_id`, as committed in `pointer-records.toml`.
fn shipped_state(app_id: &str) -> Vec<u8> {
    let doc: toml::Table = RECORDS_TOML.parse().unwrap();
    let record = doc["record"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["app_id"].as_str() == Some(app_id))
        .unwrap();
    unhex(record["state"].as_str().unwrap())
}

/// River's real records, signed with River's real key, read against this build: its own
/// generation, so they stop nothing.
#[test]
fn the_shipped_records_are_this_builds_own() {
    for w in watched_pointers(&Records::shipped().unwrap(), Artifact::embedded_wasm) {
        assert_eq!(
            evaluate(&w, &shipped_state(w.artifact.app_id())),
            Verdict::Current,
            "{}",
            w.artifact.app_id()
        );
    }
}

/// The same real records, read by a build one generation older (its record names other WASM
/// one version below), are newer: River's real signature verifies under the real key.
#[test]
fn the_shipped_records_are_newer_than_an_older_build() {
    static OLD: &[u8] = b"an older build's wasm";
    let shipped = Records::shipped().unwrap();
    let older = Records {
        author: shipped.author.clone(),
        records: shipped
            .records
            .iter()
            .map(|r| BuildRecord {
                app_id: r.app_id.clone(),
                version: r.version - 1,
                code_hash: *blake3::hash(OLD).as_bytes(),
            })
            .collect(),
    };
    let watched = watched_pointers(&older, |_| OLD);
    assert_eq!(watched.len(), 2);
    for w in watched {
        let theirs = shipped
            .records
            .iter()
            .find(|r| r.app_id == w.artifact.app_id())
            .unwrap();
        assert_eq!(
            evaluate(&w, &shipped_state(w.artifact.app_id())),
            Verdict::Newer(theirs.version)
        );
    }
}

/// A record for one app does not count for the other, even signed by River.
#[test]
fn a_shipped_record_for_the_other_app_is_no_signal() {
    let watched = watched_pointers(&Records::shipped().unwrap(), Artifact::embedded_wasm);
    let room = watched[0];
    assert_eq!(room.artifact, Artifact::RoomContract);
    assert_eq!(
        evaluate(&room, &shipped_state("river.chat-delegate")),
        Verdict::NoSignal
    );
}

// ---- verdicts, against a test key (the web UI's cases) -------------------------------------

fn test_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn author_of(sk: &SigningKey) -> String {
    format!(
        "{AUTHOR_VK_PREFIX}{}",
        bs58::encode(sk.verifying_key().as_bytes()).into_string()
    )
}

pub(crate) fn signed_state(
    sk: &SigningKey,
    app_id: &str,
    version: u32,
    code_hash: [u8; 32],
) -> Vec<u8> {
    let params = freenet_migrate::pointer_params(&sk.verifying_key(), app_id.as_bytes()).unwrap();
    let msg = freenet_migrate::pointer_signing_message(&params, version, &code_hash);
    freenet_migrate::PointerRecord {
        version,
        code_hash,
        signature: sk.sign(&msg).to_bytes(),
    }
    .encode()
    .to_vec()
}

static ROOM_V1: &[u8] = b"room contract v1";
static DELEGATE_V1: &[u8] = b"chat delegate v1";

/// Both pointers watched at build version 1, signed by the test key.
pub(crate) fn test_watched() -> Vec<Watched> {
    let records = Records {
        author: author_of(&test_key()),
        records: vec![
            BuildRecord {
                app_id: "river.room-contract".into(),
                version: 1,
                code_hash: *blake3::hash(ROOM_V1).as_bytes(),
            },
            BuildRecord {
                app_id: "river.chat-delegate".into(),
                version: 1,
                code_hash: *blake3::hash(DELEGATE_V1).as_bytes(),
            },
        ],
    };
    let watched = watched_pointers(&records, |a| match a {
        Artifact::RoomContract => ROOM_V1,
        Artifact::ChatDelegate => DELEGATE_V1,
    });
    assert_eq!(watched.len(), 2);
    watched
}

pub(crate) fn delegate_state(version: u32) -> Vec<u8> {
    let code_hash = if version == 1 {
        *blake3::hash(DELEGATE_V1).as_bytes()
    } else {
        [0x40 + version as u8; 32]
    };
    signed_state(&test_key(), "river.chat-delegate", version, code_hash)
}

pub(crate) fn room_state(version: u32) -> Vec<u8> {
    let code_hash = if version == 1 {
        *blake3::hash(ROOM_V1).as_bytes()
    } else {
        [version as u8; 32]
    };
    signed_state(&test_key(), "river.room-contract", version, code_hash)
}

#[test]
fn same_record_as_the_build_is_current() {
    let w = test_watched()[1];
    assert_eq!(evaluate(&w, &delegate_state(1)), Verdict::Current);
}

#[test]
fn a_newer_signed_record_is_newer() {
    let w = test_watched()[1];
    assert_eq!(evaluate(&w, &delegate_state(5)), Verdict::Newer(5));
}

/// The pointer is published after the build, so a fresh build sees it lag.
#[test]
fn a_pointer_behind_the_build_is_no_signal() {
    let sk = test_key();
    static WASM: &[u8] = b"chat delegate v4";
    let records = Records {
        author: author_of(&sk),
        records: vec![BuildRecord {
            app_id: "river.chat-delegate".into(),
            version: 4,
            code_hash: *blake3::hash(WASM).as_bytes(),
        }],
    };
    let w = watched_pointers(&records, |_| WASM)[0];
    let behind = signed_state(&sk, "river.chat-delegate", 3, [0x33; 32]);
    assert_eq!(evaluate(&w, &behind), Verdict::NoSignal);
}

#[test]
fn unsigned_forged_or_foreign_records_are_no_signal() {
    let w = test_watched()[1];
    let stranger = SigningKey::from_bytes(&[9u8; 32]);
    let forged = signed_state(&stranger, "river.chat-delegate", 9, [0x99; 32]);
    assert_eq!(evaluate(&w, &forged), Verdict::NoSignal);
    let other_app = signed_state(&test_key(), "river.room-contract", 9, [0x99; 32]);
    assert_eq!(evaluate(&w, &other_app), Verdict::NoSignal);
    // A real record with its version raised: the signature no longer covers it.
    let mut tampered = delegate_state(1);
    tampered[3] = 9;
    assert_eq!(evaluate(&w, &tampered), Verdict::NoSignal);
    // Unsigned: a version and a hash with a zero signature.
    let mut unsigned = delegate_state(9);
    for b in &mut unsigned[36..] {
        *b = 0;
    }
    assert_eq!(evaluate(&w, &unsigned), Verdict::NoSignal);
    assert_eq!(evaluate(&w, &[]), Verdict::NoSignal);
    assert_eq!(evaluate(&w, &[0u8; 7]), Verdict::NoSignal);
}

/// Only the author can sign two records at one version. Whichever side of the tiebreak the
/// rival lands on, it is not a newer generation.
#[test]
fn a_competing_record_at_the_build_version_is_no_signal() {
    let w = test_watched()[1];
    for rival_hash in [[0x01; 32], [0xFF; 32]] {
        let rival = signed_state(&test_key(), "river.chat-delegate", 1, rival_hash);
        assert_eq!(evaluate(&w, &rival), Verdict::NoSignal, "{rival_hash:?}");
    }
}

#[test]
fn a_withdrawal_is_no_signal() {
    let w = test_watched()[1];
    let state = signed_state(&test_key(), "river.chat-delegate", 5, [0u8; 32]);
    assert_eq!(evaluate(&w, &state), Verdict::NoSignal);
}

// ---- the watch ---------------------------------------------------------------------------

/// Answers every fetch with what the test set, counting fetches.
#[derive(Default)]
pub(crate) struct FakePointers {
    pub answer: Mutex<Vec<(ContractInstanceId, Fetch)>>,
    pub fetches: AtomicUsize,
}

impl PointerSource for FakePointers {
    fn fetch(
        &self,
        _: &str,
        _: &GrantToken,
        ids: &[ContractInstanceId],
        _: Duration,
    ) -> Vec<(ContractInstanceId, Fetch)> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let answer = self.answer.lock().unwrap().clone();
        ids.iter()
            .map(|id| {
                let f = answer
                    .iter()
                    .find(|(i, _)| i == id)
                    .map_or(Fetch::Unreachable, |(_, f)| f.clone());
                (*id, f)
            })
            .collect()
    }
}

fn pace() -> CheckPace {
    CheckPace {
        first_wait: Duration::from_secs(5),
        fetch_wait: Duration::from_secs(1),
        recheck: Duration::from_millis(400),
        retry_first: Duration::from_millis(40),
    }
}

fn token() -> GrantToken {
    GrantToken::new("t".into())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("river-generation-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The fetch both pointers answer with: the room contract current, the delegate at `delegate`.
fn answer(source: &FakePointers, watched: &[Watched], delegate: Option<Fetch>) {
    *source.answer.lock().unwrap() = vec![
        (watched[0].pointer_id, Fetch::State(room_state(1))),
        (
            watched[1].pointer_id,
            delegate.unwrap_or(Fetch::State(delegate_state(1))),
        ),
    ];
}

fn settle(watch: &Watch) {
    watch.settle("a", &token(), &pace(), &|| false);
}

#[test]
fn a_current_network_leaves_writes_on() {
    let source = Arc::new(FakePointers::default());
    let watched = test_watched();
    answer(&source, &watched, None);
    let watch = Watch::new(watched, source.clone(), None);
    assert!(
        !watch.writes_allowed(&pace()),
        "writes wait for the first check"
    );
    settle(&watch);
    assert!(watch.writes_allowed(&pace()));
    assert!(!watch.is_stale());
}

#[test]
fn a_newer_delegate_closes_the_gate_and_is_kept() {
    let dir = temp_dir("newer");
    let source = Arc::new(FakePointers::default());
    let watched = test_watched();
    answer(&source, &watched, Some(Fetch::State(delegate_state(2))));
    let watch = Watch::new(watched.clone(), source.clone(), Some(&dir));
    settle(&watch);
    assert!(watch.is_stale());
    assert!(watch.gate().is_closed());
    assert!(!watch.writes_allowed(&pace()));
    assert_eq!(
        std::fs::read(dir.join("newer-river.river.chat-delegate")).unwrap(),
        delegate_state(2)
    );

    // A later start that cannot reach the pointer at all still knows.
    let offline = Arc::new(FakePointers::default());
    let again = Watch::new(watched, offline.clone(), Some(&dir));
    assert!(again.is_stale(), "the kept record closes the gate on load");
    settle(&again);
    assert!(again.is_stale());
    assert_eq!(
        offline.fetches.load(Ordering::SeqCst),
        0,
        "nothing more to learn"
    );
}

#[test]
fn a_newer_room_contract_closes_the_gate_too() {
    let source = Arc::new(FakePointers::default());
    let watched = test_watched();
    *source.answer.lock().unwrap() = vec![
        (watched[0].pointer_id, Fetch::State(room_state(3))),
        (watched[1].pointer_id, Fetch::State(delegate_state(1))),
    ];
    let watch = Watch::new(watched, source, None);
    settle(&watch);
    assert!(watch.is_stale());
}

/// A kept file that is not a newer River record (forged, older, this build's own, garbage)
/// stops nothing, and is removed.
#[test]
fn a_kept_file_that_proves_nothing_is_ignored() {
    let stranger = SigningKey::from_bytes(&[9u8; 32]);
    for bytes in [
        signed_state(&stranger, "river.chat-delegate", 9, [0x99; 32]),
        delegate_state(1),
        b"garbage".to_vec(),
        signed_state(&test_key(), "river.chat-delegate", 7, [0u8; 32]),
    ] {
        let dir = temp_dir("ignored");
        let path = dir.join("newer-river.river.chat-delegate");
        std::fs::write(&path, &bytes).unwrap();
        let watch = Watch::new(
            test_watched(),
            Arc::new(FakePointers::default()),
            Some(&dir),
        );
        assert!(!watch.is_stale());
        assert!(!path.exists());
    }
}

/// Every answer that is no evidence of a newer River leaves writes on once settled.
#[test]
fn forged_old_or_unreachable_answers_never_stop_writes() {
    let stranger = SigningKey::from_bytes(&[9u8; 32]);
    for fetch in [
        Fetch::Unreachable,
        Fetch::Absent,
        Fetch::State(Vec::new()),
        Fetch::State(signed_state(
            &stranger,
            "river.chat-delegate",
            9,
            [0x99; 32],
        )),
        Fetch::State(signed_state(
            &test_key(),
            "river.chat-delegate",
            9,
            [0u8; 32],
        )),
        Fetch::State(signed_state(
            &test_key(),
            "river.chat-delegate",
            1,
            [0x01; 32],
        )),
    ] {
        let source = Arc::new(FakePointers::default());
        let watched = test_watched();
        answer(&source, &watched, Some(fetch.clone()));
        let watch = Watch::new(watched, source, None);
        settle(&watch);
        assert!(watch.writes_allowed(&pace()), "{fetch:?}");
    }
}

/// A fetch that never answers (a node that hangs) holds writes for `first_wait` only.
#[test]
fn a_silent_node_holds_writes_only_for_the_first_wait() {
    struct Hangs;
    impl PointerSource for Hangs {
        fn fetch(
            &self,
            _: &str,
            _: &GrantToken,
            _: &[ContractInstanceId],
            _: Duration,
        ) -> Vec<(ContractInstanceId, Fetch)> {
            std::thread::sleep(Duration::from_secs(30));
            Vec::new()
        }
    }
    let pace = CheckPace {
        first_wait: Duration::from_millis(100),
        ..pace()
    };
    let watch = Watch::new(test_watched(), Arc::new(Hangs), None);
    let started = Instant::now();
    watch.settle("a", &token(), &pace, &|| false);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(watch.writes_allowed(&pace));
}

/// Checked again periodically while running: a re-key published after the app started is
/// noticed. A check that learned nothing is retried sooner.
#[test]
fn a_re_key_while_running_is_noticed() {
    let source = Arc::new(FakePointers::default());
    let watched = test_watched();
    *source.answer.lock().unwrap() = vec![];
    let watch = Watch::new(watched.clone(), source.clone(), None);
    settle(&watch);
    assert!(watch.writes_allowed(&pace()));
    // Nothing reachable: asked again after `retry_first`, well before `recheck`.
    let deadline = Instant::now() + Duration::from_secs(5);
    while source.fetches.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline, "never asked again");
        watch.poll("a", &token(), &pace());
        std::thread::sleep(Duration::from_millis(5));
    }
    answer(&source, &watched, Some(Fetch::State(delegate_state(4))));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !watch.is_stale() {
        assert!(Instant::now() < deadline, "the re-key was never noticed");
        watch.poll("a", &token(), &pace());
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn nothing_to_watch_never_holds_writes() {
    let source = Arc::new(FakePointers::default());
    let watch = Watch::new(Vec::new(), source.clone(), None);
    assert!(watch.writes_allowed(&pace()));
    assert!(!watch.poll("a", &token(), &pace()));
    assert_eq!(source.fetches.load(Ordering::SeqCst), 0);
}

// ---- the fetch over a connection ---------------------------------------------------------

/// [`NodePointers`] asks each pointer with a plain GET (no code, no subscription) on a
/// connection of its own, and reads each answer by the contract it names.
#[test]
fn node_pointers_reads_each_answer_by_its_contract() {
    use freenet_stdlib::prelude::{ContractKey, WrappedState};
    use tungstenite::Message;
    let watched = test_watched();
    let ids: Vec<ContractInstanceId> = watched.iter().map(|w| w.pointer_id).collect();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        let mut asked = Vec::new();
        while asked.len() < 2 {
            let Ok(Message::Binary(b)) = ws.read() else {
                continue;
            };
            let request: ClientRequest = bincode::deserialize(&b).unwrap();
            let ClientRequest::ContractOp(ContractRequest::Get {
                key,
                return_contract_code,
                subscribe,
                ..
            }) = request
            else {
                panic!("not a GET: {request:?}");
            };
            assert!(!return_contract_code && !subscribe);
            asked.push(key);
        }
        // The delegate's answer first, the room contract's as absent.
        let delegate_key = ContractKey::from_id_and_code(
            asked[1],
            freenet_stdlib::prelude::CodeHash::from_code(&[1u8]),
        );
        let replies: Vec<Result<HostResponse, freenet_stdlib::client_api::ClientError>> = vec![
            Ok(HostResponse::ContractResponse(
                ContractResponse::GetResponse {
                    key: delegate_key,
                    contract: None,
                    state: WrappedState::new(delegate_state(2)),
                },
            )),
            Ok(HostResponse::ContractResponse(ContractResponse::NotFound {
                instance_id: asked[0],
            })),
        ];
        for r in replies {
            ws.send(Message::Binary(bincode::serialize(&r).unwrap().into()))
                .unwrap();
        }
        while ws.read().is_ok() {}
        asked
    });
    let found = NodePointers.fetch(&authority, &token(), &ids, Duration::from_secs(10));
    assert_eq!(found[0], (ids[0], Fetch::Absent));
    assert_eq!(found[1], (ids[1], Fetch::State(delegate_state(2))));
    assert_eq!(server.join().unwrap(), ids);
}

#[test]
fn node_pointers_with_no_node_is_unreachable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    drop(listener);
    let ids: Vec<ContractInstanceId> = test_watched().iter().map(|w| w.pointer_id).collect();
    let found = NodePointers.fetch(&authority, &token(), &ids, Duration::from_millis(200));
    assert!(found.iter().all(|(_, f)| *f == Fetch::Unreachable));
}
