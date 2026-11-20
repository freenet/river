//! Do not disturb: a switch and an optional daily schedule that hold back OS notifications
//! (and with them their sounds) while on. Unread counts are not touched, and what was held
//! back is never shown later.
//!
//! The settings are this device's own: they are kept in the app's local preferences
//! (`rune::prefs`, beside the update choice), never in the shared `rooms_meta` or any delegate
//! state, so a phone or the web UI is not affected.
//!
//! **Local clock.** The schedule is in minutes since local midnight, read from the operating
//! system's clock each time it is checked. Nothing adjusts for daylight saving: the schedule
//! follows the wall clock, so on the night the clock jumps an hour the window starts and ends
//! at the same wall-clock times as on any other night. All the logic here takes the minute as
//! a parameter; only [`local_minute`] reads the clock.

use rune::{El, TextEdit};
use rune_kit::{Button, ButtonVariant, Checkbox, Dialog, Kit, TextInput};
use serde::{Deserialize, Serialize};

use super::style::{self as st};
use super::view::label;
use crate::RiverApp;
use crate::notify::Arrival;

/// The preference key the settings are stored under.
pub const PREF: &str = "do_not_disturb";

/// Minutes in a day; a schedule time is below this.
const DAY: u16 = 24 * 60;

/// The schedule given when the person turns it on.
pub const DEFAULT_SCHEDULE: Schedule = Schedule {
    start: 22 * 60,
    end: 7 * 60,
};

/// A daily window in minutes since local midnight. It may cross midnight (`start > end`).
/// `start == end` is an empty window: the schedule never turns do not disturb on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub start: u16,
    pub end: u16,
}

impl Schedule {
    fn valid(self) -> bool {
        self.start < DAY && self.end < DAY
    }

    /// Whether `minute` (since local midnight) is inside the window: from `start` up to but not
    /// including `end`.
    pub fn contains(self, minute: u16) -> bool {
        let m = minute % DAY;
        match self.start.cmp(&self.end) {
            std::cmp::Ordering::Equal => false,
            std::cmp::Ordering::Less => self.start <= m && m < self.end,
            std::cmp::Ordering::Greater => m >= self.start || m < self.end,
        }
    }
}

/// What the person chose.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// The switch: on until turned off.
    #[serde(default)]
    pub manual: bool,
    /// The daily window, when one is set.
    #[serde(default)]
    pub schedule: Option<Schedule>,
    /// Whether a message that mentions you or replies to you still notifies.
    #[serde(default)]
    pub allow_mentions: bool,
}

/// Why do not disturb is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Switch,
    Schedule,
}

impl Settings {
    /// Why do not disturb is on at `minute`, if it is. The switch wins over the schedule.
    pub fn reason(&self, minute: u16) -> Option<Reason> {
        if self.manual {
            Some(Reason::Switch)
        } else if self
            .schedule
            .is_some_and(|s| s.valid() && s.contains(minute))
        {
            Some(Reason::Schedule)
        } else {
            None
        }
    }

    pub fn active(&self, minute: u16) -> bool {
        self.reason(minute).is_some()
    }

    /// Whether a notification may show at `minute`. `mention` is whether it is about a message
    /// that mentions you.
    pub fn allows(&self, mention: bool, minute: u16) -> bool {
        !self.active(minute) || (mention && self.allow_mentions)
    }

    /// What of `a` may notify at `minute`: all of it when do not disturb is off; otherwise,
    /// with "allow mentions through", only the messages that mention you or reply to you (the
    /// count shrinks to them); otherwise nothing.
    pub fn filter(&self, mut a: Arrival, minute: u16) -> Option<Arrival> {
        if !self.active(minute) {
            return Some(a);
        }
        if self.allow_mentions && a.for_me_count > 0 {
            a.count = a.for_me_count;
            return Some(a);
        }
        None
    }
}

