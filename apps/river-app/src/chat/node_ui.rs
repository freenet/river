//! Which Freenet node River uses, as the person sees it: the "Node" entry under the room list,
//! the form that picks another node, and the warnings that go with it.
//!
//! The form is the same screen at first run (where it replaces the setup card) and later (a
//! dialog over the rooms). It checks the address as typed ([`NodeAddress::parse`]), then asks
//! the person to confirm when something about the choice needs saying: the node is not on this
//! computer (traffic to it is not encrypted and its client interface has no login), or the
//! person is changing from a node whose rooms are showing (the rooms and the identity River
//! uses belong to the node). What the choice does is [`crate::RiverApp::apply_node`].

use rune::{El, TextEdit, col, row};
use rune_kit::{Button, ButtonVariant, Dialog, Kit, TextInput};

use super::Chat;
use super::style::{self as st, Palette};
use super::view::label;
use crate::RiverApp;
use crate::address::{NodeAddress, NodeChoice, Source};

/// The node in use, and the form that picks another, when it is open.
pub struct NodeUi {
    pub choice: NodeChoice,
    pub form: Option<NodeForm>,
    /// The flag or variable the person gave wasn't an address: the app waits on this until they
    /// choose a node.
    pub problem: Option<String>,
    /// Something to tell the person above the rooms (a saved address that isn't valid).
    pub notice: Option<String>,
}

impl Default for NodeUi {
    fn default() -> Self {
        NodeUi {
            choice: NodeChoice::local(),
            form: None,
            problem: None,
            notice: None,
        }
    }
}

pub struct NodeForm {
    pub input: TextEdit,
    /// Why what was typed was refused.
    pub error: Option<String>,
    /// An address that checked out and waits for the person to confirm it.
    pub confirm: Option<NodeAddress>,
}

impl NodeUi {
    /// Open the form, with the node now in use filled in when it is one the person chose.
    pub fn open(&mut self) {
        let start = if self.choice.is_existing_node() {
            self.choice.address.authority()
        } else {
            String::new()
        };
        self.form = Some(NodeForm {
            input: TextEdit::new(start),
            error: None,
            confirm: None,
        });
    }

    pub fn close(&mut self) {
        self.form = None;
    }

    /// The address to show beside the connection state: only when it is not the default, so the
    /// common case reads as it always has.
    pub fn label(&self) -> Option<String> {
        (!self.choice.address.is_default() || self.choice.is_existing_node())
            .then(|| self.choice.address.authority())
    }

    /// What to say, however the node was chosen and at every start, when it is not on this
    /// computer: the warning that goes with such a node.
    pub fn insecure(&self) -> Option<String> {
        (!self.choice.address.is_loopback()).then(|| {
            format!(
                "Node {}: not encrypted, no login. Use an SSH tunnel or a VPN.",
                self.choice.address
            )
        })
    }

    /// Check what was typed. `Some(address)` when it can be used now; `None` when it was
    /// refused (the form says why) or needs the person's confirmation first. `connected` says
    /// whether rooms from a node are showing, which a change would replace.
    pub fn submit(&mut self, connected: bool) -> Option<NodeAddress> {
        let current = self.choice.address.clone();
        let form = self.form.as_mut()?;
        match NodeAddress::parse(&form.input.text) {
            Err(e) => {
                form.error = Some(e.0);
                None
            }
            Ok(address) => {
                form.error = None;
                if needs_confirmation(&address, &current, connected) {
                    form.confirm = Some(address);
                    None
                } else {
                    Some(address)
                }
            }
        }
    }

    /// The person confirmed: the address that was waiting.
    pub fn confirm(&mut self) -> Option<NodeAddress> {
        self.form.as_mut()?.confirm.take()
    }

    /// Back from the confirmation to the address.
    pub fn back(&mut self) {
        if let Some(f) = &mut self.form {
            f.confirm = None;
        }
    }
}

/// A node that is not on this computer is always confirmed. A node on this computer (an SSH
/// tunnel's end included) is confirmed when it would replace the rooms showing.
fn needs_confirmation(address: &NodeAddress, current: &NodeAddress, connected: bool) -> bool {
    !address.is_loopback() || (connected && address != current)
}

/// Said before connecting to a node that is not on this computer.
pub fn remote_warning(address: &NodeAddress) -> String {
    format!(
        "{address} is not on this computer. What River exchanges with it, your messages and \
         the permission it is given included, crosses the network without encryption, and \
         the node's client interface has no login: anyone who can reach it can use it (see \
         freenet-core#5264). Connect only over an SSH tunnel or a VPN you trust.\n\nFreenet \
         also gives River its permission only over a connection from the same computer, so a \
         direct connection to another machine is refused today. An SSH tunnel is what works: \
         forward the node's port to this computer, then enter localhost:<port> here."
    )
}

/// Said before replacing the node whose rooms are showing.
pub fn change_note(address: &NodeAddress) -> String {
    format!(
        "Your rooms, and the identity River uses in them, are kept by the node. River will \
         show the rooms of {address} instead of the ones it shows now, and has to be approved \
         on that node first. Nothing is deleted from the current node, and you can switch \
         back. Messages you have written that are not sent yet may be discarded."
    )
}

pub const INTRO: &str = "Enter the address of your Freenet node as host:port, such as \
    localhost:7509. If the node runs on another computer, reach it through an SSH tunnel and \
    enter the tunnel's address on this computer. River opens Freenet's approval page at this \
    address in your browser, so the address has to work from your browser too.";

/// What the form shows, for a card or a dialog to lay out.
pub struct Screen {
    pub title: String,
    pub intro: String,
    pub content: Vec<El>,
    pub actions: Vec<El>,
}

