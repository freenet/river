//! Building the chat screen's elements from a [`Chat`].

use rune::tok::TextStyle;
use rune::{Cx, El, ScrollSpec, col, el, icon, row, spacer, text};
use rune_kit::{Kit, TextInput};

use super::Chat;
use super::icons;
use super::model::{self, Group, MINUTES_PER_DAY, Member, Message, Role, Room};
use super::style::{self as st, Palette};
use crate::RiverApp;

/// Below this window width the members panel gives way to the conversation.
pub const NARROW_WIDTH: f32 = 980.0;

const SIDEBAR_W: f32 = 264.0;
const SIDEBAR_NARROW_W: f32 = 224.0;
const MEMBERS_W: f32 = 248.0;
const HEADER_H: f32 = 64.0;
const AVATAR: f32 = 38.0;
/// The history's widest line, so long messages stay readable on a wide window.
const HISTORY_MAX_W: f32 = 860.0;

pub fn view(chat: &Chat, version: &str, cx: &mut Cx<RiverApp>) -> El {
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
        .child(sidebar(chat, version, narrow, &p));
    let Some(room) = chat.room() else {
        return screen.child(no_rooms(&p));
    };
    screen = screen.child(conversation(chat, room, narrow, &p, &k));
    if chat.members_open && !narrow {
        screen = screen.child(members(room, &p));
    }
    screen
}

fn vline(p: &Palette) -> El {
    el().w(1.0).h_full().fill(p.line)
}

fn hline(p: &Palette) -> El {
    el().w_full().h(1.0).fill(p.line)
}

fn label(s: impl Into<String>, style: TextStyle, color: rune::Color) -> El {
    text(s.into()).font(style).color(color)
}

// The room list -------------------------------------------------------------------------------

fn sidebar(chat: &Chat, version: &str, narrow: bool, p: &Palette) -> El {
    let brand = row()
        .key("brand")
        .h(HEADER_H)
        .px(18.0)
        .gap(10.0)
        .center_y()
        .child(river_mark(26.0))
        .child(label("River", st::BRAND, p.text))
        .child(spacer())
        .child(
            row()
                .key("create-room")
                .role("button")
                .label("Create room")
                .help("Create room")
                .size(30.0, 30.0)
                .center()
                .radius(8.0)
                .fill(p.raised)
                .border(1.0, p.line)
                .hover(p.hover)
                .cursor(rune::Cursor::Pointer)
                .on_press(|_: &mut RiverApp| {})
                .child(icon(&icons::PLUS, 15.0).color(p.text_secondary)),
        );
    let mut list = col().key("room-list").gap(2.0).px(10.0).pb(10.0);
    for room in &chat.rooms {
        let selected = chat.room().is_some_and(|r| r.key == room.key);
        list = list.child(room_row(room, selected, p));
    }
    let status = row()
        .key("node-status")
        .h(52.0)
        .px(18.0)
        .gap(10.0)
        .center_y()
        .child(el().size(8.0, 8.0).radius(4.0).fill(p.online))
        .child(
            col()
                .gap(1.0)
                .grow(1.0)
                .min_w(0.0)
                .child(label("Connected to Freenet", st::SMALL_STRONG, p.text))
                .child(label(
                    format!("Node {version} · example data"),
                    st::SMALL,
                    p.text_faint,
                )),
        );
    let column = col()
        .key("sidebar")
        .w(if narrow { SIDEBAR_NARROW_W } else { SIDEBAR_W })
        .h_full()
        .fill(p.sidebar)
        .child(brand)
        .child(
            row()
                .px(20.0)
                .pt(6.0)
                .pb(8.0)
                .child(label("ROOMS", st::SECTION, p.text_faint)),
        )
        .child(
            list.scroll(ScrollSpec {
                stick_bottom: false,
            })
            .grow(1.0)
            .min_h(0.0),
        )
        .child(hline(p))
        .child(status);
    row()
        .key("sidebar-panel")
        .h_full()
        .child(column)
        .child(vline(p))
}

