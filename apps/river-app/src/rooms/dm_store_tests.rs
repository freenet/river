use super::*;
use ciborium::Value;
use ed25519_dalek::SigningKey;
use river_core::chat_delegate::{HiddenDmThreadEntry, OutboundDmStore};

fn vk(n: u8) -> ed25519_dalek::VerifyingKey {
    SigningKey::from_bytes(&[n; 32]).verifying_key()
}

fn id(n: u8) -> MemberId {
    MemberId::from(&vk(n))
}

const ME: u8 = 99;

fn key(room: u8, recipient: u8, token: u8) -> EntryKey {
    (vk(room).to_bytes(), id(recipient), PurgeToken([token; 16]))
}

fn entry(room: u8, recipient: u8, token: u8, ts: u64, text: &str) -> OutboundDmEntry {
    OutboundDmEntry {
        room_owner_vk: vk(room).to_bytes(),
        sender: id(ME),
        recipient: id(recipient),
        purge_token: PurgeToken([token; 16]),
        timestamp: ts,
        plaintext: text.to_string(),
    }
}

fn add(room: u8, recipient: u8, token: u8, ts: u64, text: &str) -> DmOp {
    DmOp::Add {
        key: key(room, recipient, token),
        sender: id(ME),
        timestamp: ts,
        text: Plaintext::new(text.to_string()),
    }
}

fn remove(room: u8, recipient: u8, token: u8) -> DmOp {
    DmOp::Remove {
        key: key(room, recipient, token),
    }
}

fn bytes_of(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).unwrap();
    out
}

fn typed(store: &OutboundDmStore) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(store, &mut out).unwrap();
    out
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

/// The stored value as a map of its fields.
fn fields(bytes: &[u8]) -> Vec<(Value, Value)> {
    match ciborium::de::from_reader::<Value, _>(bytes).unwrap() {
        Value::Map(m) => m,
        other => panic!("not a map: {other:?}"),
    }
}

fn field<'a>(map: &'a [(Value, Value)], name: &str) -> &'a Value {
    &map.iter()
        .find(|(k, _)| k.as_text() == Some(name))
        .unwrap_or_else(|| panic!("no field {name}"))
        .1
}

fn array(v: &Value) -> &Vec<Value> {
    match v {
        Value::Array(a) => a,
        other => panic!("not an array: {other:?}"),
    }
}

fn decoded(bytes: &[u8]) -> OutboundDmStore {
    ciborium::de::from_reader(bytes).unwrap()
}

/// A stored value as a newer web UI might write it: the entries, the archive list, and a field
/// of the map and a field of an entry that this build knows nothing about.
fn web_blob() -> (Vec<u8>, Vec<Value>) {
    let mut first = Value::serialized(&entry(1, 2, 1, 100, "web one")).unwrap();
    let mut second = Value::serialized(&entry(1, 3, 2, 200, "web two")).unwrap();
    for (n, e) in [&mut first, &mut second].into_iter().enumerate() {
        let Value::Map(m) = e else { unreachable!() };
        m.push((text("future_entry_field"), Value::from(n as u32 + 7)));
    }
    let hidden = Value::serialized(&vec![HiddenDmThreadEntry {
        room_owner_vk: vk(1).to_bytes(),
        peer: id(2),
        hidden_at_ts: 150,
    }])
    .unwrap();
    let items = vec![first, second];
    let blob = Value::Map(vec![
        (text("entries"), Value::Array(items.clone())),
        (text("hidden_threads"), hidden),
        (
            text("future_field"),
            Value::Map(vec![(text("a"), Value::Bytes(vec![1, 2, 3]))]),
        ),
    ]);
    (bytes_of(&blob), items)
}

#[test]
fn a_store_that_holds_nothing_gets_a_store_the_web_ui_reads() {
    let merged = merge(None, false, &[add(1, 2, 1, 100, "hello")]).unwrap();
    let bytes = merged.bytes.expect("a value to store");
    assert!(merged.evicted.is_empty());
    let want = OutboundDmStore {
        entries: vec![entry(1, 2, 1, 100, "hello")],
        hidden_threads: vec![],
    };
    assert_eq!(decoded(&bytes), want);
    // The very bytes the web UI's own save of that store writes.
    assert_eq!(bytes, typed(&want));
}

