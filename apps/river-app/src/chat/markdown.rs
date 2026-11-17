//! Message and room-description markdown, drawn with the same rules as the web UI
//! (`ui/src/components/conversation/markdown.rs`): GFM, a message's line breaks kept,
//! mention chips, plain text for input too costly to parse, and links that only ever open
//! over http(s).

use rune::{Color, El, Span};
use rune_kit::{Kit, Markdown, MarkdownStyle};

use super::model;
use super::style::Palette;
use crate::RiverApp;

/// Longest text rendered as markdown (the web UI's `MARKDOWN_MAX_SOURCE_BYTES`).
const MAX_SOURCE_BYTES: usize = 4096;
/// Most block containers (`>`, `-`, `1.`) opened at the start of one line.
const MAX_LINE_NESTING: usize = 16;
/// Most `|` on one line, which bounds the columns of a table.
const MAX_LINE_PIPES: usize = 32;

/// Whether `text` is cheap enough to parse as markdown. The parser's time grows faster than
/// linearly with nesting depth and a table's size with columns times rows, so text past these
/// limits is shown as plain text, as in the web UI.
pub fn is_cheap(text: &str) -> bool {
    if text.len() > MAX_SOURCE_BYTES {
        return false;
    }
    // A byte order mark is skipped by the parser but would hide a marker from the count.
    let text: std::borrow::Cow<str> = if text.contains('\u{feff}') {
        text.replace('\u{feff}', "").into()
    } else {
        text.into()
    };
    // A lone `\r` also ends a line in markdown.
    text.split(['\n', '\r']).all(|line| {
        container_depth(line) <= MAX_LINE_NESTING
            && line.bytes().filter(|&b| b == b'|').count() <= MAX_LINE_PIPES
    })
}

/// How many block quote, list item or footnote definition markers open `line` (an over-count
/// only makes plain text more likely).
fn container_depth(line: &str) -> usize {
    let bytes = line.as_bytes();
    let (mut i, mut depth) = (0, 0);
    loop {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let marker_end = match bytes.get(i) {
            Some(b'>') => i + 1,
            Some(b'[') if bytes.get(i + 1) == Some(&b'^') => match line[i..].find("]:") {
                Some(end) => {
                    depth += 1;
                    i += end + 2;
                    continue;
                }
                None => return depth,
            },
            Some(b'-' | b'*' | b'+') => i + 1,
            Some(b'0'..=b'9') => {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                match bytes.get(j) {
                    Some(b'.' | b')') => j + 1,
                    _ => return depth,
                }
            }
            _ => return depth,
        };
        let is_quote = bytes[i] == b'>';
        if !is_quote && !matches!(bytes.get(marker_end), None | Some(b' ' | b'\t')) {
            return depth;
        }
        depth += 1;
        i = marker_end;
    }
}

