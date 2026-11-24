//! Rehearsal of a re-key for the native app (freenet/river#889), run by
//! `apps/scripts/rekey-rehearsal.sh` against one isolated Freenet node.
//!
//! The same file is built twice: in a checkout of an OLDER River (whose chat delegate and room
//! contract are earlier generations) to seed the node, and in this checkout to load it.
//!
//! - `seed <node> <dir>`: create the rooms "Alpha" (with a message), "Beta" and "Gamma", then
//!   leave "Gamma". Everything goes to the older delegate and the older room contract.
//! - `load <node> <dir>`: this build must show Alpha and Beta, with Alpha's message, and not
//!   Gamma; its store must hold both rooms and Gamma's tombstone; it must send into Alpha, under
//!   Alpha's current contract key.
//! - `seed-late <node> <dir>`: the older build again, after the new one ran: create "Delta" (a
//!   late write from an earlier generation).
//! - `load-late <node> <dir>`: this build must show Delta too.
//!
//! Only the River app's own API is used, so the file builds at the older commit too.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use river_app::chat::model::Room;
use river_app::rooms::{Access, Action, Command, Link, RoomsUpdate};

const STEP: Duration = Duration::from_secs(240);

struct App {
    authority: String,
    link: Link,
    updates: Receiver<RoomsUpdate>,
    rooms: Vec<Room>,
    work: Vec<String>,
    /// Every line the room list showed for work, in order.
    work_seen: Vec<String>,
    /// The link reached the node and read the rooms.
    ready: bool,
}

impl App {
    fn start(authority: &str, dir: &Path) -> App {
        let (tx, rx) = channel();
        let link = Link::with_authority(authority, Some(dir.to_owned()), move |u| {
            let _ = tx.send(u);
        });
        link.start();
        App {
            authority: authority.to_owned(),
            link,
            updates: rx,
            rooms: Vec::new(),
            work: Vec::new(),
            work_seen: Vec::new(),
            ready: false,
        }
    }

    fn until(&mut self, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + STEP;
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "never: {what} (rooms: {:?}; work: {:?})",
                self.names(),
                self.work
            );
            let Ok(update) = self.updates.recv_timeout(left.min(Duration::from_secs(1))) else {
                continue;
            };
            match update {
                RoomsUpdate::Access(a) => {
                    println!("access: {a:?}");
                    if let Access::WaitingForApproval { approve_url } = &a {
                        approve(&self.authority, approve_url);
                    }
                    assert!(!a.is_stopped(), "stopped: {a:?}");
                    self.ready |= matches!(a, Access::Ready);
                }
                RoomsUpdate::Rooms { rooms, work, .. } => {
                    self.rooms = rooms;
                    self.work = work
                        .iter()
                        .map(|w| format!("{} [{:?}]", w.what, w.state))
                        .collect();
                    for line in &self.work {
                        if self.work_seen.last() != Some(line) {
                            println!("work: {line}");
                            self.work_seen.push(line.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        println!("ok: {what}");
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

    fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self.rooms.iter().map(|r| r.name.clone()).collect();
        n.sort();
        n
    }

    fn has_text(&self, room: &str, text: &str) -> bool {
        self.room(room)
            .is_some_and(|r| r.messages.iter().any(|m| m.text == text))
    }

    /// Whether `text` was sent in `room` and the node took it.
    fn sent(&self, room: &str, text: &str) -> bool {
        self.room(room).is_some_and(|r| {
            r.messages
                .iter()
                .any(|m| m.text == text && m.pending.is_none())
        })
    }

    fn create(&mut self, name: &str) {
        self.send(
            "",
            Action::CreateRoom {
                name: name.into(),
                nickname: "Ada".into(),
                private: false,
            },
        );
        self.until(&format!("{name} is created"), |app| {
            app.room(name).is_some()
        });
    }

    fn say(&mut self, name: &str, text: &str) {
        let key = self.room(name).unwrap().key.clone();
        self.send(
            &key,
            Action::Send {
                text: text.into(),
                reply_to: None,
            },
        );
        self.until(&format!("{text:?} is sent in {name}"), |app| {
            app.sent(name, text)
        });
    }
}

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
    agent
        .post(url)
        .set("Origin", &format!("http://{authority}"))
        .set("Content-Type", "application/json")
        .send_string(&format!("{{\"nonce\":\"{nonce}\",\"allow\":true}}"))
        .unwrap_or_else(|e| panic!("allowing the grant failed: {e}"));
    println!("approved the grant");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, mode, authority, dir] = args.as_slice() else {
        panic!("usage: rekey_rehearsal seed|load|seed-late|load-late <node> <dir>");
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = App::start(authority, &dir);
    app.until("connected", |app| app.ready);
    match mode.as_str() {
        "seed" => {
            app.create("Alpha");
            app.say("Alpha", "before the re-key");
            app.create("Beta");
            app.create("Gamma");
            let gamma = app.room("Gamma").unwrap().key.clone();
            app.send(&gamma, Action::LeaveRoom);
            app.until("Gamma is left", |app| app.room("Gamma").is_none());
            // Let the leave's writes finish before the run ends.
            std::thread::sleep(Duration::from_secs(3));
            app.until("Gamma stays left", |app| {
                app.room("Gamma").is_none() && app.work.is_empty()
            });
        }
        "load" => {
            app.until(
                "Alpha and Beta are carried over, and Gamma stays left",
                |app| {
                    app.names() == ["Alpha", "Beta"]
                        && app.has_text("Alpha", "before the re-key")
                        && app.work.is_empty()
                },
            );
            app.say("Alpha", "after the re-key");
            assert!(
                app.work_seen
                    .iter()
                    .any(|l| l.contains("from an earlier version of River")),
                "the list said nothing about carrying rooms over: {:?}",
                app.work_seen
            );
        }
        "seed-late" => {
            app.create("Delta");
            app.say("Delta", "a late write");
        }
        "load-late" => {
            app.until(
                "Delta, written late by the older build, is carried over",
                |app| {
                    app.names() == ["Alpha", "Beta", "Delta"]
                        && app.has_text("Delta", "a late write")
                        && app.has_text("Alpha", "after the re-key")
                },
            );
        }
        other => panic!("unknown mode {other}"),
    }
    println!("rehearsal step {mode} passed");
}
