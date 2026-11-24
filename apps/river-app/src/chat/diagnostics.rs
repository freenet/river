//! "Diagnostics", a section of Settings in the desktop app: where River keeps its log file,
//! and a button that opens that folder.
//!
//! River's diagnostics are lines on stderr, which a Windows release build, or a River started
//! from a launcher or the tray, has nowhere to show. The shell writes them to a file when
//! nothing else is reading stderr (see `apps/desktop/src/log.rs`); this section tells the user
//! where, so a bug report can attach it, or says that the output goes where River was started
//! from. What the file is and how it is opened belongs to the shell, which passes a
//! [`LogHost`]. A shell without one (snapshots, tests) has no such section.

use std::path::PathBuf;
use std::sync::Arc;

use rune::{El, col};
use rune_kit::{Button, ButtonVariant, Kit};

use super::settings::{section_rows, setting_row};
use super::style::{self as st, Palette};
use super::view::label;
use crate::RiverApp;

/// Where the shell's diagnostics go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogState {
    /// To this file.
    File(PathBuf),
    /// To where River was started from (a terminal, a file, a system logger): no file.
    Elsewhere,
    /// Nowhere: nothing reads them and the file could not be made.
    Unavailable,
}

/// What the shell offers for its log file.
pub trait LogHost: Send + Sync + 'static {
    /// Where the diagnostics go.
    fn log_state(&self) -> LogState;

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

    /// Where the diagnostics go, as the shell says.
    pub fn state(&self) -> LogState {
        self.host.log_state()
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
    let path = match d.state() {
        LogState::File(path) => path,
        // Each sentence is true whatever else is going on: that is why there is no file.
        LogState::Elsewhere => {
            return notice(
                "River was started with its output going elsewhere (a terminal, a file or the \
                 system's log), so its diagnostics go there and River keeps no log file of its \
                 own.",
                "settings-log-elsewhere",
                p,
            );
        }
        LogState::Unavailable => {
            return notice(
                "River could not open its log file, so its diagnostics are lost. Check that \
                 River's config folder can be written to, then start River again.",
                "settings-log-unavailable",
                p,
            );
        }
    };
    let note = col()
        .gap(2.0)
        .child(
            label(path.display().to_string(), st::BODY, p.text_secondary)
                .wrap()
                .w_full()
                .key("settings-log-path"),
        )
        .child(
            label(
                "River writes what it reports here, with the time of each line. It does not \
                 write message text, nicknames or signing keys, but it does write room and \
                 contract keys, and your user name where it is part of a path: look through \
                 it before you share it. A new file starts when this one is over 2 MB at \
                 startup; the old one is kept as river.log.1.",
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

fn notice(text: &str, key: &'static str, p: &Palette) -> El {
    col().key("settings-diagnostics").w_full().child(
        label(text.to_owned(), st::BODY, p.text_secondary)
            .wrap()
            .w_full()
            .key(key),
    )
}
