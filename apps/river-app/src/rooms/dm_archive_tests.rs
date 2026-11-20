use super::*;
use ed25519_dalek::SigningKey;
use river_core::room_state::direct_messages::compose_direct_message;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn id(n: u8) -> MemberId {
    MemberId::from(&sk(n).verifying_key())
}

fn thread(room: u8, peer: u8) -> ThreadKey {
    ([room; 32], id(peer))
}

fn entry(room: u8, peer: u8, ts: u64) -> HiddenDmThreadEntry {
    HiddenDmThreadEntry {
        room_owner_vk: [room; 32],
        peer: id(peer),
        hidden_at_ts: ts,
    }
}

// The web UI's `archive_cutoff_*` tests, ported (ui dm_rail_section.rs) -------------------------

#[test]
fn archive_cutoff_takes_the_row_when_it_leads() {
    assert_eq!(archive_cutoff(1_500, Some(1_000)), 1_500);
}

#[test]
fn archive_cutoff_takes_live_when_it_leads() {
    assert_eq!(archive_cutoff(1_000, Some(1_500)), 1_500);
}

#[test]
fn archive_cutoff_falls_back_to_the_row_when_live_is_unreadable() {
    assert_eq!(archive_cutoff(1_200, None), 1_200);
}

#[test]
fn archive_cutoff_is_zero_when_peer_never_wrote() {
    assert_eq!(archive_cutoff(0, Some(0)), 0);
    // And a thread nobody wrote in is still hidden by it: `<=`.
    assert!(is_hidden(Some(0), 0));
}

/// The invariant the gate depends on: the cutoff is never below the thread's true inbound
/// maximum, however implausibly a peer stamped it (up to the clamp every native time has).
#[test]
fn archive_cutoff_is_never_below_the_true_inbound_max() {
    let now = 5_000u64;
    let forged = now + MAX_DM_FUTURE_SKEW_SECS + 100_000;
    let cutoff = archive_cutoff(forged, Some(forged));
    assert!(cutoff >= forged);
    assert!(is_hidden(Some(cutoff), forged), "so the rail hides it");
}

/// Raw times, as the web UI: a peer who signs a time past 2100 sets a cutoff at that time, so the
/// web UI's own gate (which compares raw times) cannot drop the archive written here.
#[test]
fn a_far_future_time_is_kept_raw_and_survives_the_web_uis_gate() {
    let now = 1_700_000_000;
    for signed in [u64::MAX, 4_102_444_801, now + 10_000_000_000] {
        let cutoff = archive_cutoff(signed, Some(signed));
        assert_eq!(cutoff, signed, "not clamped");
        assert!(is_hidden(Some(cutoff), signed), "the thread is archived");
        // The web UI's gate: no incoming stamp is strictly past it, whatever it is.
        for incoming in [signed, signed.saturating_add(1), u64::MAX, 0, now] {
            assert!(
                !should_unhide_for_inbound(Some(cutoff), incoming, now),
                "{incoming} against {cutoff}"
            );
        }
    }
}

/// The cutoff carries no wall-clock term: nothing in this module reads the time.
#[test]
fn the_rules_read_no_clock() {
    let src = include_str!("dm_archive.rs").replace("\r\n", "\n");
    let (body, _) = src
        .split_once("#[cfg(test)]\n#[path")
        .expect("the marker of the end of the code");
    let code: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!code.contains("SystemTime::now"));
    assert!(!code.contains("Instant::now"));
}

// The gate (the web UI's `should_unhide_for_inbound_dm_*`) -------------------------------------

#[test]
fn the_gate_is_false_when_not_archived() {
    assert!(!should_unhide_for_inbound(None, 9_999, 1_000_000));
    assert!(!should_unhide_for_inbound(None, 0, 1_000_000));
}

#[test]
fn the_gate_is_true_past_the_cutoff() {
    assert!(should_unhide_for_inbound(Some(1_000), 1_001, 1_000_000));
}

