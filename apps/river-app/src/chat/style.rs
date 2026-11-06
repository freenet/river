//! The chat screen's colours and type, in the spirit of the web UI's dark theme.

use rune::tok::TextStyle;
use rune::{Color, oklch, oklcha};

pub struct Palette {
    pub bg: Color,
    pub sidebar: Color,
    pub raised: Color,
    pub input: Color,
    pub hover: Color,
    pub selected: Color,
    pub line: Color,
    pub line_strong: Color,
    pub shadow: Color,
    pub text: Color,
    pub text_body: Color,
    pub text_secondary: Color,
    pub text_faint: Color,
    pub accent: Color,
    pub accent_bright: Color,
    pub accent_deep: Color,
    pub accent_soft: Color,
    pub accent_line: Color,
    pub link: Color,
    pub link_line: Color,
    pub private: Color,
    pub private_soft: Color,
    pub owner: Color,
    pub online: Color,
}

pub fn palette() -> Palette {
    Palette {
        bg: oklch(0.205, 0.012, 255.0),
        sidebar: oklch(0.180, 0.012, 255.0),
        raised: oklch(0.255, 0.014, 255.0),
        input: oklch(0.235, 0.013, 255.0),
        hover: oklcha(1.0, 0.0, 0.0, 0.045),
        selected: oklcha(1.0, 0.0, 0.0, 0.075),
        line: oklcha(1.0, 0.0, 0.0, 0.07),
        line_strong: oklcha(1.0, 0.0, 0.0, 0.11),
        shadow: oklcha(0.0, 0.0, 0.0, 0.28),
        text: oklch(0.96, 0.004, 255.0),
        text_body: oklch(0.90, 0.006, 255.0),
        text_secondary: oklch(0.74, 0.012, 255.0),
        text_faint: oklch(0.58, 0.012, 255.0),
        accent: oklch(0.60, 0.15, 255.0),
        accent_bright: oklch(0.74, 0.13, 250.0),
        accent_deep: oklch(0.50, 0.16, 262.0),
        accent_soft: oklcha(0.60, 0.15, 255.0, 0.20),
        accent_line: oklcha(0.68, 0.14, 255.0, 0.55),
        link: oklch(0.76, 0.12, 245.0),
        link_line: oklcha(0.76, 0.12, 245.0, 0.45),
        private: oklch(0.80, 0.11, 75.0),
        private_soft: oklcha(0.80, 0.11, 75.0, 0.14),
        owner: oklch(0.82, 0.13, 85.0),
        online: oklch(0.74, 0.17, 150.0),
    }
}

pub const BRAND: TextStyle = TextStyle::new(17.0, 600.0);
pub const SECTION: TextStyle = TextStyle::new(11.0, 600.0).tracking(0.6);
pub const ROOM: TextStyle = TextStyle::new(13.5, 500.0);
pub const ROOM_STRONG: TextStyle = TextStyle::new(13.5, 600.0);
pub const HEADER: TextStyle = TextStyle::new(16.0, 600.0);
pub const HEADER_SMALL: TextStyle = TextStyle::new(14.0, 600.0);
pub const TITLE: TextStyle = TextStyle::new(16.0, 600.0);
pub const BODY: TextStyle = TextStyle::new(13.5, 400.0);
pub const NAME: TextStyle = TextStyle::new(14.0, 600.0);
pub const MESSAGE: TextStyle = TextStyle::new(14.5, 400.0).lh(22.0);
pub const SMALL: TextStyle = TextStyle::new(12.0, 400.0);
pub const SMALL_STRONG: TextStyle = TextStyle::new(12.0, 600.0);
pub const BADGE: TextStyle = TextStyle::new(11.0, 600.0);
pub const EMOJI: TextStyle = TextStyle::new(14.0, 400.0);

/// A member's avatar fill, initials colour, and name colour, from their id: one of a fixed set
/// of hues, so a member keeps their colour everywhere they appear.
pub fn avatar_colors(id: u32) -> (Color, Color, Color) {
    const HUES: [f32; 8] = [25.0, 60.0, 145.0, 185.0, 230.0, 265.0, 305.0, 345.0];
    // A multiplicative hash spreads consecutive ids across the hues.
    let hue = HUES[(id.wrapping_mul(2_654_435_761) >> 29) as usize];
    (
        oklch(0.62, 0.13, hue),
        oklch(0.98, 0.02, hue),
        oklch(0.80, 0.11, hue),
    )
}
