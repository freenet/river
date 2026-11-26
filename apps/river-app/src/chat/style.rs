//! The chat screen's colours and type, taken from the web UI's light and dark themes
//! (`ui/assets/tailwind.css`) so the two look like one product.
//!
//! [`palette`] returns the colours of the theme the current frame is drawn in. The view sets
//! that once at the top of each frame ([`set_dark`]) from the appearance setting and the
//! system's appearance, so the ~30 call sites need no extra argument.

use std::cell::Cell;

use rune::tok::TextStyle;
use rune::{Color, oklch};

pub struct Palette {
    /// The conversation column.
    pub bg: Color,
    /// The room list and members panel.
    pub rail: Color,
    /// Selected rows, inputs, buttons.
    pub surface: Color,
    pub surface_hover: Color,
    /// Hover on a row in a rail.
    pub row_hover: Color,
    /// Rows of messages this device sent, and under the pointer.
    pub own_row: Color,
    pub own_row_hover: Color,
    pub line: Color,
    pub line_strong: Color,
    pub shadow: Color,
    /// Over the conversation behind a panel that covers it.
    pub scrim: Color,
    pub text: Color,
    pub text_secondary: Color,
    pub text_muted: Color,
    pub quote: Color,
    pub accent: Color,
    /// The accent at 15% over the conversation column (badges, own reactions).
    pub accent_soft: Color,
    pub accent_line: Color,
    /// Text that says something failed (the web UI's `--color-danger-text`).
    pub danger: Color,
    /// A similar-name warning's text, fill and rule (the web UI's `--color-warning-text`,
    /// `--color-warning-bg` and `--color-warning`).
    pub warning_text: Color,
    pub warning_bg: Color,
    pub warning: Color,
    /// A danger notice's fill, and the filled danger button with its hover.
    pub danger_bg: Color,
    pub danger_fill: Color,
    pub danger_fill_hover: Color,
    /// The connection dot: connected and failed (the web UI's `--color-success` and
    /// `--color-error`).
    pub success: Color,
    pub error: Color,
}

fn hex(s: &str) -> Color {
    rune::color::hex(s).expect("a valid colour")
}

thread_local! {
    /// The theme the frame being built is drawn in. The UI runs on one thread, and each test
    /// thread builds its own frames, so a thread-local needs no locking and cannot leak a
    /// theme between parallel tests.
    static DARK: Cell<bool> = const { Cell::new(true) };
}

/// Draw the frames that follow in the dark theme (`true`) or the light one.
pub fn set_dark(dark: bool) {
    DARK.with(|d| d.set(dark));
}

/// Whether the frame being built is dark.
pub fn is_dark() -> bool {
    DARK.with(Cell::get)
}

/// The colours of the theme the current frame is drawn in.
pub fn palette() -> Palette {
    palette_for(is_dark())
}

/// The colours of the dark theme (`true`) or the light one.
pub fn palette_for(dark_theme: bool) -> Palette {
    if dark_theme { dark() } else { light() }
}

/// What the Appearance setting asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Follow the operating system.
    #[default]
    System,
    Light,
    Dark,
}

/// The preference the mode is kept under.
pub const MODE_PREF: &str = "appearance";

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::System, Mode::Light, Mode::Dark];

    /// Whether to draw dark, given whether the system is.
    pub fn dark(self, system_dark: bool) -> bool {
        match self {
            Mode::System => system_dark,
            Mode::Light => false,
            Mode::Dark => true,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::System => "system",
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Mode::System => "Same as system",
            Mode::Light => "Light",
            Mode::Dark => "Dark",
        }
    }

    /// The stored value's mode; anything unrecognised means [`Mode::System`].
    pub fn from_name(s: &str) -> Mode {
        Mode::ALL
            .into_iter()
            .find(|m| m.name() == s)
            .unwrap_or_default()
    }
}

/// `fg` at opacity `a` over `bg`: what the web UI's `color-mix(in srgb, fg a%, bg)` gives. The
/// mix is in the gamma-encoded sRGB the web mixes in, not in the linear light colours are held in.
fn over(fg: Color, a: f32, bg: Color) -> Color {
    use rune::color::{srgb_decode, srgb_encode};
    let mix = |f: f32, b: f32| srgb_decode(srgb_encode(f) * a + srgb_encode(b) * (1.0 - a));
    Color {
        r: mix(fg.r, bg.r),
        g: mix(fg.g, bg.g),
        b: mix(fg.b, bg.b),
        a: 1.0,
    }
}

