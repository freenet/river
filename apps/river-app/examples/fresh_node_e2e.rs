//! End-to-end run of the native app against two Freenet nodes that River never ran on.
//!
//! Each node is fresh: no River web UI, no chat delegate. On each, the app's own background
//! work ([`Link`]) asks for a grant (approved here as the user would approve it on the page),
//! registers the chat delegate, and reads an empty room list. Then the app on node A creates a
//! room and makes an invitation, the app on node B joins from it, and each sends a message the
//! other receives.
//!
//! ```text
//! cargo run -p river-app --example fresh_node_e2e -- <node A host:port> <node B host:port> <dir>
//! ```
//!
//! `<dir>` holds each app's grant token. Run it through `apps/scripts/fresh-node-e2e.sh`,
//! which starts the two nodes. Never point it at a node with data you care about: it creates
//! a room there.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use river_app::chat::model::Room;
use river_app::rooms::work::WorkState;
use river_app::rooms::{Access, Action, Command, InviteCode, Issues, Link, RoomsUpdate};

/// How long each step may take. Network operations between two loopback nodes take seconds;
/// this is generous so a slow CI runner does not fail it.
const STEP: Duration = Duration::from_secs(180);

/// One app: its link to its node and what it has reported.
struct App {
    name: &'static str,
    authority: String,
    link: Link,
    updates: Receiver<RoomsUpdate>,
    rooms: Vec<Room>,
    issues: Issues,
    work: Vec<river_app::rooms::work::WorkView>,
    invites: Vec<(String, Result<String, String>)>,
    access: Option<Access>,
    /// Why a finished create or join could not store the room's signing key in the delegate,
    /// each time one was seen. Empty when the delegate held the key (the app asks it again
    /// after storing, and a task finishes without a note only when it answered with the key).
    key_notes: Vec<String>,
}

impl App {
    fn start(name: &'static str, authority: &str, dir: &Path) -> App {
        let (tx, rx) = channel();
        let link = Link::with_authority(authority, Some(dir.to_owned()), move |u| {
            let _ = tx.send(u);
        });
        link.start();
        App {
            name,
            authority: authority.to_owned(),
            link,
            updates: rx,
            rooms: Vec::new(),
            issues: Issues::default(),
            work: Vec::new(),
            invites: Vec::new(),
            access: None,
            key_notes: Vec::new(),
        }
    }

    /// Take updates until `done` holds, approving a grant page when one is shown.
    fn until(&mut self, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + STEP;
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "{}: never: {what} (last: {:?})",
                self.name,
                self.access
            );
            let Ok(update) = self.updates.recv_timeout(left.min(Duration::from_secs(1))) else {
                continue;
            };
            match update {
                RoomsUpdate::Access(a) => {
                    println!("{}: {a:?}", self.name);
                    if let Access::WaitingForApproval { approve_url } = &a {
                        approve(&self.authority, approve_url);
                    }
                    assert!(!a.is_stopped(), "{}: stopped: {a:?}", self.name);
                    self.access = Some(a);
                }
                RoomsUpdate::Rooms {
                    rooms,
                    issues,
                    work,
                    ..
                } => {
                    self.rooms = rooms;
                    self.issues = issues;
                    for w in &work {
                        if let WorkState::Done { note: Some(n), .. } = &w.state {
                            self.key_notes.push(n.clone());
                        }
                    }
                    self.work = work;
                }
                RoomsUpdate::Invite { room, link } => self.invites.push((room, link)),
                RoomsUpdate::Status(_) => {}
            }
        }
        println!("{}: {what}", self.name);
    }

    fn send(&self, room: &str, action: Action) {
        self.link.send(Command {
            room: room.to_owned(),
            action,
        });
    }

    fn room(&self, name: &str) -> Option<&Room> {
        self.rooms.iter().find(|r| r.name == name)
    }

    fn has_text(&self, room: &str, text: &str) -> bool {
        self.room(room)
            .is_some_and(|r| r.messages.iter().any(|m| m.text == text))
    }
}

