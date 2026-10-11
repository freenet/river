//! Message text to HTML: markdown, links (Freenet share links and gateway
//! URLs), @mention chips, and the per-message render cache.

use crate::util::display_name::sanitize_display_name;
use river_core::room_state::member::MemberId;
use river_core::room_state::message::MessageId;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Clean a quoted reply-preview snapshot for display: resolve `@[name](rv:id)`
/// mention tokens to plain `@name` (using each member's *current* nickname, with
/// the token snapshot as fallback) and strip markdown formatting, so the preview
/// reads as plain text. Caller truncates the result.
pub(super) fn clean_reply_preview(text: &str, member_names: &HashMap<MemberId, String>) -> String {
    use river_core::mention::{parse_segments, MentionSegment};

    // Mirrors `river_core::mention::render_plaintext`, except the fallback
    // display name — the `[name]` snapshot the SENDER embedded in the message
    // — is sanitised. That snapshot is arbitrary attacker text, so an
    // unsanitised fallback would let `@[Alice 🛡](rv:…)` paint a shield into a
    // reply preview for a member id nobody in the room resolves. Names that DO
    // resolve come from `member_names`, which is already sanitised at source.
    let mut with_mentions = String::with_capacity(text.len());
    for seg in parse_segments(text) {
        match seg {
            MentionSegment::Text(t) => with_mentions.push_str(&t),
            MentionSegment::Mention(m) => {
                let name = member_names
                    .iter()
                    .find(|(id, _)| m.member_ref.matches(**id))
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| sanitize_display_name(&m.display_name));
                with_mentions.push('@');
                with_mentions.push_str(&name);
            }
        }
    }
    strip_markdown(&with_mentions)
}

/// Reduce markdown to its plain-text content (emphasis/headings/code-fences
/// removed, links rendered as their visible text). Used for the single-line
/// reply-preview snapshot, never for the message body (which renders full
/// markdown). Falls back to the input unchanged if parsing fails.
fn strip_markdown(text: &str) -> String {
    // The preview is truncated anyway, so parse at most a bounded prefix.
    let text = truncate_to_char_boundary(text, MARKDOWN_MAX_SOURCE_BYTES);
    if !markdown_cost_is_bounded(text) {
        return text.to_string();
    }
    match markdown::to_mdast(text, &markdown::ParseOptions::gfm()) {
        Ok(node) => {
            let mut out = String::with_capacity(text.len());
            collect_mdast_text(&node, &mut out);
            drop_mdast(node);
            out
        }
        Err(_) => text.to_string(),
    }
}

/// Depth-first collection of the visible text from a markdown AST node.
///
/// Iterative, not recursive: nesting depth is chosen by whoever wrote the
/// text, and a recursive walk of a deep tree overflows the stack.
fn collect_mdast_text(root: &markdown::mdast::Node, out: &mut String) {
    use markdown::mdast::Node;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node {
            Node::Text(t) => out.push_str(&t.value),
            Node::InlineCode(c) => out.push_str(&c.value),
            Node::Code(c) => out.push_str(&c.value),
            // A hard/soft break or thematic break becomes a space so words on
            // separate lines don't run together in the single-line preview.
            Node::Break(_) | Node::ThematicBreak(_) => out.push(' '),
            _ => {}
        }
        if let Some(children) = node.children() {
            stack.extend(children.iter().rev());
        }
    }
}

/// Drop a markdown AST without recursing: the tree's own `Drop` recurses once
/// per nesting level, which overflows the stack on deeply nested text.
fn drop_mdast(root: markdown::mdast::Node) {
    let mut stack = vec![root];
    while let Some(mut node) = stack.pop() {
        if let Some(children) = node.children_mut() {
            stack.append(children);
        }
    }
}

/// The longest prefix of `text` that is at most `max` bytes and ends on a
/// char boundary.
fn truncate_to_char_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Convert message text to HTML with clickable links that open in new tabs.
///
/// Uses GFM autolink literals to linkify plain URLs while correctly
/// skipping URLs inside code spans and other non-text contexts.
/// Re-exported as `pub(crate)` so the DM thread renderer can share the
/// same linkify + Freenet-URL-rewrite path as room messages.
pub(crate) fn message_to_html(text: &str) -> String {
    message_to_html_inner(text, running_behind_freenet_gateway())
}

fn message_to_html_inner(text: &str, behind_gateway: bool) -> String {
    if !markdown_cost_is_bounded(text) {
        return plain_text_to_html(text);
    }

    // Convert single newlines to hard breaks (two spaces + newline)
    // This preserves line breaks in chat messages as users expect
    let with_hard_breaks = text.replace("\n", "  \n");

    markdown_to_html(&with_hard_breaks, text, behind_gateway)
}

/// Longest text rendered as markdown.
const MARKDOWN_MAX_SOURCE_BYTES: usize = 4096;

/// Most block containers (`>`, `-`, `1.` ...) opened at the start of one line.
const MARKDOWN_MAX_LINE_NESTING: usize = 16;

/// Most `|` on one line, which bounds the columns of a table.
const MARKDOWN_MAX_LINE_PIPES: usize = 32;

/// Longest HTML kept from rendering markdown (see `markdown_to_html`).
const MARKDOWN_MAX_HTML_BYTES: usize = 64 * 1024;

/// Whether `text` is cheap enough to parse as markdown. The parser's time
/// grows faster than linearly with nesting depth, and a table's output with
/// columns times rows, so text past these limits is shown as plain text.
/// Depth is counted per line. Nesting built up across lines needs growing
/// indentation, which the size limit bounds to a few hundred levels (a few
/// ms to parse at the default 1000-byte message size, tens of ms at the size
/// limit). Only DMs and rooms that raised `max_message_size` above the
/// default can carry text past the size limit, which is large enough for the
/// longest valid share link (see `longest_valid_bare_link_still_converts`).
fn markdown_cost_is_bounded(text: &str) -> bool {
    if text.len() > MARKDOWN_MAX_SOURCE_BYTES {
        return false;
    }
    // Characters that can hide a marker from the count below but not from
    // the parser: a leading byte order mark, which the parser skips, and the
    // sentinels `extract_bare_freenet_links` may drop before parsing.
    let ignored = ['\u{feff}', BARE_LINK_OPEN, BARE_LINK_CLOSE];
    let text: std::borrow::Cow<str> = if text.contains(ignored) {
        text.replace(ignored, "").into()
    } else {
        text.into()
    };
    // `\r` alone also ends a line in markdown.
    text.split(['\n', '\r']).all(|line| {
        line_container_depth(line) <= MARKDOWN_MAX_LINE_NESTING
            && line.bytes().filter(|&b| b == b'|').count() <= MARKDOWN_MAX_LINE_PIPES
    })
}

/// How many block quote, list item or footnote definition markers open at
/// the start of `line` (an over-count is fine: it only makes plain text more
/// likely).
fn line_container_depth(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut depth = 0;
    loop {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let marker_end = match bytes.get(i) {
            Some(b'>') => i + 1,
            // A GFM footnote definition, `[^label]: `.
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
        // A list marker must be followed by whitespace or the line's end.
        let is_quote = bytes[i] == b'>';
        if !is_quote && !matches!(bytes.get(marker_end), None | Some(b' ' | b'\t')) {
            return depth;
        }
        depth += 1;
        i = marker_end;
    }
}

/// Render a room description: markdown, or plain text when
/// [`markdown_cost_is_bounded`] says no.
pub(super) fn description_to_html(text: &str, behind_gateway: bool) -> String {
    if !markdown_cost_is_bounded(text) {
        plain_text_to_html(text)
    } else {
        markdown_to_html(text, text, behind_gateway)
    }
}

/// Text not rendered as markdown: escaped, with each line break kept.
fn plain_text_to_html(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    format!("<p>{}</p>", escape_html(&text).replace('\n', "<br />\n"))
}

/// Convert markdown text to HTML with clickable links that open in new tabs.
///
/// `plain` is what to show as plain text if the HTML comes out too long: the
/// text as the user wrote it, before any markdown-specific rewriting.
fn markdown_to_html(original_text: &str, plain: &str, behind_gateway: bool) -> String {
    // GFM only autolinks http(s)/www text, so a bare `freenet:<id>` share link
    // would stay plain text. Pull valid ones out BEFORE markdown runs, so
    // emphasis/strikethrough syntax inside a link (`…/a*b*c`) cannot split it,
    // and put them back as anchors afterwards; turning an anchor into a link to
    // the reader's node is `finalize_anchors`' job, the same as for every other
    // share link. Only behind a gateway, because without one there is no node
    // to link to (see `finalize_anchors`).
    let (text, bare_links) = if behind_gateway {
        extract_bare_freenet_links(original_text)
    } else {
        (std::borrow::Cow::Borrowed(original_text), Vec::new())
    };

    // Some shapes (many references to a definition with a long title, a
    // table with many rows) expand a short message into megabytes, so HTML
    // past `MARKDOWN_MAX_HTML_BYTES` is dropped for plain text before any
    // further work is done on it.
    let too_long = |html: &String| html.len() > MARKDOWN_MAX_HTML_BYTES;
    let rendered = render_gfm(&text);
    if too_long(&rendered) {
        return plain_text_to_html(plain);
    }
    let html = if bare_links.is_empty() {
        rendered
    } else {
        // The source scan and the render parse slightly different text (a
        // sentinel in place of each link), so in rare shapes they disagree
        // about what is prose, e.g. a link's own `(` inside a link
        // destination. If any link did not come back exactly once, in prose,
        // or a sentinel survived anywhere, render the original text with no
        // bare-link pass rather than show a mangled message.
        match restore_bare_freenet_links(&rendered, &bare_links) {
            Some(html) => html,
            None => render_gfm(original_text),
        }
    };
    if too_long(&html) {
        return plain_text_to_html(plain);
    }

    let html = finalize_anchors(&html, behind_gateway);
    if too_long(&html) {
        return plain_text_to_html(plain);
    }
    html
}

/// Convert markdown to HTML using GFM mode, which includes autolink literals
/// that correctly handle code spans, existing links, etc.
fn render_gfm(text: &str) -> String {
    markdown::to_html_with_options(text, &markdown::Options::gfm())
        .unwrap_or_else(|_| markdown::to_html(text))
}

/// Render message text to HTML, turning `@[name](rv:id)` mention tokens into
/// styled, clickable chips that show each member's *current* nickname.
///
/// Mentions are extracted *before* markdown runs — each is replaced by an inert
/// private-use sentinel, markdown + anchor finalization run over the sentinel'd
/// text, then the sentinels are swapped for chip HTML. This keeps mention
/// rendering independent of markdown's link grammar (so a token can never be
/// mangled by adjacent markdown, and a malicious `[..](javascript:..)` payload
/// can't masquerade as a mention).
///
/// `member_names` maps each member id to their decrypted current nickname (the
/// `[name]` snapshot in the token is only used as a fallback when the id is not
/// in the map). `self_member_id` gets a distinct highlight (a mention of you);
/// `None` (no locally-known identity) simply means no chip is self-highlighted.
pub(crate) fn message_to_html_with_mentions(
    text: &str,
    member_names: &HashMap<MemberId, String>,
    self_member_id: Option<MemberId>,
) -> String {
    use river_core::mention::{parse_segments, MentionSegment};

    let segments = parse_segments(text);
    // Fast path: no mentions -> byte-identical to the plain renderer.
    if !segments
        .iter()
        .any(|s| matches!(s, MentionSegment::Mention(_)))
    {
        return message_to_html(text);
    }

    // Private-use sentinels that markdown passes through verbatim and that
    // never legitimately appear in chat text. Strip any pre-existing
    // occurrences from plain-text runs so a crafted message can't smuggle a
    // sentinel and hijack the post-markdown substitution.
    const OPEN: char = '\u{E000}';
    const CLOSE: char = '\u{E001}';

    let mut working = String::with_capacity(text.len());
    let mut chips: Vec<String> = Vec::new();
    for seg in segments {
        match seg {
            MentionSegment::Text(t) => {
                working.extend(t.chars().filter(|c| *c != OPEN && *c != CLOSE));
            }
            MentionSegment::Mention(m) => {
                let idx = chips.len();
                // The token's reference is the member's short (truncated-base32)
                // label; recover the full id by matching it against the room's
                // known members so the chip stays clickable and self-highlighted.
                // An unknown member yields `None` -> non-clickable snapshot chip.
                let resolved = m.member_ref.resolve(member_names.keys().copied());
                // The `[name]` snapshot is sender-controlled text, so an
                // unresolved mention must be sanitised before it becomes a
                // chip — otherwise `@[Admin 🛡](rv:…)` paints a shield inside
                // a message. Resolved names come from `member_names`, which is
                // already sanitised at source.
                let name = resolved
                    .and_then(|id| member_names.get(&id).cloned())
                    .unwrap_or_else(|| sanitize_display_name(&m.display_name));
                chips.push(render_mention_chip_html(
                    resolved,
                    &name,
                    // `self_member_id.is_some_and(..)` rather than
                    // `resolved == self_member_id`: the latter would call an
                    // UNRESOLVED mention a mention of you when the local
                    // identity is also unknown (`None == None`).
                    self_member_id.is_some_and(|me| resolved == Some(me)),
                ));
                working.push(OPEN);
                working.push_str(&idx.to_string());
                working.push(CLOSE);
            }
        }
    }

    let mut html = message_to_html(&working);
    // Markdown can move a placeholder into a tag (a link title) or copy it
    // (a reference definition's title, used many times). Substituting there
    // would put chip markup inside an attribute or multiply it, so unless
    // every placeholder comes back exactly once, in text, show the message
    // as plain text, where each one does.
    if !mention_placeholders_are_sound(&html, chips.len(), OPEN, CLOSE) {
        html = plain_text_to_html(&working);
    }
    // The CLOSE delimiter bounds each index, so `…0␁` never matches inside
    // `…10␁` — replacement is unambiguous regardless of order.
    for (idx, chip) in chips.iter().enumerate() {
        html = html.replace(&format!("{OPEN}{idx}{CLOSE}"), chip);
    }
    html
}

/// Whether each of the `count` mention placeholders (`{open}{index}{close}`)
/// appears exactly once in `html`, and only in text, never inside a tag. The
/// markdown crate encodes `>` inside attributes, so the first `>` always ends
/// a tag.
fn mention_placeholders_are_sound(html: &str, count: usize, open: char, close: char) -> bool {
    let mut seen = vec![0usize; count];
    let mut in_tag = false;
    for (i, c) in html.char_indices() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if c == open => {
                if in_tag {
                    return false;
                }
                let rest = &html[i + open.len_utf8()..];
                let Some(end) = rest.find(close) else {
                    return false;
                };
                match rest[..end].parse::<usize>() {
                    Ok(idx) if idx < count => seen[idx] += 1,
                    _ => return false,
                }
            }
            _ => {}
        }
    }
    seen.iter().all(|&n| n == 1)
}

thread_local! {
    /// Per-message rendered-HTML cache for message bodies.
    ///
    /// The `message_groups` memo rebuilds the *entire* visible message list
    /// whenever `ROOMS` changes — which is on every sync tick, incoming
    /// message, reaction, and DM. Rendering one body runs a full markdown
    /// parse + HTML serialize + mention/anchor rewrite (tens of µs natively,
    /// several-fold more in WASM on a mobile CPU). Re-parsing all
    /// `max_recent_messages` (default 100) bodies on every update was a major
    /// source of the mobile jank users reported ("really slows down the mobile
    /// browser").
    ///
    /// This cache keys each body's HTML by `MessageId` plus a fingerprint of
    /// every input that affects the output, so steady-state message flow
    /// re-parses only the one new/changed message instead of the whole
    /// history. It is single-threaded (WASM) / per-thread (native tests) and
    /// is pruned each render (`prune_message_html_cache`) to the currently
    /// visible set, so it stays bounded to the current room's messages.
    static MESSAGE_HTML_CACHE: RefCell<HashMap<MessageId, (u64, String)>> =
        RefCell::new(HashMap::new());
}

