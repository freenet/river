//! The notification dialog, which opens from the bell in the conversation header. It follows
//! the web UI's `NotificationModal` (`ui/src/components/room_list/notification_modal.rs`): the
//! room's three modes with the web UI's labels and help, the current one checked, a pick
//! applies and closes it.
//!
//! The mode is the web UI's too: it is stored in the same `rooms_meta` value, so a pick here
//! shows in the browser and the other way round (`crate::rooms::modes`). The web UI's
//! "Browser permission" section has no counterpart: the operating system asks the first time
//! this app shows a notification.

use rune::{El, col};
use rune_kit::{Dialog, Kit, RadioGroup, RadioItem};

use super::icons;
use super::model::Room;
use super::style::{self as st};
use super::view::label;
use crate::RiverApp;
use crate::rooms::modes::NotificationMode;

/// The bell's icon for a mode, as the web UI's header shows it: struck through when muted.
pub(super) fn bell(mode: NotificationMode) -> &'static rune::Icon {
    match mode {
        NotificationMode::Muted => &icons::BELL_OFF,
        _ => &icons::BELL,
    }
}

/// The bell's name and tooltip, as the web UI's (`mode_title`).
pub(super) fn bell_title(mode: NotificationMode) -> String {
    format!("Notifications: {}", mode.label())
}

pub(super) fn notify_dialog(room: &Room, mode: NotificationMode, k: &Kit, layer: &mut Vec<El>) {
    let p = st::palette();
    let mut group = RadioGroup::new();
    for m in NotificationMode::ALL {
        let key = room.key.clone();
        group = group.item(
            RadioItem::new(m.label(), m == mode)
                .description(m.help())
                .on_select(move |s: &mut RiverApp| s.chat.set_mode(&key, m)),
        );
    }
    let body = col()
        .key("notify-modes")
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(12.0)
        .child(group.build(k).key("notify-mode-options"))
        .child(
            label(
                "Applies to this room and syncs across your devices.",
                st::SMALL,
                p.text_muted,
            )
            .wrap()
            .w_full()
            .key("notify-modes-note"),
        );
    Dialog::new("notify", "Notifications")
        .description(room.name.clone())
        .width(400.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.chat.close_notify_dialog())
        .child(body)
        .build(k, layer);
}
