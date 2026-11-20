//! Export ID and Import ID, as the web UI has them (`ExportIdentityModal` and
//! `ImportIdentityModal` in `ui/src/components/members.rs`).
//!
//! **Export ID** is a quiet button at the foot of the members panel. It opens "Export identity"
//! over the open room: the token in a box, the web UI's warning that it holds your private key,
//! Close and "Copy to clipboard" (which reads "Copied!" after). Nothing is copied until the
//! user presses it. The token is made by the background work ([`crate::rooms::identity`]) and
//! lives only in the open dialog: closing it drops it.
//!
//! **Import ID** is a quiet button in the room list's footer. It opens "Import identity": a
//! box to paste into, the web UI's error ("Invalid token: ...") for a token that does not
//! check, and, when the room is already in the list, the web UI's two warnings (importing
//! REPLACES the identity you have in it; close other sessions first) with "Replace identity".
//! The token checked is the one the warnings are about: the box can be edited afterwards, but
//! Replace imports the token the warning was raised for, and editing the box drops the
//! warning (the web UI's freenet/river#414 rule). The import itself is a task in the room list
//! like a join ([`crate::rooms::work`]), opening the room when it is stored.
//!
//! The web UI also holds Import until every room has loaded, so it never takes a room it has
//! not loaded for a new one. Here the decision is made against the stored slot itself, under
//! the storage's compare-and-swap, so it does not depend on the list.

use rune::{El, col};
use rune_kit::{Button, ButtonVariant, Kit, TextArea};

use super::dialog_kit::{self as dk, DialogSpec, NoticeKind, notice_text};
use super::style::{self as st};
use super::view::label;
use super::word_end::{Get, word_end};
use super::{Chat, DialogKind};
use crate::RiverApp;
use crate::rooms::Action;

/// An identity token: the user's signing key. `Debug` shows nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<hidden>)")
    }
}

/// Where an export stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportState {
    /// Asked for; the token is being made.
    Making,
    Ready(Secret),
    /// Why there is no token (the web UI's "Cannot export: ...").
    Failed(String),
}

/// The open "Export identity" dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportDialog {
    /// The room's key.
    pub room: String,
    pub state: ExportState,
    /// Whether Copy was pressed: the button then reads "Copied!".
    pub copied: bool,
}

/// An import waiting for the user to confirm replacing an identity: the token as it was when
/// the warning was raised, never a fresh read of the box.
#[derive(Clone, PartialEq, Eq)]
pub struct ReplaceConfirm {
    pub token: crate::rooms::IdentityToken,
    /// The name of the room you have, as the list shows it.
    pub room_name: Option<String>,
}

impl std::fmt::Debug for ReplaceConfirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplaceConfirm(<hidden>)")
    }
}

/// The web UI's export warning, word for word.
pub const EXPORT_WARNING: &str = "\u{26a0} This token contains your private key. Treat it like a password \u{2014} do not share it publicly.";
/// The web UI's explanation above the token.
pub const EXPORT_INTRO: &str = "Copy this token and import it in another River client (UI or riverctl) to use the same identity.";
/// The web UI's import explanation.
pub const IMPORT_INTRO: &str = "Paste a River identity token exported from another client.";
/// The web UI's first replace warning.
pub const REPLACE_WARNING: &str = "This room already has an identity. Importing will REPLACE it \u{2014} you'll lose access to your current identity for this room unless you've exported it first.";
/// The web UI's second replace warning.
pub const REPLACE_SESSIONS_WARNING: &str = "Close any other tabs or devices open to this room first. A session still using the old identity can write it back and undo the switch.";

impl Chat {
    /// Open "Export identity" for the open room, and ask for its token.
    pub fn open_export(&mut self) {
        if !self.can_change_rooms() {
            return;
        }
        let Some(key) = self.room().map(|r| r.key.clone()) else {
            return;
        };
        self.export = Some(ExportDialog {
            room: key.clone(),
            state: ExportState::Making,
            copied: false,
        });
        self.emit(key, Action::ExportId);
    }

