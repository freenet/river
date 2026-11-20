//! The member info dialog, which opens from a row of the members panel. It follows the web UI's
//! `MemberInfoModal` (`ui/src/components/members/member_info_modal.rs`): who the member is, how
//! they came to be in the room, and a warning when their name imitates the owner's.
//!
//! Its Message button opens the conversation with the member (for the member the dialog shows, by full id), and Share
//! invite opens the picker that sends them an invitation to another room ([`super::dm_picker`]). Your own dialog edits your nickname
//! (`nickname_editor`), as the web UI's `NicknameField` does. Deputize and Revoke deputy
//! (`deputy_block`) work as the web UI's `DeputyButton` does, with a confirmation first. Ban
//! (`ban_confirmation`) is offered as the web UI's `ban_gate` decides it, with a confirmation;
//! in a private room the ban also rotates the room's secret, which only the owner's computer can
//! do, so others are told why not ([`crate::rooms::bans`]).

use std::collections::HashMap;

use rune::{El, col, el, icon, row};
use rune_kit::{Button, ButtonVariant, Kit, TextInput};

use super::conversation::avatar;
use super::dialog_kit::{self as dk, DialogSpec, NoticeKind, copy_field, notice};
use super::icons;
use super::model::{Appointer, BanOffer, Member, MemberId, Role, Room};
use super::style::{self as st, Palette};
use super::view::label;
use super::{Chat, Source};
use crate::RiverApp;

/// The web UI's `ImpersonationWarning::tooltip` for an imitated owner. The text before the
/// first colon is the chip's label.
/// The web UI's `ImpersonationWarning::tooltip` for a member imitating the owner.
pub(super) const IMPERSONATION: &str = "Impersonation warning: this member is NOT the room \
     owner, but their name closely resembles the owner's. The real owner has a crown icon in the \
     member list and an OWNER tag on their messages.";
/// The same for a member imitating a deputy.
pub(super) const IMPERSONATION_DEPUTY: &str = "Impersonation warning: this member is NOT a \
     moderator, but their name closely resembles one. A real moderator has a shield icon next \
     to their name and a DEPUTY tag on their messages.";
/// The same when the flagged member has privileges too, so "NOT the owner" would be false.
pub(super) const NAME_CONFLICT: &str = "Name conflict: this member's name is visually the same \
     as another member's who also has privileges in this room. Both are real; check the member \
     ID shown on hover to tell them apart.";

/// The web UI's `UNNAMED` and `UNKNOWN_MEMBER` (`ui/src/util/display_name.rs`), which it never
/// protects.
const PLACEHOLDERS: [&str; 2] = ["Unnamed", "Unknown"];

/// A member whose name resembles a privileged member's, and who that is.
pub(super) struct Imitation<'a> {
    pub who: &'a Member,
    /// The warning's sentence. It names no member: a name could forge a role in a flat string.
    pub text: &'static str,
}

impl Imitation<'_> {
    /// The text before the first colon, the dialog's chip label.
    pub fn label(&self) -> &'static str {
        self.text.split(':').next().unwrap_or_default()
    }
}

/// The privileged member `m`'s name resembles without being them, if it does.
///
/// The protected names are the owner's and every deputy's in this viewer's view, as the web
/// UI's `impersonation_checker_for_viewer` builds them: a name only counts when its member chose
/// it and this computer can read it, and not when it folds to a placeholder or to a generated
/// handle (each shared by a whole class of members). The owner is checked first, then the
/// deputies in River's member id order (`Member::order`). A member is never accused of imitating a name that is theirs,
/// and a flagged member who holds privileges themselves gets a sentence that is true of both.
pub(super) fn imitation<'a>(room: &'a Room, m: &Member) -> Option<Imitation<'a>> {
    use super::visual_fold::{names_fold_to_generated_handle, names_fold_together};
    let protected = |p: &Member| {
        p.claimed
            && !PLACEHOLDERS
                .iter()
                .any(|ph| names_fold_together(&p.nickname, ph))
            && !names_fold_to_generated_handle(&p.nickname)
    };
    let mut deputies: Vec<&Member> = room
        .members
        .iter()
        .filter(|d| d.role == Role::Deputy)
        .collect();
    deputies.sort_by_key(|d| d.order);
    let owners = room.members.iter().filter(|o| o.role == Role::Owner);
    let who = owners
        .chain(deputies)
        .filter(|p| protected(p))
        .find(|p| p.id != m.id && names_fold_together(&m.nickname, &p.nickname))?;
    let text = if m.role != Role::Member {
        NAME_CONFLICT
    } else if who.role == Role::Owner {
        IMPERSONATION
    } else {
        IMPERSONATION_DEPUTY
    };
    Some(Imitation { who, text })
}

