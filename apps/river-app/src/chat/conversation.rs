//! The open room's conversation: its header, the message history and the composer.

use rune::tok::TextStyle;
use rune::{Color, El, ScrollSpec, col, el, icon, row, text};
use rune_kit::{Kit, TextInput};

use super::icons;
use super::model::{self, Group, MINUTES_PER_DAY, Member, Message, Notice, Pending, Role, Room};
use super::style::{self as st, Palette};
use super::view::{HEADER_H, hline, label, not_yet};
use super::{Chat, Mode};
use crate::RiverApp;

const AVATAR: f32 = 36.0;
/// The ring around your own avatar sits this far outside the disc, and is this wide, as the
/// web UI's `outline: 1.5px; outline-offset: 2px`.
const RING_GAP: f32 = 2.0;
const RING_W: f32 = 1.5;
/// The avatar column of a message row.
const GUTTER: f32 = 60.0;
/// A message body's widest line: the web UI's `max-w-[76ch]` at 15 pixels.
const BODY_MAX_W: f32 = 604.0;
/// The history and composer column, centred on a wide window: the web UI's
/// `.conversation-column` (`60px + 76ch + 24px`).
const COLUMN_MAX_W: f32 = GUTTER + BODY_MAX_W + 24.0;

pub(super) fn conversation(chat: &Chat, room: &Room, narrow: bool, p: &Palette, k: &Kit) -> El {
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

/// The room's name and description, and the members toggle. Like the web UI's header it spans
/// the whole conversation pane, left-aligned, rather than the centred message column.
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
        .max_w(COLUMN_MAX_W)
        .self_center()
        .pt(16.0)
        .pb(26.0)
        .gap(14.0);
    let mut day = None;
    for group in model::groups(&room.messages) {
        let d = group.messages[0].time / MINUTES_PER_DAY;
        if day != Some(d) {
            day = Some(d);
            let label = model::day_label(
                chat.day_zero + i64::from(d),
                chat.day_zero + i64::from(chat.today),
            );
            items = items.child(day_divider(d, label, p));
        }
        items = items.child(group_el(chat, room, &group, p));
    }
    // Keyed by room, so each room's viewport is its own: a room opens at its newest message
    // instead of at the offset the previous room was scrolled to.
    col()
        .key(format!("history-scroll-{}", room.key))
        .grow(1.0)
        .min_h(0.0)
        .w_full()
        .child(items)
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_edge_fade(40.0)
}

fn day_divider(day: u32, text: String, p: &Palette) -> El {
    row()
        .key(format!("day-{day}"))
        .w_full()
        .px(24.0)
        .pt(12.0)
        .gap(14.0)
        .center_y()
        .child(el().grow(1.0).h(1.0).fill(p.line))
        .child(label(text, st::SMALL_STRONG, p.text_muted))
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
        .key(format!("tag-{}", s.to_lowercase()))
        .h(18.0)
        .px(5.0)
        .radius(4.0)
        .center()
        .border(1.0, p.line_strong)
        .child(label(s.to_owned(), st::TAG, p.text_secondary))
}