/// The form's screen. `connected` says rooms from a node are showing (a dialog over them, so
/// Cancel closes it) as against first run (Back returns to the setup card).
pub fn screen(chat: &Chat, connected: bool, k: &Kit) -> Option<Screen> {
    let form = chat.node.form.as_ref()?;
    let p = st::palette();
    let cancel = |text: &'static str, key: &'static str| {
        Button::new(text)
            .variant(ButtonVariant::Ghost)
            .on_press(|s: &mut RiverApp| s.node_cancel())
            .build(k)
            .key(key)
    };
    if let Some(address) = &form.confirm {
        let mut content = Vec::new();
        if !address.is_loopback() {
            content.push(warning_box("node-warning", &remote_warning(address), &p));
        }
        if connected {
            content.push(
                label(change_note(address), st::BODY, p.text_secondary)
                    .wrap()
                    .w_full()
                    .key("node-change-note"),
            );
        }
        let go = if connected {
            "Change node"
        } else {
            "Connect anyway"
        };
        return Some(Screen {
            title: format!("Connect to {address}?"),
            intro: String::new(),
            content,
            actions: vec![
                Button::new("Back")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.chat.node.back())
                    .build(k)
                    .key("node-back"),
                Button::new(go)
                    .variant(ButtonVariant::Contrast)
                    .on_press(|s: &mut RiverApp| s.node_confirm())
                    .build(k)
                    .key("node-confirm"),
            ],
        });
    }
    let mut content = vec![
        TextInput::new(&form.input)
            .label("Node address".to_owned())
            .placeholder("localhost:7509".to_owned())
            .width(None)
            .on_edit(|s: &mut RiverApp| &mut s.chat.node.form.as_mut().expect("open").input)
            .on_key(|s: &mut RiverApp, ev, _| {
                s.chat
                    .node
                    .form
                    .as_mut()
                    .is_some_and(|f| super::keys::word_keys(&mut f.input, ev))
            })
            .on_submit(|s: &mut RiverApp, _| s.node_submit())
            .build(k)
            .key("node-input")
            .w_full(),
    ];
    if let Some(error) = &form.error {
        content.push(
            label(error.clone(), st::BODY, p.danger)
                .wrap()
                .w_full()
                .key("node-error"),
        );
    }
    let mut actions = vec![cancel(
        if connected { "Cancel" } else { "Back" },
        "node-cancel",
    )];
    if connected && chat.node.choice.is_existing_node() {
        actions.insert(
            0,
            Button::new("Use Freenet on this computer")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut RiverApp| s.node_use_local())
                .build(k)
                .key("node-use-local"),
        );
    }
    actions.push(
        Button::new("Connect")
            .variant(ButtonVariant::Contrast)
            .on_press(|s: &mut RiverApp| s.node_submit())
            .build(k)
            .key("node-connect"),
    );
    if let Source::Flag | Source::Env = chat.node.choice.source {
        let how = if chat.node.choice.source == Source::Flag {
            "--node"
        } else {
            "FREENET_NODE"
        };
        content.push(
            label(
                format!(
                    "River was started with {how}, which overrides what you choose here the \
                     next time it starts."
                ),
                st::SMALL,
                p.text_muted,
            )
            .wrap()
            .w_full()
            .key("node-override-note"),
        );
    }
    Some(Screen {
        title: "Use a Freenet node I already have".into(),
        intro: INTRO.into(),
        content,
        actions,
    })
}

fn warning_box(key: &str, text: &str, p: &Palette) -> El {
    col()
        .key(key.to_owned())
        .w_full()
        .padding(12.0)
        .radius(8.0)
        .fill(p.warning_bg)
        .border(1.0, p.warning)
        .child(label(text.to_owned(), st::SMALL, p.text).wrap().w_full())
}

/// The form as a dialog over the rooms, when open.
pub(super) fn node_dialog(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    let Some(screen) = screen(chat, true, k) else {
        return;
    };
    let p = st::palette();
    let mut body = col().key("node-form").w_full().px(16.0).pb(16.0).gap(12.0);
    // In the body, where it wraps: a dialog's own description stays on one line.
    if !screen.intro.is_empty() {
        body = body.child(
            label(screen.intro, st::BODY, p.text_secondary)
                .wrap()
                .w_full()
                .key("node-intro"),
        );
    }
    for c in screen.content {
        body = body.child(c);
    }
    let mut buttons = row().w_full().gap(8.0).justify_end().flex_wrap();
    for a in screen.actions {
        buttons = buttons.child(a);
    }
    body = body.child(buttons);
    Dialog::new("node", screen.title)
        .width(460.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.node_cancel())
        .child(body)
        .build(k, layer);
}

/// The lines under the connection state about the node in use: why it can't be trusted with
/// more than a tunnel, and anything the app had to say at start. Empty for the node on this
/// computer.
pub fn banners(node: &NodeUi, p: &Palette) -> Vec<El> {
    let mut out = Vec::new();
    if let Some(n) = &node.notice {
        out.push(
            label(n.clone(), st::SMALL, p.text_secondary)
                .wrap()
                .w_full()
                .key("node-notice"),
        );
    }
    if let Some(w) = node.insecure() {
        out.push(warning_box("node-insecure", &w, p));
    }
    out
}

/// The "Node" entry under the connection state: the address in use, and a way to change it.
pub(super) fn node_entry(chat: &Chat, p: &Palette, k: &Kit) -> El {
    row()
        .key("node-entry")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            label(
                format!("Node: {}", chat.node.choice.address),
                st::SMALL,
                p.text_muted,
            )
            .key("node-address")
            .wrap()
            .grow(1.0),
        )
        .child(
            Button::new("Change")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut RiverApp| s.node_open())
                .build(k)
                .key("node-change"),
        )
}
