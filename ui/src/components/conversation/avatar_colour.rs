//! Another member's avatar colour, from their member id: a hue anywhere on
//! the wheel and one of three lightness steps. The lightness band and chroma
//! per theme are CSS tokens (`--avatar-l`, `--avatar-l-step`, `--avatar-c` in
//! `tailwind.css`); `.msg-avatar-hashed` in `main.css` combines them with the
//! `--avatar-hue` / `--avatar-tone` the row sets inline.

use river_core::room_state::member::MemberId;

/// Hue (0..360) and lightness step (0..3) for another member's avatar, from
/// the member id. The id rather than the nickname, so a rename keeps the
/// colour and an exact-name impersonator does not inherit it.
pub(super) fn avatar_hue_tone(author_id: MemberId) -> (u16, u8) {
    let digest = blake3::hash(&author_id.0 .0.to_le_bytes());
    let d = digest.as_bytes();
    let hue = u32::from_le_bytes([d[0], d[1], d[2], d[3]]) % 360;
    let tone = d[4] % 3;
    (hue as u16, tone)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins each member's (hue, tone), so a change to the hash input, the
    /// byte order or the reduction (any of which would repaint every member)
    /// shows up here. Values recorded from the first passing run: a pin, not
    /// an oracle.
    #[test]
    fn avatar_hue_tone_is_pinned_per_member_id() {
        let cases: [(i64, (u16, u8)); 6] = [
            (0, (321, 1)),
            (1, (178, 0)),
            (-1, (123, 2)),
            (42, (202, 2)),
            (i64::MIN, (350, 0)),
            (i64::MAX, (287, 2)),
        ];
        for (id, expected) in cases {
            assert_eq!(
                avatar_hue_tone(MemberId(freenet_scaffold::util::FastHash(id))),
                expected,
                "member id {id}"
            );
        }
    }

    /// One theme's avatar tokens and the surfaces an avatar sits on, as
    /// `tailwind.css` declares them.
    struct Theme {
        name: &'static str,
        /// `--avatar-l`: lightness at tone 0.
        l: f64,
        /// `--avatar-l-step`: lightness added per tone.
        l_step: f64,
        /// `--avatar-c`: chroma requested before clipping to sRGB.
        c: f64,
        /// `--color-bg`.
        bg: [u8; 3],
        /// `--color-surface`.
        surface: [u8; 3],
        /// `--color-row-hover`'s share of `--color-surface` over `bg`.
        hover_mix: f64,
    }

    const TAILWIND_CSS: &str = include_str!("../../../assets/tailwind.css");

    /// The value of `token` in a `:root` block, up to its `;`.
    fn token<'a>(block: &'a str, theme: &str, token: &str) -> &'a str {
        let decl = format!("{token}: ");
        let from = block
            .find(&decl)
            .unwrap_or_else(|| panic!("{theme} :root is missing `{token}`"))
            + decl.len();
        block[from..][..block[from..].find(';').expect("declaration ends")].trim()
    }

    fn number(block: &str, theme: &str, name: &str) -> f64 {
        let value = token(block, theme, name);
        value
            .parse()
            .unwrap_or_else(|_| panic!("{theme} `{name}: {value}` is not a number"))
    }

    fn hex(block: &str, theme: &str, name: &str) -> [u8; 3] {
        let value = token(block, theme, name);
        let digits = value
            .strip_prefix('#')
            .filter(|d| d.len() == 6)
            .unwrap_or_else(|| panic!("{theme} `{name}: {value}` is not #rrggbb"));
        std::array::from_fn(|i| u8::from_str_radix(&digits[2 * i..2 * i + 2], 16).unwrap())
    }

    /// Both themes, read from `tailwind.css`. The dark block inherits the
    /// light `--color-row-hover`, which resolves against the dark surface.
    fn themes() -> [Theme; 2] {
        let light = block_after(TAILWIND_CSS, "/* Light mode colors (default) */\n:root");
        let dark = block_after(
            TAILWIND_CSS,
            "@media (prefers-color-scheme: dark) {\n  :root",
        );
        let row_hover = token(light, "light", "--color-row-hover");
        let hover_mix = row_hover
            .strip_prefix("color-mix(in srgb, var(--color-surface) ")
            .and_then(|rest| rest.strip_suffix("%, transparent)"))
            .and_then(|pct| pct.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("unexpected `--color-row-hover: {row_hover}`"))
            / 100.0;
        [("light", light), ("dark", dark)].map(|(name, block)| Theme {
            name,
            l: number(block, name, "--avatar-l"),
            l_step: number(block, name, "--avatar-l-step"),
            c: number(block, name, "--avatar-c"),
            bg: hex(block, name, "--color-bg"),
            surface: hex(block, name, "--color-surface"),
            hover_mix,
        })
    }

    /// WCAG 1.4.11 minimum for non-text graphics.
    const MIN_CONTRAST: f64 = 3.0;
    /// Clipped chroma floor. Grey reads below about 0.03.
    const MIN_CHROMA: f64 = 0.08;

    /// OKLab to linear sRGB (Ottosson's matrices), unclipped.
    fn oklab_to_linear_srgb(l: f64, a: f64, b: f64) -> [f64; 3] {
        let l_ = l + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
        let m_ = l - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
        let s_ = l - 0.089_484_177_5 * a - 1.291_485_548_0 * b;
        let (l3, m3, s3) = (l_.powi(3), m_.powi(3), s_.powi(3));
        [
            4.076_741_662_1 * l3 - 3.307_711_591_3 * m3 + 0.230_969_929_2 * s3,
            -1.268_438_004_6 * l3 + 2.609_757_401_1 * m3 - 0.341_319_396_5 * s3,
            -0.004_196_086_3 * l3 - 0.703_418_614_7 * m3 + 1.707_614_701_0 * s3,
        ]
    }

    /// Linear sRGB to OKLab, for the chroma left after clipping.
    fn linear_srgb_to_oklab([r, g, b]: [f64; 3]) -> [f64; 3] {
        let l = 0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b;
        let m = 0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b;
        let s = 0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b;
        let (l_, m_, s_) = (l.cbrt(), m.cbrt(), s.cbrt());
        [
            0.210_454_255_3 * l_ + 0.793_617_785_0 * m_ - 0.004_072_046_8 * s_,
            1.977_998_495_1 * l_ - 2.428_592_205_0 * m_ + 0.450_593_709_9 * s_,
            0.025_904_037_1 * l_ + 0.782_771_766_2 * m_ - 0.808_675_766_0 * s_,
        ]
    }

    /// An `oklch()` colour as the browser paints it on an sRGB screen: each
    /// linear channel clipped to [0, 1].
    fn oklch_clipped_linear(l: f64, c: f64, hue_deg: f64) -> [f64; 3] {
        let h = hue_deg.to_radians();
        oklab_to_linear_srgb(l, c * h.cos(), c * h.sin()).map(|v| v.clamp(0.0, 1.0))
    }

    fn srgb_to_linear(v: f64) -> f64 {
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG relative luminance of linear sRGB.
    fn luminance([r, g, b]: [f64; 3]) -> f64 {
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn srgb8_luminance(rgb: [f64; 3]) -> f64 {
        luminance(rgb.map(|v| srgb_to_linear(v / 255.0)))
    }

    fn contrast(a: f64, b: f64) -> f64 {
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The worst (contrast, hue, tone) and the lowest clipped chroma over
    /// every hue and tone, against the background and the hover band.
    fn worst_case(theme: &Theme) -> ((f64, u16, u8), (f64, u16, u8)) {
        let bg = theme.bg.map(f64::from);
        // `color-mix(in srgb, surface N%, transparent)` over bg: an alpha
        // composite of the encoded channels.
        let hover: [f64; 3] = std::array::from_fn(|i| {
            theme.hover_mix * f64::from(theme.surface[i]) + (1.0 - theme.hover_mix) * bg[i]
        });
        let surfaces = [srgb8_luminance(bg), srgb8_luminance(hover)];
        let mut worst_contrast = (f64::INFINITY, 0, 0);
        let mut worst_chroma = (f64::INFINITY, 0, 0);
        for hue in 0..360u16 {
            for tone in 0..3u8 {
                let l = theme.l + f64::from(tone) * theme.l_step;
                let rgb = oklch_clipped_linear(l, theme.c, f64::from(hue));
                let y = luminance(rgb);
                for surface in surfaces {
                    let ratio = contrast(y, surface);
                    if ratio < worst_contrast.0 {
                        worst_contrast = (ratio, hue, tone);
                    }
                }
                let [_, a, b] = linear_srgb_to_oklab(rgb);
                let chroma = a.hypot(b);
                if chroma < worst_chroma.0 {
                    worst_chroma = (chroma, hue, tone);
                }
            }
        }
        (worst_contrast, worst_chroma)
    }

    /// Every hue and tone, in both themes, clears WCAG's 3:1 for graphics
    /// against the conversation background and the row hover band, and keeps
    /// enough chroma after sRGB clipping not to read as grey. This checks the
    /// token values, not the hash: any hash output is covered.
    #[test]
    fn every_avatar_colour_clears_contrast_and_stays_chromatic() {
        for theme in &themes() {
            let ((ratio, hue, tone), (chroma, c_hue, c_tone)) = worst_case(theme);
            assert!(
                ratio >= MIN_CONTRAST,
                "{}: contrast {ratio:.2}:1 at hue {hue} tone {tone}",
                theme.name
            );
            assert!(
                chroma >= MIN_CHROMA,
                "{}: chroma {chroma:.3} at hue {c_hue} tone {c_tone}",
                theme.name
            );
        }
    }

    /// The body of the first `{ ... }` block after `start` (no nesting).
    fn block_after<'a>(css: &'a str, start: &str) -> &'a str {
        let from = css
            .find(start)
            .unwrap_or_else(|| panic!("missing `{start}`"));
        let open = from + css[from..].find('{').expect("block opens") + 1;
        let close = open + css[open..].find('}').expect("block closes");
        &css[open..close]
    }

    /// The formula `worst_case` models; change both together.
    #[test]
    fn main_css_paints_the_modelled_formula() {
        let css = include_str!("../../../assets/main.css");
        let block = block_after(css, ".msg-avatar-hashed {");
        assert!(
            block.contains(
                "color: oklch(calc(var(--avatar-l) + var(--avatar-tone) * var(--avatar-l-step)) var(--avatar-c) var(--avatar-hue));"
            ),
            "`.msg-avatar-hashed` no longer paints the colour the contrast sweep checks:\n{block}"
        );
    }
}