/// One author's run of messages. Each message is its own row, as in the web UI: the first
/// carries the avatar and the name line, and the others show their time in the gutter while
/// the pointer is over them.
fn group_el(chat: &Chat, room: &Room, group: &Group, p: &Palette) -> El {
    let first = &group.messages[0];
    let author = room.member(group.author);
    let mine = group.author == room.me;
    let mut face = avatar(author, group.author, AVATAR);
    if mine {
        // Your own avatar has a ring around it, outside the disc.
        let outer = AVATAR + 2.0 * (RING_GAP + RING_W);
        face = el()
            .size(outer, outer)
            .radius(outer / 2.0)
            .center()
            .border(RING_W, p.accent)
            .child(face);
    }
    let mut name_row = row().gap(8.0).center_y().child(label(
        room.nickname(group.author).to_owned(),
        st::NAME,
        if mine { p.accent } else { p.text },
    ));
    if author.is_some_and(|m| m.role == Role::Owner) {
        name_row = name_row.child(tag("OWNER", p));
    }
    name_row = name_row.child(label(model::clock(first.time), st::SMALL, p.text_muted));
    // Both go in the first row only.
    let (mut face, mut name_row) = (Some(face), Some(name_row));
    let mut rows = col()
        .key(format!("group-{}", first.id))
        .w_full()
        .radius(8.0);
    if mine {
        rows = rows.fill(p.own_row);
    }
    for (i, m) in group.messages.iter().enumerate() {
        let (gutter, content) = if i == 0 {
            (
                row()
                    .w(GUTTER)
                    .justify_center()
                    .pt(2.0)
                    .child(face.take().unwrap_or_else(el)),
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(4.0)
                    .child(name_row.take().unwrap_or_else(row))
                    .child(message_el(chat, room, m, p)),
            )
        } else {
            (
                row().w(GUTTER).justify_center().pt(4.0).child(
                    label(model::clock(m.time), st::GUTTER_TIME, p.text_muted)
                        .group_opacity(0.0, 1.0),
                ),
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .child(message_el(chat, room, m, p)),
            )
        };
        let mut line = row()
            .key(format!("row-{}", m.id))
            .w_full()
            .pt(if i == 0 { 6.0 } else { 3.0 })
            .pb(3.0)
            .pr(24.0)
            .radius(8.0)
            .items_start()
            .hover_group()
            .hover(if mine { p.own_row_hover } else { p.row_hover })
            .child(gutter)
            .child(content);
        if let Some(bar) = actions(chat, room, m, p) {
            line = line.child(bar);
        }
        rows = rows.child(line);
    }
    rows
}

/// A small icon button on a message's action bar.
fn action_button(
    key: String,
    name: &'static str,
    glyph: &'static rune::Icon,
    p: &Palette,
    press: impl Fn(&mut RiverApp) + 'static,
) -> El {
    row()
        .key(key)
        .role("button")
        .label(name)
        .help(name)
        .size(28.0, 28.0)
        .radius(6.0)
        .center()
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(press)
        .child(icon(glyph, 15.0).color(p.text_secondary))
}

/// React, Reply, and for your own messages Edit and Delete, shown while the pointer is over
/// the message, as the web UI's action cluster. `None` where the user can't act, and on a
/// message still on its way.
fn actions(chat: &Chat, room: &Room, m: &Message, p: &Palette) -> Option<El> {
    if room.send_block.is_some() || m.pending.is_some() {
        return None;
    }
    let k = &m.key;
    let mut bar = row()
        .key(format!("actions-{k}"))
        .gap(2.0)
        .padding(2.0)
        .radius(8.0)
        .fill(p.surface)
        .border(1.0, p.line)
        .pin(None, Some(-12.0), Some(16.0), None);
    // Always shown while its picker is open, so the picker stays tied to what opened it.
    if chat.picker.as_deref() != Some(k.as_str()) {
        bar = bar.group_opacity(0.0, 1.0);
    }
    let key = k.clone();
    bar = bar.child(action_button(
        format!("react-{k}"),
        "React",
        &icons::SMILE,
        p,
        move |s| s.chat.toggle_picker(&key),
    ));
    let key = k.clone();
    bar = bar.child(action_button(
        format!("reply-{k}"),
        "Reply",
        &icons::REPLY,
        p,
        move |s| s.chat.start_reply(&key),
    ));
    if m.author == room.me {
        let key = k.clone();
        bar = bar.child(action_button(
            format!("edit-{k}"),
            "Edit",
            &icons::EDIT,
            p,
            move |s| s.chat.start_edit(&key),
        ));
        let key = k.clone();
        bar = bar.child(action_button(
            format!("delete-{k}"),
            "Delete",
            &icons::TRASH,
            p,
            move |s| s.chat.ask_delete(&key),
        ));
    }
    Some(bar)
}

/// The web UI's frequent reactions (`FREQUENT_EMOJIS` in
/// `ui/src/components/conversation/emoji_picker.rs`).
const QUICK_REACTIONS: &[&str] = &[
    "👍", "❤️", "😂", "👀", "🔥", "😮", "😢", "😡", "🎉", "🤔", "👋", "✅", "💯", "🙏", "😍", "👏",
];

