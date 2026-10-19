//! When the ban cap still has free slots after every ban that takes effect,
//! it fills them with the remaining bans in its order, a second ban for an
//! already kept (issuer, target) pair included. A merged state then absorbs
//! what it merged: merging the same incoming state again changes nothing.
//! The pairs are generated room states (`ban_cap_fill_pairs.hex`: per line
//! the direction, then the CBOR of the parameters and the two states).
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
fn a_merge_that_fills_the_ban_cap_absorbs_what_it_merged() {
    let pairs = include_str!("ban_cap_fill_pairs.hex");
    let mut checked = 0;
    for line in pairs.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split(' ').collect();
        let params: ChatRoomParametersV1 = ciborium::de::from_reader(&hex(f[1])[..]).unwrap();
        let a: ChatRoomStateV1 = ciborium::de::from_reader(&hex(f[2])[..]).unwrap();
        let b: ChatRoomStateV1 = ciborium::de::from_reader(&hex(f[3])[..]).unwrap();
        let (base, incoming) = if f[0] == "ab" { (&a, &b) } else { (&b, &a) };
        let mut merged = base.clone();
        merged.merge_from(&params, incoming).expect("merge");
        let mut again = merged.clone();
        again.merge_from(&params, incoming).expect("merge again");
        assert!(
            ser(&again) == ser(&merged),
            "pair {checked} ({}): merging the incoming state again changed the result",
            f[0]
        );
        checked += 1;
    }
    assert_eq!(checked, 4);
}
