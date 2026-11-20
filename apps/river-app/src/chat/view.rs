//! Building the chat screen's elements from a [`Chat`]: the frame, the room list and the members
//! panel. The conversation in the middle is in `conversation.rs`. Sizes and colours follow the
//! web UI (`ui/src/components/`, `ui/assets/tailwind.css`).

use rune::tok::TextStyle;
use rune::{Color, Cx, El, Pixels, ScrollSpec, col, el, icon, row, spacer, text};
use rune_kit::{Button, ButtonVariant, Checkbox, Dialog, Kit, TextInput};

use super::conversation::{avatar, conversation, mention_overlay, picker_overlay};
use super::icons;
use super::model::{self, Member, Role, Room};
use super::style::{self as st, Palette};
use super::visual_fold;
use super::{Chat, DialogKind, InviteState, JoinPreview, LinkStatus, Source};
use crate::RiverApp;
use crate::rooms::work::{WorkState, WorkView};

/// Below this window width the members panel gives way to the conversation, as the web UI
/// shows it beside the conversation only from 1024 pixels.
pub const NARROW_WIDTH: f32 = 1024.0;

const ROOMS_W: f32 = 256.0;
const ROOMS_NARROW_W: f32 = 216.0;
const MEMBERS_W: f32 = 248.0;
const BRAND_H: f32 = 60.0;
pub(super) const HEADER_H: f32 = 64.0;

/// The tooltip on controls that are drawn but do nothing yet.
pub(super) const NOT_YET: &str = "Not available yet";

pub fn view(chat: &Chat, logo: Option<&Pixels>, cx: &mut Cx<RiverApp>) -> El {
    let p = st::palette();
    let k = Kit::neutral(true);
    let narrow = cx
        .layout_with("chat", |r| r[2] < NARROW_WIDTH)
        .unwrap_or(false);
    let mut screen = row()
        .key("chat")
        .w_full()
        .h_full()
        .fill(p.bg)
        // The narrow layout's members overlay is modal. A frame laid out wide shows no
        // overlay, so close it then: otherwise it would come back, and take focus from the
        // composer, the next time the window narrows.
        .on_layout(|s: &mut RiverApp, r: [f32; 4]| {
            s.chat.narrow = r[2] < NARROW_WIDTH;
            if r[2] >= NARROW_WIDTH {
                s.chat.members_overlay = false;
            }
        })
        .child(rooms(chat, logo, narrow, &p, &k));
    let mut layer = Vec::new();
    dialogs(chat, &k, &mut layer);
    super::identity::export_dialog(chat, &k, &mut layer);
    super::background::background_dialog(chat, &k, &mut layer);
    super::node_status::dialog(chat, &k, &mut layer);
    super::dnd::dnd_dialog(chat, &k, &mut layer);
    super::node_ui::node_dialog(chat, &k, &mut layer);
    if chat.update.settings_open {
        super::update_view::settings_dialog(&chat.update, &k, &mut layer);
    }
    super::switcher::switcher_dialog(chat, &k, &mut layer);
    super::overview::overview_dialog(chat, &k, &mut layer);
    if chat.shortcuts_open {
        let win_h = cx.layout_with("chat", |r| r[3]).unwrap_or(800.0);
        super::shortcuts::shortcuts_dialog(chat, win_h, &k, &mut layer);
    }
    screen = screen.child(link_menu(chat, &k, &mut layer));
    let Some(room) = chat.room() else {
        return rune_kit::root(screen.child(no_rooms(chat, &p)), layer);
    };
    let win_h = cx.layout_with("chat", |r| r[3]).unwrap_or(800.0);
    // The composer's field grows with the draft, and the search panel gives way to it.
    let field_h = cx.layout_with("composer-field", |r| r[3]).unwrap_or(24.0);
    if let Some(uid) = &chat.member_modal {
        super::member_modal::member_modal(chat, room, uid, &k, &mut layer);
    }
    if let Some(open) = &chat.notify_dialog
        && let Some(room) = chat.rooms().iter().find(|r| &r.key == open)
    {
        super::notify_dialog::notify_dialog(room, chat.mode_of(&room.key), &k, &mut layer);
    }
    if let Some(form) = &chat.details {
        super::details::details_modal(chat, room, form, narrow, win_h, &k, &mut layer);
    }
    screen = screen.child(conversation(chat, room, narrow, win_h, field_h, &p, &k));
    picker_overlay(chat, room, &p, &k, &mut layer);
    mention_overlay(chat, &k, &mut layer);
    super::shortcodes::shortcode_overlay(chat, &k, &mut layer);
    if chat.members_shown(narrow) {
        let panel = members(chat, room, narrow, &p, &k);
        if narrow {
            // Dim everything under the panel, the room list too: while it is open the panel
            // is modal, and a press anywhere outside it only closes it.
            screen = screen.child(
                el().key("members-scrim")
                    .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                    .fill(p.scrim),
            );
        }
        screen = screen.child(if narrow {
            // Over the conversation, against the window's right edge. Modal: Tab stays inside
            // it, and Escape or a press outside closes it.
            panel
                .key("members-overlay")
                .pin(None, Some(0.0), Some(0.0), Some(0.0))
                .modal(true, |s: &mut RiverApp| s.chat.members_overlay = false)
        } else {
            panel
        });
    }
    rune_kit::root(screen, layer)
}