#[test]
fn a_merge_keeps_every_other_entry_the_archive_list_and_unknown_fields_value_for_value() {
    let (blob, items) = web_blob();
    let merged = merge(Some(&blob), true, &[add(1, 4, 3, 300, "native")]).unwrap();
    let bytes = merged.bytes.expect("a value to store");
    let out = fields(&bytes);
    let input = fields(&blob);

    // The order of the fields and the bytes of every field this app did not edit.
    let names = |m: &[(Value, Value)]| {
        m.iter()
            .map(|(k, _)| k.as_text().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&out), names(&input));
    for name in ["hidden_threads", "future_field"] {
        assert_eq!(
            bytes_of(field(&out, name)),
            bytes_of(field(&input, name)),
            "{name}"
        );
    }
    // The entries that were there, each as it was (the field of its own this build does not
    // know included), then the new one.
    let entries = array(field(&out, "entries"));
    assert_eq!(entries.len(), 3);
    for (kept, original) in entries.iter().zip(&items) {
        assert_eq!(bytes_of(kept), bytes_of(original));
    }
    assert_eq!(
        bytes_of(&entries[2]),
        bytes_of(&Value::serialized(&entry(1, 4, 3, 300, "native")).unwrap())
    );
    // Another build of this app reads the result.
    let read = decoded(&bytes);
    assert_eq!(read.entries.len(), 3);
    assert_eq!(read.hidden_threads.len(), 1);
}

#[test]
fn an_entry_already_there_changes_nothing() {
    let (blob, _) = web_blob();
    // The same room, recipient and token, even with other text: the stored one stays.
    let merged = merge(Some(&blob), true, &[add(1, 2, 1, 999, "other words")]).unwrap();
    assert_eq!(merged.bytes, None);
    assert!(merged.evicted.is_empty());
    // Removing what is not there changes nothing either.
    assert_eq!(
        merge(Some(&blob), true, &[remove(1, 2, 77)]).unwrap().bytes,
        None
    );
    // A store that holds nothing and a removal: nothing to write, and nothing created.
    assert_eq!(merge(None, false, &[remove(1, 2, 1)]).unwrap().bytes, None);
}

#[test]
fn a_removal_drops_only_the_named_entry() {
    let (blob, items) = web_blob();
    let merged = merge(Some(&blob), true, &[remove(1, 3, 2)]).unwrap();
    let out = fields(&merged.bytes.unwrap());
    let entries = array(field(&out, "entries"));
    assert_eq!(entries.len(), 1);
    assert_eq!(bytes_of(&entries[0]), bytes_of(&items[0]));
    let input = fields(&blob);
    assert_eq!(
        bytes_of(field(&out, "hidden_threads")),
        bytes_of(field(&input, "hidden_threads"))
    );
}

#[test]
fn a_value_that_cannot_be_read_is_never_written_over() {
    let (blob, _) = web_blob();
    let ops = [add(1, 4, 3, 300, "native")];
    let refused = |bytes: &[u8]| merge(Some(bytes), true, &ops).unwrap_err();

    assert_eq!(refused(&[]), Refusal::Unreadable);
    assert_eq!(
        refused(&bytes_of(&Value::Array(vec![]))),
        Refusal::Unreadable
    );
    assert_eq!(refused(&bytes_of(&text("x"))), Refusal::Unreadable);
    assert_eq!(refused(&[0xff, 0x00, 0x13]), Refusal::Unreadable);
    // Trailing bytes after a value that does decode.
    let mut trailing = blob.clone();
    trailing.push(0);
    assert_eq!(refused(&trailing), Refusal::Unreadable);
    // The envelope the delegate strips: a value that still has it is not a map.
    let mut enveloped = vec![1u8];
    enveloped.extend_from_slice(&5u64.to_le_bytes());
    enveloped.extend_from_slice(&blob);
    assert_eq!(refused(&enveloped), Refusal::Unreadable);
    // Fields of the wrong shape.
    for (entries, hidden) in [
        (text("no"), Value::Array(vec![])),
        (
            Value::Array(vec![text("not an entry")]),
            Value::Array(vec![]),
        ),
        (Value::Array(vec![]), text("no")),
        (Value::Null, Value::Array(vec![])),
    ] {
        let bad = Value::Map(vec![
            (text("entries"), entries),
            (text("hidden_threads"), hidden),
        ]);
        assert_eq!(refused(&bytes_of(&bad)), Refusal::Unreadable);
    }
    // Too long to be what this app wrote.
    assert_eq!(
        refused(&vec![0xa0; MAX_STORE_BYTES + 1]),
        Refusal::Unreadable
    );
}