/// A message's text, with any web links in it styled and pressable.
fn message_body(m: &Message, p: &Palette) -> El {
    let mut content = m.text.clone();
    let links = model::links(&m.text);
    let mut spans: Vec<rune::Span> = links
        .iter()
        .map(|r| {
            rune::Span::new(r.start, r.end)
                .color(p.link)
                .underline(p.link_line)
        })
        .collect();
    if m.edited {
        let start = content.len() + 1;
        content.push_str(" (edited)");
        spans.push(
            rune::Span::new(start, content.len())
                .color(p.text_faint)
                .size(12.0),
        );
    }
    let mut body = rune::rich(content, spans)
        .font(st::MESSAGE)
        .color(p.text_body)
        .w_full()
        .selectable(true);
    for r in links {
        let url = m.text[r.clone()].to_owned();
        body = body.link(rune::Link::new(r, url).hover(rune::LinkPaint::new().underline(p.link)));
    }
    body.on_link(|_: &mut RiverApp, url: &str, tx: &mut rune::Tx| {
        let _ = tx.open_url(url);
    })
}

/// The River mark: a rounded square with two waves.
fn river_mark(size: f32) -> El {
    let p = st::palette();
    el().size(size, size)
        .radius(size * 0.28)
        .linear_gradient(
            std::f32::consts::PI * 0.75,
            [(0.0, p.accent_bright), (1.0, p.accent_deep)],
        )
        .center()
        .child(icon(&icons::WAVES, size * 0.66).color(rune::Color::WHITE))
}

fn room_row(room: &Room, selected: bool, p: &Palette) -> El {
    let key = room.key.clone();
    let glyph = if room.private {
        &icons::LOCK
    } else {
        &icons::HASH
    };
    let unread = room.unread > 0 && !selected;
    let preview = room
        .last_message()
        .map(|m| format!("{}: {}", room.nickname(m.author), m.text))
        .unwrap_or_else(|| "No messages yet".into());
    let mut r = row()
        .key(format!("room-{}", room.key))
        .role("button")
        .label(room.name.clone())
        .w_full()
        .h(52.0)
        .px(10.0)
        .gap(10.0)
        .center_y()
        .radius(8.0)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut RiverApp| s.chat.select(&key))
        .child(
            el().size(30.0, 30.0)
                .radius(8.0)
                .center()
                .fill(if selected { p.accent_soft } else { p.raised })
                .child(icon(glyph, 15.0).color(if selected {
                    p.accent_bright
                } else {
                    p.text_secondary
                })),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(
                    label(
                        room.name.clone(),
                        if unread || selected {
                            st::ROOM_STRONG
                        } else {
                            st::ROOM
                        },
                        if unread || selected {
                            p.text
                        } else {
                            p.text_secondary
                        },
                    )
                    .truncate(),
                )
                .child(label(preview, st::SMALL, p.text_faint).truncate()),
        );
    if unread {
        r = r.child(
            el().key("unread")
                .h(20.0)
                .min_w(20.0)
                .px(6.0)
                .radius(10.0)
                .center()
                .fill(p.accent)
                .child(label(
                    room.unread.to_string(),
                    st::BADGE,
                    rune::Color::WHITE,
                )),
        );
    }
    if selected {
        r.fill(p.selected)
    } else {
        r.hover(p.hover)
    }
}

fn no_rooms(p: &Palette) -> El {
    col()
        .grow(1.0)
        .h_full()
        .center()
        .gap(8.0)
        .child(label("No rooms yet", st::TITLE, p.text))
        .child(label(
            "Create a room or accept an invitation to start chatting.",
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
    let mut title = row()
        .gap(8.0)
        .center_y()
        .child(label(room.name.clone(), st::HEADER, p.text).truncate());
    if room.private {
        title = title.child(
            row()
                .key("private-badge")
                .h(20.0)
                .px(7.0)
                .gap(4.0)
                .radius(6.0)
                .center_y()
                .fill(p.private_soft)
                .child(icon(&icons::LOCK, 11.0).color(p.private))
                .child(label("Private", st::BADGE, p.private)),
        );
    }
    let open = chat.members_open && !narrow;
    let toggle = row()
        .key("toggle-members")
        .role("button")
        .label(if open { "Hide members" } else { "Show members" })
        .help(if open { "Hide members" } else { "Show members" })
        .h(32.0)
        .px(10.0)
        .gap(6.0)
        .radius(8.0)
        .center_y()
        .fill(if open { p.selected } else { p.raised })
        .border(1.0, p.line)
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(|s: &mut RiverApp| s.chat.toggle_members())
        .child(icon(&icons::USERS, 15.0).color(if open { p.text } else { p.text_secondary }))
        .child(label(
            room.members.len().to_string(),
            st::SMALL_STRONG,
            if open { p.text } else { p.text_secondary },
        ));
    row()
        .key("room-header")
        .w_full()
        .h(HEADER_H)
        .px(24.0)
        .gap(16.0)
        .center_y()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(3.0)
                .child(title)
                .child(label(room.description.clone(), st::SMALL, p.text_secondary).truncate()),
        )
        .child(toggle)
}

fn history(chat: &Chat, room: &Room, p: &Palette) -> El {
    let mut items = col()
        .key("history")
        .w_full()
        .max_w(HISTORY_MAX_W)
        .self_center()
        .px(24.0)
        .pt(12.0)
        .pb(16.0);
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
                    .fill(p.raised)
                    .child(icon(&icons::HASH, 26.0).color(p.text_faint)),
            )
            .child(label("No messages yet", st::TITLE, p.text))
            .child(label(
                format!("Be the first to say something in {}.", room.name),
                st::BODY,
                p.text_secondary,
            ));
    }
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
}

