//! End-to-end run of the app pointed at a Freenet node it was told about, not found: the node's
//! address comes from the saved setting, the `FREENET_NODE` variable, or a forwarded port (what
//! an SSH tunnel to a node on another machine looks like), always on a port other than 7509.
//!
//! ```text
//! cargo run -p river-app --example remote_node_e2e -- <saved|env|tunnel> <node host:port> <dir> [<forward host:port>]
//! ```
//!
//! - `saved`: the address is written to the setting file in `<dir>` and nothing else names a node.
//! - `env`: the setting file holds a wrong address, and `FREENET_NODE` (set by the caller) names
//!   the node: the variable must win. Also checks that `--node` beats the variable.
//! - `tunnel`: a plain TCP forwarder listens on `<forward host:port>` and relays to the node (it
//!   is not SSH; the node sees the same thing: a loopback peer and a loopback `Host`). The
//!   setting names `localhost:<forward port>`.
//!
//! Then, with the address that was chosen: the setup controller asks the node its version and
//! reaches `Connected` without ever installing, starting or probing a Freenet on this computer
//! (the host below panics if it is asked); a wrong address is reported with that address; the
//! app's rooms link gets its grant approved on the page at the chosen address, registers the
//! chat delegate, creates a room and sees its own message. Run it through
//! `apps/scripts/fresh-node-e2e.sh`, which starts the nodes. It creates a room on the node.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use river_app::address::{NodeAddress, NodeChoice, NodeStore, Source};
use river_app::rooms::token::node_dir;
use river_app::rooms::work::WorkState;
use river_app::rooms::{Access, Action, Command, Link, RoomsUpdate};
use river_app::{Controller, InstallStep, NodeHost, NodeProbe, Setup};

const STEP: Duration = Duration::from_secs(180);

/// A host that knows how to ask a node its version and nothing else: anything that would touch
/// a Freenet on this computer fails the run.
struct AskOnly {
    asked: Mutex<Vec<String>>,
}

impl NodeHost for AskOnly {
    fn probe(&self) -> NodeProbe {
        panic!("the Freenet on this computer was probed for a node the person gave an address for");
    }
    fn probe_at(&self, authority: &str) -> NodeProbe {
        self.asked.lock().unwrap().push(authority.to_owned());
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .redirects(0)
            .build();
        match agent.get(&format!("http://{authority}/v1/version")).call() {
            Ok(r) => match r
                .into_string()
                .ok()
                .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                .and_then(|v| v.get("version")?.as_str().map(str::to_owned))
            {
                Some(version) => NodeProbe::Running { version },
                None => NodeProbe::Outdated {
                    running: None,
                    minimum: "0.2.156".into(),
                },
            },
            Err(ureq::Error::Status(..)) => NodeProbe::Outdated {
                running: None,
                minimum: "0.2.156".into(),
            },
            Err(ureq::Error::Transport(_)) => NodeProbe::Unreachable,
        }
    }
    fn can_install(&self) -> bool {
        true
    }
    fn install(&self, _: &mut dyn FnMut(InstallStep)) -> Result<(), String> {
        panic!("River installed a Freenet for a node the person gave an address for");
    }
    fn start(&self) -> Result<(), String> {
        panic!("River started a Freenet for a node the person gave an address for");
    }
}

/// Relay every connection on `listen` to `to`, both ways.
fn forwarder(listen: &str, to: &str) {
    let listener = TcpListener::bind(listen).expect("the forward port is free");
    let to = to.to_owned();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { return };
            let Ok(server) = TcpStream::connect(&to) else {
                continue;
            };
            for (mut from, mut into) in [
                (client.try_clone().unwrap(), server.try_clone().unwrap()),
                (server, client),
            ] {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 16 * 1024];
                    loop {
                        match from.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if into.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = into.shutdown(std::net::Shutdown::Both);
                });
            }
        }
    });
}

/// Allow the grant on the approval page, as the user would (see `fresh_node_e2e.rs`).
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
    // The node refuses an Allow sent sooner than this after the page was shown (its delay against
    // a click-through); 1700 ms mirrors it with a margin.
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

