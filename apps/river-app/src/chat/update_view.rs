//! The update block in the room list and the settings dialog (the state they show is
//! [`crate::update::UpdateUi`]).

use rune::{El, col, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, Dialog, Kit};

use super::style::{self as st, Palette};
use super::view::label;
use crate::RiverApp;
use crate::update::{UpdateStatus, UpdateSupport, UpdateUi};
use std::time::Instant;

/// "5 minutes ago", for the settings.
fn ago(then: Instant) -> String {
    let secs = then.elapsed().as_secs();
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} minutes ago", secs / 60),
        _ => format!("{} hours ago", secs / 3600),
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

/// The block in the room list: what is available, downloading or ready, with its button.
pub(super) fn update_block(ui: &UpdateUi, p: &Palette, k: &Kit) -> Option<El> {
    if !ui.shows_block() {
        return None;
    }
    let mut block = col().key("update-block").w_full().px(4.0).gap(6.0);
    if let Some(notice) = &ui.notice {
        block = block
            .child(
                label(notice.clone(), st::SMALL, p.warning_text)
                    .wrap()
                    .w_full()
                    .key("update-notice"),
            )
            .child(
                Button::new("Dismiss")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.chat.update.notice = None)
                    .build(k)
                    .key("update-notice-dismiss"),
            );
    }
    let say = |key: &str, line: String, color| {
        label(line, st::SMALL, color)
            .wrap()
            .w_full()
            .key(key.to_string())
    };
    if matches!(ui.status, UpdateStatus::Restarting) {
        return Some(block.child(say(
            "update-line",
            "Restarting River…".into(),
            p.text_secondary,
        )));
    }
    // A downloaded version waits for a restart whatever the latest check said.
    if let Some(version) = ui.ready() {
        block = block.child(say(
            "update-line",
            format!("River {version} is ready. Restart to use it."),
            p.text_secondary,
        ));
        if let Some(err) = &ui.restart_error {
            block = block.child(say("update-restart-error", err.clone(), p.danger));
        }
        if let Some(what) = &ui.confirm_restart {
            block = block
                .child(say(
                    "update-confirm",
                    format!("{what} Restart anyway?"),
                    p.warning_text,
                ))
                .child(
                    row()
                        .gap(8.0)
                        .child(
                            Button::new("Wait")
                                .variant(ButtonVariant::Contrast)
                                .on_press(|s: &mut RiverApp| s.chat.update.confirm_restart = None)
                                .build(k)
                                .key("update-wait"),
                        )
                        .child(
                            Button::new("Restart anyway")
                                .variant(ButtonVariant::Ghost)
                                .on_press(|s: &mut RiverApp| s.restart_now())
                                .build(k)
                                .key("update-restart-anyway"),
                        ),
                );
        } else {
            block = block.child(
                Button::new("Restart to update")
                    .variant(ButtonVariant::Contrast)
                    .on_press(|s: &mut RiverApp| s.restart_to_update())
                    .build(k)
                    .key("update-restart"),
            );
        }
        return Some(block);
    }
    match &ui.status {
        UpdateStatus::Available(i) => {
            block = block.child(say(
                "update-line",
                match (&ui.support, i.required) {
                    (UpdateSupport::Automatic, true) => format!(
                        "River {} is available. This version is no longer supported.",
                        i.version
                    ),
                    (UpdateSupport::Automatic, false) => {
                        format!("River {} is available.", i.version)
                    }
                    (UpdateSupport::External(why) | UpdateSupport::Unavailable(why), _) => {
                        format!("River {} is available. {why}", i.version)
                    }
                },
                p.text_secondary,
            ));
            if ui.support == UpdateSupport::Automatic {
                block = block.child(
                    Button::new("Download update")
                        .variant(ButtonVariant::Contrast)
                        .on_press(|s: &mut RiverApp| s.start_update_download())
                        .build(k)
                        .key("update-download"),
                );
            }
        }
        UpdateStatus::Downloading {
            version,
            received,
            total,
        } => {
            let of = if *total > 0 {
                format!(" ({} of {})", megabytes(*received), megabytes(*total))
            } else {
                String::new()
            };
            block = block.child(say(
                "update-line",
                format!("Downloading River {version}{of}…"),
                p.text_secondary,
            ));
        }
        _ => {}
    }
    Some(block)
}

