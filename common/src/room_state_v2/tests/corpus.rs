//! Writes state corpora for freenet-core's merge verifier (`fdev
//! verify-merge`), into the directory RIVER_V2_CORPUS. Each corpus is a
//! directory with `params.cbor` (the room parameters), `sNNN.cbor` states,
//! and `tNNN-base.cbor` / `tNNN-result.cbor` transitions (a peer held base,
//! applied an update, reached result). Run by hand:
//!
//! `RIVER_V2_CORPUS=<dir> RIVER_V1_STATE=<v1.cbor> RIVER_V1_OWNER=<base58>
//!  cargo test -p river-core --lib write_fdev_corpora -- --ignored`

use super::builder::{owner as builder_owner, sk};
use super::families::{rand_cfg, rand_universe_l5};
use super::harness::Rng;
use crate::room_state::ChatRoomParametersV1;
use crate::room_state_v2::bench::{largest_state, owner_key, Caps};
use crate::room_state_v2::encoding::{to_cbor, Key};
use crate::room_state_v2::records::Record;
use crate::room_state_v2::{merge, normalize, RoomStateV2, Verifier};
use std::path::{Path, PathBuf};

struct Out {
    dir: PathBuf,
    states: usize,
    transitions: usize,
    params: Vec<u8>,
    bundle: Bundle,
}

/// freenet-core's `conformance::bundle::ReplayBundle`, field for field, so
/// bincode 1 encodes it identically. The bundle is how deltas, summaries
/// and delta-carrying transitions reach the verifier.
#[derive(serde::Serialize, Default)]
struct Bundle {
    schema_version: u16,
    code: Option<Vec<u8>>,
    code_hash: Option<[u8; 32]>,
    parameters: Vec<u8>,
    /// Always `None`: a `ContractInstanceId` never needs encoding here.
    instance: Option<()>,
    states: Vec<Vec<u8>>,
    deltas: Vec<Vec<u8>>,
    summaries: Vec<Vec<u8>>,
    transitions: Vec<Transition>,
    related: Vec<((), Vec<u8>)>,
    note: Option<String>,
}

#[derive(serde::Serialize)]
struct Transition {
    base_state: Vec<u8>,
    delta: Option<Vec<u8>>,
    incoming_state: Option<Vec<u8>>,
    summary: Option<Vec<u8>>,
    result_state: Vec<u8>,
}

impl Drop for Out {
    fn drop(&mut self) {
        self.bundle.schema_version = 1;
        self.bundle.parameters = self.params.clone();
        self.bundle.note = Some("room-state-v2 corpus".into());
        let mut bytes = b"FRNTCNF1".to_vec();
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend(bincode::serialize(&self.bundle).unwrap());
        std::fs::write(self.dir.with_extension("bundle"), bytes).unwrap();
    }
}

impl Out {
    fn new(root: &Path, name: &str, owner: ed25519_dalek::VerifyingKey) -> Out {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let p = ChatRoomParametersV1 { owner };
        std::fs::write(dir.join("params.cbor"), to_cbor(&p)).unwrap();
        Out {
            dir,
            states: 0,
            transitions: 0,
            params: to_cbor(&p),
            bundle: Bundle::default(),
        }
    }
    fn state_bytes(&mut self, bytes: Vec<u8>) {
        self.bundle.states.push(bytes.clone());
        std::fs::write(self.dir.join(format!("s{:03}.cbor", self.states)), bytes).unwrap();
        self.states += 1;
    }
    fn state(&mut self, s: &RoomStateV2) {
        self.state_bytes(s.to_bytes());
    }
    fn transition(&mut self, base: &RoomStateV2, result: &RoomStateV2) {
        self.bundle.transitions.push(Transition {
            base_state: base.to_bytes(),
            delta: None,
            incoming_state: Some(result.to_bytes()),
            summary: None,
            result_state: result.to_bytes(),
        });
        let n = self.transitions;
        std::fs::write(self.dir.join(format!("t{n:03}-base.cbor")), base.to_bytes()).unwrap();
        std::fs::write(
            self.dir.join(format!("t{n:03}-result.cbor")),
            result.to_bytes(),
        )
        .unwrap();
        self.transitions += 1;
    }
}

/// `f` of a random subset of `recs` (each kept with probability p/1000).
fn subset(o: &Key, recs: &[Record], rng: &mut Rng, p: u64) -> RoomStateV2 {
    let pick: Vec<Record> = recs
        .iter()
        .filter(|_| rng.below(1000) < p)
        .cloned()
        .collect();
    normalize(o, pick, &mut Verifier::new()).0
}

