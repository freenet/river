//! "Share an invite" in a member's dialog: pick one of your other rooms, add a note, and send the
//! member an invitation to it in a direct message.
//!
//! It follows the web UI's "Share an invite via DM" picker (`ui/src/components/
//! direct_messages/invite_via_dm_picker_modal.rs`). The invitation is made, sealed and sent by
//! the background work (`rooms::dm_invite`); this module chooses the room and the note, and
//! says what became of the ask.
//!
//! - **Which rooms.** The web UI lists every other room. This lists the rooms you can write in
//!   (the same standing a message needs: you have the key here, are not banned, are a member,
//!   and for a private room have its secret) and says when there are none. The background work
//!   also requires that you are listed as a member of the room, which an invitation needs.
//! - **The note** is optional and bounded ([`MAX_NOTE_CHARS`]); characters that draw nothing or
//!   reorder text are refused.
//! - **Off, with the reason in words,** when the room you are writing in forbids sending (which
//!   includes River having moved on), the person is no longer a member, or there is no other room.
//! - **Nothing is sent twice.** While a send waits for its answer the dialog cannot send; when
//!   the answer is late it says so and lets you look in the conversation first.
//! - **Nothing here holds an invitation**, and nothing is logged or printed.

use std::time::Instant;

use rune::{El, col, icon, row};
use rune_kit::{Kit, ScrollBar, TextInput};

use super::dialog_kit::{self as dk, DialogSpec};
use super::icons;
use super::model::Room;
use super::style::{self as st, Palette};
use super::view::label;
use super::{Chat, Source};
use crate::RiverApp;
use crate::rooms::dm_invite::{MAX_NOTE_CHARS, note_refusal};
use crate::rooms::dm_send::Refusal;
use crate::rooms::generation::UPDATE_REQUIRED_SEND;
use crate::rooms::outbox::Action;

/// What the dialog says when a send has not been answered for the wait.
pub const NO_ANSWER: &str = "River has not answered yet. The invitation may still go out: look \
     in the conversation before you send it again.";

struct Open {
    /// The room the message goes in, and the person it goes to (their `peer_key`).
    room: String,
    peer: String,
    /// The room chosen to invite them to.
    selected: Option<String>,
    /// Why the background work refused the last send.
    error: Option<String>,
    sending: Option<Sending>,
}

struct Sending {
    ticket: u64,
    at: Instant,
    /// Not answered within the wait.
    silent: bool,
}

/// The picker's state.
#[derive(Default)]
pub struct Picker {
    open: Option<Open>,
    /// The note's box. Kept apart from `open` so its edit handler is always valid.
    note: rune::TextEdit,
}

impl Picker {
    /// Drop what waits for a room that is no longer listed.
    pub(super) fn forget_rooms(&mut self, listed: impl Fn(&str) -> bool) {
        if self.open.as_ref().is_some_and(|o| !listed(&o.room)) {
            self.open = None;
            self.note.clear();
        }
    }

    /// Mark a send not answered within the wait. True when that changed what shows.
    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let Some(s) = self.open.as_mut().and_then(|o| o.sending.as_mut()) else {
            return false;
        };
        if !s.silent && now.saturating_duration_since(s.at) >= super::dm_invite::ANSWER_WAIT {
            s.silent = true;
            return true;
        }
        false
    }

    #[cfg(test)]
    pub(super) fn note_text(&self) -> &str {
        &self.note.text
    }
}

impl Chat {
    /// Whether the picker is showing.
    pub fn invite_picker_open(&self) -> bool {
        self.dm.invite.picker.open.is_some()
    }

