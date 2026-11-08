//! Building the chat screen's elements from a [`Chat`]: the frame, the room list and the members
//! panel. The conversation in the middle is in `conversation.rs`. Sizes and colours follow the
//! web UI (`ui/src/components/`, `ui/assets/tailwind.css`).

use rune::tok::TextStyle;
use rune::{Color, Cx, El, Pixels, ScrollSpec, col, el, icon, row, spacer, text};
use rune_kit::{Button, ButtonVariant, Checkbox, Dialog, Kit, TextInput};

use super::conversation::{avatar, conversation};
use super::icons;
use super::model::{self, Member, Role, Room};
use super::style::{self as st, Palette};
use super::{Chat, DialogKind, InviteState, LinkStatus, Source};
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
            if r[2] >= NARROW_WIDTH {
                s.chat.members_overlay = false;
            }
        })
        .child(rooms(chat, logo, narrow, &p, &k));
    let mut layer = Vec::new();
    dialogs(chat, &k, &mut layer);
    let Some(room) = chat.room() else {
        return rune_kit::root(screen.child(no_rooms(chat, &p)), layer);
    };
    screen = screen.child(conversation(chat, room, narrow, &p, &k));
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

type Get = fn(&mut RiverApp) -> &mut rune::TextEdit;

/// The create and join dialogs, asking what the web UI's `CreateRoomModal` and
/// `JoinWithCodeModal` ask: a name, a nickname and whether the room is private; or an invite
/// code (or link) and a nickname.
fn dialogs(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    let Some(kind) = chat.form.open else {
        return;
    };
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
        };
    let field = |key: &str, label: &str, placeholder: &str, edit: &rune::TextEdit, get: Get| {
        TextInput::new(edit)
            .label(label.to_owned())
            .placeholder(placeholder.to_owned())
            .width(None)
            .on_edit(get)
            .on_submit(move |s: &mut RiverApp, _| submit(s))
            .build(k)
            .key(key.to_owned())
            .w_full()
    };
    let mut body = col().key("form").w_full().px(16.0).pb(16.0).gap(12.0);
    match kind {
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
            body = body
                .child(field(
                    "invite-code",
                    "Invite code",
                    "Invite code or link",
                    &chat.form.code,
                    |s| &mut s.chat.form.code,
                ))
                .child(field(
                    "join-nickname",
                    "Your nickname",
                    "Your nickname in this room",
                    &chat.form.nickname,
                    |s| &mut s.chat.form.nickname,
                ));
        }
    }
    if let Some(error) = &chat.form.error {
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
                Button::new(submit_label)
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
        .child(label("River", st::BRAND, p.text));
    let heading = row()
        .pl(18.0)
        .pr(12.0)
        .pt(14.0)
        .pb(6.0)
        .center_y()
        .child(label("Rooms", st::SECTION, p.text_muted))
        .child(spacer())
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
    for room in chat.rooms() {
        let selected = chat.room().is_some_and(|r| r.key == room.key);
        list = list.child(room_row(room, selected, width, p));
    }
    let enter = outlined("enter-invite", "Enter invite code", &icons::INVITE, 34.0, p);
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
        });
    for w in &chat.work {
        footer = footer.child(work_row(w, p, k));
    }
    let footer = footer.child(source_status(chat, p));
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
        WorkState::Working | WorkState::Done { .. } => {
            r = r.child(label("Working on it…", st::SMALL, p.text_muted).key("work-status"));
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

/// Where the rooms come from, at the foot of the room list, and for rooms from the node the
/// connection's state, as the web UI's connection pill shows it.
fn source_status(chat: &Chat, p: &Palette) -> El {
    let (text, color) = match chat.source {
        Source::Example => ("Example rooms, not connected to Freenet", p.text_muted),
        Source::Node(LinkStatus::Loading) => ("Catching up with Freenet…", p.text_muted),
        Source::Node(LinkStatus::Live) => ("Connected to Freenet", p.text_muted),
        Source::Node(LinkStatus::Reconnecting) => (
            "Connection to Freenet lost. Reconnecting…",
            p.text_secondary,
        ),
    };
    let mut status = col()
        .key("source-status")
        .px(4.0)
        .gap(4.0)
        .child(label(text, st::SMALL, color).wrap().w_full());
    let issues = &chat.issues;
    let mut notes: Vec<(&str, String)> = Vec::new();
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
    status
}

/// About how many characters of 12 point text fit in a room row's second line.
fn preview_chars(row_width: f32, badge: bool) -> usize {
    let text_w = row_width - 40.0 - if badge { 30.0 } else { 0.0 };
    (text_w / 6.4).max(8.0) as usize
}

fn room_row(room: &Room, selected: bool, width: f32, p: &Palette) -> El {
    let key = room.key.clone();
    let unread = room.unread > 0 && !selected;
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
                .child(label(room.unread.to_string(), st::BADGE, p.accent)),
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
        // The node has no chat delegate under the key this build carries: River in the browser
        // never ran on this node, or it runs a newer delegate than this build knows (rooms
        // migrate forward with the web UI, so an older app no longer finds them).
        Source::Node(_) if chat.issues.store_missing => {
            "River's storage isn't on this computer's Freenet under the version this app knows. \
             If you have never opened River in your browser on this computer, do that first. If \
             you have, this app is probably older than River on the web: update it to see your \
             rooms here."
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
    // A stand-in for the web UI's order, which is you first and then the invite tree from the
    // owner down. These view models carry no invite tree yet, so after you come the owner and
    // then everyone else by name.
    let mut sorted: Vec<&Member> = room.members.iter().collect();
    sorted.sort_by_key(|m| {
        (
            m.id != room.me,
            m.role != Role::Owner,
            m.nickname.to_lowercase(),
        )
    });
    let mut list = col().key("member-list").gap(2.0).px(8.0).pt(8.0);
    for m in sorted {
        list = list.child(member_row(room, m, p));
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
        ));
    if overlay {
        // Over the conversation the panel covers the header's toggle, so it closes itself.
        header = header.child(spacer()).child(
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
                    Button::new("Copy again")
                        .variant(ButtonVariant::Ghost)
                        .on_press_tx(move |_: &mut RiverApp, tx: &mut rune::Tx| {
                            tx.copy(link.clone())
                        })
                        .build(k)
                        .key("invite-copy"),
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

fn member_row(room: &Room, m: &Member, p: &Palette) -> El {
    let mut r = row()
        .key(format!("member-{}", m.id))
        .h(40.0)
        .px(8.0)
        .gap(10.0)
        .radius(8.0)
        .center_y()
        .hover(p.surface)
        .child(avatar(Some(m), m.id, 28.0))
        .child(
            label(m.nickname.clone(), st::BODY, p.text_secondary)
                .truncate()
                .shrink(),
        );
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
    }
    r
}