#[test]
fn the_gate_is_false_at_or_below_the_cutoff() {
    assert!(!should_unhide_for_inbound(Some(1_000), 999, 1_000_000));
    assert!(!should_unhide_for_inbound(Some(1_000), 1_000, 1_000_000));
}

/// The incoming stamp is bounded at now plus the contract's skew, and the cutoff is not.
#[test]
fn the_gate_bounds_the_incoming_stamp_not_the_cutoff() {
    let now = 1_000_000;
    let bound = now + MAX_DM_FUTURE_SKEW_SECS;
    // A peer who already inflated the cutoff to the bound cannot escalate past it.
    assert!(!should_unhide_for_inbound(Some(bound), u64::MAX, now));
    // But a first forgery can revive a normally archived thread (documented).
    assert!(should_unhide_for_inbound(Some(1_000), u64::MAX, now));
    assert!(should_unhide_for_inbound(Some(bound - 1), u64::MAX, now));
}

/// The #526 shape, driven through the gate and applying its effect to a list, so "the archive
/// survived" can fail: the pair is swept and both messages re-land with their original times.
#[test]
fn relanded_old_messages_do_not_destroy_the_archive() {
    let t = thread(1, 11);
    let mut list = Vec::new();
    let times = [800u64, 1_000];
    let cutoff = archive_cutoff(*times.iter().max().unwrap(), Some(1_000));
    assert!(apply(
        &mut list,
        &ArchiveOp::Hide {
            thread: t,
            hidden_at_ts: cutoff
        }
    ));
    for ts in times {
        if let Some(op) = revival_op(t, stored_cutoff(&list, &t).unwrap(), ts, 1_000_000) {
            apply(&mut list, &op);
        }
    }
    assert_eq!(stored_cutoff(&list, &t), Some(1_000), "kept");
    assert!(is_hidden(stored_cutoff(&list, &t), 1_000));

    // A new message past the cutoff does revive it.
    let op = revival_op(t, 1_000, 1_001, 1_000_000).expect("past the cutoff");
    apply(&mut list, &op);
    assert_eq!(stored_cutoff(&list, &t), None);
}

/// The op the gate makes names the bounded stamp, so what the write compares is what the gate
/// judged.
#[test]
fn a_revival_op_carries_the_bounded_stamp() {
    let t = thread(1, 11);
    let now = 1_000_000;
    assert_eq!(
        revival_op(t, 10, u64::MAX, now),
        Some(ArchiveOp::UnhideIfPast {
            thread: t,
            past: now + MAX_DM_FUTURE_SKEW_SECS
        })
    );
    assert_eq!(revival_op(t, 10, 10, now), None);
}

// The list (the web UI's `decide_hide_action_*` and `hide_unhide_rehide_round_trip`) ------------

#[test]
fn hide_inserts_when_nothing_is_there_and_raises_an_older_cutoff_only() {
    let t = thread(1, 11);
    let mut list = Vec::new();
    let hide = |ts| ArchiveOp::Hide {
        thread: t,
        hidden_at_ts: ts,
    };
    assert!(apply(&mut list, &hide(1_000)));
    assert_eq!(list, vec![entry(1, 11, 1_000)]);
    // At or below what is held: nothing changes.
    assert!(!apply(&mut list, &hide(1_000)));
    assert!(!apply(&mut list, &hide(500)));
    assert_eq!(list, vec![entry(1, 11, 1_000)]);
    // Strictly above: the cutoff moves (re-archive after a revival).
    assert!(apply(&mut list, &hide(1_500)));
    assert_eq!(list, vec![entry(1, 11, 1_500)]);
}

#[test]
fn archive_revive_and_archive_again_round_trips() {
    let t = thread(1, 11);
    let mut list = Vec::new();
    apply(
        &mut list,
        &ArchiveOp::Hide {
            thread: t,
            hidden_at_ts: 1_000,
        },
    );
    assert!(is_hidden(stored_cutoff(&list, &t), 1_000));
    // A message at 1500 revives it on the rail with no change to the list.
    assert!(!is_hidden(stored_cutoff(&list, &t), 1_500));
    // Archiving again at 1500 must advance the cutoff (not no-op), or it revives at once.
    apply(
        &mut list,
        &ArchiveOp::Hide {
            thread: t,
            hidden_at_ts: 1_500,
        },
    );
    assert!(is_hidden(stored_cutoff(&list, &t), 1_500));
}