fn picker(room: &Room, m: &Message, p: &Palette) -> El {
    let mine = room.own_reaction(&m.key);
    let mut strip = row()
        .key(format!("picker-{}", m.key))
        .max_w(BODY_MAX_W)
        .gap(2.0)
        .padding(4.0)
        .radius(10.0)
        .fill(p.surface)
        .border(1.0, p.line_strong)
        .flex_wrap();
    for (i, emoji) in QUICK_REACTIONS.iter().enumerate() {
        let (key, e) = (m.key.clone(), (*emoji).to_owned());
        let current = mine == Some(*emoji);
        strip = strip.child(
            row()
                .key(format!("pick-{}-{i}", m.key))
                .role("button")
                .label(format!("React with {emoji}"))
                .size(32.0, 32.0)
                .radius(6.0)
                .center()
                .fill(if current {
                    p.accent_soft
                } else {
                    Color::TRANSPARENT
                })
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .on_press(move |s: &mut RiverApp| s.chat.react(&key, &e))
                .child(label((*emoji).to_owned(), st::EMOJI, p.text)),
        );
    }
    strip
}

/// Under a message of yours that the node has not taken: "Sending…", or why it was not sent
/// with Retry and Discard.
fn delivery(pending: &Pending, p: &Palette) -> El {
    let Some(reason) = &pending.failed else {
        return row()
            .key("sending")
            .child(label("Sending…", st::SMALL, p.text_muted));
    };
    let entry = pending.entry;
    col()
        .key("not-sent")
        .max_w(BODY_MAX_W)
        .gap(4.0)
        .child(
            label(format!("Not sent. {reason}"), st::SMALL, p.danger)
                .wrap()
                .w_full(),
        )
        .child(
            row()
                .gap(12.0)
                .child(text_button(
                    format!("retry-{entry}"),
                    "Retry",
                    p.accent,
                    move |s| s.chat.retry(entry),
                ))
                .child(text_button(
                    format!("discard-{entry}"),
                    "Discard",
                    p.text_secondary,
                    move |s| s.chat.discard(entry),
                )),
        )
}

/// A pressable word, for small inline choices.
fn text_button(
    key: String,
    name: &'static str,
    color: Color,
    press: impl Fn(&mut RiverApp) + 'static,
) -> El {
    row()
        .key(key)
        .role("button")
        .label(name)
        .cursor(rune::Cursor::Pointer)
        .on_press(press)
        .child(label(name, st::SMALL_STRONG, color))
}