/// Whether `m` was invited, directly or not, by `by`. The walk is bounded by the member count,
/// so a cycle (or an invite of oneself) in hostile data ends.
fn invited_through(by_id: &HashMap<MemberId, &Member>, m: &Member, by: &Member) -> bool {
    let mut at = m;
    for _ in 0..=by_id.len() {
        let Some(inviter) = at.invited_by.and_then(|i| by_id.get(&i)) else {
            return false;
        };
        if inviter.id == by.id {
            return true;
        }
        at = inviter;
    }
    false
}

fn chip(key: &str, text: &str, color: rune::Color, line: rune::Color) -> El {
    row()
        .key(key.to_owned())
        .h(22.0)
        .px(8.0)
        .radius(11.0)
        .center()
        .border(1.0, line)
        .child(label(text.to_owned(), st::SMALL_STRONG, color))
}

/// The verifying key is 44 characters: one size under the body text fits it on a line at the
/// dialog's width, where the body size left a lone character on a second line.
const KEY_TEXT: rune::tok::TextStyle = rune::tok::TextStyle::new(13.0, 400.0);

/// Your own nickname, editable as the web UI's `NicknameField` is, but with Save and Cancel
/// (and Enter to save). That is a deliberate divergence from the web UI, which saves when the
/// field loses focus or on Enter: a change that signs and publishes room state should be an
/// explicit act here, not a side effect of tabbing away. What is refused is said under the
/// field, and a change on its way, or that failed, is shown with Retry and Discard. When the
/// room would not take a change (you can't write there, or it removed you for inactivity) that
/// is said before Save, which is then disabled.
fn nickname_editor(room: &Room, m: &Member, chat: &Chat, p: &Palette, k: &Kit) -> El {
    let pending = room.own_nickname.as_ref();
    let sending = pending.is_some_and(|n| n.failed.is_none());
    let problem = chat.nickname_problem().or_else(|| {
        chat.nickname_error
            .clone()
            .filter(|_| chat.nickname.text.trim().is_empty())
    });
    let mut editor = col().key("member-nickname-edit").w_full().gap(8.0).child(
        TextInput::new(&chat.nickname)
            .label("Nickname".to_owned())
            .placeholder("Your nickname in this room".to_owned())
            .width(None)
            .on_edit(|s: &mut RiverApp| &mut s.chat.nickname)
            .on_submit(|s: &mut RiverApp, _| s.chat.submit_nickname())
            .build(k)
            .key("member-nickname-input")
            .w_full(),
    );
    if !m.claimed && pending.is_none() {
        editor = editor.child(
            label(
                "You haven't chosen a nickname in this room yet, so others see you as a number.",
                st::SMALL,
                p.text_muted,
            )
            .wrap()
            .w_full()
            .key("member-nickname-unset"),
        );
    }
    if let Some(problem) = problem {
        editor = editor.child(
            label(problem, st::SMALL, p.danger)
                .wrap()
                .w_full()
                .key("member-nickname-error"),
        );
    }
    let block = room.send_block.as_ref().or(room.nickname_block.as_ref());
    if let Some(block) = block {
        editor = editor.child(
            label(block.clone(), st::SMALL, p.text_muted)
                .wrap()
                .w_full()
                .key("member-nickname-blocked"),
        );
    }
    if let Some(n) = pending {
        let mut status = row()
            .key("member-nickname-status")
            .w_full()
            .gap(8.0)
            .center_y();
        match &n.failed {
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
                if n.retry {
                    status = status.child(
                        Button::new("Retry")
                            .variant(ButtonVariant::Ghost)
                            .on_press(|s: &mut RiverApp| s.chat.retry_nickname())
                            .build(k)
                            .key("nickname-retry"),
                    );
                }
                status = status.child(
                    Button::new("Discard")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut RiverApp| s.chat.discard_nickname())
                        .build(k)
                        .key("nickname-discard"),
                );
            }
        }
        editor = editor.child(status);
    }
    let mut save = Button::new("Save")
        .variant(ButtonVariant::Contrast)
        .on_press(|s: &mut RiverApp| s.chat.submit_nickname())
        .build(k)
        .key("nickname-save");
    if sending || block.is_some() {
        save = save.disabled(true).opacity(0.45);
    }
    editor.child(
        row()
            .w_full()
            .gap(8.0)
            .justify_end()
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.chat.close_member())
                    .build(k)
                    .key("nickname-cancel"),
            )
            .child(save),
    )
}