/// Order-independent fingerprint of the member-name map (mention chips resolve
/// to members' *current* nicknames, so a rename must invalidate any cached body
/// that renders a chip). Combining per-entry hashes with `wrapping_add` makes
/// the result independent of `HashMap` iteration order, so identical logical
/// content always fingerprints identically. `len()` is folded in to
/// distinguish an empty map from an all-zero-hash one.
pub(super) fn member_names_fingerprint(member_names: &HashMap<MemberId, String>) -> u64 {
    let mut acc: u64 = member_names.len() as u64;
    for (id, name) in member_names {
        let mut h = DefaultHasher::new();
        id.hash(&mut h);
        name.hash(&mut h);
        acc = acc.wrapping_add(h.finish());
    }
    acc
}

/// Fingerprint of every input that determines a message body's rendered HTML.
///
/// The body depends on the effective `text` always, and — only when the text
/// can carry a mention (`rv:` reference scheme present) — on the member-name
/// map (chip nicknames) and the local member id (self-mention highlight).
/// `running_behind_freenet_gateway()` is constant per session, so it is not
/// part of the key. Gating the member inputs on the presence of a mention
/// means an ordinary (mention-free) message survives member joins/renames in
/// the cache, while a message that renders a chip is invalidated by them.
fn message_html_fingerprint(text: &str, members_fp: u64, self_member_id: Option<MemberId>) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    if text.contains(river_core::mention::REF_SCHEME) {
        members_fp.hash(&mut h);
        self_member_id.hash(&mut h);
    }
    h.finish()
}

/// Cached wrapper around [`message_to_html_with_mentions`]. Returns the cached
/// HTML on a fingerprint match, otherwise renders it and stores it. The output
/// is always byte-identical to calling `message_to_html_with_mentions` directly
/// with the current inputs — the cache only skips redundant re-rendering.
pub(super) fn render_message_html_cached(
    message_id: &MessageId,
    text: &str,
    member_names: &HashMap<MemberId, String>,
    members_fp: u64,
    self_member_id: Option<MemberId>,
) -> String {
    let fp = message_html_fingerprint(text, members_fp, self_member_id);
    MESSAGE_HTML_CACHE.with(|cache| {
        if let Some((cached_fp, html)) = cache.borrow().get(message_id) {
            if *cached_fp == fp {
                return html.clone();
            }
        }
        let html = message_to_html_with_mentions(text, member_names, self_member_id);
        cache
            .borrow_mut()
            .insert(message_id.clone(), (fp, html.clone()));
        html
    })
}

/// Drop cached entries for messages no longer visible (e.g. after a room
/// switch or history trim), keeping the cache bounded to the current room's
/// message set.
pub(super) fn prune_message_html_cache(seen: &std::collections::HashSet<MessageId>) {
    MESSAGE_HTML_CACHE.with(|cache| {
        cache.borrow_mut().retain(|id, _| seen.contains(id));
    });
}

/// Test-only: reset the cache so each test observes a clean cache regardless of
/// thread reuse across the test binary.
#[cfg(test)]
fn clear_message_html_cache() {
    MESSAGE_HTML_CACHE.with(|cache| cache.borrow_mut().clear());
}

/// Build the inline chip markup for one mention. `name` is the resolved current
/// nickname (or snapshot fallback) and is HTML-escaped — nicknames are
/// attacker-controlled and this string goes through `dangerous_inner_html`
/// (freenet/river#227).
///
/// `id` is the resolved member id, or `None` when the token's short reference
/// names a member this client doesn't know. When present, `data-member-id`
/// carries the lossless hex id (an in-session, full-precision handoff — NOT the
/// wire token) so the document-level click interceptor can open the member-info
/// modal. When absent the chip still renders the `@name` but is inert (nothing
/// to open), which is the correct degradation for an unknown member.
fn render_mention_chip_html(id: Option<MemberId>, name: &str, is_self: bool) -> String {
    let class = if is_self {
        "river-mention river-mention-self"
    } else {
        "river-mention"
    };
    let data_member_id = match id {
        Some(id) => format!(
            " data-member-id=\"{}\"",
            river_core::mention::member_id_to_hex(id)
        ),
        None => String::new(),
    };
    format!(
        "<span class=\"{class}\" data-river-mention=\"1\"{data_member_id} \
         role=\"button\" tabindex=\"0\" title=\"@{title}\">@{label}</span>",
        title = escape_html_attr(name),
        label = escape_html(name),
    )
}

/// Escape `&<>` for HTML text content.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape `&<>"'` for an HTML attribute value (double-quoted).
fn escape_html_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// True when River is currently being served from a path under
/// `/v1/contract/web/` (or `/v2/…`), which is what gateway hosting looks like to the
/// browser. Returning false here suppresses the host-stripping href rewrite
/// for `dx serve`, `cargo make dev-example`, and the static-server flows
/// documented in AGENTS.md, where rewriting `https://gw.example/v1/...` to
/// `/v1/...` would only break the link (the dev server has no gateway
/// behind it). Label beautification is unconditional — that's purely
/// cosmetic and doesn't depend on the hosting environment.
#[cfg(target_arch = "wasm32")]
pub(super) fn running_behind_freenet_gateway() -> bool {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .map(|p| p.starts_with("/v1/contract/web/") || p.starts_with("/v2/contract/web/"))
        .unwrap_or(false)
}

/// Native test builds: default to true so existing tests verify the
/// production (gateway-hosted) behavior. Tests covering the dev-mode path
/// call `message_to_html_inner` with an explicit `false` flag.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn running_behind_freenet_gateway() -> bool {
    true
}

/// Walk anchor tags in HTML once and:
///
/// - Add `target="_blank" rel="noopener noreferrer"` to every anchor.
/// - When `rewrite_freenet_hrefs` is true, rewrite Freenet web-contract URLs
///   to a host/port-agnostic same-origin absolute path so the link works for
///   any reader regardless of which gateway they're connected to. The flag
///   is only true when River itself is hosted under `/v1/contract/web/...`
///   (i.e. behind a gateway). In `dx serve` / dev-example / static-server
///   modes there is no gateway to redirect to, so the original absolute URL
///   is left in place — letting the user reach the embedded gateway directly.
/// - For bare Freenet web URLs (where the visible text equals the original
///   href), shorten the label to `freenet:<id-prefix>[/<path>]` regardless of
///   hosting. User-supplied link text from `[label](url)` is left alone.
///
/// Assumes the markdown crate emits anchors as `<a href="...">...</a>` with
/// `href` as the first attribute. If that ever changes, target/rel injection
/// silently no-ops and href rewrite + label beautification are skipped.
fn finalize_anchors(html: &str, rewrite_freenet_hrefs: bool) -> String {
    let mut out = String::with_capacity(html.len() + 32);
    let mut rest = html;
    while let Some(pos) = rest.find("<a ") {
        out.push_str(&rest[..pos]);
        let tag = &rest[pos..];
        let Some(open_end) = tag.find('>') else {
            out.push_str(tag);
            return out;
        };
        let opening = &tag[..=open_end];
        let after_open = &tag[open_end + 1..];
        let Some(close_pos) = matching_anchor_close(after_open) else {
            out.push_str(tag);
            return out;
        };
        // The markdown crate still emits an anchor for an angle-bracket
        // autolink inside a link label (`[<x:y>](url)`), i.e. a nested `<a>`.
        // Browsers split those into two anchors, so the inner one would escape
        // this function (no target/rel, and an empty href that reloads River
        // in place). Flatten it to its text: the outer link is what is shown.
        let flattened;
        let inner = if after_open[..close_pos].contains("<a ") {
            flattened = strip_anchor_tags(&after_open[..close_pos]);
            flattened.as_str()
        } else {
            &after_open[..close_pos]
        };
        let tail = &after_open[close_pos + 4..];

        let opening = opening.replacen(
            "<a href=\"",
            "<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"",
            1,
        );
        let original_href = extract_href(&opening);

        // A share link (freenet.org/open or `freenet:`) opens the named webapp
        // on the reader's own node. The destination is derived ONLY from the
        // visible text, never from the hidden href, and the text is kept as
        // it is (full contract id included): the link goes exactly where it
        // says, so `[freenet:<A>](https://freenet.org/open#<B>)` opens A, and
        // a share link hidden behind a label is left as a plain link to the
        // freenet.org page, which shows the id before anything opens. (Pasted
        // gateway URLs, `http://…/v1/contract/web/<id>/…`, are a separate,
        // older rewrite below, which does apply to labelled links.)
        if rewrite_freenet_hrefs {
            if let Some(target) = share_link_in_anchor_text(inner) {
                // Rebuilt from scratch rather than edited, so nothing else
                // from the message survives on the anchor: a `title` naming
                // some other link would otherwise show as its tooltip.
                out.push_str(&format!(
                    "<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"{}\">",
                    escape_html_attr(&target.local_path())
                ));
                out.push_str(inner);
                out.push_str("</a>");
                rest = tail;
                continue;
            }
        }

        // A link whose href opens a contract on the reader's node (a gateway
        // URL, which the rewrite below makes same-origin, or a relative href,
        // which already is) must not carry a label that visibly names a
        // DIFFERENT contract: show such a label as plain text rather than as
        // a link that is not what it says. A bare pasted link (its text is
        // the URL) names its own destination and is exempt.
        if rewrite_freenet_hrefs {
            if let Some(dest_id) = node_destination(original_href.as_deref()) {
                // A pasted absolute URL (its text IS the href) shows where it
                // goes, host and contract id first, so its label is not
                // checked (a River invite's long `?invitation=` code, or an
                // app path holding other ids, is not a claim). Unless its
                // destination is unknown AND something in it moves where it
                // lands unreadably (a dot segment, a percent-escape).
                let bare =
                    original_href.as_deref() == Some(inner) && has_scheme_and_authority(inner);
                // A tooltip is label text too.
                let title = extract_attr(&opening, "title");
                let unlink = if bare {
                    (dest_id.is_none() && !plain_absolute_url(inner))
                        || pasted_url_poses_before_its_path(inner)
                        || title.as_deref().is_some_and(|t| {
                            label_contradicts_destination(t, None, dest_id.clone())
                        })
                } else {
                    label_contradicts_destination(inner, title.as_deref(), dest_id)
                };
                if unlink {
                    out.push_str(inner);
                    rest = tail;
                    continue;
                }
            }
        }
        let opening = if rewrite_freenet_hrefs {
            match original_href.as_deref().and_then(rewrite_freenet_href) {
                Some(new_href) => {
                    let orig = original_href.as_deref().unwrap();
                    opening.replacen(
                        &format!("href=\"{orig}\""),
                        &format!("href=\"{new_href}\""),
                        1,
                    )
                }
                None => opening,
            }
        } else {
            opening
        };
        let new_inner = match original_href.as_deref() {
            Some(h) if h == inner => beautify_freenet_label(h).unwrap_or_else(|| inner.to_string()),
            _ => inner.to_string(),
        };

        out.push_str(&opening);
        out.push_str(&new_inner);
        out.push_str("</a>");
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// If an anchor's visible text is exactly a valid share link, return it.
///
/// `inner` is the anchor's inner HTML as the markdown crate emits it: text is
/// entity-encoded (`&amp;`, `&lt;`, `&gt;`, `&quot;`), and any nested markup
/// (emphasis, a mention sentinel's chip, ...) means it is not a bare link.
fn share_link_in_anchor_text(inner: &str) -> Option<crate::util::share_link::ShareTarget> {
    // Defense in depth: the validator refuses `<` and `>` anyway, but nested
    // markup is never a bare link, so do not even decode it.
    if inner.contains('<') {
        return None;
    }
    crate::util::share_link::parse_share_link(&decode_html_text(inner))
}

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
    if path.split('/').any(crate::util::share_link::is_dot_segment) {
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

/// True if an anchor's visible text names a contract other than `dest_id`
/// (or names any contract, when the destination is unknown).
///
/// Best effort, as defence in depth for links River did not write: what counts
/// as naming a contract is judged on what the reader sees
/// ([`crate::util::confusable::visual_ascii`] of the decoded text, tags
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
    dest_id: Option<String>,
) -> bool {
    // The label, the link's tooltip, and any tooltip of an image inside it.
    let mut claims = label_claimed_ids(inner);
    let img_titles = inner
        .split("<img")
        .skip(1)
        .filter_map(|rest| extract_attr(&format!("<img{}", rest.split('>').next()?), "title"));
    for text in title.map(str::to_string).into_iter().chain(img_titles) {
        claims.extend(label_claimed_ids(&text));
    }
    if claims.is_empty() {
        return false;
    }
    let Some(dest_id) = dest_id else { return true };
    claims
        .iter()
        .any(|claim| !dest_id.starts_with(claim.as_str()))
}

/// The contract-id claims in an anchor's visible text. See
/// [`label_contradicts_destination`].
fn label_claimed_ids(inner: &str) -> Vec<String> {
    // Anything that can split an id without the reader seeing a break must
    // not (the whitespace and joining rules below). This is a comparison, not
    // rendering, so joining too much can only unlink more, never link
    // something new.
    let decoded = decode_html_text(&visible_text_with_alt(inner));
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
    let folded: Vec<char> = crate::util::confusable::visual_ascii(&spaced)
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

/// True if a pasted URL's text before its contract path could be read as a
/// different destination: any userinfo (`http://freenet:<A>@host/…`), or a
/// contract id or `freenet:` claim in the host (`http://<A>.example/…`).
/// What follows the contract path is the app's own business (a River
/// invite's long code, an app path holding other ids) and is not read.
fn pasted_url_poses_before_its_path(url: &str) -> bool {
    let decoded = decode_html_text(url);
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
    !path.split('/').any(crate::util::share_link::is_dot_segment)
}

/// The raw (still entity-encoded) value of attribute `name` in a tag emitted
/// by the markdown crate, which always double-quotes values and encodes `"`
/// inside them.
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let marker = format!(" {name}=\"");
    let start = tag.find(&marker)? + marker.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}

/// An anchor's inner HTML reduced to what a reader sees: tags dropped, except
/// that an image contributes its `alt` text (which the browser shows when the
/// image does not load).
fn visible_text_with_alt(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find('<') {
        out.push_str(&rest[..pos]);
        let Some(end) = rest[pos..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[pos..pos + end + 1];
        // No padding: a broken image shows its alt inline, and an empty one
        // shows nothing, so neither must split what the reader sees as one id.
        if tag.starts_with("<img") {
            if let Some(alt) = extract_attr(tag, "alt") {
                out.push_str(&alt);
            }
        }
        rest = &rest[pos + end + 1..];
    }
    out.push_str(rest);
    out
}

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

/// Undo the markdown crate's text encoding (`&amp;`, `&lt;`, `&gt;`, `&quot;`)
/// in one pass, so `&amp;lt;` decodes to `&lt;`, not `<`. Anything else is
/// left verbatim.
fn decode_html_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let (decoded, len) = [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
        ]
        .iter()
        .find(|(entity, _)| tail.starts_with(entity))
        .map(|(entity, c)| (*c, entity.len()))
        .unwrap_or(('&', 1));
        out.push(decoded);
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// Encode text content the way the markdown crate does (`&"<>`), so a text run
/// that is split around a new anchor re-encodes to exactly what it was.
fn encode_html_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Private-use sentinels that stand in for a bare `freenet:` link while
/// markdown runs (`{BARE_LINK_OPEN}<index>{BARE_LINK_CLOSE}`). Distinct from the
/// mention sentinels (`U+E000`/`U+E001`), and stripped from the input before
/// any are inserted, so a message cannot forge one.
const BARE_LINK_OPEN: char = '\u{E002}';
const BARE_LINK_CLOSE: char = '\u{E003}';

/// Longest whitespace-free run examined as a bare `freenet:` link candidate:
/// the longest link the validator can accept, plus slack for trailing
/// punctuation that trimming would remove.
const MAX_BARE_LINK_CANDIDATE_LEN: usize = crate::util::share_link::MAX_SHARE_LINK_LEN + 128;

/// Find each bare, valid `freenet:` share link in the markdown SOURCE and
/// replace it with a sentinel, returning the rewritten source and the links in
/// sentinel order. Messages with no `freenet:` at all are returned untouched.
///
/// A candidate starts at `freenet:` (any case) that does not follow a letter
/// or digit, runs to the next whitespace, and then loses trailing punctuation
/// exactly as the markdown crate's GFM autolink does (`!"'),.:;?_~*`, with `)`
/// only when unbalanced), so "see freenet:<id>." links the id without the full
/// stop. A `.` that follows `/` or `.` is kept, so trimming can never turn a
/// refused dot segment (`…/a/..`) into an accepted link (`…/a/`). Only a
/// candidate that passes the shared share-link validation is taken.
///
/// A candidate inside any construct whose text is not rendered as plain prose
/// (code, links and autolinks, images, raw HTML, definitions, footnote
/// references) is left alone. Those are found by parsing the source once more
/// to an mdast, which only happens when a valid candidate exists.
fn extract_bare_freenet_links(text: &str) -> (std::borrow::Cow<'_, str>, Vec<String>) {
    use std::borrow::Cow;
    const SCHEME: &str = "freenet:";
    if !text.to_ascii_lowercase().contains(SCHEME) {
        return (Cow::Borrowed(text), Vec::new());
    }
    let clean: String = text
        .chars()
        .filter(|c| *c != BARE_LINK_OPEN && *c != BARE_LINK_CLOSE)
        .collect();

    // ASCII lowercasing keeps byte offsets identical to `clean`.
    let lower = clean.to_ascii_lowercase();
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    let mut search_from = 0;
    // End of the whitespace-free run the current candidate sits in, cached so
    // `freenet:freenet:…` does not rescan the same run once per occurrence.
    let mut run_end = 0;
    while let Some(rel) = lower[search_from..].find(SCHEME) {
        let start = search_from + rel;
        search_from = start + SCHEME.len();
        if clean[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            continue;
        }
        if run_end <= start {
            // A `|` ends it too: tables split cells on it before inline
            // parsing, which this pass runs ahead of.
            run_end = clean[start..]
                .find(|c: char| c.is_whitespace() || c == '|')
                .map_or(clean.len(), |p| start + p);
        }
        // Message text is attacker-controlled, so bound the work per
        // candidate: anything longer than the longest valid link plus some
        // trailing punctuation cannot validate, so it is not even trimmed.
        if run_end - start > MAX_BARE_LINK_CANDIDATE_LEN {
            continue;
        }
        let mut token = &clean[start..run_end];
        let open_parens = token.matches('(').count();
        let mut close_parens = token.matches(')').count();
        while let Some(last) = token.chars().next_back() {
            let unbalanced_paren = last == ')' && close_parens > open_parens;
            let dot_in_segment = last == '.'
                && matches!(
                    token[..token.len() - 1].chars().next_back(),
                    Some('/' | '.')
                );
            // Non-ASCII punctuation (`。`, `」`, …) can never be part of a
            // valid link, so it is trimmed like ASCII sentence punctuation.
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
        if crate::util::share_link::parse_freenet_link(token).is_some() {
            candidates.push((start, start + token.len()));
            search_from = start + token.len();
        }
    }
    if candidates.is_empty() {
        return (Cow::Borrowed(text), Vec::new());
    }

    let mut excluded = non_prose_ranges(&clean);
    excluded.sort_unstable();
    let mut out = String::with_capacity(clean.len());
    let mut links = Vec::new();
    let mut cursor = 0;
    // Both lists are in source order and the excluded ranges do not nest
    // (the walk does not descend into an excluded node), so one sweep does.
    let mut next_excluded = 0;
    for (start, end) in candidates {
        while next_excluded < excluded.len() && excluded[next_excluded].1 <= start {
            next_excluded += 1;
        }
        if excluded.get(next_excluded).is_some_and(|&(s, _)| s < end) {
            continue;
        }
        out.push_str(&clean[cursor..start]);
        out.push(BARE_LINK_OPEN);
        out.push_str(&links.len().to_string());
        out.push(BARE_LINK_CLOSE);
        links.push(clean[start..end].to_string());
        cursor = end;
    }
    if links.is_empty() {
        return (Cow::Borrowed(text), Vec::new());
    }
    out.push_str(&clean[cursor..]);
    (Cow::Owned(out), links)
}

/// Byte ranges of the markdown source that are NOT rendered as plain prose.
fn non_prose_ranges(source: &str) -> Vec<(usize, usize)> {
    use markdown::mdast::Node;
    // Iterative for the same reason as `collect_mdast_text`. Ranges come out
    // in document order, as a recursive pre-order walk would give them.
    fn walk(root: &Node, out: &mut Vec<(usize, usize)>) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let skip = matches!(
                node,
                Node::InlineCode(_)
                    | Node::Code(_)
                    | Node::Link(_)
                    | Node::LinkReference(_)
                    | Node::Image(_)
                    | Node::ImageReference(_)
                    | Node::Definition(_)
                    | Node::Html(_)
                    | Node::FootnoteReference(_)
                    | Node::FootnoteDefinition(_)
                    | Node::InlineMath(_)
                    | Node::Math(_)
            );
            if skip {
                if let Some(pos) = node.position() {
                    out.push((pos.start.offset, pos.end.offset));
                }
                continue;
            }
            if let Some(children) = node.children() {
                stack.extend(children.iter().rev());
            }
        }
    }
    let mut out = Vec::new();
    match markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        Ok(root) => {
            walk(&root, &mut out);
            drop_mdast(root);
        }
        // Unparseable: treat everything as non-prose, so nothing is linked.
        Err(_) => out.push((0, source.len())),
    }
    out
}

/// Put the links taken out by [`extract_bare_freenet_links`] back into the
/// rendered HTML, each as `<a href="">…</a>`: the shape the markdown crate
/// itself emits for an autolink with a scheme it does not allow
/// (`<freenet:…>`). `finalize_anchors` then gives it its real href from the
/// text.
///
/// Returns `None`, so the caller renders the message without the bare-link
/// pass, unless every link comes back exactly once and in prose: a sentinel
/// inside a tag (an attribute), inside `<a>`, `<code>` or `<pre>`, repeated,
/// missing, or surviving percent-encoded in a URL means the source scan and
/// the render disagreed about the message's structure, and guessing could
/// nest an anchor or break an attribute. The markdown crate encodes `>` inside
/// attributes, so the first `>` always ends a tag.
fn restore_bare_freenet_links(html: &str, links: &[String]) -> Option<String> {
    let has_sentinel = |s: &str| s.contains([BARE_LINK_OPEN, BARE_LINK_CLOSE]);
    let mut restored = vec![0usize; links.len()];
    let mut out = String::with_capacity(html.len() + links.len() * 64);
    let mut skip_depth: usize = 0;
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with('<') {
            let end = rest.find('>')?;
            let tag = &rest[..=end];
            if has_sentinel(tag) {
                return None;
            }
            let (closing, name) = match tag[1..].strip_prefix('/') {
                Some(after) => (true, after),
                None => (false, &tag[1..]),
            };
            let name_end = name
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(name.len());
            if matches!(
                name[..name_end].to_ascii_lowercase().as_str(),
                "a" | "code" | "pre"
            ) {
                if closing {
                    skip_depth = skip_depth.saturating_sub(1);
                } else if !tag.ends_with("/>") {
                    skip_depth += 1;
                }
            }
            out.push_str(tag);
            rest = &rest[end + 1..];
        } else {
            let text_end = rest.find('<').unwrap_or(rest.len());
            let text = &rest[..text_end];
            if skip_depth > 0 {
                if has_sentinel(text) {
                    return None;
                }
                out.push_str(text);
            } else {
                out.push_str(&replace_bare_link_sentinels(text, links, &mut restored)?);
            }
            rest = &rest[text_end..];
        }
    }
    let encoded_sentinel = ["%EE%80%82", "%EE%80%83"]
        .iter()
        .any(|e| out.to_ascii_uppercase().contains(e));
    if restored.iter().any(|&n| n != 1) || has_sentinel(&out) || encoded_sentinel {
        return None;
    }
    Some(out)
}