#[test]
fn an_old_store_without_the_archive_list_or_the_entries_is_added_to() {
    // Pre-archive bytes: only `entries`.
    let only_entries = bytes_of(&Value::Map(vec![(
        text("entries"),
        Value::Array(vec![
            Value::serialized(&entry(1, 2, 1, 100, "old")).unwrap(),
        ]),
    )]));
    let out = merge(Some(&only_entries), true, &[add(1, 3, 2, 200, "new")])
        .unwrap()
        .bytes
        .unwrap();
    let read = decoded(&out);
    assert_eq!(read.entries.len(), 2);
    assert!(read.hidden_threads.is_empty());
    // A map that holds only the archive list (the web UI never writes this, `entries` has a
    // serde default): the entries field is added, the list kept.
    let hidden_only = bytes_of(&Value::Map(vec![(
        text("hidden_threads"),
        Value::Array(vec![]),
    )]));
    let out = merge(Some(&hidden_only), true, &[add(1, 3, 2, 200, "new")])
        .unwrap()
        .bytes
        .unwrap();
    assert_eq!(decoded(&out).entries.len(), 1);
}

#[test]
fn a_store_that_vanished_is_not_written_over() {
    // The cache had read a value; the delegate now says there is none.
    assert_eq!(
        merge(None, true, &[add(1, 2, 1, 100, "hello")]).unwrap_err(),
        Refusal::Vanished
    );
    assert_eq!(merge(None, true, &[]).unwrap_err(), Refusal::Vanished);
}

#[test]
fn the_per_pair_cap_drops_this_users_oldest_entry_and_nobody_elses() {
    let mut entries: Vec<OutboundDmEntry> = (0..MAX_DM_MESSAGES_PER_PAIR as u64)
        .map(|n| entry(1, 2, n as u8, 1000 + n, "x"))
        .collect();
    // Another sender's older entries to the same recipient in the same room are not ours.
    let mut theirs = entry(1, 2, 200, 1, "not mine");
    theirs.sender = id(50);
    entries.push(theirs);
    // And the same sender to another recipient.
    entries.push(entry(1, 3, 201, 1, "other pair"));
    let blob = typed(&OutboundDmStore {
        entries,
        hidden_threads: vec![],
    });

    let merged = merge(Some(&blob), true, &[add(1, 2, 250, 5000, "newest")]).unwrap();
    assert_eq!(merged.evicted, vec![key(1, 2, 0)], "the oldest of the pair");
    let out = decoded(&merged.bytes.unwrap());
    assert_eq!(out.entries.len(), MAX_DM_MESSAGES_PER_PAIR + 2);
    let has = |k: EntryKey| {
        out.entries
            .iter()
            .any(|e| (e.room_owner_vk, e.recipient, e.purge_token) == k)
    };
    assert!(has(key(1, 2, 250)));
    assert!(!has(key(1, 2, 0)));
    assert!(has(key(1, 2, 200)), "another sender's entry stays");
    assert!(has(key(1, 3, 201)), "another pair's entry stays");
}

#[test]
fn the_byte_bound_drops_this_users_oldest_entries_first_and_refuses_when_that_is_not_enough() {
    let big = "y".repeat(2000);
    // Eight recipients, so no pair is near its cap: the bytes are what is bound.
    let mine: Vec<OutboundDmEntry> = (0..8u8)
        .map(|r| entry(1, 10 + r, r, 100 + u64::from(r), &big))
        .collect();
    let blob = typed(&OutboundDmStore {
        entries: mine,
        hidden_threads: vec![],
    });
    let limit = blob.len() + 100;
    let ops = [add(1, 30, 30, 900, &big)];
    let merged = merge_within(Some(&blob), true, &ops, limit).unwrap();
    // One new entry of about 2000 bytes does not fit beside eight: the oldest go until it does.
    assert_eq!(merged.evicted.first(), Some(&key(1, 10, 0)));
    let out = merged.bytes.unwrap();
    assert!(out.len() <= limit);
    let read = decoded(&out);
    assert!(
        read.entries
            .iter()
            .any(|e| e.purge_token == PurgeToken([30; 16]))
    );

    // Entries of another sender are not ours to drop: with only those, it is refused.
    let others: Vec<OutboundDmEntry> = (0..8u8)
        .map(|r| {
            let mut e = entry(1, 10 + r, r, 100 + u64::from(r), &big);
            e.sender = id(50);
            e
        })
        .collect();
    let blob = typed(&OutboundDmStore {
        entries: others,
        hidden_threads: vec![],
    });
    assert_eq!(
        merge_within(Some(&blob), true, &ops, blob.len() + 100).unwrap_err(),
        Refusal::Full
    );
    // The real bound is far above what that costs.
    assert!(MAX_WRITE_BYTES > blob.len() * 100);
}

