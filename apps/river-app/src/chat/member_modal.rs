//! The member info dialog, which opens from a row of the members panel. It follows the web UI's
//! `MemberInfoModal` (`ui/src/components/members/member_info_modal.rs`): who the member is, how
//! they came to be in the room, and a warning when their name imitates the owner's.
//!
//! What it leaves out is what this app cannot do or know yet: sending a direct message, sharing
//! an invite by one, mentioning (the composer has no mention tokens), banning, deputizing, and
//! editing your own nickname (all moderation and profile commands), and deputy badges (the
//! view model carries no deputy grants, so only the owner's name is protected from imitation).
//! The first four are shown dimmed with the app's "not available yet" tooltip, not faked.

use rune::{El, col, el, row};
use rune_kit::{Button, ButtonVariant, Dialog, Kit};

use super::conversation::avatar;
use super::model::{Member, Role, Room};
use super::style::{self as st, Palette};
use super::view::{label, not_yet};
use crate::RiverApp;

/// The web UI's `ImpersonationWarning::tooltip` for an imitated owner. The text before the
/// first colon is the chip's label.
pub(super) const IMPERSONATION: &str = "Impersonation warning: this member is NOT the room \
     owner, but their name closely resembles the owner's. The real owner has a crown icon in the \
     member list and an OWNER tag on their messages.";

/// The owner `m`'s name resembles without being the owner, if it does.
///
/// Only the owner's name is protected: deputies are not in the view model. A member is never
/// accused of imitating a name that is theirs.
pub(super) fn imitated_owner<'a>(room: &'a Room, m: &Member) -> Option<&'a Member> {
    let owner = room.members.iter().find(|o| o.role == Role::Owner)?;
    (owner.id != m.id && super::visual_fold::names_fold_together(&m.nickname, &owner.nickname))
        .then_some(owner)
}

/// Whether `m` was invited, directly or not, by `by`. The walk is bounded, so a cycle in
/// hostile data ends.
fn invited_through(room: &Room, m: &Member, by: &Member) -> bool {
    let mut at = m;
    for _ in 0..=room.members.len() {
        let Some(inviter) = at.invited_by.and_then(|i| room.member(i)) else {
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

fn field(key: &str, name: &str, value: &str, copy_what: &'static str, p: &Palette, k: &Kit) -> El {
    let copied = value.to_owned();
    col()
        .key(key.to_owned())
        .w_full()
        .gap(2.0)
        .child(
            row()
                .w_full()
                .center_y()
                .child(label(name.to_owned(), st::SMALL_STRONG, p.text_muted))
                .child(rune::spacer())
                .child(
                    Button::new("Copy")
                        .variant(ButtonVariant::Ghost)
                        .on_press_tx(move |_: &mut RiverApp, tx: &mut rune::Tx| {
                            tx.copy(copied.clone())
                        })
                        .build(k)
                        .label(copy_what)
                        .key(format!("{key}-copy")),
                ),
        )
        .child(
            label(value.to_owned(), st::BODY, p.text)
                .wrap()
                .w_full()
                .key(format!("{key}-value")),
        )
}

pub(super) fn member_modal(room: &Room, uid: &str, k: &Kit, layer: &mut Vec<El>) {
    let p = st::palette();
    let Some(m) = room.member_by_uid(uid) else {
        return;
    };
    let is_me = m.id == room.me;
    let is_owner = m.role == Role::Owner;
    let me = room.member(room.me);
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
    if let Some(me) = me.filter(|_| !is_me) {
        if me.role == Role::Owner || invited_through(room, m, me) {
            tags = tags.child(chip(
                "member-tag-invited-by-you",
                "Invited by You",
                p.text,
                p.line_strong,
            ));
        }
        if me.invited_by == Some(m.id) {
            tags = tags.child(chip(
                "member-tag-invited-you",
                "Invited You",
                p.text,
                p.line_strong,
            ));
        }
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
                .gap(12.0)
                .center_y()
                .child(avatar(Some(m), m.id, 40.0))
                .child(
                    label(m.nickname.clone(), st::TITLE, p.text)
                        .wrap()
                        .grow(1.0)
                        .min_w(0.0)
                        .key("member-name"),
                ),
        );
    body = body.child(tags);
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
                .child(label(IMPERSONATION, st::SMALL, p.text).wrap().w_full())
                .child(
                    row()
                        .gap(6.0)
                        .center_y()
                        .child(label("Resembles:", st::SMALL, p.text_muted))
                        .child(
                            label(owner.nickname.clone(), st::SMALL_STRONG, p.text)
                                .wrap()
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
        &p,
        k,
    ));
    if !m.key.is_empty() {
        body = body.child(field(
            "member-vk",
            "Verifying key",
            &m.key,
            "Copy verifying key",
            &p,
            k,
        ));
    }
    let mut invited_by = row().key("member-invited-by").w_full().center_y();
    invited_by = if is_owner {
        invited_by.child(label("N/A (Room Owner)", st::BODY, p.text))
    } else if let Some(i) = m.invited_by.and_then(|i| room.member(i)) {
        // As in the web UI, the inviter's name opens the inviter's own dialog.
        let uid = i.uid.clone();
        invited_by.child(
            Button::new(i.nickname.clone())
                .variant(ButtonVariant::Ghost)
                .on_press(move |s: &mut RiverApp| s.chat.open_member(&uid))
                .build(k)
                .key("member-inviter"),
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
        let mut names = vec![("member-dm", "Message"), ("member-mention", "Mention")];
        let moderates = me.is_some_and(|me| me.role == Role::Owner || invited_through(room, m, me));
        if moderates && !is_owner {
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