/// Replace every `{BARE_LINK_OPEN}<index>{BARE_LINK_CLOSE}` in one prose text
/// run with its link's anchor, counting each restore in `restored`. `None` for
/// anything that is not a well-formed sentinel for a known index.
fn replace_bare_link_sentinels(
    s: &str,
    links: &[String],
    restored: &mut [usize],
) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(BARE_LINK_OPEN) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + BARE_LINK_OPEN.len_utf8()..];
        let close = after.find(BARE_LINK_CLOSE)?;
        let idx: usize = after[..close].parse().ok()?;
        let link = links.get(idx)?;
        restored[idx] += 1;
        out.push_str("<a href=\"\">");
        out.push_str(&encode_html_text(link));
        out.push_str("</a>");
        rest = &after[close + BARE_LINK_CLOSE.len_utf8()..];
    }
    out.push_str(rest);
    Some(out)
}

/// Offset of the `</a>` that closes an anchor whose content starts at the
/// beginning of `after_open`, counting any nested `<a …>` opened inside it.
fn matching_anchor_close(after_open: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = 0;
    while i < after_open.len() {
        let rest = &after_open[i..];
        let next_open = rest.find("<a ");
        let next_close = rest.find("</a>")?;
        match next_open {
            Some(o) if o < next_close => {
                depth += 1;
                i += o + 3;
            }
            _ => {
                if depth == 0 {
                    return Some(i + next_close);
                }
                depth -= 1;
                i += next_close + 4;
            }
        }
    }
    None
}

/// Remove every `<a …>` and `</a>` tag from `html`, keeping their content.
fn strip_anchor_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let open = rest.find("<a ");
        let close = rest.find("</a>");
        let (pos, is_open) = match (open, close) {
            (Some(o), Some(c)) if o < c => (o, true),
            (Some(o), None) => (o, true),
            (_, Some(c)) => (c, false),
            (None, None) => break,
        };
        out.push_str(&rest[..pos]);
        rest = if is_open {
            match rest[pos..].find('>') {
                Some(end) => &rest[pos + end + 1..],
                None => "",
            }
        } else {
            &rest[pos + 4..]
        };
    }
    out.push_str(rest);
    out
}

fn extract_href(opening_tag: &str) -> Option<String> {
    let start = opening_tag.find("href=\"")? + "href=\"".len();
    let end = opening_tag[start..].find('"')?;
    Some(opening_tag[start..start + end].to_string())
}

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
    Some(FreenetWebUrl {
        contract_id: &after_marker[..id_end],
        suffix,
        absolute_path: path,
    })
}

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
    path_only
        .split('/')
        .any(crate::util::share_link::is_dot_segment)
}