    /// Open the picker for the member `peer` (their full id, as `Member::full_id` shows it) of
    /// the room keyed `room`. False when there is no such room or member, or the member is the
    /// user.
    pub fn open_invite_picker(&mut self, room: &str, peer: &str) -> bool {
        let known = self
            .rooms()
            .iter()
            .any(|r| r.key == room && r.member_by_peer_key(peer).is_some_and(|m| m.id != r.me));
        if !known {
            return false;
        }
        // The member's dialog gives way to this one.
        self.close_member();
        self.dm.invite.picker.note.clear();
        self.dm.invite.picker.open = Some(Open {
            room: room.to_owned(),
            peer: peer.to_owned(),
            selected: None,
            error: None,
            sending: None,
        });
        true
    }

    /// Close the picker, unless a send is waiting for its answer.
    pub fn close_invite_picker(&mut self) {
        // Not while a send waits for its answer (the answer would have nowhere to say what
        // became of it), as the web UI does not close its picker then either. A send that went
        // unanswered for the wait can be left.
        if self
            .dm
            .invite
            .picker
            .open
            .as_ref()
            .and_then(|o| o.sending.as_ref())
            .is_some_and(|s| !s.silent)
        {
            return;
        }
        self.dm.invite.picker.open = None;
        self.dm.invite.picker.note.clear();
    }

    /// The rooms the open picker offers: every other room the user can write in.
    pub fn invite_picker_rooms(&self) -> Vec<&Room> {
        let Some(open) = &self.dm.invite.picker.open else {
            return Vec::new();
        };
        self.rooms()
            .iter()
            .filter(|r| r.key != open.room && r.send_block.is_none() && !r.not_member)
            .collect()
    }

    /// Choose the room to invite to. Ignored for a room not on offer.
    pub fn invite_pick(&mut self, target: &str) {
        if !self.invite_picker_rooms().iter().any(|r| r.key == target) {
            return;
        }
        if let Some(open) = self.dm.invite.picker.open.as_mut()
            && open.sending.is_none()
        {
            open.selected = Some(target.to_owned());
        }
    }

    /// Why the open picker cannot send whatever it holds, `None` when it can.
    pub fn invite_picker_block(&self) -> Option<String> {
        let open = self.dm.invite.picker.open.as_ref()?;
        let Some(room) = self.rooms().iter().find(|r| r.key == open.room) else {
            return Some("This room is no longer here.".to_owned());
        };
        if self.issues.update_required {
            return Some(UPDATE_REQUIRED_SEND.to_owned());
        }
        if let Some(why) = &room.send_block {
            return Some(why.clone());
        }
        match room.member_by_peer_key(&open.peer) {
            None => Some(Refusal::NotMember.text()),
            Some(m) if m.id == room.me => Some(Refusal::SelfMessage.text()),
            Some(_) => None,
        }
    }

    /// The note's refusal, if the box holds one that can't be sent.
    pub fn invite_note_refusal(&self) -> Option<String> {
        note_refusal(&self.dm.invite.picker.note.text)
    }

    /// Put `text` in the note's box. For scenes and tests.
    pub fn invite_write(&mut self, text: &str) {
        self.dm.invite.picker.note.replace_text(text);
    }

    /// [`Chat::invite_send_at`] at the time it is.
    pub fn invite_send(&mut self) -> bool {
        self.invite_send_at(Instant::now())
    }

    /// Send the chosen room's invitation, with the note, to the person. The dialog stays until
    /// the answer ([`Chat::dm_invite_answer`]). False when nothing was sent.
    pub fn invite_send_at(&mut self, now: Instant) -> bool {
        if self.invite_picker_block().is_some() || self.invite_note_refusal().is_some() {
            return false;
        }
        let Some(open) = self.dm.invite.picker.open.as_ref() else {
            return false;
        };
        let (room, peer_key) = (open.room.clone(), open.peer.clone());
        let Some(target) = open
            .selected
            .clone()
            .filter(|t| self.invite_picker_rooms().iter().any(|r| &r.key == t))
        else {
            return false;
        };
        if open.sending.as_ref().is_some_and(|s| !s.silent) {
            return false;
        }
        let Some(peer) = crate::rooms::dm::peer_from_key(&peer_key) else {
            return false;
        };
        let note = self.dm.invite.picker.note.text.trim().to_owned();
        if matches!(self.source, Source::Example) {
            // The example sends nothing.
            return true;
        }
        let ticket = self.dm.invite.ticket();
        if let Some(open) = self.dm.invite.picker.open.as_mut() {
            open.error = None;
            open.sending = Some(Sending {
                ticket,
                at: now,
                silent: false,
            });
        }
        self.emit(
            room,
            Action::SendInviteDm {
                peer,
                target,
                note: (!note.is_empty()).then_some(note),
                ticket,
            },
        );
        true
    }