/// States that are histories of one record universe: subsets of it (peers
/// that saw part), and transitions from one to its merge with another.
fn histories(out: &mut Out, o: &Key, recs: &[Record], seed: u64, n: usize) {
    let mut rng = Rng(seed);
    let mut states = vec![];
    for i in 0..n {
        let p = 300 + (i as u64 * 677) % 650;
        let s = subset(o, recs, &mut rng, p);
        out.state(&s);
        states.push(s);
    }
    for i in 0..states.len().min(12) {
        let j = (i * 7 + 3) % states.len();
        let m = merge(o, &states[i], &states[j], &mut Verifier::new());
        out.transition(&states[i], &m);
    }
    // Deltas as a peer computes them (from a sender's state against a
    // receiver's summary), each with the receiver it was computed for, and
    // the summaries.
    for i in 0..states.len().min(10) {
        out.bundle.summaries.push(to_cbor(&states[i].summarize()));
        for k in 1..=2 {
            let j = (i + k * 5) % states.len();
            let (sender, receiver) = (&states[j], &states[i]);
            let Some(d) = sender.delta(&receiver.summarize()) else {
                continue;
            };
            let result =
                crate::room_state_v2::apply_delta(o, receiver, d.clone(), &mut Verifier::new())
                    .unwrap();
            let db = to_cbor(&d);
            out.bundle.deltas.push(db.clone());
            out.bundle.transitions.push(Transition {
                base_state: receiver.to_bytes(),
                delta: Some(db),
                incoming_state: None,
                summary: Some(to_cbor(&receiver.summarize())),
                result_state: result.to_bytes(),
            });
        }
    }
}

#[test]
#[ignore = "writes corpora for fdev verify-merge into RIVER_V2_CORPUS"]
fn write_fdev_corpora() {
    let root = PathBuf::from(std::env::var("RIVER_V2_CORPUS").unwrap());

    // (i) The Official room converted (owner never converts: its records are
    // all real), and histories of it.
    if let (Ok(path), Ok(owner_b58)) = (
        std::env::var("RIVER_V1_STATE"),
        std::env::var("RIVER_V1_OWNER"),
    ) {
        use crate::room_state::ChatRoomStateV1;
        use crate::room_state_v2::convert::{legacy_configuration, legacy_records};
        let v1: ChatRoomStateV1 =
            ciborium::de::from_reader(&std::fs::read(path).unwrap()[..]).unwrap();
        let b = bs58::decode(owner_b58).into_vec().unwrap();
        let ovk = ed25519_dalek::VerifyingKey::from_bytes(&b.try_into().unwrap()).unwrap();
        let o = Key::from(&ovk);
        let mut recs = vec![legacy_configuration(&o, &v1)];
        recs.extend(legacy_records(&o, &v1).records);
        let mut out = Out::new(&root, "official", ovk);
        let full = normalize(&o, recs.clone(), &mut Verifier::new()).0;
        out.state(&full);
        histories(&mut out, &o, &recs, 1, 24);
    }

    // (ii) Synthetic rooms whose owner key is known.
    // The largest valid states at the ceilings (v2 and legacy-heavy), two
    // histories each, and their subsets.
    let bo = owner_key();
    let bok = Key::from(&bo.verifying_key());
    let mut out = Out::new(&root, "ceilings", bo.verifying_key());
    for caps in [Caps::ceilings(), Caps::ceilings_legacy()] {
        let (a, _) = largest_state(caps, 0);
        let (b, _) = largest_state(caps, 1);
        out.state(&a);
        out.state(&b);
        let mut all = a.records.clone();
        all.extend(b.records.iter().cloned());
        histories(&mut out, &bok, &all, 2 + caps.legacy as u64, 8);
    }

    // Random universes with every record kind (owner key k0 of the
    // builder), several histories each.
    let ok = builder_owner();
    let mut out = Out::new(&root, "universes", sk(0).verifying_key());
    let mut adv = Out::new(&root, "adversarial", sk(0).verifying_key());
    for fi in 0..6 {
        let mut rng = Rng(36 * 100_000 + fi);
        let keys = 4 + rng.below(2) as usize;
        let (c0, v1, v2) = (
            rand_cfg(&mut rng, keys),
            rand_cfg(&mut rng, keys),
            rand_cfg(&mut rng, keys),
        );
        let b = rand_universe_l5(&mut rng, keys, c0, v1, v2);
        histories(&mut out, &ok, &b.recs, 100 + fi, 10);

        // Adversarial inputs from the same universe.
        let s = normalize(&ok, b.recs.clone(), &mut Verifier::new()).0;
        if s.records.len() < 3 {
            continue;
        }
        // Reordered records (not canonical).
        let mut r = s.clone();
        r.records.reverse();
        adv.state_bytes(to_cbor(&r));
        // A duplicated record.
        let mut d = s.clone();
        d.records.insert(1, d.records[0].clone());
        adv.state_bytes(to_cbor(&d));
        // Over cap / not a fixpoint: every record of the universe, sorted.
        adv.state(&RoomStateV2::from_records(b.recs.clone()));
        // Replayed older signed records: the state plus every superseded
        // configuration and clock (f drops them; the raw union is invalid).
        let old: Vec<Record> = b
            .recs
            .iter()
            .filter(|r| matches!(r, Record::Configuration(_) | Record::Clock(_)))
            .cloned()
            .collect();
        let mut replay = s.records.clone();
        replay.extend(old);
        adv.state(&RoomStateV2::from_records(replay.clone()));
        // ... and the valid states it should merge with.
        adv.state(&s);
        adv.state(&normalize(&ok, replay, &mut Verifier::new()).0);
    }
    println!("wrote corpora to {}", root.display());
}