/// The web UI's dark theme (`@media (prefers-color-scheme: dark)` in `tailwind.css`).
fn dark() -> Palette {
    Palette {
        bg: hex("#141416"),
        rail: hex("#0f0f11"),
        surface: hex("#1e1e21"),
        surface_hover: hex("#28282c"),
        row_hover: hex("#18181b"),
        own_row: hex("#1c1d22"),
        own_row_hover: hex("#21242a"),
        line: hex("#2a2a2e"),
        line_strong: hex("#3a3a3e"),
        shadow: Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.35,
        },
        scrim: Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.5,
        },
        text: hex("#f0f0f2"),
        text_secondary: hex("#b4b4bb"),
        text_muted: hex("#9a9a9e"),
        quote: hex("#b4b4b9"),
        accent: hex("#97b2e1"),
        accent_soft: hex("#282c34"),
        accent_line: hex("#5d6f8f"),
        danger: hex("#f97066"),
        warning_text: hex("#e3a845"),
        warning_bg: hex("#422006"),
        warning: hex("#eab308"),
        danger_bg: hex("#3b1513"),
        danger_fill: hex("#b42318"),
        danger_fill_hover: hex("#d0392d"),
        success: hex("#22c55e"),
        error: hex("#ef4444"),
    }
}

/// The web UI's light theme (the `:root` block and `@theme` in `tailwind.css`). Each field
/// takes the value of the web variable its dark value came from; the comments name it.
fn light() -> Palette {
    let panel = hex("#ffffff");
    let rail = hex("#f4f4f6");
    let surface = hex("#ebebeb");
    // `--color-link` is the light `--color-accent`.
    let link = oklch(0.50, 0.11, 262.0);
    Palette {
        // --color-panel
        bg: panel,
        // --color-rail
        rail,
        // --color-surface, --color-surface-hover
        surface,
        surface_hover: hex("#e8e8ea"),
        // --color-row-hover: the surface at 40% over what the row sits on (here the rail).
        row_hover: over(surface, 0.4, rail),
        // --color-row-self and -self-hover: the link colour at 6% and 10% over the panel.
        own_row: over(link, 0.06, panel),
        own_row_hover: over(link, 0.10, panel),
        // --color-border, --color-border-hover
        line: hex("#e0e0e4"),
        line_strong: hex("#d0d0d4"),
        shadow: Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.18,
        },
        scrim: Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.35,
        },
        // --color-text, -text-secondary, -text-muted, -text-quote
        text: hex("#1a1a1a"),
        text_secondary: hex("#484850"),
        text_muted: hex("#6b6b6b"),
        quote: hex("#5c5c5c"),
        // --color-link
        accent: link,
        // --color-accent-soft: the link colour at 9% over the panel.
        accent_soft: over(link, 0.09, panel),
        // The dark value is the accent at about 55% over the panel.
        accent_line: over(link, 0.55, panel),
        // --color-danger-text
        danger: hex("#b42318"),
        // --color-warning-text, -warning-bg, --color-warning
        warning_text: hex("#8a5800"),
        warning_bg: hex("#fefce8"),
        warning: hex("#eab308"),
        // --color-error-bg; --color-danger and -danger-hover
        danger_bg: hex("#fef2f2"),
        danger_fill: hex("#b42318"),
        danger_fill_hover: hex("#912018"),
        // --color-success, --color-error
        success: hex("#22c55e"),
        error: hex("#ef4444"),
    }
}

