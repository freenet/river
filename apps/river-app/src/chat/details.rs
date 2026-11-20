//! The room details dialog, which opens from the room's name or description in the header. It
//! follows the web UI's `EditRoomModal` (`ui/src/components/room_list/edit_room_modal.rs`):
//! everyone sees the room's name, description, privacy, member count and cap, owner, keys and
//! limits; the owner edits the name, description, member cap and the six size limits there.
//!
//! Where it differs from the web UI:
//! - The web UI saves each field as it loses focus. Changing a room's configuration signs and
//!   publishes room state, so here it is one explicit Save (Enter in a one-line field saves
//!   too) and a Cancel, and all the changes go as one new configuration.
//! - Everyone sees the limits, read-only. The web UI shows them to the owner only. A member sees
//!   what matters first (the owner by name, "134 of 2000 members", the message size, a warning
//!   when the room or its ban list is full) and the rest under a "Room limits" fold-out; the
//!   room's two identifiers sit under "Technical details", each with a sentence on what it is
//!   for. Both start closed. The owner's limits are always fields, grouped, with units; nothing
//!   editable is folded away.
//! - The description is shown as rendered markdown, in full, to everyone; the owner edits its
//!   source after pressing the pencil beside its heading.
//!
//! - "Leave room" is at the foot of the dialog below a rule, as a danger button, for the owner
//!   and for members, as in the web UI,
//!   with the same confirmation wording plus a line saying the room's saved signing key goes
//!   too. Leaving is [`Action::LeaveRoom`] ([`crate::rooms::leave`]).
//! - The owner of a private room has "Rotate" beside the secret version, as in the web UI
//!   ([`Action::RotateSecret`], [`crate::rooms::bans`]).
//!
//! Privacy is shown, never changed: the web UI cannot change it either.

use rune::{El, TextEdit, col, el, icon, row};
use rune_kit::{Button, ButtonVariant, Kit, TextArea, TextInput};

