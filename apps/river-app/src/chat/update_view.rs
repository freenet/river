//! The update block in the room list and the Updates section of the settings dialog (the state
//! they show is [`crate::update::UpdateUi`]).

use rune::{El, col, row};
use rune_kit::{Button, ButtonVariant, Kit, Switch};

use super::settings::{section_rows, setting_row};
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

/// The Updates section of the settings: River's version and whether it is up to date, a button
/// to check, and the switch for downloading updates by itself.
pub(super) fn updates_section(ui: &UpdateUi, p: &Palette, k: &Kit) -> El {
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
    let automatic = ui.support == UpdateSupport::Automatic;

    let mut notes = col().gap(2.0).child(
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
        notes = notes.child(
            label(
                format!("Version {version} is ready to install."),
                st::SMALL,
                p.text_secondary,
            )
            .wrap()
            .w_full()
            .key("settings-staged"),
        );
    }
    let mut buttons = col().gap(6.0).items_end().child(
        Button::new("Check now")
            .variant(ButtonVariant::Outline)
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
    let rows = vec![
        setting_row(
            "settings-version-row",
            label(format!("River {}", ui.version), st::BODY, p.text).key("settings-version"),
            Some(notes),
            buttons,
            p,
        ),
        setting_row(
            "settings-auto-row",
            label("Update River automatically", st::BODY, p.text),
            Some(
                label(
                    "Downloads new versions in the background. River never restarts by itself; \
                     you choose when.",
                    st::SMALL,
                    p.text_muted,
                )
                .wrap()
                .w_full()
                .key("settings-auto-note"),
            ),
            Switch::new(ui.auto && automatic)
                .label("Update River automatically")
                .hide_label()
                .disabled(!automatic)
                .on_toggle(|s: &mut RiverApp| s.set_auto_update(!s.chat.update.auto))
                .build(k)
                .key("settings-auto"),
            p,
        ),
    ];
    let mut s = section_rows("settings-river", rows, p);
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
    if let Some(n) = &ui.notice {
        s = s.child(
            label(n.clone(), st::SMALL, p.warning_text)
                .wrap()
                .w_full()
                .key("settings-notice"),
        );
    }
    s
}