#[test]
fn nothing_printed_names_the_text() {
    let op = add(1, 2, 1, 100, "PLAINTEXT-SECRET");
    assert!(!format!("{op:?}").contains("PLAINTEXT"));
    for r in [
        Refusal::Unreadable,
        Refusal::Vanished,
        Refusal::Full,
        Refusal::Encode,
    ] {
        assert!(!r.to_string().is_empty());
    }
}

#[test]
fn a_batch_that_takes_a_pair_past_its_cap_drops_the_oldest_across_stored_and_added() {
    // 100 stored, the newest of them at 1100; then a batch of 101 added, 50 of them older than
    // everything stored and 51 newer.
    let stored: Vec<OutboundDmEntry> = (0..MAX_DM_MESSAGES_PER_PAIR as u64)
        .map(|n| entry(1, 2, n as u8, 1000 + n, "stored"))
        .collect();
    let blob = typed(&OutboundDmStore {
        entries: stored,
        hidden_threads: vec![],
    });
    let mut ops = Vec::new();
    for n in 0..101u64 {
        let ts = if n < 50 { 10 + n } else { 2000 + n };
        ops.push(add(1, 2, 100 + n as u8, ts, "batched"));
    }
    let merged = merge(Some(&blob), true, &ops).unwrap();
    let out = decoded(&merged.bytes.unwrap());
    assert_eq!(
        out.entries.len(),
        MAX_DM_MESSAGES_PER_PAIR,
        "back at the cap"
    );
    assert_eq!(merged.evicted.len(), 101 + 100 - MAX_DM_MESSAGES_PER_PAIR);
    // The oldest by time went: the 50 old adds and the 51 oldest of the rest. Every newer one
    // of the added stays and the newest of the stored ones too.
    let kept: Vec<u64> = out.entries.iter().map(|e| e.timestamp).collect();
    let oldest_kept = *kept.iter().min().unwrap();
    assert!(
        merged.evicted.iter().all(|k| {
            out.entries
                .iter()
                .all(|e| (e.room_owner_vk, e.recipient, e.purge_token) != *k)
        }),
        "what is reported dropped is not in the value"
    );
    assert!(oldest_kept >= 1051, "{oldest_kept}");
    // Added ones that were dropped are in the report, so the cache does not save them again.
    assert!(merged.evicted.contains(&key(1, 2, 100)));
}

#[test]
fn a_batch_of_equal_times_is_cut_by_token() {
    let adds: Vec<DmOp> = (0..=MAX_DM_MESSAGES_PER_PAIR as u8)
        .map(|n| add(1, 2, n, 500, "same second"))
        .collect();
    let merged = merge(None, false, &adds).unwrap();
    assert_eq!(merged.evicted, vec![key(1, 2, 0)]);
    assert_eq!(
        decoded(&merged.bytes.unwrap()).entries.len(),
        MAX_DM_MESSAGES_PER_PAIR
    );
}

#[test]
fn a_removal_is_made_even_when_the_adds_beside_it_are_refused() {
    let big = "z".repeat(4000);
    let blob = typed(&OutboundDmStore {
        entries: vec![entry(1, 2, 1, 100, "gone")],
        hidden_threads: vec![],
    });
    // The adds cannot fit (a bound below one entry), but the value has to lose the removed one
    // when asked alone: a removal-only merge never fails for size.
    let merged =
        merge_within(Some(&blob), true, &[remove(1, 2, 1)], 10).expect("a removal alone fits");
    assert!(decoded(&merged.bytes.unwrap()).entries.is_empty());
    assert_eq!(
        merge_within(Some(&blob), true, &[add(1, 3, 2, 200, &big)], 10).unwrap_err(),
        Refusal::Full
    );
}

