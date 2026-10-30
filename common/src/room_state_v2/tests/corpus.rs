//! Writes state corpora for freenet-core's merge verifier (`fdev
//! verify-merge`), into the directory RIVER_V2_CORPUS. Each corpus is a
//! directory with `params.cbor` (the room parameters), `sNNN.cbor` states,
//! and `tNNN-base.cbor` / `tNNN-result.cbor` transitions (a peer held base,
//! applied an update, reached result). Run by hand:
//!
//! `RIVER_V2_CORPUS=<dir> RIVER_V2_WASM=<room_contract_v2.wasm>
//!  RIVER_V1_STATE=<v1.cbor> RIVER_V1_OWNER=<base58>
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
    /// The room owner, to check what is written.
    owner: Key,
    /// Whether every state written must validate (all but the adversarial
    /// corpus).
    check: bool,
    /// States written with the legacy window closed and a legacy record.
    closed_with_legacy: usize,
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
        // A bundle names its contract by code hash (RIVER_V2_WASM, the
        // contract the corpus is for); the WASM itself is passed to fdev.
        if let Ok(w) = std::env::var("RIVER_V2_WASM") {
            self.bundle.code_hash = Some(*blake3::hash(&std::fs::read(w).unwrap()).as_bytes());
        }
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
            owner: Key::from(&owner),
            check: true,
            closed_with_legacy: 0,
        }
    }
    /// A corpus of states `validate_state` refuses.
    fn unchecked(mut self) -> Out {
        self.check = false;
        self
    }
    fn state_bytes(&mut self, bytes: Vec<u8>) {
        if self.check {
            let s = crate::room_state_v2::validate_state(&self.owner, &bytes, &mut ver())
                .unwrap_or_else(|e| {
                    panic!("{}: a corpus state is invalid: {e:?}", self.dir.display())
                });
            let closed = s
                .records
                .iter()
                .any(|r| matches!(r, Record::Configuration(c) if !c.payload.legacy_window_open));
            if closed && s.records.iter().any(|r| r.legacy_kind().is_some()) {
                self.closed_with_legacy += 1;
            }
        }
        self.bundle.states.push(bytes.clone());
        std::fs::write(self.dir.join(format!("s{:03}.cbor", self.states)), bytes).unwrap();
        self.states += 1;
    }
    fn state(&mut self, s: &RoomStateV2) {
        // The state with no records is trivial, and fdev reads its one form
        // (empty bytes) as no input: leave it out, as a state and as either
        // end of a transition.
        if !s.records.is_empty() {
            self.state_bytes(s.to_bytes());
        }
    }
    fn transition(&mut self, base: &RoomStateV2, result: &RoomStateV2) {
        // A step that changes nothing, or from or to the empty state, says
        // nothing about a path.
        if base.records.is_empty() || result.records.is_empty() || base == result {
            return;
        }
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

thread_local! {
    /// Records every verifier these helpers make trusts: owner records signed
    /// by a stand-in for an owner whose key is not here.
    static TRUSTED: std::cell::RefCell<Option<(Key, Vec<crate::room_state_v2::encoding::Digest>)>> =
        const { std::cell::RefCell::new(None) };
}

fn trust_in_this_thread(o: Key, d: Vec<crate::room_state_v2::encoding::Digest>) {
    TRUSTED.with(|t| *t.borrow_mut() = Some((o, d)));
}

fn ver() -> Verifier {
    let mut v = Verifier::new();
    TRUSTED.with(|t| {
        if let Some((o, d)) = &*t.borrow() {
            v.trust(o, d.iter().copied());
        }
    });
    v
}

/// The Official-shaped room converted by its owner (the bot's deputies
/// confirmed, the bot protected), and histories of it.
fn official_corpus(root: &Path) {
    use crate::room_state_v2::convert::{legacy_records, owner_payloads};
    let f = crate::room_state_v2::official_shaped::official_shaped(1, 1.0);
    let ovk = f.owner.verifying_key();
    let o = Key::from(&ovk);
    let legacy = legacy_records(&o, &f.v1).records;
    let deputies: Vec<Key> = f.deputies.iter().map(Key::from).collect();
    let p = owner_payloads(
        &o,
        &f.v1,
        &legacy,
        24 * 300,
        &deputies,
        &[Key::from(&f.bot)],
        &[],
    )
    .unwrap()
    .payloads;
    let mut recs = p.sign(&f.owner);
    recs.extend(legacy);
    let mut out = Out::new(root, "official", ovk);
    let full = normalize(&o, recs.clone(), &mut ver()).0;
    out.state(&full);
    histories(&mut out, &o, &recs, 1, 24);
}

/// `f` of a random subset of `recs` (each kept with probability p/1000).
fn subset(o: &Key, recs: &[Record], rng: &mut Rng, p: u64) -> RoomStateV2 {
    let pick: Vec<Record> = recs
        .iter()
        .filter(|_| rng.below(1000) < p)
        .cloned()
        .collect();
    normalize(o, pick, &mut ver()).0
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
        let m = merge(o, &states[i], &states[j], &mut ver());
        out.transition(&states[i], &m);
    }
    // Deltas as a peer computes them (from a sender's state against a
    // receiver's summary), each with the receiver it was computed for, and
    // the summaries.
    for i in 0..states.len().min(10) {
        out.bundle.summaries.push(to_cbor(&states[i].summarize()));
        for k in 1..=2 {
            // Two senders other than the receiver, so the receiver's two
            // deltas share its base (with 10 states, i + 10 is i itself).
            let n = states.len();
            let j = match (i + k * 5) % n {
                j if j == i => (i + 1) % n,
                j => j,
            };
            let (sender, receiver) = (&states[j], &states[i]);
            let Some(d) = sender.delta(&receiver.summarize()) else {
                continue;
            };
            let result =
                crate::room_state_v2::apply_delta(o, receiver, d.clone(), &mut ver()).unwrap();
            let db = to_cbor(&d);
            out.bundle.deltas.push(db.clone());
            if receiver.records.is_empty() || result.records.is_empty() || *receiver == result {
                continue;
            }
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

    // (i) The Official-shaped room, converted by its owner.
    official_corpus(&root);

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

    universes_corpora(&root);
    windowed_corpus(&root);
    println!("wrote corpora to {}", root.display());
}

/// Random universes with every record kind (owner key k0 of the builder),
/// several histories each, and adversarial inputs from them.
fn universes_corpora(root: &Path) {
    let ok = builder_owner();
    let mut out = Out::new(root, "universes", sk(0).verifying_key());
    let mut adv = Out::new(root, "adversarial", sk(0).verifying_key()).unchecked();
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
        let s = normalize(&ok, b.recs.clone(), &mut ver()).0;
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
        adv.state(&normalize(&ok, replay, &mut ver()).0);
    }
    assert!(out.transitions >= 20, "{} transitions", out.transitions);
}

/// Legacy-heavy random universes whose configurations close the window at
/// varied hours, so the corpus holds states with the window closed and
/// legacy records (rostered ones count, legacy times capped at the close).
fn windowed_corpus(root: &Path) {
    let ok = builder_owner();
    let mut out = Out::new(root, "windowed", sk(0).verifying_key());
    for fi in 0..6 {
        histories(&mut out, &ok, &windowed_universe(fi).recs, 200 + fi, 10);
    }
    assert!(
        out.closed_with_legacy > 0,
        "no state with the window closed and a legacy record"
    );
}

/// The windowed corpus's universe `fi`.
fn windowed_universe(fi: u64) -> super::builder::Builder {
    use super::families::rand_universe_legacy;
    let mut rng = Rng(37 * 100_000 + fi);
    let keys = 4 + rng.below(2) as usize;
    let cfg = |rng: &mut Rng, open: bool| {
        let mut c = rand_cfg(rng, keys);
        c.window_open = open;
        c.closed_at = rng.below(48) as u32;
        c
    };
    let (c0, v1, v2) = (
        cfg(&mut rng, true),
        cfg(&mut rng, false),
        cfg(&mut rng, false),
    );
    rand_universe_legacy(&mut rng, keys, c0, v1, v2)
}

/// The same heal check as [`delta_permutation_pairs_heal`] over the windowed
/// corpus's histories (closed windows, legacy records).
#[test]
fn delta_permutation_pairs_heal_in_the_windowed_corpus() {
    let ok = builder_owner();
    let mut tot = (0, 0, 0);
    for fi in 0..6 {
        let b = windowed_universe(fi);
        let mut srng = Rng(200 + fi);
        let states: Vec<RoomStateV2> = (0..10)
            .map(|i| subset(&ok, &b.recs, &mut srng, 300 + (i as u64 * 677) % 650))
            .collect();
        let (p, d, h) = delta_pairs_heal(&ok, &states);
        tot = (tot.0 + p, tot.1 + d, tot.2 + h);
    }
    println!(
        "delta pairs {}, orders differ {}, healed {}",
        tot.0, tot.1, tot.2
    );
    assert!(tot.1 > 0, "no pair exercised a cap");
    assert_eq!(tot.1, tot.2);
}

/// The corpus writers check what they write: every state validates (but
/// the adversarial ones), every transition changes its base, enough
/// transitions are written, and a state with the window closed and legacy
/// records exists.
#[test]
fn the_corpora_check_themselves() {
    let root = std::env::temp_dir().join(format!("river-v2-corpus-{}", std::process::id()));
    universes_corpora(&root);
    windowed_corpus(&root);
    std::fs::remove_dir_all(&root).unwrap();
}

/// For every receiver and pair of deltas computed against its summary, the
/// two application orders; returns (pairs, orders that differ, differing
/// pairs that heal: merging the two results, then re-merging every input,
/// reaches one state no further merge changes).
fn delta_pairs_heal(o: &Key, states: &[RoomStateV2]) -> (usize, usize, usize) {
    let (mut pairs, mut differ, mut healed) = (0, 0, 0);
    let ap = |s: &RoomStateV2, d: &Vec<Record>| {
        crate::room_state_v2::apply_delta(o, s, d.clone(), &mut ver()).unwrap()
    };
    for base in states {
        let sum = base.summarize();
        let ds: Vec<Vec<Record>> = states.iter().filter_map(|s| s.delta(&sum)).collect();
        for x in 0..ds.len() {
            for y in x + 1..ds.len() {
                pairs += 1;
                let l = ap(&ap(base, &ds[x]), &ds[y]);
                let r = ap(&ap(base, &ds[y]), &ds[x]);
                if l == r {
                    continue;
                }
                differ += 1;
                let mut conv = merge(o, &l, &r, &mut ver());
                assert_eq!(conv, merge(o, &r, &l, &mut ver()));
                for _ in 0..4 {
                    let before = conv.clone();
                    for input in [&l, &r, base] {
                        conv = merge(o, &conv, input, &mut ver());
                    }
                    for d in [&ds[x], &ds[y]] {
                        conv = ap(&conv, d);
                    }
                    if conv == before {
                        break;
                    }
                }
                let fixed = [&l, &r, base]
                    .iter()
                    .all(|s| merge(o, &conv, s, &mut ver()) == conv);
                healed += fixed as usize;
            }
        }
    }
    (pairs, differ, healed)
}

/// Strict delta permutation invariance fails for a contract with caps: a
/// cap can evict in one order what the other order keeps. Over the random
/// universes, every pair whose orders differ heals.
#[test]
fn delta_permutation_pairs_heal() {
    let ok = builder_owner();
    let mut tot = (0, 0, 0);
    for fi in 0..6 {
        let mut rng = Rng(36 * 100_000 + fi);
        let keys = 4 + rng.below(2) as usize;
        let (c0, v1, v2) = (
            rand_cfg(&mut rng, keys),
            rand_cfg(&mut rng, keys),
            rand_cfg(&mut rng, keys),
        );
        let b = rand_universe_l5(&mut rng, keys, c0, v1, v2);
        let mut srng = Rng(100 + fi);
        let states: Vec<RoomStateV2> = (0..10)
            .map(|i| subset(&ok, &b.recs, &mut srng, 300 + (i as u64 * 677) % 650))
            .collect();
        let (p, d, h) = delta_pairs_heal(&ok, &states);
        tot = (tot.0 + p, tot.1 + d, tot.2 + h);
    }
    println!(
        "delta pairs {}, orders differ {}, healed {}",
        tot.0, tot.1, tot.2
    );
    assert!(tot.1 > 0, "no pair exercised a cap");
    assert_eq!(tot.1, tot.2);
}

/// The same over histories of a converted live room (RIVER_V1_STATE).
#[test]
#[ignore = "reads RIVER_V1_STATE and RIVER_V1_OWNER"]
fn delta_permutation_pairs_heal_on_live_room() {
    use crate::room_state::ChatRoomStateV1;
    use crate::room_state_v2::convert::{legacy_records, owner_payloads};
    let v1: ChatRoomStateV1 = ciborium::de::from_reader(
        &std::fs::read(std::env::var("RIVER_V1_STATE").unwrap()).unwrap()[..],
    )
    .unwrap();
    let b = bs58::decode(std::env::var("RIVER_V1_OWNER").unwrap())
        .into_vec()
        .unwrap();
    let o = crate::room_state_v2::encoding::Bytes32(b.try_into().unwrap());
    // The owner's records, signed by a stand-in and trusted (its key is not
    // here); no deputies confirmed.
    let legacy = legacy_records(&o, &v1).records;
    let p = owner_payloads(&o, &v1, &legacy, 1, &[], &[], &[])
        .unwrap()
        .payloads;
    let mut recs = p.sign(&ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));
    trust_in_this_thread(o, recs.iter().map(|r| r.digest()).collect());
    recs.extend(legacy);
    let mut rng = Rng(1);
    let states: Vec<RoomStateV2> = (0..12)
        .map(|i| subset(&o, &recs, &mut rng, 300 + (i as u64 * 677) % 650))
        .collect();
    let (p, d, h) = delta_pairs_heal(&o, &states);
    println!("delta pairs {p}, orders differ {d}, healed {h}");
    assert_eq!(d, h);
}

