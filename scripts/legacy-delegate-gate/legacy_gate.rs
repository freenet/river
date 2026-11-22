//! River legacy chat-delegate release gate (river#630).
//!
//! NOT compiled in River. `scripts/legacy-delegate-gate.sh` copies this file
//! next to freenet-core's `wasm_runtime/delegate/test.rs` (in a scratch checkout
//! of the core version under test) and declares it as a child module of that
//! test module, so it can use the real `Runtime`, `SecretsStore` and
//! `DelegateStore`, which are not public API. Nothing is committed in
//! freenet-core.
//!
//! Environment (set by the script):
//!   LEGACY_GATE_CORPUS   directory of `<blake3>.wasm` files
//!   LEGACY_GATE_MANIFEST resolved manifest, one entry per line:
//!                        `<blake3> <version> <works|dead> <bytes|array> [reason...]`
//!   LEGACY_GATE_MIN_WORKING  minimum number of `works` entries that must have run
//!
//! Core internals used (keep this list short; each is a way a core refactor breaks the
//! harness): Runtime::build_with_config, ContractStore/DelegateStore/SecretsStore::new,
//! Storage::new, SecretsStore::register_delegate, Runtime::inbound_app_message.
//!
//! For each entry: hash-verify the bytes, instantiate on the core runtime with a
//! real file-backed SecretsStore, probe an empty store (`value: None`), store a
//! secret through River's CBOR StoreRequest, read it back, then drop the runtime,
//! reopen the same data dir and read it again.

use super::*;
use ciborium::Value;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} not set; run via scripts/legacy-delegate-gate.sh"))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Enc {
    Bytes,
    Array,
}

fn enc_bytes(enc: Enc, b: &[u8]) -> Value {
    match enc {
        Enc::Bytes => Value::Bytes(b.to_vec()),
        Enc::Array => Value::Array(b.iter().map(|x| Value::Integer((*x).into())).collect()),
    }
}

fn request(variant: &str, enc: Enc, fields: &[(&str, &[u8])]) -> Vec<u8> {
    let inner = Value::Map(
        fields
            .iter()
            .map(|(k, v)| (Value::Text((*k).to_string()), enc_bytes(enc, v)))
            .collect(),
    );
    let outer = Value::Map(vec![(Value::Text(variant.to_string()), inner)]);
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&outer, &mut buf).unwrap();
    buf
}

