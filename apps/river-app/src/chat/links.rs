//! Links to Freenet apps in message and description markdown, handled as the web UI does
//! (`ui/src/components/conversation/markdown.rs`, `ui/src/util/share_link.rs`).
//!
//! [`prepare`] runs on the markdown source before it reaches the widget and rewrites it so the
//! widget only ever draws links that are safe and that go where they say:
//!
//! 1. A share link (`https://freenet.org/open#<id><rest>`, `freenet:<id>`, `freenet://<id>`)
//!    that is the visible text of a link, or bare in prose, opens that webapp on the READER'S
//!    node (`http://127.0.0.1:7509/v1/contract/web/<id><rest>`). The destination comes only
//!    from the visible text, never from a hidden href.
//! 2. A Freenet web-contract URL on any host (`http(s)://host/v1|v2/contract/web/<id>/...`), or
//!    a relative `/v1|v2/contract/web/<id>` href, is rewritten to the reader's node. A path
//!    with a dot segment (literal or percent-encoded) is never rewritten.
//! 3. A link whose destination opens a contract on the node but whose label visibly names a
//!    DIFFERENT contract is unlinked: its label is shown as plain text.
//! 4. A pasted gateway URL's label is shortened to `freenet:<first 8 of the id>[suffix]`.
//!
//! PARITY RULE: the decisions (which labels contradict their destination, which URLs are
//! rewritten, what a share link is) must give the web UI's verdict for the same text. They are
//! ported function for function from the web file, each marked "keep in sync with ...", and
//! their tests are ported from the web file's tests. The share-link validator
//! (`share_link.rs`) is included from the web crate unchanged, and the visual fold behind the
//! label check is copied verbatim (`visual_fold.rs`, with a drift test). A change to a rule
//! belongs in the web file and here in the same commit.
//!
//! Deliberately STRICTER than the web UI (which leaves these links live): a label that names a
//! contract, a `freenet:` id, or a URL on another host than its destination, for a destination
//! that is not on the node, is unlinked; a text with any link reference definition, or whose
//! output reads back as different links than `prepare` wrote, is shown as plain text.
//!
//! What differs from the web UI because this renders markdown with another engine: the web
//! rewrites the HTML it generated, this rewrites the markdown source (found with the parser's
//! source offsets) and the widget draws the result, with its own autolinking turned off: a bare
//! URL is a link only where `prepare` wrote one. An unlinked label is its visible text with
//! punctuation escaped (emphasis inside it is dropped), a pasted URL that must not be a link is
//! shown as inline code (fenced longer than any backtick run in the message), and a `\` in a
//! URL is refused (a browser reads it as `/`).

use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};

use super::model;
use super::share_link::{self, parse_share_link};
use super::visual_fold::{is_display_hidden, is_format_or_control};
use crate::rooms::NODE_AUTHORITY;

/// Most rewrites made to one text. Past it [`prepare`] gives up (`None`) and the caller shows
/// the text as plain text: handing the widget the unchanged source would switch every
/// protection here off.
const MAX_REWRITES: usize = 50;
/// Longest prepared text (twice `markdown::MAX_SOURCE_BYTES`). A reference definition used many
/// times expands a short text into many long links; past this [`prepare`] gives up.
const MAX_OUTPUT_BYTES: usize = 8192;
/// Longest whitespace-free run examined as a bare link candidate (the web UI's
/// `MAX_BARE_LINK_CANDIDATE_LEN`).
const MAX_CANDIDATE_LEN: usize = share_link::MAX_SHARE_LINK_LEN + 128;

/// The options the widget parses with (`rune_kit`'s markdown parser), so both see the same
/// links.
fn options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS
}

/// `src` rewritten so its links to Freenet apps behave as in the web UI (see the module docs).
/// Cheap, and `src` is returned as it is when nothing needs changing. Call it only on text
/// that passed `markdown::is_cheap`.
///
/// `None` means the text is too complex to make safe (more than [`MAX_REWRITES`] rewrites, a
/// result past [`MAX_OUTPUT_BYTES`], overlapping edits): the caller must show it as plain text,
/// never as the unprepared markdown.
pub fn prepare(src: &str) -> Option<String> {
    let (edits, kept) = rewrites(src)?;
    if edits.is_empty() && !src.contains('|') {
        return Some(strip_label_format(src));
    }
    // What the links in the output must be: the ones kept as written, and the ones the edits
    // wrote (each edit parsed on its own).
    let mut expected = kept;
    for e in &edits {
        expected.extend(link_dests(&e.text));
    }
    expected.sort();
    let out = strip_label_format(&apply(src, edits)?);
    // The output is parsed again, as the widget will. Text that reads differently next to what
    // we wrote around it (a `[label](<dest>)` right after a `]` becoming a reference link, a
    // definition turning a shortcut into a link) shows up as a link we did not write.
    let mut got = link_dests(&out);
    got.sort();
    (got == expected && !has_definitions(&out)).then_some(out)
}

/// `md` without the control and format characters (bidi overrides, zero-width spaces) inside
/// its links, so a label cannot be drawn reordered or hidden. The web UI's sanitiser drops them
/// from the whole rendered text; here only links are touched, which leaves the joiners of emoji
/// sequences elsewhere alone. Destinations are checked on their own, so only labels change.
fn strip_label_format(md: &str) -> String {
    let bad = |c: char| is_format_or_control(c) && !matches!(c, '\n' | '\r' | '\t');
    if !md.chars().any(bad) {
        return md.to_owned();
    }
    let ranges: Vec<std::ops::Range<usize>> = Parser::new_ext(md, options())
        .into_offset_iter()
        .filter_map(|(ev, r)| matches!(ev, Event::Start(Tag::Link { .. })).then_some(r))
        .collect();
    let mut out = String::with_capacity(md.len());
    for (i, c) in md.char_indices() {
        if !(bad(c) && ranges.iter().any(|r| r.contains(&i))) {
            out.push(c);
        }
    }
    out
}

/// The destination of every link in `md`.
fn link_dests(md: &str) -> Vec<String> {
    Parser::new_ext(md, options())
        .filter_map(|ev| match ev {
            Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
            _ => None,
        })
        .collect()
}

/// Whether `md` has a link reference definition (`[label]: dest`), which can turn any
/// bracketed text, ours included, into a link elsewhere. (A footnote-style `[^x]:` line is not
/// one here, since footnotes are off.)
fn has_definitions(md: &str) -> bool {
    Parser::new_ext(md, options())
        .reference_definitions()
        .iter()
        .next()
        .is_some()
}

/// One replacement of `src[start..end]`.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

fn apply(src: &str, mut edits: Vec<Edit>) -> Option<String> {
    edits.sort_by_key(|e| e.start);
    let mut out = String::with_capacity(src.len() + edits.len() * 64);
    let mut cursor = 0;
    for e in edits {
        if e.start < cursor || e.end < e.start || e.end > src.len() {
            // Overlapping edits cannot happen by construction; never guess if they do.
            return None;
        }
        out.push_str(&src[cursor..e.start]);
        out.push_str(&e.text);
        cursor = e.end;
        if out.len() > MAX_OUTPUT_BYTES {
            return None;
        }
    }
    out.push_str(&src[cursor..]);
    (out.len() <= MAX_OUTPUT_BYTES).then_some(out)
}

/// A link found in the source, collected from its parser events.
struct Link {
    range: std::ops::Range<usize>,
    kind: LinkType,
    dest: String,
    title: String,
    /// The label as the reader sees it: text, code and image alt text, a line break as `\n`.
    text: String,
    /// Whether the label is nothing but text (no emphasis, code, image or break).
    plain_only: bool,
    /// Source range of the label's content.
    label_start: Option<usize>,
    label_end: usize,
    /// Tooltips of images inside the label.
    img_titles: Vec<String>,
    /// Whether another link (an autolink) sits inside the label.
    nested: bool,
    /// Destinations of the links inside the label.
    inner_dests: Vec<String>,
}

impl Link {
    fn new(range: std::ops::Range<usize>, kind: LinkType, dest: CowStr, title: CowStr) -> Self {
        Link {
            range,
            kind,
            dest: dest.into_string(),
            title: title.into_string(),
            text: String::new(),
            plain_only: true,
            label_start: None,
            label_end: 0,
            img_titles: Vec::new(),
            nested: false,
            inner_dests: Vec::new(),
        }
    }

    fn child(&mut self, ev: &Event, range: &std::ops::Range<usize>) {
        self.label_start = Some(self.label_start.map_or(range.start, |s| s.min(range.start)));
        self.label_end = self.label_end.max(range.end);
        match ev {
            Event::Text(t) => self.text.push_str(t),
            Event::Code(t) => {
                self.text.push_str(t);
                self.plain_only = false;
            }
            Event::SoftBreak | Event::HardBreak => {
                self.text.push('\n');
                self.plain_only = false;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.inner_dests.push(dest_url.to_string());
                self.nested = true;
                self.plain_only = false;
            }
            Event::Start(Tag::Image { title, .. }) => {
                if !title.is_empty() {
                    self.img_titles.push(title.to_string());
                }
                self.plain_only = false;
            }
            _ => self.plain_only = false,
        }
    }
}

/// The edits `prepare` makes and the destinations of the links it leaves as written, `None`
/// when there are more than [`MAX_REWRITES`] edits or the text has a reference definition
/// (chat messages have none in practice, and one can redirect any `[bracketed]` text).
fn rewrites(src: &str) -> Option<(Vec<Edit>, Vec<String>)> {
    let lower = src.to_ascii_lowercase();
    if !src.contains(['[', '<'])
        && !lower.contains("freenet:")
        && !lower.contains("://")
        && !lower.contains("www.")
    {
        return Some((Vec::new(), Vec::new()));
    }
    if has_definitions(src) {
        return None;
    }
    let mut edits = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    let ticks = longest_backtick_run(src);
    // Source ranges of text rendered as plain prose: not code, not inside a link or an image.
    let mut prose: Vec<(usize, usize)> = Vec::new();
    let mut link: Option<Link> = None;
    // Links opened inside `link` (an autolink in a label): the label goes on across them, and
    // the outer link ends at its own closing event, not at theirs.
    let mut inner_links = 0usize;
    let (mut in_image, mut in_code_block) = (0usize, false);
    let mut containers = 0usize;
    for (ev, range) in Parser::new_ext(src, options()).into_offset_iter() {
        if let Some(l) = link.as_mut() {
            if matches!(ev, Event::Start(Tag::Link { .. })) {
                inner_links += 1;
                l.child(&ev, &range);
            } else if matches!(ev, Event::End(TagEnd::Link)) && inner_links > 0 {
                inner_links -= 1;
                l.child(&ev, &range);
            } else if matches!(ev, Event::End(TagEnd::Link)) {
                let done = link.take().expect("checked above");
                match rewrite_link(&done, src) {
                    Some(text) => edits.push(Edit {
                        start: done.range.start,
                        end: done.range.end,
                        text,
                    }),
                    None => {
                        kept.push(done.dest.clone());
                        kept.extend(done.inner_dests.iter().cloned());
                    }
                }
            } else {
                l.child(&ev, &range);
            }
            continue;
        }
        match ev {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            }) => link = Some(Link::new(range, link_type, dest_url, title)),
            Event::Start(Tag::Image { .. }) => in_image += 1,
            Event::End(TagEnd::Image) => in_image = in_image.saturating_sub(1),
            // The widget parses a paragraph that starts with `|` again, as a table, with its
            // own code (splitting at `|` even inside a code span). Whatever it would find
            // there, we have not checked: such a paragraph (pulldown did not make it a
            // table, or it would not be a paragraph) is plain text.
            Event::Start(Tag::Paragraph) => {
                if src[range.clone()].lines().any(starts_like_table_row) {
                    return None;
                }
            }
            Event::Start(Tag::BlockQuote(_) | Tag::List(_) | Tag::Item) => containers += 1,
            Event::End(TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item) => {
                containers = containers.saturating_sub(1)
            }
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            // A backslash escape leaves the backslash itself between two text events, so a
            // run that crosses one (`http://h/x\_y`) is still one run of prose.
            Event::Text(_) if in_image == 0 && !in_code_block => match prose.last_mut() {
                Some(last)
                    if last.1 == range.start
                        || (last.1 + 1 == range.start && src.as_bytes()[last.1] == b'\\') =>
                {
                    last.1 = range.end
                }
                _ => prose.push((range.start, range.end)),
            },
            _ => {}
        }
        if edits.len() > MAX_REWRITES {
            return None;
        }
    }
    // (No further check on a pipe elsewhere in the last paragraph: the widget re-parses it as a
    // table only when its FIRST line, trimmed, starts with `|` (rune-kit `pending_table`), which
    // `starts_like_table_row` above rejects for every line of every paragraph. A pipe anywhere
    // else, as in `docs | https://freenet.org`, is ordinary text and its links are kept.)
    bare_links(src, &prose, ticks, &mut edits);
    (edits.len() <= MAX_REWRITES).then_some((edits, kept))
}