/// The same over the ceilings corpus's histories: the largest states at the
/// ceilings (v2 and legacy-heavy), where every cap is full. Every record's
/// signature is trusted (the builder signed them; this is about the caps).
#[test]
#[ignore = "the largest states; minutes in a release build"]
fn delta_permutation_pairs_heal_at_the_ceilings() {
    let o = Key::from(&owner_key().verifying_key());
    let mut tot = (0, 0, 0);
    for caps in [Caps::ceilings(), Caps::ceilings_legacy()] {
        let (a, _) = largest_state(caps, 0);
        let (b, _) = largest_state(caps, 1);
        let mut all = a.records.clone();
        all.extend(b.records.iter().cloned());
        trust_in_this_thread(o, all.iter().map(|r| r.digest()).collect());
        let mut rng = Rng(2 + caps.legacy as u64);
        let states: Vec<RoomStateV2> = (0..8)
            .map(|i| subset(&o, &all, &mut rng, 300 + (i as u64 * 677) % 650))
            .collect();
        let (p, d, h) = delta_pairs_heal(&o, &states);
        println!(
            "legacy {}: delta pairs {p}, orders differ {d}, healed {h}",
            caps.legacy
        );
        tot = (tot.0 + p, tot.1 + d, tot.2 + h);
    }
    assert!(tot.1 > 0, "no pair exercised a cap");
    assert_eq!(tot.1, tot.2);
}

