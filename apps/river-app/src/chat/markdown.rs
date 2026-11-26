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
/// Most `[` in one text: a reference definition used many times expands a short text into
/// many link records (the web UI bounds its HTML size instead).
const MAX_BRACKETS: usize = 100;
/// Most `|` on one line, which bounds the columns of a table.
const MAX_LINE_PIPES: usize = 32;

/// Whether `text` is cheap enough to parse as markdown. The parser's time grows faster than
/// linearly with nesting depth and a table's size with columns times rows, so text past these
/// limits is shown as plain text, as in the web UI.
pub fn is_cheap(text: &str) -> bool {
    if text.len() > MAX_SOURCE_BYTES || text.bytes().filter(|&b| b == b'[').count() > MAX_BRACKETS {
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
    strip_with(text, true)
}

/// [`strip`], with `keep_html` false to leave out raw HTML, which the message renderer does not
/// draw (message search compares what is drawn).
pub fn strip_with(text: &str, keep_html: bool) -> String {
    use pulldown_cmark::{Event, Options, Parser, TagEnd};
    if !is_cheap(text) {
        return text.to_owned();
    }
    // An image is drawn as its words (`images_to_text`); read it the same way.
    let text = &*images_to_text(text);
    let mut out = String::with_capacity(text.len());
    let gap = |out: &mut String| {
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
    };
    for ev in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES) {
        match ev {
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            Event::Html(t) | Event::InlineHtml(t) if keep_html => out.push_str(&t),
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

/// `src` with each markdown image (`![alt](src)`, reference-style, inside a link or a table)
/// rewritten as plain text: its alt text and, after it in parentheses, its address. Images are
/// not displayed in messages, as in the web UI: nothing is fetched or drawn for one. The
/// address is left out when it names a scheme other than http(s) (`data:`, `javascript:`),
/// as the web UI does. Raw HTML (`<img>`) is never drawn by the widget, so needs nothing here.
/// The text is escaped, so it reads back as text and cannot start new markup.
pub(super) fn images_to_text(src: &str) -> std::borrow::Cow<'_, str> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let mut out = String::new();
    let mut copied = 0;
    // The open image's range, address and alt text, and how deep inside it we are.
    let mut open: Option<(std::ops::Range<usize>, String, String)> = None;
    let mut depth = 0usize;
    for (ev, range) in Parser::new_ext(src, super::links::options()).into_offset_iter() {
        match ev {
            Event::Start(Tag::Image { dest_url, .. }) => {
                if depth == 0 {
                    open = Some((range, dest_url.into_string(), String::new()));
                }
                depth += 1;
            }
            Event::End(TagEnd::Image) if depth > 0 => {
                depth -= 1;
                if depth == 0
                    && let Some((range, url, alt)) = open.take()
                {
                    // Ranges of images that are not nested only ever move forward.
                    if range.start < copied {
                        continue;
                    }
                    out.push_str(&src[copied..range.start]);
                    let alt = alt.split_whitespace().collect::<Vec<_>>().join(" ");
                    let url = url.trim();
                    let words = match (alt.is_empty(), url.is_empty() || !shown_address(url)) {
                        (false, false) => format!("{alt} ({url})"),
                        (false, true) => alt,
                        (true, false) => url.to_owned(),
                        (true, true) => String::new(),
                    };
                    escape_into(&words, &mut out);
                    copied = range.end;
                }
            }
            Event::Text(t) | Event::Code(t) if depth > 0 => {
                if let Some((_, _, alt)) = open.as_mut() {
                    alt.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak if depth > 0 => {
                if let Some((_, _, alt)) = open.as_mut() {
                    alt.push(' ');
                }
            }
            _ => {}
        }
    }
    if copied == 0 {
        return src.into();
    }
    out.push_str(&src[copied..]);
    out.into()
}

/// Whether the widget's parser finds an image in `src`.
fn has_image(src: &str) -> bool {
    use pulldown_cmark::{Event, Parser, Tag};
    Parser::new_ext(src, super::links::options())
        .any(|e| matches!(e, Event::Start(Tag::Image { .. })))
}

/// Whether an image's address is shown beside its alt text: one with no scheme (relative)
/// or an http(s) one, as the web UI's renderer keeps.
fn shown_address(url: &str) -> bool {
    let scheme = url
        .split_once(':')
        .map(|(s, _)| s)
        .filter(|s| !s.contains(['/', '?', '#']));
    match scheme {
        None => !url.chars().any(char::is_control),
        Some(s) => {
            (s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https"))
                && !url.chars().any(char::is_control)
        }
    }
}

/// `text` with every ASCII punctuation character backslash-escaped, so markdown reads it back
/// as the same text.
fn escape_into(text: &str, out: &mut String) {
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
}

/// Text with each line break kept, as chat messages are written (two spaces before a newline
/// is markdown's hard break). Descriptions are not given this, as in the web UI.
fn keep_line_breaks(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "  \n")
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

/// The mention kind named by a chip link, `None` for any other link. A chip is
/// `<scheme>:<kind>` or, for a member this room knows, `<scheme>:<kind>:<member uid>`.
fn mention_kind(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix(model::mention_scheme())?
        .strip_prefix(':')?;
    Some(rest.split_once(':').map_or(rest, |(kind, _)| kind))
}

/// The member a chip link names, `None` for any other link and for a chip of someone this room
/// does not know. Only a chip written with this process's scheme names anyone.
pub(crate) fn mention_member(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix(model::mention_scheme())?
        .strip_prefix(':')?;
    let (kind, uid) = rest.split_once(':')?;
    (matches!(kind, "self" | "member") && !uid.is_empty()).then_some(uid)
}

fn style(p: &Palette, k: &Kit, font: rune::tok::TextStyle, color: Color) -> MarkdownStyle {
    MarkdownStyle::from_kit(k)
        .text(color)
        .muted(p.text_muted)
        .link(p.accent)
        .link_underline(p.accent_line)
        .body_font(font)
}

/// A horizontal rule: three or more `-`, `*` or `_` and spaces.
fn thematic(t: &str) -> bool {
    let t = t.trim();
    t.chars().filter(|c| !c.is_whitespace()).count() >= 3
        && t.chars().all(|c| matches!(c, '-' | '*' | '_' | ' '))
}

/// The inline markdown of a description's first line, for the header's one line: the line
/// without the block marker that opens it (`#`, `-`, `*`, `+`, `>`, `1.`), so a heading or a list
/// item shows its words with their links, and a line that starts with bold text is not taken
/// for a list. `None` for a line that is itself a block (fence, table, html, image) or empty.
pub fn header_inline(line: &str) -> Option<&str> {
    // Four spaces or a tab of indent make code, not prose.
    if line.starts_with("    ") || line.starts_with('\t') {
        return None;
    }
    let mut t = line.trim();
    if t.contains(['\n', '\r']) {
        return None;
    }
    // A marker is followed by a space; `**bold**` and `-5` are text.
    fn after(rest: &str) -> Option<&str> {
        rest.strip_prefix([' ', '\t']).map(str::trim_start)
    }
    loop {
        let next = if let Some(rest) = t.strip_prefix('#') {
            after(rest.trim_start_matches('#'))
        } else if let Some(rest) = t.strip_prefix('>') {
            Some(rest.trim_start())
        } else if let Some(rest) = t.strip_prefix(['-', '*', '+']) {
            after(rest)
        } else if t.starts_with(|c: char| c.is_ascii_digit()) {
            let digits = t.trim_start_matches(|c: char| c.is_ascii_digit());
            digits.strip_prefix(['.', ')']).and_then(after)
        } else {
            None
        };
        match next {
            Some(n) => t = n,
            None => break,
        }
    }
    // A marker with nothing after it (`-`, `#`, `1.`) is no text.
    let marker_only = t
        .chars()
        .all(|c| matches!(c, '-' | '*' | '+' | '#' | '>' | '.' | ')') || c.is_ascii_digit());
    (!t.is_empty()
        && !marker_only
        && !thematic(line)
        && !thematic(t)
        && !t.starts_with(['`', '~', '<', '|', '!']))
    .then_some(t)
}

/// Markdown for `src` under `key`. `keep_breaks` is true for messages. `None` when the text
/// cannot be made safe to draw as markdown (see `links::prepare`): show it as plain text.
#[allow(clippy::too_many_arguments)] // one call site per kind of text; a struct would only rename them
pub fn render(
    src: &str,
    keep_breaks: bool,
    font: rune::tok::TextStyle,
    color: Color,
    key: String,
    p: &Palette,
    k: &Kit,
    selectable: bool,
) -> Option<El> {
    let src = if keep_breaks {
        keep_line_breaks(src)
    } else {
        src.to_owned()
    };
    // Links to Freenet apps behave as in the web UI (see `links`).
    // The line breaks added bytes, so the text may no longer be cheap; the unprepared source
    // must never reach the widget.
    if !is_cheap(&src) {
        return None;
    }
    // Images are not displayed in messages: each is rewritten as its words, and a text in
    // which the parser would still find one is shown as plain text.
    let src = images_to_text(&src);
    if !is_cheap(&src) || has_image(&src) {
        return None;
    }
    let src = super::links::prepare(&src)?;
    let (accent, soft, line, muted) = (p.accent, p.accent_soft, p.accent_line, p.text_muted);
    Some(
        Markdown::new(src)
            // Nothing is linked unless `links::prepare` wrote it as a link, so a bare URL
            // it has not checked is only text.
            .autolink(false)
            .on_link_tx(|s: &mut RiverApp, url: &str, tx: &mut rune::Tx| {
                if let Some(uid) = mention_member(url) {
                    // A mention chip opens the member's dialog, as in the web UI.
                    s.chat.open_member(uid);
                } else if openable(url) {
                    let _ = tx.open_url(url.trim());
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
            // Right-click a link for Open and Copy link, as in a browser. This takes the press from
            // every link, so a mention chip or a non-web link shows no menu at all.
            .on_link_secondary(
                |s: &mut RiverApp, url: &str, press: rune::PointerPress, _: &mut rune::Tx| {
                    s.chat.open_link_menu(url, press);
                },
            )
            // Text can be selected and copied. A message drawn in the history joins the history's
            // selection group (conversation.rs), so a drag can run on into the next message.
            // The header's line is a press target (it opens the room details), and a press
            // on selectable text starts a selection instead.
            .selectable(selectable)
            .render(&style(p, k, font, color))
            .key(key),
    )
}

/// Every way of writing an image, alone and inside the blocks and inlines that keep an
/// image in its line (heading, table cell, link, emphasis, quote, list).
#[cfg(test)]
pub(super) const IMAGE_VARIANTS: &[&str] = &[
    "![alt text](https://example.com/a.png)",
    "![alt text](https://example.com/a.png \"title\")",
    "![alt text][ref]\n\n[ref]: https://example.com/a.png",
    "![ref]\n\n[ref]: https://example.com/a.png",
    "![ref][]\n\n[ref]: https://example.com/a.png",
    "![](https://example.com/a.png)",
    "![alt <b>x</b>](<https://example.com/a b.png>)",
    "[![alt text](https://example.com/a.png)](https://example.com/page)",
    "![outer ![inner](https://example.com/i.png)](https://example.com/o.png)",
    "![alt text](data:image/png;base64,iVBORw0KGgo=)",
    "![alt text](javascript:alert(1))",
    "![alt text](file:///etc/passwd)",
    "![alt text](/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/a.png)",
    "<img src=\"https://example.com/a.png\">",
    "a <img src=\"https://example.com/a.png\"> b",
    "<picture><img src=\"https://example.com/a.png\"></picture>",
    "<svg><image href=\"https://example.com/a.png\"/></svg>",
    "# ![alt text](https://example.com/a.png)",
    "> ![alt text](https://example.com/a.png)",
    "- ![alt text](https://example.com/a.png)",
    "*![alt text](https://example.com/a.png)*",
    "| a |\n|---|\n| ![alt text](https://example.com/a.png) |",
    "text\n![alt text](https://example.com/a.png)\nmore",
];

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
        assert_eq!(mention_member(&real), None);
        let with_uid = format!("{}:member:ABCD2345", model::mention_scheme());
        assert_eq!(mention_kind(&with_uid), Some("member"));
        assert_eq!(mention_member(&with_uid), Some("ABCD2345"));
        assert_eq!(mention_member("rvm-0000000000000000:member:ABCD2345"), None);
        let unknown = format!("{}:unknown:ABCD2345", model::mention_scheme());
        assert_eq!(mention_member(&unknown), None);
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
    fn many_brackets_are_not_parsed() {
        assert!(is_cheap(&"[a] ".repeat(MAX_BRACKETS)));
        assert!(!is_cheap(&"[a] ".repeat(MAX_BRACKETS + 1)));
    }

    #[test]
    fn only_a_plain_line_is_a_header_description() {
        let ok = |s| header_inline(s);
        assert_eq!(ok("The **public** room"), Some("The **public** room"));
        assert_eq!(
            ok("**Welcome** to [x](https://a.b)"),
            Some("**Welcome** to [x](https://a.b)")
        );
        assert_eq!(
            ok("[Freenet](https://freenet.org) is it"),
            Some("[Freenet](https://freenet.org) is it")
        );
        // A block marker is dropped, its words kept.
        assert_eq!(ok("# Title"), Some("Title"));
        assert_eq!(ok("- a [x](https://a.b)"), Some("a [x](https://a.b)"));
        assert_eq!(ok("> q"), Some("q"));
        assert_eq!(ok("1. a"), Some("a"));
        assert_eq!(ok("-5 degrees"), Some("-5 degrees"));
        for no in [
            "```",
            "a\nb",
            "| a |",
            "***",
            "- - -",
            "",
            "<b>x",
            "![i](x)",
            "-",
            "#",
            "1.",
            "> -",
            "    indented code",
            "\tcode",
            "- | a | b |",
            "> | a |",
            "\r",
        ] {
            assert_eq!(ok(no), None, "{no:?}");
        }
        assert_eq!(strip("a <b>html</b> c"), "a <b>html</b> c");
    }

    #[test]
    fn markdown_that_cannot_be_made_safe_is_not_rendered() {
        let p = super::super::style::palette();
        let k = Kit::neutral(true);
        let draw = |src: &str| {
            render(
                src,
                true,
                super::super::style::MESSAGE,
                p.text,
                "k".into(),
                &p,
                &k,
                true,
            )
        };
        assert!(draw("hello [a](https://example.com/)").is_some());
        let over = "<ab:c> ".repeat(60);
        assert!(is_cheap(&keep_line_breaks(&over)));
        assert!(draw(&over).is_none());
    }

    #[test]
    fn no_image_survives_the_rewrite() {
        for src in IMAGE_VARIANTS {
            for src in [src.to_string(), keep_line_breaks(src)] {
                let out = images_to_text(&src);
                assert!(!has_image(&out), "{src:?} -> {out:?}");
            }
        }
    }

    #[test]
    fn an_image_reads_as_its_alt_text_and_address() {
        let read = |s: &str| strip(&images_to_text(s));
        assert_eq!(
            read("see ![a cat](https://example.com/cat.png) here"),
            "see a cat (https://example.com/cat.png) here"
        );
        assert_eq!(
            read("![](https://example.com/cat.png)"),
            "https://example.com/cat.png"
        );
        // A relative address is shown; one with any other scheme leaves the alt text only.
        assert_eq!(read("![x](/a.png)"), "x (/a.png)");
        assert_eq!(read("![only alt](javascript:alert(1))"), "only alt");
        assert_eq!(read("![only alt](data:image/png;base64,AAAA)"), "only alt");
        assert_eq!(read("![](data:image/png;base64,AAAA)"), "");
        // Markup in the alt text and the address is shown as written, not as markup.
        assert_eq!(
            read("![*a* [b]](https://e.example/x_y_.png)"),
            "a [b] (https://e.example/x_y_.png)"
        );
        // A reference image uses its definition's address.
        assert_eq!(
            read("![r]\n\n[r]: https://example.com/a.png"),
            "r (https://example.com/a.png)"
        );
        // A linked image keeps its link, with the image's words as the label.
        assert_eq!(
            images_to_text("[![a](https://e.example/i.png)](https://e.example/p)"),
            "[a \\(https\\:\\/\\/e\\.example\\/i\\.png\\)](https://e.example/p)"
        );
        // Text without an image is returned as it is.
        assert_eq!(
            images_to_text("plain **text** [l](https://x.example)"),
            "plain **text** [l](https://x.example)"
        );
    }

    #[test]
    fn previews_and_search_show_an_image_as_drawn() {
        assert_eq!(
            strip("look ![a cat](https://example.com/cat.png)"),
            "look a cat (https://example.com/cat.png)"
        );
        assert_eq!(strip_with("![x](javascript:y)", false), "x");
    }

    #[test]
    fn messages_keep_their_line_breaks() {
        assert_eq!(keep_line_breaks("a\nb\r\nc\rd"), "a  \nb  \nc  \nd");
    }
}