/// Whether a line is one the widget's table parser would take as a row: after trimming
/// whitespace (Unicode: NBSP, U+3000, U+0085, VT, FF …) and block-quote markers it starts with
/// `|`. Mirrors rune-kit `markdown/parser.rs` (`pending_table`, which uses `trim_start`/`trim`);
/// keep the two the same.
fn starts_like_table_row(line: &str) -> bool {
    line.trim_start_matches(|c: char| c.is_whitespace() || c == '>')
        .starts_with('|')
}

/// What the whole link `[label](dest)` becomes, `None` to leave it as it is.
fn rewrite_link(l: &Link, src: &str) -> Option<String> {
    // Mention chips are this app's own links (a scheme drawn at random per run, so a message
    // cannot forge one): never rewritten, never unlinked.
    if l.dest
        .strip_prefix(model::mention_scheme())
        .is_some_and(|rest| rest.starts_with(':'))
    {
        return None;
    }
    if l.kind == LinkType::Email {
        return None;
    }
    let label = l.text.as_str();
    // The label's markdown: its own source when that is safe to move (no backslash escapes to
    // misread), else its text escaped.
    let label_md = || match l.label_start {
        Some(a) if l.kind != LinkType::Autolink && !l.nested => src
            .get(a..l.label_end)
            .filter(|s| !s.contains('\\') && !s.is_empty())
            .map_or_else(|| escape_md(label), str::to_owned),
        _ => escape_md(label),
    };
    // keep in sync with `finalize_anchors` (the share-link step): a share link as the label's
    // whole visible text opens that webapp on the reader's node; the hidden href is ignored.
    if l.plain_only
        && let Some(target) = parse_share_link(label)
    {
        return Some(link_md(&label_md(), &node_url(&target.local_path())));
    }
    // A destination a browser may read differently from how it is written (`\` is `/`), or
    // that no link can have: never linked, so host and contract checks below can trust it.
    if l.dest
        .chars()
        .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
    {
        return Some(unlinked_md(label));
    }
    // keep in sync with `finalize_anchors` (the node-destination step).
    let title = (!l.title.is_empty()).then_some(l.title.as_str());
    if let Some(dest_id) = node_destination(Some(&l.dest)) {
        // A pasted absolute URL (its text IS the href) shows where it goes, so its label is not
        // checked, unless its destination is unknown and something in it hides where it lands.
        let bare = l.dest == label && has_scheme_and_authority(label);
        let unlink = if bare {
            (dest_id.is_none() && !plain_absolute_url(label))
                || pasted_url_poses_before_its_path(label)
                || title
                    .is_some_and(|t| label_contradicts_destination(t, None, &[], dest_id.clone()))
        } else {
            label_contradicts_destination(label, title, &l.img_titles, dest_id)
        };
        if unlink {
            return Some(unlinked_md(label));
        }
    } else if !(l.dest == label && has_scheme_and_authority(label))
        && (label_contradicts_destination(label, title, &l.img_titles, None)
            || label_names_another_host(label, &l.dest))
    {
        // Stricter than the web UI, which leaves such a link live: a label that names a
        // contract (or a `freenet:` id) and a destination that is not on the node, as
        // `[freenet:<A>](https://evil.example/)` with the id mangled, or a node URL as the
        // label of an external page. Nothing here opens it on the node, so the label is a
        // claim nothing backs: show it as text.
        return Some(unlinked_md(label));
    }
    let new_dest = rewrite_dest(&l.dest);
    let new_label = if l.dest == label {
        beautify_freenet_label(&l.dest).map(|b| escape_md(&b))
    } else {
        None
    };
    let dest = new_dest.as_deref().unwrap_or(&l.dest);
    // Only web pages open from the app; any other link is shown as its text.
    if !(starts_with_ci(dest, "http://") || starts_with_ci(dest, "https://")) {
        return Some(unlinked_md(label));
    }
    if new_dest.is_none() && new_label.is_none() {
        // A link with another link in its label is flattened to one, so the inner one (any
        // scheme) does not survive.
        return l.nested.then(|| link_md(&escape_md(label), dest));
    }
    Some(link_md(&new_label.unwrap_or_else(label_md), dest))
}

/// Whether `label` names a host other than the host of `dest`, anywhere in its text:
/// `https://…` or `www.…` (even glued to other text: `go:https://paypal.com`, `→https://…`),
/// or a `host.tld/path` word (`paypal.com/login`). `[https://paypal.com/login](https://evil.example/)`
/// is the case. Stricter than the web UI, which leaves such a link live. Only for a destination
/// that is not on the node (the caller has checked). A word with no path and no scheme
/// (`Node.js`, `v1.2`) names nothing.
fn label_names_another_host(label: &str, dest: &str) -> bool {
    let dest_host = host_of(dest);
    let word_char = |c: char| c.is_ascii_alphanumeric() || ".-_/:@%?#=&~+".contains(c);
    label.split(|c: char| !word_char(c)).any(|w| {
        let w = w.trim_end_matches(['.', ',', ':', ';', '?', '!', '/']);
        let has_scheme = w.contains("://");
        let host = host_of(w);
        let host_like = host
            .as_deref()
            .and_then(|h| h.rsplit_once('.'))
            .is_some_and(|(_, tld)| tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic()));
        let named = has_scheme
            || starts_with_ci(w, "www.")
            || (w.contains('/') && !w.starts_with('/') && host_like);
        if !named {
            return false;
        }
        // Userinfo (`https://paypal.com@evil.example/`) reads as another host.
        let userinfo = w
            .split_once("://")
            .map_or(w, |(_, r)| r)
            .split(['/', '?', '#'])
            .next()
            .is_some_and(|a| a.contains('@'));
        userinfo || host.is_none() || host != dest_host
    })
}

/// The host of an `http(s)://host…` or `www.host…` URL, lowercase, without userinfo, port or
/// a leading `www.`.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host
        .rsplit_once(':')
        .filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit()))
        .map_or(host, |(h, _)| h);
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    (!host.is_empty()).then(|| host.to_owned())
}

/// The destination a URL in plain text (the fallback for markdown that cannot be made safe)
/// should open, `None` when it must not be a link. A gateway URL is not linked at all (the text
/// shown would differ from where it opens); anything the markdown path refuses (userinfo, a dot segment, a backslash, hidden or
/// control characters, a scheme other than http(s)) is refused here too.
pub fn plain_link_target(url: &str) -> Option<String> {
    let url = url.trim();
    if !(starts_with_ci(url, "http://") || starts_with_ci(url, "https://"))
        || !url
            .split_once("://")
            .is_some_and(|(_, r)| r.starts_with(|c: char| c.is_ascii_alphanumeric()))
        || url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || is_display_hidden(c))
        || url.contains('\\')
        || pasted_url_poses_before_its_path(url)
    {
        return None;
    }
    // A gateway URL is not linked here: the text shown would not be where it opens.
    match node_destination(Some(url)) {
        Some(_) => None,
        None => Some(url.to_owned()),
    }
}

/// A destination rewritten to the reader's node, if it names a Freenet web contract on any
/// host, or relatively. (The web UI rewrites an absolute URL to the same-origin path
/// (`rewrite_freenet_href`) and lets the browser resolve a relative href against the node it is
/// served from; here the node is [`NODE_AUTHORITY`].)
fn rewrite_dest(dest: &str) -> Option<String> {
    let path = |u: &str| rewrite_freenet_href(u).map(|p| node_url(&p));
    if let Some(p) = path(dest) {
        return Some(p);
    }
    if let Some(rest) = dest.strip_prefix("//") {
        return path(&format!("http://{rest}"));
    }
    // `http:/v1/...` and `/v1/...` resolve against the page.
    let relative = ["http:", "https:"]
        .iter()
        .find_map(|s| {
            dest.get(..s.len())
                .filter(|p| p.eq_ignore_ascii_case(s))
                .map(|_| &dest[s.len()..])
        })
        .unwrap_or(dest);
    let on_node = ["/v1/contract/web/", "/v2/contract/web/"]
        .iter()
        .any(|m| relative.starts_with(m));
    if on_node && !relative.starts_with("//") {
        return path(&format!("http://{NODE_AUTHORITY}{relative}"));
    }
    None
}

/// Every bare `freenet:` share link and `http://`, `https://` or `www.` URL in prose, each as an
/// edit: a checked `[label](dest)`, or an inline code span (shown, not clickable). The widget
/// is told not to link anything bare (`Markdown::autolink(false)`), so a token left alone is
/// plain text and harmless; nothing it would have linked is left unchecked.
///
/// Both label and destination are made from the token's own source text, so what is shown and
/// where it goes cannot differ (an entity such as `&#114;` stays the literal text it is). A
/// token the checks cannot fully read (a backslash, too long) is shown as code.
///
/// keep in sync with `extract_bare_freenet_links` (the scan and its trimming) and with the GFM
/// autolinks the web UI's markdown crate makes of a pasted URL.
fn bare_links(src: &str, prose: &[(usize, usize)], ticks: usize, edits: &mut Vec<Edit>) {
    let lower = src.to_ascii_lowercase();
    let mut starts: Vec<usize> = Vec::new();
    // All of them: a text is at most a few KB.
    for scheme in ["freenet:", "http://", "https://", "www."] {
        starts.extend(lower.match_indices(scheme).map(|(i, _)| i));
    }
    starts.sort_unstable();
    // End of the last candidate taken; a later one that starts inside it is part of it.
    let mut claimed_until = 0;
    for start in starts {
        if start < claimed_until
            || src[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            continue;
        }
        // The run of prose it sits in (code, links and images are none of ours); a token never
        // runs past it, which is also where a table cell ends.
        let i = prose.partition_point(|&(_, pe)| pe <= start);
        let Some(&(ps, pe)) = prose.get(i) else {
            continue;
        };
        if ps > start {
            continue;
        }
        // The widget's own rule for the end of a URL: whitespace, a control character, `<`, `>`.
        let run_end = src[start..pe]
            .find(|c: char| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>'))
            .map_or(pe, |p| start + p);
        let is_url = !starts_with_ci(&src[start..], "freenet:");
        let (end, text) = if run_end - start > MAX_CANDIDATE_LEN {
            // Too long to be checked.
            claimed_until = run_end;
            if is_url {
                edits.push(Edit {
                    start,
                    end: run_end,
                    text: code_span(&src[start..run_end], ticks),
                });
            }
            continue;
        } else {
            let token = trim_candidate(&src[start..run_end]);
            let end = start + token.len();
            // A `!` before `[` would make an image: only code is safe there.
            // A `!` or `]` before `[` would make an image or a reference link: only code (or,
            // for a share link, nothing) is safe there.
            let after_bang = src[..start].ends_with(['!', ']']);
            let text = if !is_url {
                match parse_share_link(token) {
                    Some(target) if !after_bang => {
                        // keep in sync with the share-link step of `finalize_anchors`.
                        link_md(&escape_md(token), &node_url(&target.local_path()))
                    }
                    _ => continue,
                }
            } else {
                match url_md(token, after_bang, ticks) {
                    Some(text) => text,
                    None => continue,
                }
            };
            (end, text)
        };
        // Whatever it is, a URL is one unit: nothing inside it is a link of its own.
        claimed_until = end;
        edits.push(Edit { start, end, text });
    }
}

/// What a pasted `http://`, `https://` or `www.` URL in prose becomes: a link to the reader's
/// node for a share or gateway URL, a link to what it shows for any other URL, inline code when
/// it must not be a link; `None` when it is not a URL at all.
///
/// keep in sync with `finalize_anchors` (the node-destination step, bare branch).
fn url_md(token: &str, as_code: bool, ticks: usize) -> Option<String> {
    let url = if starts_with_ci(token, "www.") {
        format!("http://{token}")
    } else {
        token.to_owned()
    };
    // Something to open: a host after the scheme.
    let after_scheme = url.split_once("://")?.1;
    if !after_scheme.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return None;
    }
    // Cannot be fully read: a backslash is `/` to a browser.
    if as_code || token.contains('\\') || token.chars().any(is_display_hidden) {
        return Some(code_span(token, ticks));
    }
    // A pasted URL's text before its path must not read as another destination, whatever the
    // destination is (userinfo, or a contract id in the host).
    let poses = pasted_url_poses_before_its_path(&url);
    let ordinary = || link_md(&escape_md(token), &url);
    if let Some(target) = parse_share_link(&url) {
        return Some(if poses {
            code_span(token, ticks)
        } else {
            link_md(&escape_md(token), &node_url(&target.local_path()))
        });
    }
    Some(match node_destination(Some(&url)) {
        Some(Some(_)) if poses => code_span(token, ticks),
        Some(Some(_)) => {
            let path = rewrite_freenet_href(&url)?;
            let label = beautify_freenet_label(&url).unwrap_or_else(|| token.to_owned());
            link_md(&escape_md(&label), &node_url(&path))
        }
        // An unknown destination (a dot segment, a refused `\`) must not pose as another.
        Some(None) if !plain_absolute_url(&url) || poses => code_span(token, ticks),
        Some(None) => ordinary(),
        None if poses => code_span(token, ticks),
        None => ordinary(),
    })
}