/// Decode a byte field that may be a CBOR byte string or an integer array.
fn as_bytes(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Bytes(b) => Some(b.clone()),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::Integer(i) => u8::try_from(i128::from(*i)).ok(),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Send one River request; return the `value` of the GetResponse (None = absent).
fn call(
    rt: &mut Runtime,
    key: &DelegateKey,
    payload: Vec<u8>,
) -> Result<Option<Option<Vec<u8>>>, String> {
    let origin = MessageOrigin::WebApp(ContractInstanceId::new([7u8; 32]));
    let params: Parameters = vec![].into();
    let out = rt
        .inbound_app_message(
            key,
            &params,
            Some(&origin),
            None,
            vec![InboundDelegateMsg::ApplicationMessage(ApplicationMessage::new(payload))],
        )
        .map_err(|e| format!("host error: {e}"))?;
    let mut got = None;
    for m in out {
        if let OutboundDelegateMsg::ApplicationMessage(am) = m {
            let v: Value = ciborium::de::from_reader(am.payload.as_slice())
                .map_err(|e| format!("reply is not CBOR: {e}"))?;
            if let Value::Map(outer) = v {
                for (k, body) in outer {
                    if k == Value::Text("GetResponse".into()) {
                        if let Value::Map(fields) = body {
                            for (fk, fv) in fields {
                                if fk == Value::Text("value".into()) {
                                    got = Some(if fv == Value::Null {
                                        None
                                    } else {
                                        Some(as_bytes(&fv).ok_or_else(|| {
                                            format!("reply: `value` is neither null nor bytes: {fv:?}")
                                        })?)
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(got)
}

async fn open_runtime(dir: &std::path::Path) -> Result<Runtime, String> {
    use crate::contract::storages::Storage;
    let db = Storage::new(dir).await.map_err(|e| format!("storage: {e}"))?;
    let contract_store = ContractStore::new(dir.join("c"), 10_000, db.clone()).map_err(|e| format!("{e}"))?;
    let delegate_store = DelegateStore::new(dir.join("d"), 10_000, db.clone()).map_err(|e| format!("{e}"))?;
    let secret_store = SecretsStore::new(dir.join("s"), Default::default(), db).map_err(|e| format!("{e}"))?;
    Runtime::build_with_config(contract_store, delegate_store, secret_store, false, Default::default())
        .map_err(|e| format!("runtime: {e}"))
}

/// Run the full tier 1-3 exercise on one WASM. Ok(()) means every step passed.
async fn exercise(bytes: &[u8], enc: Enc) -> Result<(), String> {
    exercise_with(bytes, enc, false).await
}

/// `fresh_reopen` is a negative control: tier 3 reopens an EMPTY data dir instead of
/// the one written in tier 2, so the persistence check must go red.
async fn exercise_with(bytes: &[u8], enc: Enc, fresh_reopen: bool) -> Result<(), String> {
    let temp = tempfile::tempdir().expect("tempdir");
    let fresh = tempfile::tempdir().expect("tempdir");
    let delegate = DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(Delegate::from((
        &bytes.to_vec().into(),
        &vec![].into(),
    ))));
    let key = delegate.key().clone();
    let cipher_key = XChaCha20Poly1305::generate_key(&mut OsRng);
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let register = |rt: &mut Runtime| {
        let _ = rt
            .secret_store
            .register_delegate(key.clone(), XChaCha20Poly1305::new(&cipher_key), nonce);
    };

    let ckey: &[u8] = b"rooms_data";
    let value: &[u8] = b"legacy-gate-secret";
    {
        let mut rt = open_runtime(temp.path()).await?;
        rt.delegate_store
            .store_delegate(delegate.clone())
            .map_err(|e| format!("store_delegate: {e}"))?;
        register(&mut rt);
        // Tier 2a: empty-store probe, exactly what the migration walk sends.
        match call(&mut rt, &key, request("GetRequest", enc, &[("key", ckey)]))? {
            Some(None) => {}
            other => return Err(format!("empty-store probe: expected value None, got {other:?}")),
        }
        // Tier 2b: StoreRequest then GetRequest.
        call(&mut rt, &key, request("StoreRequest", enc, &[("key", ckey), ("value", value)]))?;
        match call(&mut rt, &key, request("GetRequest", enc, &[("key", ckey)]))? {
            Some(Some(v)) if v == value => {}
            other => return Err(format!("round trip: expected stored value back, got {other:?}")),
        }
    }
    // Tier 3: drop everything, reopen the same data dir, read again.
    {
        let dir = if fresh_reopen { fresh.path() } else { temp.path() };
        let mut rt = open_runtime(dir).await?;
        rt.delegate_store
            .store_delegate(delegate.clone())
            .map_err(|e| format!("store_delegate (reopen): {e}"))?;
        register(&mut rt);
        match call(&mut rt, &key, request("GetRequest", enc, &[("key", ckey)]))? {
            Some(Some(v)) if v == value => {}
            other => return Err(format!("after reopen: expected stored value, got {other:?}")),
        }
    }
    Ok(())
}

struct Entry {
    hash: String,
    version: String,
    works: bool,
    enc: Enc,
}

fn manifest() -> Vec<Entry> {
    let text = std::fs::read_to_string(env("LEGACY_GATE_MANIFEST")).expect("manifest unreadable");
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            assert!(f.len() >= 4, "bad manifest line: {l}");
            Entry {
                hash: f[0].to_string(),
                version: f[1].to_string(),
                works: match f[2] {
                    "works" => true,
                    "dead" => false,
                    x => panic!("bad expectation {x}"),
                },
                enc: match f[3] {
                    "bytes" => Enc::Bytes,
                    "array" => Enc::Array,
                    x => panic!("bad encoding {x}"),
                },
            }
        })
        .collect()
}

fn verify_hash(bytes: &[u8], expected: &str) -> Result<(), String> {
    let actual = blake3::hash(bytes).to_hex().to_string();
    if actual != expected {
        return Err(format!("HASH MISMATCH: file hashes to {actual}, registry says {expected}"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_chat_delegate_gate() {
    let corpus = env("LEGACY_GATE_CORPUS");
    let min_working: usize = env("LEGACY_GATE_MIN_WORKING").parse().expect("MIN_WORKING");
    let entries = manifest();
    let mut failures = Vec::new();
    let mut working_ran = 0usize;
    let mut dead_ran = 0usize;
    for e in &entries {
        let path = std::path::Path::new(&corpus).join(format!("{}.wasm", e.hash));
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(err) => {
                failures.push(format!("{}: corpus file missing: {err}", e.version));
                continue;
            }
        };
        if let Err(m) = verify_hash(&bytes, &e.hash) {
            failures.push(format!("{}: {m}", e.version));
            continue;
        }
        let res = exercise(&bytes, e.enc).await;
        eprintln!("LEGACY-GATE {} works={} -> {:?}", e.version, e.works, res);
        match (e.works, res) {
            (true, Ok(())) => working_ran += 1,
            (true, Err(m)) => failures.push(format!("{}: expected to work, FAILED: {m}", e.version)),
            // A dead delegate must fail while RUNNING (instantiate/call). A failure in
            // our own setup (storage, runtime build, delegate store) says nothing
            // about the delegate and must not be counted as "dead as expected".
            (false, Err(m)) if m.starts_with("host error:") => dead_ran += 1,
            (false, Err(m)) => failures.push(format!(
                "{}: listed dead, but failed outside the delegate (harness/setup problem?): {m}",
                e.version
            )),
            (false, Ok(())) => failures.push(format!(
                "{}: listed as dead in manifest.txt but now WORKS; update the manifest (stale allowlist)",
                e.version
            )),
        }
    }
    if working_ran < min_working {
        failures.push(format!("only {working_ran} working generations ran, need at least {min_working}"));
    }
    eprintln!("LEGACY-GATE summary: {working_ran} working ok, {dead_ran} dead as expected, {} failures", failures.len());
    assert!(failures.is_empty(), "legacy delegate gate failed:\n  {}", failures.join("\n  "));
}

/// Falsification: the harness must go red on a broken WASM, or a green run
/// proves nothing. Mangles the first working entry in three ways.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_gate_goes_red_on_mangled_wasm() {
    let corpus = env("LEGACY_GATE_CORPUS");
    let e = manifest().into_iter().find(|e| e.works).expect("no working entry");
    let good = std::fs::read(std::path::Path::new(&corpus).join(format!("{}.wasm", e.hash))).unwrap();
    exercise(&good, e.enc).await.expect("control: unmangled WASM must pass");

    let mut truncated = good.clone();
    truncated.truncate(good.len() / 2);
    assert!(exercise(&truncated, e.enc).await.is_err(), "truncated WASM passed");

    // The V9 corruption class: a UTF-8 replacement char smeared into the header region.
    let mut smeared = good.clone();
    for (i, b) in [0xefu8, 0xbf, 0xbd].iter().enumerate() {
        smeared[12 + i] = *b;
    }
    assert!(exercise(&smeared, e.enc).await.is_err(), "mangled WASM passed");

    // The gate's real hash guard goes red on bytes that disagree with the registry.
    assert!(verify_hash(&truncated, &e.hash).is_err(), "hash guard accepted wrong bytes");
    assert!(verify_hash(&good, &e.hash).is_ok(), "hash guard rejected the right bytes");

    // Persistence control: reopening an empty data dir must NOT return the stored
    // secret, so tier 3 genuinely reads what tier 2 wrote to disk.
    let err = exercise_with(&good, e.enc, true).await.expect_err("fresh reopen passed");
    assert!(err.starts_with("after reopen"), "fresh reopen failed in the wrong place: {err}");
}
