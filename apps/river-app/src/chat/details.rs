//! The room details dialog, which opens from the room's name or description in the header. It
//! follows the web UI's `EditRoomModal` (`ui/src/components/room_list/edit_room_modal.rs`):
//! everyone sees the room's name, description, privacy, member count and cap, owner, keys and
//! limits; the owner edits the name, description, member cap and the six size limits there.
//!
//! Where it differs from the web UI:
//! - The web UI saves each field as it loses focus. Changing a room's configuration signs and
//!   publishes room state, so here it is one explicit Save (Enter in a one-line field saves
//!   too) and a Cancel, and all the changes go as one new configuration.
//! - Everyone sees the limits, read-only. The web UI shows them to the owner only.
//! - The description is shown as rendered markdown, in full, to everyone; the owner edits its
//!   source after pressing the pencil beside its heading.
//!
//! PARITY-GAPS: the web UI's "Leave Room" and the private room's "Rotate" (secret rotation)
//! are in its dialog too; this app has neither command yet, so the dialog has neither button.
//! Privacy is shown, never changed: the web UI cannot change it either.

use rune::{El, col, el, icon, row};
use rune_kit::{Button, ButtonVariant, Dialog, Kit, TextArea, TextInput};

use super::icons;
use super::markdown;
use super::model::{Details, Editable, Limits, Role, Room};
use super::style::{self as st, Palette};
use super::view::label;
use super::{Chat, Source};
use crate::RiverApp;
use crate::rooms::outbox::{Action, ConfigEdit, LIMIT_LABELS, details_refusal, lowering_warnings};

/// The numeric fields, in the order the web UI lists them (its member cap first).
pub const LIMIT_COUNT: usize = 7;

/// What is typed in the open room details dialog.
#[derive(Clone)]
pub struct DetailsForm {
    /// The room it is open in (`Room::key`).
    pub room: String,
    pub name: rune::TextEdit,
    pub description: rune::TextEdit,
    /// The owner pressed the pencil: the description is the text area, not rendered markdown.
    pub editing_description: bool,
    /// The limits as text, in [`limit_values`] order.
    pub limits: [rune::TextEdit; LIMIT_COUNT],
    /// Save was pressed and the change went to the node: the dialog closes once the room
    /// shows it, and stays to say why when the node refused it.
    pub saving: bool,
    /// What was sent, to know when the room shows it.
    sent: Option<ConfigEdit>,
    /// What the dialog showed when it opened. Only a field typed over is the owner's change:
    /// one another device changed while the dialog was open must not be signed back to the
    /// value it showed.
    orig_name: String,
    orig_description: String,
    orig_limits: [usize; LIMIT_COUNT],
    /// Save would lower a limit below what the room uses: the consequences, waiting for the
    /// owner to confirm.
    pub confirming: Option<Vec<String>>,
}

/// The limits as the form lists them.
pub fn limit_values(l: &Limits) -> [usize; LIMIT_COUNT] {
    [
        l.max_members,
        l.max_recent_messages,
        l.max_message_size,
        l.max_user_bans,
        l.max_nickname_size,
        l.max_room_name,
        l.max_room_description,
    ]
}

impl DetailsForm {
    fn new(room: &Room) -> Self {
        let d = &room.details;
        let mut name = rune::TextEdit::new(&d.name_source);
        name.focused = d.editable == Editable::Yes;
        DetailsForm {
            room: room.key.clone(),
            name,
            description: rune::TextEdit::new(&d.description_source),
            editing_description: false,
            limits: limit_values(&d.limits).map(|v| rune::TextEdit::new(v.to_string())),
            saving: false,
            sent: None,
            orig_name: d.name_source.clone(),
            orig_description: d.description_source.clone(),
            orig_limits: limit_values(&d.limits),
            confirming: None,
        }
    }
}

impl Chat {
    /// Open the open room's details dialog, showing the room as it is now.
    pub fn open_details(&mut self) {
        let Some(form) = self.room().map(DetailsForm::new) else {
            return;
        };
        self.member_modal = None;
        self.details = Some(form);
    }