/// The menu a right-click on a link opens: Open it, or copy its address. The returned element
/// is the zero-size trigger the menu hangs from.
fn link_menu(chat: &Chat, k: &Kit, layer: &mut Vec<El>) -> El {
    let (url, press) = match &chat.link_menu {
        Some((url, press)) => (url.clone(), Some(*press)),
        None => (String::new(), None),
    };
    let (open_url, copy_url) = (url.clone(), url);
    rune_kit::context_menu(
        "link-menu",
        el().key("link-menu-anchor").size(0.0, 0.0),
        press,
    )
    .item(rune_kit::MenuItem::new("Open link").on_select_tx(
        move |s: &mut RiverApp, tx: &mut rune::Tx| {
            let _ = tx.open_url(&open_url);
            s.chat.link_menu = None;
        },
    ))
    .item(rune_kit::MenuItem::new("Copy link address").on_select_tx(
        move |s: &mut RiverApp, tx: &mut rune::Tx| {
            tx.copy(copy_url.clone());
            s.chat.link_menu = None;
        },
    ))
    .on_close(|s: &mut RiverApp| s.chat.link_menu = None)
    .build(k, layer)
}

type Get = fn(&mut RiverApp) -> &mut rune::TextEdit;

/// The join dialog's invite code box. Each edit decides what the code is
/// ([`Chat::join_code_edited`]). The word keys edit without the field's change hook, so the
/// change is noticed there, as in the import dialog.
fn join_code_field(chat: &Chat, k: &Kit) -> El {
    TextInput::new(&chat.form.code)
        .label("Invite code")
        .placeholder("Invite code or link")
        .width(None)
        .on_edit(|s: &mut RiverApp| &mut s.chat.form.code)
        .on_change(|s: &mut RiverApp, _| s.chat.join_code_edited())
        .on_key(|s: &mut RiverApp, ev, _| {
            let before = s.chat.form.code.text.clone();
            let handled = super::keys::word_keys(&mut s.chat.form.code, ev);
            if handled && s.chat.form.code.text != before {
                s.chat.join_code_edited();
            }
            handled
        })
        .on_submit(|s: &mut RiverApp, _| s.chat.submit_join())
        .build(k)
        .key("invite-code")
        .w_full()
}