/// "Deputized by:" and each appointer as its own button that opens their dialog, as the web UI
/// lists them: the owner and you as the fixed labels "the room owner" and "you", any other
/// member by nickname (or "an unknown member" when they are no longer listed). Each is its own
/// element, never one joined string, so a name cannot pass as two appointers.
fn deputized_by_row(
    room: &Room,
    m: &Member,
    by_id: &HashMap<MemberId, &Member>,
    p: &Palette,
) -> Option<El> {
    if m.deputized_by.is_empty() {
        return None;
    }
    let mut names = row()
        .key("member-deputized-by")
        .w_full()
        .gap(6.0)
        .flex_wrap()
        .center_y()
        .child(label("Deputized by:", st::SMALL, p.text_muted));
    for (n, a) in m.deputized_by.iter().enumerate() {
        let target = match a {
            Appointer::Owner => by_id.values().find(|o| o.role == Role::Owner).copied(),
            Appointer::You => by_id.get(&room.me).copied(),
            Appointer::Member(id) => by_id.get(id).copied(),
        };
        let text = match (a, target) {
            (Appointer::Owner, _) => "the room owner".to_owned(),
            (Appointer::You, _) => "you".to_owned(),
            (_, Some(t)) => t.nickname.clone(),
            (_, None) => "an unknown member".to_owned(),
        };
        let mut chip = row()
            .key(format!("member-appointer-{n}"))
            .h(22.0)
            .px(8.0)
            .radius(6.0)
            .center()
            .fill(p.surface)
            .border(1.0, p.line)
            .child(label(text.clone(), st::SMALL, p.accent).truncate().shrink());
        if let Some(t) = target {
            let uid = t.full_id;
            chip = chip
                .role("button")
                .label(format!("Member info: {text}"))
                .focusable(true)
                .cursor(rune::Cursor::Pointer)
                .on_press(move |s: &mut RiverApp| s.chat.open_member_full(uid));
        }
        names = names.child(chip);
    }
    Some(names)
}

/// What the dialog offers about making a member your deputy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeputyOffer {
    Deputize,
    Revoke,
}

/// The web UI's `deputy_button_visible`, for `m` as `room` shows them. Nothing for yourself or the
/// owner (who already has full authority). **Revoke** for one of your deputies, always, even when
/// you no longer have anyone to moderate: otherwise the appointer could never take it back after
/// a deputy removed their last invitee. **Deputize** only when you hold authority, as the owner or
/// with someone you invited in the room, so a member who can ban nobody is not offered a power
/// they do not have.
pub(super) fn deputy_offer(room: &Room, m: &Member) -> Option<DeputyOffer> {
    if m.id == room.me || m.role == Role::Owner {
        return None;
    }
    if m.my_deputy {
        return Some(DeputyOffer::Revoke);
    }
    let by_id: HashMap<MemberId, &Member> = room.members.iter().rev().map(|m| (m.id, m)).collect();
    let me = by_id.get(&room.me).copied()?;
    let authority = me.role == Role::Owner
        || room
            .members
            .iter()
            .any(|x| x.id != me.id && invited_through(&by_id, x, me));
    authority.then_some(DeputyOffer::Deputize)
}