    /// The answer to a picker send. Queued: the picker closes and the conversation with the
    /// person opens, where the invitation shows as sent. Refused: the dialog keeps its choice and
    /// note and says why. Returns the thread to scroll to its end when it was opened.
    pub(super) fn dm_picker_answer(
        &mut self,
        room: &str,
        peer: &str,
        ticket: u64,
        result: Result<(), String>,
    ) -> Option<(String, String)> {
        let open = self.dm.invite.picker.open.as_mut()?;
        if open.sending.as_ref().map(|s| s.ticket) != Some(ticket) {
            return None;
        }
        open.sending = None;
        match result {
            Ok(()) => {
                self.dm.invite.picker.open = None;
                self.dm.invite.picker.note.clear();
                self.open_dm(room, peer)
                    .then(|| (room.to_owned(), peer.to_owned()))
            }
            Err(why) => {
                open.error = Some(why);
                None
            }
        }
    }
}

// The view ------------------------------------------------------------------------------------

/// The picker dialog, when it is open.
pub(super) fn dialog(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    let Some(open) = &chat.dm.invite.picker.open else {
        return;
    };
    let p = st::palette();
    let room = chat.rooms().iter().find(|r| r.key == open.room);
    let name = room
        .and_then(|r| r.member_by_peer_key(&open.peer))
        .map(|m| super::dm_view::clean_name(&m.nickname))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "this person".to_owned());
    let candidates = chat.invite_picker_rooms();
    let block = chat.invite_picker_block();
    let waiting = open.sending.as_ref().is_some_and(|s| !s.silent);
    let silent = open.sending.as_ref().is_some_and(|s| s.silent);
    let mut body = DialogSpec::new(
        "dm-invite-picker",
        format!("Invite {name} to another room"),
        |s: &mut RiverApp| s.chat.close_invite_picker(),
    )
    .body_key("dm-invite-picker-body")
    .width(460.0);
    if let Some(why) = &block {
        body = body.child(
            label(why.clone(), st::SMALL, p.danger)
                .key("dm-invite-block")
                .wrap()
                .w_full(),
        );
    }
    if candidates.is_empty() {
        body = body.child(
            label(
                "You are not in another room you can invite from. Join or create one, then come \
                 back here.",
                st::BODY,
                p.text_secondary,
            )
            .key("dm-invite-none")
            .wrap()
            .w_full(),
        );
    } else {
        let mut list = col().key("dm-invite-scroll").w_full().gap(12.0);
        list = list
            .child(
                label(
                    format!(
                        "Send {name} an invitation card in this conversation. They see an Accept \
                         button; nothing is copied or pasted."
                    ),
                    st::SMALL,
                    p.text_secondary,
                )
                .wrap()
                .w_full(),
            )
            .child(label("Which room?", st::SMALL_STRONG, p.text_muted));
        for r in &candidates {
            list = list.child(room_row(
                r,
                open.selected.as_deref() == Some(&r.key),
                waiting,
                &p,
            ));
        }
        let over = chat.invite_note_refusal();
        list = list.child(label(
            "Personal note (optional)",
            st::SMALL_STRONG,
            p.text_muted,
        ));
        list = list.child(
            TextInput::new(&chat.dm.invite.picker.note)
                .label("Personal note (optional)")
                .placeholder("e.g. Thought you would enjoy this room")
                .width(None)
                .on_edit(|s: &mut RiverApp| &mut s.chat.dm.invite.picker.note)
                .on_submit(|s: &mut RiverApp, _| {
                    s.chat.invite_send();
                })
                .build(k)
                .key("dm-invite-note")
                .w_full(),
        );
        let chars = chat.dm.invite.picker.note.text.chars().count();
        if let Some(why) = over {
            list = list.child(
                label(why, st::SMALL, p.danger)
                    .key("dm-invite-note-error")
                    .wrap()
                    .w_full(),
            );
        } else if chars > MAX_NOTE_CHARS / 5 * 4 {
            list = list.child(
                label(format!("{chars}/{MAX_NOTE_CHARS}"), st::SMALL, p.text_muted)
                    .key("dm-invite-note-count")
                    .self_end(),
            );
        }
        // The rooms and the note scroll when they are taller than the window, so Cancel and
        // Send stay in view.
        body = body.child(
            list.scroll(rune::ScrollSpec {
                stick_bottom: false,
            })
            .shrink()
            .min_h(0.0)
            .scroll_edge_fade(24.0)
            .scroll_bar(k),
        );
    }
    if let Some(why) = &open.error {
        body = body.child(
            label(why.clone(), st::SMALL, p.danger)
                .key("dm-invite-error")
                .wrap()
                .w_full(),
        );
    }
    if silent {
        body = body.child(
            label(NO_ANSWER, st::SMALL, p.danger)
                .key("dm-invite-no-answer")
                .wrap()
                .w_full(),
        );
    }
    if waiting {
        body = body.child(
            label("Sending\u{2026}", st::SMALL, p.text_muted)
                .key("dm-invite-sending")
                .w_full(),
        );
    }
    let can_send = block.is_none()
        && !waiting
        && open
            .selected
            .as_ref()
            .is_some_and(|t| candidates.iter().any(|r| &r.key == t))
        && chat.invite_note_refusal().is_none();
    let mut send = dk::primary_button(
        k,
        "Send invitation",
        "dm-invite-send",
        |s: &mut RiverApp| {
            s.chat.invite_send();
        },
    );
    if !can_send {
        send = send.disabled(true).opacity(0.45);
    }
    body.footer(dk::footer(
        Vec::new(),
        vec![
            dk::quiet_button(k, "Cancel", "dm-invite-cancel", |s: &mut RiverApp| {
                s.chat.close_invite_picker()
            }),
            send,
        ],
    ))
    .build(k, layer);
}