    pub fn close_details(&mut self) {
        self.details = None;
    }

    /// Open the owner's own member dialog from the details dialog, which closes.
    pub fn open_owner_from_details(&mut self, uid: &str) {
        self.close_details();
        self.open_member(uid);
    }

    /// What Save would send: the changes in the form against the room as shown, or why it
    /// can't be saved (a value that is not a number, or one the room would refuse). `Ok` with
    /// nothing in it when nothing changed.
    pub fn details_edit(&self) -> Result<ConfigEdit, String> {
        let (Some(form), Some(room)) = (&self.details, self.room()) else {
            return Ok(ConfigEdit::default());
        };
        let d = &room.details;
        let mut edit = ConfigEdit::default();
        let name = form.name.text.trim();
        if name != form.orig_name.trim() && name != d.name_source.trim() {
            edit.name = Some(name.to_owned());
        }
        let description = form.description.text.trim();
        if description != form.orig_description.trim() && description != d.description_source.trim()
        {
            edit.description = Some(description.to_owned());
        }
        let held = limit_values(&d.limits);
        let mut parsed = [None; LIMIT_COUNT];
        for (i, typed) in form.limits.iter().enumerate() {
            let typed = typed.text.trim();
            if typed == form.orig_limits[i].to_string() {
                continue;
            }
            let n = typed.parse::<usize>().map_err(|_| {
                format!("{} must be a whole number of at least 1.", LIMIT_LABELS[i])
            })?;
            parsed[i] = (n != held[i]).then_some(n);
        }
        [
            edit.max_members,
            edit.max_recent_messages,
            edit.max_message_size,
            edit.max_user_bans,
            edit.max_nickname_size,
            edit.max_room_name,
            edit.max_room_description,
        ] = parsed;
        match details_refusal(&d.limits, &d.name_source, &d.description_source, &edit) {
            Some(why) => Err(why),
            None => Ok(edit),
        }
    }

    /// Whether Save is possible now: the viewer is the owner, who can sign here, and the last
    /// change has been taken or refused.
    fn can_save_details(&self) -> bool {
        self.room().is_some_and(|r| {
            r.details.editable == Editable::Yes
                && r.details
                    .pending
                    .as_ref()
                    .is_none_or(|p| p.failed.is_some())
        })
    }

    /// A change was sent and has not been taken or refused yet: Save does nothing more.
    fn details_busy(&self) -> bool {
        let pending = self.room().and_then(|r| r.details.pending.as_ref());
        match pending {
            Some(p) => p.failed.is_none(),
            None => self.details.as_ref().is_some_and(|f| f.saving),
        }
    }

    /// Save the changes in the dialog. What can't be used is said in the dialog, which stays
    /// open; nothing changed closes it; a limit lowered below what the room uses asks to be
    /// confirmed first ([`Chat::confirm_details`]); otherwise the change goes to the node
    /// (rooms from the node) and the dialog shows it on its way, closing when the room shows
    /// it, or changes the room in memory (example rooms).
    pub fn submit_details(&mut self) {
        if !self.can_save_details()
            || self.details_busy()
            || self.details.as_ref().is_none_or(|f| f.confirming.is_some())
        {
            return;
        }
        // What is refused is said in the dialog as it is typed, and Save is off.
        let Ok(edit) = self.details_edit() else {
            return;
        };
        if edit.is_empty() {
            self.close_details();
            return;
        }
        let warnings = self
            .room()
            .map(|r| lowering_warnings(&r.details, &edit))
            .unwrap_or_default();
        if !warnings.is_empty() {
            if let Some(form) = &mut self.details {
                form.confirming = Some(warnings);
            }
            return;
        }
        self.send_details(edit);
    }

