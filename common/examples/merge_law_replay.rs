//! Replay a captured conformance corpus against `ChatRoomStateV1::merge` and
//! report merge-law violations, in pure Rust rather than through the WASM
//! runtime. Mirrors exactly what the room contract's `update_state` does for
//! `UpdateData::State`, which is what `fdev verify-merge` calls `merge`.
//!
//! Usage: cargo run --release --example merge_law_replay -- <corpus-dir>

use ciborium::{de::from_reader, ser::into_writer};
use freenet_scaffold::ComposableState;
use river_core::room_state::ChatRoomParametersV1;
use river_core::ChatRoomStateV1;
use std::fs;

fn load<T: serde::de::DeserializeOwned>(p: &str) -> T {
    let b = fs::read(p).unwrap_or_else(|e| panic!("read {p}: {e}"));
    from_reader::<T, &[u8]>(&b[..]).unwrap_or_else(|e| panic!("decode {p}: {e}"))
}

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = vec![];
    into_writer(s, &mut v).unwrap();
    v
}

fn merge(
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
    p: &ChatRoomParametersV1,
) -> Result<ChatRoomStateV1, String> {
    let mut s = a.clone();
    let parent = s.clone();
    s.merge(&parent, p, b).map_err(|e| e.to_string())?;
    Ok(s)
}

fn h(s: &ChatRoomStateV1) -> String {
    blake3::hash(&ser(s)).to_hex()[..12].to_string()
}

/// Which top-level fields differ between two states.
fn diff_fields(x: &ChatRoomStateV1, y: &ChatRoomStateV1) -> Vec<&'static str> {
    let mut d = vec![];
    if x.configuration != y.configuration {
        d.push("configuration");
    }
    if x.bans != y.bans {
        d.push("bans");
    }
    if x.members != y.members {
        d.push("members");
    }
    if x.member_info != y.member_info {
        d.push("member_info");
    }
    if x.secrets != y.secrets {
        d.push("secrets");
    }
    if x.recent_messages != y.recent_messages {
        d.push("recent_messages");
    }
    if x.direct_messages != y.direct_messages {
        d.push("direct_messages");
    }
    if x.upgrade != y.upgrade {
        d.push("upgrade");
    }
    if x.version != y.version {
        d.push("version");
    }
    d
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: <corpus-dir>");
    let params: ChatRoomParametersV1 = load(&format!("{dir}/params.bin"));

    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let n = e.unwrap().file_name().to_string_lossy().to_string();
            n.starts_with("state_").then_some(n)
        })
        .collect();
    names.sort();
    let states: Vec<(String, ChatRoomStateV1)> = names
        .iter()
        .map(|n| (n.clone(), load(&format!("{dir}/{n}"))))
        .collect();
    println!("loaded {} states", states.len());

    let mut comm_fail = 0usize;
    let mut field_tally: std::collections::BTreeMap<String, usize> = Default::default();
    let mut first: Option<(String, String)> = None;
    for i in 0..states.len() {
        for j in (i + 1)..states.len() {
            let (na, a) = &states[i];
            let (nb, b) = &states[j];
            match (merge(a, b, &params), merge(b, a, &params)) {
                (Ok(x), Ok(y)) => {
                    if ser(&x) != ser(&y) {
                        comm_fail += 1;
                        let f = diff_fields(&x, &y);
                        *field_tally.entry(f.join("+")).or_default() += 1;
                        if first.is_none() {
                            first = Some((na.clone(), nb.clone()));
                            println!(
                                "first commutativity failure: {na} x {nb}\n  merge(A,B)={} merge(B,A)={}\n  differing fields: {:?}",
                                h(&x),
                                h(&y),
                                f
                            );
                        }
                    }
                }
                (x, y) => {
                    if x.is_err() != y.is_err() {
                        println!("ASYMMETRIC ERROR {na} x {nb}: {:?} / {:?}", x.err(), y.err());
                    }
                }
            }
        }
    }
    println!("\ncommutativity: {comm_fail} failing pairs out of {} ", states.len() * (states.len() - 1) / 2);
    println!("differing-field tally: {field_tally:?}");
}
