//! Do not disturb: a switch and an optional daily schedule that hold back OS notifications
//! (and with them their sounds) while on. Unread counts are not touched, and what was held
//! back is never shown later.
//!
//! The controls are the first group of Settings > Notifications ([`dnd_group`]); the room
//! list's footer keeps one quiet row that shows the state and flips the switch
//! ([`Chat::quick_dnd`]). The group opens with a state line ([`Settings::state_line`]) that is
//! always true of the moment it is drawn, so a schedule that turned it on is never contradicted
//! by a switch that reads "off".
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

use rune::{El, TextEdit, col, row};
use rune_kit::{Kit, Switch, TextInput};
use serde::{Deserialize, Serialize};

use super::settings::{section_rows, setting_row};
use super::style::{self as st, Palette};
use super::view::label;
use crate::RiverApp;
use crate::notify::Arrival;

/// The preference key the settings are stored under.
pub const PREF: &str = "do_not_disturb";

/// Said after leaving the schedule's times with something that is not a time.
const INVALID_NOTICE: &str = "Do not disturb schedule not changed: the times were not valid.";

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

    /// One sentence saying whether do not disturb is on at `minute` and until when. It reads the
    /// same settings [`Settings::active`] does, so it cannot disagree with what the app does.
    pub fn state_line(&self, minute: u16) -> String {
        match self.reason(minute) {
            Some(Reason::Switch) => "Do not disturb is on until you turn it off.".into(),
            Some(Reason::Schedule) => {
                let end = self.schedule.map_or(0, |s| s.end);
                format!(
                    "Do not disturb is on until {} (your daily schedule).",
                    format_time(end)
                )
            }
            None => match self.schedule.filter(|s| s.valid()) {
                Some(s) if s.start == s.end => "Do not disturb is off. Your daily schedule starts \
                     and ends at the same time, so it never turns it on."
                    .into(),
                Some(s) => format!(
                    "Do not disturb is off. Your daily schedule turns it on at {}.",
                    format_time(s.start)
                ),
                None => "Do not disturb is off.".into(),
            },
        }
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
    /// Whether the schedule's times are on screen (Settings > Notifications is the open
    /// section). Kept in step by [`super::Chat::track_dnd_editor`].
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
    /// The section was left while the typed times were not valid: the text and the error are
    /// kept for when it comes back, so the person can fix them.
    kept_invalid: bool,
    /// What the room list and the dialog say after such a leave, until it is dismissed or the
    /// times are fixed.
    pub notice: Option<String>,
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
            kept_invalid: false,
            notice: None,
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
        self.kept_invalid = false;
        self.notice = None;
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
                self.kept_invalid = false;
                self.notice = None;
                let new = Schedule { start, end };
                let editing = self.start.focused || self.end.focused;
                if self.settings.schedule != Some(new) {
                    self.settings.schedule = Some(new);
                    self.last_schedule = new;
                    self.times_pending = true;
                }
                // Saved once typing pauses (neither field has the cursor), and at the latest
                // when the dialog closes: not on every keystroke.
                if self.times_pending && !editing {
                    self.times_pending = false;
                    self.dirty = true;
                }
            }
            _ => {
                self.error = Some(
                    "Times are 24 hour, like 22:00 or 7:30. To discard the schedule, turn it off."
                        .into(),
                );
            }
        }
    }
}

impl super::Chat {
    /// Open Settings on the Notifications section. Does nothing while another dialog is open.
    pub fn open_dnd(&mut self) {
        if self.open_settings() {
            self.settings
                .select(super::settings::Section::Notifications);
            self.track_dnd_editor();
        }
    }

    /// Keep the schedule's text fields in step with the Settings dialog. Filled from the saved
    /// schedule when Notifications comes on screen, unless times that were not valid were left
    /// there (those stay, with their error, so they can be fixed). When the section goes (another
    /// section, Esc, the X) the typed times are taken if they read as times. If they do not, the
    /// schedule in force is left as it was, the text and error are kept, and a notice says the
    /// schedule was not changed. Run every frame and by the opening functions.
    pub fn track_dnd_editor(&mut self) {
        let showing =
            self.settings.open && self.settings.section == super::settings::Section::Notifications;
        if showing && !self.dnd.open {
            if !self.dnd.kept_invalid {
                self.dnd.fill_inputs();
            }
            self.dnd.open = true;
        } else if !showing && self.dnd.open {
            self.dnd.sync_inputs();
            if std::mem::take(&mut self.dnd.times_pending) {
                self.dnd.dirty = true;
            }
            if self.dnd.error.is_some() && self.dnd.settings.schedule.is_some() {
                self.dnd.kept_invalid = true;
                self.dnd.notice = Some(INVALID_NOTICE.into());
            } else {
                self.dnd.error = None;
            }
            self.dnd.open = false;
        }
    }

