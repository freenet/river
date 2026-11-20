//! Invitation cards in a direct-message thread, and Accept.
//!
//! The card is part of a thread's history: a received invitation has the room (its name when the
//! user has that room, else a short form of its key), the sender's note as plain text and an
//! Accept button; one the user sent has no button. Sending is in [`super::dm_picker`] and the
//! signing, sealing and joining in `rooms::dm_invite`.
//!
//! - **The view model holds no invitation.** An invitation is a bearer credential. The card is
//!   drawn from the room it names and the note; pressing Accept sends the background work the
//!   message's name (its purge token), which reads the invitation from the room at that moment,
//!   checks it and starts the join. Nothing here has a `Debug` that prints a note or a token.
//! - **The card says who SENT the message, never who invited.** The envelope carries no sender
//!   binding, so a member can pass on an invitation someone else made; the card names the person
//!   whose signature is on the message and makes no claim about the invitation's own inviter.
//! - **States.** Accept is offered only when the user does not have the room: a room in the
//!   list is "already a member" (or, when the room no longer has the user, that the invitation
//!   is not used), as the join dialog says for a pasted invitation. While the join runs the card
//!   says so, a refusal or a failed join is said on the card, and a second press while one is
//!   under way does nothing. With River moved on, Accept is off and says why.
//! - **Never automatic.** Nothing joins a room except the user pressing Accept on the card, and
//!   the nickname is the generated one the join dialog offers for a pasted invitation, which
//!   the user can change in the room.

use std::time::{Duration, Instant};

use rune::{El, col, icon, row};
use rune_kit::{Button, ButtonVariant, Kit};

use super::dm_view::{clean_name, plain};
use super::icons;
use super::style::{self as st, Palette};
use super::view::label;
use super::{Chat, JoinPreview, Source};
use crate::RiverApp;
use crate::rooms::dm::token_from_hex;
use crate::rooms::generation::UPDATE_REQUIRED_WORK;
use crate::rooms::outbox::Action;
use crate::rooms::work::WorkState;

/// How long an Accept may wait for the background work's answer before the card says it has not
/// heard back.
pub const ANSWER_WAIT: Duration = Duration::from_secs(20);

/// What a card says when its Accept has not been answered for [`ANSWER_WAIT`].
pub const NO_ANSWER: &str = "River has not answered yet. The join may still start: look at the \
     room list before you press Accept again.";

/// What a card offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardState {
    /// Accept can be pressed.
    New,
    /// Accept was pressed and the join is under way.
    Accepting,
    /// Accept was pressed and has not been answered for [`ANSWER_WAIT`].
    Silent,
    /// The background work refused it, in these words. Accept can be pressed again.
    Refused(String),
    /// The join was started and failed, in these words (the room list has Try again).
    Failed(String),
    /// The user is a current member of the room (named as the user's list names it).
    Member { name: String },
    /// The room is in the user's list but no longer has them as a member, so the invitation is
    /// not used.
    Have { name: String },
    /// Accept is off for this reason.
    Blocked(String),
}

/// Where an Accept has got to.
enum Stage {
    /// Sent, not answered. `silent` once [`ANSWER_WAIT`] has passed.
    Asked { at: Instant, silent: bool },
    /// The join was started.
    Started,
    /// Refused, in these words.
    Refused(String),
}

/// An Accept pressed on one card.
struct Ask {
    ticket: u64,
    /// The thread's room (the carrier) and peer, and the message's key (its purge token in hex).
    room: String,
    peer: String,
    key: String,
    /// The room the card names (a sender's claim, checked when the invitation is read).
    stage: Stage,
}

/// The invitation state of the direct-message view: Accepts pressed, and the picker.
#[derive(Default)]
pub struct DmInviteUi {
    next_ticket: u64,
    asks: Vec<Ask>,
    /// The "Share an invite" dialog ([`super::dm_picker`]).
    pub(super) picker: super::dm_picker::Picker,
}

impl std::fmt::Debug for DmInviteUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmInviteUi")
            .field("asks", &self.asks.len())
            .finish_non_exhaustive()
    }
}

impl DmInviteUi {
    /// The next ticket, for an ask of either kind.
    pub(super) fn ticket(&mut self) -> u64 {
        self.next_ticket += 1;
        self.next_ticket
    }

    /// Drop what waits for a room that is no longer listed: its answer will not come.
    pub(super) fn forget_rooms(&mut self, listed: impl Fn(&str) -> bool) {
        self.asks.retain(|a| listed(&a.room));
        self.picker.forget_rooms(listed);
    }

