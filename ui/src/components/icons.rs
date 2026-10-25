//! Line icons: 24-unit paths, stroked in the current colour, as in the design
//! mockup. Decorative (`aria-hidden`); the control that holds one carries the
//! accessible name.

use dioxus::prelude::*;

/// A 24-unit line icon (stroke, no fill) at `size` px, decorative.
pub(crate) fn line_icon(icon_d: &'static str, size: u32) -> Element {
    rsx! {
        svg {
            "aria-hidden": "true",
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.8",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            class: "flex-none",
            path { d: icon_d }
        }
    }
}

pub(crate) const SHIELD_ICON_PATH: &str = "M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10";
pub(crate) const JOIN_ICON_PATH: &str =
    "M15 3h4a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-4M10 17l5-5-5-5M15 12H3";
pub(crate) const REPLY_ICON_PATH: &str = "M9 14 4 9l5-5M20 20v-7a4 4 0 0 0-4-4H4";
pub(crate) const REACT_ICON_PATH: &str =
    "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01";
pub(crate) const EDIT_ICON_PATH: &str = "M12 20h9M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z";
pub(crate) const DELETE_ICON_PATH: &str = "M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6";
pub(crate) const INFO_ICON_PATH: &str = "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 16v-4M12 8h.01";
pub(crate) const BELL_ICON_PATH: &str =
    "M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9M10.3 21a1.94 1.94 0 0 0 3.4 0";
pub(crate) const BELL_OFF_ICON_PATH: &str =
    "M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9M10.3 21a1.94 1.94 0 0 0 3.4 0M3 3l18 18";
pub(crate) const PEOPLE_ICON_PATH: &str = "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2M9 3a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM22 21v-2a4 4 0 0 0-3-3.87M16 3.13a4 4 0 0 1 0 7.75";
pub(crate) const MENU_ICON_PATH: &str = "M4 6h16M4 12h16M4 18h16";
pub(crate) const CHEVRON_DOWN_ICON_PATH: &str = "m6 9 6 6 6-6";
pub(crate) const CHEVRON_UP_ICON_PATH: &str = "m18 15-6-6-6 6";
pub(crate) const SEARCH_ICON_PATH: &str = "M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14zM20 20l-3.5-3.5";
pub(crate) const SEND_ICON_PATH: &str = "m22 2-7 20-4-9-9-4ZM22 2 11 13";
pub(crate) const PLUS_ICON_PATH: &str = "M5 12h14M12 5v14";
pub(crate) const LOCK_ICON_PATH: &str = "M6 11h12a2 2 0 0 1 2 2v6a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2v-6a2 2 0 0 1 2-2zM8 11V7a4 4 0 0 1 8 0v4";
pub(crate) const IMPORT_ICON_PATH: &str = "M12 3v12M7 10l5 5 5-5M5 21h14";
pub(crate) const EXPORT_ICON_PATH: &str = "M12 15V3M7 8l5-5 5 5M5 21h14";
pub(crate) const INVITE_ICON_PATH: &str =
    "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2M9 3a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM19 8v6M22 11h-6";
pub(crate) const CROWN_ICON_PATH: &str = "m2 7 4.5 10h11L22 7l-5.5 4L12 4l-4.5 7L2 7zM6 21h12";
pub(crate) const KEY_ICON_PATH: &str =
    "M7.5 10a5.5 5.5 0 1 0 0 11 5.5 5.5 0 0 0 0-11zM21 2l-9.6 9.6M15.5 7.5l3 3L22 7l-3-3";
pub(crate) const GLOBE_ICON_PATH: &str =
    "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 3a14 14 0 0 0 0 18 14 14 0 0 0 0-18M3 12h18";
pub(crate) const LINK_ICON_PATH: &str = "M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71";
pub(crate) const REORDER_ICON_PATH: &str = "M7 4v16M3 8l4-4 4 4M17 20V4M13 16l4 4 4-4";
pub(crate) const COPY_ICON_PATH: &str =
    "M10 8h9a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-9a2 2 0 0 1-2-2v-9a2 2 0 0 1 2-2zM16 8V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h3";
