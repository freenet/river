//! A failed composer send, shown while the open room shows a notice instead
//! of the composer: its private key not here yet, self banned or not a
//! member, its identity not on this device, or the room syncing.
//!
//! A failed send stays in the outbox (`outbox.rs`) until the composer takes
//! it back as its draft, and `MessageInput` is what takes it back. With a
//! notice up, `MessageInput` is not mounted, so the send would be shown
//! nowhere while still holding back a reload (`Outbox::has_unsent`). This
//! shows it there instead, with Discard. When the composer returns, it takes
//! the send back as it always does (`message_input.rs`), and this unmounts
//! with the notice: nothing here restores a draft.
//!
//! Only the identity that wrote a send sees it. With no identity for the room
//! on this device, who wrote it cannot be told, so only its presence is
//! shown, with how to recover it: never its text or its quote. Such a row
//! does not hold a reload either (`Outbox::has_unsent` counts only a row its
//! room's current identity wrote), so there is nothing to discard for that.
//!
//! Its own component, so an outbox change renders this and not the
//! conversation around it.

use super::outbox::{Outbox, PendingRow, OUTBOX};
use super::outbox_commit;
use super::value_for_room;
use crate::components::icons::{line_icon, REPLY_TARGET_ICON_PATH};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;

/// What the notice shows of the room's failed send.
#[derive(Clone, PartialEq, Debug)]
enum Shown {
    /// The author's own failed send: its text, reply, reason and Discard.
    Send(PendingRow),
    /// A failed send, while who self is in the room is unknown: that it
    /// waits, and nothing of it.
    Unattributed,
}

/// The room's failed send as the notice shows it, for `author`, or for an
/// unknown identity with `None`.
fn shown(outbox: &Outbox, room: &VerifyingKey, author: Option<VerifyingKey>) -> Option<Shown> {
    match author {
        Some(author) => outbox
            .composer_send(room, &author)
            .filter(|row| row.failed.is_some())
            .map(Shown::Send),
        None => outbox.has_failed_send(room).then_some(Shown::Unattributed),
    }
}

/// Mounted beside each notice that replaces the composer (`Conversation`).
#[component]
pub fn FailedSendNotice(
    /// The open room.
    room: VerifyingKey,
    /// The identity it is written as, if this device knows it.
    author: Option<VerifyingKey>,
) -> Element {
    // The outbox as the last uncontended read saw it, for this room and
    // identity only (freenet/river#555), as `MessageInput` keeps it.
    let mut last_good =
        use_hook(|| CopyValue::new(None::<((VerifyingKey, Option<VerifyingKey>), Option<Shown>)>));
    crate::util::signal_guard::anchor();
    let shown = match OUTBOX.try_read() {
        Ok(outbox) => {
            let shown = shown(&outbox, &room, author);
            last_good.set(Some(((room, author), shown.clone())));
            shown
        }
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            value_for_room(&last_good.peek(), &(room, author)).flatten()
        }
    };

    let panel = "mx-4 mb-2 px-4 py-3 bg-surface border border-border rounded-lg text-sm";
    match (shown, author) {
        (Some(Shown::Send(row)), Some(author)) => {
            let id = row.id;
            let reason = row.failed.clone().unwrap_or_default();
            rsx! {
                div {
                    "data-testid": "failed-send-notice",
                    role: "group",
                    "aria-label": "Message not sent",
                    class: "{panel}",
                    if let Some(reply) = row.reply.as_ref() {
                        div {
                            "data-testid": "failed-send-reply",
                            class: "flex items-center gap-2 mb-1.5 text-[13px] text-text-quote",
                            span { "aria-hidden": "true", class: "flex-shrink-0 inline-flex", {line_icon(REPLY_TARGET_ICON_PATH, 14)} }
                            span { class: "flex-1 truncate",
                                span { class: "font-bold text-text-secondary", "Replying to {reply.author_name} " }
                                "{reply.content_preview}"
                            }
                        }
                    }
                    p {
                        "data-testid": "failed-send-text",
                        class: "max-h-40 overflow-y-auto whitespace-pre-wrap [overflow-wrap:anywhere] select-text text-text",
                        "{row.text}"
                    }
                    div { class: "mt-2 flex items-start gap-3",
                        p {
                            "data-testid": "failed-send-reason",
                            role: "alert",
                            class: "flex-1 text-xs text-danger [overflow-wrap:anywhere]",
                            "Not sent: {reason}. It is kept here until you discard it, or until you can send in this room again."
                        }
                        button {
                            r#type: "button",
                            "data-testid": "failed-send-discard-button",
                            class: "flex-shrink-0 px-3 py-1 rounded-lg border border-border text-xs text-text hover:bg-surface-hover transition-colors",
                            // Checked again when it runs: a second click, or
                            // a send the composer took back meanwhile, does
                            // nothing.
                            onclick: move |_| outbox_commit::discard_failed_send(room, author, id),
                            "Discard"
                        }
                    }
                }
            }
        }
        (Some(Shown::Unattributed), None) => rsx! {
            div {
                "data-testid": "failed-send-notice",
                role: "status",
                class: "{panel}",
                p {
                    "data-testid": "failed-send-identity-note",
                    class: "text-text",
                    "A message written here could not be sent. Restore this room's original identity on this device and it comes back to the composer, to send again. Until then it is kept only until the page reloads."
                }
            }
        },
        _ => rsx! {},
    }
}

