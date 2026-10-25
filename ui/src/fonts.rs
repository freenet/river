//! The bundled Lato webfont.
//!
//! The gateway serves the webapp under a CSP whose `default-src` is the node's
//! own origin, so a font CDN is blocked: the files have to ship inside the
//! webapp. They come from `@fontsource/lato` 5.3.0 (`files/`, Regular 400 and
//! Bold 700, the latin and latin-ext subsets), under the SIL Open Font
//! License 1.1 in `assets/fonts/lato/OFL.txt`.
//!
//! The `@font-face` rules are built here rather than in `tailwind.css`,
//! because `asset!()` is what resolves a file's served URL, base path included
//! (`/v1/contract/web/<id>/assets/…` on a node). A `url()` written in CSS
//! would depend on where dx places the compiled stylesheet relative to it.

use dioxus::prelude::*;

const LATIN_400: Asset = asset!("/assets/fonts/lato/lato-latin-400-normal.woff2");
const LATIN_700: Asset = asset!("/assets/fonts/lato/lato-latin-700-normal.woff2");
const LATIN_EXT_400: Asset = asset!("/assets/fonts/lato/lato-latin-ext-400-normal.woff2");
const LATIN_EXT_700: Asset = asset!("/assets/fonts/lato/lato-latin-ext-700-normal.woff2");

/// `unicode-range`s from `@fontsource/lato` 5.3.0's `unicode.json`, so a page
/// that uses only basic Latin never downloads the latin-ext files.
const LATIN_RANGE: &str = "U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,U+02DA,U+02DC,\
U+0304,U+0308,U+0329,U+2000-206F,U+20AC,U+2122,U+2191,U+2193,U+2212,U+2215,U+FEFF,U+FFFD";
const LATIN_EXT_RANGE: &str = "U+0100-02BA,U+02BD-02C5,U+02C7-02CC,U+02CE-02D7,U+02DD-02FF,\
U+0304,U+0308,U+0329,U+1D00-1DBF,U+1E00-1E9F,U+1EF2-1EFF,U+2020,U+20A0-20AB,U+20AD-20C0,\
U+2113,U+2C60-2C7F,U+A720-A7FF";

fn font_face(weight: u16, url: &str, range: &str) -> String {
    format!(
        "@font-face{{font-family:'Lato';font-style:normal;font-weight:{weight};\
         font-display:swap;src:url(\"{url}\") format('woff2');unicode-range:{range};}}"
    )
}

/// The four `@font-face` rules, given each file's served URL.
fn font_faces_css(latin_400: &str, latin_700: &str, ext_400: &str, ext_700: &str) -> String {
    [
        font_face(400, ext_400, LATIN_EXT_RANGE),
        font_face(400, latin_400, LATIN_RANGE),
        font_face(700, ext_700, LATIN_EXT_RANGE),
        font_face(700, latin_700, LATIN_RANGE),
    ]
    .concat()
}

/// Puts the Lato `@font-face` rules in the document head.
#[component]
pub fn LatoFontFaces() -> Element {
    let css = font_faces_css(
        &LATIN_400.to_string(),
        &LATIN_700.to_string(),
        &LATIN_EXT_400.to_string(),
        &LATIN_EXT_700.to_string(),
    );
    rsx! {
        document::Style { "{css}" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_weight_has_a_latin_and_a_latin_ext_face() {
        let css = font_faces_css("a.woff2", "b.woff2", "c.woff2", "d.woff2");
        for (url, weight, range) in [
            ("a.woff2", 400, LATIN_RANGE),
            ("b.woff2", 700, LATIN_RANGE),
            ("c.woff2", 400, LATIN_EXT_RANGE),
            ("d.woff2", 700, LATIN_EXT_RANGE),
        ] {
            let face = css
                .split("@font-face")
                .find(|face| face.contains(url))
                .unwrap_or_else(|| panic!("no face for {url}"));
            assert!(face.contains(&format!("font-weight:{weight};")), "{face}");
            assert!(face.contains(&format!("unicode-range:{range};")), "{face}");
            assert!(face.contains("font-family:'Lato'"), "{face}");
            assert!(face.contains("format('woff2')"), "{face}");
        }
        assert_eq!(css.matches("@font-face").count(), 4);
    }

    /// The OFL requires the licence to travel with the font files.
    #[test]
    fn the_font_licence_ships_beside_the_files() {
        let licence = include_str!("../assets/fonts/lato/OFL.txt");
        assert!(licence.contains("SIL OPEN FONT LICENSE Version 1.1"));
        assert!(licence.contains("Reserved Font Name \"Lato\""));
    }

    /// The stylesheet names the bundled family first, for body text and for
    /// Tailwind's `font-sans` alike.
    #[test]
    fn the_stylesheet_uses_the_bundled_family() {
        let tailwind = include_str!("../assets/tailwind.css");
        assert!(tailwind.contains("--font-sans: 'Lato', system-ui, sans-serif;"));
        assert!(tailwind.contains("font-family: var(--font-sans);"));
    }
}
