//! Building the chat screen's elements from a [`Chat`]. Sizes and colours follow the web UI
//! (`ui/src/components/`, `ui/assets/tailwind.css`).

use rune::tok::TextStyle;
use rune::{Color, Cx, El, Pixels, ScrollSpec, col, el, icon, row, spacer, text};
use rune_kit::{Kit, TextInput};

use super::Chat;
use super::icons;
use super::model::{self, Group, MINUTES_PER_DAY, Member, Message, Role, Room};
use super::style::{self as st, Palette};
use crate::RiverApp;

/// Below this window width the members panel gives way to the conversation, as the web UI
/// shows it only from 1024 pixels.
pub const NARROW_WIDTH: f32 = 1024.0;

const ROOMS_W: f32 = 256.0;
const ROOMS_NARROW_W: f32 = 216.0;
const MEMBERS_W: f32 = 248.0;
const BRAND_H: f32 = 60.0;
const HEADER_H: f32 = 64.0;
const AVATAR: f32 = 36.0;
/// The avatar column of a message row.
const GUTTER: f32 = 60.0;
/// The history's widest row, so long messages stay readable on a wide window.
const HISTORY_MAX_W: f32 = 880.0;

pub fn view(chat: &Chat, version: &str, logo: Option<&Pixels>, cx: &mut Cx<RiverApp>) -> El {
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
        .child(rooms(chat, version, logo, narrow, &p));
    let Some(room) = chat.room() else {
        return screen.child(no_rooms(&p));
    };
    screen = screen.child(conversation(chat, room, narrow, &p, &k));
    if chat.members_shown(narrow) {
        let panel = members(room, narrow, &p);
        if narrow {
            // Dim the conversation under the panel; pressing it closes the panel.
            screen = screen.child(
                el().key("members-scrim")
                    .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                    .fill(p.shadow)
                    .on_press(|s: &mut RiverApp| s.chat.toggle_members(true)),
            );
        }
        screen = screen.child(if narrow {
            // Over the conversation, against the window's right edge.
            panel
                .key("members-overlay")
                .pin(None, Some(0.0), Some(0.0), Some(0.0))
                .shadow(0.0, 40.0, p.shadow)
        } else {
            panel
        });
    }
    screen
}

fn vline(p: &Palette) -> El {
    el().w(1.0).h_full().fill(p.line)
}

fn hline(p: &Palette) -> El {
    el().w_full().h(1.0).fill(p.line)
}

fn label(s: impl Into<String>, style: TextStyle, color: Color) -> El {
    text(s.into()).font(style).color(color)
}

/// A small square button holding an icon.
fn icon_button(key: &str, name: &str, glyph: &'static rune::Icon, p: &Palette) -> El {
    row()
        .key(key.to_owned())
        .role("button")
        .label(name.to_owned())
        .help(name.to_owned())
        .size(26.0, 26.0)
        .radius(6.0)
        .center()
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
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
        .hover(p.surface)
        .cursor(rune::Cursor::Pointer)
        .child(icon(glyph, 15.0).color(p.text_secondary))
        .child(label(name.to_owned(), st::ROOM, p.text_secondary))
}

// The room list -------------------------------------------------------------------------------

