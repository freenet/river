//! The member info dialog, which opens from a row of the members panel. It follows the web UI's
//! `MemberInfoModal` (`ui/src/components/members/member_info_modal.rs`): who the member is, how
//! they came to be in the room, and a warning when their name imitates the owner's.
//!
//! What it leaves out is what this app cannot do or know yet: sending a direct message, sharing
//! an invite by one, banning and deputizing (moderation commands). Those are shown dimmed with
//! the app's "not available yet" tooltip, not faked. Your own dialog edits your nickname
//! (`nickname_editor`), as the web UI's `NicknameField` does.
//!
//! PARITY-GAPS: deputies. The view model carries no deputy grants, so there is no Deputy tag or
//! "Deputized by" line, and only the owner's name is protected from imitation (the web UI also
//! protects every badged deputy's name). Closing it means carrying the viewer's deputy badges
//! (`deputy_badges_for_viewer` in `ui/src/components/members.rs`) into `Member`.

use std::collections::HashMap;

use rune::{El, col, el, icon, row};
use rune_kit::{Button, ButtonVariant, Dialog, Kit, TextInput};

use super::Chat;
use super::conversation::avatar;
use super::icons;
use super::model::{Member, MemberId, Role, Room};
use super::style::{self as st, Palette};
use super::view::{label, not_yet};
use crate::RiverApp;

/// The web UI's `ImpersonationWarning::tooltip` for an imitated owner. The text before the
/// first colon is the chip's label.
pub(super) const IMPERSONATION: &str = "Impersonation warning: this member is NOT the room \
     owner, but their name closely resembles the owner's. The real owner has a crown icon in the \
     member list and an OWNER tag on their messages.";

/// The web UI's `UNNAMED` and `UNKNOWN_MEMBER` (`ui/src/util/display_name.rs`), which it never
/// protects.
const PLACEHOLDERS: [&str; 2] = ["Unnamed", "Unknown"];

/// The owner `m`'s name resembles without being the owner, if it does.
///
/// The owner's name is protected as the web UI's `impersonation_checker_for_viewer` does: not
/// when they have not chosen one this computer can read, nor when it folds to a placeholder or
/// to a generated handle (each shared by a whole class of members). A member is never accused of
/// imitating a name that is theirs.
pub(super) fn imitated_owner<'a>(room: &'a Room, m: &Member) -> Option<&'a Member> {
    use super::visual_fold::{names_fold_to_generated_handle, names_fold_together};
    let owner = room.members.iter().find(|o| o.role == Role::Owner)?;
    let protected = owner.claimed
        && !PLACEHOLDERS
            .iter()
            .any(|p| names_fold_together(&owner.nickname, p))
        && !names_fold_to_generated_handle(&owner.nickname);
    (protected && owner.id != m.id && names_fold_together(&m.nickname, &owner.nickname))
        .then_some(owner)
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

pub(super) fn field(
    key: &str,
    name: &str,
    value: &str,
    copy_what: &'static str,
    text: rune::tok::TextStyle,
    p: &Palette,
) -> El {
    let copied = value.to_owned();
    col()
        .key(key.to_owned())
        .w_full()
        .gap(2.0)
        .child(label(name.to_owned(), st::SMALL_STRONG, p.text_muted))
        .child(
            row()
                .w_full()
                .gap(6.0)
                .child(
                    label(value.to_owned(), text, p.text)
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
                        .on_press_tx(move |_: &mut RiverApp, tx: &mut rune::Tx| {
                            tx.copy(copied.clone())
                        })
                        .child(icon(&icons::COPY, 14.0).color(p.text_muted)),
                ),
        )
}

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
            .on_key(|s: &mut RiverApp, ev, _| super::keys::word_keys(&mut s.chat.nickname, ev))
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

pub(super) fn member_modal(chat: &Chat, room: &Room, uid: &str, k: &Kit, layer: &mut Vec<El>) {
    let p = st::palette();
    let Some(m) = room.member_by_uid(uid) else {
        return;
    };
    let by_id: HashMap<MemberId, &Member> = room.members.iter().rev().map(|m| (m.id, m)).collect();
    let is_me = m.id == room.me;
    let is_owner = m.role == Role::Owner;
    let me = by_id.get(&room.me).copied();
    // Whether the viewer is above `m` in the invite tree. A member who left has no known
    // inviter and is above nobody's reach, so is never one the viewer invited.
    let above = me
        .filter(|_| !is_me && m.invited_by.is_some())
        .is_some_and(|me| me.role == Role::Owner || invited_through(&by_id, m, me));
    let imitated = imitated_owner(room, m);

    let mut tags = row().key("member-tags").w_full().gap(8.0).flex_wrap();
    if is_owner {
        tags = tags.child(chip(
            "member-tag-owner",
            "Room Owner",
            p.text,
            p.line_strong,
        ));
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
    if imitated.is_some() {
        let short = IMPERSONATION.split(':').next().unwrap_or_default();
        tags = tags.child(chip(
            "member-tag-impersonation",
            &format!("\u{26a0} {short}"),
            p.warning_text,
            p.warning,
        ));
    }

    let mut body = col()
        .key("member-info")
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(12.0)
        .child(
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
    if is_me {
        body = body.child(nickname_editor(room, m, chat, &p, k));
    }
    if let Some(owner) = imitated {
        body = body.child(
            col()
                .key("member-impersonation")
                .w_full()
                .padding(12.0)
                .gap(6.0)
                .radius(8.0)
                .fill(p.warning_bg)
                .border(1.0, p.warning)
                .child(
                    label(IMPERSONATION, st::SMALL, p.text)
                        .wrap_anywhere()
                        .w_full(),
                )
                .child(
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
                ),
        );
    }
    body = body.child(field(
        "member-id",
        "Member ID",
        &m.uid,
        "Copy member ID",
        st::BODY,
        &p,
    ));
    if !m.key.is_empty() {
        body = body.child(field(
            "member-vk",
            "Verifying key",
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
        let uid = i.uid.clone();
        invited_by.child(
            row()
                .key("member-inviter")
                .role("button")
                .label(format!("Member info: {}", i.nickname))
                .focusable(true)
                .cursor(rune::Cursor::Pointer)
                .shrink()
                .min_w(0.0)
                .on_press(move |s: &mut RiverApp| s.chat.open_member(&uid))
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

    // What needs commands this app does not send yet is shown, not faked.
    let mut actions = row().key("member-actions").w_full().gap(8.0).flex_wrap();
    if !is_me {
        let mut names = vec![("member-dm", "DM")];
        if above && !is_owner {
            names.extend([("member-ban", "Ban user"), ("member-deputize", "Deputize")]);
        }
        for (key, name) in names {
            actions = actions.child(not_yet(
                Button::new(name)
                    .variant(ButtonVariant::Ghost)
                    .build(k)
                    .key(key),
            ));
        }
    }
    body = body.child(el().w_full().h(1.0).fill(p.line)).child(actions);

    Dialog::new("member-info", "Member Info")
        .width(440.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.chat.close_member())
        .child(body)
        .build(k, layer);
}