    /// The owner confirmed what lowering a limit does: send the change.
    pub fn confirm_details(&mut self) {
        if !self.can_save_details()
            || self.details_busy()
            || self.details.as_ref().is_none_or(|f| f.confirming.is_none())
        {
            return;
        }
        let confirmed = self.details.as_mut().and_then(|f| f.confirming.take());
        if let Ok(edit) = self.details_edit()
            && !edit.is_empty()
        {
            // Fields typed over since the consequences were shown need confirming again.
            let now = self
                .room()
                .map(|r| lowering_warnings(&r.details, &edit))
                .unwrap_or_default();
            if !now.is_empty() && Some(&now) != confirmed.as_ref() {
                if let Some(form) = &mut self.details {
                    form.confirming = Some(now);
                }
                return;
            }
            self.send_details(edit);
        }
    }

    /// Go back from the confirmation to the fields.
    pub fn back_from_confirm(&mut self) {
        if let Some(form) = &mut self.details {
            form.confirming = None;
        }
    }

    fn send_details(&mut self, edit: ConfigEdit) {
        let Some(key) = self.room().map(|r| r.key.clone()) else {
            return;
        };
        match self.source {
            Source::Example => {
                if let Some(room) = self.room_mut() {
                    apply_in_memory(room, &edit);
                }
                self.close_details();
            }
            Source::Node(_) => {
                if let Some(form) = &mut self.details {
                    form.saving = true;
                    form.sent = Some(edit.clone());
                }
                self.emit(key, Action::SetRoomDetails { edit });
            }
        }
    }

    /// Try the change to the details that failed again.
    pub fn retry_details(&mut self) {
        if let Some(entry) = self
            .room()
            .and_then(|r| r.details.pending.as_ref())
            .map(|p| p.entry)
        {
            self.retry(entry);
        }
    }

    /// Drop the change to the details that failed.
    pub fn discard_details(&mut self) {
        if let Some(entry) = self
            .room()
            .and_then(|r| r.details.pending.as_ref())
            .map(|p| p.entry)
        {
            self.discard(entry);
        }
        if let Some(form) = &mut self.details {
            form.saving = false;
        }
    }

    /// Settle the dialog after the rooms changed: it belongs to its room; and once the change
    /// it sent shows in the room (nothing is on its way) it has done its work and closes.
    pub(super) fn settle_details(&mut self) {
        let Some(form) = &self.details else {
            return;
        };
        let Some(room) = self.rooms.iter().find(|r| r.key == form.room) else {
            self.details = None;
            return;
        };
        if room.key != self.selected {
            self.details = None;
            return;
        }
        // Sent, nothing is on its way any more, and the room shows what was sent: it was taken.
        if form.saving
            && room.details.pending.is_none()
            && form
                .sent
                .as_ref()
                .is_some_and(|sent| edit_is_shown(&room.details, sent))
        {
            self.details = None;
        }
    }
}

/// Whether the room's details already show every change in `edit`.
fn edit_is_shown(d: &Details, edit: &ConfigEdit) -> bool {
    let held = limit_values(&d.limits);
    let sent = [
        edit.max_members,
        edit.max_recent_messages,
        edit.max_message_size,
        edit.max_user_bans,
        edit.max_nickname_size,
        edit.max_room_name,
        edit.max_room_description,
    ];
    edit.name
        .as_ref()
        .is_none_or(|n| n.trim() == d.name_source.trim())
        && edit
            .description
            .as_ref()
            .is_none_or(|n| n.trim() == d.description_source.trim())
        && sent.iter().zip(held).all(|(s, h)| s.is_none_or(|s| s == h))
}

/// An example room's details changed in memory, as the node would show them.
fn apply_in_memory(room: &mut Room, edit: &ConfigEdit) {
    let d = &mut room.details;
    if let Some(name) = &edit.name {
        room.name = name.clone();
        d.name_source = name.clone();
    }
    if let Some(description) = &edit.description {
        room.description = description.clone();
        d.description_source = description.clone();
    }
    let l = &mut d.limits;
    for (field, new) in [
        (&mut l.max_members, edit.max_members),
        (&mut l.max_recent_messages, edit.max_recent_messages),
        (&mut l.max_message_size, edit.max_message_size),
        (&mut l.max_user_bans, edit.max_user_bans),
        (&mut l.max_nickname_size, edit.max_nickname_size),
        (&mut l.max_room_name, edit.max_room_name),
        (&mut l.max_room_description, edit.max_room_description),
    ] {
        if let Some(n) = new {
            *field = n;
        }
    }
}