#[cfg(test)]
mod tests {
    use super::super::outbox::{PendingKind, PendingState};
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::member::MemberId;
    use river_core::room_state::message::{MessageV1, RoomMessageBody};

    fn key(b: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[b; 32]).verifying_key()
    }

    fn failed_send(
        outbox: &mut Outbox,
        room: VerifyingKey,
        author: VerifyingKey,
        text: &str,
    ) -> u64 {
        let message = MessageV1 {
            room_owner: MemberId::from(&room),
            author: MemberId::from(&author),
            content: RoomMessageBody::public(text.to_string()),
            time: std::time::SystemTime::UNIX_EPOCH,
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&message, &mut bytes).unwrap();
        let (id, _) = outbox.enqueue(
            room,
            PendingKind::Send {
                text: text.to_string(),
                reply: None,
            },
            author,
            vec![(message, bytes)],
        );
        outbox.record_failure(&room, id, "signing failed".to_string());
        id
    }

    #[test]
    fn the_author_sees_their_failed_send_and_nobody_else_sees_its_text() {
        let (room, author, other) = (key(1), key(2), key(3));
        let mut outbox = Outbox::default();
        assert_eq!(shown(&outbox, &room, Some(author)), None);
        assert_eq!(shown(&outbox, &room, None), None);

        let id = failed_send(&mut outbox, room, author, "kept");
        match shown(&outbox, &room, Some(author)) {
            Some(Shown::Send(row)) => {
                assert_eq!((row.id, row.text.as_str()), (id, "kept"));
                assert_eq!(row.failed.as_deref(), Some("signing failed"));
            }
            other => panic!("the author's failed send, got {other:?}"),
        }
        assert_eq!(
            shown(&outbox, &room, Some(other)),
            None,
            "another identity sees nothing of it"
        );
        assert_eq!(
            shown(&outbox, &room, None),
            Some(Shown::Unattributed),
            "an unknown identity learns only that it waits"
        );
        assert_eq!(shown(&outbox, &key(9), None), None, "another room has none");
    }

    #[test]
    fn a_send_still_being_signed_is_not_shown() {
        let (room, author) = (key(1), key(2));
        let mut outbox = Outbox::default();
        let id = failed_send(&mut outbox, room, author, "older, failed");
        let message = MessageV1 {
            room_owner: MemberId::from(&room),
            author: MemberId::from(&author),
            content: RoomMessageBody::public("signing".to_string()),
            time: std::time::SystemTime::UNIX_EPOCH,
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&message, &mut bytes).unwrap();
        let (newer, _) = outbox.enqueue(
            room,
            PendingKind::Send {
                text: "signing".to_string(),
                reply: None,
            },
            author,
            vec![(message, bytes)],
        );
        assert_eq!(
            outbox.entry(&room, newer).unwrap().state,
            PendingState::Signing
        );
        assert_eq!(
            shown(&outbox, &room, Some(author)),
            None,
            "the composer's newest send is being signed, as `MessageInput` reads it"
        );
        outbox.discard(&room, id);
        assert_eq!(shown(&outbox, &room, Some(author)), None);
        assert_eq!(shown(&outbox, &room, None), None, "nothing failed waits");
    }

    /// The notice reads the outbox under the signal guard, keeps its own
    /// last good value keyed by room and identity, and discards through the
    /// deferred, rechecking adapter, never by writing the outbox itself.
    #[test]
    fn the_read_is_guarded_and_discard_is_deferred() {
        let source = include_str!("failed_send_notice.rs");
        let prod = &source[..source.find("#[cfg(test)]\nmod tests").unwrap()];
        let anchor = prod
            .find("signal_guard::anchor()")
            .expect("anchors before reading");
        let read = prod.find("OUTBOX.try_read()").expect("a fallible read");
        assert!(anchor < read, "the anchor comes first");
        assert!(prod.contains("signal_guard::schedule_nudge()"));
        assert!(prod.contains("value_for_room(&last_good.peek(), &(room, author))"));
        assert!(prod.contains("outbox_commit::discard_failed_send(room, author, id)"));
        for write in ["OUTBOX.write(", "OUTBOX.with_mut(", ".discard("] {
            assert!(!prod.contains(write), "`{write}` outside the adapter");
        }
    }
}