/// The deputy action of the dialog: its button and what it does (the web UI's own words), a
/// confirmation in its place once asked for, and the state of a change on its way or failed.
fn deputy_block(
    chat: &Chat,
    room: &Room,
    m: &Member,
    offer: DeputyOffer,
    why_not: Option<String>,
    p: &Palette,
    k: &Kit,
) -> El {
    let grant = offer == DeputyOffer::Deputize;
    let name = m.nickname.clone();
    let words = if grant {
        format!("Let {name} help moderate people you invited.")
    } else {
        format!("{name} can currently help moderate people you invited.")
    };
    let mut block = col().key("member-deputy").w_full().gap(8.0);
    if chat.deputy_confirm.as_ref() == Some(&(m.uid.clone(), grant)) {
        let title = if grant {
            "Deputize this member?"
        } else {
            "Revoke this deputy?"
        };
        return notice(
            NoticeKind::Warning,
            "member-deputy",
            vec![
                label(title, st::ROOM_STRONG, p.text).key("member-deputy-title"),
                label(words, st::SMALL, p.text)
                    .wrap_anywhere()
                    .w_full()
                    .key("member-deputy-words"),
                dk::footer(
                    Vec::new(),
                    vec![
                        dk::quiet_button(
                            k,
                            "Cancel",
                            "member-deputy-cancel",
                            |s: &mut RiverApp| s.chat.cancel_deputy(),
                        ),
                        dk::primary_button(
                            k,
                            if grant { "Deputize" } else { "Revoke deputy" },
                            "member-deputy-confirm",
                            |s: &mut RiverApp| s.chat.confirm_deputy(),
                        ),
                    ],
                ),
            ],
        );
    }
    let mut button = Button::new(if grant { "Deputize" } else { "Revoke deputy" })
        .variant(ButtonVariant::Ghost)
        .on_press(|s: &mut RiverApp| s.chat.ask_deputy())
        .build(k)
        .key(if grant {
            "member-deputize"
        } else {
            "member-revoke-deputy"
        });
    if why_not.is_some() {
        button = button.disabled(true).opacity(0.45);
    }
    block = block.child(row().child(button)).child(
        label(words, st::SMALL, p.text_muted)
            .wrap_anywhere()
            .w_full()
            .key("member-deputy-words"),
    );
    let change = room.deputy_changes.iter().find(|c| c.uid == m.uid);
    match change {
        // On its way: the button says so.
        Some(c) if c.pending.failed.is_none() => {
            block = block.child(
                label("Sending\u{2026}", st::SMALL, p.text_muted).key("member-deputy-status"),
            );
        }
        Some(c) => {
            let mut status = row()
                .key("member-deputy-status")
                .w_full()
                .gap(8.0)
                .center_y()
                .child(
                    label(
                        c.pending.failed.clone().unwrap_or_default(),
                        st::SMALL,
                        p.danger,
                    )
                    .wrap()
                    .shrink()
                    .min_w(0.0),
                );
            if c.pending.retry {
                status = status.child(
                    Button::new("Retry")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut RiverApp| s.chat.retry_deputy())
                        .build(k)
                        .key("deputy-retry"),
                );
            }
            status = status.child(
                Button::new("Discard")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.chat.discard_deputy())
                    .build(k)
                    .key("deputy-discard"),
            );
            block = block.child(status);
        }
        None => {}
    }
    // Why it can't be used now, said under it (a change on its way says "Sending" above).
    if change.is_none()
        && let Some(why) = why_not
    {
        block = block.child(
            label(why, st::SMALL, p.text_muted)
                .wrap()
                .w_full()
                .key("member-deputy-blocked"),
        );
    }
    block
}