/// Bitcoin-style base58 alphabet: digits and letters minus the visually
/// ambiguous `0`, `O`, `I`, `l`. A base58 string never contains these four.
fn is_base58_char(c: char) -> bool {
    matches!(c,
        '1'..='9'
        | 'A'..='H' | 'J'..='N' | 'P'..='Z'
        | 'a'..='k' | 'm'..='z'
    )
}

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
    let suffix_seen = crate::util::confusable::visual_ascii(parsed.suffix).to_ascii_lowercase();
    let names_a_contract = parsed
        .suffix
        .split(|c: char| !is_base58_char(c))
        .any(crate::util::share_link::is_valid_contract_id);
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

    /// A mention token carries a `[name]` SNAPSHOT written by the sender. When
    /// the member reference doesn't resolve, that snapshot is what renders —
    /// so `@[Admin 🛡](rv:<nobody>)` would paint a shield inside a message with
    /// no nickname involved at all. Any member can type this into any message,
    /// which makes it a broader vector than the nickname one.
    #[test]
    fn unresolved_mention_chip_snapshot_is_sanitised() {
        let unknown = MemberId(freenet_scaffold::util::FastHash(0xDEAD_BEEF));
        let me = MemberId(freenet_scaffold::util::FastHash(1));
        let token = river_core::mention::encode_mention(unknown, "Admin 🛡");

        let html = message_to_html_with_mentions(&token, &HashMap::new(), Some(me));
        assert!(
            !html.contains('\u{1F6E1}'),
            "an unresolved mention rendered a badge glyph from its snapshot: {html}"
        );
        assert!(
            html.contains("Admin"),
            "the name itself must survive: {html}"
        );
    }

    /// Same snapshot, the other renderer: the quoted reply preview resolves
    /// mention tokens to plain `@name` and falls back to the sender-written
    /// snapshot for an unknown member.
    #[test]
    fn reply_preview_sanitises_an_unresolved_mention_snapshot() {
        let unknown = MemberId(freenet_scaffold::util::FastHash(0xDEAD_BEEF));
        let token = river_core::mention::encode_mention(unknown, "Admin 🛡");

        let preview = clean_reply_preview(&token, &HashMap::new());
        assert!(
            !preview.contains('\u{1F6E1}'),
            "reply preview rendered a badge glyph from a mention snapshot: {preview}"
        );
        assert!(preview.contains("@Admin"), "got: {preview}");
    }

    // A previous revision of this branch sanitised
    // `ReplyContentV1.target_author_name` on both decode paths, because the
    // reply strip rendered that sender-written snapshot. freenet/river#480
    // removed the field from the decoder's return type entirely, so there is
    // no longer a value to sanitise: the quote author comes from
    // `resolve_member_nickname` on live state, covered by
    // `resolve_reply_strip_tests::quote_author_label_is_sanitised`.

    /// Issue #315 — pin that the markdown renderer never passes raw HTML
    /// through to the DOM. `markdown_to_html` feeds attacker-controlled
    /// message text into `markdown::to_html_with_options(_, Options::gfm())`,
    /// whose output is then injected via `dangerous_inner_html` at three
    /// sites (room description, message body, DM body). GFM mode defaults
    /// `allow_dangerous_html = false`, so raw HTML is escaped rather than
    /// emitted as live markup. This test fails loudly if a future `markdown`
    /// upgrade or an `Options` change ever flips that on — which would
    /// re-open the stored-XSS hole closed alongside #227 / #314.
    #[test]
    fn raw_html_is_escaped_not_executed() {
        // Each payload is a classic stored-XSS vector. With
        // allow_dangerous_html=false the leading `<` must be escaped to
        // `&lt;`, so the markup survives as inert text instead of a live
        // element.
        //
        // `<img>` and `<svg>` are deliberately chosen: they are NOT on the
        // GFM tagfilter's neutralization list, so if `allow_dangerous_html`
        // were ever flipped on they would pass through as live `<img …>` /
        // `<svg …>` tags — making these the payloads that actually trip the
        // tripwire. (`<script>`/`<iframe>` are masked by the tagfilter even
        // with dangerous HTML enabled, so they can't distinguish the flip;
        // they're covered below only for the escaping guarantee.)
        let live_tag_vectors = ["<img src=x onerror=alert(1)>", "<svg onload=alert(1)>"];
        for payload in live_tag_vectors {
            let html = message_to_html(payload);
            assert!(
                html.contains("&lt;"),
                "raw HTML payload should be HTML-escaped (expected `&lt;`): \
                 input={payload:?} output={html:?}"
            );
            assert!(
                !html.contains("<img")
                    && !html.contains("<svg")
                    && !html.contains("<iframe")
                    && !html.contains("<script"),
                "raw HTML payload must not survive as an executable tag: \
                 input={payload:?} output={html:?}"
            );
        }

        // `<script>`/`<iframe>` must still be escaped on the safe path.
        for payload in ["<script>alert(1)</script>", "<iframe></iframe>"] {
            let html = message_to_html(payload);
            assert!(
                html.contains("&lt;"),
                "raw HTML should be HTML-escaped (expected `&lt;`): \
                 input={payload:?} output={html:?}"
            );
        }
    }

    /// Issue #315 — pin that the markdown renderer never emits a dangerous
    /// URL scheme in an `href`. GFM mode defaults
    /// `allow_dangerous_protocol = false`, so `javascript:` / `vbscript:` /
    /// `data:` links (whether autolinked `<scheme:...>` or `[text](scheme:...)`)
    /// have their `href` neutralized to empty rather than carrying the
    /// executable scheme into the DOM. This fails loudly if that protection
    /// is ever switched off.
    #[test]
    fn dangerous_url_schemes_are_neutralized() {
        let cases = [
            "<javascript:alert(1)>",
            "[click](javascript:alert(1))",
            "[x](vbscript:msgbox(1))",
            "[d](data:text/html,<script>alert(1)</script>)",
        ];
        for payload in cases {
            let html = message_to_html(payload);
            assert!(
                !html.contains("href=\"javascript:")
                    && !html.contains("href=\"vbscript:")
                    && !html.contains("href=\"data:"),
                "dangerous URL scheme must not reach an href: \
                 input={payload:?} output={html:?}"
            );
        }
    }

    #[test]
    fn bare_url_is_linkified() {
        let html = message_to_html("check out https://freenet.org for more info");
        assert!(
            html.contains(
                r#"<a target="_blank" rel="noopener noreferrer" href="https://freenet.org">"#
            ),
            "bare URL should be linkified with target=_blank: {html}"
        );
    }

    #[test]
    fn url_in_code_span_not_linkified() {
        let html = message_to_html("did you do `curl -fsSL https://freenet.org/install.sh | sh`?");
        assert!(
            !html.contains("<a "),
            "URL inside backticks should NOT be linkified: {html}"
        );
    }

    #[test]
    fn url_in_fenced_code_block_not_linkified() {
        let html = message_to_html("```\ncurl https://freenet.org/install.sh\n```");
        assert!(
            !html.contains("<a "),
            "URL in code block should NOT be linkified: {html}"
        );
    }

    #[test]
    fn markdown_link_preserved() {
        let html = message_to_html("see [Freenet](https://freenet.org)");
        assert!(
            html.contains(r#"href="https://freenet.org">"#),
            "markdown link should be preserved: {html}"
        );
        assert!(
            html.contains(">Freenet</a>"),
            "markdown link text should be preserved: {html}"
        );
    }

    #[test]
    fn newlines_become_hard_breaks() {
        let html = message_to_html("line one\nline two");
        assert!(
            html.contains("<br"),
            "newlines should become hard breaks: {html}"
        );
    }

    /// Issue #158: a blank line between two blocks of text is a paragraph
    /// break, and the Markdown renderer must emit a separate `<p>` for each
    /// so the stylesheet can space them apart. This is the HTML-structure
    /// half of the fix; the CSS half is pinned by
    /// `prose_paragraph_spacing_css_present` below.
    #[test]
    fn blank_line_produces_separate_paragraphs() {
        let html = message_to_html("First paragraph.\n\nSecond paragraph.");
        let paragraphs = html.matches("<p>").count();
        assert_eq!(
            paragraphs, 2,
            "blank-line-separated text should render as two <p> blocks: {html}"
        );
    }

    /// Issue #158 root cause: Tailwind v4's Preflight reset zeroes the
    /// margin on every element, so the `<p>` blocks above collapse into a
    /// single wall of text unless the stylesheet re-adds paragraph spacing.
    /// The message body is rendered inside a `.prose` container, so the
    /// `.prose p` margin rule is what actually makes paragraph breaks
    /// visible. Pin its presence in the source stylesheet so a future
    /// Tailwind bump or CSS refactor that silently drops it fails CI rather
    /// than regressing the rendering. `styles.css` (the compiled output) is
    /// a gitignored build artifact, so we assert against the tracked source.
    #[test]
    fn prose_paragraph_spacing_css_present() {
        const TAILWIND_CSS: &str = include_str!("../../../assets/tailwind.css");
        assert!(
            TAILWIND_CSS.contains(".prose p {"),
            "tailwind.css must keep a `.prose p` rule so Markdown paragraph \
             breaks are visible (issue #158); the Tailwind reset zeroes <p> \
             margins otherwise"
        );
    }

    /// The `> quoted` Markdown a reader types has to reach the stylesheet as a
    /// `<blockquote>` for any of the blockquote rules below to apply at all.
    /// `message_to_html` rewrites every single newline to a hard break
    /// (`"  \n"`) before Markdown runs, so pin that the block construct still
    /// parses through that rewrite.
    #[test]
    fn markdown_quote_renders_as_blockquote() {
        let html = message_to_html("> quoted line\n\nnormal line");
        assert!(
            html.contains("<blockquote>"),
            "`> …` must render as a <blockquote> for the blockquote CSS to \
             apply: {html}"
        );
    }

    /// Extract the declaration body of a CSS rule by its exact selector text.
    ///
    /// Anchored on a LINE START (`"\n{selector} {{"`), not a bare substring:
    /// `.prose blockquote {` is a suffix of `.bg-accent .prose blockquote {`,
    /// so a substring search would silently return the wrong rule's body if
    /// the two were ever reordered in the file.
    fn css_rule_body<'a>(css: &'a str, selector: &str) -> &'a str {
        let needle = format!("\n{selector} {{");
        let start = css
            .find(&needle)
            .unwrap_or_else(|| panic!("tailwind.css should contain a `{selector}` rule"))
            + needle.len();
        let end = start
            + css[start..]
                .find('}')
                .unwrap_or_else(|| panic!("`{selector}` rule should be closed"));
        &css[start..end]
    }

    /// A quote inside the local user's own bubble must take the bubble's own
    /// text colour, not a hardcoded one.
    ///
    /// `.bg-accent` is the solid brand-blue sent-message bubble, whose text is
    /// `text-white`, and it is the SAME blue in light and dark mode. The rule
    /// used to paint the quote `rgba(0, 0, 0, 0.8)`, so quoted text rendered
    /// near-black inside a bubble of white text — 3.52:1 against the bubble,
    /// under the 4.5:1 WCAG AA floor, in BOTH colour schemes. Pinning
    /// `color: inherit` is what keeps the quote tied to the bubble's palette
    /// instead of drifting from it again.
    ///
    /// The browser-side counterpart, which measures the real composited
    /// contrast in both colour schemes rather than the declaration text, is
    /// `ui/tests/blockquote-contrast.spec.ts`.
    #[test]
    fn sent_bubble_blockquote_inherits_bubble_text_colour() {
        const TAILWIND_CSS: &str = include_str!("../../../assets/tailwind.css");
        let body = css_rule_body(TAILWIND_CSS, ".bg-accent .prose blockquote");

        // Check the LAST `color:` declaration, not merely that `inherit`
        // appears somewhere in the body. CSS is last-one-wins within a rule,
        // so `color: inherit; color: #111;` reintroduces the bug in full while
        // a `contains("color: inherit")` check happily passes — a substring
        // search cannot model the cascade. Take the final declaration, which
        // is the one that actually paints.
        //
        // `border-left-color:` also ends in `color:`, hence the `-` guard.
        let last_color = body
            .split(';')
            .map(str::trim)
            .filter(|d| d.starts_with("color:"))
            .next_back()
            .unwrap_or_else(|| {
                panic!("`.bg-accent .prose blockquote` should declare a colour. Found: {body}")
            });

        assert_eq!(
            last_color, "color: inherit",
            "the winning `color` declaration in `.bg-accent .prose blockquote` \
             must be `inherit` so the quote takes the sent bubble's own text \
             colour; a hardcoded colour is what made quoted text render \
             black-on-blue beside white text. Full rule: {body}"
        );
    }

    /// Quotes outside the sent bubble use a dedicated `--color-text-quote`
    /// token rather than the app-wide `--color-text-muted`.
    ///
    /// The muted token is tuned for glanceable metadata (timestamps, field
    /// labels) and falls under 4.5:1 on two of the backgrounds a quote
    /// actually appears over, both in LIGHT mode: `--color-surface` (4.47:1)
    /// and the `bg-accent/20` outgoing DM bubble (3.96:1, the worst quote
    /// surface in the app). The dark-mode equivalents were already fine.
    ///
    /// The token must be declared in BOTH colour schemes, and the two ways to
    /// break that fail DIFFERENTLY — which is why this counts declarations
    /// instead of asserting either symptom:
    ///
    /// - Drop the DARK value and `:root`'s light value still resolves, so
    ///   every dark-mode quote silently paints the light colour (2.49:1 on a
    ///   dark received bubble — worse than the bug this PR fixes).
    /// - Drop the LIGHT value and the token is undefined everywhere `:root`
    ///   would have supplied it, so `var()` is invalid at computed-value time,
    ///   `color` inherits the body text colour, and the muted look vanishes
    ///   entirely while contrast stays high enough that no AA floor notices.
    #[test]
    fn blockquote_uses_dedicated_quote_colour_token_in_both_schemes() {
        const TAILWIND_CSS: &str = include_str!("../../../assets/tailwind.css");

        let body = css_rule_body(TAILWIND_CSS, ".prose blockquote");
        assert!(
            body.contains("color: var(--color-text-quote)"),
            "`.prose blockquote` must use `--color-text-quote`, not the \
             app-wide muted token, so a quote stays above the WCAG AA floor \
             without repainting every timestamp and label. Found: {body}"
        );

        // `:root` carries the light-mode values; the dark-mode overrides live
        // in the `prefers-color-scheme: dark` block. Both must define the
        // token, so count declarations rather than merely finding one.
        let declarations = TAILWIND_CSS.matches("--color-text-quote:").count();
        assert_eq!(
            declarations, 2,
            "`--color-text-quote` must be declared exactly twice — once in \
             `:root` (light) and once under `prefers-color-scheme: dark` — \
             found {declarations}"
        );
    }

    const SAMPLE_ID: &str = "UDzGbcWrKN748tYbhvbPCCCQrZc9r9xkN3tUuun5Rts";
    /// Real-shape 44-char base58 ID for tests that need a second distinct ID.
    const SAMPLE_ID_2: &str = "EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr";

    // ---- Share links (freenet.org/open and `freenet:`) ----

    /// River's own contract id (a vector id in the shared share-link file).
    const RIVER_ID: &str = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
    /// Harvest's contract id, for the store-link shape Harvest shares.
    const HARVEST_ID: &str = "6FzSeAUKcqJrveKyU8RJgGKc5jRB1Z2juvxXtwTA4Em9";

    fn anchor_to(html: &str, href: &str, text: &str) -> bool {
        html.contains(&format!(
            "<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"{href}\">{text}</a>"
        ))
    }

    #[test]
    fn freenet_org_open_link_opens_app_on_own_node() {
        let link = format!("https://freenet.org/open#{HARVEST_ID}/#store=ABCDEFGHJKLMNPQR");
        let html = message_to_html(&format!("my store: {link}"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{HARVEST_ID}/#store=ABCDEFGHJKLMNPQR"),
                &link
            ),
            "a freenet.org/open link must point at the app on the reader's node, \
             keeping the original text: {html}"
        );
        assert!(!html.contains("href=\"https://freenet.org"), "{html}");
    }

    #[test]
    fn freenet_org_open_slash_form_is_converted() {
        let link = format!("https://freenet.org/open/#{RIVER_ID}/?invitation=abc123&x=y");
        let html = message_to_html(&link);
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/?invitation=abc123&amp;x=y"),
                &link.replace('&', "&amp;")
            ),
            "{html}"
        );
    }

    #[test]
    fn bare_freenet_scheme_links_are_converted() {
        for link in [
            format!("freenet:{RIVER_ID}/"),
            format!("freenet://{RIVER_ID}/"),
            format!("FREENET:{HARVEST_ID}/#store=ABCDEFGHJKLMNPQR"),
        ] {
            let html = message_to_html(&format!("open {link} please"));
            let path = crate::util::share_link::parse_share_link(&link)
                .unwrap()
                .local_path();
            assert!(anchor_to(&html, &path, &link), "{link}: {html}");
        }
    }

    #[test]
    fn bare_freenet_link_trailing_punctuation_is_not_part_of_it() {
        let html = message_to_html(&format!(
            "try freenet:{RIVER_ID}. Or (freenet:{HARVEST_ID}/)!"
        ));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}"),
                &format!("freenet:{RIVER_ID}")
            ),
            "{html}"
        );
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{HARVEST_ID}/"),
                &format!("freenet:{HARVEST_ID}/")
            ),
            "{html}"
        );
    }

    #[test]
    fn angle_bracket_freenet_autolink_is_converted() {
        // The markdown crate emits `<a href="">` for a scheme it does not
        // allow; the share-link pass gives it its real destination.
        let html = message_to_html(&format!("<freenet:{RIVER_ID}/a/b#x/../y>"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/a/b#x/../y"),
                &format!("freenet:{RIVER_ID}/a/b#x/../y")
            ),
            "{html}"
        );
    }

    #[test]
    fn share_links_in_code_are_not_converted() {
        for text in [
            format!("`freenet:{RIVER_ID}/`"),
            format!("```\nfreenet:{RIVER_ID}/\n```"),
            format!("`https://freenet.org/open#{RIVER_ID}/`"),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("<a "), "{text:?}: {html}");
        }
    }

    #[test]
    fn freenet_scheme_inside_other_words_or_attributes_is_not_converted() {
        for text in [
            format!("xfreenet:{RIVER_ID}/"),
            format!("![freenet:{RIVER_ID}/](https://example.com/x.png)"),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("/v1/contract/web/"), "{text:?}: {html}");
        }
    }

    /// The destination comes only from the visible text. A share-link label
    /// over a different href goes where the label says; a share-link href
    /// behind an ordinary label is left pointing at the freenet.org page
    /// (which shows the id), never turned into a one-click open.
    #[test]
    fn share_link_destination_always_matches_visible_text() {
        let html = message_to_html(&format!(
            "[freenet:{RIVER_ID}/](https://freenet.org/open#{HARVEST_ID}/)"
        ));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
        assert!(!html.contains(HARVEST_ID), "{html}");

        let html = message_to_html(&format!(
            "[https://freenet.org/open#{RIVER_ID}/](https://evil.example/)"
        ));
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{RIVER_ID}/\"")),
            "{html}"
        );
        assert!(!html.contains("evil.example"), "{html}");

        let html = message_to_html(&format!(
            "[click here](https://freenet.org/open#{HARVEST_ID}/)"
        ));
        assert!(
            html.contains(&format!("href=\"https://freenet.org/open#{HARVEST_ID}/\"")),
            "a labelled share link must stay a link to the freenet.org page: {html}"
        );
        assert!(!html.contains("/v1/contract/web/"), "{html}");
    }

    #[test]
    fn share_links_untouched_without_a_gateway() {
        let link = format!("https://freenet.org/open#{RIVER_ID}/");
        let html = message_to_html_inner(&link, false);
        assert!(html.contains(&format!("href=\"{link}\"")), "{html}");
        let html = message_to_html_inner(&format!("freenet:{RIVER_ID}/"), false);
        assert!(!html.contains("<a "), "{html}");
    }

    /// Markdown syntax that is valid inside a share link must not split it:
    /// `…/a*b*c` used to render as `…/a<em>b</em>c` and link only `…/a`.
    #[test]
    fn markdown_syntax_inside_a_bare_link_does_not_split_it() {
        for rest in ["/a*b*c", "/a_b_c", "/a~~b~~c", "/x*y", "/#a*b*c"] {
            let link = format!("freenet:{RIVER_ID}{rest}");
            let html = message_to_html(&format!("see {link} now"));
            let path = format!("/v1/contract/web/{RIVER_ID}{rest}");
            assert!(anchor_to(&html, &path, &link), "{link}: {html}");
        }
    }

    #[test]
    fn emphasised_bare_link_is_linked_inside_the_emphasis() {
        let html = message_to_html(&format!(
            "*freenet:{RIVER_ID}/* and **freenet:{HARVEST_ID}/**"
        ));
        assert!(
            html.contains(&format!(
                "<em><a target=\"_blank\" rel=\"noopener noreferrer\" \
                 href=\"/v1/contract/web/{RIVER_ID}/\">freenet:{RIVER_ID}/</a></em>"
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<strong><a target=\"_blank\" rel=\"noopener noreferrer\" \
                 href=\"/v1/contract/web/{HARVEST_ID}/\">freenet:{HARVEST_ID}/</a></strong>"
            )),
            "{html}"
        );
    }

    #[test]
    fn bare_link_at_the_very_start_of_a_message() {
        let link = format!("freenet:{RIVER_ID}/");
        let html = message_to_html(&link);
        assert!(
            anchor_to(&html, &format!("/v1/contract/web/{RIVER_ID}/"), &link),
            "{html}"
        );
    }

    /// Byte offsets from the markdown parser must line up with the source even
    /// after multi-byte text: the code-span link stays code, the other links.
    #[test]
    fn non_ascii_before_links_keeps_code_exclusion_aligned() {
        let text = format!("é ü 🦀 `freenet:{RIVER_ID}/` then freenet:{HARVEST_ID}/");
        let html = message_to_html(&text);
        assert!(
            html.contains(&format!("<code>freenet:{RIVER_ID}/</code>")),
            "{html}"
        );
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{HARVEST_ID}/"),
                &format!("freenet:{HARVEST_ID}/")
            ),
            "{html}"
        );
        assert!(
            !html.contains(&format!("/v1/contract/web/{RIVER_ID}")),
            "{html}"
        );
    }

    /// A bare link inside a construct that is not prose must never nest an
    /// anchor or land in an attribute.
    #[test]
    fn bare_links_inside_links_titles_and_footnotes_stay_plain() {
        for text in [
            format!("[see freenet:{RIVER_ID}/](https://example.com/)"),
            format!("[x](https://example.com/ \"freenet:{RIVER_ID}/\")"),
            format!("text[^freenet:{RIVER_ID}/]\n\n[^freenet:{RIVER_ID}/]: note"),
            format!("[r]: https://example.com/ \"freenet:{RIVER_ID}/\"\n\n[x][r]"),
            format!("<span title=\"freenet:{RIVER_ID}/\">x</span>"),
            format!("[x](freenet:{RIVER_ID}/)"),
            format!("![x](freenet:{RIVER_ID}/)"),
        ] {
            let html = message_to_html(&text);
            assert!(
                !html.contains(&format!("href=\"/v1/contract/web/{RIVER_ID}")),
                "{text:?}: {html}"
            );
            assert!(
                !html.contains('\u{E002}') && !html.contains("%EE%80%82"),
                "{text:?}: sentinel leaked: {html}"
            );
            // No anchor opens inside another anchor.
            for piece in html.split("<a ").skip(1) {
                let close = piece.find("</a>").unwrap_or(piece.len());
                assert!(!piece[..close].contains("<a "), "{text:?}: nested: {html}");
            }
        }
    }

    /// A message cannot forge the private-use sentinels the bare-link pass
    /// uses: they are stripped before any are inserted.
    #[test]
    fn forged_bare_link_sentinels_are_inert() {
        let text = format!("\u{E002}0\u{E003} \u{E002}1\u{E003} freenet:{RIVER_ID}/");
        let html = message_to_html(&text);
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert!(
            !html.contains('\u{E002}') && !html.contains('\u{E003}'),
            "{html}"
        );
    }

    #[test]
    fn share_links_render_alongside_mentions() {
        let member = MemberId(freenet_scaffold::util::FastHash(42));
        let names: HashMap<MemberId, String> = [(member, "Bob".to_string())].into();
        let token = river_core::mention::encode_mention(member, "Bob");
        let html = message_to_html_with_mentions(
            &format!("{token} try freenet:{RIVER_ID}/ and https://freenet.org/open#{HARVEST_ID}/"),
            &names,
            None,
        );
        assert!(html.contains("river-mention"), "{html}");
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{HARVEST_ID}/"),
                &format!("https://freenet.org/open#{HARVEST_ID}/")
            ),
            "{html}"
        );
    }

    /// A converted anchor carries nothing else from the message, so a `title`
    /// cannot name a different link in the tooltip.
    #[test]
    fn converted_share_link_drops_title() {
        let html = message_to_html(&format!(
            "[freenet:{RIVER_ID}/](https://x.example/ \"https://freenet.org/open#{HARVEST_ID}/\")"
        ));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
        assert!(!html.contains("title="), "{html}");
    }

    /// Pre-existing gateway-URL rewrite: an ENCODED dot segment is resolved by
    /// the browser just like `..`, so it must be refused too.
    #[test]
    fn gateway_url_with_encoded_dot_segments_is_not_rewritten() {
        for suffix in [
            format!("/%2e%2e/{SAMPLE_ID_2}/"),
            "/%2E%2e/%2e%2E/%2e%2e/permission/apps".to_string(),
            "/.%2e/x".to_string(),
            "/%2e./x".to_string(),
        ] {
            let url = format!("https://x.example/v1/contract/web/{SAMPLE_ID}{suffix}");
            let html = message_to_html(&format!("[River update]({url})"));
            assert!(
                !html.contains("href=\"/v1/contract/web/"),
                "{suffix}: {html}"
            );
        }
    }

    /// Message text is attacker-controlled: a crafted body must not make the
    /// bare-link scan super-linear. `freenet:freenet:…)))` made every
    /// occurrence a candidate spanning the rest of the run, trimmed one
    /// character at a time with a full parenthesis recount each step (O(n^3):
    /// 186 s for 16 KB natively). The size is small enough that a cubic
    /// regression still FINISHES (2.4 s release, far longer in a debug test
    /// build) and fails the bound, rather than hanging the test run; the fixed
    /// scan renders all of these in milliseconds.
    #[test]
    fn adversarial_bare_link_text_renders_in_bounded_time() {
        let n = 4 * 1024;
        let payloads = [
            format!("{}{}", "freenet:".repeat(n / 16), ")".repeat(n / 2)),
            format!("freenet:{}", ")".repeat(n)),
            "freenet:".repeat(n / 8),
            format!("freenet:{RIVER_ID}/ ").repeat(n / 55),
            "(freenet:x) ".repeat(n / 12),
        ];
        for payload in payloads {
            let started = std::time::Instant::now();
            let _ = message_to_html(&payload);
            let elapsed = started.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(10),
                "rendering {} bytes took {elapsed:?}",
                payload.len()
            );
        }
    }

    /// Every path that renders user-written text as markdown: message bodies
    /// (with and without the gateway-only bare link pass, which also parses
    /// to an AST), room descriptions, bodies with mentions, and reply
    /// previews.
    fn render_every_markdown_path(text: &str) {
        let member = MemberId(freenet_scaffold::util::FastHash(7));
        let names: HashMap<MemberId, String> = [(member, "Bob".to_string())].into();
        for behind_gateway in [true, false] {
            let _ = message_to_html_inner(text, behind_gateway);
            let _ = description_to_html(text, behind_gateway);
        }
        let with_mention = format!(
            "{} {text}",
            river_core::mention::encode_mention(member, "Bob")
        );
        let _ = message_to_html_with_mentions(&with_mention, &names, Some(member));
        let _ = clean_reply_preview(text, &names);
        let _ = strip_markdown(text);
    }

    /// Inputs the unpatched `markdown` crate panics on (see
    /// `[patch.crates-io]` in the root Cargo.toml).
    #[test]
    fn malformed_markdown_renders_without_panicking() {
        let inputs = [
            // A line ending inside a link title or reference label. The
            // renderer turns every `\n` into a hard break (`"  \n"`), so a
            // plain line break inside the title is enough.
            "[a](b \"x\ny\")",
            "[a](b 'x\ny')",
            "[a](b (x\ny))",
            "![a](b \"x\ny\")",
            "[a](b \"x \ny\")",
            "[a](b \"x \r\ny\")",
            "[x](/x \"> \n\")",
            "[a][b\nc]\n\n[b c]: d",
            "[][a \n]\n\n[a ]:\0",
            // An email address in an image title that spans lines.
            "![a](b \"c@d.com\ne\")",
            // Setext underlines next to each other.
            "=\n=\n=\na\n=",
            "}\n-\n--\n]\n=",
            // A list item ending in unclosed code or HTML, then another marker.
            "1. <!--\n-",
            "*\t~~~\n1.",
            "- ```\n1)",
            // An unfinished CDATA opener, then an empty numeric reference.
            "<![C&#;",
            // A table head, then a new container on the last line.
            "a\n|-\n- <",
            "a\n|-\n> <",
        ];
        for input in inputs {
            for text in [input.to_string(), format!("freenet:{RIVER_ID}/ {input}")] {
                let rendered = std::panic::catch_unwind(|| render_every_markdown_path(&text));
                assert!(rendered.is_ok(), "rendering {text:?} panicked");
            }
        }
    }

    /// The AST helpers walk and drop a tree without recursion, so a deep tree
    /// cannot overflow the stack even if one gets past
    /// `markdown_cost_is_bounded`. Runs on a small stack to leave a margin
    /// below wasm's 1 MiB.
    #[test]
    fn deep_markdown_trees_are_walked_without_recursion() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let deep = format!("{}`a` b", ">".repeat(5_000));
                assert!(!non_prose_ranges(&deep).is_empty());
                let tree = markdown::to_mdast(&deep, &markdown::ParseOptions::gfm()).unwrap();
                let mut text = String::new();
                collect_mdast_text(&tree, &mut text);
                assert!(text.starts_with('a'), "{text}");
                drop_mdast(tree);
            })
            .expect("spawn")
            .join()
            .expect("walking a deep markdown tree panicked");
    }

    /// Deep block nesting on one line, a wide table, or too much text is
    /// shown as plain text, however the line breaks are written.
    #[test]
    fn costly_markdown_is_shown_as_plain_text() {
        let deep_list = format!("{}x", "- ".repeat(MARKDOWN_MAX_LINE_NESTING + 1));
        let deep_quote = format!("{}x", "> 1. ".repeat(MARKDOWN_MAX_LINE_NESTING));
        let wide_table = format!("{}\n{}\n|", "|a".repeat(40), "|-".repeat(40));
        for costly in [
            deep_list.clone(),
            format!("a\r{deep_list}"),
            format!("a\r\n{deep_quote}"),
            format!("[^a]: {deep_list}"),
            format!("\u{feff}{deep_list}"),
            format!("{BARE_LINK_OPEN}{deep_list}"),
            wide_table,
            "a\n".repeat(MARKDOWN_MAX_SOURCE_BYTES),
        ] {
            assert!(!markdown_cost_is_bounded(&costly), "{costly:?}");
            assert!(message_to_html(&costly).starts_with("<p>"), "{costly:?}");
        }
        for fine in [
            format!("{}x", "- ".repeat(MARKDOWN_MAX_LINE_NESTING)),
            "> quote\n- item\n  1. nested".to_string(),
            "| a | b |\n| - | - |\n| 1 | 2 |".to_string(),
            "-1 and 2.5 and -x".to_string(),
        ] {
            assert!(markdown_cost_is_bounded(&fine), "{fine:?}");
        }
        assert_eq!(line_container_depth("  > - 1) * x"), 4);
        assert_eq!(line_container_depth(">>> x"), 3);
        assert_eq!(line_container_depth("-x"), 0);
        assert_eq!(line_container_depth("2024. was"), 1);
        assert_eq!(line_container_depth("[^a]: > - x"), 3);
        assert_eq!(line_container_depth("[^a] x"), 0);
    }

    /// Markdown that renders to far more HTML than its size is shown as plain
    /// text instead.
    #[test]
    fn markdown_that_expands_too_far_is_shown_as_plain_text() {
        let text = format!("[a]: b '{}'\n\n{}", "\"".repeat(100), "[a]".repeat(300));
        assert!(markdown_cost_is_bounded(&text));
        assert!(render_gfm(&text).len() > MARKDOWN_MAX_HTML_BYTES);
        let html = message_to_html(&text);
        assert!(html.starts_with("<p>[a]: b"), "{}", &html[..200]);
        assert!(html.len() < 2 * text.len(), "{}", html.len());
        assert!(description_to_html(&text, true).starts_with("<p>[a]: b"));
        // The plain text is the message as written, without the hard-break
        // rewrite, so CRLF line breaks are not doubled.
        let crlf = text.replace('\n', "\r\n");
        let html = message_to_html(&crlf);
        assert!(html.starts_with("<p>[a]: b"), "{}", &html[..200]);
        assert!(!html.contains("  <br />"), "{}", &html[..400]);
    }

    /// A mention that markdown moves into an attribute or copies (through a
    /// reference definition's title) is not substituted there: the message
    /// is shown as plain text with one chip per mention.
    #[test]
    fn mentions_markdown_moves_or_copies_fall_back_to_plain_text() {
        let member = MemberId(freenet_scaffold::util::FastHash(7));
        let names: HashMap<MemberId, String> = [(member, "Bob".to_string())].into();
        let token = river_core::mention::encode_mention(member, "Bob");
        let copied = format!("[a]: b '{}'\n\n{}", token.repeat(20), "[a] ".repeat(150));
        let in_title = format!("[x](https://x.example \"{token}\")");
        let in_destination = format!("[x](https://x.example/{token})");
        let in_alt = format!("![{token}](https://x.example/i.png)");
        for text in [copied, in_title, in_destination, in_alt] {
            let html = message_to_html_with_mentions(&text, &names, None);
            assert!(html.starts_with("<p>"), "{html}");
            assert!(!html.contains("<a "), "{html}");
            assert!(html.len() < 20 * text.len(), "{}", html.len());
            assert!(html.contains("river-mention"), "{html}");
        }
        // A mention in link text still renders as a chip inside the link.
        let html = message_to_html_with_mentions(
            &format!("[hi {token}](https://x.example)"),
            &names,
            None,
        );
        assert!(
            html.contains("<a ") && html.contains("river-mention"),
            "{html}"
        );
    }

    /// Past the size limit, text is shown as escaped plain text with its line
    /// breaks, and mentions still become chips.
    #[test]
    fn text_past_the_markdown_limit_renders_as_plain_text() {
        let member = MemberId(freenet_scaffold::util::FastHash(7));
        let names: HashMap<MemberId, String> = [(member, "Bob".to_string())].into();
        let long = "**<b>x</b>**\n".repeat(MARKDOWN_MAX_SOURCE_BYTES / 10);
        assert!(long.len() > MARKDOWN_MAX_SOURCE_BYTES);
        let html = message_to_html(&long);
        assert!(
            html.starts_with("<p>**&lt;b&gt;x&lt;/b&gt;**<br />\n"),
            "{html}"
        );
        assert!(
            !html.contains("<strong>") && !html.contains("<b>"),
            "{html}"
        );
        let with_mention = format!(
            "{} {long}",
            river_core::mention::encode_mention(member, "Bob")
        );
        let html = message_to_html_with_mentions(&with_mention, &names, None);
        assert!(html.contains("river-mention"), "{html}");
        assert!(description_to_html(&long, true).starts_with("<p>**&lt;b&gt;"));
        // The reply preview parses only a prefix, as markdown.
        assert!(
            strip_markdown(&long).starts_with('x'),
            "{}",
            strip_markdown(&long)
        );
    }

    /// A seeded sweep over short strings of markdown syntax, the shape that
    /// found every input above. It is deterministic (fixed seed), so a failure
    /// reproduces exactly; the panic message names the input.
    #[test]
    fn generated_markdown_renders_without_panicking() {
        #[rustfmt::skip]
        const PIECES: &[&str] = &[
            "[", "]", "(", ")", "\"", "'", " ", "  ", "\t", "\n", "\n", "\r\n", "\r", "a",
            "x y", "!", ":", "<", ">", "*", "_", "~", "`", "```", "~~~", "\\", "-", "#", "|",
            "^", "=", "&", "&amp;", "&#;", "&#65;", "https://x.example", "www.a.example",
            "c@d.example", "1.", "1)", "é", "\u{a0}", "😀", "[a](b \"", "[a](b '", "[a](b (",
            "[a][", "[^", "]: ", "![", "](", "<a ", "<!--", "-->", "<![C", "    ", "> ",
            "- ", "---", "| - |", "\\\n", "[ ]", "[x]", "\0",
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3_000 {
            let len = 1 + (next() % 24) as usize;
            let text: String = (0..len)
                .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
                .collect();
            let rendered = std::panic::catch_unwind(|| render_every_markdown_path(&text));
            assert!(rendered.is_ok(), "rendering {text:?} panicked");
        }
    }

    #[test]
    fn longest_valid_bare_link_still_converts() {
        let rest = format!("/{}", "a".repeat(1999));
        let link = format!("freenet://{RIVER_ID}{rest}");
        let html = message_to_html(&format!("{link}."));
        assert!(
            anchor_to(&html, &format!("/v1/contract/web/{RIVER_ID}{rest}"), &link),
            "a 2000-byte rest is valid and must still link"
        );
    }

    /// `[<freenet:A/>](https://x)` nests an anchor inside a link label; the
    /// inner one used to escape `finalize_anchors` with an empty href.
    #[test]
    fn nested_autolink_in_a_label_is_flattened() {
        let html = message_to_html(&format!("[<freenet:{RIVER_ID}/>](https://evil.example/)"));
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
        assert!(!html.contains("evil.example"), "{html}");

        let html = message_to_html("[see <mailto:a@b.example> here](https://x.example/)");
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert_eq!(html.matches("</a>").count(), 1, "{html}");
        assert!(html.contains("href=\"https://x.example/\""), "{html}");
    }

    /// A link that opens a contract on the reader's node must not carry a
    /// label naming a different contract; such a label is shown as text.
    #[test]
    fn label_naming_another_contract_is_not_a_link() {
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
                let html = message_to_html(&format!("[{label}]({hidden})"));
                assert!(!html.contains("<a "), "{label:?} -> {hidden}: {html}");
            }
        }
    }

    /// Labels that name the destination's own contract, or none at all, keep
    /// their link (and the gateway rewrite).
    #[test]
    fn honest_labels_keep_their_link_and_rewrite() {
        let href = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let local = format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"");
        for label in [
            "River update".to_string(),
            "my freenet: node".to_string(),
            format!("freenet:{SAMPLE_ID} (mirror)"),
            format!("freenet:{}", &SAMPLE_ID[..8]),
            format!("freenet:{}\u{22EF}{}", &SAMPLE_ID[..8], &SAMPLE_ID[36..]),
        ] {
            let html = message_to_html(&format!("[{label}]({href})"));
            assert!(html.contains(&local), "{label:?}: {html}");
        }
        let html = message_to_html(&format!(
            "[/v1/contract/web/{SAMPLE_ID}/](/v1/contract/web/{SAMPLE_ID}/)"
        ));
        assert!(html.contains("<a "), "{html}");
        let html = message_to_html("[see /contract/web/ docs](/docs)");
        assert!(html.contains("<a "), "{html}");
        // A bare link whose href is the markdown crate's normalisation of its
        // text is still rewritten.
        let bare = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/{{x}}");
        let html = message_to_html(&bare);
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/%7Bx%7D\"")),
            "{html}"
        );
        // So is a v2 gateway URL.
        let html = message_to_html(&format!(
            "http://127.0.0.1:7509/v2/contract/web/{SAMPLE_ID}/"
        ));
        assert!(
            html.contains(&format!("href=\"/v2/contract/web/{SAMPLE_ID}/\"")),
            "{html}"
        );
    }

    /// The label check sees what the reader sees: invisible characters,
    /// character references, fancy text, homoglyphs and image alt text do not
    /// hide an id.
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
            let html = message_to_html(&format!("[{label}]({hidden})"));
            assert!(
                !html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}"))
                    && !html.contains(&format!("href=\"{hidden}\"")),
                "{label:?}: {html}"
            );
        }
    }

    /// A dot segment moves where the browser lands, so a link whose path
    /// has one (or that is a contract path the parser refuses) has an unknown
    /// destination, and any contract its label names blocks it.
    #[test]
    fn dot_segments_make_the_destination_unknown() {
        let a = RIVER_ID;
        let b = SAMPLE_ID;
        for href in [
            format!("/v1/contract/web/{a}/../{b}/"),
            format!("/v1/contract/web/{a}/%2e%2e/{b}/"),
            format!("/v1/contract/web/{a}/.%2E/{b}/"),
            format!("http:/v1/contract/web/{a}/../{b}/"),
            format!("http://127.0.0.1:7509/v1/contract/web/{a}/../{b}/"),
            format!("//127.0.0.1:7509/v1/contract/web/{a}/%2e%2e/{b}/"),
            format!("http://127.0.0.1:7509/v1/./contract/web/{b}/"),
            format!("http://127.0.0.1:7509/v1/contract/web/%36%46{}/", &b[2..]),
        ] {
            for label in [format!("River: {a}"), "open freenet:raAqMhMG".to_string()] {
                let html = message_to_html(&format!("[{label}]({href})"));
                assert!(!html.contains("<a "), "{label:?} -> {href}: {html}");
            }
        }
        // A relative "bare" link hides nothing only if it parses; this one
        // shows A while resolving elsewhere.
        let rel = format!("/v1/contract/web/{a}/%2e%2e/{b}/");
        let html = message_to_html(&format!("[{rel}]({rel})"));
        assert!(!html.contains("<a "), "{html}");
    }

    /// Breaks the reader does not see (or reads as an id wrapping) do not
    /// split an id into runs too short to count.
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
            format!("{head}\u{000C}{tail}"),
            format!("{head}\u{0085}{tail}"),
            format!("{head}\u{2028}{tail}"),
            format!("{head}\u{2029}{tail}"),
            format!("{head}\u{A7B3}{tail}"),
            format!("{head}\u{10317}{tail}"),
            format!("{head}\u{05BC}\u{05BC}\u{05BC}\u{05BC}{tail}"),
            format!("{head}\u{A7AB}{tail}"),
            format!("{head}\u{2D5D}{tail}"),
            format!("{head}\u{302A}{tail}"),
            format!("{head}\u{1D167}{tail}"),
            "freenet:\u{05BC}raAqMhMG".to_string(),
            "freenet:/\u{0E31}raAqMhMG".to_string(),
            "freenet\u{A4FD}raAqMhMG".to_string(),
            format!("{head}\u{222A}{tail}"),
            format!("{head}\u{2A2F}{tail}"),
            format!("{head}\u{311A}{tail}"),
            format!("{head}{}{tail}", "\u{05BC}".repeat(12)),
            format!("{head}\u{27D9}{tail}"),
            format!("freenet:{}\u{2178}GbcW", &SAMPLE_ID_2[..3]),
            format!("{head}{}{tail}", "\u{0EC8}".repeat(4)),
            format!("{head}{}{tail}", "\u{0730}".repeat(9)),
            format!("{head}\u{0E31}{}{tail}", "\u{0730}".repeat(4)),
            format!("{head}\u{222A}\u{05C4}{tail}"),
            format!("{head}\u{2A2F}\u{15F7}{tail}"),
            format!("{head}\u{2282}\u{10B3}{tail}"),
            format!("{head}\u{2E26}{tail}"),
            format!("{head}\u{3147}{tail}"),
            "freenet\u{2236}raAqMhMG".to_string(),
        ] {
            let html = message_to_html(&format!("[{label}]({hidden})"));
            assert!(
                !html.contains(&format!("href=\"{hidden}\"")),
                "{label:?}: {html}"
            );
        }
    }

    /// A tooltip is label text too: one naming another contract unlinks a
    /// link that opens on the reader's node; an honest one is kept.
    #[test]
    fn node_link_title_is_checked_like_the_label() {
        let href = format!("/v1/contract/web/{SAMPLE_ID}/");
        for text in [
            format!("[River]({href} \"freenet:{RIVER_ID}\")"),
            format!("[![River](x.png \"freenet:{RIVER_ID}\")]({href})"),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("<a "), "{text:?}: {html}");
        }
        let html = message_to_html(&format!(
            "[Explore with Atlas](http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/ \"Atlas search engine\")"
        ));
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\""))
                && html.contains("title=\"Atlas search engine\""),
            "{html}"
        );
    }

    #[test]
    fn round5_label_bypasses_are_closed() {
        let a = RIVER_ID;
        let b = SAMPLE_ID;
        let (head, tail) = a.split_at(20);
        for text in [
            // `://` later in a relative href does not make it absolute.
            format!("[River {a}](/v1/contract/./web/{b}/?x=://)"),
            format!("[River {a}](../{b}/#://)"),
            format!("[freenet:raAqMhMG](../{b}/#://)"),
            // Unfolded homoglyphs and zero-width marks inside an id.
            format!("[{head}\u{0261}{tail}](/v1/contract/web/{b}/)"),
            format!("[{head}\u{051A}{tail}](/v1/contract/web/{b}/)"),
            format!("[{head}\u{0417}{tail}](/v1/contract/web/{b}/)"),
            format!("[River: {head}\u{05BC}{tail}](/v1/contract/web/{b}/)"),
            format!("[open freenet:raAq\u{05BC}MhMG](/v1/contract/web/{b}/)"),
            // A look-alike that is not base58.
            format!("[River: {head}O{tail}](/v1/contract/web/{b}/)"),
            // Userinfo shows `freenet:<A>` first.
            format!(
                "[http://freenet:{a}@x.example/v1/contract/web/{b}/](http://127.0.0.1:7509/v1/contract/web/{b}/)"
            ),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("<a "), "{text:?}: {html}");
        }
        // A bare URL whose text hides where it lands (encoded dot segment and
        // id) is not a link.
        let html = message_to_html(&format!(
            "http://127.0.0.1:7509/v1/contract/web/{a}/%2e%2e/%36%46{}/",
            &b[2..]
        ));
        assert!(!html.contains("<a "), "{html}");
        let rel = format!("/v1/contract/web/{a}/%2e%2e/{b}/#://");
        let html = message_to_html(&format!("[{rel}]({rel})"));
        assert!(!html.contains("<a "), "{html}");
    }

    #[test]
    fn claim_rule_edges() {
        let b = SAMPLE_ID;
        let href = format!("/v1/contract/web/{b}/");
        // A claim must be a PREFIX of the destination id, not just appear in it.
        let html = message_to_html(&format!("[freenet:{}]({href})", &b[3..12]));
        assert!(!html.contains("<a "), "substring, not prefix: {html}");
        let html = message_to_html(&format!("[freenet:{}]({href})", &b[..9]));
        assert!(html.contains("<a "), "a true prefix is consistent: {html}");
        // A long lowercase run (a hex hash) is not an id claim.
        let html = message_to_html(&format!(
            "[commit 3f2a9c1be47d58a6f0c2e9b1d3a4f5e6c7b8a9d0]({href})"
        ));
        assert!(html.contains("<a "), "hex hash is not a claim: {html}");
        // A URL-shaped label for the right contract that ALSO names another.
        let html = message_to_html(&format!(
            "[http://other.example/v1/contract/web/{b}/freenet:{RIVER_ID}](http://other.example/v1/contract/web/{b}/x)"
        ));
        assert!(!html.contains("<a "), "embedded second claim: {html}");
        // A bare URL whose id is percent-encoded does not show where it goes.
        let html = message_to_html(&format!(
            "http://127.0.0.1:7509/v1/contract/web/%36%46{}/",
            &b[2..]
        ));
        assert!(!html.contains("<a "), "encoded id: {html}");
    }

    /// A pasted URL is a link to what it shows, but not when the text before
    /// its contract path reads as another destination, or its tooltip does.
    #[test]
    fn pasted_url_that_poses_before_its_path_is_not_a_link() {
        let b = SAMPLE_ID;
        let a = RIVER_ID;
        for url in [
            format!("http://freenet.org:{a}@127.0.0.1:7509/v1/contract/web/{b}/#freenet"),
            format!("http://freenet:{a}@127.0.0.1:7509/v1/contract/web/{b}/?via=freenet"),
            format!("http://{a}.example/v1/contract/web/{b}/#freenet"),
            format!("http://River@127.0.0.1:7509/v1/contract/web/{b}/"),
        ] {
            for text in [url.clone(), format!("<{url}>"), format!("[{url}]({url})")] {
                let html = message_to_html(&text);
                assert!(!html.contains("<a "), "{text:?}: {html}");
            }
        }
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{b}/");
        let html = message_to_html(&format!("[{url}]({url} \"freenet:{a}\")"));
        assert!(!html.contains("<a "), "title on a pasted link: {html}");
    }

    #[test]
    fn freenet_in_prose_labels_is_not_a_claim() {
        let href = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        for label in [
            "Try it on Freenet: Harvest market",
            "Freenet: Ghostkey",
            "Get Freenet: Windows installer",
            "freenet:Harvest",
            "Freenet: GitHub mirror",
            "freenet: iPhone app",
            "Freenet: River2026 launch",
            "Freenet: Stra\u{00DF}e",
            "Join\u{00A0}the\u{00A0}Freenet\u{00A0}Official\u{00A0}River\u{00A0}Chat\u{00A0}Room",
            "FreenetのRiverでチャット、DeltaでWebサイト、AtlasでHarvest市場",
            "ติดตั้งFreenetแล้วเปิดRiverและDeltaหรือGhostKeyและHarvestได้เลย",
        ] {
            let html = message_to_html(&format!("[{label}]({href})"));
            assert!(
                html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"")),
                "{label:?}: {html}"
            );
        }
    }

    #[test]
    fn bare_link_in_a_table_cell_stops_at_the_cell() {
        let html = message_to_html(&format!("|a|b|\n|-|-|\n|freenet:{RIVER_ID}/x|y|"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/x"),
                &format!("freenet:{RIVER_ID}/x")
            ),
            "{html}"
        );
        assert!(html.contains("<td>y</td>"), "{html}");
    }

    #[test]
    fn bare_link_inside_cjk_text() {
        let html = message_to_html(&format!("打开freenet:{RIVER_ID}/。"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
    }

    /// A pasted gateway URL is a link to what it shows (host and contract
    /// id first); when its path also names a contract it is shown in full
    /// rather than shortened to an 8-character id prefix.
    #[test]
    fn pasted_gateway_url_naming_another_contract_is_shown_in_full() {
        for url in [
            format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/freenet:{RIVER_ID}"),
            format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/x/{RIVER_ID}"),
        ] {
            let html = message_to_html(&url);
            assert!(html.contains(&format!(">{url}</a>")), "{html}");
            assert!(
                html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/")),
                "{html}"
            );
        }
    }

    /// River's own invite links carry a long base58 code; a pasted one must
    /// stay a (shortened) link, not be read as naming another contract.
    #[test]
    fn pasted_river_invite_link_stays_a_link() {
        let code =
            "2NEpo7TZRRrLZSi2U7MpNwwdMV3fjDaZaYcAhMR1FUD4xYLQ7mQAnfk9dVYNEUoGXxBxcMGRgHtqXPC";
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{RIVER_ID}/?invitation={code}");
        for text in [
            url.clone(),
            format!("Join: {url}"),
            format!("2. Open this link: {url}"),
        ] {
            let html = message_to_html(&text);
            assert!(
                html.contains(&format!(
                    "href=\"/v1/contract/web/{RIVER_ID}/?invitation={code}\""
                )),
                "{text:?}: {html}"
            );
            assert!(
                html.contains(&format!(
                    ">freenet:{}/?invitation={code}</a>",
                    &RIVER_ID[..8]
                )),
                "{text:?}: {html}"
            );
        }
    }

    /// A share link that sits right against a non-prose range (no space)
    /// still links: the overlap sweep treats ranges as half-open.
    #[test]
    fn bare_link_right_after_a_code_span_still_links() {
        let html = message_to_html(&format!("`code`freenet:{RIVER_ID}"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}"),
                &format!("freenet:{RIVER_ID}")
            ),
            "{html}"
        );
    }

    /// A relative link already resolves on the reader's node, so behind a
    /// label that reads as a different Freenet link it is shown as text.
    #[test]
    fn relative_link_behind_a_freenet_looking_label_is_unlinked() {
        for text in [
            format!("[freenet:{RIVER_ID}/ ](/v1/contract/web/{SAMPLE_ID}/)"),
            format!("[**freenet:{RIVER_ID}/**](../{SAMPLE_ID}/)"),
            format!("[https://freenet.org/open#{RIVER_ID}/ ](/v1/contract/web/{SAMPLE_ID}/)"),
            format!("[freenet:{RIVER_ID}/ ](javascript:alert(1))"),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("<a "), "{text:?}: {html}");
            assert!(html.contains("freenet"), "{text:?}: label kept: {html}");
        }
        // An ordinary relative link, and an external one, are left alone.
        let html = message_to_html(&format!("[docs](/v1/contract/web/{SAMPLE_ID}/)"));
        assert!(html.contains("<a "), "{html}");
        let html = message_to_html(&format!("[freenet:{RIVER_ID}/ ](https://example.com/)"));
        assert!(html.contains("href=\"https://example.com/\""), "{html}");
    }

    #[test]
    fn relative_href_classification() {
        for h in ["", "/v1/x", "../x", "x", "?q", "#f", "a/b:c"] {
            assert!(is_relative_href(h), "{h:?}");
        }
        for h in ["//evil.example/", "https://x", "mailto:a@b", "javascript:x"] {
            assert!(!is_relative_href(h), "{h:?}");
        }
    }

    /// The source scan and the render parse different text (a sentinel in
    /// place of the link); where they disagree, the message renders as if the
    /// bare-link pass had not run, never with a sentinel in a URL or with
    /// content lost.
    #[test]
    fn bare_link_pass_falls_back_when_the_two_parses_disagree() {
        for text in [
            format!("[x](freenet:{RIVER_ID}/(a )"),
            format!("![x](freenet:{RIVER_ID}/(a )"),
            format!("[r]: freenet:{RIVER_ID}/(a \n\n[x][r]"),
            format!("[r]: /x/freenet:{RIVER_ID}/(\n\n[r]"),
            format!("[see freenet:{RIVER_ID}/] ](https://example.com/)"),
        ] {
            let html = message_to_html(&text);
            let plain = message_to_html_inner(&text, false);
            assert!(
                !html.contains('\u{E002}') && !html.to_ascii_uppercase().contains("%EE%80%82"),
                "{text:?}: {html}"
            );
            // Same structure as a render with no bare-link pass (the gateway
            // flag only changes hrefs, and none of these has a gateway URL).
            assert_eq!(html, plain, "{text:?}");
        }
    }

    #[test]
    fn restore_accepts_only_prose_sentinels_each_exactly_once() {
        let links = vec![format!("freenet:{RIVER_ID}/")];
        let s = |i: usize| format!("{BARE_LINK_OPEN}{i}{BARE_LINK_CLOSE}");
        let ok = restore_bare_freenet_links(&format!("<p>a {} b</p>", s(0)), &links);
        assert_eq!(
            ok.as_deref(),
            Some(format!("<p>a <a href=\"\">freenet:{RIVER_ID}/</a> b</p>").as_str())
        );
        for html in [
            format!("<p><code>{}</code></p>", s(0)),
            format!("<p><a href=\"x\">{}</a></p>", s(0)),
            format!("<pre><code>{}</code></pre>", s(0)),
            format!("<img alt=\"{}\" />", s(0)),
            format!("<p>{} {}</p>", s(0), s(0)),
            "<p>missing</p>".to_string(),
            format!("<p>{}</p>", s(1)),
            format!("<a href=\"%EE%80%820%EE%80%83\">x</a><p>{}</p>", s(0)),
        ] {
            assert_eq!(restore_bare_freenet_links(&html, &links), None, "{html}");
        }
    }

    /// Candidates longer than the longest valid link plus trailing-punctuation
    /// slack are not examined at all (the scan's work bound), even if trimming
    /// would have left a valid link.
    #[test]
    fn bare_link_candidate_length_cap_is_enforced() {
        let link = format!("freenet:{RIVER_ID}");
        let short = format!("{link}{}", "!".repeat(10));
        let html = message_to_html(&short);
        assert!(html.contains("href=\"/v1/contract/web/"), "{html}");
        let over = format!("{link}{}", "!".repeat(MAX_BARE_LINK_CANDIDATE_LEN));
        let html = message_to_html(&over);
        assert!(!html.contains("<a "), "over-long candidate must be skipped");
    }

    /// Whitespace, `<` and `>` end an angle-bracket autolink, after which GFM
    /// autolinks the leading part as a plain URL. A vector containing one
    /// therefore tests markdown's tokenising, not the validator, and its
    /// prefix may legitimately be a valid link (the text shown is then that
    /// prefix, so it is still honest).
    fn ends_markdown_autolink(raw: &str) -> bool {
        raw.contains(|c: char| c.is_whitespace() || c == '<' || c == '>')
    }

    /// Through the full renderer, every shared vector must convert to exactly
    /// its `local_path` (valid) or produce no link to a node path at all
    /// (invalid). The angle-bracket autolink form keeps the text verbatim
    /// (no GFM trailing-punctuation trimming), so the check is exact.
    #[test]
    fn shared_vectors_through_the_renderer() {
        let parsed: serde_json::Value =
            serde_json::from_str(include_str!("../../util/share-link-vectors.json")).unwrap();
        for v in parsed["vectors"].as_array().unwrap() {
            let raw = v["raw"].as_str().unwrap();
            let note = v["note"].as_str().unwrap_or("");
            for prefix in ["https://freenet.org/open#", "freenet:"] {
                let html = message_to_html(&format!("<{prefix}{raw}>"));
                if v["valid"].as_bool().unwrap() {
                    let path = v["local_path"].as_str().unwrap();
                    let href = format!("href=\"{}\"", escape_html_attr(path));
                    assert!(
                        html.contains(&href),
                        "valid vector ({note}) via {prefix:?}: expected {href} in {html}"
                    );
                } else if !ends_markdown_autolink(raw) {
                    assert!(
                        !html.contains("href=\"/v1/"),
                        "invalid vector ({note}) via {prefix:?} must not link \
                         to the node: {html}"
                    );
                }
            }
            // Bare text: an invalid vector must never become a node link.
            // Whitespace ends a bare candidate (as it ends any autolink), so a
            // vector containing it tests tokenising, not validation: skip it.
            if !v["valid"].as_bool().unwrap() && !raw.contains(char::is_whitespace) {
                let html = message_to_html(&format!("see freenet:{raw} now"));
                assert!(
                    !html.contains("href=\"/v1/"),
                    "invalid vector ({note}) as bare text must not link: {html}"
                );
            }
        }
    }

    #[test]
    fn freenet_web_url_label_shortened() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"")),
            "href should be rewritten to absolute path: {html}"
        );
        assert!(
            !html.contains("href=\"http://127.0.0.1:7509/"),
            "host/port must be stripped from href: {html}"
        );
        assert!(
            html.contains(">freenet:UDzGbcWr</a>"),
            "label should be shortened to 8-char prefix: {html}"
        );
        assert!(
            !html.contains(&format!(">{url}</a>")),
            "raw URL should not appear as link text: {html}"
        );
    }

    #[test]
    fn freenet_web_url_with_path_keeps_path() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/index.html");
        let html = message_to_html(&url);
        assert!(
            html.contains(">freenet:UDzGbcWr/index.html</a>"),
            "label should include path: {html}"
        );
    }

    #[test]
    fn freenet_web_url_https_handled() {
        let url = format!("https://nova.locut.us/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            html.contains(">freenet:UDzGbcWr</a>"),
            "https URL should also be beautified: {html}"
        );
    }

    #[test]
    fn freenet_web_url_with_query_kept() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/?invite=abc");
        let html = message_to_html(&url);
        assert!(
            html.contains(">freenet:UDzGbcWr/?invite=abc</a>"),
            "query string should be kept: {html}"
        );
    }

    #[test]
    fn custom_markdown_link_text_preserved() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&format!("see [my room]({url}) here"));
        assert!(
            html.contains(">my room</a>"),
            "user-supplied link text should be preserved: {html}"
        );
        assert!(
            !html.contains("freenet:UDzGbcWr"),
            "should not rewrite label when link text is custom: {html}"
        );
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"")),
            "href should still be rewritten so the link works for any reader: {html}"
        );
    }

    #[test]
    fn non_freenet_url_unchanged() {
        let html = message_to_html("https://example.com/foo/bar");
        assert!(
            html.contains(">https://example.com/foo/bar</a>"),
            "unrelated URLs should keep their full text: {html}"
        );
    }

    #[test]
    fn freenet_url_in_code_span_unchanged() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&format!("run `curl {url}`"));
        assert!(
            !html.contains("<a "),
            "URL inside backticks should not be linkified: {html}"
        );
        assert!(
            !html.contains("freenet:UDzGbcWr"),
            "URL inside backticks should not be beautified: {html}"
        );
    }

    #[test]
    fn non_web_contract_path_left_alone() {
        // /v1/contract/<id>/ (no /web/) is not a real browsable route; leave the
        // link text as-is rather than pretend we beautified it.
        let url = format!("http://127.0.0.1:7509/v1/contract/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!(">{url}</a>")),
            "non-web contract URL should keep its full text: {html}"
        );
    }

    #[test]
    fn marker_in_query_string_not_beautified() {
        // External redirect URLs that happen to embed the marker in a query
        // parameter must NOT be presented as Freenet links — that would be a
        // phishing vector (#223).
        let url = format!("https://evil.example/redirect?next=/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "marker buried in query must not produce a freenet: label: {html}"
        );
    }

    #[test]
    fn marker_after_userinfo_or_path_segment_not_beautified() {
        // Marker buried deeper in the path must not match either.
        let url = format!("https://evil.example/foo/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "marker as a deeper path segment must not match: {html}"
        );
    }

    #[test]
    fn multiple_freenet_links_in_one_message() {
        let url_a = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let url_b = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID_2}/foo.html");
        let html = message_to_html(&format!("see {url_a} and also {url_b} thanks"));
        assert!(
            html.contains(">freenet:UDzGbcWr</a>"),
            "first link should be shortened: {html}"
        );
        assert!(
            html.contains(">freenet:EqJ5YpEE/foo.html</a>"),
            "second link should be shortened: {html}"
        );
        assert!(
            html.matches("<a ").count() == 2,
            "both anchors should be present: {html}"
        );
    }

    #[test]
    fn empty_contract_id_not_beautified() {
        // /v1/contract/web// has no id — bail out and leave the original.
        let url = "http://127.0.0.1:7509/v1/contract/web//".to_string();
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "empty contract id must not produce a label: {html}"
        );
    }

    #[test]
    fn ampersand_in_query_keeps_full_url() {
        // GFM HTML-escapes `&` to `&amp;` in BOTH href and text content, so the
        // h == inner equality still holds and beautification proceeds with the
        // entity-encoded suffix.
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/?a=1&b=2");
        let html = message_to_html(&url);
        assert!(
            html.contains(">freenet:UDzGbcWr/?a=1&amp;b=2</a>"),
            "ampersand-bearing query should be kept (entity-encoded): {html}"
        );
    }

    #[test]
    fn href_rewritten_strips_host_and_port() {
        // The whole point of this fix: a link pasted with `127.0.0.1:7509`
        // by user A must still resolve for user B, who is connected to
        // their own gateway on a different host/port.
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/index.html");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/index.html\"")),
            "href should be rewritten to a same-origin absolute path: {html}"
        );
        assert!(
            !html.contains("127.0.0.1:7509"),
            "the original host:port must not survive anywhere in the output: {html}"
        );
    }

    #[test]
    fn href_rewrite_preserves_fragment() {
        // Lukas's report (matrix, 2026-04-27): pasted River room URLs include a
        // fragment like `#AWPjDQdKey/1/home`. Stripping the host but losing the
        // fragment would still break navigation, so this guards the suffix path.
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID_2}/#AWPjDQdKey/1/home");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!(
                "href=\"/v1/contract/web/{SAMPLE_ID_2}/#AWPjDQdKey/1/home\""
            )),
            "fragment should be carried through the rewrite: {html}"
        );
    }

    #[test]
    fn href_rewrite_handles_https() {
        let url = format!("https://gw.example.com/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"")),
            "https URLs should also have host stripped: {html}"
        );
    }

    #[test]
    fn invalid_base58_mid_id_not_rewritten() {
        // The id_end scan stops at the first non-base58 char. Construct a
        // string where an `O` sits 20 chars in: id_end == 20 falls outside
        // the 43|44 length window, so the URL must not be rewritten. This
        // exercises the "valid base58 chars surround a forbidden char"
        // path, not the "first char is forbidden" path that returns id_end=0.
        let mid_bogus = format!(
            "{}O{}",
            &SAMPLE_ID[..20],
            &SAMPLE_ID[21..] // total = 20 + 1 + 22 = 43 chars, but with `O` at position 20
        );
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{mid_bogus}/index.html");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "bogus ID with mid-string non-base58 char must not be beautified: {html}"
        );
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "bogus ID must leave href alone (host/port preserved): {html}"
        );
    }

    #[test]
    fn short_id_segment_not_rewritten() {
        // ID segments shorter than 43 chars cannot be BLAKE3 hashes.
        let url = "http://127.0.0.1:7509/v1/contract/web/tooshort/page.html".to_string();
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "short ID must not be beautified: {html}"
        );
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "short ID must leave href alone: {html}"
        );
    }

    #[test]
    fn overlong_id_segment_not_rewritten() {
        // ID segments longer than 44 chars also cannot be BLAKE3 hashes.
        let too_long = "a".repeat(45);
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{too_long}/");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "overlong ID must not be beautified: {html}"
        );
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "overlong ID must leave href alone: {html}"
        );
    }

    #[test]
    fn id_segment_42_chars_not_rewritten() {
        // 42-char base58 segment: one char short of the lower bound.
        // Pins the lower edge of the `matches!(id_end, 43 | 44)` predicate.
        let too_short = "a".repeat(42);
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{too_short}/");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "42-char ID is one short of lower bound, must not be beautified: {html}"
        );
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "42-char ID must leave href alone: {html}"
        );
    }

    #[test]
    fn dev_mode_does_not_rewrite_href_but_still_beautifies_label() {
        // When River is served outside a gateway (e.g. `dx serve`,
        // `cargo make dev-example`, or `python -m http.server`), there is no
        // gateway behind the dev server to redirect to. Stripping the host
        // would turn a working `https://gw.example/v1/contract/web/<id>/`
        // link into a 404 against the dev server. Leave the href intact;
        // the label can still be shortened (purely cosmetic).
        let url = format!("https://gw.example.com/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html_inner(&url, /* behind_gateway = */ false);
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "dev-mode must preserve the original gateway-qualified href: {html}"
        );
        assert!(
            !html.contains("href=\"/v1/contract/web/"),
            "dev-mode must not produce a same-origin absolute path: {html}"
        );
        assert!(
            html.contains(">freenet:UDzGbcWr</a>"),
            "label beautification still applies in dev mode (cosmetic only): {html}"
        );
    }

    #[test]
    fn path_traversal_in_suffix_not_rewritten() {
        // Defense against an attacker pasting a URL whose suffix contains
        // `..` segments. Without this guard, the same-origin rewrite would
        // hand the browser a path that normalizes onto unrelated endpoints
        // on the reader's local gateway — turning a paste into a CSRF-style
        // redirect. (#224)
        let url = format!(
            "http://attacker.example/v1/contract/web/{SAMPLE_ID}/../../v1/peer/diagnostics"
        );
        let html = message_to_html(&url);
        assert!(
            !html.contains("href=\"/v1/contract/web/"),
            "URL with `..` segments in suffix must NOT be rewritten to same-origin: {html}"
        );
        assert!(
            !html.contains("freenet:"),
            "URL with `..` segments in suffix must not be beautified either: {html}"
        );
        // Its text names a contract but the dot segments move where it lands
        // (on the reader's node too, when the host is theirs), so since #733
        // it is shown as plain text rather than as a link to either place.
        assert!(
            !html.contains("<a ") && html.contains(&url),
            "a `..` gateway URL must be shown as text, never as a link: {html}"
        );
    }

    #[test]
    fn dotdot_in_query_or_fragment_is_fine() {
        // `..` is only dangerous as a path segment — the browser normalizes
        // path segments before the `?` or `#`. A literal `..` inside a query
        // value or fragment is just data and should not block the rewrite.
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/?next=../foo");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!(
                "href=\"/v1/contract/web/{SAMPLE_ID}/?next=../foo\""
            )),
            "`..` in query string should not block the rewrite: {html}"
        );
    }

    #[test]
    fn non_http_scheme_not_rewritten() {
        // `parse_freenet_web_url` must restrict to http/https. Defense in
        // depth: if the markdown crate (or a future change) ever surfaces a
        // `[label](javascript:...)` link, we want the parser to refuse to
        // touch it rather than trust the upstream sanitizer alone.
        // Markdown autolinks won't normally produce non-http(s) URLs from
        // bare text, so we exercise this via the explicit-link form.
        let html = message_to_html(&format!(
            "[click](ftp://x.example/v1/contract/web/{SAMPLE_ID}/)"
        ));
        assert!(
            !html.contains("href=\"/v1/contract/web/"),
            "non-http(s) scheme must not be rewritten to same-origin: {html}"
        );
    }

    // --- @mention rendering ---------------------------------------------

    fn mid_from(hex: &str) -> MemberId {
        river_core::mention::member_id_from_hex(hex).unwrap()
    }

    fn msg_id(seed: &[u8]) -> MessageId {
        MessageId(freenet_scaffold::util::fast_hash(seed))
    }

    /// The cached renderer must always return exactly what the uncached
    /// renderer would for the *current* inputs — a repeat call (hit) and a
    /// content edit (same id) must each reflect current state.
    #[test]
    fn cached_message_html_matches_uncached_across_input_changes() {
        clear_message_html_cache();
        let id = msg_id(b"m-cache-1");
        let alice = mid_from("00000000000000aa");
        let me = mid_from("00000000000000bb");
        let mut names = HashMap::new();
        names.insert(alice, "Alice".to_string());
        let fp = member_names_fingerprint(&names);

        // Plain (mention-free) body: repeat call returns identical HTML.
        let text = "hello **world** https://freenet.org";
        let first = render_message_html_cached(&id, text, &names, fp, Some(me));
        let second = render_message_html_cached(&id, text, &names, fp, Some(me));
        assert_eq!(first, second, "cache hit is identical");
        assert_eq!(
            first,
            message_to_html_with_mentions(text, &names, Some(me)),
            "cached output equals uncached"
        );

        // Editing the body (same id) invalidates and re-renders.
        let edited = "goodbye **world**";
        let after_edit = render_message_html_cached(&id, edited, &names, fp, Some(me));
        assert_eq!(
            after_edit,
            message_to_html_with_mentions(edited, &names, Some(me)),
            "edited content re-rendered"
        );
        assert_ne!(after_edit, first, "edit changed the HTML");
    }

    /// A mention chip renders the member's *current* nickname; renaming the
    /// member must invalidate the cached body even though its text is unchanged.
    #[test]
    fn cached_message_html_reflects_member_rename_for_mentions() {
        clear_message_html_cache();
        let id = msg_id(b"m-cache-2");
        let alice = mid_from("00000000000000aa");
        let me = mid_from("00000000000000bb");
        let token = river_core::mention::encode_mention(alice, "Alice");
        let text = format!("hey {token}!");

        let mut names = HashMap::new();
        names.insert(alice, "Alice".to_string());
        let fp1 = member_names_fingerprint(&names);
        let before = render_message_html_cached(&id, &text, &names, fp1, Some(me));
        assert!(
            before.contains("Alice"),
            "chip shows current name: {before}"
        );

        // Rename Alice -> Roberta (not a substring of the old name). Same
        // message text, new member map.
        names.insert(alice, "Roberta".to_string());
        let fp2 = member_names_fingerprint(&names);
        assert_ne!(fp1, fp2, "rename changes the member fingerprint");
        let after = render_message_html_cached(&id, &text, &names, fp2, Some(me));
        assert_eq!(
            after,
            message_to_html_with_mentions(&text, &names, Some(me)),
            "rename invalidated the cached chip"
        );
        assert!(after.contains("Roberta"), "chip shows new name: {after}");
        assert!(!after.contains("Alice"), "old name gone: {after}");
    }

    /// A mention-free body must fingerprint the same regardless of the member
    /// map, so ordinary messages survive member joins/renames in the cache; a
    /// body carrying a mention token must incorporate the member fingerprint.
    #[test]
    fn message_html_fingerprint_gates_member_inputs_on_mentions() {
        let me = mid_from("00000000000000bb");
        let plain = "just a normal message, no mentions here";
        assert_eq!(
            message_html_fingerprint(plain, 111, Some(me)),
            message_html_fingerprint(plain, 222, Some(me)),
            "mention-free fingerprint ignores member fp"
        );

        let alice = mid_from("00000000000000aa");
        let token = river_core::mention::encode_mention(alice, "Alice");
        let mentioned = format!("ping {token}");
        assert_ne!(
            message_html_fingerprint(&mentioned, 111, Some(me)),
            message_html_fingerprint(&mentioned, 222, Some(me)),
            "mention fingerprint depends on member fp"
        );
    }

    /// `prune_message_html_cache` drops entries for messages no longer visible
    /// (e.g. after a room switch) while keeping the still-visible ones.
    #[test]
    fn prune_message_html_cache_bounds_to_visible_set() {
        clear_message_html_cache();
        let me = mid_from("00000000000000bb");
        let names = HashMap::new();
        let fp = member_names_fingerprint(&names);
        let keep = msg_id(b"keep");
        let drop = msg_id(b"drop");
        render_message_html_cached(&keep, "keep me", &names, fp, Some(me));
        render_message_html_cached(&drop, "drop me", &names, fp, Some(me));
        assert_eq!(MESSAGE_HTML_CACHE.with(|c| c.borrow().len()), 2);

        let mut visible = std::collections::HashSet::new();
        visible.insert(keep.clone());
        prune_message_html_cache(&visible);

        MESSAGE_HTML_CACHE.with(|c| {
            let c = c.borrow();
            assert!(c.contains_key(&keep), "visible entry kept");
            assert!(!c.contains_key(&drop), "off-screen entry pruned");
        });
    }

    #[test]
    fn strip_markdown_removes_formatting_and_keeps_link_text() {
        let s = strip_markdown("**bold** and `code` and [a link](http://x.example) end");
        assert!(!s.contains('*'), "emphasis markers removed: {s}");
        assert!(!s.contains('`'), "code fences removed: {s}");
        assert!(!s.contains("http://x.example"), "link url dropped: {s}");
        assert!(s.contains("bold") && s.contains("code") && s.contains("end"));
        assert!(s.contains("a link"), "link visible text kept: {s}");
    }

    #[test]
    fn clean_reply_preview_resolves_mention_to_current_name_and_strips_markdown() {
        let id = mid_from("00000000000000aa");
        let mut names = HashMap::new();
        names.insert(id, "Alice".to_string());
        // Token snapshot is "OldAlice"; the live map says "Alice".
        let token = river_core::mention::encode_mention(id, "OldAlice");
        let cleaned = clean_reply_preview(&format!("hey {token}, **see** this"), &names);
        assert!(
            cleaned.contains("@Alice"),
            "current nickname used: {cleaned}"
        );
        assert!(
            !cleaned.contains("OldAlice"),
            "snapshot overridden: {cleaned}"
        );
        assert!(
            !cleaned.contains("rv:"),
            "no raw mention token syntax: {cleaned}"
        );
        assert!(
            !cleaned.contains("**") && !cleaned.contains("]("),
            "markdown stripped: {cleaned}"
        );
        assert!(cleaned.contains("see"));
    }

    #[test]
    fn mention_chip_uses_current_name_not_snapshot() {
        let id = mid_from("00000000000000aa");
        let mut names = HashMap::new();
        names.insert(id, "CurrentName".to_string());
        let token = river_core::mention::encode_mention(id, "OldName");
        let html = message_to_html_with_mentions(
            &format!("hi {token}!"),
            &names,
            Some(mid_from("00000000000000ff")),
        );
        assert!(
            html.contains("data-member-id=\"00000000000000aa\""),
            "chip must carry the lossless member id: {html}"
        );
        assert!(
            html.contains(">@CurrentName</span>"),
            "chip must show the CURRENT name, following renames: {html}"
        );
        assert!(
            !html.contains("OldName"),
            "stale snapshot name must be overridden: {html}"
        );
    }

    #[test]
    fn mention_chip_falls_back_to_snapshot_for_unknown_member() {
        let id = mid_from("0000000000000abc");
        let names = HashMap::new(); // member not resolvable
        let token = river_core::mention::encode_mention(id, "Ghost");
        let html =
            message_to_html_with_mentions(&token, &names, Some(mid_from("0000000000000001")));
        assert!(
            html.contains(">@Ghost</span>"),
            "unknown member falls back to the token's snapshot name: {html}"
        );
    }

    #[test]
    fn mention_chip_escapes_attacker_controlled_nickname() {
        // Nicknames are attacker-controlled and the chip enters the DOM via
        // dangerous_inner_html (freenet/river#227) — must be escaped.
        let id = mid_from("0000000000000001");
        let mut names = HashMap::new();
        names.insert(id, "<img src=x onerror=alert(1)>".to_string());
        let token = river_core::mention::encode_mention(id, "snap");
        let html =
            message_to_html_with_mentions(&token, &names, Some(mid_from("0000000000000002")));
        assert!(
            !html.contains("<img"),
            "raw markup must not survive: {html}"
        );
        assert!(
            html.contains("&lt;img"),
            "nickname must be HTML-escaped: {html}"
        );
    }

    #[test]
    fn mention_of_self_gets_distinct_highlight_class() {
        let me = mid_from("0000000000000007");
        let mut names = HashMap::new();
        names.insert(me, "Me".to_string());
        let token = river_core::mention::encode_mention(me, "Me");
        let html = message_to_html_with_mentions(&token, &names, Some(me));
        assert!(
            html.contains("river-mention-self"),
            "a mention of the local user must get the self class: {html}"
        );
    }

    #[test]
    fn text_without_mentions_renders_identically_to_plain() {
        let names = HashMap::new();
        let text = "a normal *markdown* msg with a https://example.com link";
        let any = mid_from("0000000000000001");
        assert_eq!(
            message_to_html_with_mentions(text, &names, Some(any)),
            message_to_html(text),
            "the no-mention fast path must match the plain renderer byte-for-byte"
        );
    }

    #[test]
    fn multiple_mentions_each_resolve_independently() {
        let a = mid_from("000000000000000a");
        let b = mid_from("000000000000000b");
        let mut names = HashMap::new();
        names.insert(a, "Ann".to_string());
        names.insert(b, "Bob".to_string());
        let text = format!(
            "{} and {}",
            river_core::mention::encode_mention(a, "x"),
            river_core::mention::encode_mention(b, "y")
        );
        let html = message_to_html_with_mentions(&text, &names, Some(mid_from("00000000000000ff")));
        assert!(html.contains(">@Ann</span>"), "{html}");
        assert!(html.contains(">@Bob</span>"), "{html}");
        assert!(
            html.contains("data-member-id=\"000000000000000a\""),
            "{html}"
        );
        assert!(
            html.contains("data-member-id=\"000000000000000b\""),
            "{html}"
        );
    }

    #[test]
    fn current_token_chip_carries_full_id_resolved_from_short_ref() {
        // The wire token now carries only the 8-char short ref; the chip must
        // still recover the FULL id (for the click interceptor) by matching the
        // short ref against the known members.
        let id = mid_from("00000000000000aa");
        let mut names = HashMap::new();
        names.insert(id, "Alice".to_string());
        let token = river_core::mention::encode_mention(id, "Alice");
        assert!(
            token.contains(&format!(
                "rv:{}",
                river_core::mention::member_id_to_short(id)
            )),
            "token uses the short base32 ref: {token}"
        );
        let html =
            message_to_html_with_mentions(&token, &names, Some(mid_from("00000000000000ff")));
        assert!(
            html.contains("data-member-id=\"00000000000000aa\""),
            "chip recovers the lossless id from the short ref: {html}"
        );
    }

    #[test]
    fn legacy_hex_mention_chip_is_clickable_even_when_member_unknown() {
        // A legacy `rv:<hex>` token carries the full id, so its chip stays
        // clickable (data-member-id present) even for a member we can't name.
        let id = mid_from("0000000000000abc");
        let names = HashMap::new(); // member not resolvable by name
        let legacy = format!(
            "hi @[Bob]({}{})!",
            river_core::mention::REF_SCHEME,
            river_core::mention::member_id_to_hex(id)
        );
        let html =
            message_to_html_with_mentions(&legacy, &names, Some(mid_from("0000000000000001")));
        assert!(
            html.contains("data-member-id=\"0000000000000abc\""),
            "legacy chip keeps the full id: {html}"
        );
        assert!(html.contains(">@Bob</span>"), "snapshot name used: {html}");
    }

    #[test]
    fn unknown_short_mention_renders_inert_chip_without_member_id() {
        // A current (short) token naming a member this client doesn't know
        // cannot recover a full id, so the chip renders the snapshot name but
        // carries no data-member-id (nothing for the interceptor to open).
        let id = mid_from("0000000000000abc");
        let names = HashMap::new();
        let token = river_core::mention::encode_mention(id, "Ghost");
        let html =
            message_to_html_with_mentions(&token, &names, Some(mid_from("0000000000000001")));
        assert!(
            html.contains(">@Ghost</span>"),
            "snapshot name shown: {html}"
        );
        assert!(
            !html.contains("data-member-id"),
            "unknown short ref must not fabricate a member id: {html}"
        );
    }

    #[test]
    fn legacy_hex_self_mention_gets_highlight() {
        // A self-mention in an OLD (hex) message must still resolve to self and
        // get the self-highlight class, just like the current short form.
        let me = mid_from("0000000000000007");
        let mut names = HashMap::new();
        names.insert(me, "Me".to_string());
        let legacy = format!(
            "@[Me]({}{})",
            river_core::mention::REF_SCHEME,
            river_core::mention::member_id_to_hex(me)
        );
        let html = message_to_html_with_mentions(&legacy, &names, Some(me));
        assert!(
            html.contains("river-mention-self"),
            "legacy self-mention must get the self class: {html}"
        );
    }
}
