//! The open room's conversation: its header, the message history and the composer.

use rune::tok::TextStyle;
use rune::{Color, El, ScrollSpec, col, el, icon, row, text};
use rune_kit::{Kit, ScrollBar, ScrollBarStyle};

use super::emoji;
use super::icons;
use super::links;
use super::markdown;
use super::model::{self, Group, MINUTES_PER_DAY, Member, Message, Notice, Pending, Role, Room};
use super::style::{self as st, Palette};
use super::view::{HEADER_H, hline, label};
use super::{Chat, Mode};
use crate::RiverApp;
use crate::rooms::modes::NotificationMode;

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
/// The most lines the composer grows to before it scrolls.
const COMPOSER_MAX_LINES: usize = 8;

pub(super) fn conversation(
    chat: &Chat,
    room: &Room,
    narrow: bool,
    win_h: f32,
    field_h: f32,
    p: &Palette,
    k: &Kit,
) -> El {
    let mut c = col()
        .key("conversation")
        // Escape abandons a reply or an edit wherever focus is in the conversation, not only
        // in the composer (a reply started with the mouse leaves focus on the message). Inner
        // handlers (the composer, the emoji panel) run first. Dialogs and menus live in the
        // overlay layer, outside this subtree, and take their own Escape.
        .on_key(|s: &mut RiverApp, ev, _| ev.key == rune::Key::Escape && s.chat.cancel_compose())
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .child(header(chat, room, narrow, p, k))
        .child(hline(p));
    // Between the header and the history, which gives up the room.
    if let Some(panel) = super::search::search_panel(chat, win_h, field_h, p, k) {
        c = c.child(panel).child(hline(p));
    }
    c.child(history(chat, room, p, k))
        .child(composer(chat, room, p, k))
}

/// The room's name and description, and the members toggle. Like the web UI's header it spans
/// the whole conversation pane, left-aligned, rather than the centred message column.
fn header(chat: &Chat, room: &Room, narrow: bool, p: &Palette, k: &Kit) -> El {
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
    // The name opens the room's details dialog, from the keyboard too.
    let title = title
        .key("room-title")
        .role("button")
        .label(format!("Room details: {}", room.name))
        .help("Room details")
        .focusable(true)
        .cursor(rune::Cursor::Pointer)
        .on_press(|s: &mut RiverApp| s.chat.open_details());
    let mut info = col().grow(1.0).min_w(0.0).gap(2.0).child(title);
    // The description is markdown, shown as its first line (links work there); the details
    // dialog shows all of it.
    let desc = room.description.as_str();
    // The first line that says something: a description may start with a blank line.
    let first = desc
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if !desc.is_empty() {
        let key = format!("description-{}", room.key);
        let rendered = markdown::header_inline(first)
            .filter(|line| markdown::is_cheap(line))
            .and_then(|line| {
                markdown::render(line, false, st::SMALL, p.text_muted, key, p, k, false)
            });
        let line = match rendered {
            Some(md) => md.max_h(18.0).clip_soft(0.0),
            None => label(markdown::strip(desc), st::SMALL, p.text_muted).truncate(),
        };
        // A press on the text between links opens the dialog; a link keeps its own press.
        info = info.child(
            row()
                .key("description-open")
                .w_full()
                .min_w(0.0)
                .cursor(rune::Cursor::Pointer)
                .on_press(|s: &mut RiverApp| s.chat.open_details())
                .child(line),
        );
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
    // The room's notification mode, as the web UI's bell: struck through when muted. Opens the
    // dialog to change it.
    let mode = chat.mode_of(&room.key);
    let bell = Some({
        let title = super::notify_dialog::bell_title(mode);
        let muted = mode == NotificationMode::Muted;
        row()
            .key("notification-bell")
            .role("button")
            .label(title.clone())
            .help(title)
            .size(32.0, 32.0)
            .radius(8.0)
            .center()
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| s.chat.open_notify_dialog())
            .child(
                icon(super::notify_dialog::bell(mode), 18.0).color(if muted {
                    p.text_muted
                } else {
                    p.text_secondary
                }),
            )
    });
    let mut header = row()
        .key("room-header")
        .w_full()
        .min_h(HEADER_H)
        .pl(20.0)
        .pr(12.0)
        .gap(16.0)
        .center_y()
        .child(info);
    if let Some(bell) = bell {
        header = header.child(bell);
    }
    header.child(toggle)
}