/// `H:MM` or `HH:MM` (24 hour) as minutes since midnight.
pub fn parse_time(text: &str) -> Option<u16> {
    let (h, m) = text.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    if !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Minutes since midnight as `HH:MM`.
pub fn format_time(minute: u16) -> String {
    format!("{:02}:{:02}", minute % DAY / 60, minute % 60)
}

/// The local time of day now, in minutes since midnight. The only clock read.
pub fn local_minute() -> u16 {
    use chrono::Timelike;
    let t = chrono::Local::now();
    (t.hour() * 60 + t.minute()) as u16
}

/// The settings, the dialog and the clock they are judged against.
pub struct DndUi {
    pub settings: Settings,
    /// The local minute of day the chat is judged at; the app refreshes it from [`local_minute`].
    pub now: u16,
    /// Whether `now` is fixed (tests and snapshots) and not to be refreshed.
    pub pinned: bool,
    pub open: bool,
    pub start: TextEdit,
    pub end: TextEdit,
    /// Why the times typed are not in use.
    pub error: Option<String>,
    /// Set when the settings changed and have not been saved.
    dirty: bool,
    /// Times were typed since the dialog opened; saved when it closes, not per keystroke.
    times_pending: bool,
    /// The schedule last in use, offered again when the schedule is turned back on.
    last_schedule: Schedule,
}

impl Default for DndUi {
    fn default() -> Self {
        DndUi {
            settings: Settings::default(),
            now: 0,
            pinned: false,
            open: false,
            start: TextEdit::new(""),
            end: TextEdit::new(""),
            error: None,
            dirty: false,
            times_pending: false,
            last_schedule: DEFAULT_SCHEDULE,
        }
    }
}

impl DndUi {
    /// Replace the settings with the stored ones (an invalid schedule is dropped).
    pub fn load(&mut self, mut s: Settings) {
        if s.schedule.is_some_and(|x| !x.valid()) {
            s.schedule = None;
        }
        if let Some(x) = s.schedule {
            self.last_schedule = x;
        }
        self.settings = s;
    }

    /// Whether do not disturb is on now.
    pub fn active(&self) -> bool {
        self.settings.active(self.now)
    }

    /// The settings to save, once, if they changed since the last call.
    pub fn take_dirty(&mut self) -> Option<Settings> {
        std::mem::take(&mut self.dirty).then(|| self.settings.clone())
    }

    fn fill_inputs(&mut self) {
        let s = self.settings.schedule.unwrap_or(self.last_schedule);
        self.start.replace_text(format_time(s.start));
        self.end.replace_text(format_time(s.end));
        self.error = None;
    }

    /// Take the times typed into the schedule when both are valid; say so when not. The
    /// previous schedule stays in force until they are.
    pub fn sync_inputs(&mut self) {
        if !self.open || self.settings.schedule.is_none() {
            return;
        }
        match (parse_time(&self.start.text), parse_time(&self.end.text)) {
            (Some(start), Some(end)) => {
                self.error = None;
                let new = Schedule { start, end };
                if self.settings.schedule != Some(new) {
                    self.settings.schedule = Some(new);
                    self.last_schedule = new;
                    self.times_pending = true;
                }
            }
            _ => {
                self.error = Some("Times are 24 hour, like 22:00 or 7:30.".into());
            }
        }
    }
}

impl super::Chat {
    pub fn open_dnd(&mut self) {
        self.dnd.fill_inputs();
        self.dnd.open = true;
    }

    /// Close the dialog, saving typed times. Times that do not parse keep it open with the
    /// reason shown, so a schedule is never silently ignored.
    pub fn close_dnd(&mut self) {
        self.dnd.sync_inputs();
        if self.dnd.error.is_some() {
            return;
        }
        if std::mem::take(&mut self.dnd.times_pending) {
            self.dnd.dirty = true;
        }
        self.dnd.open = false;
    }

    /// Flip the switch (the footer button).
    pub fn toggle_dnd(&mut self) {
        self.dnd.settings.manual = !self.dnd.settings.manual;
        self.dnd.dirty = true;
    }

    pub fn toggle_dnd_schedule(&mut self) {
        let d = &mut self.dnd;
        d.settings.schedule = match d.settings.schedule {
            Some(s) => {
                d.last_schedule = s;
                None
            }
            None => Some(d.last_schedule),
        };
        d.times_pending = false;
        d.fill_inputs();
        d.dirty = true;
    }

    pub fn toggle_dnd_mentions(&mut self) {
        self.dnd.settings.allow_mentions = !self.dnd.settings.allow_mentions;
        self.dnd.dirty = true;
    }
}

/// The dialog, when open.
pub(super) fn dnd_dialog(chat: &super::Chat, k: &Kit, layer: &mut Vec<El>) {
    let d = &chat.dnd;
    if !d.open {
        return;
    }
    let p = st::palette();
    let mut body = rune::col()
        .key("dnd-body")
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(12.0);
    body = body.child(
        Checkbox::new(d.settings.manual)
            .label("Do not disturb")
            .description("No notifications or sounds. Unread counts still update.")
            .on_toggle(|s: &mut RiverApp| s.chat.toggle_dnd())
            .build(k)
            .key("dnd-switch"),
    );
    body = body.child(
        Checkbox::new(d.settings.schedule.is_some())
            .label("Turn on every day on a schedule")
            .description("Times are on this computer's clock.")
            .on_toggle(|s: &mut RiverApp| s.chat.toggle_dnd_schedule())
            .build(k)
            .key("dnd-schedule"),
    );
    if d.settings.schedule.is_some() {
        body = body.child(
            rune::row()
                .key("dnd-times")
                .w_full()
                .gap(12.0)
                .child(
                    TextInput::new(&d.start)
                        .label("From".to_owned())
                        .placeholder("22:00".to_owned())
                        .width(None)
                        .on_edit(|s: &mut RiverApp| &mut s.chat.dnd.start)
                        .build(k)
                        .key("dnd-start")
                        .grow(1.0),
                )
                .child(
                    TextInput::new(&d.end)
                        .label("Until".to_owned())
                        .placeholder("07:00".to_owned())
                        .width(None)
                        .on_edit(|s: &mut RiverApp| &mut s.chat.dnd.end)
                        .build(k)
                        .key("dnd-end")
                        .grow(1.0),
                ),
        );
        if let Some(e) = &d.error {
            body = body.child(
                label(e.clone(), st::SMALL, p.danger)
                    .wrap()
                    .w_full()
                    .key("dnd-error"),
            );
        }
    }
    body = body.child(
        Checkbox::new(d.settings.allow_mentions)
            .label("Allow mentions through")
            .description("Messages that mention you or reply to you still notify.")
            .on_toggle(|s: &mut RiverApp| s.chat.toggle_dnd_mentions())
            .build(k)
            .key("dnd-mentions"),
    );
    body = body.child(
        label(
            "These settings stay on this device. Messages held back are not shown later.",
            st::SMALL,
            p.text_muted,
        )
        .wrap()
        .w_full()
        .key("dnd-note"),
    );
    body = body.child(
        rune::row().w_full().justify_end().child(
            Button::new("Close")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut RiverApp| s.chat.close_dnd())
                .build(k)
                .key("dnd-close"),
        ),
    );
    Dialog::new("dnd", "Do not disturb")
        .width(420.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.chat.close_dnd())
        .child(body)
        .build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: u16, m: u16) -> u16 {
        h * 60 + m
    }

    #[test]
    fn a_window_inside_one_day() {
        let s = Schedule {
            start: at(9, 0),
            end: at(17, 0),
        };
        assert!(!s.contains(at(8, 59)));
        assert!(s.contains(at(9, 0)));
        assert!(s.contains(at(16, 59)));
        assert!(!s.contains(at(17, 0)));
    }

    #[test]
    fn a_window_across_midnight() {
        let s = DEFAULT_SCHEDULE;
        assert!(!s.contains(at(21, 59)));
        assert!(s.contains(at(22, 0)));
        assert!(s.contains(at(23, 59)));
        assert!(s.contains(0));
        assert!(s.contains(at(6, 59)));
        assert!(!s.contains(at(7, 0)));
        assert!(!s.contains(at(12, 0)));
    }

    #[test]
    fn equal_start_and_end_is_never_on() {
        let s = Schedule {
            start: at(7, 0),
            end: at(7, 0),
        };
        assert!((0..DAY).all(|m| !s.contains(m)));
    }

    #[test]
    fn the_switch_and_the_schedule() {
        let mut s = Settings {
            schedule: Some(DEFAULT_SCHEDULE),
            ..Default::default()
        };
        assert_eq!(s.reason(at(23, 0)), Some(Reason::Schedule));
        assert_eq!(s.reason(at(12, 0)), None);
        s.manual = true;
        assert_eq!(s.reason(at(12, 0)), Some(Reason::Switch));
        assert_eq!(s.reason(at(23, 0)), Some(Reason::Switch));
        assert!(!Settings::default().active(at(3, 0)));
    }

    #[test]
    fn an_out_of_range_stored_schedule_is_never_on() {
        let s = Settings {
            schedule: Some(Schedule {
                start: 5000,
                end: 10,
            }),
            ..Default::default()
        };
        assert!(!s.active(5));
        let mut ui = DndUi::default();
        ui.load(s);
        assert_eq!(ui.settings.schedule, None);
    }

    #[test]
    fn mentions_pass_only_when_allowed() {
        let mut s = Settings {
            manual: true,
            ..Default::default()
        };
        assert!(!s.allows(false, 0));
        assert!(!s.allows(true, 0));
        s.allow_mentions = true;
        assert!(!s.allows(false, 0));
        assert!(s.allows(true, 0));
        s.manual = false;
        assert!(s.allows(false, 0), "off means everything shows");
    }

    #[test]
    fn times_parse_and_print() {
        assert_eq!(parse_time("22:00"), Some(at(22, 0)));
        assert_eq!(parse_time(" 7:30 "), Some(at(7, 30)));
        assert_eq!(parse_time("00:00"), Some(0));
        assert_eq!(parse_time("23:59"), Some(at(23, 59)));
        for bad in [
            "", "24:00", "7:60", "7", "7:5", "ab:cd", "-1:00", "007:00", "7:300",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        assert_eq!(format_time(at(7, 5)), "07:05");
        for m in [0, 59, 60, 1439] {
            assert_eq!(parse_time(&format_time(m)), Some(m));
        }
    }

    #[test]
    fn typed_times_become_the_schedule_and_bad_ones_keep_the_old() {
        let mut ui = DndUi {
            open: true,
            ..Default::default()
        };
        ui.settings.schedule = Some(DEFAULT_SCHEDULE);
        ui.fill_inputs();
        ui.start.replace_text("23:30");
        ui.sync_inputs();
        assert_eq!(
            ui.settings.schedule,
            Some(Schedule {
                start: at(23, 30),
                end: at(7, 0)
            })
        );
        assert!(ui.error.is_none());
        assert!(ui.take_dirty().is_none(), "typing alone does not save");
        ui.end.replace_text("soon");
        ui.sync_inputs();
        assert_eq!(
            ui.settings.schedule,
            Some(Schedule {
                start: at(23, 30),
                end: at(7, 0)
            })
        );
        assert!(ui.error.is_some());
    }

    #[test]
    fn the_dialog_will_not_close_on_times_that_do_not_parse_and_saves_good_ones_once() {
        let mut chat = crate::chat::Chat::example();
        chat.dnd.settings.schedule = Some(DEFAULT_SCHEDULE);
        chat.open_dnd();
        chat.dnd.start.replace_text("nope");
        chat.close_dnd();
        assert!(chat.dnd.open && chat.dnd.error.is_some());
        chat.dnd.start.replace_text("21:00");
        chat.close_dnd();
        assert!(!chat.dnd.open);
        let saved = chat.dnd.take_dirty().expect("saved on close");
        assert_eq!(saved.schedule.unwrap().start, at(21, 0));
        assert!(chat.dnd.take_dirty().is_none());
        // Off and on again offers the times that were in use.
        chat.toggle_dnd_schedule();
        chat.toggle_dnd_schedule();
        assert_eq!(chat.dnd.settings.schedule.unwrap().start, at(21, 0));
    }

    #[test]
    fn settings_round_trip_and_tolerate_missing_fields() {
        let s = Settings {
            manual: true,
            schedule: Some(DEFAULT_SCHEDULE),
            allow_mentions: true,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
        assert_eq!(
            serde_json::from_str::<Settings>("{}").unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn settings_survive_a_save_and_reload_through_the_preferences_file() {
        let dir = std::env::temp_dir().join(format!("river-dnd-test-{}", std::process::id()));
        let path = dir.join("prefs.json");
        let s = Settings {
            manual: false,
            schedule: Some(DEFAULT_SCHEDULE),
            allow_mentions: true,
        };
        rune::prefs::Prefs::at(&path).set(PREF, &s).unwrap();
        let back: Settings = rune::prefs::Prefs::at(&path).get(PREF).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(back, s);
    }

    /// Do not disturb is per device: nothing in it may reach the shared rooms state.
    #[test]
    fn this_module_does_not_touch_shared_state() {
        let src = include_str!("dnd.rs").replace("\r\n", "\n");
        let code: String = src
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["rooms_meta", "Action::", "emit(", "Command::"] {
            assert!(!code.contains(forbidden), "{forbidden}");
        }
    }
}