fn day_divider(day: u32, today: u32, p: &Palette) -> El {
    row()
        .key(format!("day-{day}"))
        .w_full()
        .pt(14.0)
        .pb(6.0)
        .gap(12.0)
        .center_y()
        .child(el().grow(1.0).h(1.0).fill(p.line))
        .child(label(
            model::day_label(day, today),
            st::SMALL_STRONG,
            p.text_faint,
        ))
        .child(el().grow(1.0).h(1.0).fill(p.line))
}

pub(super) fn avatar(member: Option<&Member>, id: u32, size: f32) -> El {
    let name = member.map_or("?", |m| m.nickname.as_str());
    let (bg, fg, _) = st::avatar_colors(id);
    el().size(size, size)
        .radius(size / 2.0)
        .center()
        .fill(bg)
        .child(
            text(model::initials(name))
                .font(TextStyle::new((size * 0.38).round(), 600.0))
                .color(fg),
        )
}

fn group_el(room: &Room, group: &Group, p: &Palette) -> El {
    let first = &group.messages[0];
    let author = room.member(group.author);
    let mine = group.author == room.me;
    let mut name_row = row().gap(8.0).center_y().child(label(
        room.nickname(group.author).to_owned(),
        st::NAME,
        st::avatar_colors(group.author).2,
    ));
    if author.is_some_and(|m| m.role == Role::Owner) {
        name_row = name_row.child(icon(&icons::CROWN, 12.0).color(p.owner));
    }
    if mine {
        name_row = name_row.child(label("you", st::SMALL, p.text_faint));
    }
    name_row = name_row.child(label(model::clock(first.time), st::SMALL, p.text_faint));
    let mut body = col().grow(1.0).min_w(0.0).gap(4.0).child(name_row);
    for m in group.messages {
        body = body.child(message_el(room, m, p));
    }
    row()
        .key(format!("group-{}", first.id))
        .w_full()
        .pt(12.0)
        .gap(14.0)
        .items_start()
        .child(avatar(author, group.author, AVATAR))
        .child(body)
}

fn message_el(room: &Room, m: &Message, p: &Palette) -> El {
    let mut c = col().key(format!("msg-{}", m.id)).w_full().gap(6.0);
    if let Some(reply) = &m.reply_to {
        c = c.child(
            row()
                .key("reply")
                .gap(10.0)
                .child(el().w(3.0).h_full().radius(1.5).fill(p.accent))
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .py(2.0)
                        .gap(2.0)
                        .child(
                            row()
                                .gap(5.0)
                                .center_y()
                                .child(icon(&icons::REPLY, 12.0).color(p.text_faint))
                                .child(label(
                                    room.nickname(reply.author).to_owned(),
                                    st::SMALL_STRONG,
                                    p.text_secondary,
                                )),
                        )
                        .child(label(reply.preview.clone(), st::SMALL, p.text_faint).truncate()),
                ),
        );
    }
    c = c.child(message_body(m, p));
    if !m.reactions.is_empty() {
        let mut pills = row().key("reactions").gap(6.0).flex_wrap();
        for r in &m.reactions {
            let mine = r.by.contains(&room.me);
            pills = pills.child(
                row()
                    .h(26.0)
                    .px(8.0)
                    .gap(5.0)
                    .radius(13.0)
                    .center_y()
                    .fill(if mine { p.accent_soft } else { p.raised })
                    .border(1.0, if mine { p.accent_line } else { p.line })
                    .child(label(r.emoji.clone(), st::EMOJI, p.text))
                    .child(label(
                        r.by.len().to_string(),
                        st::SMALL_STRONG,
                        if mine {
                            p.accent_bright
                        } else {
                            p.text_secondary
                        },
                    )),
            );
        }
        c = c.child(pills);
    }
    c
}