/// One room on offer: its name, a lock for a private room, and how many members it has.
fn room_row(r: &Room, selected: bool, waiting: bool, p: &Palette) -> El {
    let key = r.key.clone();
    let name = super::dm_view::clean_name(&r.name);
    let name = if name.is_empty() {
        "Unnamed room".to_owned()
    } else {
        name
    };
    let members = r.members.len();
    let mut line = row()
        .key(format!("dm-invite-room-{}", r.key))
        .role("button")
        .label(format!("Invite to {name}"))
        .w_full()
        .px(12.0)
        .py(8.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .border(1.0, if selected { p.accent } else { p.line })
        .fill(if selected { p.surface_hover } else { p.surface })
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut RiverApp| s.chat.invite_pick(&key));
    if waiting {
        line = line.disabled(true).opacity(0.6);
    } else {
        line = line.hover(p.surface_hover);
    }
    if r.private {
        line = line.child(icon(&icons::LOCK, 13.0).color(p.text_muted));
    }
    line.child(
        label(name, st::BODY, p.text)
            .truncate()
            .shrink()
            .min_w(0.0)
            .grow(1.0),
    )
    .child(label(
        if members == 1 {
            "1 member".to_owned()
        } else {
            format!("{members} members")
        },
        st::SMALL,
        p.text_muted,
    ))
}