/// The create and join dialogs, asking what the web UI's `CreateRoomModal` and
/// `JoinWithCodeModal` ask: a name, a nickname and whether the room is private; or an invite
/// code (or link) and a nickname.
fn dialogs(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    let Some(kind) = chat.form.open else {
        return;
    };
    if kind == DialogKind::Import {
        return super::identity::import_dialog(chat, k, layer);
    }
    let (key, title, description, submit_label, submit): (_, _, _, _, fn(&mut RiverApp)) =
        match kind {
            DialogKind::Create => (
                "create-room",
                "Create a room",
                "You own the room you create. People join it with an invitation from a member.",
                "Create room",
                |s| s.chat.submit_create(),
            ),
            DialogKind::Join => (
                "join-room",
                "Join a room",
                "Paste the invite code or link someone sent you.",
                "Join room",
                |s| s.chat.submit_join(),
            ),
            DialogKind::Import => unreachable!("handled above"),
        };
    let field = |key: &str, label: &str, placeholder: &str, edit: &rune::TextEdit, get: Get| {
        TextInput::new(edit)
            .label(label.to_owned())
            .placeholder(placeholder.to_owned())
            .width(None)
            .on_edit(get)
            .on_key(move |s: &mut RiverApp, ev, _| super::keys::word_keys(get(s), ev))
            .on_submit(move |s: &mut RiverApp, _| submit(s))
            .build(k)
            .key(key.to_owned())
            .w_full()
    };
    let mut body = col().key("form").w_full().px(16.0).pb(16.0).gap(12.0);
    match kind {
        DialogKind::Import => unreachable!("handled above"),
        DialogKind::Create => {
            body = body
                .child(field(
                    "room-name",
                    "Room name",
                    "Room name",
                    &chat.form.name,
                    |s| &mut s.chat.form.name,
                ))
                .child(field(
                    "room-nickname",
                    "Your nickname",
                    "Your nickname in this room",
                    &chat.form.nickname,
                    |s| &mut s.chat.form.nickname,
                ))
                .child(
                    Checkbox::new(chat.form.private)
                        .label("Private room")
                        .description("Messages and names are encrypted for members only.")
                        .on_toggle(|s: &mut RiverApp| s.chat.form.private = !s.chat.form.private)
                        .build(k)
                        .key("room-private"),
                );
        }
        DialogKind::Join => {
            body = body.child(join_code_field(chat, k));
            let note = |s: String| {
                text(s)
                    .font(k.ty.ui)
                    .color(st::palette().text_secondary)
                    .wrap()
                    .w_full()
                    .key("join-note")
            };
            match &chat.form.join {
                JoinPreview::Empty => {}
                JoinPreview::Refused(why) => {
                    body = body.child(
                        text(why.clone())
                            .font(k.ty.ui)
                            .color(st::palette().danger)
                            .wrap()
                            .w_full()
                            .key("join-note"),
                    );
                }
                JoinPreview::New { room } => {
                    // An invitation carries no room name, only the owner's key.
                    let id: String = room.chars().take(8).collect();
                    body = body.child(note(format!(
                        "This invitation is for a new room, not one you have (room id {id}…). \
                         Choose the nickname you will have there."
                    )));
                }
                JoinPreview::Member { name, .. } => {
                    body = body.child(note(format!(
                        "You are already a member of “{}”. This invitation is not needed.",
                        crate::rooms::create::room_name_or_standin(name)
                    )));
                }
                JoinPreview::Have { name, .. } => {
                    body = body.child(note(format!(
                        "You already have “{}” in your list, but you are not a current member \
                         of it. This invitation is not used for it; open the room to see why.",
                        crate::rooms::create::room_name_or_standin(name)
                    )));
                }
            }
            if !matches!(
                chat.form.join,
                JoinPreview::Member { .. } | JoinPreview::Have { .. }
            ) {
                body = body.child(field(
                    "join-nickname",
                    "Your nickname",
                    "Your nickname in this room",
                    &chat.form.nickname,
                    |s| &mut s.chat.form.nickname,
                ));
            }
        }
    }
    // A refusal the preview already shows is not said twice.
    let said = matches!(&chat.form.join, JoinPreview::Refused(why) if Some(why) == chat.form.error.as_ref());
    if let Some(error) = chat.form.error.as_ref().filter(|_| !said) {
        body = body.child(
            text(error.clone())
                .font(k.ty.ui)
                .color(st::palette().danger)
                .wrap()
                .w_full()
                .key("form-error"),
        );
    }
    body = body.child(
        row()
            .w_full()
            .gap(8.0)
            .justify_end()
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.chat.close_dialog())
                    .build(k)
                    .key("form-cancel"),
            )
            .child(
                Button::new(
                    if kind == DialogKind::Join
                        && matches!(
                            chat.form.join,
                            JoinPreview::Member { .. } | JoinPreview::Have { .. }
                        )
                    {
                        "Open room"
                    } else {
                        submit_label
                    },
                )
                .variant(ButtonVariant::Contrast)
                .on_press(move |s: &mut RiverApp| submit(s))
                .build(k)
                .key("form-submit"),
            ),
    );
    Dialog::new(key, title)
        .description(description)
        .width(440.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.chat.close_dialog())
        .child(body)
        .build(k, layer);
}

pub(super) fn vline(p: &Palette) -> El {
    el().w(1.0).h_full().fill(p.line)
}

pub(super) fn hline(p: &Palette) -> El {
    el().w_full().h(1.0).fill(p.line)
}

pub(super) fn label(s: impl Into<String>, style: TextStyle, color: Color) -> El {
    text(s.into()).font(style).color(color)
}

/// A control that is drawn but does nothing yet: dimmed, reported as disabled, no hover and no
/// pointer cursor, with a tooltip that says so.
pub(super) fn not_yet(e: El) -> El {
    e.disabled(true).opacity(0.45).help(NOT_YET)
}

/// A small square button holding an icon.
fn icon_button(key: &str, name: &str, glyph: &'static rune::Icon, p: &Palette) -> El {
    row()
        .key(key.to_owned())
        .role("button")
        .label(name.to_owned())
        .size(26.0, 26.0)
        .radius(6.0)
        .center()
        .child(icon(glyph, 15.0).color(p.text_muted))
}