fn composer(chat: &Chat, room: &Room, p: &Palette, k: &Kit) -> El {
    let has_text = !chat.composer.text.trim().is_empty();
    let field = TextInput::new(&chat.composer)
        .plain()
        .width(None)
        .height(40.0)
        .text_style(st::MESSAGE)
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
        .radius(10.0)
        .center()
        .fill(if has_text { p.accent } else { p.raised })
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(|s: &mut RiverApp| {
            s.chat.send();
        })
        .child(icon(&icons::SEND, 17.0).color(if has_text {
            rune::Color::WHITE
        } else {
            p.text_faint
        }));
    let focused = chat.composer.focused;
    col()
        .key("composer")
        .w_full()
        .max_w(HISTORY_MAX_W)
        .self_center()
        .px(24.0)
        .pt(4.0)
        .pb(20.0)
        .child(
            row()
                .key("composer-box")
                .w_full()
                .h(52.0)
                .pl(10.0)
                .pr(8.0)
                .gap(6.0)
                .center_y()
                .radius(14.0)
                .fill(p.input)
                .border(
                    1.0,
                    if focused {
                        p.accent_line
                    } else {
                        p.line_strong
                    },
                )
                .shadow(6.0, 18.0, p.shadow)
                .child(icon(&icons::SMILE, 18.0).color(p.text_faint).ml(4.0))
                .child(field)
                .child(send),
        )
}

// Members -------------------------------------------------------------------------------------

fn members(room: &Room, p: &Palette) -> El {
    let mut sorted: Vec<&Member> = room.members.iter().collect();
    sorted.sort_by_key(|m| (m.role != Role::Owner, m.nickname.to_lowercase()));
    let mut list = col().key("member-list").gap(2.0).px(10.0);
    for m in sorted {
        list = list.child(member_row(room, m, p));
    }
    let invite = row()
        .key("invite")
        .role("button")
        .label("Invite member")
        .h(36.0)
        .mx(14.0)
        .mb(14.0)
        .gap(8.0)
        .radius(9.0)
        .center()
        .fill(p.raised)
        .border(1.0, p.line_strong)
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(|_: &mut RiverApp| {})
        .child(icon(&icons::INVITE, 15.0).color(p.text_secondary))
        .child(label("Invite member", st::SMALL_STRONG, p.text));
    row().key("members-panel").h_full().child(vline(p)).child(
        col()
            .key("members")
            .w(MEMBERS_W)
            .h_full()
            .fill(p.sidebar)
            .child(
                row()
                    .h(HEADER_H)
                    .px(20.0)
                    .gap(8.0)
                    .center_y()
                    .child(label("Members", st::HEADER_SMALL, p.text))
                    .child(label(
                        room.members.len().to_string(),
                        st::SMALL_STRONG,
                        p.text_faint,
                    )),
            )
            .child(hline(p))
            .child(
                list.pt(10.0)
                    .scroll(ScrollSpec {
                        stick_bottom: false,
                    })
                    .grow(1.0)
                    .min_h(0.0),
            )
            .child(invite),
    )
}

fn member_row(room: &Room, m: &Member, p: &Palette) -> El {
    let mut name = row()
        .gap(6.0)
        .center_y()
        .grow(1.0)
        .min_w(0.0)
        .child(label(m.nickname.clone(), st::ROOM, p.text).truncate());
    if m.id == room.me {
        name = name.child(label("you", st::SMALL, p.text_faint));
    }
    let mut r = row()
        .key(format!("member-{}", m.id))
        .h(40.0)
        .px(10.0)
        .gap(10.0)
        .radius(8.0)
        .center_y()
        .hover(p.hover)
        .child(avatar(Some(m), m.id, 28.0))
        .child(name);
    if m.role == Role::Owner {
        r = r.child(
            row()
                .gap(4.0)
                .center_y()
                .child(icon(&icons::CROWN, 12.0).color(p.owner))
                .child(label("Owner", st::SMALL, p.owner)),
        );
    }
    r
}