/// What a value that is not in the canonical form comes back as: the map's decoded content,
/// order, text, byte strings and integer values are kept; the lengths and integer widths are
/// written again in the shortest form, and an indefinite-length container comes back counted.
#[test]
fn a_value_in_a_non_canonical_form_is_kept_value_for_value_and_written_canonically() {
    let mut blob: Vec<u8> = Vec::new();
    // An indefinite-length map: {"entries": [], "hidden_threads": [], "future": <non-minimal 5>}.
    blob.push(0xbf);
    blob.extend([0x67]);
    blob.extend(b"entries");
    blob.extend([0x9f, 0xff]); // indefinite-length empty array
    blob.extend([0x6e]);
    blob.extend(b"hidden_threads");
    blob.extend([0x80]);
    blob.extend([0x66]);
    blob.extend(b"future");
    blob.extend([0x19, 0x00, 0x05]); // 5 written in two bytes
    blob.push(0xff);

    let merged = merge(Some(&blob), true, &[add(1, 2, 1, 100, "new")]).unwrap();
    let out = merged.bytes.unwrap();
    let map = fields(&out);
    let names: Vec<&str> = map.iter().map(|(k, _)| k.as_text().unwrap()).collect();
    assert_eq!(names, ["entries", "hidden_threads", "future"], "order kept");
    assert_eq!(field(&map, "future"), &Value::from(5u32), "same value");
    assert!(array(field(&map, "hidden_threads")).is_empty());
    assert_eq!(array(field(&map, "entries")).len(), 1);
    // The bytes are the canonical ones: a definite-length map header and a one-byte integer.
    assert_eq!(out[0], 0xa3, "a counted map of three");
    assert!(
        out.windows(3).all(|w| w != [0x19, 0x00, 0x05]),
        "the non-minimal integer is written minimal"
    );
    assert!(out.ends_with(&[0x05]), "{out:?}");
}

// The archive list ------------------------------------------------------------------------------

fn thread(room: u8, peer: u8) -> super::ThreadKey {
    (vk(room).to_bytes(), id(peer))
}

fn hide(room: u8, peer: u8, ts: u64) -> DmOp {
    DmOp::Archive(ArchiveOp::Hide {
        thread: thread(room, peer),
        hidden_at_ts: ts,
    })
}

fn unhide(room: u8, peer: u8) -> DmOp {
    DmOp::Archive(ArchiveOp::Unhide {
        thread: thread(room, peer),
    })
}

fn unhide_if_past(room: u8, peer: u8, past: u64) -> DmOp {
    DmOp::Archive(ArchiveOp::UnhideIfPast {
        thread: thread(room, peer),
        past,
    })
}

fn hidden_entry(room: u8, peer: u8, ts: u64) -> HiddenDmThreadEntry {
    HiddenDmThreadEntry {
        room_owner_vk: vk(room).to_bytes(),
        peer: id(peer),
        hidden_at_ts: ts,
    }
}

/// A stored value with archive entries, one of them carrying a field this build does not know,
/// next to entries, a field of the map this build does not know, and an unknown entry field.
fn archived_blob() -> Vec<u8> {
    let mut first = Value::serialized(&hidden_entry(1, 2, 150)).unwrap();
    let Value::Map(m) = &mut first else {
        unreachable!()
    };
    m.push((text("pinned_by"), Value::from(7u32)));
    let second = Value::serialized(&hidden_entry(1, 3, 400)).unwrap();
    let mut sent = Value::serialized(&entry(1, 2, 1, 100, "web one")).unwrap();
    let Value::Map(m) = &mut sent else {
        unreachable!()
    };
    m.push((text("future_entry_field"), Value::from(9u32)));
    bytes_of(&Value::Map(vec![
        (text("entries"), Value::Array(vec![sent])),
        (text("hidden_threads"), Value::Array(vec![first, second])),
        (
            text("future_field"),
            Value::Map(vec![(text("a"), Value::Bytes(vec![1, 2, 3]))]),
        ),
    ]))
}