/// `text` as plain words, for a one-line preview: the markup dropped, link text and code kept,
/// each block and line break a space. Text too costly to parse is returned as it is.
pub fn strip(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, TagEnd};
    if !is_cheap(text) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let gap = |out: &mut String| {
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
    };
    for ev in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES) {
        match ev {
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            Event::SoftBreak | Event::HardBreak => gap(&mut out),
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::Item
                | TagEnd::CodeBlock
                | TagEnd::TableCell
                | TagEnd::BlockQuote(_),
            ) => gap(&mut out),
            _ => {}
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Text with each line break kept, as chat messages are written (two spaces before a newline
/// is markdown's hard break). Descriptions are not given this, as in the web UI.
fn keep_line_breaks(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "  \n")
}

/// Whether a link may be opened from the app: only web pages. A markdown link can name any
/// scheme (`file:`, `javascript:`, an app's own handler); none of those is opened.
pub fn openable(url: &str) -> bool {
    let u = url.trim();
    u.len() > 8
        && (u
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
            || u.get(..8)
                .is_some_and(|s| s.eq_ignore_ascii_case("https://")))
        && !u.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// The mention kind named by a chip link, `None` for any other link.
fn mention_kind(url: &str) -> Option<&str> {
    url.strip_prefix(model::mention_scheme())?.strip_prefix(':')
}

fn style(p: &Palette, k: &Kit, font: rune::tok::TextStyle, color: Color) -> MarkdownStyle {
    MarkdownStyle::from_kit(k)
        .text(color)
        .muted(p.text_muted)
        .link(p.accent)
        .link_underline(p.accent_line)
        .body_font(font)
}

/// Markdown for `src` under `key`. `keep_breaks` is true for messages.
pub fn render(
    src: &str,
    keep_breaks: bool,
    font: rune::tok::TextStyle,
    color: Color,
    key: String,
    p: &Palette,
    k: &Kit,
) -> El {
    let src = if keep_breaks {
        keep_line_breaks(src)
    } else {
        src.to_owned()
    };
    let (accent, soft, line, muted) = (p.accent, p.accent_soft, p.accent_line, p.text_muted);
    Markdown::new(src)
        .on_link_tx(|_: &mut RiverApp, url: &str, tx: &mut rune::Tx| {
            if openable(url) {
                let _ = tx.open_url(url);
            }
        })
        .link_style(move |url| {
            let chip = |bg: Color, fg: Color| {
                (
                    Span::new(0, 0).bold().color(fg).bg(bg, 2.0),
                    rune::LinkPaint::new().bg(line),
                )
            };
            match mention_kind(url)? {
                // A mention of you stands out, as the web UI's self chip does.
                "self" => Some(chip(line, accent)),
                "member" => Some(chip(soft, accent)),
                _ => Some(chip(soft, muted)),
            }
        })
        .link_help(|url| openable(url).then(|| format!("Opens {url}")))
        .render(&style(p, k, font, color))
        .key(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_pages_open() {
        for ok in [
            "https://freenet.org/",
            "http://127.0.0.1:7509/x",
            "HTTPS://A.B/c",
        ] {
            assert!(openable(ok), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://x.example/",
            "https://",
            "https://a b/",
            "https://a\u{0}b/",
            "//evil.example/",
            "freenet:abc",
            "",
        ] {
            assert!(!openable(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_forged_chip_is_just_a_link() {
        assert_eq!(mention_kind("rvm-0000000000000000:self"), None);
        assert_eq!(mention_kind("rv:abc"), None);
        let real = format!("{}:member", model::mention_scheme());
        assert_eq!(mention_kind(&real), Some("member"));
    }

    #[test]
    fn costly_text_is_not_parsed() {
        assert!(is_cheap("hello **world**\n- a\n- b"));
        assert!(!is_cheap(&"a".repeat(MAX_SOURCE_BYTES + 1)));
        assert!(!is_cheap(&format!(
            "{}x",
            "> ".repeat(MAX_LINE_NESTING + 1)
        )));
        assert!(is_cheap(&format!("{}x", "> ".repeat(MAX_LINE_NESTING))));
        assert!(!is_cheap(&"|".repeat(MAX_LINE_PIPES + 1)));
        // A byte order mark between markers cannot hide them from the count.
        assert!(!is_cheap(&format!(
            "{}x",
            "> \u{feff}".repeat(MAX_LINE_NESTING + 1)
        )));
    }

    #[test]
    fn previews_drop_the_markup() {
        assert_eq!(
            strip("**Bold** and `code` and [a link](https://x.example)"),
            "Bold and code and a link"
        );
        assert_eq!(strip("```rust\nfn main() {}\n```"), "fn main() {}");
        assert_eq!(strip("# Title\n\n- one\n- two"), "Title one two");
        assert_eq!(strip("a\nb"), "a b");
        assert_eq!(
            strip("snake_case_name and 2 * 3 * 4"),
            "snake_case_name and 2 * 3 * 4"
        );
        assert_eq!(strip(&"> ".repeat(40)), "> ".repeat(40));
    }

    #[test]
    fn messages_keep_their_line_breaks() {
        assert_eq!(keep_line_breaks("a\nb\r\nc"), "a  \nb  \nc");
    }
}