fn message_el(chat: &Chat, room: &Room, m: &Message, p: &Palette) -> El {
    let mut c = col().key(format!("msg-{}", m.id)).w_full().gap(5.0);
    if let Some(reply) = &m.reply_to {
        let author = room.nickname(reply.author).to_owned();
        let line = format!("{author}  {}", reply.preview);
        c = c.child(
            row()
                .key("reply")
                .max_w(BODY_MAX_W)
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
    if let Some(pending) = &m.pending {
        c = c.child(delivery(pending, p));
    }
    // Pressing a reaction toggles yours: the same emoji removes it, another replaces it.
    let can_react = room.send_block.is_none() && m.pending.is_none();
    if !m.reactions.is_empty() {
        let mut pills = row().key("reactions").gap(5.0).pt(2.0).flex_wrap();
        for r in &m.reactions {
            let mine = r.by.contains(&room.me);
            let mut pill = row();
            if can_react {
                let (key, e) = (m.key.clone(), r.emoji.clone());
                pill = pill
                    .role("button")
                    .label(if mine {
                        format!("Remove your {} reaction", r.emoji)
                    } else {
                        format!("React with {}", r.emoji)
                    })
                    .cursor(rune::Cursor::Pointer)
                    .on_press(move |s: &mut RiverApp| s.chat.react(&key, &e));
            }
            pills = pills.child(
                pill.key(format!("reaction-{}-{}", m.key, r.emoji))
                    .h(26.0)
                    .px(8.0)
                    .gap(5.0)
                    .radius(13.0)
                    .center_y()
                    .fill(if mine { p.accent_soft } else { p.rail })
                    .border(1.0, if mine { p.accent_line } else { p.line })
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
    if can_react && chat.picker.as_deref() == Some(m.key.as_str()) {
        c = c.child(picker(room, m, p));
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
        .max_w(BODY_MAX_W)
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
    let can_send = room.send_block.is_none();
    let has_text = can_send && !chat.composer.text.trim().is_empty();
    let placeholder = match &room.send_block {
        None => format!("Message {}", room.name),
        Some(reason) => reason.clone(),
    };
    let field = TextInput::new(&chat.composer)
        .plain()
        .width(None)
        .height(44.0)
        .text_style(st::MESSAGE.lh(24.0))
        .placeholder(placeholder)
        .disabled(!can_send)
        .label("Message")
        .on_edit(|s: &mut RiverApp| &mut s.chat.composer)
        .on_submit(|s: &mut RiverApp, _| {
            s.chat.send();
        })
        .build(k)
        .key("composer-field")
        .grow(1.0)
        .min_w(0.0);
    let mut send = row()
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
    if !can_send {
        send = send.disabled(true).opacity(0.45);
    }
    let emoji = not_yet(
        row()
            .key("emoji")
            .role("button")
            .label("Emoji")
            .size(36.0, 36.0)
            .radius(8.0)
            .center()
            .child(icon(&icons::SMILE, 19.0).color(p.text_muted)),
    );
    let mut column = col()
        .key("composer")
        .w_full()
        .max_w(COLUMN_MAX_W)
        .self_center()
        .px(20.0)
        .pt(4.0)
        .pb(18.0)
        .gap(6.0);
    for n in &room.notices {
        column = column.child(notice(n, p));
    }
    if let Some(reason) = &room.send_block {
        // Said in full, not only as the placeholder, which a long reason would not fit.
        column = column.child(
            label(reason.clone(), st::SMALL, p.text_secondary)
                .key("send-block")
                .wrap()
                .w_full()
                .px(4.0),
        );
    } else if let Some(bar) = mode_bar(chat, room, p) {
        column = column.child(bar);
    }
    column.child(
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

/// A failed edit, deletion or reaction, with Retry (when it can be sent again) and Dismiss.
fn notice(n: &Notice, p: &Palette) -> El {
    let entry = n.entry;
    let mut choices = row().gap(12.0);
    if n.retry {
        choices = choices.child(text_button(
            format!("retry-{entry}"),
            "Retry",
            p.accent,
            move |s| s.chat.retry(entry),
        ));
    }
    choices = choices.child(text_button(
        format!("dismiss-{entry}"),
        "Dismiss",
        p.text_secondary,
        move |s| s.chat.discard(entry),
    ));
    col()
        .key(format!("notice-{entry}"))
        .w_full()
        .gap(4.0)
        .px(12.0)
        .py(8.0)
        .radius(8.0)
        .border(1.0, p.line_strong)
        .child(label(n.text.clone(), st::SMALL, p.danger).wrap().w_full())
        .child(choices)
}

/// Above the composer: what it is replying to, that it is editing, or the delete question.
fn mode_bar(chat: &Chat, room: &Room, p: &Palette) -> Option<El> {
    let bar = |key: &str, text: String| {
        row()
            .key(key.to_owned())
            .w_full()
            .gap(12.0)
            .px(12.0)
            .py(6.0)
            .radius(8.0)
            .center_y()
            .fill(p.surface)
            .child(
                label(text, st::SMALL, p.text_secondary)
                    .truncate()
                    .grow(1.0)
                    .min_w(0.0),
            )
    };
    let cancel = || {
        text_button("cancel".to_owned(), "Cancel", p.text_secondary, |s| {
            s.chat.cancel()
        })
    };
    match &chat.mode {
        Mode::Write => None,
        Mode::Reply(key) => {
            let target = room.message_by_key(key);
            let who = target.map_or("a message", |m| room.nickname(m.author));
            let what = target
                .map(|m| model::preview(&m.text, 80))
                .unwrap_or_default();
            Some(bar("replying", format!("Replying to {who}: {what}")).child(cancel()))
        }
        Mode::Edit { .. } => {
            Some(bar("editing", "Editing your message. Send saves it.".to_owned()).child(cancel()))
        }
        Mode::Delete(_) => Some(
            bar(
                "deleting",
                "Delete this message for everyone in the room?".to_owned(),
            )
            .child(text_button(
                "confirm-delete".to_owned(),
                "Delete",
                p.danger,
                |s| s.chat.confirm_delete(),
            ))
            .child(cancel()),
        ),
    }
}
