//! Pointing a [`Link`] at another node: the next session asks the new node, what was meant for the
//! old one is dropped, and the old node is not asked again.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::room::LiveRoom;
use super::room_tests::Fixture;
use super::*;

/// A node that answers every request 404 (so River reports it can't take grants), recording the
/// request line and `Host` header of each.
struct FakeNode {
    authority: String,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    count: Arc<AtomicUsize>,
}

fn fake_node() -> FakeNode {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let (log, n) = (requests.clone(), count.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = Vec::new();
            let mut buf = [0u8; 2048];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => request.extend_from_slice(&buf[..k]),
                }
            }
            let text = String::from_utf8_lossy(&request).into_owned();
            // Take the whole request, body included. Closing a socket that still has unread
            // bytes makes the kernel reset the connection, and a client that hears the reset
            // before (or instead of) the answer reports the node unreachable and tries again:
            // macOS does that, with "Invalid argument (os error 22)", which made these tests
            // count extra requests there.
            let body_len: usize = text
                .lines()
                .find_map(|l| {
                    let l = l.to_ascii_lowercase();
                    l.strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse().ok())
                })
                .unwrap_or(0);
            let head_len = request
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map_or(request.len(), |i| i + 4);
            let mut have = request.len().saturating_sub(head_len);
            while have < body_len {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => have += k,
                }
            }
            let line = text.lines().next().unwrap_or("").to_owned();
            let host = text
                .lines()
                .find_map(|l| {
                    l.strip_prefix("Host: ")
                        .or_else(|| l.strip_prefix("host: "))
                })
                .unwrap_or("")
                .to_owned();
            log.lock().unwrap().push((line, host));
            n.fetch_add(1, Ordering::SeqCst);
            // One request per connection, said so, so the client does not keep the connection
            // for a request this node will not answer on it.
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
            // Close politely: stop sending, then wait for the client to finish and close.
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            while matches!(stream.read(&mut buf), Ok(k) if k > 0) {}
        }
    });
    FakeNode {
        authority,
        requests,
        count,
    }
}

/// A node that takes each connection and drops it, so a session on it fails and backs off. The
/// count says how many times it was tried.
fn dropping_node() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    let count = Arc::new(AtomicUsize::new(0));
    let n = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            n.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (authority, count)
}

/// Wait until `done` holds, or fail after a bound.
fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::yield_now();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn link_to(authority: &str) -> (Link, Receiver<RoomsUpdate>) {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let link = Link::with_authority(authority, None, move |u| {
        let _ = tx.lock().unwrap().send(u);
    });
    (link, rx)
}

fn wait_for(rx: &Receiver<RoomsUpdate>, what: &str, done: impl Fn(&Access) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "never saw {what}");
        if let Ok(RoomsUpdate::Access(a)) = rx.recv_timeout(left.min(Duration::from_millis(200)))
            && done(&a)
        {
            return;
        }
    }
}

#[test]
fn a_link_asks_the_node_it_was_pointed_at_and_only_that() {
    let (a, b) = (fake_node(), fake_node());
    let (link, rx) = link_to(&a.authority);
    link.start();
    wait_for(&rx, "the first node's answer", |a| {
        *a == Access::Unsupported
    });
    assert_eq!(a.count.load(Ordering::SeqCst), 1);

    link.set_node(&b.authority, false);
    link.start();
    wait_for(&rx, "the second node's answer", |a| {
        *a == Access::Unsupported
    });
    assert_eq!(
        a.count.load(Ordering::SeqCst),
        1,
        "the old node was asked again"
    );
    let asked = b.requests.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].0.starts_with("POST /v1/app-grants/request"),
        "{asked:?}"
    );
    // The Host the node judges is the address River was given, and nothing else.
    assert_eq!(asked[0].1, b.authority);
}

#[test]
fn a_change_while_a_session_is_still_retrying_starts_the_new_node() {
    let (dead, tried) = dropping_node();
    let b = fake_node();
    let (link, rx) = link_to(&dead);
    link.start();
    // The first session is running, failing to get an answer and backing off.
    eventually("the first session tried the node", || {
        tried.load(Ordering::SeqCst) >= 1
    });
    assert!(link.inner.running.load(Ordering::SeqCst));

    link.set_node(&b.authority, false);
    // As the setup does once the new node has answered its version check.
    link.start();
    wait_for(&rx, "the new node's answer", |a| *a == Access::Unsupported);
    assert_eq!(b.count.load(Ordering::SeqCst), 1);
}

#[test]
fn what_was_meant_for_the_old_node_is_not_sent_to_the_new_one() {
    let (a, b) = (fake_node(), fake_node());
    let (link, rx) = link_to(&a.authority);
    link.send(Command {
        room: "old-room".into(),
        action: Action::Send {
            text: "meant for the first node".into(),
            reply_to: None,
        },
    });
    // What was sent but not yet taken by the old node is held in the outbox too.
    {
        let f = Fixture::new();
        let room = LiveRoom::new(f.stored(f.private_state()));
        let mut outbox = link.inner.outbox.lock().unwrap();
        outbox.command(
            &room,
            Action::Send {
                text: "queued for the first node".into(),
                reply_to: None,
            },
        );
        assert!(!outbox.is_empty(), "the outbox was not loaded");
    }
    link.set_node(&b.authority, false);
    link.start();
    wait_for(&rx, "the new node's answer", |a| *a == Access::Unsupported);
    eventually("the session ended", || {
        !link.inner.running.load(Ordering::SeqCst)
    });
    let leftover = link.inner.inbox.lock().unwrap().try_recv();
    assert!(leftover.is_err(), "a command for the old node survived");
    assert!(
        link.inner.outbox.lock().unwrap().is_empty(),
        "what the old node had not taken was kept for the new one"
    );
}

#[test]
fn a_session_that_waits_for_the_user_is_not_restarted_by_a_second_start() {
    let a = fake_node();
    let (link, rx) = link_to(&a.authority);
    link.start();
    wait_for(&rx, "the answer", |a| *a == Access::Unsupported);
    // It stopped on a state that needs the user. Starting again with no change of node is the
    // user's own retry: one new session, not a loop.
    link.start();
    wait_for(&rx, "the answer again", |a| *a == Access::Unsupported);
    // The session ends on its own and nothing restarts it.
    eventually("the session ended", || {
        !link.inner.running.load(Ordering::SeqCst)
    });
    assert_eq!(a.count.load(Ordering::SeqCst), 2);
    assert!(!link.inner.restart.load(Ordering::SeqCst));
}