#[test]
fn unhide_removes_every_entry_of_the_thread_and_only_that_thread() {
    let t = thread(1, 11);
    let mut list = vec![
        entry(1, 11, 100),
        entry(1, 11, 300),
        entry(1, 12, 100),
        entry(2, 11, 100),
    ];
    assert_eq!(stored_cutoff(&list, &t), Some(300), "the largest");
    assert!(apply(&mut list, &ArchiveOp::Unhide { thread: t }));
    assert_eq!(list, vec![entry(1, 12, 100), entry(2, 11, 100)]);
    assert!(!apply(&mut list, &ArchiveOp::Unhide { thread: t }));
}

#[test]
fn a_hide_is_scoped_per_room_and_per_peer() {
    let mut list = vec![entry(1, 11, 1_000)];
    // The same peer in another room, another peer in the same room: not hidden.
    assert_eq!(stored_cutoff(&list, &thread(2, 11)), None);
    assert_eq!(stored_cutoff(&list, &thread(1, 12)), None);
    assert!(apply(
        &mut list,
        &ArchiveOp::Hide {
            thread: thread(2, 11),
            hidden_at_ts: 5
        }
    ));
    assert_eq!(list.len(), 2);
}

#[test]
fn a_conditional_unhide_leaves_a_fresher_archive_alone() {
    let t = thread(1, 11);
    let mut list = vec![entry(1, 11, 2_000)];
    // Decided on a read that held 1000: a message at 1500 would have revived it. The write finds
    // 2000 (another device archived again) and leaves it.
    assert!(!apply(
        &mut list,
        &ArchiveOp::UnhideIfPast {
            thread: t,
            past: 1_500
        }
    ));
    assert_eq!(list, vec![entry(1, 11, 2_000)]);
    assert!(apply(
        &mut list,
        &ArchiveOp::UnhideIfPast {
            thread: t,
            past: 2_001
        }
    ));
    assert!(list.is_empty());
}

// The room's own messages ----------------------------------------------------------------------

#[test]
fn the_live_inbound_time_counts_only_what_the_peer_sent_the_user() {
    let owner = sk(1).verifying_key();
    let (me, peer, other) = (sk(2), sk(3), sk(4));
    let mut dms = DirectMessagesV1::default();
    let mut put = |from: &SigningKey, to: &SigningKey, ts: u64| {
        dms.messages
            .push(compose_direct_message(from, &to.verifying_key(), &owner, ts, ts, b"x").unwrap());
    };
    put(&peer, &me, 500);
    put(&peer, &me, 900);
    // The user's own reply, later than any of theirs: never counted.
    put(&me, &peer, 5_000);
    // Someone else's message to the user, and the peer's to someone else.
    put(&other, &me, 7_000);
    put(&peer, &other, 8_000);
    assert_eq!(live_inbound_ts(&dms, id(2), id(3)), 900);
    // Nobody wrote: 0, so the thread archives at 0 and `<=` hides it.
    assert_eq!(live_inbound_ts(&dms, id(2), id(9)), 0);
}

#[test]
fn the_live_inbound_time_is_as_signed() {
    let owner = sk(1).verifying_key();
    let (me, peer) = (sk(2), sk(3));
    let mut dms = DirectMessagesV1::default();
    // A stamp far past 2100 (the contract's own skew check is client side only).
    let m = compose_direct_message(&peer, &me.verifying_key(), &owner, 1, 1, b"x").unwrap();
    let mut forged = m.clone();
    forged.message.timestamp = u64::MAX;
    dms.messages.push(forged);
    assert_eq!(live_inbound_ts(&dms, id(2), id(3)), u64::MAX);
    assert_eq!(live_inbound_by_peer(&dms, id(2))[&id(3)], u64::MAX);
}