fn badge(p: &Palette) -> El {
    row()
        .key("details-private")
        .h(20.0)
        .px(6.0)
        .radius(4.0)
        .center_y()
        .border(1.0, p.line_strong)
        .child(label("PRIVATE", st::TAG, p.text_secondary))
}

/// The width of the name column of the one-line rows.
const LABEL_W: f32 = 110.0;

/// Key and address text, a little under the body size so a 44-character key fits one line.
const KEY_TEXT: rune::tok::TextStyle = rune::tok::TextStyle::new(12.0, 400.0);

/// The description, rendered as markdown (links open as in messages), under its heading. The
/// owner has a pencil beside the heading that turns it into the text area.
fn description_section(
    room: &Room,
    form: &DetailsForm,
    editable: bool,
    p: &Palette,
    k: &Kit,
) -> El {
    // The owner sees what they have typed; everyone else the room's description.
    let desc = if editable {
        form.description.text.as_str()
    } else {
        room.description.as_str()
    };
    let mut head = row().w_full().gap(6.0).h(24.0).center_y().child(label(
        "Description",
        st::SMALL_STRONG,
        p.text_muted,
    ));
    if editable && !form.editing_description {
        head = head.child(
            row()
                .key("details-description-edit")
                .role("button")
                .label("Edit description")
                .help("Edit description")
                .focusable(true)
                .size(24.0, 24.0)
                .radius(6.0)
                .center()
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .on_press(|s: &mut RiverApp| {
                    if let Some(f) = &mut s.chat.details {
                        f.editing_description = true;
                        f.description.focused = true;
                    }
                })
                .child(icon(&icons::EDIT, 14.0).color(p.text_muted)),
        );
    }
    let mut section = col()
        .key("details-description")
        .w_full()
        .gap(2.0)
        .child(head);
    if editable && form.editing_description {
        return section.child(
            TextArea::new(&form.description)
                .label("Room Description".to_owned())
                .placeholder("Optional room description".to_owned())
                .width(None)
                .auto_grow(2, 4)
                .on_edit(|s: &mut RiverApp| &mut s.chat.details.as_mut().expect("open").description)
                .on_key(|s: &mut RiverApp, ev, _| {
                    super::keys::word_keys(
                        &mut s.chat.details.as_mut().expect("open").description,
                        ev,
                    )
                })
                .build(k)
                .key("details-description-input")
                .w_full(),
        );
    }
    let key = format!("details-description-{}", room.key);
    let body = if desc.trim().is_empty() {
        label("No description", st::BODY, p.text_muted)
    } else if markdown::is_cheap(desc) {
        match markdown::render(desc, false, st::BODY, p.text, key, p, k) {
            Some(md) => md,
            None => label(desc.to_owned(), st::BODY, p.text).wrap().w_full(),
        }
    } else {
        label(desc.to_owned(), st::BODY, p.text).wrap().w_full()
    };
    section = section.child(
        col()
            .key("details-description-scroll")
            .w_full()
            .max_h(120.0)
            .scroll(rune::ScrollSpec {
                stick_bottom: false,
            })
            .child(body),
    );
    section
}

fn status(room: &Room, p: &Palette, k: &Kit) -> Option<El> {
    let pending = room.details.pending.as_ref()?;
    let mut status = row().key("details-status").w_full().gap(8.0).center_y();
    match &pending.failed {
        None => {
            status = status.child(label("Sending\u{2026}", st::SMALL, p.text_muted));
        }
        Some(why) => {
            status = status.child(
                label(why.clone(), st::SMALL, p.danger)
                    .wrap()
                    .shrink()
                    .min_w(0.0),
            );
            if pending.retry {
                status = status.child(
                    Button::new("Retry")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut RiverApp| s.chat.retry_details())
                        .build(k)
                        .key("details-retry"),
                );
            }
            status = status.child(
                Button::new("Discard")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.chat.discard_details())
                    .build(k)
                    .key("details-discard"),
            );
        }
    }
    Some(status)
}