use super::dialog_kit::{
    self as dk, DialogSpec, KEY_TEXT, NoticeKind, copy_field, disclosure, label_value, notice,
    notice_text, rule, value_text,
};
use super::icons;
use super::markdown;
use super::model::{Details, Editable, Limits, Role, Room, RotateOffer};
use super::style::{self as st, Palette};
use super::view::label;
use super::{Chat, Mode, Source};
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
    /// Leave room was pressed: the confirmation is showing.
    pub leaving: bool,
    /// The "Room limits" fold-out (read-only view) is open. It starts closed.
    pub limits_open: bool,
    /// The "Technical details" fold-out is open. It starts closed.
    pub technical_open: bool,
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
    /// Whether anything in the dialog has been typed over: what a restart would lose.
    pub fn has_edits(&self) -> bool {
        self.name.text != self.orig_name
            || self.description.text != self.orig_description
            || self
                .limits
                .iter()
                .zip(self.orig_limits)
                .any(|(typed, orig)| typed.text.trim() != orig.to_string())
    }

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
            leaving: false,
            limits_open: false,
            technical_open: false,
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
        self.clear_ban_confirm();
        self.details = Some(form);
    }

    pub fn close_details(&mut self) {
        self.details = None;
    }

    /// Leave room was pressed: show what leaving does, and ask. Nothing is sent yet.
    pub fn ask_leave(&mut self) {
        if let Some(form) = &mut self.details {
            form.leaving = true;
        }
    }

    /// Back from the leave confirmation: nothing was changed.
    pub fn cancel_leave(&mut self) {
        if let Some(form) = &mut self.details {
            form.leaving = false;
        }
    }

    /// The user confirmed leaving the open room. Unsent text in its composer goes with it (its
    /// unsent messages are dropped by the node side once the room is gone), the dialogs
    /// close, and the room is left; the next room opens when the list no longer has it.
    pub fn confirm_leave(&mut self) {
        let Some(form) = self.details.as_ref().filter(|f| f.leaving) else {
            return;
        };
        let key = form.room.clone();
        if self.room().is_none_or(|r| r.key != key) {
            self.details = None;
            return;
        }
        self.details = None;
        self.member_modal = None;
        self.clear_ban_confirm();
        self.notify_dialog = None;
        self.invite = self.invite.take().filter(|(room, _)| *room != key);
        self.mode = Mode::Write;
        self.picker = None;
        self.composer = TextEdit::new("");
        self.drafts.remove(&key);
        // Where it was read up to is not kept: joined again, it counts unread afresh.
        self.read.remove(&key);
        match self.source {
            Source::Node(_) => self.emit(key, Action::LeaveRoom),
            // Example rooms are only in memory: the room just goes.
            Source::Example => {
                let rest: Vec<Room> = self
                    .rooms
                    .iter()
                    .filter(|r| r.key != key)
                    .cloned()
                    .collect();
                self.set_rooms(rest);
            }
        }
    }

    /// Open or close the "Room limits" fold-out.
    pub fn toggle_details_limits(&mut self) {
        if let Some(form) = &mut self.details {
            form.limits_open = !form.limits_open;
        }
    }

    /// Open or close the "Technical details" fold-out.
    pub fn toggle_details_technical(&mut self) {
        if let Some(form) = &mut self.details {
            form.technical_open = !form.technical_open;
        }
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

    /// Rotate the room's secret (the owner of a private room, from the details dialog). Nothing
    /// goes when it is not offered or one is already on its way. Example rooms move their
    /// version in memory.
    pub fn rotate_secret(&mut self) {
        let Some(room) = self.room() else {
            return;
        };
        // Not while Leave room is being confirmed: the room is about to go.
        if self.details.as_ref().is_some_and(|f| f.leaving)
            || room.details.rotate != RotateOffer::Allowed
            || room
                .details
                .rotation
                .as_ref()
                .is_some_and(|r| r.failed.is_none())
        {
            return;
        }
        let key = room.key.clone();
        match self.source {
            Source::Example => {
                if let Some(r) = self.room_mut() {
                    r.details.secret_version += 1;
                }
            }
            Source::Node(_) => self.emit(key, Action::RotateSecret),
        }
    }

    /// Try the rotation that failed again, or drop it.
    pub fn retry_rotation(&mut self) {
        if let Some(entry) = self
            .room()
            .and_then(|r| r.details.rotation.as_ref())
            .map(|p| p.entry)
        {
            self.retry(entry);
        }
    }

    pub fn discard_rotation(&mut self) {
        if let Some(entry) = self
            .room()
            .and_then(|r| r.details.rotation.as_ref())
            .map(|p| p.entry)
        {
            self.discard(entry);
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
    // (A description that could not be unsealed has an empty source: show the room's own.)
    let desc = if editable && !form.description.text.is_empty() {
        form.description.text.as_str()
    } else {
        room.description.as_str()
    };
    // Members read the description with no heading; the owner's pencil sits beside one.
    let mut head = row().w_full().gap(6.0).h(24.0).center_y();
    if editable {
        head = head.child(label("Description", st::SMALL_STRONG, p.text_muted));
    }
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
                        f.name.focused = false;
                    }
                })
                .child(icon(&icons::EDIT, 14.0).color(p.text_muted)),
        );
    }
    let mut section = col().key("details-description").w_full().gap(2.0);
    if editable {
        section = section.child(head);
    }
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
        match markdown::render(desc, false, st::BODY, p.text, key, p, k, true) {
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

/// A rotation of the secret on its way or failed, in the dialog's words for it.
fn rotation_status(room: &Room, p: &Palette, k: &Kit) -> Option<El> {
    let pending = room.details.rotation.as_ref()?;
    let mut status = row()
        .key("details-rotation-status")
        .w_full()
        .gap(8.0)
        .center_y();
    match &pending.failed {
        None => {
            status = status.child(label(
                "Rotating the secret\u{2026}",
                st::SMALL,
                p.text_muted,
            ));
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
                        .on_press(|s: &mut RiverApp| s.chat.retry_rotation())
                        .build(k)
                        .key("rotation-retry"),
                );
            }
            status = status.child(
                Button::new("Discard")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.chat.discard_rotation())
                    .build(k)
                    .key("rotation-discard"),
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

/// `n` of `max`, in words.
fn of(n: usize, max: usize) -> String {
    format!("{n} of {max}")
}

/// Whether the room already holds as many members as it allows.
fn room_full(d: &Details) -> bool {
    d.usage.members >= d.limits.max_members
}

/// Whether the room's ban list already holds as many bans as it allows.
fn bans_full(d: &Details) -> bool {
    d.usage.bans >= d.limits.max_user_bans
}

/// The line under an owner's field: what the limit is for, its unit, and where the room stands
/// against it. The member count does not include the owner.
fn limit_help(i: usize, d: &Details) -> String {
    match i {
        0 => format!(
            "People the room can hold, not counting the owner. {} now.",
            d.usage.members
        ),
        1 => format!("Recent messages kept. {} held now.", d.usage.messages),
        2 => "Largest message, in bytes.".into(),
        3 => format!(
            "Bans the list can hold. {} on the list now{}.",
            d.usage.bans,
            if bans_full(d) { ", so it is full" } else { "" }
        ),
        4 => "Longest nickname, in bytes.".into(),
        5 => "Longest room name, in bytes.".into(),
        _ => "Longest description, in bytes.".into(),
    }
}

/// `cells` in rows of `cols`, each cell as wide as the others.
fn grid(cells: Vec<El>, cols: usize) -> El {
    let mut out = col().w_full().gap(10.0);
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

/// A small heading over a group of fields.
fn group_heading(text: &str, p: &Palette) -> El {
    label(text.to_owned(), st::SMALL_STRONG, p.text_muted)
}

/// The room's two identifiers, folded away: members rarely need them.
fn technical(d: &Details, form: &DetailsForm, p: &Palette) -> El {
    disclosure(
        "details-technical",
        "Technical details",
        form.technical_open,
        |s| s.chat.toggle_details_technical(),
        vec![
            copy_field(
                "details-owner-key",
                "Room ID",
                Some(
                    "The room owner's public key. It names the room: give it to anyone who needs to refer to this room.",
                ),
                &d.owner_key,
                "Copy room ID",
                KEY_TEXT,
                p,
            ),
            copy_field(
                "details-contract-id",
                "Contract ID",
                Some(
                    "The room's address on the Freenet network. Support and developer tools ask for this one.",
                ),
                &d.contract_id,
                "Copy contract ID",
                KEY_TEXT,
                p,
            ),
        ],
        p,
    )
}

/// What a member sees of the limits: one plain line each, in the room's own units.
fn limit_rows(d: &Details, p: &Palette) -> Vec<El> {
    let l = &d.limits;
    let value = |i: usize, text: String| value_text(&format!("details-limit-{i}"), text, p);
    vec![
        label_value(
            "details-limit-row-1",
            "Messages kept",
            value(
                1,
                format!(
                    "Latest {} ({} held now)",
                    l.max_recent_messages, d.usage.messages
                ),
            ),
            p,
        ),
        label_value(
            "details-limit-row-3",
            "Ban list",
            value(
                3,
                format!(
                    "{} used{}",
                    of(d.usage.bans, l.max_user_bans),
                    if bans_full(d) { ", full" } else { "" }
                ),
            ),
            p,
        ),
        label_value(
            "details-limit-row-4",
            "Nickname",
            value(4, format!("Up to {} bytes", l.max_nickname_size)),
            p,
        ),
        label_value(
            "details-limit-row-5",
            "Room name",
            value(5, format!("Up to {} bytes", l.max_room_name)),
            p,
        ),
        label_value(
            "details-limit-row-6",
            "Description length",
            value(6, format!("Up to {} bytes", l.max_room_description)),
            p,
        ),
    ]
}

/// The owner's editable limits, in two groups, each with its unit and a line saying what it is
/// for. None of them folds away.
fn limit_fields(room: &Room, form: &DetailsForm, narrow: bool, p: &Palette, k: &Kit) -> El {
    let d = &room.details;
    let field = |i: usize| {
        col()
            .key(format!("details-limit-cell-{i}"))
            .grow(1.0)
            .shrink()
            .min_w(0.0)
            .gap(4.0)
            .child(label(LIMIT_LABELS[i], st::SMALL_STRONG, p.text_muted).truncate())
            .child(limit_input(i, LIMIT_LABELS[i], form, k))
            .child(
                label(limit_help(i, d), st::SMALL, p.text_muted)
                    .wrap()
                    .w_full()
                    .key(format!("details-limit-help-{i}")),
            )
    };
    let (cols_a, cols_b) = if narrow { (1, 1) } else { (3, 2) };
    let mut out = col()
        .key("details-limits")
        .w_full()
        .gap(12.0)
        .child(group_heading("Room size", p))
        .child(grid(vec![field(0), field(1), field(3)], cols_a));
    if room_full(d) {
        out = out.child(
            label(
                "The room is at its member limit. Someone joining may cause the most recently invited member to be removed.",
                st::SMALL,
                p.warning_text,
            )
            .wrap()
            .w_full()
            .key("details-room-full"),
        );
    }
    if bans_full(d) {
        out = out.child(
            label(
                "The ban list is full. Adding a ban may drop an older one, and that person could come back.",
                st::SMALL,
                p.warning_text,
            )
            .wrap()
            .w_full()
            .key("details-bans-full"),
        );
    }
    out.child(group_heading("Size limits (bytes)", p))
        .child(
            label(
                "Bytes, not characters: an accented letter takes 2 and an emoji up to 4.",
                st::SMALL,
                p.text_muted,
            )
            .wrap()
            .w_full()
            .key("details-bytes-note"),
        )
        .child(grid(vec![field(2), field(4), field(5), field(6)], cols_b))
}

/// The room's owner, by name, as a link to their member dialog. A room whose member list does
/// not have the owner (it can happen before the first sync) still shows the row, with the short
/// form of the owner's key and no link.
fn owner_link(room: &Room, editable: bool, p: &Palette) -> El {
    let Some(owner) = room.members.iter().find(|m| m.role == Role::Owner) else {
        let key = &room.details.owner_key;
        let short: String = key.chars().take(8).collect();
        let text = if short.is_empty() {
            "Unknown".to_owned()
        } else {
            format!("{short}\u{2026}")
        };
        return label(text, st::BODY, p.text)
            .key("details-owner-unknown")
            .h(if editable { 24.0 } else { 20.0 });
    };
    let uid = owner.uid.clone();
    row()
        .key("details-owner-open")
        .role("button")
        .label(format!("Member info: {}", owner.nickname))
        .focusable(true)
        .cursor(rune::Cursor::Pointer)
        .hover(p.surface_hover)
        .radius(4.0)
        .on_press({
            let uid = uid.clone();
            move |s: &mut RiverApp| s.chat.open_owner_from_details(&uid)
        })
        .on_key(move |s: &mut RiverApp, ev, _| {
            let enter = ev.key == rune::Key::Enter;
            if enter {
                s.chat.open_owner_from_details(&uid);
            }
            enter
        })
        .child(
            col()
                .shrink()
                .min_w(0.0)
                .child(
                    label(owner.nickname.clone(), st::BODY, p.accent)
                        .truncate()
                        .shrink()
                        .min_w(0.0),
                )
                // Underlined, as links are elsewhere.
                .child(el().w_full().h(1.0).fill(p.accent_line)),
        )
}

/// A private room's secret version, with Rotate for the owner.
fn secret_version(d: &Details, form: &DetailsForm, p: &Palette, k: &Kit) -> El {
    // The web UI's Rotate sits beside the version, for the owner only.
    let mut version = row().w_full().gap(8.0).center_y().child(label(
        d.secret_version.to_string(),
        st::BODY,
        p.text,
    ));
    let busy = d.rotation.as_ref().is_some_and(|r| r.failed.is_none());
    match &d.rotate {
        RotateOffer::No => {}
        offer => {
            let mut rotate = Button::new("Rotate")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut RiverApp| s.chat.rotate_secret())
                .build(k)
                .key("details-rotate");
            if busy || form.leaving || matches!(offer, RotateOffer::Blocked(_)) {
                rotate = rotate.disabled(true).opacity(0.45);
            }
            version = version.child(rotate.help(
                "Rotate room secret now (e.g., after suspecting a leak). Banning a member \
                 rotates it too.",
            ));
        }
    }
    label_value("details-secret", "Secret version", version, p)
}

/// "Leave room": a danger action, set apart from the rest of the dialog.
fn leave_button(k: &Kit) -> El {
    dk::danger_outline_button(k, "Leave room", "details-leave", |s: &mut RiverApp| {
        s.chat.ask_leave()
    })
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

/// What the web UI asks before leaving (its owner and member wording), and what this app adds:
/// the room's saved signing key goes with it.
pub const LEAVE_OWNER: &str = "Warning: You are the owner of this room. Leaving will \
permanently delete it for you. Other members might retain access if they have the contract key, \
but coordination will be lost.";
pub const LEAVE_MEMBER: &str = "Are you sure you want to leave this room? This action cannot be \
undone.";
pub const LEAVE_KEY: &str = "Your signing key for this room is removed from River's saved \
rooms on this computer (River in your browser stops showing the room too). A copy may remain \
with the Freenet delegate, which this app cannot remove. If you have no other copy of the \
key, you cannot post as yourself in the room again, and an owner cannot get ownership back.";

fn leave_confirm(owner: bool, p: &Palette, k: &Kit) -> El {
    notice(
        NoticeKind::Danger,
        "details-leave-ask",
        vec![
            label(
                if owner { LEAVE_OWNER } else { LEAVE_MEMBER },
                st::SMALL,
                p.text,
            )
            .wrap()
            .w_full()
            .key("details-leave-text"),
            label(LEAVE_KEY, st::SMALL, p.text)
                .wrap()
                .w_full()
                .key("details-leave-key"),
            dk::footer(
                Vec::new(),
                vec![
                    dk::quiet_button(k, "Cancel", "details-leave-cancel", |s: &mut RiverApp| {
                        s.chat.cancel_leave()
                    }),
                    dk::danger_button(
                        k,
                        "Leave room",
                        "details-leave-confirm",
                        |s: &mut RiverApp| s.chat.confirm_leave(),
                    ),
                ],
            ),
        ],
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
    let mut body = col().key("details").w_full().px(16.0).pb(12.0).gap(14.0);
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
        body = body.child(notice_text(NoticeKind::Warning, "details-blocked", why));
    }
    if editable {
        body = body.child(name_field(form, &p, k));
    }
    body = body.child(description_section(room, form, editable, &p, k));

    // Who owns the room, by name, and a private room's secret version.
    let mut facts = col().key("details-facts").w_full().gap(8.0);
    facts = facts.child(label_value(
        "details-owner",
        "Owner",
        owner_link(room, editable, &p),
        &p,
    ));
    if !editable {
        // What matters to a member: how full the room is and how big a message may be.
        let mut members = col().gap(2.0).child(value_text(
            "details-limit-0",
            format!(
                "{} members (not counting the owner)",
                of(d.usage.members, d.limits.max_members)
            ),
            &p,
        ));
        if room_full(d) {
            members = members.child(
                label("The room is at its member limit. Someone joining may cause the most recently invited member to be removed.", st::SMALL, p.warning_text)
                    .wrap()
                    .w_full()
                    .key("details-room-full"),
            );
        }
        facts = facts.child(label_value("details-members", "Members", members, &p));
        facts = facts.child(label_value(
            "details-message-size",
            "Messages",
            value_text(
                "details-limit-2",
                format!("Up to {} bytes each", d.limits.max_message_size),
                &p,
            ),
            &p,
        ));
        if bans_full(d) {
            facts = facts.child(label_value(
                "details-bans",
                "Bans",
                label(
                    "The ban list is full. Adding a ban may drop an older one, and that person could come back.",
                    st::SMALL,
                    p.warning_text,
                )
                .wrap()
                .w_full()
                .key("details-bans-full"),
                &p,
            ));
        }
    }
    if room.private {
        facts = facts.child(secret_version(d, form, &p, k));
    }
    body = body.child(facts);

    if editable {
        body = body
            .child(rule(&p))
            .child(limit_fields(room, form, narrow, &p, k));
    } else {
        body = body.child(disclosure(
            "details-limits-fold",
            "Room limits",
            form.limits_open,
            |s| s.chat.toggle_details_limits(),
            limit_rows(d, &p),
            &p,
        ));
    }
    body = body.child(technical(d, form, &p));

    let scroll = col()
        .key("details-scroll")
        .w_full()
        // The dialog's title and footer and the window's margins take the rest of the window.
        .max_h((win_h - 240.0).clamp(60.0, 700.0))
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
    if let Some(status) = rotation_status(room, &p, k) {
        footer = footer.child(status);
    }
    if form.leaving {
        // The owner is warned even when the room's details cannot be edited from here yet.
        footer = footer.child(leave_confirm(d.editable != Editable::No, &p, k));
    } else if let Some(warnings) = form.confirming.as_ref().filter(|_| editable) {
        let sure = notice(
            NoticeKind::Warning,
            "details-confirm",
            warnings
                .iter()
                .map(|w| label(w.clone(), st::SMALL, p.text).wrap().w_full())
                .collect(),
        );
        footer = footer.child(sure).child(dk::footer(
            Vec::new(),
            vec![
                dk::quiet_button(k, "Back", "details-back", |s: &mut RiverApp| {
                    s.chat.back_from_confirm()
                }),
                dk::primary_button(
                    k,
                    "Lower and save",
                    "details-confirm-save",
                    |s: &mut RiverApp| s.chat.confirm_details(),
                ),
            ],
        ));
    } else if editable {
        let mut save = dk::primary_button(k, "Save", "details-save", |s: &mut RiverApp| {
            s.chat.submit_details()
        });
        if !chat.can_save_details() || chat.details_edit().is_err() {
            save = save.disabled(true).opacity(0.45);
        }
        footer = footer.child(dk::footer(
            Vec::new(),
            vec![
                dk::quiet_button(k, "Cancel", "details-cancel", |s: &mut RiverApp| {
                    s.chat.close_details()
                }),
                save,
            ],
        ));
    }
    if !form.leaving {
        // Set apart from Save and Cancel: it is the one action here that cannot be undone.
        footer = footer
            .child(rule(&p).key("details-leave-rule"))
            .child(row().w_full().child(leave_button(k)));
    }
    DialogSpec::new("room-details", "Room details", |s: &mut RiverApp| {
        s.chat.close_details()
    })
    .body_key("details-body")
    .width(560.0)
    .gap(10.0)
    .flush()
    .child(scroll)
    .child(footer)
    .build(k, layer);
}