/// A full-width outlined button, as the rails' footers use.
fn outlined(key: &str, name: &str, glyph: &'static rune::Icon, h: f32, p: &Palette) -> El {
    row()
        .key(key.to_owned())
        .role("button")
        .label(name.to_owned())
        .w_full()
        .h(h)
        .gap(8.0)
        .radius(if h > 36.0 { 12.0 } else { 8.0 })
        .center()
        .border(1.0, p.line_strong)
        .child(icon(glyph, 15.0).color(p.text_secondary))
        .child(label(name.to_owned(), st::ROOM, p.text_secondary))
}

/// The footer's "Do not disturb" button. While it is on the button says so and takes the
/// accent colour, so the state is visible without opening anything.
fn dnd_entry(chat: &Chat, p: &Palette) -> El {
    let on = chat.dnd.active();
    let name = if on {
        "Do not disturb is on"
    } else {
        "Do not disturb"
    };
    let ink = if on { p.accent } else { p.text_secondary };
    row()
        .key("dnd")
        .role("button")
        .label(name.to_owned())
        .w_full()
        .h(34.0)
        .gap(8.0)
        .radius(8.0)
        .center()
        .border(1.0, if on { p.accent } else { p.line_strong })
        .child(icon(&icons::MOON, 15.0).color(ink))
        .child(label(name.to_owned(), st::ROOM, ink))
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(|s: &mut RiverApp| s.chat.open_dnd())
}

// The room list -------------------------------------------------------------------------------

fn rooms(chat: &Chat, logo: Option<&Pixels>, narrow: bool, p: &Palette, k: &Kit) -> El {
    let mark = match logo {
        Some(px) => el().key("logo").size(30.0, 30.0).pixels(px),
        None => el().key("logo").size(30.0, 30.0),
    };
    let brand = row()
        .key("brand")
        .h(BRAND_H)
        .px(18.0)
        .gap(10.0)
        .center_y()
        .child(mark)
        .child(label("River", st::BRAND, p.text))
        .child(spacer())
        .child(
            icon_button("settings", "Settings", &icons::GEAR, p)
                .help("Settings")
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .on_press(|s: &mut RiverApp| s.chat.update.settings_open = true),
        );
    let heading = row()
        .pl(18.0)
        .pr(12.0)
        .pt(14.0)
        .pb(6.0)
        .center_y()
        .child(label("Rooms", st::SECTION, p.text_muted))
        .child(spacer())
        .children(super::zoom::badge(chat))
        .child(super::shortcuts::entry(p))
        .child(if chat.can_change_rooms() {
            icon_button("create-room", "Create room", &icons::PLUS, p)
                .help("Create room")
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .on_press(|s: &mut RiverApp| s.chat.open_dialog(DialogKind::Create))
        } else {
            not_yet(icon_button("create-room", "Create room", &icons::PLUS, p))
        });
    let width = if narrow { ROOMS_NARROW_W } else { ROOMS_W };
    let mut list = col().key("room-list").gap(2.0).px(10.0).pb(10.0);
    if !chat.rooms().is_empty() {
        list = list.child(super::overview::entry(chat, p));
    }
    for room in chat.rooms() {
        let selected = chat.room().is_some_and(|r| r.key == room.key);
        list = list.child(room_row(room, chat.unread_of(room), selected, width, p));
    }
    let enter = outlined("enter-invite", "Enter invite code", &icons::INVITE, 34.0, p);
    let import = outlined("import-id", "Import ID", &icons::IMPORT, 34.0, p);
    let mut footer = col()
        .key("rooms-footer")
        .px(12.0)
        .pt(12.0)
        .pb(14.0)
        .gap(12.0)
        .child(if chat.can_change_rooms() {
            enter
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .on_press(|s: &mut RiverApp| s.chat.open_dialog(DialogKind::Join))
        } else {
            not_yet(enter)
        })
        .child(if chat.can_change_rooms() {
            import
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .on_press(|s: &mut RiverApp| s.chat.open_import())
        } else {
            not_yet(import)
        });
    footer = footer.child(dnd_entry(chat, p));
    if chat.background.is_some() {
        footer = footer.child(
            outlined(
                "background",
                "Background and startup",
                &icons::BACKGROUND,
                34.0,
                p,
            )
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| s.chat.open_background()),
        );
    }
    for w in &chat.work {
        footer = footer.child(work_row(w, p, k));
    }
    if let Some(block) = super::update_view::update_block(&chat.update, p, k) {
        footer = footer.child(block);
    }
    let footer = footer.child(source_status(chat, p, k));
    let column = col()
        .key("rooms")
        .w(width)
        .h_full()
        .fill(p.rail)
        .child(brand)
        .child(heading)
        .child(
            list.scroll(ScrollSpec {
                stick_bottom: false,
            })
            .grow(1.0)
            .min_h(0.0),
        )
        .child(hline(p))
        .child(footer);
    row()
        .key("rooms-panel")
        .h_full()
        .child(column)
        .child(vline(p))
}

