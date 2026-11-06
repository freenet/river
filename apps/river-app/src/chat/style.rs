//! The chat screen's colours and type, taken from the web UI's dark theme
//! (`ui/assets/tailwind.css`) so the two look like one product.

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
    /// Rows of messages this device sent.
    pub own_row: Color,
    pub line: Color,
    pub line_strong: Color,
    pub shadow: Color,
    pub text: Color,
    pub text_secondary: Color,
    pub text_muted: Color,
    pub quote: Color,
    pub accent: Color,
    /// The accent at 15% over the conversation column (badges, own reactions).
    pub accent_soft: Color,
    pub accent_line: Color,
    pub success: Color,
}

fn hex(s: &str) -> Color {
    rune::color::hex(s).expect("a valid colour")
}

pub fn palette() -> Palette {
    Palette {
        bg: hex("#141416"),
        rail: hex("#0f0f11"),
        surface: hex("#1e1e21"),
        surface_hover: hex("#28282c"),
        row_hover: hex("#18181b"),
        own_row: hex("#1c1d22"),
        line: hex("#2a2a2e"),
        line_strong: hex("#3a3a3e"),
        shadow: Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.35,
        },
        text: hex("#f0f0f2"),
        text_secondary: hex("#b4b4bb"),
        text_muted: hex("#9a9a9e"),
        quote: hex("#b4b4b9"),
        accent: hex("#97b2e1"),
        accent_soft: hex("#282c34"),
        accent_line: hex("#5d6f8f"),
        success: hex("#47cd89"),
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
pub const SMALL: TextStyle = TextStyle::new(12.0, 400.0);
pub const SMALL_STRONG: TextStyle = TextStyle::new(12.5, 600.0);
pub const TAG: TextStyle = TextStyle::new(10.5, 700.0).tracking(0.3);
pub const BADGE: TextStyle = TextStyle::new(11.5, 700.0);
pub const EMOJI: TextStyle = TextStyle::new(15.0, 400.0);

/// A member's avatar disc and initials colours. The web UI hashes the member's key with a
/// per-device salt and rounds the result to one of ten hues; this keeps the ten hues and the
/// colours, and picks the hue from the member id.
pub fn avatar_colors(id: u32) -> (Color, Color) {
    // Multiplying by 7 (coprime with 10) gives consecutive ids different hues.
    let hue = id.wrapping_mul(7) % 10;
    let h = hue as f32 * 36.0;
    (oklch(0.42, 0.07, h), oklch(0.93, 0.045, h))
}