/// `token` without the trailing punctuation the markdown autolink leaves out of a link
/// (`!"'),.:;?_~*`, with `)` only when unbalanced). A `.` after `/` or `.` is kept, so
/// trimming can never turn a refused dot segment (`…/a/..`) into an accepted link.
fn trim_candidate(mut token: &str) -> &str {
    let open_parens = token.matches('(').count();
    let mut close_parens = token.matches(')').count();
    while let Some(last) = token.chars().next_back() {
        let unbalanced_paren = last == ')' && close_parens > open_parens;
        let dot_in_segment = last == '.'
            && matches!(
                token[..token.len() - 1].chars().next_back(),
                Some('/' | '.')
            );
        // Non-ASCII punctuation (`。`, `」`, …) can never be part of a valid link.
        let foreign_punctuation = !last.is_ascii() && !last.is_alphanumeric();
        if ("!\"',.:;?_~*".contains(last) && !dot_in_segment)
            || unbalanced_paren
            || foreign_punctuation
        {
            token = &token[..token.len() - last.len_utf8()];
            if last == ')' {
                close_parens -= 1;
            }
        } else {
            break;
        }
    }
    token
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

/// The URL of `path` (`/v1/contract/web/<id>…`) on the reader's own node.
fn node_url(path: &str) -> String {
    format!("http://{NODE_AUTHORITY}{path}")
}

/// `[label](<dest>)`, the destination made safe to sit between `<` and `>` (and in a table
/// cell): whitespace, controls and `<`, `>`, `\`, `|` are percent-encoded and `&` written as the
/// entity that reads back as `&`.
fn link_md(label_md: &str, dest: &str) -> String {
    let mut d = String::with_capacity(dest.len());
    for c in dest.chars() {
        if c.is_ascii() && (c <= ' ' || c == '\u{7f}' || matches!(c, '<' | '>' | '\\' | '|')) {
            d.push_str(&format!("%{:02X}", c as u8));
        } else if c == '&' {
            // Markdown reads `&amp;` in a destination as `&`; the destination is this text.
            d.push_str("&amp;");
        } else {
            d.push(c);
        }
    }
    format!("[{label_md}](<{d}>)")
}

/// `text` as markdown that shows it literally: every ASCII punctuation character escaped, a
/// line break a space.
fn escape_md(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\n' | '\r' => out.push(' '),
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// An unlinked label: its text with every punctuation character escaped, so `freenet:<id>` in
/// it is not read as a share link by a second pass. Nothing links bare text (the widget's own
/// autolinking is off), so a URL in it is only shown.
fn unlinked_md(label: &str) -> String {
    escape_md(label)
}

/// The longest run of backticks in `src`.
fn longest_backtick_run(src: &str) -> usize {
    src.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// `text` as an inline code span. Its fence is longer than any backtick run in the whole
/// SOURCE (`ticks`), not just in `text`, so no stray backtick earlier in the message can pair
/// with it and turn the text back into prose.
fn code_span(text: &str, ticks: usize) -> String {
    let text = text.replace(['\n', '\r'], " ");
    let fence = "`".repeat(ticks.max(longest_backtick_run(&text)) + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

// ---- Ported from the web UI -------------------------------------------------------------

/// Parsed shape of a Freenet web-contract URL.
struct FreenetWebUrl<'a> {
    /// The contract ID — base58-encoded 32-byte BLAKE3 hash (43 or 44 chars).
    contract_id: &'a str,
    /// Anything after the contract ID: leading slash + path, query, fragment.
    /// `""` for `/v1/contract/web/<id>` with nothing after.
    suffix: &'a str,
    /// Same-origin absolute path including the marker: `/v1/contract/web/<id><suffix>`.
    /// Used as a host/port-agnostic href.
    absolute_path: &'a str,
}

// keep in sync with ui/src/components/conversation/markdown.rs::node_destination (parity rule: same inputs, same verdict; see module docs)
/// The contract a link opens on the reader's node, if it opens one there:
/// `Some(Some(id))` for a gateway URL on any host (the rewrite below makes it
/// same-origin) or a relative `/v1|v2/contract/web/<id>` path; `Some(None)`
/// for any other relative href (it resolves against River's own URL, e.g.
/// `../<id>/`, so where it lands is not worth guessing); `None` for a link
/// that goes elsewhere.
fn node_destination(href: Option<&str>) -> Option<Option<String>> {
    let href = href?;
    // Protocol-relative `//host/…` takes the page's scheme.
    let absolute = match href.strip_prefix("//") {
        Some(rest) => Some(format!("http://{rest}")),
        None => has_scheme_and_authority(href).then(|| href.to_string()),
    };
    if let Some(absolute) = absolute {
        if let Some(parsed) = parse_freenet_web_url(&absolute) {
            return Some(Some(parsed.contract_id.to_string()));
        }
        // A contract path that does not parse (a dot segment, an encoded id
        // character, …) may still resolve to SOME contract on a node; which
        // one is not worth guessing.
        return absolute.contains("/contract/web/").then_some(None);
    }
    // `http:/path` and `http:../x` (a special scheme with no `//`) resolve
    // against the page when it has that scheme, exactly like a relative href.
    let relative = ["http:", "https:"]
        .iter()
        .find_map(|scheme| {
            href.get(..scheme.len())
                .filter(|p| p.eq_ignore_ascii_case(scheme))
                .map(|_| &href[scheme.len()..])
        })
        .or_else(|| is_relative_href(href).then_some(href))?;
    // A dot segment (literal or encoded) moves the path when the browser
    // resolves it, so the first segment would not be the destination.
    let path = relative.split(['?', '#']).next().unwrap_or("");
    if path.split('/').any(super::share_link::is_dot_segment) {
        return Some(None);
    }
    let id = ["/v1/contract/web/", "/v2/contract/web/"]
        .iter()
        .find_map(|m| relative.strip_prefix(m))
        .map(|after| {
            after
                .split(['/', '?', '#'])
                .next()
                .unwrap_or("")
                .to_string()
        })
        .filter(|id| !id.is_empty());
    Some(id)
}

// keep in sync with ui/src/components/conversation/markdown.rs::label_contradicts_destination (parity rule: same inputs, same verdict; see module docs)
/// True if an anchor's visible text names a contract other than `dest_id`
/// (or names any contract, when the destination is unknown).
///
/// Best effort, as defence in depth for links River did not write: what counts
/// as naming a contract is judged on what the reader sees
/// ([`super::visual_fold::visual_ascii`] of the decoded text, tags
/// dropped but image `alt` kept), so invisible characters, character
/// references, fancy text and homoglyphs inside an id do not hide it. A claim
/// is a run of 32+ ASCII letters/digits with an uppercase letter (a contract
/// id is 43-44; a hex hash or an ordinary word is not), or an id-looking run
/// of 8+ directly after a `freenet:` marker with no space (a shortened id
/// such as `freenet:UDzGbcWr`). Each claim must be a prefix of the destination id,
/// and the label's tooltip and any image tooltip inside it are read too. An id
/// broken up by a visible ASCII space or punctuation is not recognised
/// (freenet/river#736); a label naming no contract (`[River update](…)`)
/// claims nothing, like any other link text.
fn label_contradicts_destination(
    inner: &str,
    title: Option<&str>,
    img_titles: &[String],
    dest_id: Option<String>,
) -> bool {
    // The label, the link's tooltip, and any tooltip of an image inside it.
    // (Native: `inner` is the label's visible text, image alt included, and the image
    // tooltips are collected by the caller; the web reads them out of the HTML.)
    let mut claims = label_claimed_ids(inner);
    for text in title
        .into_iter()
        .chain(img_titles.iter().map(String::as_str))
    {
        claims.extend(label_claimed_ids(text));
    }
    if claims.is_empty() {
        return false;
    }
    let Some(dest_id) = dest_id else { return true };
    claims
        .iter()
        .any(|claim| !dest_id.starts_with(claim.as_str()))
}

// keep in sync with ui/src/components/conversation/markdown.rs::label_claimed_ids (parity rule: same inputs, same verdict; see module docs)
/// The contract-id claims in an anchor's visible text. See
/// [`label_contradicts_destination`].
fn label_claimed_ids(inner: &str) -> Vec<String> {
    // Anything that can split an id without the reader seeing a break must
    // not (the whitespace and joining rules below). This is a comparison, not
    // rendering, so joining too much can only unlink more, never link
    // something new.
    // Native: `inner` is already the decoded visible text.
    let decoded = inner;
    // Visible, full-width spaces separate, like an ASCII space. Every other
    // whitespace character is dropped: line and paragraph breaks (an id
    // wrapping looks the same), narrow and hair spaces, and controls a
    // browser draws with no width.
    let visible_space = |c: char| {
        matches!(
            c,
            ' ' | '\t' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
                ..='\u{2005}' | '\u{2007}' | '\u{2008}' | '\u{3000}'
        )
    };
    let spaced: String = decoded
        .chars()
        .filter(|&c| !c.is_whitespace() || visible_space(c))
        .collect();
    // Colon look-alikes read as the `:` of a `freenet:` marker.
    let folded: Vec<char> = super::visual_fold::visual_ascii(&spaced)
        .chars()
        .map(|c| match c {
            '\u{02D0}' | '\u{02F8}' | '\u{0589}' | '\u{05C3}' | '\u{0903}' | '\u{0A83}'
            | '\u{1361}' | '\u{1804}' | '\u{205A}' | '\u{2236}' | '\u{A4FD}' | '\u{A789}'
            | '\u{FE13}' | '\u{FE30}' | '\u{FE55}' => ':',
            other => other,
        })
        .collect();
    // A run of non-ASCII characters left between two ASCII letters/digits
    // after the visual fold is kept IN the id run as one placeholder (which
    // can never be a prefix of an ASCII id): an unfolded homoglyph, a lone
    // symbol look-alike, or any number of zero-width marks stacked on a
    // letter. It separates, as it looks, only if it holds a space,
    // punctuation, an ellipsis, a full-width character (CJK, kana, Hangul,
    // Yi, fullwidth forms, emoji), or a real word of a script written without
    // spaces (Thai, Lao, Myanmar, Khmer). This errs towards joining: joining
    // can only unlink more, never link something new.
    const JOINED: char = '\u{FFFD}';
    let separates = |c: char| {
        // Combining marks inside the full-width blocks still stack, and a few
        // characters there read as a Latin letter (`⸦`/`⸧` as C, `〇` and the
        // Hangul compatibility jamo such as `ㅇ` as o; real Korean text uses
        // syllables, U+AC00 on): these join like any look-alike.
        let exempt = matches!(u32::from(c),
            0x302A..=0x302F | 0x3099..=0x309A | 0x2E26 | 0x2E27 | 0x3007 | 0x3130..=0x318F);
        c.is_whitespace()
            || !exempt
                && matches!(u32::from(c),
                0x2000..=0x206F      // general punctuation
                | 0x22EE..=0x22F1    // ellipses
                | 0x2E00..=0x2E7F    // supplemental punctuation
                | 0x2E80..=0x30FF    // CJK radicals, symbols, kana
                | 0x3130..=0x9FFF    // Hangul compatibility, CJK unified
                | 0xA000..=0xA4CF    // Yi
                | 0xAC00..=0xD7FF    // Hangul
                | 0xF900..=0xFAFF    // CJK compatibility
                | 0xFE30..=0xFE4F    // CJK compatibility forms
                | 0xFF00..=0xFFEF    // halfwidth / fullwidth forms left unfolded
                | 0x1F000..=0x1FAFF  // emoji and pictographs
                | 0x20000..=0x3FFFF) // CJK extensions
    };
    // Letters (not marks) of the scripts written without spaces.
    let no_space_letter = |c: char| {
        let cp = u32::from(c);
        let block = matches!(cp, 0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1780..=0x17FF);
        let mark = matches!(cp,
            0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E              // Thai
            | 0x0EB1 | 0x0EB4..=0x0EBC | 0x0EC8..=0x0ECE           // Lao
            | 0x102B..=0x103E | 0x1056..=0x1059 | 0x105E..=0x1060  // Myanmar
            | 0x1062..=0x1064 | 0x1067..=0x106D | 0x1071..=0x1074
            | 0x1082..=0x108D | 0x108F | 0x109A..=0x109D
            | 0x17B4..=0x17D3 | 0x17DD); // Khmer
        block && !mark
    };
    let mut text = String::with_capacity(folded.len());
    let mut k = 0;
    while k < folded.len() {
        let c = folded[k];
        if c.is_ascii() {
            text.push(c);
            k += 1;
            continue;
        }
        let run_end = (k..folded.len())
            .find(|&e| folded[e].is_ascii())
            .unwrap_or(folded.len());
        let run = &folded[k..run_end];
        let next_is_alnum = folded
            .get(run_end)
            .is_some_and(|n| n.is_ascii_alphanumeric());
        // (Bopomofo, U+3100-312F, is deliberately not a separator: a lone
        // `ㄚ` reads as Y.)
        let joins = !run.iter().any(|&c| separates(c))
            && run.iter().filter(|&&c| no_space_letter(c)).count() <= 3;
        let flanked = k > 0 && folded[k - 1].is_ascii_alphanumeric() && next_is_alnum && joins;
        // Marks sitting on a marker's `:` or `/` (`freenet:\u{05BC}<id>`)
        // are dropped, so the marker still sits right before the id.
        let on_marker = k > 0 && matches!(folded[k - 1], ':' | '/') && next_is_alnum && joins;
        if flanked {
            text.push(JOINED);
        } else if !on_marker {
            text.push(' ');
        }
        k = run_end;
    }
    let lower = text.to_ascii_lowercase();
    // Runs are over ASCII letters and digits, not just base58: `O`, `0`, `I`
    // and `l` read as part of an id, and a run containing one can never be a
    // prefix of a real id.
    let in_run = |c: char| c.is_ascii_alphanumeric() || c == JOINED;
    let mut claims = Vec::new();
    let flush = |start: usize, end: usize, claims: &mut Vec<String>| {
        let run = &text[start..end];
        let len = run.chars().count();
        let has_upper = run.chars().any(|c| c.is_ascii_uppercase());
        // A SHORT run counts only right after a `freenet:` marker with no
        // space (`freenet:UDzGbcWr`, `freenet://…`, the shapes River itself
        // displays), 8+ long and looking like an id (a digit, or a capital
        // after the first character), so "Freenet: GitHub mirror" is prose.
        let id_like_short = run.chars().any(|c| c.is_ascii_digit() || c == JOINED)
            || run.chars().skip(1).any(|c| c.is_ascii_uppercase());
        let after_marker = lower[..start].trim_end_matches('/').ends_with("freenet:");
        if (len >= 32 && has_upper) || (len >= 8 && after_marker && id_like_short) {
            claims.push(run.to_string());
        }
    };
    let mut run_start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match (in_run(c), run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(start)) => {
                flush(start, i, &mut claims);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        flush(start, text.len(), &mut claims);
    }
    claims
}

// keep in sync with ui/src/components/conversation/markdown.rs::has_scheme_and_authority (parity rule: same inputs, same verdict; see module docs)
/// True for `scheme://…` (RFC 3986 scheme syntax), i.e. an absolute URL with
/// an authority. A `://` later in the href (in a query or fragment) does not
/// count, so `../x/#://` stays relative.
fn has_scheme_and_authority(href: &str) -> bool {
    let Some(colon) = href.find("://") else {
        return false;
    };
    let scheme = &href[..colon];
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

// keep in sync with ui/src/components/conversation/markdown.rs::pasted_url_poses_before_its_path (parity rule: same inputs, same verdict; see module docs)
/// True if a pasted URL's text before its contract path could be read as a
/// different destination: any userinfo (`http://freenet:<A>@host/…`), or a
/// contract id or `freenet:` claim in the host (`http://<A>.example/…`).
/// What follows the contract path is the app's own business (a River
/// invite's long code, an app path holding other ids) and is not read.
fn pasted_url_poses_before_its_path(url: &str) -> bool {
    let decoded = url;
    let Some((_, after_scheme)) = decoded.split_once("://") else {
        return false;
    };
    let head_end = ["/v1/contract/web/", "/v2/contract/web/"]
        .iter()
        .filter_map(|m| after_scheme.find(m))
        .min()
        .unwrap_or_else(|| {
            after_scheme
                .find(['/', '?', '#'])
                .unwrap_or(after_scheme.len())
        });
    let head = &after_scheme[..head_end];
    head.contains('@') || !label_claimed_ids(head).is_empty()
}

// keep in sync with ui/src/components/conversation/markdown.rs::plain_absolute_url (parity rule: same inputs, same verdict; see module docs)
/// True for an absolute `scheme://host/path` whose text is where it goes:
/// no percent-escapes and no dot segments in the path.
fn plain_absolute_url(url: &str) -> bool {
    if !has_scheme_and_authority(url) || url.contains('%') {
        return false;
    }
    let after_authority = url
        .split_once("://")
        .map(|(_, rest)| rest.find('/').map_or("", |p| &rest[p..]))
        .unwrap_or("");
    let path = after_authority.split(['?', '#']).next().unwrap_or("");
    !path.split('/').any(super::share_link::is_dot_segment)
}

// keep in sync with ui/src/components/conversation/markdown.rs::is_relative_href (parity rule: same inputs, same verdict; see module docs)
/// True for an href with no scheme and no authority, i.e. one the browser
/// resolves against River's own URL (including the empty href the markdown
/// crate emits for a scheme it does not allow).
fn is_relative_href(href: &str) -> bool {
    if href.starts_with("//") {
        return false;
    }
    let first_delimiter = href.find(['/', '?', '#']).unwrap_or(href.len());
    !href[..first_delimiter].contains(':')
}

// keep in sync with ui/src/components/conversation/markdown.rs::parse_freenet_web_url (parity rule: same inputs, same verdict; see module docs)
/// Parse a Freenet web-contract URL, validating the contract ID looks like a
/// real base58-encoded 32-byte BLAKE3 hash. The hash shape is the reliable
/// indicator: it rejects same-prefix paths whose ID segment is too short or
/// uses characters outside the base58 alphabet (e.g. visual-confusion chars
/// `0OIl`, which a real contract ID can never contain).
///
/// The URL must use `http` or `https` (defense in depth — `[label](url)`
/// markdown can in theory carry other schemes; we don't want to rewrite a
/// `javascript:`-flavored input even though the rewrite would defang it).
///
/// The suffix must not contain dot path segments, literal or percent-encoded
/// (see `suffix_has_dotdot_segment`). Without this guard, a
/// pasted `http://attacker/v1/contract/web/<valid-shape-id>/../../foo`
/// would be rewritten to a same-origin path that the browser normalizes
/// into `/foo` on the reader's local gateway — sending the click to a
/// path the attacker chose on the *victim's* gateway, instead of to the
/// attacker's host where it would have gone before the rewrite.
fn parse_freenet_web_url(url: &str) -> Option<FreenetWebUrl<'_>> {
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let after_scheme = &url[scheme_end + 3..];
    let path_offset = after_scheme.find('/')?;
    let path = &after_scheme[path_offset..];
    let after_marker = path
        .strip_prefix("/v1/contract/web/")
        .or_else(|| path.strip_prefix("/v2/contract/web/"))?;

    let id_end = after_marker
        .find(|c: char| !is_base58_char(c))
        .unwrap_or(after_marker.len());
    if !matches!(id_end, 43 | 44) {
        return None;
    }
    let suffix = &after_marker[id_end..];
    if suffix_has_dotdot_segment(suffix) {
        return None;
    }
    // Native-only hardening (the web UI lacks it): a browser reads `\` as `/` in a special
    // URL, so `…/<id>\..\..\x` would be a dot-segment path the check above cannot see.
    if suffix.contains('\\') {
        return None;
    }
    Some(FreenetWebUrl {
        contract_id: &after_marker[..id_end],
        suffix,
        absolute_path: path,
    })
}

// keep in sync with ui/src/components/conversation/markdown.rs::suffix_has_dotdot_segment (parity rule: same inputs, same verdict; see module docs)
/// True if any path segment in `suffix` is a dot segment, literal or
/// percent-encoded (`..`, `%2e%2e`, `.%2E`, `.`, ...), which a browser resolves
/// the same way. Checking only a literal `..` let `…/<A>/%2e%2e/<B>/` open
/// contract B, or climb out of `/v1/contract/web/` altogether. Path segments
/// are the `/`-separated components before any `?` query or `#` fragment.
fn suffix_has_dotdot_segment(suffix: &str) -> bool {
    let path_only = suffix
        .split_once(['?', '#'])
        .map(|(p, _)| p)
        .unwrap_or(suffix);
    path_only.split('/').any(super::share_link::is_dot_segment)
}

// keep in sync with ui/src/components/conversation/markdown.rs::is_base58_char (parity rule: same inputs, same verdict; see module docs)
/// Bitcoin-style base58 alphabet: digits and letters minus the visually
/// ambiguous `0`, `O`, `I`, `l`. A base58 string never contains these four.
fn is_base58_char(c: char) -> bool {
    matches!(c,
        '1'..='9'
        | 'A'..='H' | 'J'..='N' | 'P'..='Z'
        | 'a'..='k' | 'm'..='z'
    )
}

// keep in sync with ui/src/components/conversation/markdown.rs::rewrite_freenet_href (parity rule: same inputs, same verdict; see module docs)
/// Rewrite a Freenet web-contract URL's href to a same-origin absolute path,
/// stripping the scheme + host + port. Returns None for non-Freenet URLs.
///
/// `http://127.0.0.1:7509/v1/contract/web/<id>/foo` → `/v1/contract/web/<id>/foo`
/// `https://gw.example/v1/contract/web/<id>/#hash`  → `/v1/contract/web/<id>/#hash`
///
/// The browser resolves the absolute path against the current page's origin,
/// so the rewritten link points at whichever gateway River is loaded from —
/// fixing pasted links that hard-code the sender's local gateway address.
fn rewrite_freenet_href(url: &str) -> Option<String> {
    Some(parse_freenet_web_url(url)?.absolute_path.to_string())
}

// keep in sync with ui/src/components/conversation/markdown.rs::beautify_freenet_label (parity rule: same inputs, same verdict; see module docs)
/// If `url` is a Freenet web-contract URL, return a beautified label like
/// `freenet:UDzGbcWr` or `freenet:UDzGbcWr/index.html`. Returns None for any
/// other URL so the caller falls back to the original link text.
fn beautify_freenet_label(url: &str) -> Option<String> {
    let parsed = parse_freenet_web_url(url)?;
    // Defense in depth: refuse to beautify if the suffix carries raw HTML
    // metacharacters. The markdown crate URL-encodes these today, but the
    // label is rendered via dangerous_inner_html with no further escaping,
    // so we'd rather skip the rewrite than risk smuggling markup.
    if parsed.suffix.contains(['<', '>', '"']) {
        return None;
    }
    // The label shows only an 8-character id prefix, so the rest of it must
    // not read as a Freenet link of its own: `…/web/<B>/freenet:<A>` would
    // otherwise read as a link to A. Show such a URL in full instead.
    let suffix_seen = super::visual_fold::visual_ascii(parsed.suffix).to_ascii_lowercase();
    let names_a_contract = parsed
        .suffix
        .split(|c: char| !is_base58_char(c))
        .any(super::share_link::is_valid_contract_id);
    if suffix_seen.contains("freenet") || suffix_seen.contains("contract/web") || names_a_contract {
        return None;
    }
    // A bare trailing slash adds no information; drop it.
    let suffix = if parsed.suffix == "/" {
        ""
    } else {
        parsed.suffix
    };
    let id_prefix = &parsed.contract_id[..8];
    Some(format!("freenet:{id_prefix}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `prepare` for text it can handle.
    fn prepare(md: &str) -> String {
        super::prepare(md).expect("prepared")
    }

    const SAMPLE_ID: &str = "UDzGbcWrKN748tYbhvbPCCCQrZc9r9xkN3tUuun5Rts";
    const SAMPLE_ID_2: &str = "EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr";
    const RIVER_ID: &str = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
    const HARVEST_ID: &str = "6FzSeAUKcqJrveKyU8RJgGKc5jRB1Z2juvxXtwTA4Em9";

    fn local(path: &str) -> String {
        format!("http://127.0.0.1:7509{path}")
    }

    /// What the widget draws as links after `prepare` (it links nothing bare itself): each
    /// explicit link as (destination, label text).
    fn opens(md: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut label: Option<(String, String)> = None;
        for ev in Parser::new_ext(md, options()) {
            match ev {
                Event::Start(Tag::Link { dest_url, .. }) => {
                    label = Some((dest_url.to_string(), String::new()))
                }
                Event::End(TagEnd::Link) => out.extend(label.take()),
                Event::Text(t) | Event::Code(t) if label.is_some() => {
                    label.as_mut().unwrap().1.push_str(&t)
                }
                _ => {}
            }
        }
        out
    }

    /// All plain text the reader sees outside links.
    fn plain(md: &str) -> String {
        let mut out = String::new();
        let mut in_link = false;
        for ev in Parser::new_ext(md, options()) {
            match ev {
                Event::Start(Tag::Link { .. }) => in_link = true,
                Event::End(TagEnd::Link) => in_link = false,
                Event::Text(t) | Event::Code(t) if !in_link => out.push_str(&t),
                _ => {}
            }
        }
        out
    }

    fn no_link(md: &str) {
        let out = prepare(md);
        assert!(
            opens(&out).is_empty(),
            "{md:?} -> {out:?}: {:?}",
            opens(&out)
        );
    }

    fn one_link(md: &str, dest: &str, label: &str) {
        let out = prepare(md);
        assert_eq!(
            opens(&out),
            vec![(dest.to_owned(), label.to_owned())],
            "{md:?} -> {out:?}"
        );
    }

    // ---- Share links ----

    #[test]
    fn share_link_as_a_labels_text_opens_on_the_readers_node() {
        let dest = local(&format!("/v1/contract/web/{RIVER_ID}"));
        // The hidden href is ignored.
        for md in [
            format!("[freenet:{RIVER_ID}](https://evil.example/x)"),
            format!("[freenet://{RIVER_ID}](freenet:{HARVEST_ID})"),
            format!("[https://freenet.org/open#{RIVER_ID}](/docs)"),
            format!("[https://freenet.org/open/#{RIVER_ID}](javascript:alert(1))"),
            format!("<freenet:{RIVER_ID}>"),
        ] {
            let out = prepare(&md);
            let found = opens(&out);
            assert_eq!(found.len(), 1, "{md:?} -> {out:?}");
            assert_eq!(found[0].0, dest, "{md:?} -> {out:?}");
        }
        one_link(
            &format!("[freenet:{HARVEST_ID}/a?b=c#d](https://evil.example)"),
            &local(&format!("/v1/contract/web/{HARVEST_ID}/a?b=c#d")),
            &format!("freenet:{HARVEST_ID}/a?b=c#d"),
        );
    }

    #[test]
    fn a_share_link_hidden_behind_a_label_is_not_converted() {
        // As in the web UI: the label names nothing, so the link is left as written (it
        // opens the freenet.org page, which shows the id before anything opens).
        let md = format!("[click me](https://freenet.org/open#{RIVER_ID})");
        assert_eq!(prepare(&md), md);
    }

    #[test]
    fn invalid_share_links_are_not_converted() {
        for md in [
            "freenet:short".to_owned(),
            format!("freenet:{}", &RIVER_ID[..42]),
            format!("freenet:{RIVER_ID}/a/../b"),
            format!("[freenet:{RIVER_ID}/..](https://evil.example)"),
            format!("https://freenet.org/open?x#{RIVER_ID}"),
        ] {
            let out = prepare(&md);
            assert!(
                !opens(&out)
                    .iter()
                    .any(|(d, _)| d.starts_with("http://127.0.0.1:7509")),
                "{md:?} -> {out:?}"
            );
        }
    }

    #[test]
    fn bare_share_links_in_prose_are_converted() {
        let a = local(&format!("/v1/contract/web/{RIVER_ID}"));
        let h = local(&format!("/v1/contract/web/{HARVEST_ID}/"));
        one_link(
            &format!("see freenet:{RIVER_ID}"),
            &a,
            &format!("freenet:{RIVER_ID}"),
        );
        one_link(
            &format!("see FREENET://{RIVER_ID}"),
            &a,
            &format!("FREENET://{RIVER_ID}"),
        );
        one_link(
            &format!("open https://freenet.org/open#{RIVER_ID}"),
            &a,
            &format!("https://freenet.org/open#{RIVER_ID}"),
        );
        // Trailing punctuation is not part of the link.
        let out = prepare(&format!(
            "try freenet:{RIVER_ID}. Or (freenet:{HARVEST_ID}/)!"
        ));
        assert_eq!(
            opens(&out),
            vec![
                (a.clone(), format!("freenet:{RIVER_ID}")),
                (h, format!("freenet:{HARVEST_ID}/")),
            ],
            "{out:?}"
        );
        // Emphasis around it, a table cell, the start of the text, right after a code span.
        one_link(
            &format!("*freenet:{RIVER_ID}*"),
            &a,
            &format!("freenet:{RIVER_ID}"),
        );
        one_link(
            &format!("freenet:{RIVER_ID}"),
            &a,
            &format!("freenet:{RIVER_ID}"),
        );
        one_link(
            &format!("`code`freenet:{RIVER_ID}"),
            &a,
            &format!("freenet:{RIVER_ID}"),
        );
        let table = prepare(&format!("|a|b|\n|-|-|\n|freenet:{RIVER_ID}/x|y|"));
        let found = opens(&table);
        assert_eq!(found.len(), 1, "{table:?}");
        assert_eq!(found[0].0, local(&format!("/v1/contract/web/{RIVER_ID}/x")));
    }

    #[test]
    fn markdown_in_a_bare_link_shows_literally_and_goes_where_it_says() {
        // `&amp;` and `_` inside the link are shown as written, and the destination is made
        // from the same text, so what is shown and where it goes cannot differ.
        let md = format!("freenet:{RIVER_ID}/a?x=1&amp;y=2");
        let out = prepare(&md);
        let found = opens(&out);
        assert_eq!(found.len(), 1, "{out:?}");
        assert_eq!(found[0].1, format!("freenet:{RIVER_ID}/a?x=1&amp;y=2"));
        // The destination is the same text, `&amp;` and all (as in the web UI).
        assert_eq!(
            found[0].0,
            local(&format!("/v1/contract/web/{RIVER_ID}/a?x=1&amp;y=2"))
        );
        assert!(!found[0].0.contains(['<', '>', ' ']));
    }

    #[test]
    fn share_links_in_code_are_not_converted() {
        for md in [
            format!("`freenet:{RIVER_ID}`"),
            format!("```\nfreenet:{RIVER_ID}\n```"),
            format!("    freenet:{RIVER_ID}"),
            format!("`x` `https://evil.example/v1/contract/web/{RIVER_ID}/`"),
        ] {
            assert_eq!(prepare(&md), md, "{md:?}");
        }
    }

    #[test]
    fn freenet_inside_other_words_is_not_converted() {
        for md in [
            format!("notfreenet:{RIVER_ID}"),
            format!("x{RIVER_ID} freenet:"),
            format!("2freenet:{RIVER_ID}"),
        ] {
            assert_eq!(prepare(&md), md, "{md:?}");
        }
    }

    #[test]
    fn an_image_sign_before_a_bare_link_does_not_make_an_image() {
        let out = prepare(&format!("!freenet:{RIVER_ID}"));
        assert!(!out.contains("!["), "{out:?}");
    }

    // ---- Gateway URLs ----

    #[test]
    fn a_gateway_url_on_any_host_opens_on_the_readers_node() {
        let want = local(&format!("/v1/contract/web/{SAMPLE_ID}/x?y=1#z"));
        for host in [
            "http://127.0.0.1:7509",
            "https://gw.example.org",
            "HTTP://localhost:9",
        ] {
            let dest = format!("{host}/v1/contract/web/{SAMPLE_ID}/x?y=1#z");
            // An explicit link with an honest label.
            one_link(&format!("[Atlas]({dest})"), &want, "Atlas");
            // The same pasted bare and as an autolink: the label is shortened.
            let label = format!("freenet:{}/x?y=1#z", &SAMPLE_ID[..8]);
            one_link(&dest, &want, &label);
            one_link(&format!("see {dest}."), &want, &label);
            one_link(&format!("<{dest}>"), &want, &label);
            one_link(&format!("[{dest}]({dest})"), &want, &label);
        }
        one_link(
            &format!("[a](https://gw.example/v2/contract/web/{SAMPLE_ID}/)"),
            &local(&format!("/v2/contract/web/{SAMPLE_ID}/")),
            "a",
        );
        // A bare trailing slash is dropped from the shortened label.
        one_link(
            &format!("http://h.example/v1/contract/web/{SAMPLE_ID}/"),
            &local(&format!("/v1/contract/web/{SAMPLE_ID}/")),
            &format!("freenet:{}", &SAMPLE_ID[..8]),
        );
    }

    #[test]
    fn relative_and_protocol_relative_hrefs_open_on_the_readers_node() {
        let want = local(&format!("/v1/contract/web/{SAMPLE_ID}/"));
        for href in [
            format!("/v1/contract/web/{SAMPLE_ID}/"),
            format!("//gw.example/v1/contract/web/{SAMPLE_ID}/"),
            format!("http:/v1/contract/web/{SAMPLE_ID}/"),
        ] {
            one_link(&format!("[River]({href})"), &want, "River");
        }
        // Not a valid id, or no contract path: nothing to open, so not a link at all.
        no_link("[docs](/v1/contract/web/short/)");
        no_link("[docs](/docs)");
        no_link("[docs](../x)");
    }

    #[test]
    fn a_dot_segment_is_never_rewritten() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        for dest in [
            format!("http://attacker.example/v1/contract/web/{b}/../../v1/peer/diagnostics"),
            format!("http://attacker.example/v1/contract/web/{b}/%2e%2e/x"),
            format!("http://attacker.example/v1/contract/web/{b}/.%2E/x"),
            format!("http://attacker.example/v1/contract/web/{b}/./x"),
            format!("/v1/contract/web/{a}/../{b}/"),
            format!("/v1/contract/web/{a}/%2e%2e/{b}/"),
            format!("http://127.0.0.1:7509/v1/./contract/web/{b}/"),
        ] {
            // Pasted bare: shown as text, never a link to either place.
            let out = prepare(&dest);
            assert!(opens(&out).is_empty(), "{dest:?} -> {out:?}");
            assert!(plain(&out).contains(&dest), "{dest:?} -> {out:?}");
            // As an explicit link with a label that names no contract: still no rewrite.
            let out = prepare(&format!("[open]({dest})"));
            assert!(
                !opens(&out)
                    .iter()
                    .any(|(d, _)| d.starts_with("http://127.0.0.1:7509/v1/")
                        && d.contains(b)
                        && !dest.starts_with("http://127.0.0.1:7509")),
                "{dest:?} -> {out:?}"
            );
        }
        // A `..` in the query or fragment is only data.
        one_link(
            &format!("http://h.example/v1/contract/web/{SAMPLE_ID}/?next=../foo"),
            &local(&format!("/v1/contract/web/{SAMPLE_ID}/?next=../foo")),
            &format!("freenet:{}/?next=../foo", &SAMPLE_ID[..8]),
        );
    }

    #[test]
    fn a_backslash_after_the_id_is_not_rewritten() {
        // A browser reads `\` as `/`, so this would be a dot-segment path.
        let url = format!("http://h.example/v1/contract/web/{SAMPLE_ID}/a\\..\\x");
        let out = prepare(&format!("see {url} ok"));
        assert_eq!(out, format!("see `{url}` ok"));
        assert!(
            parse_freenet_web_url(&format!(
                "http://h.example/v1/contract/web/{SAMPLE_ID}/a\\..\\x"
            ))
            .is_none()
        );
    }

    #[test]
    fn non_gateway_urls_and_other_schemes_are_left_alone() {
        // An ordinary URL is linked by `prepare` (the widget links nothing bare), to what it shows.
        one_link(
            "see https://example.com/page now",
            "https://example.com/page",
            "https://example.com/page",
        );
        assert_eq!(
            prepare("[a](https://example.com/)"),
            "[a](https://example.com/)"
        );
        let out = prepare(&format!(
            "[click](ftp://x.example/v1/contract/web/{SAMPLE_ID}/)"
        ));
        assert!(opens(&out).is_empty(), "{out:?}");
        for dest in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "freenet:abc",
            "data:text/html,x",
        ] {
            let out = prepare(&format!("[x]({dest})"));
            assert!(
                opens(&out).is_empty() && plain(&out).contains('x'),
                "{dest}: {out:?}"
            );
        }
    }

    #[test]
    fn a_pasted_gateway_url_naming_another_contract_is_shown_in_full() {
        for url in [
            format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/freenet:{RIVER_ID}"),
            format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/x/{RIVER_ID}"),
        ] {
            let out = prepare(&url);
            let found = opens(&out);
            assert_eq!(found.len(), 1, "{out:?}");
            assert_eq!(found[0].1, url, "{out:?}");
            assert!(
                found[0]
                    .0
                    .starts_with(&local(&format!("/v1/contract/web/{SAMPLE_ID}/")))
            );
        }
    }

    #[test]
    fn a_pasted_river_invite_link_stays_a_shortened_link() {
        let code =
            "2NEpo7TZRRrLZSi2U7MpNwwdMV3fjDaZaYcAhMR1FUD4xYLQ7mQAnfk9dVYNEUoGXxBxcMGRgHtqXPC";
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{RIVER_ID}/?invitation={code}");
        for text in [
            url.clone(),
            format!("Join: {url}"),
            format!("2. Open this link: {url}"),
        ] {
            one_link(
                &text,
                &local(&format!("/v1/contract/web/{RIVER_ID}/?invitation={code}")),
                &format!("freenet:{}/?invitation={code}", &RIVER_ID[..8]),
            );
        }
    }

    #[test]
    fn a_pasted_url_that_poses_before_its_path_is_not_a_link() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        for url in [
            format!("http://freenet.org:{a}@127.0.0.1:7509/v1/contract/web/{b}/#freenet"),
            format!("http://freenet:{a}@127.0.0.1:7509/v1/contract/web/{b}/?via=freenet"),
            format!("http://{a}.example/v1/contract/web/{b}/#freenet"),
            format!("http://River@127.0.0.1:7509/v1/contract/web/{b}/"),
        ] {
            for md in [url.clone(), format!("<{url}>"), format!("[{url}]({url})")] {
                no_link(&md);
                assert!(plain(&prepare(&md)).contains(&url), "{md:?}: text kept");
            }
        }
        // Its tooltip is label text too.
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{b}/");
        no_link(&format!("[{url}]({url} \"freenet:{a}\")"));
    }

    // ---- A label naming a different contract ----

    #[test]
    fn a_label_naming_another_contract_is_not_a_link() {
        for hidden in [
            format!("http://x.example/v1/contract/web/{SAMPLE_ID}/"),
            format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/"),
            format!("//127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/"),
            format!("/v1/contract/web/{SAMPLE_ID}/"),
            format!("http:/v1/contract/web/{SAMPLE_ID}/"),
            format!("HTTPS:../{SAMPLE_ID}/"),
        ] {
            for label in [
                format!("freenet:{RIVER_ID}/ "),
                format!("freenet:{RIVER_ID}/&#8203;"),
                format!("*freenet:{RIVER_ID}/* app"),
                format!("https://freenet.org/open#{RIVER_ID}/ "),
                format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID_2}/"),
                "freenet:raAqMhMG".to_string(),
            ] {
                let md = format!("[{label}]({hidden})");
                no_link(&md);
                assert!(
                    plain(&prepare(&md)).contains("freenet")
                        || plain(&prepare(&md)).contains("http"),
                    "{md:?}: label kept as text"
                );
            }
        }
    }

    #[test]
    fn a_label_naming_one_contract_never_opens_another() {
        // The web UI's order: a label that is a whole share link opens THAT contract on the
        // reader's node (the hidden href is ignored), so it cannot open the one in the href.
        let md = format!("[freenet:{SAMPLE_ID}](https://evil.example/v1/contract/web/{RIVER_ID}/)");
        one_link(
            &md,
            &local(&format!("/v1/contract/web/{SAMPLE_ID}")),
            &format!("freenet:{SAMPLE_ID}"),
        );
        // A label that names a contract but is no share link (a shortened id) and an href that
        // opens a node contract: unlinked, label kept as text.
        let md = format!(
            "[freenet:{}](https://evil.example/v1/contract/web/{RIVER_ID}/)",
            &SAMPLE_ID[..10]
        );
        let out = prepare(&md);
        assert!(opens(&out).is_empty(), "{out:?}");
        assert_eq!(plain(&out), format!("freenet:{}", &SAMPLE_ID[..10]));
    }

    #[test]
    fn honest_labels_keep_their_link_and_rewrite() {
        let href = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let want = local(&format!("/v1/contract/web/{SAMPLE_ID}/"));
        for label in [
            "River update".to_string(),
            "my freenet: node".to_string(),
            format!("freenet:{SAMPLE_ID} (mirror)"),
            format!("freenet:{}", &SAMPLE_ID[..8]),
            format!("freenet:{}\u{22EF}{}", &SAMPLE_ID[..8], &SAMPLE_ID[36..]),
            format!("freenet:{}", &SAMPLE_ID[..9]),
            "commit 3f2a9c1be47d58a6f0c2e9b1d3a4f5e6c7b8a9d0".to_string(),
        ] {
            let out = prepare(&format!("[{label}]({href})"));
            let found = opens(&out);
            assert_eq!(found.len(), 1, "{label:?}: {out:?}");
            assert_eq!(found[0].0, want, "{label:?}");
        }
        one_link(
            &format!("[/v1/contract/web/{SAMPLE_ID}/](/v1/contract/web/{SAMPLE_ID}/)"),
            &want,
            &format!("/v1/contract/web/{SAMPLE_ID}/"),
        );
        // A label that merely mentions the path names no contract.
    }

    /// The label check sees what the reader sees: invisible characters, character
    /// references, fancy text, homoglyphs and image alt text do not hide an id.
    #[test]
    fn disguised_ids_in_labels_are_still_seen() {
        let hidden = format!("http://x.example/v1/contract/web/{SAMPLE_ID}/");
        for label in [
            RIVER_ID.to_string(),
            format!("free&#8203;net:{RIVER_ID}/"),
            format!("freenet\u{FF1A}{RIVER_ID}/"),
            format!("open {RIVER_ID}"),
            format!("open {}", RIVER_ID.replace('a', "\u{0430}")),
            format!("open {}", RIVER_ID.replace('M', "\u{200B}M\u{2060}")),
            format!("open {}", RIVER_ID.replace('r', "&#114;")),
            format!("free\u{200B}net:{}", &RIVER_ID[..10]),
            format!("\u{1D41F}\u{1D42B}eenet:{RIVER_ID}"),
            format!("![freenet:{RIVER_ID}](x.png)"),
        ] {
            no_link(&format!("[{label}]({hidden})"));
        }
    }

    /// Breaks the reader does not see (or reads as an id wrapping) do not split an id into
    /// runs too short to count.
    #[test]
    fn invisible_or_wrapping_breaks_do_not_split_a_claimed_id() {
        let (head, tail) = RIVER_ID.split_at(20);
        let hidden = format!("/v1/contract/web/{SAMPLE_ID}/");
        for label in [
            format!("{head}![]({hidden}x.png){tail}"),
            format!("{head}![Q](x.png){tail}"),
            format!("{head}\n{tail}"),
            format!("{head}\u{0591}{tail}"),
            format!("{head}\u{200A}{tail}"),
            format!("{head}\u{2009}{tail}"),
            format!("{head}\u{202F}{tail}"),
            "freenet:/raAqMhMG".to_string(),
            "freenet\u{A789}raAqMhMG".to_string(),
            format!("{head}\u{000B}{tail}"),
            format!("{head}\u{0085}{tail}"),
            format!("{head}\u{2028}{tail}"),
            format!("{head}\u{A7B3}{tail}"),
            format!("{head}\u{10317}{tail}"),
            format!("{head}\u{05BC}\u{05BC}\u{05BC}\u{05BC}{tail}"),
            format!("{head}\u{2D5D}{tail}"),
            format!("{head}\u{302A}{tail}"),
            "freenet:\u{05BC}raAqMhMG".to_string(),
            "freenet:/\u{0E31}raAqMhMG".to_string(),
            "freenet\u{A4FD}raAqMhMG".to_string(),
            format!("{head}\u{222A}{tail}"),
            format!("{head}\u{311A}{tail}"),
            format!("{head}{}{tail}", "\u{05BC}".repeat(12)),
            format!("freenet:{}\u{2178}GbcW", &SAMPLE_ID_2[..3]),
            format!("{head}{}{tail}", "\u{0EC8}".repeat(4)),
            format!("{head}\u{2E26}{tail}"),
            format!("{head}\u{3147}{tail}"),
            "freenet\u{2236}raAqMhMG".to_string(),
        ] {
            no_link(&format!("[{label}]({hidden})"));
        }
    }

    #[test]
    fn a_dot_segment_destination_is_unknown_so_a_naming_label_blocks_it() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        for href in [
            format!("/v1/contract/web/{a}/../{b}/"),
            format!("/v1/contract/web/{a}/%2e%2e/{b}/"),
            format!("http:/v1/contract/web/{a}/../{b}/"),
            format!("http://127.0.0.1:7509/v1/contract/web/{a}/../{b}/"),
            format!("//127.0.0.1:7509/v1/contract/web/{a}/%2e%2e/{b}/"),
            format!("http://127.0.0.1:7509/v1/./contract/web/{b}/"),
            format!("http://127.0.0.1:7509/v1/contract/web/%36%46{}/", &b[2..]),
        ] {
            for label in [format!("River: {a}"), "open freenet:raAqMhMG".to_string()] {
                no_link(&format!("[{label}]({href})"));
            }
        }
        let rel = format!("/v1/contract/web/{RIVER_ID}/%2e%2e/{SAMPLE_ID}/");
        no_link(&format!("[{rel}]({rel})"));
    }

    #[test]
    fn a_link_title_is_checked_like_the_label() {
        let href = format!("/v1/contract/web/{SAMPLE_ID}/");
        no_link(&format!("[River]({href} \"freenet:{RIVER_ID}\")"));
        no_link(&format!("[![River](x.png \"freenet:{RIVER_ID}\")]({href})"));
        one_link(
            &format!(
                "[Explore with Atlas](http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/ \"Atlas search engine\")"
            ),
            &local(&format!("/v1/contract/web/{SAMPLE_ID}/")),
            "Explore with Atlas",
        );
    }

    #[test]
    fn label_bypasses_are_closed() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        let (head, tail) = a.split_at(20);
        for md in [
            format!("[River {a}](/v1/contract/./web/{b}/?x=://)"),
            format!("[River {a}](../{b}/#://)"),
            format!("[freenet:raAqMhMG](../{b}/#://)"),
            format!("[{head}\u{0261}{tail}](/v1/contract/web/{b}/)"),
            format!("[{head}\u{051A}{tail}](/v1/contract/web/{b}/)"),
            format!("[{head}\u{0417}{tail}](/v1/contract/web/{b}/)"),
            format!("[River: {head}\u{05BC}{tail}](/v1/contract/web/{b}/)"),
            format!("[open freenet:raAq\u{05BC}MhMG](/v1/contract/web/{b}/)"),
            format!("[River: {head}O{tail}](/v1/contract/web/{b}/)"),
            format!(
                "[http://freenet:{a}@x.example/v1/contract/web/{b}/](http://127.0.0.1:7509/v1/contract/web/{b}/)"
            ),
            format!("[freenet:{a}/ ](/v1/contract/web/{b}/)"),
            format!("[**freenet:{a}/**](../{b}/)"),
            format!("[freenet:{a}/ ](javascript:alert(1))"),
        ] {
            no_link(&md);
        }
        // A bare URL whose text hides where it lands is not a link.
        no_link(&format!(
            "http://127.0.0.1:7509/v1/contract/web/{a}/%2e%2e/%36%46{}/",
            &b[2..]
        ));
        no_link(&format!(
            "http://127.0.0.1:7509/v1/contract/web/%36%46{}/",
            &b[2..]
        ));
    }

    #[test]
    fn claim_rule_edges() {
        let b = SAMPLE_ID;
        let href = format!("/v1/contract/web/{b}/");
        // A claim must be a PREFIX of the destination id.
        no_link(&format!("[freenet:{}]({href})", &b[3..12]));
        assert_eq!(
            opens(&prepare(&format!("[freenet:{}]({href})", &b[..9]))).len(),
            1
        );
        // A URL-shaped label for the right contract that ALSO names another.
        no_link(&format!(
            "[http://other.example/v1/contract/web/{b}/freenet:{RIVER_ID}](http://other.example/v1/contract/web/{b}/x)"
        ));
    }

    #[test]
    fn freenet_in_prose_labels_is_not_a_claim() {
        let href = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        for label in [
            "Try it on Freenet: Harvest market",
            "Freenet: Ghostkey",
            "freenet:Harvest",
            "Freenet: GitHub mirror",
            "freenet: iPhone app",
            "Freenet: River2026 launch",
            "Freenet: Stra\u{00DF}e",
            "FreenetのRiverでチャット、DeltaでWebサイト、AtlasでHarvest市場",
            "ติดตั้งFreenetแล้วเปิดRiverและDeltaหรือGhostKeyและHarvestได้เลย",
        ] {
            let out = prepare(&format!("[{label}]({href})"));
            assert_eq!(opens(&out).len(), 1, "{label:?}: {out:?}");
        }
    }

    // ---- What is left alone ----

    #[test]
    fn a_real_mention_chip_is_left_alone_and_a_forged_one_is_just_a_link() {
        let chip = format!("[@freenet:{RIVER_ID}]({}:member)", model::mention_scheme());
        assert_eq!(prepare(&chip), chip);
        let chip = format!(
            "hi [@bob]({}:self) see freenet:{RIVER_ID}",
            model::mention_scheme()
        );
        let out = prepare(&chip);
        assert!(out.starts_with(&format!("hi [@bob]({}:self) see ", model::mention_scheme())));
        // A scheme that is not this run's is not a chip: not a web page, so no link.
        let forged = format!(
            "[@x](rvm-0000000000000000:self) [freenet:{RIVER_ID}](rvm-0000000000000000:member)"
        );
        let out = prepare(&forged);
        assert!(!out.contains("rvm-0000000000000000"), "{out:?}");
    }

    #[test]
    fn links_in_code_are_untouched_but_surrounding_links_are_not() {
        let md = format!(
            "`[freenet:{RIVER_ID}](https://evil.example)`\n\n```\n[freenet:{RIVER_ID}](https://evil.example)\nhttp://h/v1/contract/web/{SAMPLE_ID}/\n```\n\nfreenet:{HARVEST_ID}"
        );
        let out = prepare(&md);
        assert!(out.starts_with(&md[..md.len() - format!("freenet:{HARVEST_ID}").len()]));
        assert!(out.contains(&local(&format!("/v1/contract/web/{HARVEST_ID}"))));
    }

    #[test]
    fn text_without_links_is_returned_as_it_is() {
        for md in [
            "",
            "plain words",
            "**bold** and `code`",
            "a [b] c",
            "- a\n- b",
        ] {
            assert_eq!(prepare(md), md);
        }
    }

    #[test]
    fn more_than_the_cap_of_rewrites_gives_up_so_the_caller_shows_plain_text() {
        let one = format!("[freenet:{RIVER_ID}](https://evil.example)\n\n");
        let out = super::prepare(&one.repeat(MAX_REWRITES)).expect("at the cap");
        assert!(
            opens(&out)
                .iter()
                .all(|(d, _)| d.starts_with("http://127.0.0.1:7509/"))
        );
        assert_eq!(super::prepare(&one.repeat(MAX_REWRITES + 1)), None);
        let bare = format!("freenet:{RIVER_ID}\n\n").repeat(MAX_REWRITES + 1);
        assert_eq!(super::prepare(&bare), None);
        // The spoof that used to hide behind the cap: many autolinks, then a false label.
        let spoof = format!(
            "{}[freenet:{RIVER_ID}/ x](https://evil.example/)",
            "<ab:c> ".repeat(MAX_REWRITES + 1)
        );
        assert_eq!(super::prepare(&spoof), None);
    }

    #[test]
    fn the_candidate_cap_counts_all_schemes() {
        // Plenty of `freenet:` words first must not stop the URLs after them being checked.
        let url = format!("http://freenet:{RIVER_ID}@evil.example/v1/contract/web/{SAMPLE_ID}/");
        let few = format!("{} {url}", "freenet:x ".repeat(300));
        let out = prepare(&few);
        assert!(opens(&out).is_empty(), "{out:?}");
        // Over the cap of ACCEPTED candidates: give up.
        let many = format!("freenet:{RIVER_ID} ").repeat(MAX_REWRITES + 1);
        assert_eq!(super::prepare(&many), None);
    }

    #[test]
    fn a_nested_autolink_in_a_label_does_not_truncate_it() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        // The spoof is still caught, and no text is deleted.
        let md = format!("[<ab:c> freenet:{a}](http://evil.example/v1/contract/web/{b}/) tail");
        let out = prepare(&md);
        assert!(opens(&out).is_empty(), "{out:?}");
        let shown = plain(&out);
        assert!(
            shown.contains("ab:c") && shown.contains(&format!("freenet:{a}")),
            "{out:?}"
        );
        assert!(shown.ends_with(" tail"), "{out:?}");
        // An honest label with a nested autolink: one link, flattened, text kept.
        let md = format!("[see <ab:c> app](http://evil.example/v1/contract/web/{b}/) tail");
        let out = prepare(&md);
        let found = opens(&out);
        assert_eq!(found.len(), 1, "{out:?}");
        assert_eq!(found[0].0, local(&format!("/v1/contract/web/{b}/")));
        assert!(
            found[0].1.contains("ab:c") && plain(&out).ends_with(" tail"),
            "{out:?}"
        );
        // Not rewritten, still flattened: no inner link of another scheme survives.
        let out = prepare("[x <ab:c> y](https://example.com/) z");
        assert_eq!(
            opens(&out),
            vec![("https://example.com/".to_owned(), "x ab:c y".to_owned())]
        );
        // Many nested ones, and a nested one in a nested one's neighbour: no panic, no overlap.
        let _ = super::prepare(&format!(
            "[{}](https://e.example/) <ab:c>",
            "<ab:c>".repeat(20)
        ));
    }

    #[test]
    fn a_label_claiming_a_contract_for_an_external_destination_is_unlinked() {
        // Stricter than the web UI, which leaves these live: nothing backs the claim.
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        for md in [
            format!("[*freenet:{a}/ app*](https://evil.example/)"),
            format!("[freenet:{} app](https://evil.example/)", &a[..12]),
            format!("[http://127.0.0.1:7509/v1/contract/web/{a}](https://evil.example/)"),
            format!("[open {b}](https://evil.example/)"),
            format!("[![x](y.png \"freenet:{a}\")](https://evil.example/)"),
        ] {
            no_link(&md);
        }
        // An ordinary label, and a pasted external URL, stay links.
        one_link(
            "[River docs](https://example.com/)",
            "https://example.com/",
            "River docs",
        );
        let long = format!("https://example.com/{}", "aB".repeat(20));
        one_link(&format!("[{long}]({long})"), &long, &long);
    }

    #[test]
    fn a_pasted_url_the_scan_cannot_skip_is_never_left_to_the_widget() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        // A backslash escape splits the text events; the URL is still checked.
        let url = format!("http://freenet:{a}@evil.example/v1/contract/web/{b}/x\\_y");
        let out = prepare(&url);
        assert!(opens(&out).is_empty(), "{out:?}");
        // So is a long one: shown as code, not left to the widget.
        let long = format!(
            "http://freenet:{a}@evil.example/{}",
            "p".repeat(MAX_CANDIDATE_LEN)
        );
        let out = prepare(&format!("see {long} now"));
        assert!(
            opens(&out).is_empty() && out.contains(&format!("`{long}`")),
            "{out:?}"
        );
        // A URL that runs into emphasis markup: the part in prose is checked.
        let out = prepare(&format!("http://freenet:{a}@evil.example/x*y*z"));
        assert!(opens(&out).is_empty(), "{out:?}");
        // `>` ends the URL, as in the widget (`<` and whitespace already did).
        let out = prepare(&format!("a http://h.example/v1/contract/web/{b}/x>y"));
        assert_eq!(opens(&out).len(), 1, "{out:?}");
        assert!(out.ends_with(">y"), "{out:?}");
        // Next to a `!` a gateway URL is code, never an image.
        let out = prepare(&format!("!http://h.example/v1/contract/web/{b}/"));
        assert!(opens(&out).is_empty() && !out.contains("!["), "{out:?}");
    }

    /// The widget links nothing bare (`.autolink(false)`), so every bare URL that could have
    /// been linked is either a link `prepare` wrote and checked, or text. These are the inputs a
    /// scan that trusted its own reading let through.
    #[test]
    fn no_bare_url_is_left_for_the_widget_to_link() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        // A stray backtick earlier in the message cannot pair with a code span of ours.
        let md = format!("a ` b http://freenet:{a}@evil.example/v1/contract/web/{b}/ c ` d");
        let out = prepare(&md);
        assert!(opens(&out).is_empty(), "{out:?}");
        let code: Vec<_> = Parser::new_ext(&out, options())
            .filter_map(|e| match e {
                Event::Code(c) => Some(c.to_string()),
                _ => None,
            })
            .collect();
        assert!(
            code.iter().any(|c| c.contains("evil.example")),
            "the URL is code, not prose: {out:?}"
        );
        // `|` does not end a URL outside a table: the dot segment after it is read, so the
        // URL is shown as code, never linked.
        let out = super::prepare(&format!("http://h.example/v1/contract/web/{b}/a|/../b"))
            .expect("prepared");
        assert!(out.starts_with('`') && opens(&out).is_empty(), "{out:?}");
        // In a table cell it does end at the cell.
        let out = prepare(&format!(
            "|a|b|\n|-|-|\n|http://h.example/v1/contract/web/{b}/x|y|"
        ));
        assert_eq!(opens(&out).len(), 1, "{out:?}");
        // `www.` is checked and linked as http://www.
        one_link(
            "go www.example.com/x now",
            "http://www.example.com/x",
            "www.example.com/x",
        );
        let out = prepare(&format!("www.h.example/v1/contract/web/{b}/"));
        assert_eq!(opens(&out).len(), 1, "{out:?}");
        assert!(
            opens(&out)[0]
                .0
                .starts_with("http://127.0.0.1:7509/v1/contract/web/"),
            "{out:?}"
        );
        let out = prepare(&format!("www.freenet:{a}@evil.example/"));
        assert!(opens(&out).is_empty(), "{out:?}");
        // An entity in a URL is shown as written and is what the link goes to: no decoding.
        let out = prepare("http://h.example/?q=&#114;x");
        let found = opens(&out);
        assert_eq!(found.len(), 1, "{out:?}");
        assert!(
            found[0].0.contains("&#114;") && found[0].1.contains("&#114;"),
            "{found:?}"
        );
        let out = prepare(&format!("http://&#102;reenet:{a}@evil.example/"));
        assert!(opens(&out).is_empty(), "{out:?}");
        // The code fence is longer than any backtick run in the message.
        assert!(code_span("u", 3).starts_with("````"));
    }

    #[test]
    fn the_widget_is_told_not_to_autolink() {
        // Everything above depends on it; `markdown::render` must keep calling it.
        assert!(
            include_str!("markdown.rs")
                .matches(concat!(".autoli", "nk(false)"))
                .count()
                >= 1
        );
    }

    /// Text that reads differently once our rewrite sits next to it must not become a link we
    /// did not write.
    #[test]
    fn a_rewrite_cannot_join_the_text_around_it_into_another_link() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        // The reviewer's input: a bare URL right after `]`, and a definition for its label.
        let md =
            format!("[freenet:{b}]http://x.co\n\n[http\\:\\/\\/x\\.co]: https://evil.example/");
        assert_eq!(super::prepare(&md), None);
        // Without the definition the URL right after `]` is code, not a link.
        let out = prepare(&format!("[freenet:{b}]http://x.co"));
        assert!(opens(&out).is_empty(), "{out:?}");
        // A shortcut link redirected by a definition; images likewise.
        for md in [
            format!("[foo] freenet:{a}\n\n[foo]: https://evil.example/"),
            format!("[freenet:{a}](https://e.example/) [foo]\n\n[foo]: https://evil.example/"),
            "![x]http://x.co\n\n[x]: https://evil.example/i.png".to_string(),
            "![x] [x]\n\n[x]: https://evil.example/".to_string(),
        ] {
            assert_eq!(super::prepare(&md), None, "{md:?}");
        }
        // `!` and `]` before a share link: left alone.
        for md in [
            format!("]freenet:{a}"),
            format!("!freenet:{a}"),
            format!("[x]freenet:{a}"),
        ] {
            let out = prepare(&md);
            assert!(opens(&out).is_empty(), "{md:?} -> {out:?}");
        }
        // The check on the output: the links read back are exactly the ones written.
        let out = prepare(&format!(
            "[Atlas](http://h.example/v1/contract/web/{b}/) freenet:{a}"
        ));
        assert_eq!(opens(&out).len(), 2);
    }

    #[test]
    fn a_label_naming_another_host_is_unlinked() {
        for md in [
            "[https://paypal.com/login](https://evil.example/)",
            "[Sign in at www.bank.com](https://evil.example/)",
            "[www.bank.com](https://evil)",
            "[![https://paypal.com](x.png)](https://evil.example/)",
            "[HTTPS://PayPal.com](https://evil.example/)",
            "[https://paypal.com@evil.example/](https://evil.example/)",
        ] {
            no_link(md);
        }
        // The same host (any path, port, case, `www.`) links.
        for md in [
            "[https://freenet.org](https://freenet.org/x)",
            "[www.freenet.org/a](https://freenet.org:8080/x)",
            "[see HTTPS://Freenet.ORG/a.](https://www.freenet.org/)",
            "[River](https://freenet.org/)",
        ] {
            assert_eq!(opens(&prepare(md)).len(), 1, "{md}");
        }
    }

    #[test]
    fn a_paragraph_the_widget_would_read_as_a_table_is_plain_text() {
        for md in [
            "| `a | [https://paypal.com](https://evil.example/)` |",
            "|- [https://paypal.com](https://evil.example/) | x",
            "text\n| a | [x](https://evil.example/) | b |",
            "> | `a | [https://paypal.com](https://evil.example/)` |",
        ] {
            assert_eq!(super::prepare(md), None, "{md:?}");
        }
        // A table pulldown itself parses is fine.
        let out = prepare(&format!("|a|b|\n|-|-|\n|freenet:{RIVER_ID}|y|"));
        assert_eq!(opens(&out).len(), 1, "{out:?}");
    }

    #[test]
    fn unicode_whitespace_before_a_pipe_does_not_hide_a_table_paragraph() {
        for ws in [
            '\u{a0}', '\u{3000}', '\u{85}', '\u{2003}', '\u{2009}', '\u{b}', '\u{c}',
        ] {
            let md = format!("{ws}| `a | [https://paypal.com](https://evil.example/)` |");
            assert_eq!(super::prepare(&md), None, "{ws:?}");
        }
        // A pipe elsewhere in the paragraph is ordinary text: the widget only re-parses a
        // paragraph whose first line starts with one, so its links are kept (and vetted).
        for md in [
            "x `a | b` [https://paypal.com](https://evil.example/)",
            "first\n\nsee | http://example.com/x",
            "Official room | [site](https://freenet.org) | https://freenet.org/docs",
        ] {
            let out = super::prepare(md).unwrap_or_else(|| panic!("{md:?}"));
            assert!(!out.contains("| `a | ["), "{out:?}");
        }
        let out = super::prepare("Docs | [site](https://freenet.org)").unwrap();
        assert_eq!(opens(&out).len(), 1, "{out:?}");
        // A pipe alone, or links with no pipe in the last paragraph, are fine; a link in an
        // earlier paragraph does not matter.
        assert_eq!(super::prepare("a | b").as_deref(), Some("a | b"));
        assert!(super::prepare("[a](https://example.com/)\n\nx | y").is_some());
        let out = prepare(&format!("|a|b|\n|-|-|\n|freenet:{RIVER_ID}|y|"));
        assert_eq!(opens(&out).len(), 1, "{out:?}");
    }

    #[test]
    fn a_destination_a_browser_reads_differently_is_unlinked() {
        for md in [
            "[https://good.com](https://evil.example\\good.com/)",
            "[x](<https://evil.example/a b>)",
            "[x](<https://evil.example/\u{7}>)",
        ] {
            no_link(md);
        }
    }

    #[test]
    fn a_host_named_anywhere_in_a_label_is_checked() {
        for md in [
            "[Log in (https://paypal.com)](https://evil.example/)",
            "[go:https://paypal.com](https://evil.example/)",
            "[\u{2192}https://paypal.com](https://evil.example/)",
            "[paypal.com/login](https://evil.example/)",
            "[Sign in: www.paypal.com.](https://evil.example/)",
        ] {
            no_link(md);
        }
        for md in [
            "[River update](https://example.com/)",
            "[v1.2](https://example.com/)",
            "[Node.js docs](https://nodejs.org/)",
            "[see example.com/a](https://example.com/b)",
            "[(https://example.com)](https://www.example.com/x)",
        ] {
            assert_eq!(opens(&prepare(md)).len(), 1, "{md}");
        }
    }

    #[test]
    fn hidden_characters_in_a_url_make_it_code() {
        for ch in ['\u{202E}', '\u{2066}', '\u{200B}', '\u{FEFF}'] {
            let out = prepare(&format!("see http://exa{ch}mple.com/x ok"));
            assert!(
                opens(&out).is_empty() && out.contains('`'),
                "{ch:?}: {out:?}"
            );
        }
    }

    #[test]
    fn the_plain_text_fallback_links_only_what_the_markdown_path_would() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        let ok = |u: &str| plain_link_target(u);
        assert_eq!(
            ok("https://example.com/a?b=c"),
            Some("https://example.com/a?b=c".into())
        );
        // A gateway URL is not linked in plain text.
        assert_eq!(
            ok(&format!("http://gw.example/v1/contract/web/{b}/x")),
            None
        );
        for bad in [
            format!("http://freenet:{a}@evil.example/"),
            "http://user@evil.example/".to_owned(),
            format!("http://h.example/v1/contract/web/{b}/../x"),
            format!("http://h.example/v1/contract/web/{b}/%2e%2e/x"),
            "http://h.example/a\\b".to_owned(),
            "http://h.example/a\u{202E}b".to_owned(),
            "http://h.example/a\u{200B}b".to_owned(),
            "http://h.exa\nmple/".to_owned(),
            "javascript:alert(1)".to_owned(),
            "ftp://h.example/".to_owned(),
            "http://".to_owned(),
        ] {
            assert_eq!(ok(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn output_is_capped() {
        // One definition used by many references: each becomes a long link.
        let dest = format!(
            "http://h.example/v1/contract/web/{SAMPLE_ID}/{}",
            "a".repeat(1500)
        );
        let md = format!("{}\n\n[r]: {dest}", "[x][r] ".repeat(40));
        assert_eq!(super::prepare(&md), None);
        // Any definition at all: plain text.
        assert_eq!(super::prepare(&format!("[x][r]\n\n[r]: {dest}")), None);
    }

    #[test]
    fn prepare_is_idempotent() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        for md in [
            format!("freenet:{a}"),
            format!("see freenet:{a}/x?y=1, and https://freenet.org/open#{b}!"),
            format!(
                "[freenet:{a}](https://evil.example/x) [Atlas](http://h.example/v1/contract/web/{b}/)"
            ),
            format!("http://h.example/v1/contract/web/{b}/x?y=1"),
            format!("<http://h.example/v2/contract/web/{b}/>"),
            format!("[freenet:{b}](https://evil.example/v1/contract/web/{a}/)"),
            format!("http://freenet:{a}@127.0.0.1:7509/v1/contract/web/{b}/"),
            format!("[River](/v1/contract/web/{a}/) *freenet:{b}* `freenet:{a}`"),
            format!("| x |\n|-|\n| freenet:{a}/p |"),
            "[docs](/docs) [x](javascript:alert(1))".to_string(),
            format!("[@m]({}:member) freenet:{a}", model::mention_scheme()),
        ] {
            let once = prepare(&md);
            assert_eq!(prepare(&once), once, "{md:?}");
        }
    }

    #[test]
    fn output_links_only_ever_open_web_pages_or_are_chips() {
        let (a, b) = (RIVER_ID, SAMPLE_ID);
        let chip = format!("{}:member", model::mention_scheme());
        for md in [
            format!(
                "[x](javascript:alert(1)) [y](file:///etc/passwd) [z](freenet:{a}) [w](//evil.example/)"
            ),
            format!("[a](<> ) [b]() [c](#frag) [d](mailto:a@b.c) <a@b.c> <freenet:{a}>"),
            format!("[@x]({chip}) [@y](rvm-1:self)"),
            format!("[{a}](/v1/contract/web/{b}/) [freenet:{b}](ftp://x/)"),
        ] {
            let out = prepare(&md);
            for ev in Parser::new_ext(&out, options()) {
                if let Event::Start(Tag::Link {
                    dest_url,
                    link_type,
                    ..
                }) = ev
                {
                    let ok = starts_with_ci(&dest_url, "http://")
                        || starts_with_ci(&dest_url, "https://")
                        || dest_url.as_ref() == chip
                        || link_type == LinkType::Email;
                    assert!(ok, "{md:?} -> {out:?}: {dest_url}");
                }
            }
        }
    }

    #[test]
    fn adversarial_text_is_prepared_quickly_and_without_panicking() {
        let start = std::time::Instant::now();
        for md in [
            "freenet:".repeat(500),
            format!("{}x", "http://".repeat(500)),
            "[".repeat(90) + &"](freenet:a)".repeat(3),
            "<freenet:a>".repeat(300),
            format!("freenet:{}", "a".repeat(3000)),
            "![](freenet:)\u{0}\u{feff}\r\n".repeat(100),
            "\\".repeat(2000),
        ] {
            let _ = super::prepare(&md);
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    // ---- The ported helpers ----

    #[test]
    fn relative_href_classification() {
        for h in ["", "/v1/x", "../x", "x", "?q", "#f", "a/b:c"] {
            assert!(is_relative_href(h), "{h:?}");
        }
        for h in ["//evil.example/", "https://x", "mailto:a@b", "javascript:x"] {
            assert!(!is_relative_href(h), "{h:?}");
        }
    }

    #[test]
    fn freenet_web_url_label_shortened() {
        let id = SAMPLE_ID;
        assert_eq!(
            beautify_freenet_label(&format!("http://127.0.0.1:7509/v1/contract/web/{id}/")),
            Some(format!("freenet:{}", &id[..8]))
        );
        assert_eq!(
            beautify_freenet_label(&format!(
                "https://h.example/v1/contract/web/{id}/index.html?a=b"
            )),
            Some(format!("freenet:{}/index.html?a=b", &id[..8]))
        );
        assert_eq!(beautify_freenet_label("https://example.com/x"), None);
    }

    #[test]
    fn href_rewrite_cases() {
        let id = SAMPLE_ID;
        assert_eq!(
            rewrite_freenet_href(&format!("http://127.0.0.1:7509/v1/contract/web/{id}/foo")),
            Some(format!("/v1/contract/web/{id}/foo"))
        );
        assert_eq!(
            rewrite_freenet_href(&format!("https://gw.example/v1/contract/web/{id}/#hash")),
            Some(format!("/v1/contract/web/{id}/#hash"))
        );
        let ok = |s: &str| rewrite_freenet_href(s).is_some();
        for bad in [
            // Invalid base58 (`0`, `O`, `I`, `l`), wrong lengths, other schemes, other paths.
            format!("http://h/v1/contract/web/{}0{}/", &id[..20], &id[21..]),
            format!("http://h/v1/contract/web/{}/", &id[..42]),
            format!("http://h/v1/contract/web/{id}ZZ/"),
            format!("ftp://h/v1/contract/web/{id}/"),
            format!("javascript://h/v1/contract/web/{id}/"),
            format!("http://h/v1/contract/other/{id}/"),
            format!("http://h/v1/contract/web/{id}/%2e%2e/x"),
            format!("http://h/v1/contract/web/{id}/../x"),
        ] {
            assert!(!ok(&bad), "{bad}");
        }
        assert!(ok(&format!(
            "http://h/v1/contract/web/{id}/x?next=../y#../z"
        )));
    }

    #[test]
    fn marker_in_query_or_userinfo_is_not_beautified() {
        let id = SAMPLE_ID;
        assert_eq!(
            beautify_freenet_label(&format!("http://h/x?u=http://g/v1/contract/web/{id}/")),
            None
        );
        assert_eq!(
            beautify_freenet_label(&format!("http://u@h/other/v1/contract/web/{id}/")),
            None
        );
    }

    #[test]
    fn an_escaped_label_shows_literally() {
        let t = "freenet:abc_def *x* [y](z) <a> &amp; 1. #h";
        let md = escape_md(t);
        let mut shown = String::new();
        for ev in Parser::new_ext(&md, options()) {
            if let Event::Text(s) = ev {
                shown.push_str(&s);
            }
        }
        assert_eq!(shown, t);
    }

    #[test]
    fn a_code_span_shows_the_text_whatever_it_holds() {
        for t in ["http://x/", "a`b", "`a", "a``b``c", " x "] {
            let md = code_span(t, 0);
            let got: Vec<_> = Parser::new_ext(&md, options())
                .filter_map(|e| match e {
                    Event::Code(c) => Some(c.to_string()),
                    _ => None,
                })
                .collect();
            assert_eq!(got.len(), 1, "{t:?}: {md:?}");
            assert_eq!(got[0].trim(), t.trim(), "{t:?}: {md:?}");
        }
    }

    #[test]
    fn bidi_and_format_characters_are_stripped_from_link_labels() {
        for md in [
            "[evil\u{202e}gnp.exe](https://freenet.org/a)",
            "[zero\u{200b}width](https://freenet.org/a)",
            "see [a\u{2066}b\u{2069}](https://freenet.org/a) and https://freenet.org/b",
        ] {
            let out = prepare(md);
            assert!(
                !out.chars().any(super::is_format_or_control),
                "{md:?} -> {out:?}"
            );
            assert!(!opens(&out).is_empty());
        }
        // Outside a link, an emoji's joiner is left as it is.
        let family = "x \u{1f469}\u{200d}\u{1f467}";
        assert_eq!(prepare(family), family);
    }
}