    /// Flip the switch ("Turn on until I turn it off").
    pub fn toggle_dnd(&mut self) {
        self.dnd.settings.manual = !self.dnd.settings.manual;
        self.dnd.dirty = true;
    }

    /// The room list's quick control. If the switch is on, turn it off (the schedule may still
    /// have it on; the row and the tooltip say so). If the daily schedule alone has it on,
    /// flipping the switch on would change nothing you could see, so open the settings where the
    /// schedule can be changed. Off: turn the switch on, until it is turned off.
    pub fn quick_dnd(&mut self) {
        if !self.dnd.settings.manual && self.dnd.active() {
            self.open_dnd();
        } else {
            self.toggle_dnd();
        }
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

    /// Flip "Show the sender and text in notifications".
    pub fn toggle_previews(&mut self) {
        self.private_previews = !self.private_previews;
        self.previews_dirty = true;
    }

    /// The previews choice to save, once, if it changed since the last call.
    pub fn take_previews_dirty(&mut self) -> Option<bool> {
        std::mem::take(&mut self.previews_dirty).then_some(self.private_previews)
    }

    pub fn toggle_dnd_mentions(&mut self) {
        self.dnd.settings.allow_mentions = !self.dnd.settings.allow_mentions;
        self.dnd.dirty = true;
    }
}

impl Settings {
    /// Whether the daily schedule is inside its window at `minute`, whatever the switch says.
    fn reason_schedule(&self, minute: u16) -> bool {
        self.schedule
            .is_some_and(|s| s.valid() && s.contains(minute))
    }
}

/// The room list's row text: the state in two words.
pub(super) fn quick_label(on: bool) -> &'static str {
    if on {
        "Do not disturb: on"
    } else {
        "Do not disturb: off"
    }
}

/// What pressing the room list's row does, for its tooltip.
pub(super) fn quick_help(chat: &super::Chat) -> String {
    let d = &chat.dnd;
    if d.settings.manual {
        if d.settings.reason_schedule(d.now) {
            "Press to turn off the switch. Your daily schedule still has do not disturb on.".into()
        } else {
            "Press to turn do not disturb off.".into()
        }
    } else if d.active() {
        "On because of your daily schedule. Press to change the schedule.".into()
    } else {
        "Press to turn do not disturb on until you turn it off.".into()
    }
}

/// The do not disturb group of Settings > Notifications: the state line, the switch, the daily
/// schedule with its times, and whether mentions and replies still come through.
pub(super) fn dnd_group(chat: &super::Chat, p: &Palette, k: &Kit) -> El {
    let d = &chat.dnd;
    let on = d.active();
    let state = label(
        d.settings.state_line(d.now),
        st::BODY,
        if on { p.accent } else { p.text_secondary },
    )
    .wrap()
    .w_full()
    .key("dnd-state");
    let switch = setting_row(
        "dnd-switch-row",
        label("Turn on until I turn it off", st::BODY, p.text),
        Some(note(
            "No notifications or sounds. Unread counts still update.",
            "dnd-switch-note",
            p,
        )),
        Switch::new(d.settings.manual)
            .label("Turn on do not disturb until I turn it off")
            .hide_label()
            .on_toggle(|s: &mut RiverApp| s.chat.toggle_dnd())
            .build(k)
            .key("dnd-switch"),
        p,
    );
    let mut schedule = col()
        .key("dnd-schedule-block")
        .w_full()
        .gap(10.0)
        .child(setting_row(
            "dnd-schedule-row",
            label("Turn on every day on a schedule", st::BODY, p.text),
            Some(note(
                "Times use this computer's clock.",
                "dnd-schedule-note",
                p,
            )),
            Switch::new(d.settings.schedule.is_some())
                .label("Turn on do not disturb every day on a schedule")
                .hide_label()
                .on_toggle(|s: &mut RiverApp| s.chat.toggle_dnd_schedule())
                .build(k)
                .key("dnd-schedule"),
            p,
        ));
    if d.settings.schedule.is_some() {
        schedule = schedule.child(
            row()
                .key("dnd-times")
                .w_full()
                .gap(12.0)
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(4.0)
                        .child(label("From", st::SMALL, p.text_secondary))
                        .child(
                            TextInput::new(&d.start)
                                .label("From".to_owned())
                                .placeholder("22:00".to_owned())
                                .width(None)
                                .on_edit(|s: &mut RiverApp| &mut s.chat.dnd.start)
                                .build(k)
                                .key("dnd-start"),
                        ),
                )
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(4.0)
                        .child(label("Until", st::SMALL, p.text_secondary))
                        .child(
                            TextInput::new(&d.end)
                                .label("Until".to_owned())
                                .placeholder("07:00".to_owned())
                                .width(None)
                                .on_edit(|s: &mut RiverApp| &mut s.chat.dnd.end)
                                .build(k)
                                .key("dnd-end"),
                        ),
                ),
        );
        if let Some(e) = &d.error {
            schedule = schedule.child(
                label(e.clone(), st::SMALL, p.danger)
                    .wrap()
                    .w_full()
                    .key("dnd-error"),
            );
        }
    }
    let mentions = setting_row(
        "dnd-mentions-row",
        label("Let mentions and replies through", st::BODY, p.text),
        Some(note(
            "Messages that mention you or reply to you still notify.",
            "dnd-mentions-note",
            p,
        )),
        Switch::new(d.settings.allow_mentions)
            .label("Let mentions and replies through do not disturb")
            .hide_label()
            .on_toggle(|s: &mut RiverApp| s.chat.toggle_dnd_mentions())
            .build(k)
            .key("dnd-mentions"),
        p,
    );
    col()
        .key("settings-dnd")
        .w_full()
        .gap(10.0)
        .child(label("Do not disturb", st::ROOM_STRONG, p.text))
        .child(state)
        .child(section_rows(
            "settings-dnd-rows",
            vec![switch, schedule, mentions],
            p,
        ))
}