    /// The token for the room keyed `room` came back. Dropped when that dialog is no longer
    /// open (closed, or for another room), so a token nobody is looking at is not kept.
    pub fn identity_exported(&mut self, room: String, token: Result<String, String>) {
        let Some(dialog) = &mut self.export else {
            return;
        };
        if dialog.room != room || dialog.state != ExportState::Making {
            return;
        }
        dialog.state = match token {
            Ok(t) => ExportState::Ready(Secret(t)),
            Err(why) => ExportState::Failed(why),
        };
    }

    /// Copy was pressed: the label changes. The copy itself is the caller's, to the clipboard.
    pub fn export_copied(&mut self) {
        if let Some(d) = &mut self.export {
            d.copied = true;
        }
    }

    /// Close "Export identity", dropping the token.
    pub fn close_export(&mut self) {
        self.export = None;
    }

    /// Open "Import identity", empty.
    pub fn open_import(&mut self) {
        self.open_dialog(DialogKind::Import);
    }

    /// Check the pasted token. A bad one is said in the dialog, which stays open. One for a room
    /// that is in the list asks first (the dialog stays open, holding the checked token);
    /// otherwise the import goes to the background work and the dialog closes.
    pub fn submit_import(&mut self) {
        if self.form.open != Some(DialogKind::Import) || self.form.replace.is_some() {
            return;
        }
        let pasted = self.form.token.text.clone();
        let export = match crate::rooms::identity::parse_token(&pasted) {
            Ok(e) => e,
            Err(why) => {
                self.form.error = Some(why);
                return;
            }
        };
        let key = crate::rooms::identity::room_key(&export.room_owner);
        let token = crate::rooms::IdentityToken(pasted);
        if let Some(room) = self.rooms.iter().find(|r| r.key == key) {
            self.form.error = None;
            // The name of the room you have, not the one the token says: nothing from a pasted
            // token is shown here.
            self.form.replace = Some(ReplaceConfirm {
                token,
                room_name: Some(room.name.clone()),
            });
            return;
        }
        self.close_dialog();
        self.emit(
            String::new(),
            Action::ImportId {
                token,
                replace: false,
            },
        );
    }

    /// The user confirmed replacing: import the token the warning was raised for.
    pub fn confirm_replace(&mut self) {
        let Some(confirm) = self.form.replace.take() else {
            return;
        };
        self.close_dialog();
        self.emit(
            String::new(),
            Action::ImportId {
                token: confirm.token,
                replace: true,
            },
        );
    }

    /// The user backed out of replacing: back to the box, keeping what was pasted.
    pub fn cancel_replace(&mut self) {
        self.form.replace = None;
    }

    /// The box was edited: a warning raised for the token before the edit no longer applies.
    pub fn import_token_edited(&mut self) {
        self.form.replace = None;
        self.form.error = None;
    }
}

