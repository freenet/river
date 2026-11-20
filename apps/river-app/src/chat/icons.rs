//! The chat screen's icons, as SVG bodies on a 16 point grid (Rune tints them with `.color()`).

use rune::Icon;

pub static LOCK: Icon = Icon {
    name: "river-lock",
    view: 16.0,
    body: r#"<rect x="3.5" y="7" width="9" height="6.5" rx="1.5" stroke="currentColor" stroke-width="1.4"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#,
};

pub static PLUS: Icon = Icon {
    name: "river-plus",
    view: 16.0,
    body: r#"<path d="M8 3v10M3 8h10" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

pub static USERS: Icon = Icon {
    name: "river-users",
    view: 16.0,
    body: r#"<circle cx="6" cy="5.5" r="2.5" stroke="currentColor" stroke-width="1.4"/><path d="M1.5 13.5c0-2.5 2-4 4.5-4s4.5 1.5 4.5 4" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/><path d="M10.5 3.2a2.5 2.5 0 0 1 0 4.6M12.2 9.8c1.4.5 2.3 1.8 2.3 3.7" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#,
};

pub static SEND: Icon = Icon {
    name: "river-send",
    view: 16.0,
    body: r#"<path d="M2.5 8 13.5 2.5 10.5 13.5 7.5 9 2.5 8Z" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/><path d="M7.5 9 13.5 2.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#,
};

pub static REPLY: Icon = Icon {
    name: "river-reply",
    view: 16.0,
    body: r#"<path d="M6.5 4 2.5 8l4 4" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/><path d="M3 8h6.5a4 4 0 0 1 4 4v1" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#,
};

pub static CROWN: Icon = Icon {
    name: "river-crown",
    view: 16.0,
    body: r#"<path d="M2.5 5.5 5.5 8 8 3.5 10.5 8l3-2.5-1.2 7H3.7l-1.2-7Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>"#,
};

/// The deputy shield: the web UI's `SHIELD_ICON_PATH` (a 24 point grid).
pub static SHIELD: Icon = Icon {
    name: "river-shield",
    view: 24.0,
    body: r#"<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10Z" stroke="currentColor" stroke-width="2" stroke-linejoin="round"/>"#,
};

pub static CHAT: Icon = Icon {
    name: "river-chat",
    view: 16.0,
    body: r#"<path d="M3 3.5h10a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H7l-3 2.5v-2.5H3a1 1 0 0 1-1-1v-6a1 1 0 0 1 1-1Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M5 6.5h6M5 8.5h4" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#,
};

pub static INVITE: Icon = Icon {
    name: "river-invite",
    view: 16.0,
    body: r#"<circle cx="6.5" cy="5.5" r="2.5" stroke="currentColor" stroke-width="1.4"/><path d="M2 13.5c0-2.5 2-4 4.5-4 1.2 0 2.3.3 3.1 1M12.5 9v5M10 11.5h5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#,
};

pub static SMILE: Icon = Icon {
    name: "river-smile",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="6" stroke="currentColor" stroke-width="1.4"/><path d="M5.5 9.5a3 3 0 0 0 5 0" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/><circle cx="6" cy="6.5" r=".9" fill="currentColor"/><circle cx="10" cy="6.5" r=".9" fill="currentColor"/>"#,
};

pub static EDIT: Icon = Icon {
    name: "river-edit",
    view: 16.0,
    body: r#"<path d="M10.5 2.5 13.5 5.5 5.5 13.5H2.5V10.5Z" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/><path d="M9 4l3 3" stroke="currentColor" stroke-width="1.4"/>"#,
};

pub static TRASH: Icon = Icon {
    name: "river-trash",
    view: 16.0,
    body: r#"<path d="M2.5 4.5h11M6.5 4.5V3h3v1.5M4 4.5l.7 9h6.6l.7-9" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// A box with a lid and a handle: put a conversation away.
pub static ARCHIVE: Icon = Icon {
    name: "river-archive",
    view: 16.0,
    body: r#"<rect x="2" y="3" width="12" height="3.2" rx="1" stroke="currentColor" stroke-width="1.4"/><path d="M3.2 6.2V12a1.3 1.3 0 0 0 1.3 1.3h7A1.3 1.3 0 0 0 12.8 12V6.2M6.5 9h3" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static BELL: Icon = Icon {
    name: "river-bell",
    view: 16.0,
    body: r#"<path d="M4 11V7a4 4 0 0 1 8 0v4l1.2 1.5H2.8L4 11Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M6.5 14a1.7 1.7 0 0 0 3 0" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#,
};

pub static COPY: Icon = Icon {
    name: "river-copy",
    view: 16.0,
    body: r#"<rect x="5.5" y="5.5" width="8" height="8" rx="1.8" stroke="currentColor" stroke-width="1.4"/><path d="M10.5 3.5v-.2a1.8 1.8 0 0 0-1.8-1.8H3.8A1.8 1.8 0 0 0 2 3.3v4.9a1.8 1.8 0 0 0 1.8 1.8h.2" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#,
};

/// The bell struck through: a muted room.
pub static BELL_OFF: Icon = Icon {
    name: "river-bell-off",
    view: 16.0,
    body: r#"<path d="M4 11V7a4 4 0 0 1 6.8-2.8M12 8v3l1.2 1.5H2.8L4 11" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" stroke-linecap="round"/><path d="M6.5 14a1.7 1.7 0 0 0 3 0" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><path d="M2.5 2.5l11 11" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#,
};

pub static CHEVRON_DOWN: Icon = Icon {
    name: "river-chevron-down",
    view: 16.0,
    body: r#"<path d="M3.5 6 8 10.5 12.5 6" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// The web UI's `EXPORT_ICON_PATH` (a 24 point grid): an arrow leaving a tray.
pub static EXPORT: Icon = Icon {
    name: "river-export",
    view: 24.0,
    body: r#"<path d="M12 15V3M7 8l5-5 5 5M5 21h14" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// The web UI's `IMPORT_ICON_PATH` (a 24 point grid): an arrow entering a tray.
pub static IMPORT: Icon = Icon {
    name: "river-import",
    view: 24.0,
    body: r#"<path d="M12 3v12M7 10l5 5 5-5M5 21h14" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// A gear: the settings button.
pub static GEAR: Icon = Icon {
    name: "river-gear",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="2.2" stroke="currentColor" stroke-width="1.4"/><path d="M8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6l1.1 1.1M11.3 11.3l1.1 1.1M12.4 3.6l-1.1 1.1M4.7 11.3l-1.1 1.1" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#,
};

/// A window with its corner tucked behind it, for "Background and startup" (a 24 point grid).
pub static BACKGROUND: Icon = Icon {
    name: "river-background",
    view: 24.0,
    body: r#"<rect x="3" y="5" width="18" height="14" rx="2" stroke="currentColor" stroke-width="2" fill="none"/><path d="M3 10h18" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>"#,
};

/// A crescent moon, for "Do not disturb" (a 16 point grid).
pub static MOON: Icon = Icon {
    name: "river-moon",
    view: 16.0,
    body: r#"<path d="M13.4 9.6A5.6 5.6 0 0 1 6.4 2.6a5.6 5.6 0 1 0 7 7z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" fill="none"/>"#,
};

/// A circle with an "i", for an informational notice.
pub static INFO: Icon = Icon {
    name: "river-info",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="6.2" stroke="currentColor" stroke-width="1.4"/><path d="M8 7.2v3.6" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><circle cx="8" cy="5" r="0.9" fill="currentColor"/>"#,
};

/// A triangle with an exclamation mark, for a warning notice.
pub static WARNING: Icon = Icon {
    name: "river-warning",
    view: 16.0,
    body: r#"<path d="M8 2.2 14.4 13.2H1.6L8 2.2Z" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/><path d="M8 6.4v3.2" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><circle cx="8" cy="11.6" r="0.9" fill="currentColor"/>"#,
};

/// An octagon with an exclamation mark, for a danger notice: a stop sign, not a caution sign.
pub static DANGER: Icon = Icon {
    name: "river-danger",
    view: 16.0,
    body: r#"<path d="M5.2 1.8h5.6l3.4 3.4v5.6l-3.4 3.4H5.2l-3.4-3.4V5.2l3.4-3.4Z" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/><path d="M8 5v3.6" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><circle cx="8" cy="10.8" r="0.9" fill="currentColor"/>"#,
};