fn note(text: &str, key: &'static str, p: &Palette) -> El {
    label(text.to_owned(), st::SMALL, p.text_muted)
        .wrap()
        .w_full()
        .key(key)
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
        ui.start.focused = true;
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
        ui.start.focused = false;
        ui.sync_inputs();
        assert!(ui.take_dirty().is_some(), "saved once typing pauses");
        assert!(ui.take_dirty().is_none());
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
    fn leaving_with_times_that_do_not_parse_keeps_the_schedule_says_so_and_keeps_the_text() {
        let mut chat = crate::chat::Chat::example();
        chat.dnd.settings.schedule = Some(DEFAULT_SCHEDULE);
        chat.open_dnd();
        assert!(chat.dnd.open && chat.settings.open);
        chat.dnd.start.replace_text("nope");
        chat.dnd.sync_inputs();
        assert!(chat.dnd.error.is_some(), "the reason shows while editing");
        chat.close_settings();
        assert!(!chat.dnd.open);
        assert_eq!(
            chat.dnd.settings.schedule,
            Some(DEFAULT_SCHEDULE),
            "the schedule in force is untouched"
        );
        assert_eq!(chat.dnd.notice.as_deref(), Some(INVALID_NOTICE));
        assert!(chat.dnd.take_dirty().is_none(), "nothing to save");
        // Coming back shows what was typed, and why it is not in use.
        chat.open_dnd();
        assert_eq!(chat.dnd.start.text, "nope");
        assert!(chat.dnd.error.is_some());
        // Fixing it applies it and takes the notice away.
        chat.dnd.start.replace_text("21:00");
        chat.dnd.sync_inputs();
        assert!(chat.dnd.error.is_none() && chat.dnd.notice.is_none());
        chat.close_settings();
        let saved = chat.dnd.take_dirty().expect("saved on leaving");
        assert_eq!(saved.schedule.unwrap().start, at(21, 0));
        assert!(chat.dnd.take_dirty().is_none());
        assert!(chat.dnd.notice.is_none());
        // Off and on again offers the times that were in use.
        chat.toggle_dnd_schedule();
        chat.toggle_dnd_schedule();
        assert_eq!(chat.dnd.settings.schedule.unwrap().start, at(21, 0));
    }

    #[test]
    fn turning_the_schedule_off_forgets_kept_invalid_times() {
        let mut chat = crate::chat::Chat::example();
        chat.dnd.settings.schedule = Some(DEFAULT_SCHEDULE);
        chat.open_dnd();
        chat.dnd.end.replace_text("x");
        chat.dnd.sync_inputs();
        chat.close_settings();
        assert!(chat.dnd.notice.is_some());
        chat.open_dnd();
        chat.toggle_dnd_schedule();
        assert!(chat.dnd.notice.is_none() && chat.dnd.error.is_none());
        assert_eq!(chat.dnd.settings.schedule, None);
    }

    #[test]
    fn choosing_another_section_while_times_are_pending_saves_them() {
        let mut chat = crate::chat::Chat::example();
        chat.dnd.settings.schedule = Some(DEFAULT_SCHEDULE);
        chat.open_dnd();
        chat.dnd.start.replace_text("20:15");
        chat.dnd.start.focused = true;
        chat.dnd.sync_inputs();
        assert!(chat.dnd.take_dirty().is_none(), "still typing");
        chat.select_settings_section(crate::chat::settings::Section::Connection);
        assert!(!chat.dnd.open);
        let saved = chat.dnd.take_dirty().expect("saved on leaving");
        assert_eq!(saved.schedule.unwrap().start, at(20, 15));
    }

    /// The sentence at the top of the group, for every way do not disturb can be on or off.
    #[test]
    fn the_state_line_is_true_in_every_state() {
        let sched = Some(Schedule {
            start: at(22, 0),
            end: at(7, 0),
        });
        // (switch, schedule, minute) -> line, and whether the app treats it as on.
        let cases: [(bool, Option<Schedule>, u16, &str, bool); 9] = [
            (false, None, at(12, 0), "Do not disturb is off.", false),
            (
                true,
                None,
                at(12, 0),
                "Do not disturb is on until you turn it off.",
                true,
            ),
            (
                true,
                sched,
                at(23, 0),
                "Do not disturb is on until you turn it off.",
                true,
            ),
            (
                false,
                sched,
                at(23, 0),
                "Do not disturb is on until 07:00 (your daily schedule).",
                true,
            ),
            (
                false,
                sched,
                at(12, 0),
                "Do not disturb is off. Your daily schedule turns it on at 22:00.",
                false,
            ),
            // The window's edges: on at its start, off at its end.
            (
                false,
                sched,
                at(22, 0),
                "Do not disturb is on until 07:00 (your daily schedule).",
                true,
            ),
            (
                false,
                sched,
                at(7, 0),
                "Do not disturb is off. Your daily schedule turns it on at 22:00.",
                false,
            ),
            (
                true,
                sched,
                at(12, 0),
                "Do not disturb is on until you turn it off.",
                true,
            ),
            (
                false,
                Some(Schedule {
                    start: at(7, 0),
                    end: at(7, 0),
                }),
                at(7, 0),
                "Do not disturb is off. Your daily schedule starts and ends at the same time, \
                 so it never turns it on.",
                false,
            ),
        ];
        for (manual, schedule, minute, line, on) in cases {
            let s = Settings {
                manual,
                schedule,
                allow_mentions: false,
            };
            assert_eq!(s.state_line(minute), line, "{s:?} at {minute}");
            assert_eq!(s.active(minute), on, "{s:?} at {minute}");
            // The line says "is on" exactly when the app holds notifications back.
            assert_eq!(line.contains("is on"), on, "{line}");
        }
    }

    /// A schedule that expires while the dialog is open: the line follows the clock the app
    /// refreshes, with no reopening.
    #[test]
    fn the_state_line_follows_the_clock_while_it_is_open() {
        let mut chat = crate::chat::Chat::example();
        chat.dnd.settings.schedule = Some(Schedule {
            start: at(9, 0),
            end: at(17, 0),
        });
        chat.dnd.now = at(16, 59);
        assert!(
            chat.dnd
                .settings
                .state_line(chat.dnd.now)
                .contains("until 17:00")
        );
        chat.dnd.now = at(17, 0);
        assert_eq!(
            chat.dnd.settings.state_line(chat.dnd.now),
            "Do not disturb is off. Your daily schedule turns it on at 09:00."
        );
    }

    #[test]
    fn the_quick_control_flips_the_switch_and_sends_a_schedule_to_settings() {
        let mut chat = crate::chat::Chat::example();
        chat.dnd.pinned = true;
        chat.dnd.now = at(12, 0);
        chat.dnd.settings.schedule = Some(DEFAULT_SCHEDULE);
        // Off: on, until turned off.
        assert!(!chat.dnd.active());
        chat.quick_dnd();
        assert!(chat.dnd.settings.manual && chat.dnd.active());
        assert!(!chat.settings.open);
        assert!(chat.dnd.take_dirty().is_some());
        // On by the switch alone: off.
        chat.quick_dnd();
        assert!(!chat.dnd.settings.manual && !chat.dnd.active());
        // On because of the schedule: pressing could change nothing, so Settings opens.
        chat.dnd.now = at(23, 0);
        assert!(chat.dnd.active());
        chat.quick_dnd();
        assert!(!chat.dnd.settings.manual, "the switch is not touched");
        assert!(chat.settings.open && chat.dnd.open);
        assert_eq!(
            chat.settings.section,
            crate::chat::settings::Section::Notifications
        );
        // The row's words and tooltip agree with what it does.
        assert_eq!(quick_label(true), "Do not disturb: on");
        assert!(quick_help(&chat).contains("schedule"));
        // The switch on during the schedule: the press turns the switch off first, and the
        // tooltip says the schedule still applies.
        chat.close_settings();
        chat.dnd.settings.manual = true;
        assert!(quick_help(&chat).contains("schedule still has"));
        chat.quick_dnd();
        assert!(!chat.dnd.settings.manual, "the switch goes off");
        assert!(!chat.settings.open, "without opening Settings");
        assert!(chat.dnd.active(), "the schedule still has it on");
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