/// A room being created or joined: working, or failed with why, Try again and Dismiss.
fn work_row(w: &WorkView, p: &Palette, k: &Kit) -> El {
    let id = w.id;
    let mut r = col()
        .key(format!("work-{id}"))
        .w_full()
        .px(4.0)
        .gap(4.0)
        .child(
            label(w.what.clone(), st::SMALL, p.text_secondary)
                .wrap()
                .w_full(),
        );
    match &w.state {
        WorkState::Working => {
            r = r.child(label("Working on it…", st::SMALL, p.text_muted).key("work-status"));
        }
        // Stored; it shows here once the stored rooms are read again. Should that not happen
        // (a read that keeps failing), the row can still be put away.
        WorkState::Done { .. } => {
            r = r
                .child(
                    label(
                        "Saved. It will show in your rooms in a moment.",
                        st::SMALL,
                        p.text_muted,
                    )
                    .wrap()
                    .w_full()
                    .key("work-status"),
                )
                .child(
                    Button::new("Dismiss")
                        .variant(ButtonVariant::Ghost)
                        .on_press(move |s: &mut RiverApp| s.chat.dismiss_work(id))
                        .build(k)
                        .key(format!("work-dismiss-{id}")),
                );
        }
        WorkState::Failed { why, retry } => {
            r = r.child(
                text(why.clone())
                    .font(st::SMALL)
                    .color(st::palette().danger)
                    .wrap()
                    .w_full()
                    .key("work-why"),
            );
            let mut choices = row().gap(8.0);
            if *retry {
                choices = choices.child(
                    Button::new("Try again")
                        .variant(ButtonVariant::Ghost)
                        .on_press(move |s: &mut RiverApp| s.chat.retry_work(id))
                        .build(k)
                        .key(format!("work-retry-{id}")),
                );
            }
            r = r.child(
                choices.child(
                    Button::new("Dismiss")
                        .variant(ButtonVariant::Ghost)
                        .on_press(move |s: &mut RiverApp| s.chat.dismiss_work(id))
                        .build(k)
                        .key(format!("work-dismiss-{id}")),
                ),
            );
        }
    }
    r
}

/// How the connection to Freenet is shown: the dot, the words, their colour and the tooltip.
pub(super) struct Pill {
    pub dot: rune::Color,
    pub text: String,
    pub color: rune::Color,
    pub tip: String,
}

/// The web UI's connection pill (`ConnectionStatusIndicator`): a green dot and "Connected", an
/// amber dot and "Connecting...", a red dot and "Disconnected" in the error colour. The app
/// has no per-attempt error report to explain, so the tooltip says what the state means.
pub(super) fn connection_pill(source: Source, node: Option<&str>, p: &Palette) -> Pill {
    // A node other than the default is named, so the person can see which one River is on.
    let named = |plain: &str, with: &dyn Fn(&str) -> String| match node {
        Some(n) => with(n),
        None => plain.to_owned(),
    };
    let tip = |t: &str| match node {
        Some(n) => format!("{t} The node is at {n}."),
        None => t.to_owned(),
    };
    match source {
        Source::Example => Pill {
            dot: p.text_muted,
            text: "Example rooms, not connected to Freenet".into(),
            color: p.text_muted,
            tip: "These are example rooms. River is not connected to Freenet.".into(),
        },
        Source::Node(LinkStatus::Loading) => Pill {
            dot: p.warning,
            text: named("Connecting...", &|n| format!("Connecting to {n}...")),
            color: p.text_muted,
            tip: tip("Showing your stored rooms while River catches up with Freenet."),
        },
        Source::Node(LinkStatus::Live) => Pill {
            dot: p.success,
            text: named("Connected", &|n| format!("Connected to {n}")),
            color: p.text_muted,
            tip: tip("Connected to Freenet."),
        },
        Source::Node(LinkStatus::Reconnecting) => Pill {
            dot: p.error,
            text: named("Disconnected", &|n| format!("Disconnected from {n}")),
            color: p.danger,
            tip: tip(
                "Connection to Freenet lost. The rooms shown are as last seen. River is reconnecting.",
            ),
        },
    }
}