fn history(chat: &Chat, room: &Room, p: &Palette, k: &Kit) -> El {
    if room.messages.is_empty() && room.joins.is_empty() {
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
    // The ids a reply's strip can jump to, built once for the whole history.
    let present: std::collections::HashSet<model::MessageId> =
        room.messages.iter().map(|m| m.id).collect();
    let mut day = None;
    // A join line is not part of a message group: it ends the group before it, so the same
    // author's messages either side of it are two groups, as in the web UI.
    let mut at = 0;
    let mut joins = room.joins.iter().peekable();
    loop {
        while let Some(j) = joins.next_if(|j| j.before.min(room.messages.len()) <= at) {
            items = day_break(items, &mut day, j.time, chat, p);
            items = items.child(join_el(chat, j, p));
        }
        let end = joins
            .peek()
            .map_or(room.messages.len(), |j| j.before.min(room.messages.len()));
        if at >= room.messages.len() && joins.peek().is_none() {
            break;
        }
        for group in model::groups(&room.messages[at..end]) {
            items = day_break(items, &mut day, group.messages[0].time, chat, p);
            items = items.child(group_el(chat, room, &group, &present, p, k));
        }
        at = end;
    }
    // Keyed by room, so each room's viewport is its own: a room opens at its newest message
    // instead of at the offset the previous room was scrolled to.
    let scroll_key = format!("history-scroll-{}", room.key);
    let mut frame = col()
        .key("history-frame")
        .grow(1.0)
        .min_h(0.0)
        .w_full()
        .child(
            col()
                .key(scroll_key.clone())
                .grow(1.0)
                .min_h(0.0)
                .w_full()
                .child(items.selection_group(true))
                .scroll(ScrollSpec { stick_bottom: true })
                // Focusable, so a click on the history puts keyboard focus on it and Up and
                // Down scroll it by a line (rune does that for a focused scroll view), as in
                // the web UI where the arrows scroll the page when no field has focus.
                .focusable(true)
                .label("Conversation history")
                .scroll_edge_fade(40.0)
                .on_at_bottom(|s: &mut RiverApp, at| s.chat.at_bottom = at)
                .on_scroll_offset({
                    let at = chat.history_offset.clone();
                    move |p| at.set(p.offset[1])
                })
                .scroll_bar_styled(ScrollBarStyle::scroll_view(k)),
        );
    // Latest: the web UI's round button in the corner, while the newest message is off screen.
    if !chat.at_bottom {
        frame = frame.child(
            row()
                .key("latest")
                .role("button")
                .label("Scroll to latest message")
                .help("Latest")
                .size(40.0, 40.0)
                .radius(20.0)
                .center()
                .fill(p.surface)
                .border(1.0, p.line)
                .shadow(8.0, 24.0, p.shadow)
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .pin(None, None, Some(16.0), Some(16.0))
                .on_press_tx(move |_: &mut RiverApp, tx: &mut rune::Tx| {
                    tx.scroll_to_bottom(scroll_key.clone())
                })
                .child(icon(&icons::CHEVRON_DOWN, 18.0).color(p.text_muted)),
        );
    }
    frame
}

/// `items` with the divider for `time`'s day first, when that is not the day of the last item.
fn day_break(items: El, day: &mut Option<u32>, time: u32, chat: &Chat, p: &Palette) -> El {
    let d = time / MINUTES_PER_DAY;
    if *day == Some(d) {
        return items;
    }
    *day = Some(d);
    let label = model::day_label(
        chat.day_zero + i64::from(d),
        chat.day_zero + i64::from(chat.today),
    );
    items.child(day_divider(d, label, p))
}

/// A join line, as the web UI's `msg-event`: the icon in the avatar column, then one quiet line
/// with the names in bold and the time. It has no actions and nothing to press; three or more
/// people are named in its tooltip.
fn join_el(chat: &Chat, j: &model::Join, p: &Palette) -> El {
    let (mut line, bold) = j.line();
    let said = line.clone();
    line.push_str(" \u{b7} ");
    line.push_str(&model::clock(j.time));
    let spans = bold
        .into_iter()
        .map(|r| {
            rune::Span::new(r.start, r.end)
                .bold()
                .color(p.text_secondary)
        })
        .collect();
    let text = rune::rich(line, spans)
        .font(st::JOIN)
        .color(p.text_muted)
        .shrink()
        .selectable(true);
    let when = model::full_time(chat.day_zero, j.time);
    let key = format!("join-{}", j.key);
    let icon_cell = el()
        .w(GUTTER)
        .center()
        .child(icon(&icons::JOIN, 14.0).color(p.text_muted));
    // Three or more are counted, not named: pressing the line (or Space or Enter on it) lists
    // them, as the web UI's `details` does, and the names are in its label and tooltip too.
    let names = j.names.join(", ");
    let counted = j.names.len() > 2;
    let open = counted && chat.joins_open.contains(&j.key);
    let mut line_row = row()
        .key(key.clone())
        .w_full()
        .pt(3.0)
        .pb(3.0)
        .pr(24.0)
        .center_y()
        .child(icon_cell)
        .child(text);
    if counted {
        let toggle_key = j.key.clone();
        let toggle = move |s: &mut RiverApp| {
            if !s.chat.joins_open.remove(&toggle_key) {
                s.chat.joins_open.insert(toggle_key.clone());
            }
        };
        let on_enter = toggle.clone();
        line_row = line_row
            .role("button")
            .label(format!("{said} at {when}: {names}"))
            .help(names.clone())
            .focusable(true)
            .cursor(rune::Cursor::Pointer)
            .on_press(toggle)
            // Space presses a focused control; Return is the app's to take.
            .on_key(move |s: &mut RiverApp, ev, _| {
                let enter = ev.key == rune::Key::Enter && ev.mods == rune::Mods::default();
                if enter {
                    on_enter(s);
                }
                enter
            });
    } else {
        line_row = line_row.label(format!("{said} at {when}"));
    }
    // Always a column, so the line keeps its place in the tree (and the keyboard focus) when
    // the names open under it.
    let mut c = col().key(format!("{key}-item")).w_full().child(line_row);
    if open {
        c = c.child(
            row().w_full().pr(24.0).pb(3.0).child(el().w(GUTTER)).child(
                label(names, st::JOIN, p.text_muted)
                    .key(format!("{key}-names"))
                    .wrap()
                    .shrink()
                    .selectable(true),
            ),
        );
    }
    c
}

pub(super) fn day_divider(day: u32, text: String, p: &Palette) -> El {
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

/// The "DEPUTY" tag after a deputy's name: the web UI's `message-author-deputy-badge`, the same
/// outlined small caps as the OWNER tag with the shield before the word. `tooltip` says who
/// appointed them and whether they can ban you.
fn deputy_tag(tooltip: String, p: &Palette) -> El {
    row()
        .key("tag-deputy")
        .h(18.0)
        .px(5.0)
        .gap(3.0)
        .radius(4.0)
        .center()
        .border(1.0, p.line_strong)
        .label(tooltip.clone())
        .help(tooltip)
        .child(icon(&icons::SHIELD, 11.0).color(p.text_secondary))
        .child(label("DEPUTY".to_owned(), st::TAG, p.text_secondary))
}

/// One author's run of messages. Each message is its own row, as in the web UI: the first
/// carries the avatar and the name line, and the others show their time in the gutter while
/// the pointer is over them.
fn group_el(
    chat: &Chat,
    room: &Room,
    group: &Group,
    present: &std::collections::HashSet<model::MessageId>,
    p: &Palette,
    k: &Kit,
) -> El {
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
    // The avatar and the name open the author's dialog, as in the web UI.
    let author_uid = author.map(|m| m.full_id);
    if let Some(uid) = author_uid {
        face = face
            .key(format!("author-avatar-{}", first.key))
            .role("button")
            .label(format!("Member info: {}", room.nickname(group.author)))
            .cursor(rune::Cursor::Pointer)
            .on_press(move |s: &mut RiverApp| s.chat.open_member_full(uid));
    }
    let mut name = label(
        room.nickname(group.author).to_owned(),
        st::NAME,
        if mine { p.accent } else { p.text },
    );
    if let Some(uid) = author_uid {
        name = name
            .key(format!("author-name-{}", first.key))
            .role("button")
            .label(format!("Member info: {}", room.nickname(group.author)))
            .cursor(rune::Cursor::Pointer)
            .on_press(move |s: &mut RiverApp| s.chat.open_member_full(uid));
    }
    let mut name_row = row().gap(8.0).center_y().child(name);
    if author.is_some_and(|m| m.role == Role::Owner) {
        name_row = name_row.child(tag("OWNER", p));
    } else if let Some(tooltip) = author.and_then(Member::deputy_tooltip) {
        name_row = name_row.child(deputy_tag(tooltip, p));
    }
    name_row = name_row.child(
        label(model::clock(first.time), st::SMALL, p.text_muted)
            .key(format!("time-{}", first.key))
            .help(model::full_time(chat.day_zero, first.time)),
    );
    // Both go in the first row only.
    let (mut face, mut name_row) = (Some(face), Some(name_row));
    let mut rows = col()
        .key(format!("group-{}", first.key))
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
                    .child(message_el(room, m, present, p, k)),
            )
        } else {
            (
                row().w(GUTTER).justify_center().pt(4.0).child(
                    label(model::clock(m.time), st::GUTTER_TIME, p.text_muted)
                        .key(format!("time-{}", m.key))
                        .help(model::full_time(chat.day_zero, m.time))
                        .group_opacity(0.0, 1.0),
                ),
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .child(message_el(room, m, present, p, k)),
            )
        };
        let flashing = chat.flashing() == Some(m.key.as_str());
        let mut line = row()
            .key(format!("row-{}", m.key))
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
        if flashing {
            // Just jumped to from the search panel: marked until it fades out.
            line = line.fill(p.accent_soft);
        }
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
    if m.author == room.me && !m.source.is_empty() {
        let key = k.clone();
        bar = bar.child(action_button(
            format!("edit-{k}"),
            "Edit",
            &icons::EDIT,
            p,
            move |s| s.chat.start_edit(&key),
        ));
    }
    if m.author == room.me {
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

/// The open reaction picker, mounted in the window's overlay layer under the message it
/// reacts to. The surface is modal with click-outside: Escape or a press anywhere outside it
/// closes it without reacting, and that press goes no further (another message's React button
/// needs a second press). Not shown where the message can't be reacted to.
pub(super) fn picker_overlay(chat: &Chat, room: &Room, p: &Palette, k: &Kit, layer: &mut Vec<El>) {
    let Some(m) = chat
        .picker
        .as_deref()
        .and_then(|key| room.messages.iter().find(|m| m.key == key))
        .filter(|m| room.send_block.is_none() && m.pending.is_none())
    else {
        return;
    };
    let surface = picker(room, m, p).modal(true, |s: &mut RiverApp| s.chat.picker = None);
    layer.push(rune_kit::popover::anchored(
        k,
        surface,
        &format!("msg-{}", m.key),
        rune_kit::Placement::BottomStart,
        4.0,
    ));
}

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
    let mut choices = row().gap(12.0);
    if pending.retry {
        choices = choices.child(text_button(
            format!("retry-{entry}"),
            "Retry",
            p.accent,
            move |s| s.chat.retry(entry),
        ));
    }
    choices = choices.child(text_button(
        format!("discard-{entry}"),
        "Discard",
        p.text_secondary,
        move |s| s.chat.discard(entry),
    ));
    col()
        .key("not-sent")
        .max_w(BODY_MAX_W)
        .gap(4.0)
        .child(
            // A reason that can't be retried says for itself what became of it.
            label(
                if pending.retry {
                    format!("Not sent. {reason}")
                } else {
                    reason.clone()
                },
                st::SMALL,
                p.danger,
            )
            .wrap()
            .w_full(),
        )
        .child(choices)
}

/// A pressable word, for small inline choices.
pub(super) fn text_button(
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

fn message_el(
    room: &Room,
    m: &Message,
    present: &std::collections::HashSet<model::MessageId>,
    p: &Palette,
    k: &Kit,
) -> El {
    let mut c = col().key(format!("msg-{}", m.key)).w_full().gap(5.0);
    // As the web UI: a quote of a message that is in the room jumps to it; a reply whose
    // target is not (pruned, deleted, moderated) says so and is not pressable. The web UI
    // shows no author or preview then either.
    if m.reply_gone
        || m.reply_to
            .as_ref()
            .is_some_and(|r| !present.contains(&r.message))
    {
        c = c.child(
            row()
                .key("reply")
                .max_w(BODY_MAX_W)
                .gap(6.0)
                .center_y()
                .min_w(0.0)
                .child(icon(&icons::REPLY, 13.0).color(p.text_muted))
                .child(
                    label("Original message unavailable", st::QUOTE, p.quote)
                        .truncate()
                        .shrink(),
                ),
        );
    } else if let Some(reply) = &m.reply_to {
        let author = room.nickname(reply.author).to_owned();
        let target = reply.message;
        let line = format!("{author}  {}", reply.preview);
        let text = rune::rich(
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
        .shrink();
        let strip = row()
            .key("reply")
            .max_w(BODY_MAX_W)
            .gap(6.0)
            .center_y()
            .min_w(0.0)
            .child(icon(&icons::REPLY, 13.0).color(p.text_muted))
            .child(text)
            .role("button")
            .label(format!("Jump to the message from {author}"))
            .focusable(true)
            .help("Jump to the original message")
            .radius(4.0)
            // Padding with a matching negative margin keeps the text where the body's is while
            // the hover fill extends past it.
            .px(4.0)
            .ml(-4.0)
            .py(1.0)
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            // A drag that began here (to select the quote) must not jump when it ends over
            // the strip: Rune runs a press after a drag, so the drag marks itself.
            .on_drag(|s: &mut RiverApp, d: &mut rune::Drag, _| {
                s.chat.reply_press_drag(d);
            })
            .on_press(move |s: &mut RiverApp| {
                if !s.chat.reply_press_was_drag() {
                    s.chat.reply_jump(target, std::time::Instant::now());
                }
            })
            // Space presses a focused control; Return is the app's to take (as the archive
            // button does), and an HTML button takes it too.
            .on_key(move |s: &mut RiverApp, ev, _| {
                let enter = ev.key == rune::Key::Enter && ev.mods == rune::Mods::default();
                if enter {
                    s.chat.reply_jump(target, std::time::Instant::now());
                }
                enter
            });
        c = c.child(strip);
    }
    c = c.child(message_body(m, p, k));
    if let Some(pending) = &m.pending {
        c = c.child(delivery(pending, p));
    }
    // Pressing a reaction toggles yours: the same emoji removes it, another replaces it.
    let can_react = room.send_block.is_none() && m.pending.is_none();
    if !m.reactions.is_empty() {
        let mut pills = row().key("reactions").gap(5.0).pt(2.0).flex_wrap();
        for r in &m.reactions {
            let mine = r.by.contains(&room.me);
            // Who reacted, as the web UI's chip tooltip.
            let names =
                r.by.iter()
                    .map(|id| room.nickname(*id))
                    .collect::<Vec<_>>()
                    .join(", ");
            let mut pill = row().help(if mine {
                format!("{names} (click to remove)")
            } else {
                names
            });
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
    c
}

/// A message's text, with any web links in it styled and pressable.
pub(super) fn message_body(m: &Message, p: &Palette, k: &Kit) -> El {
    // Markdown that cannot be made safe to draw (`None`) falls back to the plain text.
    if !m.md.is_empty()
        && markdown::is_cheap(&m.md)
        && let Some(body) = markdown_body(m, p, k)
    {
        return body;
    }
    let mut content = m.text.clone();
    // Only URLs that pass the same checks as the markdown path, each to where it is safe to go
    // (a gateway URL opens on this node).
    let links: Vec<(std::ops::Range<usize>, String)> = model::links(&m.text)
        .into_iter()
        .filter_map(|r| links::plain_link_target(&m.text[r.clone()]).map(|u| (r, u)))
        .collect();
    let mut spans: Vec<rune::Span> = links
        .iter()
        .map(|(r, _)| {
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
    for (r, url) in links {
        body = body.link(rune::Link::new(r, url).hover(rune::LinkPaint::new().underline(p.accent)));
    }
    body.on_link(|_: &mut RiverApp, url: &str, tx: &mut rune::Tx| {
        if markdown::openable(url) {
            let _ = tx.open_url(url.trim());
        }
    })
}

/// A message as markdown, then the `(edited)` mark under it.
fn markdown_body(m: &Message, p: &Palette, k: &Kit) -> Option<El> {
    let rendered = markdown::render(
        &m.md,
        true,
        st::MESSAGE,
        p.text,
        format!("md-{}", m.key),
        p,
        k,
        true,
    )?;
    // The history is one selection group, so a drag from one message into another selects
    // across them as in a browser; the message's own group would end the selection at its edge.
    let mut body = col()
        .w_full()
        .max_w(BODY_MAX_W)
        .child(rendered.selection_group(false));
    if m.edited {
        body = body.child(label("(edited)", st::SMALL, p.text_muted));
    }
    Some(body)
}

/// The composer's keys, in this order, each consuming the key when it acts:
///
/// 1. The @mention popup (`mention_key`, #914), whenever it is open, then the `:shortcode:` popup
///    (`shortcode_key`). They come first: Esc, Up and Enter belong to a popup while it is open.
/// 2. Escape: closes the emoji picker, and only then abandons a reply or an edit.
/// 3. Up in an empty composer: edits your last message.
pub(super) fn composer_key(s: &mut RiverApp, ev: &rune::KeyEvent) -> bool {
    // 1.
    s.chat.mention_sync();
    if s.chat.mention_key(ev) {
        return true;
    }
    // The `:shortcode:` popup is the mention popup's sibling; the two never open together.
    s.chat.shortcode_sync();
    if s.chat.shortcode_key(ev) {
        return true;
    }
    // 2.
    if ev.key == rune::Key::Escape {
        if s.chat.emoji_open {
            s.chat.emoji_open = false;
            return true;
        }
        if s.chat.cancel_compose() {
            return true;
        }
    }
    // 3.
    ev.key == rune::Key::Up && s.chat.composer.text.is_empty() && s.chat.edit_last()
}

fn composer(chat: &Chat, room: &Room, p: &Palette, k: &Kit) -> El {
    let can_send = room.send_block.is_none();
    let over = chat.over_limit();
    let has_text = can_send && !over && !chat.composer.text.trim().is_empty();
    let placeholder = match &room.send_block {
        None => format!("Message {}", room.name),
        Some(reason) => reason.clone(),
    };
    // A multi-line field that grows with its text (up to eight lines), so Shift+Enter (or
    // Option+Enter) inserts a newline and plain Enter sends, as in the web UI.
    let mut input = rune::text_input(&chat.composer, st::MESSAGE.lh(24.0))
        .key("composer-field")
        .w_full()
        .color(p.text)
        .placeholder(placeholder.clone(), p.text_muted)
        .caret(p.text, k.selection())
        .label("Message")
        .disabled(!can_send);
    if can_send {
        input = input
            .on_input_key(|s: &mut RiverApp, ev, _| composer_key(s, ev))
            .on_edit(|s: &mut RiverApp| &mut s.chat.composer)
            .on_submit(|s: &mut RiverApp, _| {
                s.chat.send();
            });
    }
    // role "textbox" makes a press anywhere in the box (its padding too) focus the editor.
    let mut field = row()
        .key("composer-input")
        .role("textbox")
        .grow(1.0)
        .min_w(0.0)
        .px(8.0)
        .py(6.0)
        .child(rune::auto_grow(input, 1, COMPOSER_MAX_LINES));
    if !can_send {
        field = field.opacity(0.5);
    }
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
    if !can_send || over {
        send = send.disabled(true).opacity(0.45);
    }
    if can_send
        && over
        && let Some((_, max)) = chat.draft_size()
    {
        send = send.help(format!("Message exceeds the {max} byte limit"));
    }
    let emoji = {
        let open = chat.emoji_open;
        let mut b = row()
            .key("emoji")
            .role("button")
            .label(if open { "Close emoji picker" } else { "Emoji" })
            .help("Emoji")
            .size(36.0, 36.0)
            .radius(8.0)
            .center()
            .fill(if open {
                p.surface_hover
            } else {
                Color::TRANSPARENT
            })
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| s.chat.toggle_emoji())
            .child(icon(&icons::SMILE, 19.0).color(if open { p.text } else { p.text_muted }));
        if !can_send {
            b = b.disabled(true).opacity(0.45);
        }
        b
    };
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
    for (key, said) in chat.reply_notices(&room.key) {
        let key = key.to_owned();
        column = column.child(
            col()
                .key("reply-notice")
                .w_full()
                .gap(4.0)
                .px(12.0)
                .py(8.0)
                .radius(8.0)
                .border(1.0, p.line_strong)
                .child(label(said.to_owned(), st::SMALL, p.danger).wrap().w_full())
                .child(text_button(
                    "reply-notice-dismiss".to_owned(),
                    "Dismiss",
                    p.text_secondary,
                    move |s| s.chat.dismiss_reply_notice(&key),
                )),
        );
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
    if chat.emoji_open && can_send {
        column = column.child(emoji_panel(chat, p, k));
    }
    column = column.child(
        row()
            .key("composer-box")
            .w_full()
            .min_h(50.0)
            .px(6.0)
            .gap(4.0)
            .items_end()
            .py(7.0)
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
    );
    // The byte counter, as the web UI's: from 80% of the room's limit, and in the danger colour
    // with the reason once over it.
    if can_send
        && let Some((bytes, max)) = chat.draft_size()
        && bytes > max / 5 * 4
    {
        column = column.child(if bytes > max {
            label(
                format!("Message too long \u{2014} {bytes}/{max} bytes"),
                st::SMALL,
                p.danger,
            )
            .key("byte-counter")
            .self_end()
            .pr(4.0)
        } else {
            label(format!("{bytes}/{max}"), st::SMALL, p.text_muted)
                .key("byte-counter")
                .self_end()
                .pr(4.0)
        });
    }
    column
}

/// The composer's emoji picker: the web UI's categories as a row of tabs, and the chosen
/// category's emoji below, scrolling when there are more than fit.
fn emoji_panel(chat: &Chat, p: &Palette, k: &Kit) -> El {
    let cats = emoji::EMOJI_CATEGORIES;
    let current = chat.emoji_category.min(cats.len() - 1);
    let mut tabs = row().key("emoji-tabs").gap(2.0);
    for (i, (name, glyph, _)) in cats.iter().enumerate() {
        tabs = tabs.child(
            row()
                .key(format!("emoji-tab-{name}"))
                .role("button")
                .label(format!("{name} emoji"))
                .size(34.0, 30.0)
                .radius(6.0)
                .center()
                .fill(if i == current {
                    p.accent_soft
                } else {
                    Color::TRANSPARENT
                })
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .on_press(move |s: &mut RiverApp| s.chat.emoji_category = i)
                .child(label((*glyph).to_owned(), st::EMOJI, p.text)),
        );
    }
    let mut grid = row().key("emoji-grid").gap(2.0).flex_wrap().w_full();
    for (i, e) in cats[current].2.iter().enumerate() {
        let picked = (*e).to_owned();
        grid = grid.child(
            row()
                .key(format!("emoji-{current}-{i}"))
                .role("button")
                .label((*e).to_owned())
                .size(34.0, 34.0)
                .radius(6.0)
                .center()
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .on_press(move |s: &mut RiverApp| s.chat.insert_emoji(&picked))
                .child(label((*e).to_owned(), st::EMOJI, p.text)),
        );
    }
    col()
        .key("emoji-panel")
        .on_key(|s: &mut RiverApp, ev, _| {
            let esc = ev.key == rune::Key::Escape;
            if esc {
                s.chat.emoji_open = false;
            }
            esc
        })
        .w_full()
        .gap(6.0)
        .padding(8.0)
        .radius(12.0)
        .fill(p.surface)
        .border(1.0, p.line_strong)
        .shadow(8.0, 24.0, p.shadow)
        .child(tabs)
        .child(
            col()
                .key("emoji-scroll")
                .w_full()
                .max_h(200.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(grid)
                .scroll_bar(k),
        )
}

/// The @mention popup, floating above the composer: the members the typed `@query` matches.
/// It takes no focus, so typing carries on in the composer while it is open.
pub(super) fn mention_overlay(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    let Some(popup) = chat.mention_popup() else {
        return;
    };
    let items = popup.candidates.iter().map(|c| {
        // Two members with one name look the same, so say which is which.
        let item = rune_kit::PopupItem::new(c.uid.clone(), format!("@{}", c.name));
        if c.duplicate {
            item.description(c.uid.clone())
        } else {
            item
        }
    });
    rune_kit::PopupList::new("mention-popup", "composer-box")
        .label("Mention a member")
        .width(280.0)
        .items(items)
        .active(Some(popup.selected))
        .on_active(|s: &mut RiverApp, i| s.chat.mention_highlight(i))
        .on_choose(|s: &mut RiverApp, uid: &str, _: &mut rune::Tx| s.chat.mention_pick_uid(uid))
        .build(k, layer);
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
