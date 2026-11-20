//! Choosing the node River uses, driven through the window as a person would: the first-run
//! choice, the warning before a node that is not on this computer, and changing the node later.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rune::{Env, Runtime};

use crate::address::{NodeAddress, NodeChoice, Source};
use crate::chat::{self, LinkStatus};
use crate::{
    Access, Controller, InstallStep, NodeHost, NodeProbe, RiverApp, RoomsUpdate, Setup, view,
};

/// A host that records what it was asked, and always finds nothing.
#[derive(Default)]
struct Host {
    local_probes: AtomicUsize,
    installs: AtomicUsize,
    starts: AtomicUsize,
    asked: Mutex<Vec<String>>,
}

impl NodeHost for Host {
    fn probe(&self) -> NodeProbe {
        self.local_probes.fetch_add(1, Ordering::SeqCst);
        NodeProbe::NotInstalled
    }
    fn probe_at(&self, authority: &str) -> NodeProbe {
        self.asked.lock().unwrap().push(authority.to_owned());
        NodeProbe::Unreachable
    }
    fn can_install(&self) -> bool {
        true
    }
    fn install(&self, _: &mut dyn FnMut(InstallStep)) -> Result<(), String> {
        self.installs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn start(&self) -> Result<(), String> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn app_with(setup: Setup, access: Access) -> (Runtime<RiverApp>, Arc<Host>) {
    let host = Arc::new(Host::default());
    let ctl = Controller::reporting_to(host.clone(), |_| {});
    let state = RiverApp {
        ctl: Some(ctl),
        setup,
        access,
        ..RiverApp::detached()
    };
    let env = Env {
        system_dark: true,
        scale: 1.0,
    };
    let mut rt = Runtime::new(state, view, (900.0, 700.0), env);
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    (rt, host)
}

/// A window on the rooms from a node, as the live app shows them.
fn rooms_app() -> (Runtime<RiverApp>, Arc<Host>) {
    let (mut rt, host) = app_with(
        Setup::Connected {
            version: "0.2.156".into(),
        },
        Access::Ready,
    );
    rt.update(|s, tx| {
        s.chat.source = chat::Source::Node(LinkStatus::Live);
        s.apply_rooms(
            RoomsUpdate::Rooms {
                rooms: chat::example::data().rooms,
                open: None,
                modes: Default::default(),
                settled: Vec::new(),
                status: LinkStatus::Live,
                now: 29_000_000,
                issues: Default::default(),
                work: Vec::new(),
            },
            tx,
        );
    });
    rt.frame(0.1);
    (rt, host)
}

fn press(rt: &mut Runtime<RiverApp>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
    rt.frame(0.2);
}

fn texts(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.elements().into_iter().filter_map(|e| e.text).collect()
}

fn shows(rt: &Runtime<RiverApp>, needle: &str) -> bool {
    texts(rt).iter().any(|t| t.contains(needle))
}

fn type_address(rt: &mut Runtime<RiverApp>, address: &str) {
    let address = address.to_owned();
    rt.update(move |s, _| {
        s.chat
            .node
            .form
            .as_mut()
            .expect("the form is open")
            .input
            .replace_text(&address);
    });
    rt.frame(0.3);
}

fn wait_for_probe(host: &Host, authority: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !host.asked.lock().unwrap().iter().any(|a| a == authority) {
        assert!(
            std::time::Instant::now() < deadline,
            "{authority} never asked"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn node_of(rt: &Runtime<RiverApp>) -> NodeChoice {
    rt.state.chat.node.choice.clone()
}

#[test]
fn first_run_offers_a_node_the_person_already_has_next_to_installing() {
    let (mut rt, _) = app_with(Setup::NotInstalled, Access::Connecting);
    assert!(rt.find("install").is_some());
    assert!(rt.find("use-node").is_some());
    assert!(shows(&rt, "Use a Freenet node I already have"));
    press(&mut rt, "use-node");
    // The form replaces the setup card: an address field, no install.
    assert!(rt.find("node-input").is_some());
    assert!(rt.find("node-connect").is_some());
    assert!(rt.find("install").is_none());
    assert!(rt.find("manual").is_none());
    // The help says what address to give for a tunnel.
    assert!(shows(&rt, "SSH tunnel"));
    assert!(shows(&rt, "approval page"));
    // Back returns to the setup card.
    press(&mut rt, "node-cancel");
    assert!(rt.find("install").is_some());
    assert!(rt.find("node-input").is_none());
}

#[test]
fn an_address_the_form_refuses_is_explained_and_nothing_changes() {
    let (mut rt, host) = app_with(Setup::NotInstalled, Access::Connecting);
    press(&mut rt, "use-node");
    for bad in ["", "http://my-server:7509/", "user@host:7509", "host:99999"] {
        type_address(&mut rt, bad);
        press(&mut rt, "node-connect");
        assert!(rt.find("node-error").is_some(), "{bad:?}");
        assert!(rt.find("node-input").is_some(), "{bad:?}");
        assert_eq!(node_of(&rt), NodeChoice::local(), "{bad:?}");
    }
    assert!(host.asked.lock().unwrap().is_empty());
}

#[test]
fn an_address_on_this_computer_is_checked_at_once_without_a_warning() {
    let (mut rt, host) = app_with(Setup::NotInstalled, Access::Connecting);
    press(&mut rt, "use-node");
    type_address(&mut rt, "localhost:40302");
    press(&mut rt, "node-connect");
    assert!(rt.find("node-warning").is_none());
    wait_for_probe(&host, "localhost:40302");
    assert_eq!(node_of(&rt).address.authority(), "localhost:40302");
    assert_eq!(node_of(&rt).source, Source::Saved);
    assert!(rt.state.chat.node.form.is_none());
    // The node on this computer was never asked, installed or started.
    assert_eq!(host.local_probes.load(Ordering::SeqCst), 0);
    assert_eq!(host.installs.load(Ordering::SeqCst), 0);
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
}

#[test]
fn a_node_on_another_machine_asks_first_and_says_why() {
    let (mut rt, host) = app_with(Setup::NotInstalled, Access::Connecting);
    press(&mut rt, "use-node");
    type_address(&mut rt, "192.168.1.20:7509");
    press(&mut rt, "node-connect");
    // Nothing was contacted or changed yet.
    assert!(host.asked.lock().unwrap().is_empty());
    assert_eq!(node_of(&rt), NodeChoice::local());
    assert!(rt.find("node-warning").is_some());
    assert!(shows(&rt, "192.168.1.20:7509 is not on this computer"));
    assert!(shows(&rt, "without encryption"));
    assert!(shows(&rt, "no login"));
    assert!(shows(&rt, "SSH tunnel"));
    // Back keeps the form with what was typed; confirming goes on.
    press(&mut rt, "node-back");
    assert!(rt.find("node-input").is_some());
    assert!(host.asked.lock().unwrap().is_empty());
    press(&mut rt, "node-connect");
    press(&mut rt, "node-confirm");
    wait_for_probe(&host, "192.168.1.20:7509");
    assert_eq!(node_of(&rt).address.authority(), "192.168.1.20:7509");
    assert_eq!(host.installs.load(Ordering::SeqCst), 0);
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
}

#[test]
fn escape_or_cancel_in_the_warning_changes_nothing() {
    let (mut rt, host) = app_with(Setup::NotInstalled, Access::Connecting);
    press(&mut rt, "use-node");
    type_address(&mut rt, "my-server.example:7509");
    press(&mut rt, "node-connect");
    assert!(rt.find("node-confirm").is_some());
    press(&mut rt, "node-back");
    press(&mut rt, "node-cancel");
    assert!(rt.state.chat.node.form.is_none());
    assert_eq!(node_of(&rt), NodeChoice::local());
    assert!(host.asked.lock().unwrap().is_empty());
}

#[test]
fn a_node_the_person_has_is_never_offered_an_install() {
    let (mut rt, _) = app_with(
        Setup::Failed("Nothing answered at x:1.".into()),
        Access::Connecting,
    );
    rt.update(|s, _| {
        s.chat.node.choice = NodeChoice::saved(NodeAddress::parse("x:1").unwrap());
        s.ctl = Some(
            Controller::reporting_to(Arc::new(Host::default()), |_| {})
                .with_node(s.chat.node.choice.clone()),
        );
    });
    rt.frame(0.3);
    assert!(rt.find("install").is_none());
    assert!(rt.find("manual").is_none());
    assert!(rt.find("retry").is_some());
    assert!(rt.find("use-node").is_some());
    assert!(shows(&rt, "Use a different node"));
    assert!(shows(&rt, "Nothing answered at x:1."));
}

#[test]
fn the_rooms_screen_has_a_node_entry_that_opens_the_form() {
    let (mut rt, _) = rooms_app();
    assert!(rt.find("node-entry").is_some());
    assert!(shows(&rt, "Node: 127.0.0.1:7509"));
    // The default node reads as it always has.
    assert!(shows(&rt, "Connected"));
    assert!(!shows(&rt, "Connected to"));
    press(&mut rt, "node-change");
    assert!(rt.find("node-input").is_some());
    // Over the rooms, not replacing them.
    assert!(rt.find("rooms").is_some());
    // The choice back to this computer is only offered when another node is in use.
    assert!(rt.find("node-use-local").is_none());
}

#[test]
fn a_node_that_is_not_the_default_is_named_beside_the_connection() {
    let (mut rt, _) = rooms_app();
    rt.update(|s, _| {
        s.chat.node.choice = NodeChoice::saved(NodeAddress::parse("localhost:40302").unwrap());
    });
    rt.frame(0.3);
    assert!(shows(&rt, "Connected to localhost:40302"));
    assert!(shows(&rt, "Node: localhost:40302"));
    for (status, words) in [
        (LinkStatus::Loading, "Connecting to localhost:40302..."),
        (
            LinkStatus::Reconnecting,
            "Disconnected from localhost:40302",
        ),
    ] {
        rt.update(move |s, _| s.chat.note_status(status));
        rt.update(move |s, _| s.chat.source = chat::Source::Node(status));
        rt.frame(0.4);
        assert!(shows(&rt, words), "{words}");
    }
}

#[test]
fn changing_the_node_says_what_it_means_and_replaces_the_rooms_only_when_confirmed() {
    let (mut rt, host) = rooms_app();
    assert!(shows(&rt, "Town Square"));
    press(&mut rt, "node-change");
    type_address(&mut rt, "localhost:40302");
    press(&mut rt, "node-connect");
    // On this computer, so no network warning, but the change is explained.
    assert!(rt.find("node-warning").is_none());
    assert!(rt.find("node-change-note").is_some());
    assert!(shows(&rt, "approved on that node first"));
    assert!(shows(&rt, "Nothing is deleted from the current node"));
    // Nothing happened yet: same rooms, same node, nobody asked.
    assert!(shows(&rt, "Town Square"));
    assert_eq!(node_of(&rt), NodeChoice::local());
    assert!(host.asked.lock().unwrap().is_empty());

    press(&mut rt, "node-confirm");
    wait_for_probe(&host, "localhost:40302");
    assert_eq!(node_of(&rt).address.authority(), "localhost:40302");
    // The rooms were the previous node's: gone, and the app is back at checking the new one.
    assert!(!shows(&rt, "Town Square"));
    assert!(rt.state.chat.rooms().is_empty());
    assert_eq!(rt.state.access, Access::Connecting);
    assert!(matches!(rt.state.setup, Setup::Checking | Setup::Failed(_)));
    assert_eq!(host.installs.load(Ordering::SeqCst), 0);
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
}

#[test]
fn changing_to_a_node_on_another_machine_shows_both_notes() {
    let (mut rt, _) = rooms_app();
    press(&mut rt, "node-change");
    type_address(&mut rt, "home.example.org");
    press(&mut rt, "node-connect");
    assert!(rt.find("node-warning").is_some());
    assert!(rt.find("node-change-note").is_some());
    // A bare host gets Freenet's port, and the confirmation names exactly what will be used.
    assert!(shows(&rt, "Connect to home.example.org:7509?"));
}

#[test]
fn going_back_to_the_freenet_on_this_computer() {
    let (mut rt, host) = rooms_app();
    rt.update(|s, _| {
        s.apply_node(NodeChoice::saved(
            NodeAddress::parse("localhost:40302").unwrap(),
        ));
    });
    rt.update(|s, tx| {
        // Rooms of the other node arrive.
        s.chat.source = chat::Source::Node(LinkStatus::Live);
        let _ = tx;
        s.setup = Setup::Connected {
            version: "0.2.156".into(),
        };
        s.access = Access::Ready;
    });
    rt.frame(0.3);
    press(&mut rt, "node-change");
    press(&mut rt, "node-use-local");
    assert_eq!(node_of(&rt), NodeChoice::local());
    assert!(!rt.state.ctl.as_ref().unwrap().node().is_existing_node());
    // The local node is checked the local way, and a missing one offers its install again.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while host.local_probes.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "never asked locally");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn a_node_given_on_the_command_line_says_so_in_the_form() {
    let (mut rt, _) = rooms_app();
    rt.update(|s, _| {
        s.chat.node.choice = NodeChoice {
            address: NodeAddress::parse("localhost:40302").unwrap(),
            source: Source::Flag,
        };
    });
    press(&mut rt, "node-change");
    assert!(rt.find("node-override-note").is_some());
    assert!(shows(&rt, "--node"));
    // The field starts with the address in use, to edit.
    assert_eq!(
        rt.state.chat.node.form.as_ref().unwrap().input.text,
        "localhost:40302"
    );
}

#[test]
fn the_approval_step_names_the_address_it_opens_for_a_node_the_person_has() {
    let url = "http://localhost:40302/v1/app-grants/approve/x".to_owned();
    let (mut rt, _) = app_with(
        Setup::Connected {
            version: "0.2.156".into(),
        },
        Access::WaitingForApproval {
            approve_url: url.clone(),
        },
    );
    // The default node's text is as it was.
    assert!(!shows(&rt, "SSH tunnel"));
    rt.update(|s, _| {
        s.chat.node.choice = NodeChoice::saved(NodeAddress::parse("localhost:40302").unwrap());
    });
    rt.frame(0.3);
    assert!(shows(&rt, "The page is at localhost:40302"));
    assert!(shows(&rt, "SSH tunnel"));
    press(&mut rt, "approve");
    assert_eq!(rt.take_url_requests(), [url]);
}

#[test]
fn an_unreachable_node_can_be_swapped_for_another() {
    let (mut rt, _) = app_with(
        Setup::Connected {
            version: "0.2.156".into(),
        },
        Access::Unreachable("refused".into()),
    );
    assert!(rt.find("use-node").is_some());
    rt.update(|s, _| {
        s.chat.node.choice = NodeChoice::saved(NodeAddress::parse("localhost:40302").unwrap());
    });
    rt.frame(0.3);
    assert!(shows(&rt, "Nothing is answering River at localhost:40302"));
    press(&mut rt, "use-node");
    assert!(rt.find("node-input").is_some());
}

#[test]
fn nothing_but_the_chosen_address_is_ever_contacted_for_the_version() {
    // The version question is built from the validated address and a fixed path: whatever
    // userinfo, path or second host an input carried was refused before it got here.
    for hostile in [
        "good.example:7509@evil.example",
        "evil.example:7509/../x",
        "good.example:7509?x=@evil.example",
        "good.example:7509#@evil.example",
        "good.example:7509\r\nHost: evil.example",
    ] {
        assert!(NodeAddress::parse(hostile).is_err(), "{hostile:?}");
    }
}

#[test]
fn a_node_that_is_not_on_this_computer_is_said_on_every_screen_however_it_was_chosen() {
    for source in [Source::Flag, Source::Env, Source::Saved] {
        let choice = NodeChoice {
            address: NodeAddress::parse("home.example.org:7509").unwrap(),
            source,
        };
        // Before the rooms: checking, failed, approval.
        for (setup, access) in [
            (Setup::Checking, Access::Connecting),
            (Setup::Failed("x".into()), Access::Connecting),
            (
                Setup::Connected {
                    version: "0.2.156".into(),
                },
                Access::Connecting,
            ),
        ] {
            let (mut rt, _) = app_with(setup, access);
            let c = choice.clone();
            rt.update(move |s, _| s.chat.node.choice = c);
            rt.frame(0.3);
            assert!(rt.find("node-insecure").is_some(), "{source:?}");
            assert!(shows(&rt, "not encrypted, no login"));
            assert!(shows(&rt, "home.example.org:7509"));
        }
        // With the rooms showing.
        let (mut rt, _) = rooms_app();
        let c = choice.clone();
        rt.update(move |s, _| s.chat.node.choice = c);
        rt.frame(0.3);
        assert!(rt.find("node-insecure").is_some(), "{source:?}");
    }
    // Never for a node on this computer, a tunnel's end included.
    for address in ["127.0.0.1:7509", "localhost:40302", "[::1]:40302"] {
        let (mut rt, _) = rooms_app();
        let a = address.to_owned();
        rt.update(move |s, _| {
            s.chat.node.choice = NodeChoice::saved(NodeAddress::parse(&a).unwrap());
        });
        rt.frame(0.3);
        assert!(rt.find("node-insecure").is_none(), "{address}");
    }
    // 127.0.0.2 is not what the node accepts as its own, so it gets the warning.
    let (mut rt, _) = rooms_app();
    rt.update(|s, _| {
        s.chat.node.choice = NodeChoice::saved(NodeAddress::parse("127.0.0.2:7509").unwrap());
    });
    rt.frame(0.3);
    assert!(rt.find("node-insecure").is_some());
}

#[test]
fn a_bad_node_argument_is_said_in_the_window_and_nothing_is_checked_until_chosen() {
    let (mut rt, host) = app_with(Setup::Failed(String::new()), Access::Connecting);
    rt.update(|s, _| {
        s.chat.node.problem = Some("--node \"a@b\" isn't a node address: no user name.".into());
    });
    rt.frame(0.3);
    assert!(shows(&rt, "Can't use that node address"));
    assert!(shows(&rt, "--node \"a@b\" isn't a node address"));
    assert!(shows(&rt, "Use a different node"));
    assert!(rt.find("install").is_none());
    assert_eq!(host.local_probes.load(Ordering::SeqCst), 0);
    // Choosing a node goes on; so does going to this computer's Freenet.
    press(&mut rt, "use-node");
    assert!(rt.find("node-input").is_some());
    press(&mut rt, "node-cancel");
    press(&mut rt, "problem-local");
    assert!(rt.state.chat.node.problem.is_none());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while host.local_probes.load(Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "never checked locally"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn a_saved_address_that_is_not_valid_is_said_above_the_rooms() {
    let (mut rt, _) = rooms_app();
    rt.update(|s, _| {
        s.chat.node.notice = Some(
            "Your saved node address http://x/ is not valid; using this computer's Freenet.".into(),
        );
    });
    rt.frame(0.3);
    assert!(rt.find("node-notice").is_some());
    assert!(shows(&rt, "Your saved node address http://x/ is not valid"));
}

/// An app whose settings live in `dir`, with a host that answers `probe`.
fn saving_app(dir: &std::path::Path, answer: NodeProbe) -> RiverApp {
    struct Saving(std::path::PathBuf, NodeProbe);
    impl NodeHost for Saving {
        fn probe(&self) -> NodeProbe {
            NodeProbe::NotInstalled
        }
        fn probe_at(&self, _: &str) -> NodeProbe {
            self.1.clone()
        }
        fn can_install(&self) -> bool {
            true
        }
        fn install(&self, _: &mut dyn FnMut(InstallStep)) -> Result<(), String> {
            Ok(())
        }
        fn start(&self) -> Result<(), String> {
            Ok(())
        }
        fn config_dir(&self) -> Option<std::path::PathBuf> {
            Some(self.0.clone())
        }
    }
    let ctl = Controller::reporting_to(Arc::new(Saving(dir.to_owned(), answer)), |_| {});
    RiverApp {
        ctl: Some(ctl),
        ..RiverApp::detached()
    }
}

#[test]
fn a_typed_address_is_saved_only_once_the_node_answered_as_freenet() {
    let dir = std::env::temp_dir().join(format!("river-save-after-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = crate::address::NodeStore::in_dir(&dir);
    let mut app = saving_app(&dir, NodeProbe::Unreachable);
    app.apply_node(NodeChoice::saved(
        NodeAddress::parse("typo.example:7509").unwrap(),
    ));
    // Chosen for this run, but not remembered while nothing has answered.
    assert_eq!(
        app.chat.node.choice.address.authority(),
        "typo.example:7509"
    );
    assert_eq!(store.load(), None);
    app.set_setup(Setup::Failed("Nothing answered".into()));
    assert_eq!(store.load(), None);
    // Answering as Freenet is what saves it.
    app.set_setup(Setup::Connected {
        version: "0.2.161".into(),
    });
    assert_eq!(store.load().as_deref(), Some("typo.example:7509"));
    // Going back to this computer's Freenet forgets it at once.
    app.apply_node(NodeChoice::local());
    assert_eq!(store.load(), None);
    // A choice replaced before it answered is not what gets saved.
    app.apply_node(NodeChoice::saved(
        NodeAddress::parse("one.example:7509").unwrap(),
    ));
    app.apply_node(NodeChoice::saved(
        NodeAddress::parse("two.example:7509").unwrap(),
    ));
    app.set_setup(Setup::Connected {
        version: "0.2.161".into(),
    });
    assert_eq!(store.load().as_deref(), Some("two.example:7509"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn what_a_node_the_app_has_left_still_delivers_is_dropped() {
    let (mut rt, _) = rooms_app();
    let before = rt.state.chat.rooms().len();
    assert!(before > 0);
    // Another node is chosen: the app's generation moves on.
    rt.update(|s, _| {
        s.apply_node(NodeChoice::saved(
            NodeAddress::parse("localhost:40302").unwrap(),
        ))
    });
    rt.frame(0.3);
    assert!(rt.state.chat.rooms().is_empty());
    let late = |generation: u64| {
        move |s: &mut RiverApp, tx: &mut rune::Tx| {
            s.apply_rooms_from(
                RoomsUpdate::Rooms {
                    rooms: chat::example::data().rooms,
                    open: None,
                    modes: Default::default(),
                    settled: Vec::new(),
                    status: LinkStatus::Live,
                    now: 29_000_000,
                    issues: Default::default(),
                    work: Vec::new(),
                },
                generation,
                tx,
            )
        }
    };
    // An update from the first node (generation 0), already handed over before the change.
    rt.update(late(0));
    assert!(rt.state.chat.rooms().is_empty(), "a stale update was shown");
    // One read under the node now in use is shown.
    let now = rt.state.node_generation;
    rt.update(late(now));
    assert!(!rt.state.chat.rooms().is_empty());
}