type Get = fn(&mut RiverApp) -> &mut rune::TextEdit;

fn limit_input(i: usize, label_text: &str, form: &DetailsForm, k: &Kit) -> El {
    let get: Get = match i {
        0 => |s| &mut s.chat.details.as_mut().expect("open").limits[0],
        1 => |s| &mut s.chat.details.as_mut().expect("open").limits[1],
        2 => |s| &mut s.chat.details.as_mut().expect("open").limits[2],
        3 => |s| &mut s.chat.details.as_mut().expect("open").limits[3],
        4 => |s| &mut s.chat.details.as_mut().expect("open").limits[4],
        5 => |s| &mut s.chat.details.as_mut().expect("open").limits[5],
        _ => |s| &mut s.chat.details.as_mut().expect("open").limits[6],
    };
    TextInput::new(&form.limits[i])
        .label(label_text.to_owned())
        .width(None)
        .on_edit(get)
        .on_key(move |s: &mut RiverApp, ev, _| super::keys::word_keys(get(s), ev))
        .on_submit(|s: &mut RiverApp, _| s.chat.submit_details())
        .build(k)
        .key(format!("details-limit-{i}"))
        .w_full()
}

/// A column holding a caption over `input`.
fn captioned(caption: String, input: El, p: &Palette) -> El {
    col()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(4.0)
        .child(label(caption, st::SMALL_STRONG, p.text_muted))
        .child(input)
}

/// What a limit's room uses now, where cutting it off would lose something, shown under it.
/// The member count does not include the owner.
fn limit_hint(i: usize, d: &Details) -> Option<String> {
    match i {
        0 => Some(format!(
            "{}/{} without the owner",
            d.usage.members, d.limits.max_members
        )),
        1 => Some(format!("{} held now", d.usage.messages)),
        3 => Some(format!("{} in force now", d.usage.bans)),
        _ => None,
    }
}

/// `cells` in rows of `cols`, each cell as wide as the others.
fn grid(cells: Vec<El>, cols: usize) -> El {
    let mut out = col().w_full().gap(8.0);
    let mut cells = cells.into_iter();
    loop {
        let mut line = row().w_full().gap(10.0);
        let mut any = false;
        for _ in 0..cols {
            match cells.next() {
                Some(c) => {
                    any = true;
                    line = line.child(c);
                }
                None => line = line.child(el().grow(1.0).shrink().min_w(0.0)),
            }
        }
        if !any {
            return out;
        }
        out = out.child(line);
    }
}

/// The owner's name field.
fn name_field(form: &DetailsForm, p: &Palette, k: &Kit) -> El {
    captioned(
        "Room Name".into(),
        TextInput::new(&form.name)
            .label("Room Name".to_owned())
            .placeholder("Room name".to_owned())
            .width(None)
            .on_edit(|s: &mut RiverApp| &mut s.chat.details.as_mut().expect("open").name)
            .on_key(|s: &mut RiverApp, ev, _| {
                super::keys::word_keys(&mut s.chat.details.as_mut().expect("open").name, ev)
            })
            .on_submit(|s: &mut RiverApp, _| s.chat.submit_details())
            .build(k)
            .key("details-name-input")
            .w_full(),
        p,
    )
}