/// Where the rooms come from, at the foot of the room list, and for rooms from the node the
/// connection's state, as the web UI's connection pill shows it.
fn source_status(chat: &Chat, p: &Palette, k: &Kit) -> El {
    let pill = connection_pill(chat.source, chat.node.label().as_deref(), p);
    let mut status = col().key("source-status").px(4.0).gap(4.0).child(
        row()
            .key("connection-pill")
            .gap(8.0)
            .center_y()
            .help(format!("{} Press for node status.", pill.tip))
            .role("button")
            .label("Node status")
            .focusable(true)
            .py(6.0)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| s.chat.status.open())
            // Space presses a focused control; Enter does too, as on a button.
            .on_key(|s: &mut RiverApp, ev, _| {
                let enter = ev.key == rune::Key::Enter;
                if enter {
                    s.chat.status.open();
                }
                enter
            })
            .child(
                el().key("connection-dot")
                    .size(7.0, 7.0)
                    .radius(3.5)
                    .fill(pill.dot),
            )
            .child(
                label(pill.text.clone(), st::SMALL_STRONG, pill.color)
                    .key("connection-label")
                    .wrap()
                    .w_full(),
            ),
    );
    if matches!(chat.source, Source::Node(_)) {
        status = status.child(super::node_ui::node_entry(chat, p, k));
        for banner in super::node_ui::banners(&chat.node, p) {
            status = status.child(banner);
        }
    }
    let issues = &chat.issues;
    let mut notes: Vec<(&str, String)> = Vec::new();
    if issues.update_required {
        notes.push((
            "update-required",
            "A newer version of River is out. Update this app to send messages and to create \
             or join rooms. Until then it only shows your rooms. Download the new version \
             from github.com/freenet/river/releases."
                .into(),
        ));
    }
    if !issues.not_updated.is_empty() {
        notes.push((
            "not-updated",
            format!(
                "Couldn't apply the latest from Freenet to: {}. Showing an older copy.",
                issues.not_updated.join(", ")
            ),
        ));
    }
    if !issues.not_following.is_empty() {
        notes.push((
            "not-following",
            format!(
                "Not getting new messages yet for: {}. River keeps trying.",
                issues.not_following.join(", ")
            ),
        ));
    }
    match issues.unreadable {
        0 => {}
        1 => notes.push((
            "unreadable",
            "One of your rooms couldn't be read on this computer.".into(),
        )),
        n => notes.push((
            "unreadable",
            format!("{n} of your rooms couldn't be read on this computer."),
        )),
    }
    for (key, note) in notes {
        status = status.child(
            label(note, st::SMALL, p.text_secondary)
                .key(key)
                .wrap()
                .w_full(),
        );
    }
    if issues.update_required {
        status = status.child(
            Button::new("Get the new version")
                .variant(ButtonVariant::Contrast)
                .on_press_tx(|_: &mut RiverApp, tx: &mut rune::Tx| {
                    let _ = tx.open_url(crate::rooms::generation::RELEASES_URL);
                })
                .build(k)
                .key("get-new-version"),
        );
    }
    status
}

/// About how many characters of 12 point text fit in a room row's second line.
fn preview_chars(row_width: f32, badge: bool) -> usize {
    let text_w = row_width - 40.0 - if badge { 30.0 } else { 0.0 };
    (text_w / 6.4).max(8.0) as usize
}

fn room_row(room: &Room, unread_count: usize, selected: bool, width: f32, p: &Palette) -> El {
    let key = room.key.clone();
    let unread = unread_count > 0 && !selected;
    let strong = unread || selected;
    let preview = room.last_message().map_or_else(
        || "No messages yet".into(),
        |m| {
            model::preview(
                &format!("{}: {}", room.nickname(m.author), m.text),
                preview_chars(width, unread),
            )
        },
    );
    let mut name = row().gap(6.0).center_y().min_w(0.0).child(
        label(
            room.name.clone(),
            if strong { st::ROOM_STRONG } else { st::ROOM },
            if strong { p.text } else { p.text_secondary },
        )
        .truncate()
        .shrink(),
    );
    if room.private {
        name = name.child(
            icon(&icons::LOCK, 13.0)
                .color(p.text_muted)
                .label("Private"),
        );
    }
    let mut r = row()
        .key(format!("room-{}", room.key))
        .role("button")
        .label(room.name.clone())
        .w_full()
        .h(50.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut RiverApp| s.chat.select(&key))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(3.0)
                .child(name)
                .child(label(preview, st::SMALL, p.text_muted).truncate()),
        );
    if unread {
        r = r.child(
            el().key("unread")
                .h(18.0)
                .min_w(20.0)
                .px(6.0)
                .radius(9.0)
                .center()
                .fill(p.accent_soft)
                .child(label(unread_count.to_string(), st::BADGE, p.accent)),
        );
    }
    if selected {
        r.fill(p.surface)
    } else {
        r.hover(p.row_hover)
    }
}