#[test]
fn archiving_adds_one_entry_and_keeps_every_other_field_value_for_value() {
    let blob = archived_blob();
    let merged = merge(Some(&blob), true, &[hide(2, 5, 900)]).unwrap();
    let out = fields(&merged.bytes.expect("changed"));
    let before = fields(&blob);
    // Entries (with their unknown field) and the unknown field of the map are untouched.
    assert_eq!(field(&out, "entries"), field(&before, "entries"));
    assert_eq!(field(&out, "future_field"), field(&before, "future_field"));
    // The two archive entries read are kept, the unknown field of one too, and one is added.
    let kept = array(field(&before, "hidden_threads"));
    let now = array(field(&out, "hidden_threads"));
    assert_eq!(now.len(), 3);
    assert_eq!(&now[..2], &kept[..]);
    // The order of the map's fields is the order it was found in.
    let names = |m: &[(Value, Value)]| -> Vec<String> {
        m.iter()
            .map(|(k, _)| k.as_text().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names(&out), names(&before));
    // What the web UI reads back is the entry it would have written.
    let store = decoded(&bytes_of(&Value::Map(out)));
    assert_eq!(store.hidden_threads[2], hidden_entry(2, 5, 900));
    assert!(merged.evicted.is_empty());
}

/// The entry made here is the bytes the web UI's own encoder makes for the same entry, so the
/// web UI, which stores it with the typed `OutboundDmStore`, reads it as it reads its own.
#[test]
fn an_archive_entry_made_here_is_the_web_uis_own_encoding() {
    let merged = merge(None, false, &[hide(1, 2, 150)]).unwrap();
    let want = typed(&OutboundDmStore {
        entries: vec![],
        hidden_threads: vec![hidden_entry(1, 2, 150)],
    });
    assert_eq!(merged.bytes.unwrap(), want);
}

#[test]
fn re_archiving_raises_an_older_cutoff_keeps_the_entrys_other_fields_and_ignores_a_lower_one() {
    let blob = archived_blob();
    let raised = merge(Some(&blob), true, &[hide(1, 2, 700)]).unwrap();
    let out = fields(&raised.bytes.unwrap());
    let hidden = array(field(&out, "hidden_threads"));
    assert_eq!(hidden.len(), 2);
    let Value::Map(first) = &hidden[0] else {
        panic!("a map")
    };
    assert_eq!(
        first
            .iter()
            .find(|(k, _)| k.as_text() == Some("hidden_at_ts"))
            .unwrap()
            .1,
        Value::from(700u64)
    );
    assert_eq!(
        first
            .iter()
            .find(|(k, _)| k.as_text() == Some("pinned_by"))
            .unwrap()
            .1,
        Value::from(7u32),
        "the entry's unknown field stays"
    );
    // At or below what is held: nothing to store.
    for ts in [150, 100, 0] {
        let same = merge(Some(&blob), true, &[hide(1, 2, ts)]).unwrap();
        assert_eq!(same.bytes, None, "{ts}");
    }
}

#[test]
fn unarchiving_removes_the_thread_and_nothing_else() {
    let blob = archived_blob();
    let merged = merge(Some(&blob), true, &[unhide(1, 2)]).unwrap();
    let out = fields(&merged.bytes.unwrap());
    let before = fields(&blob);
    assert_eq!(field(&out, "entries"), field(&before, "entries"));
    assert_eq!(field(&out, "future_field"), field(&before, "future_field"));
    let hidden = decoded(&bytes_of(&Value::Map(out))).hidden_threads;
    assert_eq!(hidden, vec![hidden_entry(1, 3, 400)]);
    // A thread that is not there: nothing to store.
    assert_eq!(
        merge(Some(&blob), true, &[unhide(9, 9)]).unwrap().bytes,
        None
    );
    // The same peer in another room is another thread.
    assert_eq!(
        merge(Some(&blob), true, &[unhide(2, 2)]).unwrap().bytes,
        None
    );
}

#[test]
fn a_conditional_unarchive_checks_the_cutoff_the_write_finds() {
    let blob = archived_blob();
    // Stored 150; a message at 151 revives it.
    let gone = merge(Some(&blob), true, &[unhide_if_past(1, 2, 151)]).unwrap();
    let hidden = decoded(&gone.bytes.unwrap()).hidden_threads;
    assert_eq!(hidden, vec![hidden_entry(1, 3, 400)]);
    // Another device raised it to 400 meanwhile: a message at 300 does not destroy that.
    let kept = merge(Some(&blob), true, &[unhide_if_past(1, 3, 300)]).unwrap();
    assert_eq!(kept.bytes, None);
    // Exactly at the cutoff is not past it.
    assert_eq!(
        merge(Some(&blob), true, &[unhide_if_past(1, 2, 150)])
            .unwrap()
            .bytes,
        None
    );
}

#[test]
fn archive_changes_and_sent_text_go_in_one_write() {
    let blob = archived_blob();
    let merged = merge(
        Some(&blob),
        true,
        &[
            remove(1, 2, 1),
            add(1, 4, 5, 300, "fresh"),
            hide(1, 4, 300),
            unhide(1, 3),
        ],
    )
    .unwrap();
    let store = decoded(&merged.bytes.unwrap());
    let texts: Vec<&str> = store.entries.iter().map(|e| e.plaintext.as_str()).collect();
    assert_eq!(texts, vec!["fresh"]);
    assert_eq!(
        store.hidden_threads,
        vec![hidden_entry(1, 2, 150), hidden_entry(1, 4, 300)]
    );
}

#[test]
fn an_old_store_without_an_archive_list_gets_one_only_when_an_archive_change_needs_it() {
    let old = bytes_of(&Value::Map(vec![(
        text("entries"),
        Value::Array(vec![
            Value::serialized(&entry(1, 2, 1, 100, "old")).unwrap(),
        ]),
    )]));
    let added = merge(Some(&old), true, &[add(1, 3, 2, 200, "new")]).unwrap();
    assert!(
        fields(&added.bytes.unwrap())
            .iter()
            .all(|(k, _)| k.as_text() != Some("hidden_threads")),
        "a text save does not add the field"
    );
    let hid = merge(Some(&old), true, &[hide(1, 2, 5)]).unwrap();
    let store = decoded(&hid.bytes.unwrap());
    assert_eq!(store.hidden_threads, vec![hidden_entry(1, 2, 5)]);
    assert_eq!(store.entries.len(), 1);
    // Taking a thread out of a list that is not there changes nothing.
    assert_eq!(
        merge(Some(&old), true, &[unhide(1, 2)]).unwrap().bytes,
        None
    );
}

#[test]
fn an_archive_list_that_cannot_be_read_is_never_written_over() {
    let build = |hidden: Value| {
        bytes_of(&Value::Map(vec![
            (text("entries"), Value::Array(vec![])),
            (text("hidden_threads"), hidden),
        ]))
    };
    // Not an array.
    for blob in [build(Value::from(3u32)), build(Value::Null)] {
        for op in [hide(1, 2, 5), unhide(1, 2), unhide_if_past(1, 2, 5)] {
            assert_eq!(
                merge(Some(&blob), true, &[op]).unwrap_err(),
                Refusal::Unreadable
            );
        }
    }
    // An entry that is not an archive entry: the web UI's own decode would fail on it too.
    for bad in [
        Value::Array(vec![Value::from(1u32)]),
        Value::Array(vec![Value::Map(vec![(text("peer"), Value::from(1u32))])]),
        Value::Array(vec![Value::Map(vec![])]),
    ] {
        let blob = build(bad);
        assert_eq!(
            merge(Some(&blob), true, &[hide(1, 2, 5)]).unwrap_err(),
            Refusal::Unreadable
        );
    }
    // The shared refusals hold for an archive change as for text.
    assert_eq!(
        merge(None, true, &[hide(1, 2, 5)]).unwrap_err(),
        Refusal::Vanished
    );
    for junk in [&b""[..], &[0xff][..], &[0x80][..]] {
        assert_eq!(
            merge(Some(junk), true, &[hide(1, 2, 5)]).unwrap_err(),
            Refusal::Unreadable
        );
    }
    let mut trailing = archived_blob();
    trailing.push(0);
    assert_eq!(
        merge(Some(&trailing), true, &[hide(1, 2, 5)]).unwrap_err(),
        Refusal::Unreadable
    );
}

#[test]
fn a_text_write_does_not_read_the_archive_entries() {
    // A text write checks only that the archive list is an array (as before): a list whose
    // entries this build cannot parse must not stop the user's sent text from being saved.
    let blob = bytes_of(&Value::Map(vec![
        (text("entries"), Value::Array(vec![])),
        (
            text("hidden_threads"),
            Value::Array(vec![Value::from(1u32)]),
        ),
    ]));
    assert!(merge(Some(&blob), true, &[add(1, 2, 1, 100, "x")]).is_ok());
}

#[test]
fn a_thread_held_twice_is_archived_and_unarchived_as_one() {
    let twice = bytes_of(&Value::Map(vec![
        (text("entries"), Value::Array(vec![])),
        (
            text("hidden_threads"),
            Value::serialized(&vec![hidden_entry(1, 2, 100), hidden_entry(1, 2, 300)]).unwrap(),
        ),
    ]));
    // 300 is held: 200 changes nothing, 400 raises both so none is left below.
    assert_eq!(
        merge(Some(&twice), true, &[hide(1, 2, 200)]).unwrap().bytes,
        None
    );
    let raised = decoded(
        &merge(Some(&twice), true, &[hide(1, 2, 400)])
            .unwrap()
            .bytes
            .unwrap(),
    );
    assert_eq!(
        raised.hidden_threads,
        vec![hidden_entry(1, 2, 400), hidden_entry(1, 2, 400)]
    );
    let gone = decoded(
        &merge(Some(&twice), true, &[unhide(1, 2)])
            .unwrap()
            .bytes
            .unwrap(),
    );
    assert!(gone.hidden_threads.is_empty());
}

#[test]
fn a_new_archive_entry_over_the_byte_bound_is_refused_but_taking_one_out_never_is() {
    let blob = archived_blob();
    // Room for what is stored, not for one more entry.
    let limit = blob.len();
    assert_eq!(
        merge_within(Some(&blob), true, &[hide(2, 5, 900)], limit).unwrap_err(),
        Refusal::Full
    );
    // Moving a cutoff or taking an entry out is never refused for size, even over the bound.
    assert!(merge_within(Some(&blob), true, &[unhide(1, 2)], 10).is_ok());
    assert!(merge_within(Some(&blob), true, &[hide(1, 2, 700)], 10).is_ok());
}

#[test]
fn the_debug_form_of_an_archive_change_names_no_thread() {
    let shown = format!("{:?}", hide(1, 2, 150));
    assert_eq!(shown, "Archive { .. }");
    assert!(format!("{:?}", unhide(1, 2)).starts_with("Unarchive"));
}

/// The store merge and the in-memory list apply an op the same way: the cache's view of what was
/// saved is what the store holds. Every op over lists with none, one, two and duplicate entries.
#[test]
fn the_store_and_the_list_apply_every_archive_op_alike() {
    use crate::rooms::dm_archive::apply;
    let t = thread(1, 2);
    let lists: Vec<Vec<HiddenDmThreadEntry>> = vec![
        vec![],
        vec![hidden_entry(1, 2, 100)],
        vec![hidden_entry(1, 2, 100), hidden_entry(1, 3, 500)],
        vec![hidden_entry(1, 2, 100), hidden_entry(1, 2, 300)],
        vec![hidden_entry(2, 2, 100)],
    ];
    let mut ops = Vec::new();
    for ts in [0, 100, 200, 300, 400] {
        ops.push(ArchiveOp::Hide {
            thread: t,
            hidden_at_ts: ts,
        });
        ops.push(ArchiveOp::UnhideIfPast {
            thread: t,
            past: ts,
        });
    }
    ops.push(ArchiveOp::Unhide { thread: t });
    for list in &lists {
        for op in &ops {
            let blob = typed(&OutboundDmStore {
                entries: vec![],
                hidden_threads: list.clone(),
            });
            let merged = merge(Some(&blob), true, &[DmOp::Archive(*op)]).unwrap();
            let mut want = list.clone();
            let changed = apply(&mut want, op);
            assert_eq!(merged.bytes.is_some(), changed, "{op:?} over {list:?}");
            assert_eq!(
                merged.archive_skipped.is_empty(),
                changed,
                "{op:?} over {list:?}"
            );
            let got = match merged.bytes {
                Some(bytes) => decoded(&bytes).hidden_threads,
                None => list.clone(),
            };
            assert_eq!(got, want, "{op:?} over {list:?}");
        }
    }
}