/// The seven limits as a grid, the name over each and how much the room uses under it, with
/// `extra` cells after them. The owner's limits are fields.
fn limits_grid(
    room: &Room,
    form: &DetailsForm,
    editable: bool,
    cols: usize,
    extra: Vec<El>,
    p: &Palette,
    k: &Kit,
) -> El {
    let d = &room.details;
    let held = limit_values(&d.limits);
    let mut cells: Vec<El> = (0..LIMIT_COUNT)
        .map(|i| {
            let hint = limit_hint(i, d);
            let mut cell = col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(4.0)
                .child(label(LIMIT_LABELS[i], st::SMALL_STRONG, p.text_muted).truncate());
            cell = if editable {
                cell.child(limit_input(i, LIMIT_LABELS[i], form, k))
            } else {
                cell.child(
                    label(held[i].to_string(), st::BODY, p.text)
                        .key(format!("details-limit-{i}"))
                        .wrap_anywhere(),
                )
            };
            if let Some(hint) = hint {
                cell = cell.child(label(hint, st::SMALL, p.text_muted).truncate());
            }
            cell
        })
        .collect();
    cells.extend(extra);
    let mut out = col()
        .key("details-limits")
        .w_full()
        .gap(6.0)
        .child(grid(cells, cols));
    if d.usage.members >= held[0] {
        out = out.child(label(
            if editable {
                "The room is full, so new members will be rejected."
            } else {
                "The room is full."
            },
            st::SMALL,
            p.text_muted,
        ));
    }
    out
}

/// A name, its value on one line, and a copy button.
fn compact_field(key: &str, name: &str, value: &str, copy_what: &'static str, p: &Palette) -> El {
    let copied = value.to_owned();
    row()
        .key(key.to_owned())
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            label(name.to_owned(), st::SMALL_STRONG, p.text_muted)
                .w(LABEL_W)
                .shrink(),
        )
        .child(
            label(value.to_owned(), KEY_TEXT, p.text)
                .wrap_anywhere()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .key(format!("{key}-value")),
        )
        .child(
            row()
                .key(format!("{key}-copy"))
                .role("button")
                .label(copy_what)
                .help(copy_what)
                .size(24.0, 24.0)
                .radius(6.0)
                .center()
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .on_press_tx(move |_: &mut RiverApp, tx: &mut rune::Tx| tx.copy(copied.clone()))
                .child(icon(&icons::COPY, 14.0).color(p.text_muted)),
        )
}

