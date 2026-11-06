//! Building the chat screen's elements from a [`Chat`]: the frame, the room list and the members
//! panel. The conversation in the middle is in `conversation.rs`. Sizes and colours follow the
//! web UI (`ui/src/components/`, `ui/assets/tailwind.css`).

use rune::tok::TextStyle;
use rune::{Color, Cx, El, Pixels, ScrollSpec, col, el, icon, row, spacer, text};
use rune_kit::Kit;

use super::conversation::{avatar, conversation};
use super::icons;
use super::model::{self, Member, Role, Room};
use super::style::{self as st, Palette};
use super::{Chat, Source};
use crate::RiverApp;

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
        .child(rooms(chat, logo, narrow, &p));
    let Some(room) = chat.room() else {
        return screen.child(no_rooms(&p));
    };
    screen = screen.child(conversation(chat, room, narrow, &p, &k));
    if chat.members_shown(narrow) {
        let panel = members(room, narrow, &p);
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
    screen
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

fn rooms(chat: &Chat, logo: Option<&Pixels>, narrow: bool, p: &Palette) -> El {
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
        .child(not_yet(icon_button(
            "create-room",
            "Create room",
            &icons::PLUS,
            p,
        )));
    let width = if narrow { ROOMS_NARROW_W } else { ROOMS_W };
    let mut list = col().key("room-list").gap(2.0).px(10.0).pb(10.0);
    for room in chat.rooms() {
        let selected = chat.room().is_some_and(|r| r.key == room.key);
        list = list.child(room_row(room, selected, width, p));
    }
    let footer = col()
        .key("rooms-footer")
        .px(12.0)
        .pt(12.0)
        .pb(14.0)
        .gap(12.0)
        .child(not_yet(outlined(
            "enter-invite",
            "Enter invite code",
            &icons::INVITE,
            34.0,
            p,
        )))
        .child(source_status(chat.source, p));
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

/// Where the rooms come from, at the foot of the room list. When the app reads rooms from the
/// node, its connection state (connected, reconnecting, disconnected) belongs here, as the web
/// UI's connection pill does.
fn source_status(source: Source, p: &Palette) -> El {
    match source {
        Source::Example => row().key("source-status").px(4.0).child(
            label(
                "Example rooms, not connected to Freenet",
                st::SMALL,
                p.text_muted,
            )
            .wrap()
            .w_full(),
        ),
    }
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

fn no_rooms(p: &Palette) -> El {
    col()
        .key("no-rooms")
        .grow(1.0)
        .h_full()
        .center()
        .gap(8.0)
        .child(label("No rooms yet", st::TITLE, p.text))
        .child(label(
            "Create a room, or enter an invite code to join one.",
            st::BODY,
            p.text_secondary,
        ))
}

// Members -------------------------------------------------------------------------------------

fn members(room: &Room, overlay: bool, p: &Palette) -> El {
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
    let footer = col()
        .key("members-footer")
        .padding(12.0)
        .child(not_yet(outlined(
            "invite",
            "Invite someone",
            &icons::INVITE,
            38.0,
            p,
        )));
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
