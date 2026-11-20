use super::*;
use river_core::room_state::direct_messages::PurgeToken;
use river_core::room_state::member::MemberId;

fn key(n: u8) -> EntryKey {
    let vk = ed25519_dalek::SigningKey::from_bytes(&[n; 32]).verifying_key();
    ([n; 32], MemberId::from(&vk), PurgeToken([n; 16]))
}

#[test]
fn a_failed_save_waits_longer_each_time_and_stops_after_the_bound() {
    let first = Duration::from_secs(10);
    let mut saver = DmSaver::with_waits(first, Duration::from_secs(60));
    let t0 = Instant::now();
    assert!(saver.due(t0));

    let mut now = t0;
    let mut waits = Vec::new();
    for _ in 0..MAX_FAILURES {
        assert!(saver.due(now), "due once the wait is over");
        saver.started(vec![key(1)], vec![]);
        saver.ended(false, now);
        assert!(!saver.due(now), "not at once after a failure");
        let at = saver.not_before.unwrap();
        waits.push(at - now);
        assert!(!saver.due(at - Duration::from_millis(1)));
        now = at;
    }
    // Doubling from the first wait, to the most.
    assert_eq!(waits[0], first);
    assert_eq!(waits[1], first * 2);
    assert_eq!(waits[2], first * 4);
    assert!(waits.iter().all(|w| *w <= Duration::from_secs(60)));
    assert_eq!(*waits.last().unwrap(), Duration::from_secs(60));
    // Too many in a row: no more tries, however long it waits.
    assert_eq!(saver.failures(), MAX_FAILURES);
    assert!(!saver.due(now + Duration::from_secs(100_000)));

    // Sending something starts it again.
    saver.new_work();
    assert!(saver.due(now));
    assert_eq!(saver.failures(), 0);
}

#[test]
fn a_save_that_works_clears_the_wait_and_what_it_removed() {
    let mut saver = DmSaver::default();
    let now = Instant::now();
    saver.owes_removal(key(1));
    saver.owes_removal(key(1));
    saver.owes_removal(key(2));
    assert_eq!(saver.removals(), &[key(1), key(2)], "once each");

    // A failed write keeps its removals owed.
    saver.started(vec![], vec![key(1)]);
    saver.ended(false, now);
    assert_eq!(saver.removals().len(), 2);

    // A write that worked clears the ones it carried, and one owed since stays.
    let later = now + Duration::from_secs(10_000);
    saver.started(vec![], vec![key(1)]);
    saver.owes_removal(key(3));
    saver.ended(true, later);
    assert_eq!(saver.removals(), &[key(2), key(3)]);
    assert!(saver.due(later));
}

#[test]
fn a_discard_knows_which_entries_a_working_save_adds() {
    let mut saver = DmSaver::default();
    saver.started(vec![key(1)], vec![]);
    assert!(saver.is_being_added(&key(1)));
    assert!(!saver.is_being_added(&key(2)));
    saver.ended(true, Instant::now());
    assert!(!saver.is_being_added(&key(1)));
}

#[test]
fn the_pacing_is_in_the_hands_of_a_clock_passed_in() {
    // The source never reads the time itself: every wait is worked out from `now`.
    let src = include_str!("dm_save.rs").replace("\r\n", "\n");
    let body = src.split("#[cfg(test)]\n#[path").next().unwrap();
    assert!(!body.contains("Instant::now"));
    assert!(!body.contains("SystemTime::now"));
}

#[test]
fn the_wait_is_spread_by_a_fifth_either_way_and_owing_a_removal_starts_the_count_again() {
    let wait_with = |unit: f64| {
        let mut saver = DmSaver::with_waits(Duration::from_secs(100), Duration::from_secs(1000));
        saver.fix_jitter(unit);
        let now = Instant::now();
        saver.ended(false, now);
        saver.not_before.unwrap() - now
    };
    assert_eq!(wait_with(0.5), Duration::from_secs(100));
    assert_eq!(wait_with(0.0), Duration::from_secs(80));
    assert_eq!(wait_with(1.0), Duration::from_secs(120));

    // Many failures in a row stop it; discarding a saved message tries again at once.
    let mut saver = DmSaver::with_waits(Duration::from_secs(1), Duration::from_secs(2));
    let now = Instant::now();
    for _ in 0..MAX_FAILURES {
        saver.ended(false, now);
    }
    assert!(!saver.due(now + Duration::from_secs(1_000_000)));
    saver.owes_removal(key(1));
    assert!(saver.due(now));
}

#[test]
fn a_removal_that_keeps_failing_gives_the_adds_a_turn_and_is_then_tried_again() {
    let mut saver = DmSaver::with_waits(Duration::from_secs(1), Duration::from_secs(2));
    let mut now = Instant::now();
    saver.owes_removal(key(1));
    for _ in 0..REMOVAL_TURNS {
        assert!(saver.removals_first());
        saver.started(vec![], vec![key(1)]);
        saver.ended(false, now);
        now += Duration::from_secs(10);
    }
    assert!(!saver.removals_first(), "the adds go");
    saver.started(vec![key(2)], vec![]);
    saver.ended(true, now);
    assert!(saver.removals_first(), "then the removal again");
    // A removal that works clears it.
    saver.started(vec![], vec![key(1)]);
    saver.ended(true, now);
    assert!(!saver.removals_first());
}