/// The merge laws on states at the ceilings, where every cap binds at
/// once: for the largest states (v2 and legacy-heavy, two variants each)
/// and histories of their union, every pair commutes, merges to a state
/// that validates and absorbs a repeat of either side, and every triple
/// associates or heals. Signatures are trusted (the builder signed every
/// record; this is about the caps).
#[test]
#[ignore = "the largest states; minutes in a release build"]
fn the_laws_hold_at_the_ceilings() {
    use crate::room_state_v2::validate_state;
    let o = Key::from(&owner_key().verifying_key());
    let (mut pairs, mut triples, mut strict, mut healed) = (0, 0, 0, 0);
    for caps in [Caps::ceilings(), Caps::ceilings_legacy()] {
        let (a, _) = largest_state(caps, 0);
        let (b, _) = largest_state(caps, 1);
        let mut all = a.records.clone();
        all.extend(b.records.iter().cloned());
        trust_in_this_thread(o, all.iter().map(|r| r.digest()).collect());
        let mut rng = Rng(5 + caps.legacy as u64);
        let mut states = vec![a.clone(), b.clone()];
        states.extend((0..6).map(|i| subset(&o, &all, &mut rng, 300 + (i as u64 * 677) % 650)));
        let m = |x: &RoomStateV2, y: &RoomStateV2| merge(&o, x, y, &mut ver());
        for i in 0..states.len() {
            for j in i + 1..states.len() {
                let (x, y) = (&states[i], &states[j]);
                let xy = m(x, y);
                assert_eq!(xy, m(y, x), "commutativity {i} {j}");
                assert!(
                    validate_state(&o, &xy.to_bytes(), &mut ver()).is_ok(),
                    "validity {i} {j}"
                );
                assert_eq!(m(&xy, y), xy, "absorption {i} {j}");
                assert_eq!(m(&xy, x), xy, "absorption {j} {i}");
                pairs += 1;
            }
        }
        for i in 0..states.len() {
            for j in 0..states.len() {
                for k in 0..states.len() {
                    if i == j || j == k || i == k {
                        continue;
                    }
                    let (x, y, z) = (&states[i], &states[j], &states[k]);
                    let l = m(&m(x, y), z);
                    let r = m(x, &m(y, z));
                    triples += 1;
                    if l == r {
                        continue;
                    }
                    strict += 1;
                    // Heal: merge the two groupings, then every input
                    // again, until nothing changes; the result must then
                    // absorb every input and grouping.
                    let mut c = m(&l, &r);
                    for _ in 0..4 {
                        let before = c.clone();
                        for s in [x, y, z, &l, &r] {
                            c = m(&c, s);
                        }
                        if c == before {
                            break;
                        }
                    }
                    assert!(
                        [x, y, z, &l, &r].iter().all(|s| m(&c, s) == c),
                        "triple {i} {j} {k} does not heal"
                    );
                    healed += 1;
                }
            }
        }
    }
    println!(
        "pairs {pairs}, triples {triples}, not strictly associative {strict}, healed {healed}"
    );
    assert!(pairs > 0 && triples > 0);
}