    fn ask_of(&self, room: &str, peer: &str, key: &str) -> Option<&Ask> {
        self.asks
            .iter()
            .find(|a| a.room == room && a.peer == peer && a.key == key)
    }
}

impl Chat {
    /// What the card of the received invitation `key` (the message's key) in the thread
    /// `room`/`peer`, which names the room `target`, offers now.
    pub fn dm_card_state(&self, room: &str, peer: &str, key: &str, target: &str) -> CardState {
        match self.join_preview_for(target.to_owned()) {
            JoinPreview::Member { name, .. } => return CardState::Member { name },
            JoinPreview::Have { name, .. } => return CardState::Have { name },
            _ => {}
        }
        // A join of this room under way or failed, started from here or from anywhere (the join
        // dialog, another card): the room is one, so its card says so.
        // A join that finished and whose room is not in the list yet is still the join.
        if let Some(w) = self.work.iter().find(|w| w.room.as_deref() == Some(target)) {
            return match &w.state {
                WorkState::Failed { why, .. } => CardState::Failed(why.clone()),
                _ => CardState::Accepting,
            };
        }
        match self.dm.invite.ask_of(room, peer, key).map(|a| &a.stage) {
            Some(Stage::Asked { silent: true, .. }) => return CardState::Silent,
            Some(Stage::Asked { .. }) => return CardState::Accepting,
            Some(Stage::Refused(why)) => return CardState::Refused(why.clone()),
            // A join the work no longer lists (put away) is over: Accept is offered again.
            Some(Stage::Started) | None => {}
        }
        if self.issues.update_required {
            return CardState::Blocked(UPDATE_REQUIRED_WORK.to_owned());
        }
        CardState::New
    }

    /// The card's Accept: ask the background work to read the invitation from the message and
    /// join its room. False when nothing was asked (not offered, one is under way already, or
    /// the example, which sends nothing).
    pub fn dm_accept(&mut self, key: &str) -> bool {
        self.dm_accept_at(key, Instant::now())
    }

    /// [`Chat::dm_accept`] at `now`.
    pub fn dm_accept_at(&mut self, key: &str, now: Instant) -> bool {
        let Some((room, peer)) = self
            .dm
            .open_key()
            .map(|(r, p)| (r.to_owned(), p.to_owned()))
        else {
            return false;
        };
        let Some(target) = self
            .dm
            .snapshot()
            .threads
            .iter()
            .find(|t| t.room == room && t.peer_key == peer)
            .and_then(|t| t.messages.iter().find(|m| m.key == key && !m.outgoing))
            .and_then(|m| match &m.body {
                super::dm_model::DmBody::Invite { room, .. } => Some(room.clone()),
                _ => None,
            })
        else {
            return false;
        };
        if !matches!(
            self.dm_card_state(&room, &peer, key, &target),
            CardState::New | CardState::Refused(_) | CardState::Silent
        ) {
            return false;
        }
        let (Some(from), Some(token)) =
            (crate::rooms::dm::peer_from_key(&peer), token_from_hex(key))
        else {
            return false;
        };
        if matches!(self.source, Source::Example) {
            return false;
        }
        self.dm
            .invite
            .asks
            .retain(|a| !(a.room == room && a.peer == peer && a.key == key));
        let ticket = self.dm.invite.ticket();
        self.dm.invite.asks.push(Ask {
            ticket,
            room: room.clone(),
            peer,
            key: key.to_owned(),
            stage: Stage::Asked {
                at: now,
                silent: false,
            },
        });
        self.emit(
            room,
            Action::AcceptInviteDm {
                peer: from,
                token,
                ticket,
            },
        );
        true
    }

    /// Mark the Accepts and the send not answered within [`ANSWER_WAIT`] of `now`. True when that
    /// changed what shows.
    pub(super) fn dm_invite_tick(&mut self, now: Instant) -> bool {
        let mut changed = self.dm.invite.picker.tick(now);
        for a in &mut self.dm.invite.asks {
            if let Stage::Asked { at, silent } = &mut a.stage
                && !*silent
                && now.saturating_duration_since(*at) >= ANSWER_WAIT
            {
                *silent = true;
                changed = true;
            }
        }
        changed
    }

    /// What became of the invitation ask with `ticket`, an Accept or the picker's send. A late
    /// answer counts the same. Returns the thread to scroll to its end when a send was queued
    /// and its thread is the one open.
    pub fn dm_invite_answer(
        &mut self,
        room: &str,
        peer: &str,
        ticket: u64,
        result: Result<(), String>,
    ) -> Option<(String, String)> {
        if let Some(ask) = self.dm.invite.asks.iter_mut().find(|a| a.ticket == ticket) {
            ask.stage = match result {
                Ok(()) => Stage::Started,
                Err(why) => Stage::Refused(why),
            };
            return None;
        }
        self.dm_picker_answer(room, peer, ticket, result)
    }
}

