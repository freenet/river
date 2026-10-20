//! Generated room-state pairs whose merge runs cleanup's member loop over
//! members with two records for one id, so the cap can remove a member's
//! inviter through one record while the other record stays. Every merge in
//! both directions finishes, and its result is a fixpoint of its own merge
//! (`cleanup_loop_pairs.hex`: per line the CBOR of the parameters and the two
//! states).
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn merges_over_duplicate_member_records_finish_at_a_fixpoint() {
    let mut merged_any = 0;
    for line in include_str!("cleanup_loop_pairs.hex")
        .lines()
        .filter(|l| !l.is_empty())
    {
        let f: Vec<&str> = line.split(' ').collect();
        let params: ChatRoomParametersV1 = ciborium::de::from_reader(&hex(f[0])[..]).unwrap();
        let a: ChatRoomStateV1 = ciborium::de::from_reader(&hex(f[1])[..]).unwrap();
        let b: ChatRoomStateV1 = ciborium::de::from_reader(&hex(f[2])[..]).unwrap();
        for (x, y) in [(&a, &b), (&b, &a)] {
            let mut m = x.clone();
            // A merge may refuse a state over the receiver's bounds; that is
            // not what this checks.
            if m.merge_from(&params, y).is_err() {
                continue;
            }
            merged_any += 1;
            let mut again = m.clone();
            again.merge_from(&params, &m).expect("self-merge");
            assert!(ser(&again) == ser(&m), "a fixpoint");
        }
    }
    assert!(merged_any >= 4, "{merged_any} merges ran");
}