/// Allow the grant on its approval page, as the user would: read the page's nonce, wait out
/// the node's delay before Allow takes effect, then submit Allow from the node's own origin.
fn approve(authority: &str, url: &str) {
    let agent = ureq::agent();
    let page = agent
        .get(url)
        .call()
        .expect("the approval page")
        .into_string()
        .expect("the page's text");
    let nonce = page
        .split("data-nonce=\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .expect("a nonce on the approval page")
        .to_owned();
    // The node refuses an Allow sent sooner than this after the page was shown.
    std::thread::sleep(Duration::from_millis(1700));
    let status = agent
        .post(url)
        .set("Origin", &format!("http://{authority}"))
        .set("Content-Type", "application/json")
        .send_string(&format!("{{\"nonce\":\"{nonce}\",\"allow\":true}}"))
        .map(|r| r.status())
        .unwrap_or_else(|e| panic!("allowing the grant failed: {e}"));
    println!("approved the grant on {authority} (HTTP {status})");
}

/// A fresh store: registered (not missing), with no rooms.
fn ready_and_empty(app: &App) -> bool {
    app.access == Some(Access::Ready) && app.rooms.is_empty() && !app.issues.store_missing
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [a, b, dir] = args.as_slice() else {
        eprintln!("usage: fresh_node_e2e <node A host:port> <node B host:port> <dir>");
        std::process::exit(2);
    };
    let dir = PathBuf::from(dir);
    let (dir_a, dir_b) = (dir.join("app-a"), dir.join("app-b"));
    for d in [&dir_a, &dir_b] {
        std::fs::create_dir_all(d).unwrap();
    }

    let mut alice = App::start("A", a, &dir_a);
    alice.until("registered, with no rooms", ready_and_empty);

    const ROOM: &str = "Fresh node room";
    alice.send(
        "",
        Action::CreateRoom {
            name: ROOM.into(),
            nickname: "Alice".into(),
            private: false,
        },
    );
    alice.until("the room is created and shown", |app| {
        if let Some(t) = app
            .work
            .iter()
            .find(|t| matches!(t.state, WorkState::Failed { .. }))
        {
            panic!("creating failed: {t:?}");
        }
        app.room(ROOM).is_some()
    });
    let key = alice.room(ROOM).unwrap().key.clone();
    alice.send(&key, Action::Invite);
    alice.until("an invitation is made", |app| !app.invites.is_empty());
    let link = match alice.invites.pop().unwrap() {
        (room, Ok(link)) if room == key => link,
        other => panic!("no invitation: {:?}", other.1.map(|_| "<link>")),
    };

    let mut bob = App::start("B", b, &dir_b);
    bob.until("registered, with no rooms", ready_and_empty);
    bob.send(
        "",
        Action::Join {
            code: InviteCode(link),
            nickname: "Bob".into(),
        },
    );
    bob.until("the room is joined and shown", |app| {
        if let Some(t) = app
            .work
            .iter()
            .find(|t| matches!(t.state, WorkState::Failed { .. }))
        {
            panic!("joining failed: {t:?}");
        }
        app.room(ROOM).is_some_and(|r| r.send_block.is_none())
    });

    let from_a = "Hello from the app on node A";
    alice.send(
        &key,
        Action::Send {
            text: from_a.into(),
            reply_to: None,
        },
    );
    bob.until("node B receives node A's message", |app| {
        app.has_text(ROOM, from_a)
    });
    let from_b = "Hello back from the app on node B";
    bob.send(
        &key,
        Action::Send {
            text: from_b.into(),
            reply_to: None,
        },
    );
    alice.until("node A receives node B's message", |app| {
        app.has_text(ROOM, from_b)
    });
    alice.until("node A sees Bob as a member", |app| {
        app.room(ROOM)
            .is_some_and(|r| r.members.iter().any(|m| m.nickname == "Bob"))
    });
    // Each app stored its room's signing key in the delegate: the create and the join both
    // ended without a note about the key.
    for app in [&alice, &bob] {
        assert!(
            app.key_notes.is_empty(),
            "{}: the signing key was not stored: {:?}",
            app.name,
            app.key_notes
        );
    }
    println!("E2E OK");
}