// The view ------------------------------------------------------------------------------------

/// The card of an invitation in a thread. `received` is the sender's invitation (with Accept),
/// else the user's own. `room` and `note` are a peer's words: plain text.
#[allow(clippy::too_many_arguments)]
pub(super) fn card(
    chat: &Chat,
    thread: (&str, &str),
    key: &str,
    received: Option<(&str, &str)>,
    room_label: Option<&str>,
    note: Option<&str>,
    sender: &str,
    p: &Palette,
    k: &Kit,
) -> El {
    let title = if received.is_some() {
        "Invitation"
    } else {
        "You sent an invitation"
    };
    let mut c = col()
        .key(format!("dm-invite-{key}"))
        .w_full()
        .max_w(420.0)
        .px(12.0)
        .py(10.0)
        .gap(6.0)
        .radius(10.0)
        .fill(p.surface)
        .border(1.0, p.line_strong)
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(icon(&icons::INVITE, 15.0).color(p.text_secondary))
                .child(label(title.to_owned(), st::NAME, p.text)),
        );
    if let Some(room) = room_label {
        c = c.child(
            row()
                .gap(6.0)
                .child(label("Room:", st::SMALL, p.text_muted))
                .child(
                    label(room.to_owned(), st::SMALL_STRONG, p.text)
                        .key(format!("dm-invite-room-{key}"))
                        .wrap()
                        .shrink(),
                ),
        );
    }
    if let Some(note) = note {
        c = c.child(
            col()
                .key(format!("dm-invite-note-{key}"))
                .w_full()
                .child(plain(note.to_owned(), p.text)),
        );
    }
    let Some((_, target)) = received else {
        return c;
    };
    let state = chat.dm_card_state(thread.0, thread.1, key, target);
    let sender = clean_name(sender);
    let sender = if sender.is_empty() {
        "this person".to_owned()
    } else {
        sender
    };
    let say = |text: String, id: &str, color: rune::Color| {
        label(text, st::SMALL, color)
            .key(format!("dm-invite-{id}-{key}"))
            .wrap()
            .w_full()
    };
    let accept = |enabled: bool| {
        let msg = key.to_owned();
        let mut b = Button::new("Accept")
            .variant(ButtonVariant::Contrast)
            .on_press(move |s: &mut RiverApp| {
                s.chat.dm_accept(&msg);
            })
            .build(k)
            .key(format!("dm-invite-accept-{key}"));
        if !enabled {
            b = b.disabled(true).opacity(0.45);
        }
        row().w_full().justify_end().child(b)
    };
    match state {
        CardState::New | CardState::Refused(_) | CardState::Silent => {
            c = c.child(say(
                format!(
                    "Sent by {sender}. Accepting joins this room as a new member with a \
                     generated nickname, which you can change in the room."
                ),
                "from",
                p.text_muted,
            ));
            match &state {
                CardState::Refused(why) => c = c.child(say(why.clone(), "refused", p.danger)),
                CardState::Silent => c = c.child(say(NO_ANSWER.to_owned(), "silent", p.danger)),
                _ => {}
            }
            c = c.child(accept(true));
        }
        CardState::Accepting => {
            c = c.child(say("Joining\u{2026}".to_owned(), "joining", p.text_muted));
        }
        CardState::Failed(why) => {
            c = c.child(say(
                format!("Joining failed. {why} Use Try again in the room list."),
                "failed",
                p.danger,
            ));
        }
        CardState::Member { name } => {
            let name = clean_name(&name);
            c = c.child(say(
                format!("You are already a member of {name}."),
                "member",
                p.text_muted,
            ));
            let open = target.to_owned();
            c = c.child(
                row().w_full().justify_end().child(
                    Button::new("Open room")
                        .variant(ButtonVariant::Ghost)
                        .on_press(move |s: &mut RiverApp| s.chat.select(&open))
                        .build(k)
                        .key(format!("dm-invite-open-{key}")),
                ),
            );
        }
        CardState::Have { name } => {
            let name = clean_name(&name);
            c = c.child(say(
                format!(
                    "{name} is in your list, but it no longer has you as a member, so this \
                     invitation is not used."
                ),
                "have",
                p.text_muted,
            ));
        }
        CardState::Blocked(why) => {
            c = c
                .child(say(why, "blocked", p.text_muted))
                .child(accept(false));
        }
    }
    c
}

#[cfg(test)]
#[path = "dm_invite_tests.rs"]
mod tests;