/// The status line and buttons of the River section of the settings.
fn river_section(ui: &UpdateUi, p: &Palette, k: &Kit) -> El {
    let status_line = match &ui.status {
        UpdateStatus::Idle => "Not checked yet.".to_string(),
        UpdateStatus::Checking => "Checking…".to_string(),
        UpdateStatus::UpToDate => match ui.checked {
            Some(t) => format!("River is up to date. Checked {}.", ago(t)),
            None => "River is up to date.".to_string(),
        },
        UpdateStatus::Available(i) => format!("Version {} is available.", i.version),
        UpdateStatus::Downloading {
            version,
            received,
            total,
        } => {
            format!(
                "Downloading {version}: {} of {}.",
                megabytes(*received),
                megabytes(*total)
            )
        }
        UpdateStatus::Ready { version } => format!("Version {version} is ready to install."),
        UpdateStatus::Restarting => "Restarting…".to_string(),
        UpdateStatus::Failed(e) => format!("Couldn't update: {e}"),
    };
    let failed = matches!(ui.status, UpdateStatus::Failed(_));
    let busy = matches!(
        ui.status,
        UpdateStatus::Checking | UpdateStatus::Downloading { .. } | UpdateStatus::Restarting
    );
    let mut s = col()
        .key("settings-river")
        .w_full()
        .gap(8.0)
        .child(label(format!("River {}", ui.version), st::BODY, p.text).key("settings-version"))
        .child(
            label(
                status_line,
                st::SMALL,
                if failed { p.danger } else { p.text_secondary },
            )
            .wrap()
            .w_full()
            .key("settings-update-status"),
        );
    if let Some(version) = ui.ready()
        && !matches!(ui.status, UpdateStatus::Ready { .. })
    {
        s = s.child(
            label(
                format!("Version {version} is ready to install."),
                st::SMALL,
                p.text_secondary,
            )
            .key("settings-staged"),
        );
    }
    match &ui.support {
        UpdateSupport::Automatic => {}
        UpdateSupport::External(why) | UpdateSupport::Unavailable(why) => {
            s = s.child(
                label(why.clone(), st::SMALL, p.text_muted)
                    .wrap()
                    .w_full()
                    .key("settings-update-support"),
            );
        }
    }
    let automatic = ui.support == UpdateSupport::Automatic;
    let mut buttons = row().gap(8.0).child(
        Button::new("Check now")
            .variant(ButtonVariant::Ghost)
            .disabled(busy)
            .on_press(|s: &mut RiverApp| s.check_for_update())
            .build(k)
            .key("settings-check-now"),
    );
    if ui.ready().is_some() && automatic && !matches!(ui.status, UpdateStatus::Restarting) {
        buttons = buttons.child(
            Button::new("Restart to update")
                .variant(ButtonVariant::Contrast)
                .on_press(|s: &mut RiverApp| s.restart_to_update())
                .build(k)
                .key("settings-restart"),
        );
    }
    s.child(
        Checkbox::new(ui.auto && automatic)
            .label("Update River automatically")
            .disabled(!automatic)
            .on_toggle(|s: &mut RiverApp| s.set_auto_update(!s.chat.update.auto))
            .build(k)
            .key("settings-auto"),
    )
    .child(
        label(
            "Downloads new versions in the background. River never restarts by itself; \
             you choose when.",
            st::SMALL,
            p.text_muted,
        )
        .wrap()
        .w_full()
        .key("settings-auto-note"),
    )
    .child(buttons)
}

fn node_section(ui: &UpdateUi, p: &Palette, k: &Kit) -> El {
    let mut s = col().key("settings-freenet").w_full().gap(6.0).child(
        label(
            match &ui.node_version {
                Some(v) => format!("Freenet {}", super::node_status::safe_version(v)),
                None => "Freenet: not connected".to_string(),
            },
            st::BODY,
            p.text,
        )
        .key("settings-node-version"),
    );
    s = s.child(
        label(
            "Freenet updates itself in the background. River does not replace it, and never \
             stops a Freenet it did not start. River needs Freenet 0.2.156 or newer.",
            st::SMALL,
            p.text_muted,
        )
        .wrap()
        .w_full()
        .key("settings-node-note"),
    );
    if let Some(note) = &ui.node_note {
        s = s.child(
            label(note.clone(), st::SMALL, p.text_secondary)
                .wrap()
                .w_full()
                .key("settings-node-changed"),
        );
    }
    s.child(
        Button::new("Node status")
            .variant(ButtonVariant::Ghost)
            .on_press(|s: &mut RiverApp| {
                s.chat.update.settings_open = false;
                s.chat.status.open();
            })
            .build(k)
            .key("settings-node-status"),
    )
}

fn heading(s: &str, p: &Palette) -> El {
    text(s.to_string()).font(st::SECTION).color(p.text_muted)
}

/// The settings dialog: River's version and updates, and Freenet's version.
pub(super) fn settings_dialog(ui: &UpdateUi, k: &Kit, layer: &mut Vec<El>) {
    let p = st::palette();
    let mut body = col()
        .key("settings-body")
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(16.0)
        .child(
            col()
                .w_full()
                .gap(8.0)
                .child(heading("RIVER", &p))
                .child(river_section(ui, &p, k)),
        )
        .child(
            col()
                .w_full()
                .gap(8.0)
                .child(heading("FREENET", &p))
                .child(node_section(ui, &p, k)),
        );
    if let Some(n) = &ui.notice {
        body = body.child(
            label(n.clone(), st::SMALL, p.warning_text)
                .wrap()
                .w_full()
                .key("settings-notice"),
        );
    }
    Dialog::new("settings", "Settings")
        .width(440.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.chat.update.settings_open = false)
        .child(body)
        .build(k, layer);
}