/// What the empty room list says, by where the rooms come from.
pub(super) fn no_rooms_hint(chat: &Chat) -> &'static str {
    match chat.source {
        Source::Example => "Create a room, or enter an invite code to join one.",
        // River has moved to a newer delegate or room contract: this build writes nothing.
        Source::Node(_) if chat.issues.update_required => {
            "A newer version of River is out. Update this app to create or join rooms: download \
             it from github.com/freenet/river/releases."
        }
        // Checking River's version before setting up its storage on a node River never ran on.
        Source::Node(_) if chat.issues.setting_up => "Setting up River's storage on this computer…",
        // The node has no chat delegate under the key this build carries, though River
        // registered it: the node refused or lost it.
        Source::Node(_) if chat.issues.store_missing => {
            "River couldn't set up its storage on this computer's Freenet, so it can't show or \
             save rooms. Restart Freenet and then this app. If it keeps happening, update this \
             app."
        }
        Source::Node(_) => {
            "Create a room, or enter an invite code to join one. Rooms you create or join in \
             River in your browser, on this computer, show up here too."
        }
    }
}

fn no_rooms(chat: &Chat, p: &Palette) -> El {
    let hint = no_rooms_hint(chat);
    col()
        .key("no-rooms")
        .grow(1.0)
        .h_full()
        .center()
        .gap(8.0)
        .child(label("No rooms yet", st::TITLE, p.text))
        .child(
            label(hint, st::BODY, p.text_secondary)
                .key("no-rooms-hint")
                .wrap()
                .max_w(460.0),
        )
}

// Members -------------------------------------------------------------------------------------

fn members(chat: &Chat, room: &Room, overlay: bool, p: &Palette, k: &Kit) -> El {
    // The web UI's order: you first, then the invite tree from the owner down with deputies
    // under who appointed them, narrowed by the search box (see `member_list`).
    let rows = chat.member_rows();
    let searching = chat.member_search.open;
    let mut list = col().key("member-list").gap(2.0).px(8.0).pt(8.0);
    for &i in rows.iter() {
        list = list.child(member_row(room, &room.members[i], p));
    }
    if rows.is_empty() && searching && !chat.member_search.query.text.trim().is_empty() {
        list = list.child(
            label("No matching members", st::SMALL, p.text_muted)
                .key("member-search-empty")
                .px(8.0)
                .py(6.0),
        );
    }
    let mut header = row()
        .h(HEADER_H)
        .pl(16.0)
        .pr(10.0)
        .gap(8.0)
        .center_y()
        .child(label("Members", st::HEADER_SMALL, p.text))
        .child(label(
            room.members.len().to_string(),
            st::SMALL,
            p.text_muted,
        ))
        .child(spacer())
        .child(
            icon_button(
                "member-search-toggle",
                if searching {
                    "Close member search"
                } else {
                    "Search members"
                },
                &rune_kit::glyphs::SEARCH,
                p,
            )
            .expanded(searching)
            .fill(if searching {
                p.surface_hover
            } else {
                Color::TRANSPARENT
            })
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| s.chat.toggle_member_search()),
        );
    if overlay {
        // Over the conversation the panel covers the header's toggle, so it closes itself.
        header = header.child(
            icon_button(
                "close-members",
                "Close members",
                &rune_kit::glyphs::CLOSE,
                p,
            )
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| s.chat.toggle_members(true)),
        );
    }
    let invite = outlined("invite", "Invite someone", &icons::INVITE, 38.0, p);
    let mut footer = col().key("members-footer").padding(12.0).gap(8.0);
    footer = footer.child(if chat.can_change_rooms() {
        invite
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| s.chat.invite())
    } else {
        not_yet(invite)
    });
    // Quiet and secondary, below the invite, as the web UI's.
    let export = row()
        .key("export-id")
        .role("button")
        .label("Export ID")
        .w_full()
        .h(28.0)
        .gap(6.0)
        .radius(8.0)
        .center()
        .child(icon(&icons::EXPORT, 13.0).color(p.text_muted))
        .child(label("Export ID", st::SMALL_STRONG, p.text_muted));
    footer = footer.child(if chat.can_change_rooms() {
        export
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| s.chat.open_export())
    } else {
        not_yet(export)
    });
    match chat.open_invite() {
        None => {}
        Some(InviteState::Making) => {
            footer = footer.child(
                label("Making an invitation…", st::SMALL, p.text_muted).key("invite-status"),
            );
        }
        Some(InviteState::Copied(link)) => {
            let link = link.clone();
            footer = footer
                .child(
                    label(
                        "Invite link copied. Send it only to the person you're inviting: anyone \
                         who has it can join as them.",
                        st::SMALL,
                        p.text_secondary,
                    )
                    .wrap()
                    .w_full()
                    .key("invite-status"),
                )
                .child(
                    label(link.clone(), st::SMALL, p.text_muted)
                        .truncate()
                        .w_full()
                        .key("invite-link"),
                )
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .child(
                            Button::new("Copy link")
                                .variant(ButtonVariant::Ghost)
                                .on_press_tx({
                                    let link = link.clone();
                                    move |_: &mut RiverApp, tx: &mut rune::Tx| tx.copy(link.clone())
                                })
                                .build(k)
                                .key("invite-copy"),
                        )
                        .child(
                            Button::new("Copy message")
                                .variant(ButtonVariant::Ghost)
                                .on_press_tx(|s: &mut RiverApp, tx: &mut rune::Tx| {
                                    if let Some(message) = s.chat.invite_message() {
                                        tx.copy(message);
                                    }
                                })
                                .build(k)
                                .key("invite-copy-message"),
                        ),
                );
        }
        Some(InviteState::Failed(why)) => {
            footer = footer.child(
                text(why.clone())
                    .font(st::SMALL)
                    .color(st::palette().danger)
                    .wrap()
                    .w_full()
                    .key("invite-status"),
            );
        }
    }
    row().key("members-panel").h_full().child(vline(p)).child(
        col()
            .key("members")
            .w(MEMBERS_W)
            .h_full()
            .fill(p.rail)
            .child(header)
            .child(hline(p))
            .children(member_search_box(chat, p, k))
            .child(
                list.scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .grow(1.0)
                .min_h(0.0),
            )
            .child(footer),
    )
}

