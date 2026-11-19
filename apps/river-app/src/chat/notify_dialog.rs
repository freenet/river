//! The notification dialog, which opens from the bell in the conversation header. It follows
//! the web UI's `NotificationModal` (`ui/src/components/room_list/notification_modal.rs`): the
//! room's three modes with the web UI's labels and help, the current one checked, a pick
//! applies and closes it.
//!
//! The mode is the web UI's too: it is stored in the same `rooms_meta` value, so a pick here
//! shows in the browser and the other way round (`crate::rooms::modes`). The web UI's
//! "Browser permission" section has no counterpart: the operating system asks the first time
//! this app shows a notification.

use rune::{El, col, icon};
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

pub(super) fn notify_dialog(
    room: &Room,
    mode: NotificationMode,
    master_on: bool,
    k: &Kit,
    layer: &mut Vec<El>,
) {
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
    let mut body = col()
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
    if !master_on {
        // The switch in the room list's footer is the master: while it is off nothing
        // notifies, whatever a room's mode says.
        body = body.child(
            col()
                .key("notify-master-off")
                .w_full()
                .gap(4.0)
                .child(
                    icon(&icons::BELL_OFF, 14.0)
                        .color(p.warning_text)
                        .label("Notifications are off"),
                )
                .child(
                    label(
                        "Notifications are switched off for every room, in the room list. \
                         Turn them on there for this choice to take effect.",
                        st::SMALL,
                        p.warning_text,
                    )
                    .wrap()
                    .w_full(),
                ),
        );
    }
    Dialog::new("notify", "Notifications")
        .description(room.name.clone())
        .width(400.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.chat.close_notify_dialog())
        .child(body)
        .build(k, layer);
}
