//! "Diagnostics", a section of Settings in the desktop app: where River keeps its log file,
//! and a button that opens that folder.
//!
//! River's diagnostics are lines on stderr, which a Windows release build, or a River started
//! from a launcher or the tray, has nowhere to show. The shell writes them to a file (see
//! `apps/desktop/src/log.rs`); this section tells the user where, so a bug report can attach
//! it. What the file is and how it is opened belongs to the shell, which passes a
//! [`LogHost`]. A shell without one (snapshots, tests) has no such section.

use std::path::PathBuf;
use std::sync::Arc;

use rune::{El, col};
use rune_kit::{Button, ButtonVariant, Kit};

use super::settings::{section_rows, setting_row};
use super::style::{self as st, Palette};
use super::view::label;
use crate::RiverApp;

/// What the shell offers for its log file.
pub trait LogHost: Send + Sync + 'static {
    /// The file River is writing its diagnostics to.
    fn log_path(&self) -> PathBuf;

    /// Show the folder that holds it in the system's file manager.
    fn open_log_folder(&self) -> Result<(), String>;
}

/// The section's state.
pub struct DiagnosticsUi {
    host: Arc<dyn LogHost>,
    /// Why the folder did not open.
    pub error: Option<String>,
}

impl DiagnosticsUi {
    pub fn new(host: Arc<dyn LogHost>) -> Self {
        DiagnosticsUi { host, error: None }
    }

    /// The log file, as the shell says.
    pub fn path(&self) -> PathBuf {
        self.host.log_path()
    }
}

impl super::Chat {
    /// Press "Open log folder".
    pub fn open_log_folder(&mut self) {
        if let Some(d) = &mut self.diagnostics {
            d.error = d.host.open_log_folder().err();
        }
    }
}

/// The section's body. Empty without a shell that offers it (the list does not show it then).
pub(super) fn diagnostics_section(chat: &super::Chat, p: &Palette, k: &Kit) -> El {
    let Some(d) = chat.diagnostics.as_ref() else {
        return col().key("settings-diagnostics");
    };
    let note = col()
        .gap(2.0)
        .child(
            label(d.path().display().to_string(), st::BODY, p.text_secondary)
                .wrap()
                .w_full()
                .key("settings-log-path"),
        )
        .child(
            label(
                "River writes what it reports here, with the time of each line. It keeps this \
                 file and the one before it, about 2 MB each. It holds no message text or \
                 keys. Attach it when you report a problem.",
                st::SMALL,
                p.text_muted,
            )
            .wrap()
            .w_full()
            .key("settings-log-note"),
        );
    let mut s = col()
        .key("settings-diagnostics")
        .w_full()
        .gap(14.0)
        .child(section_rows(
            "settings-diagnostics-rows",
            vec![setting_row(
                "settings-log-row",
                label("Log file", st::BODY, p.text),
                Some(note),
                Button::new("Open log folder")
                    .variant(ButtonVariant::Outline)
                    .on_press(|s: &mut RiverApp| s.chat.open_log_folder())
                    .build(k)
                    .key("settings-log-open"),
                p,
            )],
            p,
        ));
    if let Some(e) = &d.error {
        s = s.child(
            label(e.clone(), st::SMALL, p.danger)
                .wrap()
                .w_full()
                .key("settings-log-error"),
        );
    }
    s
}