fn rooms(chat: &Chat, version: &str, logo: Option<&Pixels>, narrow: bool, p: &Palette) -> El {
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
        .child(
            icon_button("create-room", "Create room", &icons::PLUS, p)
                .on_press(|_: &mut RiverApp| {}),
        );
    let mut list = col().key("room-list").gap(2.0).px(10.0).pb(10.0);
    for room in &chat.rooms {
        let selected = chat.room().is_some_and(|r| r.key == room.key);
        list = list.child(room_row(room, selected, p));
    }
    let footer = col()
        .key("rooms-footer")
        .px(12.0)
        .pt(12.0)
        .pb(14.0)
        .gap(10.0)
        .child(
            outlined("enter-invite", "Enter invite code", &icons::INVITE, 34.0, p)
                .on_press(|_: &mut RiverApp| {}),
        )
        .child(
            col()
                .key("node-status")
                .pt(2.0)
                .px(4.0)
                .gap(3.0)
                .child(
                    row()
                        .gap(8.0)
                        .center_y()
                        .child(el().size(7.0, 7.0).radius(3.5).fill(p.success))
                        .child(label("Connected to Freenet", st::SMALL_STRONG, p.success)),
                )
                .child(
                    label(
                        format!("Node {version} · example rooms"),
                        st::SMALL,
                        p.text_muted,
                    )
                    .truncate(),
                ),
        );
    let column = col()
        .key("rooms")
        .w(if narrow { ROOMS_NARROW_W } else { ROOMS_W })
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

fn room_row(room: &Room, selected: bool, p: &Palette) -> El {
    let key = room.key.clone();
    let unread = room.unread > 0 && !selected;
    let strong = unread || selected;
    let preview = room
        .last_message()
        .map(|m| format!("{}: {}", room.nickname(m.author), m.text))
        .unwrap_or_else(|| "No messages yet".into());
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

// The conversation ----------------------------------------------------------------------------

fn conversation(chat: &Chat, room: &Room, narrow: bool, p: &Palette, k: &Kit) -> El {
    col()
        .key("conversation")
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .child(header(chat, room, narrow, p))
        .child(hline(p))
        .child(history(chat, room, p))
        .child(composer(chat, room, p, k))
}

fn header(chat: &Chat, room: &Room, narrow: bool, p: &Palette) -> El {
    let mut title = row().gap(8.0).center_y().min_w(0.0).child(
        label(room.name.clone(), st::HEADER, p.text)
            .truncate()
            .shrink(),
    );
    if room.private {
        title = title.child(
            row()
                .key("private-badge")
                .h(20.0)
                .px(6.0)
                .gap(4.0)
                .radius(4.0)
                .center_y()
                .border(1.0, p.line_strong)
                .child(icon(&icons::LOCK, 11.0).color(p.text_secondary))
                .child(label("PRIVATE", st::TAG, p.text_secondary)),
        );
    }
    let mut info = col().grow(1.0).min_w(0.0).gap(2.0).child(title);
    if !room.description.is_empty() {
        info = info.child(label(room.description.clone(), st::SMALL, p.text_muted).truncate());
    }
    let open = chat.members_shown(narrow);
    let toggle = row()
        .key("toggle-members")
        .role("button")
        .label(if open { "Hide members" } else { "Show members" })
        .help(if open { "Hide members" } else { "Show members" })
        .h(32.0)
        .px(9.0)
        .gap(6.0)
        .radius(8.0)
        .center_y()
        .fill(if open { p.surface } else { Color::TRANSPARENT })
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut RiverApp| s.chat.toggle_members(narrow))
        .child(icon(&icons::USERS, 18.0).color(if open { p.text } else { p.text_secondary }))
        .child(label(
            room.members.len().to_string(),
            st::SMALL_STRONG,
            if open { p.text } else { p.text_secondary },
        ));
    row()
        .key("room-header")
        .w_full()
        .h(HEADER_H)
        .pl(20.0)
        .pr(12.0)
        .gap(16.0)
        .center_y()
        .child(info)
        .child(toggle)
}

fn history(chat: &Chat, room: &Room, p: &Palette) -> El {
    if room.messages.is_empty() {
        return col()
            .key("history-empty")
            .grow(1.0)
            .min_h(0.0)
            .w_full()
            .center()
            .gap(10.0)
            .child(
                el().size(56.0, 56.0)
                    .radius(16.0)
                    .center()
                    .fill(p.surface)
                    .border(1.0, p.line)
                    .child(icon(&icons::CHAT, 26.0).color(p.text_muted)),
            )
            .child(label("No messages yet", st::TITLE, p.text).mt(6.0))
            .child(label(
                format!("Be the first to say something in {}.", room.name),
                st::BODY,
                p.text_secondary,
            ));
    }
    let mut items = col()
        .key("history")
        .w_full()
        .max_w(HISTORY_MAX_W)
        .self_center()
        .px(8.0)
        .pt(4.0)
        .pb(16.0)
        .gap(14.0);
    let mut day = None;
    for group in model::groups(&room.messages) {
        let d = group.messages[0].time / MINUTES_PER_DAY;
        if day != Some(d) {
            day = Some(d);
            items = items.child(day_divider(d, chat.today, p));
        }
        items = items.child(group_el(room, &group, p));
    }
    col()
        .key("history-scroll")
        .grow(1.0)
        .min_h(0.0)
        .w_full()
        .child(items)
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_edge_fade(18.0)
}

fn day_divider(day: u32, today: u32, p: &Palette) -> El {
    row()
        .key(format!("day-{day}"))
        .w_full()
        .px(16.0)
        .pt(14.0)
        .gap(14.0)
        .center_y()
        .child(el().grow(1.0).h(1.0).fill(p.line))
        .child(label(
            model::day_label(day, today),
            st::SMALL_STRONG,
            p.text_muted,
        ))
        .child(el().grow(1.0).h(1.0).fill(p.line))
}

pub(super) fn avatar(member: Option<&Member>, id: u32, size: f32) -> El {
    let name = member.map_or("?", |m| m.nickname.as_str());
    let (bg, fg) = st::avatar_colors(id);
    el().size(size, size)
        .radius(size / 2.0)
        .center()
        .fill(bg)
        .child(
            text(model::initials(name))
                .font(TextStyle::new((size * 0.38).round(), 700.0))
                .color(fg),
        )
}

/// An outlined small-caps tag after a name ("OWNER").
fn tag(s: &str, p: &Palette) -> El {
    row()
        .h(18.0)
        .px(5.0)
        .radius(4.0)
        .center()
        .border(1.0, p.line_strong)
        .child(label(s.to_owned(), st::TAG, p.text_secondary))
}

fn group_el(room: &Room, group: &Group, p: &Palette) -> El {
    let first = &group.messages[0];
    let author = room.member(group.author);
    let mine = group.author == room.me;
    let mut name_row = row().gap(8.0).center_y().child(label(
        room.nickname(group.author).to_owned(),
        st::NAME,
        if mine { p.accent } else { p.text },
    ));
    if author.is_some_and(|m| m.role == Role::Owner) {
        name_row = name_row.child(tag("OWNER", p));
    }
    name_row = name_row.child(label(model::clock(first.time), st::SMALL, p.text_muted));
    let mut body = col().grow(1.0).min_w(0.0).gap(4.0).child(name_row);
    for m in group.messages {
        body = body.child(message_el(room, m, p));
    }
    let mut face = avatar(author, group.author, AVATAR);
    if mine {
        // The web UI rings your own avatar in the accent colour.
        face = el()
            .size(AVATAR + 6.0, AVATAR + 6.0)
            .radius(AVATAR / 2.0 + 3.0)
            .center()
            .border(1.5, p.accent)
            .child(
                face.size(AVATAR - 2.0, AVATAR - 2.0)
                    .radius(AVATAR / 2.0 - 1.0),
            );
    }
    let mut r = row()
        .key(format!("group-{}", first.id))
        .w_full()
        .pt(6.0)
        .pb(6.0)
        .pr(24.0)
        .radius(8.0)
        .items_start()
        .child(row().w(GUTTER).justify_center().child(face))
        .child(body);
    if mine {
        r = r.fill(p.own_row);
    }
    r
}

fn message_el(room: &Room, m: &Message, p: &Palette) -> El {
    let mut c = col().key(format!("msg-{}", m.id)).w_full().gap(5.0);
    if let Some(reply) = &m.reply_to {
        let author = room.nickname(reply.author).to_owned();
        let line = format!("{author}  {}", reply.preview);
        c = c.child(
            row()
                .key("reply")
                .gap(6.0)
                .center_y()
                .min_w(0.0)
                .child(icon(&icons::REPLY, 13.0).color(p.text_muted))
                .child(
                    rune::rich(
                        line,
                        vec![
                            rune::Span::new(0, author.len())
                                .bold()
                                .color(p.text_secondary),
                        ],
                    )
                    .font(st::QUOTE)
                    .color(p.quote)
                    .truncate()
                    .shrink(),
                ),
        );
    }
    c = c.child(message_body(m, p));
    if !m.reactions.is_empty() {
        let mut pills = row().key("reactions").gap(5.0).pt(2.0).flex_wrap();
        for r in &m.reactions {
            let mine = r.by.contains(&room.me);
            pills = pills.child(
                row()
                    .h(26.0)
                    .px(8.0)
                    .gap(5.0)
                    .radius(13.0)
                    .center_y()
                    .fill(if mine { p.accent_soft } else { p.rail })
                    .border(1.0, if mine { p.accent_line } else { p.line })
                    .hover(p.surface_hover)
                    .child(label(r.emoji.clone(), st::EMOJI, p.text))
                    .child(label(
                        r.by.len().to_string(),
                        st::SMALL_STRONG,
                        if mine { p.accent } else { p.text_secondary },
                    )),
            );
        }
        c = c.child(pills);
    }
    c
}

/// A message's text, with any web links in it styled and pressable.
fn message_body(m: &Message, p: &Palette) -> El {
    let mut content = m.text.clone();
    let links = model::links(&m.text);
    let mut spans: Vec<rune::Span> = links
        .iter()
        .map(|r| {
            rune::Span::new(r.start, r.end)
                .color(p.accent)
                .underline(p.accent_line)
        })
        .collect();
    if m.edited {
        let start = content.len() + 2;
        content.push_str("  (edited)");
        spans.push(
            rune::Span::new(start, content.len())
                .color(p.text_muted)
                .size(12.0),
        );
    }
    let mut body = rune::rich(content, spans)
        .font(st::MESSAGE)
        .color(p.text)
        .w_full()
        .selectable(true);
    for r in links {
        let url = m.text[r.clone()].to_owned();
        body = body.link(rune::Link::new(r, url).hover(rune::LinkPaint::new().underline(p.accent)));
    }
    body.on_link(|_: &mut RiverApp, url: &str, tx: &mut rune::Tx| {
        let _ = tx.open_url(url);
    })
}

fn composer(chat: &Chat, room: &Room, p: &Palette, k: &Kit) -> El {
    let has_text = !chat.composer.text.trim().is_empty();
    let field = TextInput::new(&chat.composer)
        .plain()
        .width(None)
        .height(44.0)
        .text_style(st::MESSAGE.lh(24.0))
        .placeholder(format!("Message {}", room.name))
        .label("Message")
        .on_edit(|s: &mut RiverApp| &mut s.chat.composer)
        .on_submit(|s: &mut RiverApp, _| {
            s.chat.send();
        })
        .build(k)
        .key("composer-field")
        .grow(1.0)
        .min_w(0.0);
    let send = row()
        .key("send")
        .role("button")
        .label("Send")
        .help("Send")
        .size(36.0, 36.0)
        .radius(8.0)
        .center()
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(|s: &mut RiverApp| {
            s.chat.send();
        })
        .child(icon(&icons::SEND, 20.0).color(if has_text { p.accent } else { p.text_muted }));
    let emoji = row()
        .key("emoji")
        .role("button")
        .label("Emoji")
        .help("Emoji")
        .size(36.0, 36.0)
        .radius(8.0)
        .center()
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .child(icon(&icons::SMILE, 19.0).color(p.text_muted));
    col()
        .key("composer")
        .w_full()
        .max_w(HISTORY_MAX_W)
        .self_center()
        .px(20.0)
        .pt(4.0)
        .pb(18.0)
        .child(
            row()
                .key("composer-box")
                .w_full()
                .h(50.0)
                .px(6.0)
                .gap(4.0)
                .center_y()
                .radius(12.0)
                .fill(p.surface)
                .border(
                    1.0,
                    if chat.composer.focused {
                        p.text_muted
                    } else {
                        p.line
                    },
                )
                .shadow(8.0, 24.0, p.shadow)
                .child(emoji)
                .child(field)
                .child(send),
        )
}

// Members -------------------------------------------------------------------------------------

fn members(room: &Room, overlay: bool, p: &Palette) -> El {
    // You first, then the owner, then everyone else by name, as the web UI orders them.
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
            .on_press(|s: &mut RiverApp| s.chat.toggle_members(true)),
        );
    }
    let footer = col().key("members-footer").padding(12.0).child(
        outlined("invite", "Invite someone", &icons::INVITE, 38.0, p)
            .on_press(|_: &mut RiverApp| {}),
    );
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