/// "Export identity", when open.
pub(super) fn export_dialog(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    let Some(dialog) = &chat.export else {
        return;
    };
    let p = st::palette();
    let mut body = DialogSpec::new("export-identity", "Export identity", |s: &mut RiverApp| {
        s.chat.close_export()
    })
    .body_key("export-body")
    .width(520.0)
    .child(
        label(EXPORT_INTRO, st::BODY, p.text_secondary)
            .wrap()
            .w_full()
            .key("export-intro"),
    )
    .child(notice_text(
        NoticeKind::Warning,
        "export-warning",
        // The notice draws its own warning icon in place of the text's leading one.
        EXPORT_WARNING.trim_start_matches('\u{26a0}').trim_start(),
    ));
    let mut copy = Button::new(if dialog.copied {
        "Copied!"
    } else {
        "Copy to clipboard"
    })
    .variant(ButtonVariant::Contrast);
    match &dialog.state {
        ExportState::Making => {
            body = body.child(
                label(
                    "Preparing your identity token\u{2026}",
                    st::SMALL,
                    p.text_muted,
                )
                .key("export-status"),
            );
            copy = copy.disabled(true);
        }
        ExportState::Failed(why) => {
            body = body.child(
                label(why.clone(), st::BODY, p.danger)
                    .wrap()
                    .w_full()
                    .key("export-error"),
            );
            copy = copy.disabled(true);
        }
        ExportState::Ready(Secret(token)) => {
            body = body.child(
                col()
                    .key("export-token-box")
                    .w_full()
                    .max_h(180.0)
                    .padding(12.0)
                    .radius(12.0)
                    .fill(p.surface)
                    .border(1.0, p.line)
                    .scroll(rune::ScrollSpec {
                        stick_bottom: false,
                    })
                    .child(
                        label(token.clone(), st::SMALL, p.text)
                            .wrap_anywhere()
                            .w_full()
                            .key("export-token"),
                    ),
            );
            let token = token.clone();
            copy = copy.on_press_tx(move |s: &mut RiverApp, tx: &mut rune::Tx| {
                tx.copy(token.clone());
                s.chat.export_copied();
            });
        }
    }
    body.footer(dk::footer(
        Vec::new(),
        vec![
            dk::quiet_button(k, "Close", "export-close", |s: &mut RiverApp| {
                s.chat.close_export()
            }),
            copy.build(k).key("export-copy"),
        ],
    ))
    .build(k, layer);
}

/// "Import identity", when it is the open dialog.
pub(super) fn import_dialog(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    let p = st::palette();
    let get: Get = |s| &mut s.chat.form.token;
    let mut body = DialogSpec::new("import-identity", "Import identity", |s: &mut RiverApp| {
        s.chat.close_dialog()
    })
    .body_key("import-body")
    .width(520.0)
    .child(
        label(IMPORT_INTRO, st::BODY, p.text_secondary)
            .wrap()
            .w_full()
            .key("import-intro"),
    )
    .child(
        TextArea::new(&chat.form.token)
            .label("Identity token".to_owned())
            .placeholder(
                "-----BEGIN RIVER IDENTITY-----\n...\n-----END RIVER IDENTITY-----".to_owned(),
            )
            .width(None)
            .rows(6)
            .max_height(180.0)
            .wrap_anywhere()
            .on_edit(get)
            .on_key(word_end(get))
            .on_change(|s: &mut RiverApp, _| s.chat.import_token_edited())
            .build(k)
            .key("import-token")
            .w_full(),
    );
    if let Some(error) = &chat.form.error {
        body = body.child(
            label(error.clone(), st::BODY, p.danger)
                .wrap()
                .w_full()
                .key("form-error"),
        );
    }
    if let Some(confirm) = &chat.form.replace {
        if let Some(name) = &confirm.room_name {
            body = body.child(
                label(
                    format!("Room: {}", name.chars().take(80).collect::<String>()),
                    st::SMALL_STRONG,
                    p.text_secondary,
                )
                .wrap()
                .w_full()
                .key("import-replace-room"),
            );
        }
        body.child(notice_text(
            NoticeKind::Danger,
            "import-identity-replace-warning",
            REPLACE_WARNING,
        ))
        .child(notice_text(
            NoticeKind::Warning,
            "import-identity-replace-multitab-warning",
            REPLACE_SESSIONS_WARNING,
        ))
        .footer(dk::footer(
            Vec::new(),
            vec![
                dk::quiet_button(
                    k,
                    "Cancel",
                    "import-identity-replace-cancel",
                    |s: &mut RiverApp| s.chat.cancel_replace(),
                ),
                dk::danger_button(
                    k,
                    "Replace identity",
                    "import-identity-replace-confirm",
                    |s: &mut RiverApp| s.chat.confirm_replace(),
                ),
            ],
        ))
        .build(k, layer);
    } else {
        body.footer(dk::footer(
            Vec::new(),
            vec![
                dk::quiet_button(k, "Cancel", "form-cancel", |s: &mut RiverApp| {
                    s.chat.close_dialog()
                }),
                dk::primary_button(k, "Import", "form-submit", |s: &mut RiverApp| {
                    s.chat.submit_import()
                }),
            ],
        ))
        .build(k, layer);
    }
}
