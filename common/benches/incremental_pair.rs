//! Fixtures for timing `update_state` and `validate_state` on an
//! Official-room-shaped room (187 members, 400 bans, a 200-message window, a
//! full provisional pool) under wasmtime, with `tools/wasm-cost-bench`.
//!
//! ```text
//! CARGO_PROFILE_BENCH_OPT_LEVEL=3 BENCH_DUMP_DIR=/tmp/room-fixtures \
//!   cargo bench -p river-core --bench incremental_pair
//! PROD_CONFIG=1 wasmbench room_contract.wasm /tmp/room-fixtures/pool8/params.cbor \
//!   /tmp/room-fixtures/pool8/state.cbor /tmp/room-fixtures/pool8/c.delta 15
//! ```
//!
//! Cases: `a` one newcomer message, `b` a newcomer join (entry, member_info,
//! message), `c` one member message, `d.state` a full state carrying one new
//! message. The contract's own signature-count assertions are in
//! `contracts/room-contract/src/incremental_tests.rs`.

#[path = "../tests/fixtures/official_room.rs"]
mod fixture;

use fixture::{build, cbor, entry, info, msg, sk};
use freenet_scaffold::ComposableState;
use river_core::room_state::provisional::ProvisionalDelta;
use river_core::room_state::ChatRoomStateV1Delta;

fn main() {
    let Ok(dir) = std::env::var("BENCH_DUMP_DIR") else {
        println!("set BENCH_DUMP_DIR=<dir> to write the fixtures");
        return;
    };
    for pool in [8usize, 16] {
        let f = build(pool);
        let oid = f.params.owner_id();
        let out = std::path::Path::new(&dir).join(format!("pool{pool}"));
        std::fs::create_dir_all(&out).unwrap();
        let write = |name: &str, bytes: Vec<u8>| std::fs::write(out.join(name), bytes).unwrap();
        write("params.cbor", cbor(&f.params));
        write("state.cbor", cbor(&f.state));
        let newest = 10_000u64;
        let one = |k: &ed25519_dalek::SigningKey| ChatRoomStateV1Delta {
            recent_messages: Some(vec![msg(oid, k, newest)]),
            ..Default::default()
        };
        write("a.delta", cbor(&one(&f.pool[0])));
        write("c.delta", cbor(&one(&f.members[3])));
        let nk = sk(700_000);
        let b = ChatRoomStateV1Delta {
            provisional: Some(ProvisionalDelta {
                entries: vec![entry(&f.params, &nk)],
                bans: vec![],
            }),
            member_info: Some(vec![info(&nk, "fresh newcomer")]),
            recent_messages: Some(vec![msg(oid, &nk, newest)]),
            ..Default::default()
        };
        write("b.delta", cbor(&b));
        let mut other = f.state.clone();
        other
            .apply_delta(&f.state.clone(), &f.params, &Some(one(&f.members[5])))
            .unwrap();
        write("d.state", cbor(&other));
        println!("pool {pool}: fixtures in {}", out.display());
    }
}