pub(super) fn details_modal(
    chat: &Chat,
    room: &Room,
    form: &DetailsForm,
    narrow: bool,
    win_h: f32,
    k: &Kit,
    layer: &mut Vec<El>,
) {
    let p = st::palette();
    let d: &Details = &room.details;
    let editable = d.editable == Editable::Yes;
    let mut body = col().key("details").w_full().px(16.0).pb(12.0).gap(10.0);
    if !editable {
        let mut title = row()
            .key("details-title")
            .w_full()
            .gap(8.0)
            .center_y()
            .child(
                label(room.name.clone(), st::TITLE, p.text)
                    .wrap_anywhere()
                    .shrink()
                    .min_w(0.0)
                    .key("details-name"),
            );
        if room.private {
            title = title.child(badge(&p));
        }
        body = body.child(title);
    } else if room.private {
        body = body.child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(badge(&p))
                .child(label(
                    "Name and description are encrypted for members.",
                    st::SMALL,
                    p.text_muted,
                )),
        );
    }
    if let Editable::Blocked(why) = &d.editable {
        body = body.child(
            col()
                .key("details-blocked")
                .w_full()
                .padding(12.0)
                .radius(8.0)
                .fill(p.warning_bg)
                .border(1.0, p.warning)
                .child(label(why.clone(), st::SMALL, p.text).wrap().w_full()),
        );
    }
    if editable {
        body = body.child(name_field(form, &p, k));
    }
    body = body.child(description_section(room, form, editable, &p, k));
    // The owner opens their own dialog from here, as the web UI's member list does. The owner
    // and a private room's secret version are cells of the limits' grid.
    let mut extra = Vec::new();
    if let Some(owner) = room.members.iter().find(|m| m.role == Role::Owner) {
        let uid = owner.uid.clone();
        extra.push(
            captioned(
                "Owner".into(),
                row()
                    .key("details-owner-open")
                    .role("button")
                    .label(format!("Member info: {}", owner.nickname))
                    .focusable(true)
                    .cursor(rune::Cursor::Pointer)
                    .h(if editable { 32.0 } else { 20.0 })
                    .center_y()
                    .on_press(move |s: &mut RiverApp| s.chat.open_owner_from_details(&uid))
                    .child(
                        label(owner.nickname.clone(), st::BODY, p.accent)
                            .truncate()
                            .shrink()
                            .min_w(0.0),
                    ),
                &p,
            )
            .key("details-owner"),
        );
    }
    if room.private {
        extra.push(
            captioned(
                "Secret Version".into(),
                label(d.secret_version.to_string(), st::BODY, p.text),
                &p,
            )
            .key("details-secret"),
        );
    }
    // Three columns where the window is wide, two in a narrow one.
    let cols = if narrow { 2 } else { 3 };
    body = body.child(limits_grid(room, form, editable, cols, extra, &p, k));
    body = body.child(el().w_full().h(1.0).fill(p.line));
    body = body.child(compact_field(
        "details-owner-key",
        "Room Public Key",
        &d.owner_key,
        "Copy room public key",
        &p,
    ));
    body = body.child(compact_field(
        "details-contract-id",
        "Contract ID",
        &d.contract_id,
        "Copy contract ID",
        &p,
    ));
    let scroll = col()
        .key("details-scroll")
        .w_full()
        // The dialog's title and footer and the window's margins take the rest of the window.
        .max_h((win_h - 240.0).clamp(140.0, 700.0))
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(24.0)
        .child(body);
    // Pinned under the fields, so Save is never scrolled out of reach.
    let mut footer = col()
        .key("details-footer")
        .w_full()
        .px(16.0)
        .pb(12.0)
        .gap(8.0);
    if editable {
        // What would be refused is said as it is typed; Save says it too.
        if let Some(problem) = chat.details_edit().err() {
            footer = footer.child(
                label(problem, st::SMALL, p.danger)
                    .wrap()
                    .w_full()
                    .key("details-error"),
            );
        }
    }
    if let Some(status) = status(room, &p, k) {
        footer = footer.child(status);
    }
    if let Some(warnings) = form.confirming.as_ref().filter(|_| editable) {
        let mut sure = col()
            .key("details-confirm")
            .w_full()
            .padding(12.0)
            .gap(6.0)
            .radius(8.0)
            .fill(p.warning_bg)
            .border(1.0, p.warning);
        for w in warnings {
            sure = sure.child(label(w.clone(), st::SMALL, p.text).wrap().w_full());
        }
        footer = footer.child(sure).child(
            row()
                .w_full()
                .gap(8.0)
                .justify_end()
                .child(
                    Button::new("Back")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut RiverApp| s.chat.back_from_confirm())
                        .build(k)
                        .key("details-back"),
                )
                .child(
                    Button::new("Lower and save")
                        .variant(ButtonVariant::Contrast)
                        .on_press(|s: &mut RiverApp| s.chat.confirm_details())
                        .build(k)
                        .key("details-confirm-save"),
                ),
        );
    } else if editable {
        let mut save = Button::new("Save")
            .variant(ButtonVariant::Contrast)
            .on_press(|s: &mut RiverApp| s.chat.submit_details())
            .build(k)
            .key("details-save");
        if !chat.can_save_details() || chat.details_edit().is_err() {
            save = save.disabled(true).opacity(0.45);
        }
        footer = footer.child(
            row()
                .w_full()
                .gap(8.0)
                .justify_end()
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut RiverApp| s.chat.close_details())
                        .build(k)
                        .key("details-cancel"),
                )
                .child(save),
        );
    }
    let dialog_body = col()
        .key("details-body")
        .w_full()
        .gap(10.0)
        .child(scroll)
        .child(footer);
    Dialog::new("room-details", "Room Details")
        .width(560.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.chat.close_details())
        .child(dialog_body)
        .build(k, layer);
}