fn wait_setup(rx: &Receiver<Setup>, what: &str) -> Setup {
    loop {
        let s = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("setup never reached: {what}"));
        if s.is_settled() {
            return s;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mode, node, dir) = match args.as_slice() {
        [m, n, d] | [m, n, d, _] => (m.as_str(), n.as_str(), PathBuf::from(d)),
        _ => {
            eprintln!(
                "usage: remote_node_e2e <saved|env|tunnel> <node host:port> <dir> [<forward host:port>]"
            );
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(&dir).unwrap();
    let node_address = NodeAddress::parse(node).expect("the node address");
    let store = NodeStore::in_dir(&dir);
    let argv: Vec<std::ffi::OsString> = vec!["river".into()];

    // Which address does the app end up with?
    let choice = match mode {
        "saved" => {
            assert!(
                std::env::var_os("FREENET_NODE").is_none(),
                "unset FREENET_NODE for the saved mode"
            );
            store.save(&node_address).unwrap();
            NodeChoice::from_environment(argv, Some(&dir)).unwrap()
        }
        "env" => {
            assert_eq!(
                std::env::var("FREENET_NODE").ok().as_deref(),
                Some(node),
                "the caller sets FREENET_NODE to the node"
            );
            // A wrong saved address that the variable must override.
            store
                .save(&NodeAddress::parse("127.0.0.1:1").unwrap())
                .unwrap();
            let c = NodeChoice::from_environment(argv.clone(), Some(&dir)).unwrap();
            // And the flag beats the variable.
            let mut flagged = argv;
            flagged.extend(["--node".into(), "127.0.0.1:2".into()]);
            let f = NodeChoice::from_environment(flagged, Some(&dir)).unwrap();
            assert_eq!(
                (f.address.authority().as_str(), f.source),
                ("127.0.0.1:2", Source::Flag)
            );
            c
        }
        "tunnel" => {
            assert!(std::env::var_os("FREENET_NODE").is_none());
            let forward = args.get(3).expect("a forward address for the tunnel mode");
            let forward = NodeAddress::parse(forward).unwrap();
            forwarder(&forward.authority(), node);
            let through = NodeAddress::parse(&format!("localhost:{}", forward.port())).unwrap();
            store.save(&through).unwrap();
            NodeChoice::from_environment(argv, Some(&dir)).unwrap()
        }
        other => panic!("unknown mode {other}"),
    };
    let (want_source, want) = match mode {
        "saved" => (Source::Saved, node_address.authority()),
        "env" => (Source::Env, node_address.authority()),
        _ => (
            Source::Saved,
            format!(
                "localhost:{}",
                NodeAddress::parse(args.get(3).unwrap()).unwrap().port()
            ),
        ),
    };
    assert_eq!(choice.source, want_source);
    assert_eq!(choice.address.authority(), want);
    assert!(choice.is_existing_node());
    assert!(
        !choice.address.is_default(),
        "the point is a port other than 7509"
    );
    let authority = choice.address.authority();
    println!("{mode}: the app uses {authority}");

    // The setup step: version check, no local node touched.
    let host = Arc::new(AskOnly {
        asked: Mutex::default(),
    });
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let ctl = Controller::reporting_to(host.clone(), move |s| {
        let _ = tx.lock().unwrap().send(s);
    })
    .with_node(choice.clone());
    assert!(!ctl.can_install());
    ctl.check();
    match wait_setup(&rx, "Connected") {
        Setup::Connected { version } => println!("{mode}: the node reports Freenet {version}"),
        other => panic!("setup ended on {other:?}"),
    }
    assert_eq!(
        *host.asked.lock().unwrap(),
        std::slice::from_ref(&authority)
    );
    // A wrong address is said, with the address.
    let wrong = NodeChoice::saved(NodeAddress::parse("127.0.0.1:1").unwrap());
    ctl.set_node(wrong);
    ctl.check();
    match wait_setup(&rx, "Failed") {
        Setup::Failed(m) => {
            assert!(m.contains("127.0.0.1:1"), "{m}");
            println!("{mode}: an address that answers nothing is reported: {m}");
        }
        other => panic!("setup ended on {other:?}"),
    }

    // The rooms link: approval at the chosen address, then a room operation.
    let app_dir = dir.join("app");
    let (utx, urx) = channel();
    let utx = Mutex::new(utx);
    // As the app builds it for a node the person chose.
    let link = Link::for_node(&authority, true, Some(app_dir.clone()), move |u, _| {
        let _ = utx.lock().unwrap().send(u);
    });
    link.start();
    let mut approved = false;
    let mut room_made = false;
    let mut saw_text = false;
    let mut sent = false;
    let text = "Hello through a node the app was told about";
    let deadline = Instant::now() + STEP;
    while !saw_text {
        assert!(
            Instant::now() < deadline,
            "the room operation never finished"
        );
        let Ok(update) = urx.recv_timeout(Duration::from_secs(1)) else {
            continue;
        };
        match update {
            RoomsUpdate::Access(Access::WaitingForApproval { approve_url }) => {
                // The page is on the node's origin at the address the app was given.
                assert!(
                    approve_url.starts_with(&format!("http://{authority}/")),
                    "{approve_url}"
                );
                approve(&authority, &approve_url);
                approved = true;
            }
            RoomsUpdate::Access(a) => {
                println!("{mode}: {a:?}");
                assert!(!a.is_stopped(), "stopped: {a:?}");
            }
            RoomsUpdate::Rooms { rooms, work, .. } => {
                if let Some(t) = work
                    .iter()
                    .find(|t| matches!(t.state, WorkState::Failed { .. }))
                {
                    panic!("a task failed: {t:?}");
                }
                match rooms.iter().find(|r| r.name == "Remote node room") {
                    None if !room_made => {
                        room_made = true;
                        link.send(Command {
                            room: String::new(),
                            action: Action::CreateRoom {
                                name: "Remote node room".into(),
                                nickname: "Remote".into(),
                                private: false,
                            },
                        });
                    }
                    Some(room) if room.messages.iter().any(|m| m.text == text) => saw_text = true,
                    // The room is shown: send once, then wait for it to come back.
                    Some(room) if !sent => {
                        sent = true;
                        link.send(Command {
                            room: room.key.clone(),
                            action: Action::Send {
                                text: text.into(),
                                reply_to: None,
                            },
                        });
                    }
                    Some(_) => {}
                    None => {}
                }
            }
            _ => {}
        }
    }
    assert!(approved, "no approval was needed on a fresh directory");
    // The grant is kept for this node only, apart from the one for the node on this computer.
    let saved = node_dir(&app_dir, &authority, true).join("node-grant-token");
    assert!(saved.is_file(), "no grant saved at {}", saved.display());
    assert!(
        !Path::new(&app_dir).join("node-grant-token").exists(),
        "the node on this computer's grant file was written"
    );
    println!("{mode}: approved at {authority}, made a room and saw its own message");
}