/// The web UI's confirmation, in place of its popup: the question in its words, what else the ban
/// takes with it (a ban removes everyone the banned member invited), and in a private room that
/// the room's secret changes.
fn ban_confirmation(
    chat: &Chat,
    room: &Room,
    m: &Member,
    by_id: &HashMap<MemberId, &Member>,
    p: &Palette,
    k: &Kit,
) -> El {
    let invitees = room
        .members
        .iter()
        .filter(|x| x.id != m.id && invited_through(by_id, x, m))
        .count();
    let mut block = col()
        .w_full()
        .gap(8.0)
        .child(label("Confirm ban", st::ROOM_STRONG, p.text).key("member-ban-title"))
        .child(
            label(
                format!(
                    "Are you sure you want to ban {} (ID: {})? This action cannot be undone.",
                    m.nickname, m.uid
                ),
                st::SMALL,
                p.text,
            )
            .wrap_anywhere()
            .w_full()
            .key("member-ban-words"),
        );
    if invitees > 0 {
        let who = if invitees == 1 {
            "1 member they invited".to_owned()
        } else {
            format!("{invitees} members they invited")
        };
        block = block.child(
            label(
                format!("The ban also removes {who}, and anyone those members invited."),
                st::SMALL,
                p.text,
            )
            .wrap()
            .w_full()
            .key("member-ban-reach"),
        );
    }
    let preview = chat.ban_preview();
    if let Some(preview) = preview {
        let said = if preview.refused {
            Some((
                crate::rooms::bans::REFUSED_SENTENCE.to_owned(),
                "member-ban-refused",
            ))
        } else if !preview.lifts.is_empty() {
            Some((
                crate::rooms::bans::lifted_sentence(
                    &preview.lifts,
                    room.details.limits.max_user_bans,
                ),
                "member-ban-lifts",
            ))
        } else {
            None
        };
        if let Some((text, key)) = said {
            block = block.child(label(text, st::SMALL, p.text).wrap().w_full().key(key));
        }
    } else if !matches!(chat.source, Source::Example) {
        block = block.child(
            label(
                "Checking what this ban does\u{2026}",
                st::SMALL,
                p.text_muted,
            )
            .w_full()
            .key("member-ban-checking"),
        );
    }
    if chat.ban_changed.is_some() {
        block = block.child(
            label(
                "This changed while you were looking. Check it, then confirm again.",
                st::SMALL,
                p.warning_text,
            )
            .wrap()
            .w_full()
            .key("member-ban-changed"),
        );
    }
    if room.private {
        block = block.child(
            label(
                "This is a private room, so the ban also changes the room's secret. Messages \
                 sent from now on are sealed so the banned member cannot read them.",
                st::SMALL,
                p.text,
            )
            .wrap()
            .w_full()
            .key("member-ban-secret"),
        );
    }
    let mut yes = dk::danger_button(
        k,
        "Yes, ban user",
        "member-ban-confirm-yes",
        |s: &mut RiverApp| s.chat.confirm_ban(),
    );
    // Not until the room has said what the ban does, and never one it would drop.
    if !chat.ban_ready() {
        yes = yes.disabled(true).opacity(0.45);
    }
    notice(
        NoticeKind::Danger,
        "member-ban-confirm",
        vec![
            block,
            dk::footer(
                Vec::new(),
                vec![
                    dk::quiet_button(k, "Cancel", "member-ban-cancel", |s: &mut RiverApp| {
                        s.chat.cancel_ban()
                    }),
                    yes,
                ],
            ),
        ],
    )
}