pub const BRAND: TextStyle = TextStyle::new(18.0, 700.0).tracking(-0.18);
pub const SECTION: TextStyle = TextStyle::new(12.0, 600.0);
pub const ROOM: TextStyle = TextStyle::new(14.0, 500.0);
pub const ROOM_STRONG: TextStyle = TextStyle::new(14.0, 600.0);
pub const HEADER: TextStyle = TextStyle::new(16.0, 600.0).lh(24.0);
pub const HEADER_SMALL: TextStyle = TextStyle::new(14.5, 700.0);
pub const TITLE: TextStyle = TextStyle::new(17.0, 600.0);
pub const BODY: TextStyle = TextStyle::new(14.0, 400.0);
pub const NAME: TextStyle = TextStyle::new(15.0, 700.0);
pub const MESSAGE: TextStyle = TextStyle::new(15.0, 400.0).lh(22.0);
pub const QUOTE: TextStyle = TextStyle::new(13.0, 400.0).lh(18.0);
/// A join line: the web UI's 13px muted text on 20px lines.
pub const JOIN: TextStyle = TextStyle::new(13.0, 400.0).lh(20.0);
pub const SMALL: TextStyle = TextStyle::new(12.0, 400.0);
pub const SMALL_STRONG: TextStyle = TextStyle::new(12.5, 600.0);
pub const TAG: TextStyle = TextStyle::new(10.5, 700.0).tracking(0.3);
pub const GUTTER_TIME: TextStyle = TextStyle::new(11.0, 400.0);
pub const BADGE: TextStyle = TextStyle::new(11.5, 700.0);
pub const EMOJI: TextStyle = TextStyle::new(15.0, 400.0);

/// A member's avatar disc and initials colours. The web UI hashes the member's key with a
/// per-device salt and rounds the result to one of ten hues; this keeps the ten hues and the
/// colours, and picks the hue from the member id.
pub fn avatar_colors(id: u32) -> (Color, Color) {
    // Multiplying by 7 (coprime with 10) gives consecutive ids different hues.
    let hue = id.wrapping_mul(7) % 10;
    let h = hue as f32 * 36.0;
    if is_dark() {
        // The web UI's `--avatar-l`, `--avatar-c` and `--avatar-text-l` in each theme.
        (oklch(0.42, 0.07, h), oklch(0.93, 0.045, h))
    } else {
        (oklch(0.90, 0.05, h), oklch(0.37, 0.045, h))
    }
}

// Not in `settings_tests.rs`: the dialog-kit scan there bans the notice colours' names outside
// this file.
#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance: a `Color` holds linear light, which is what it is computed from.
    fn luminance(c: rune::Color) -> f32 {
        c.luminance()
    }

    fn contrast(a: rune::Color, b: rune::Color) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn the_light_and_dark_palettes_are_the_web_uis_values() {
        let (l, d) = (palette_for(false), palette_for(true));
        assert_eq!(l.bg, rune::color::hex("#ffffff").unwrap());
        assert_eq!(l.rail, rune::color::hex("#f4f4f6").unwrap());
        assert_eq!(l.text, rune::color::hex("#1a1a1a").unwrap());
        assert_eq!(l.warning_text, rune::color::hex("#8a5800").unwrap());
        assert_eq!(d.bg, rune::color::hex("#141416").unwrap());
        assert_eq!(d.text, rune::color::hex("#f0f0f2").unwrap());
    }

    #[test]
    fn text_is_readable_on_the_surfaces_it_sits_on_in_both_themes() {
        for dark in [false, true] {
            let p = palette_for(dark);
            let min = 4.5;
            for (name, fg) in [
                ("text", p.text),
                ("text_secondary", p.text_secondary),
                ("text_muted", p.text_muted),
                ("quote", p.quote),
                ("accent", p.accent),
                ("danger", p.danger),
                ("warning_text", p.warning_text),
            ] {
                for (surface, bg) in [("bg", p.bg), ("rail", p.rail), ("surface", p.surface)] {
                    let c = contrast(fg, bg);
                    // The web UI's own muted and secondary inks are 4.47:1 and 4.48:1 on the
                    // lifted surface (light and dark); everything else clears 4.5:1.
                    let min = if surface == "surface" { 4.4 } else { min };
                    assert!(c >= min, "dark={dark}: {name} on {surface} is {c:.2}:1");
                }
            }
            for (name, fg, bg) in [
                ("warning_text on warning_bg", p.warning_text, p.warning_bg),
                ("danger on danger_bg", p.danger, p.danger_bg),
                ("accent on accent_soft", p.accent, p.accent_soft),
                ("accent on own_row", p.accent, p.own_row),
            ] {
                let c = contrast(fg, bg);
                assert!(c >= min, "dark={dark}: {name} is {c:.2}:1");
            }
        }
    }
}