/// The members box under the header, while open: the field and a button that clears it.
fn member_search_box(chat: &Chat, p: &Palette, k: &Kit) -> Option<El> {
    let s = &chat.member_search;
    if !s.open {
        return None;
    }
    let input = TextInput::new(&s.query)
        .placeholder("Search members")
        .width(None)
        .on_edit(|s: &mut RiverApp| &mut s.chat.member_search.query)
        .on_key(|s: &mut RiverApp, ev, _| {
            if ev.key == rune::Key::Escape {
                return s.chat.escape();
            }
            super::keys::word_keys(&mut s.chat.member_search.query, ev)
        })
        .build(k)
        .key("member-search-input")
        .grow(1.0)
        .min_w(0.0);
    let mut line = row().w_full().gap(6.0).center_y().child(input);
    if !s.query.text.is_empty() {
        line = line.child(
            icon_button(
                "member-search-clear",
                "Clear member search",
                &rune_kit::glyphs::CLOSE,
                p,
            )
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| s.chat.clear_member_query()),
        );
    }
    Some(
        col()
            .key("member-search")
            .role("search")
            .label("Search members")
            .w_full()
            .px(8.0)
            .py(8.0)
            .child(line),
    )
}

fn member_row(room: &Room, m: &Member, p: &Palette) -> El {
    let uid = m.uid.clone();
    let mut r = row()
        .key(format!("member-{}", m.id))
        .h(40.0)
        .px(8.0)
        .gap(10.0)
        .radius(8.0)
        .center_y()
        .role("button")
        .label(format!("Member info: {}", m.nickname))
        .focusable(true)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut RiverApp| s.chat.open_member(&uid))
        .hover(p.surface)
        .child(avatar(Some(m), m.id, 28.0))
        .child(
            label(m.nickname.clone(), st::BODY, p.text_secondary)
                .truncate()
                .shrink(),
        );
    // The web UI shows every member whose name is nothing but symbols as "Unnamed", the same
    // for each; the Member ID in their dialog is what tells them apart, so it is offered here.
    if m.nickname == visual_fold::UNNAMED {
        r = r.help(format!("Member ID {}", m.uid));
    }
    if m.id == room.me {
        r = r.child(label("you", st::SMALL, p.text_muted));
    }
    if m.role == Role::Owner {
        r = r.child(
            icon(&icons::CROWN, 14.0)
                .color(p.text_muted)
                .label("Owner")
                .help("Room owner"),
        );
    } else if let Some(tooltip) = m.deputy_tooltip() {
        // The web UI's member-list shield: an icon after the name, the tooltip saying who
        // appointed them and whether they can ban you.
        r = r.child(
            icon(&icons::SHIELD, 14.0)
                .color(p.text_muted)
                .key(format!("member-deputy-{}", m.id))
                .label(tooltip.clone())
                .help(tooltip),
        );
    }
    r
}