pub(super) fn member_modal(chat: &Chat, room: &Room, uid: &str, k: &Kit, layer: &mut Vec<El>) {
    let p = st::palette();
    // The member the dialog was opened for, by full id; by label only where nothing said.
    let Some(m) = chat
        .member_modal_full
        .and_then(|f| room.member_by_full_id(f))
        .or_else(|| room.member_by_uid(uid))
    else {
        return;
    };
    // Another member has this member's short label. Moderation here is looked up by label (not
    // part of this change), so it is not offered for either of them; Message uses the full id.
    let shared = room.label_shared(m);
    let by_id: HashMap<MemberId, &Member> = room.members.iter().rev().map(|m| (m.id, m)).collect();
    let is_me = m.id == room.me;
    let is_owner = m.role == Role::Owner;
    let me = by_id.get(&room.me).copied();
    // Whether the viewer is above `m` in the invite tree. A member who left has no known
    // inviter and is above nobody's reach, so is never one the viewer invited.
    let above = me
        .filter(|_| !is_me && m.invited_by.is_some())
        .is_some_and(|me| me.role == Role::Owner || invited_through(&by_id, m, me));
    let imitated = imitation(room, m);

    let mut tags = row().key("member-tags").w_full().gap(8.0).flex_wrap();
    if is_owner {
        tags = tags.child(chip(
            "member-tag-owner",
            "Room Owner",
            p.text,
            p.line_strong,
        ));
    }
    if let Some(tooltip) = m.deputy_tooltip() {
        // The web UI's `member-info-deputy-tag`: the shield and the word, with who appointed
        // them in the tooltip (the names are listed below).
        tags = tags.child(
            row()
                .key("member-tag-deputy")
                .h(22.0)
                .px(8.0)
                .gap(4.0)
                .radius(11.0)
                .center()
                .border(1.0, p.line_strong)
                .label(tooltip.clone())
                .help(tooltip)
                .child(icon(&icons::SHIELD, 12.0).color(p.text))
                .child(label("Deputy".to_owned(), st::SMALL_STRONG, p.text)),
        );
    }
    if is_me {
        tags = tags.child(chip("member-tag-you", "You", p.text, p.line_strong));
    }
    if above {
        tags = tags.child(chip(
            "member-tag-invited-by-you",
            "Invited by You",
            p.text,
            p.line_strong,
        ));
    }
    if me.is_some_and(|me| !is_me && me.invited_by == Some(m.id)) {
        tags = tags.child(chip(
            "member-tag-invited-you",
            "Invited You",
            p.text,
            p.line_strong,
        ));
    }
    if let Some(i) = &imitated {
        tags = tags.child(chip(
            "member-tag-impersonation",
            &format!("\u{26a0} {}", i.label()),
            p.warning_text,
            p.warning,
        ));
    }

    let mut body = col().key("member-info").w_full().gap(12.0).child(
        row()
            .w_full()
            .gap(12.0)
            .center_y()
            .child(avatar(Some(m), m.id, 40.0))
            .child(
                label(m.nickname.clone(), st::TITLE, p.text)
                    .wrap_anywhere()
                    .grow(1.0)
                    .shrink()
                    .min_w(0.0)
                    .key("member-name"),
            ),
    );
    body = body.child(tags);
    if let Some(row) = deputized_by_row(room, m, &by_id, &p) {
        body = body.child(row);
    }
    if is_me {
        body = body.child(nickname_editor(room, m, chat, &p, k));
    }
    if let Some(Imitation { who: owner, text }) = imitated {
        body = body.child(notice(
            NoticeKind::Warning,
            "member-impersonation",
            vec![
                label(text, st::SMALL, p.text).wrap_anywhere().w_full(),
                row()
                    .w_full()
                    .gap(6.0)
                    .center_y()
                    .child(label("Resembles:", st::SMALL, p.text_muted))
                    .child(
                        label(owner.nickname.clone(), st::SMALL_STRONG, p.text)
                            .truncate()
                            .shrink()
                            .min_w(0.0)
                            .key("member-resembles"),
                    ),
            ],
        ));
    }
    body = body.child(copy_field(
        "member-id",
        "Member ID",
        None,
        &m.uid,
        "Copy member ID",
        st::BODY,
        &p,
    ));
    if !m.key.is_empty() {
        body = body.child(copy_field(
            "member-vk",
            "Verifying key",
            None,
            &m.key,
            "Copy verifying key",
            KEY_TEXT,
            &p,
        ));
    }
    let mut invited_by = row().key("member-invited-by").w_full().center_y();
    invited_by = if is_owner {
        invited_by.child(label("N/A (Room Owner)", st::BODY, p.text))
    } else if let Some(i) = m.invited_by.and_then(|i| by_id.get(&i)) {
        // As in the web UI, the inviter's name opens the inviter's own dialog.
        let uid = i.full_id;
        invited_by.child(
            row()
                .key("member-inviter")
                .role("button")
                .label(format!("Member info: {}", i.nickname))
                .focusable(true)
                .cursor(rune::Cursor::Pointer)
                .shrink()
                .min_w(0.0)
                .on_press(move |s: &mut RiverApp| s.chat.open_member_full(uid))
                .child(
                    label(i.nickname.clone(), st::BODY, p.accent)
                        .wrap_anywhere()
                        .shrink()
                        .min_w(0.0),
                ),
        )
    } else {
        invited_by.child(label("Unknown", st::BODY, p.text))
    };
    body = body.child(
        col()
            .w_full()
            .gap(4.0)
            .child(label("Invited by", st::SMALL_STRONG, p.text_muted))
            .child(invited_by),
    );

    let mut actions = row().key("member-actions").w_full().gap(8.0).flex_wrap();
    if !is_me {
        // Opens the conversation, which can be read whatever the room lets you send; the
        // composer says why it cannot send. The thread is named by the member's full id, so
        // the right member gets it even where another shares their short label.
        let (room_key, peer) = (room.key.clone(), m.full_id.to_string());
        let (invite_room, invite_peer) = (room_key.clone(), peer.clone());
        let message = Button::new("Message")
            .variant(ButtonVariant::Ghost)
            .on_press(move |s: &mut RiverApp| {
                s.chat.open_dm(&room_key, &peer);
            })
            .build(k)
            .key("member-dm");
        actions = actions.child(message);
        // Sends them an invitation to one of your other rooms, in the conversation.
        actions = actions.child(
            Button::new("Share invite")
                .variant(ButtonVariant::Ghost)
                .on_press(move |s: &mut RiverApp| {
                    s.chat.open_invite_picker(&invite_room, &invite_peer);
                })
                .build(k)
                .key("member-dm-invite"),
        );
        if !shared
            && matches!(room.ban_offers.get(uid), Some(BanOffer::Allowed))
            && chat.ban_confirm.is_none()
        {
            actions = actions.child(dk::danger_outline_button(
                k,
                "Ban user",
                "member-ban",
                |s: &mut RiverApp| s.chat.ask_ban(),
            ));
        }
    }
    body = body.child(el().w_full().h(1.0).fill(p.line)).child(actions);
    if !is_me && shared {
        body = body.child(
            label(
                "Another member has the same short name (ID). Check the key before you trust \
                 either of them. Ban and deputy actions are not offered here while two members \
                 share it.",
                st::SMALL,
                p.text_muted,
            )
            .wrap()
            .w_full()
            .key("member-dm-ambiguous"),
        );
    }
    if !is_me && !shared {
        match room.ban_offers.get(uid) {
            // As the web UI's `ban-withheld-reason`: owed to one who would otherwise have had Ban.
            Some(BanOffer::Withheld(why)) => {
                body = body.child(
                    label(why.clone(), st::SMALL, p.text_muted)
                        .wrap()
                        .w_full()
                        .key("member-ban-withheld"),
                );
            }
            Some(BanOffer::Allowed) if chat.ban_confirm.as_deref() == Some(uid) => {
                body = body.child(ban_confirmation(chat, room, m, &by_id, &p, k));
            }
            _ => {}
        }
    }
    if let Some((offer, why_not)) = chat.deputy_offer().filter(|_| !is_me && !shared) {
        body = body.child(deputy_block(chat, room, m, offer, why_not, &p, k));
    }

    DialogSpec::new("member-info", "Member info", |s: &mut RiverApp| {
        s.chat.close_member()
    })
    .body_key("member-info-body")
    .width(440.0)
    .child(body)
    .build(k, layer);
}
