//! `@mention` codec — the inline wire token that references a member by their
//! stable [`MemberId`] while displaying their *current* nickname.
//!
//! ## Wire format
//!
//! A mention is stored inline in the plaintext message `text` field as:
//!
//! ```text
//! @[Display Name](rv:8f3a2b1c0000d4e5)
//! ```
//!
//! - `rv:<hex>` is the **authoritative** reference: 16 lowercase hex digits
//!   encoding the member's [`MemberId`] (a 64-bit value). Clients re-resolve
//!   the member's *current* nickname from `member_info` by this id, so the
//!   rendered chip follows renames.
//! - `Display Name` is a **fallback snapshot** captured at send time, used only
//!   when re-resolution is impossible (member not in `member_info`, an
//!   undecryptable private-room nickname, or a client too old to parse the
//!   token). Old clients render the raw token via markdown as a plain
//!   `@Display Name` link — acceptable graceful degradation.
//!
//! The token rides inside the existing `text` field, so it needs **no** change
//! to `RoomMessageBody` / the contract, works in both public and private rooms
//! (it sits inside the encrypted text), and applies equally to plain text and
//! replies (both carry a `text` field).
//!
//! ## Why a hand-rolled parser (no regex)
//!
//! This module is gated on the `mentions` feature and compiled into the client
//! crates only; a regex dependency would be heavier than the few lines below
//! and the grammar is trivial. The parser also never has to be valid markdown —
//! clients extract mentions *before* markdown runs over the text.

use crate::room_state::member::MemberId;
use freenet_scaffold::util::FastHash;

/// Scheme prefix inside the mention token's reference, e.g. `rv:8f3a…`.
pub const REF_SCHEME: &str = "rv:";

/// Maximum number of characters retained from the snapshot display name. The
/// snapshot is only a fallback, so a hard cap keeps a hostile/huge nickname
/// from bloating the token. Authoritative resolution uses the id, not this.
pub const MAX_SNAPSHOT_NAME_LEN: usize = 64;

/// A parsed mention: the authoritative member reference plus the snapshot name
/// carried in the token (which may be empty, or stale relative to the member's
/// current nickname — callers should prefer a fresh lookup by `member_id`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mention {
    pub member_id: MemberId,
    pub display_name: String,
}

/// One piece of a message body: either a run of plain text or a mention token.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MentionSegment {
    Text(String),
    Mention(Mention),
}

/// Encode a [`MemberId`] as the 16-char lowercase-hex reference body (no scheme
/// prefix). Lossless — unlike `MemberId`'s `Display`, which is *truncated*.
pub fn member_id_to_hex(id: MemberId) -> String {
    // MemberId(FastHash(i64)); encode the raw 64 bits.
    format!("{:016x}", id.0 .0 as u64)
}

/// Parse a 1..=16 digit hex reference body back into a [`MemberId`]. Returns
/// `None` for empty input, over-long input, or non-hex characters.
pub fn member_id_from_hex(s: &str) -> Option<MemberId> {
    if s.is_empty() || s.len() > 16 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let raw = u64::from_str_radix(s, 16).ok()?;
    Some(MemberId(FastHash(raw as i64)))
}

/// Strip characters that would break token parsing (the bracket/paren
/// delimiters and control chars), trim, and cap the length. Applied to the
/// snapshot name at encode time so a name can never break the wire token.
pub fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | '(' | ')') && !c.is_control())
        .collect();
    cleaned.trim().chars().take(MAX_SNAPSHOT_NAME_LEN).collect()
}

/// Build the wire token `@[name](rv:hex)` for a mention. The name is sanitized.
pub fn encode_mention(id: MemberId, display_name: &str) -> String {
    format!(
        "@[{}]({}{})",
        sanitize_name(display_name),
        REF_SCHEME,
        member_id_to_hex(id)
    )
}

/// Split message text into ordered plain-text / mention segments. This is the
/// core parser; the other helpers are thin wrappers over it.
pub fn parse_segments(text: &str) -> Vec<MentionSegment> {
    let bytes = text.as_bytes();
    let mut segments = Vec::new();
    // Byte index where the current run of plain text began.
    let mut text_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            if let Some((mention, end)) = try_parse_token_at(text, i) {
                if text_start < i {
                    segments.push(MentionSegment::Text(text[text_start..i].to_string()));
                }
                segments.push(MentionSegment::Mention(mention));
                i = end;
                text_start = end;
                continue;
            }
        }
        i += 1;
    }
    if text_start < bytes.len() {
        segments.push(MentionSegment::Text(text[text_start..].to_string()));
    }
    segments
}

/// Try to parse a complete mention token starting at byte index `at` (which
/// must point at `@`). On success returns the parsed mention and the byte index
/// just past the closing `)`. All delimiters (`[ ] ( ) :`) are ASCII, so byte
/// scanning never lands mid-codepoint.
fn try_parse_token_at(text: &str, at: usize) -> Option<(Mention, usize)> {
    let bytes = text.as_bytes();
    if bytes.get(at) != Some(&b'@') || bytes.get(at + 1) != Some(&b'[') {
        return None;
    }
    // Name runs from just after `[` to the first `]`.
    let name_start = at + 2;
    let mut j = name_start;
    while j < bytes.len() && bytes[j] != b']' {
        j += 1;
    }
    if j >= bytes.len() {
        return None; // unterminated `[`
    }
    let name = &text[name_start..j];
    // `](rv:` must immediately follow the name.
    let after = j + 1;
    let open: &[u8] = b"(";
    let scheme = REF_SCHEME.as_bytes();
    let prefix_len = open.len() + scheme.len();
    if bytes.len() < after + prefix_len
        || &bytes[after..after + open.len()] != open
        || &bytes[after + open.len()..after + prefix_len] != scheme
    {
        return None;
    }
    // Hex id runs to the closing `)`.
    let id_start = after + prefix_len;
    let mut k = id_start;
    while k < bytes.len() && bytes[k] != b')' {
        k += 1;
    }
    if k >= bytes.len() {
        return None; // unterminated `(`
    }
    let member_id = member_id_from_hex(&text[id_start..k])?;
    Some((
        Mention {
            member_id,
            display_name: name.to_string(),
        },
        k + 1,
    ))
}

/// All mentions in the text, in order of appearance.
pub fn parse_mentions(text: &str) -> Vec<Mention> {
    parse_segments(text)
        .into_iter()
        .filter_map(|s| match s {
            MentionSegment::Mention(m) => Some(m),
            MentionSegment::Text(_) => None,
        })
        .collect()
}

/// Whether `text` mentions the member with the given id.
pub fn contains_mention_of(text: &str, id: MemberId) -> bool {
    parse_mentions(text).iter().any(|m| m.member_id == id)
}

/// Render the text for a plain-text surface (e.g. the CLI), replacing each
/// mention token with `@<name>`. `resolve` supplies the member's *current*
/// display name; when it returns `None` the snapshot name in the token is used.
pub fn render_plaintext<F>(text: &str, mut resolve: F) -> String
where
    F: FnMut(MemberId) -> Option<String>,
{
    let mut out = String::with_capacity(text.len());
    for seg in parse_segments(text) {
        match seg {
            MentionSegment::Text(t) => out.push_str(&t),
            MentionSegment::Mention(m) => {
                let name = resolve(m.member_id).unwrap_or(m.display_name);
                out.push('@');
                out.push_str(&name);
            }
        }
    }
    out
}

/// Trailing characters that are punctuation rather than part of a typed name.
const TRAILING_PUNCT: &[char] = &['.', ',', '!', '?', ';', ':', ')', ']', '}', '"', '\''];

/// Convert bare `@name` mentions typed in free text into full `@[name](rv:id)`
/// wire tokens. Used by surfaces without an autocomplete picker (riverctl).
///
/// A `@name` is a `@` at a word boundary (start of text or after whitespace)
/// followed by a run of non-whitespace characters; trailing punctuation is not
/// part of the name. `resolve(name)` maps the typed name to the member to link
/// (its id and canonical display name), or returns `None` to leave the text
/// untouched — so an unmatched `@word`, an email-like `a@b`, and an
/// already-encoded `@[Name](rv:..)` token are all left exactly as-is (the
/// bracketed token never matches a plain name lookup).
pub fn resolve_typed_mentions<F>(text: &str, mut resolve: F) -> String
where
    F: FnMut(&str) -> Option<(MemberId, String)>,
{
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut prev_was_ws = true; // start-of-text counts as a boundary
    while i < bytes.len() {
        let ch = text[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();
        if ch == '@' && prev_was_ws {
            // Take the run of non-whitespace characters after '@'.
            let run_start = i + 1;
            let mut j = run_start;
            while j < bytes.len() {
                let c = text[j..].chars().next().unwrap();
                if c.is_whitespace() {
                    break;
                }
                j += c.len_utf8();
            }
            let run = &text[run_start..j];
            let candidate = run.trim_end_matches(TRAILING_PUNCT);
            if !candidate.is_empty() {
                if let Some((id, name)) = resolve(candidate) {
                    out.push_str(&encode_mention(id, &name));
                    out.push_str(&run[candidate.len()..]); // re-append stripped punctuation
                    i = j;
                    prev_was_ws = false;
                    continue;
                }
            }
            // No match: emit '@' literally and keep scanning the run as text.
            out.push('@');
            i = run_start;
            prev_was_ws = false;
            continue;
        }
        out.push(ch);
        prev_was_ws = ch.is_whitespace();
        i += ch_len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(raw: i64) -> MemberId {
        MemberId(FastHash(raw))
    }

    #[test]
    fn member_id_hex_round_trips_including_high_bit() {
        for raw in [0i64, 1, -1, i64::MAX, i64::MIN, 0x0123_4567_89ab_cdef] {
            let id = mid(raw);
            let hex = member_id_to_hex(id);
            assert_eq!(hex.len(), 16, "hex must be zero-padded to 16: {hex}");
            assert_eq!(member_id_from_hex(&hex), Some(id), "round trip raw={raw}");
        }
    }

    #[test]
    fn member_id_from_hex_rejects_garbage() {
        assert_eq!(member_id_from_hex(""), None);
        assert_eq!(member_id_from_hex("zz"), None);
        assert_eq!(member_id_from_hex("00000000000000000"), None); // 17 digits
                                                                   // Uppercase is accepted on parse even though we always emit lowercase.
        assert_eq!(member_id_from_hex("FF"), Some(mid(0xff)));
    }

    #[test]
    fn encode_parse_round_trip() {
        let id = mid(0x8f3a_2b1c_0000_d4e5u64 as i64);
        let token = encode_mention(id, "Alice");
        assert_eq!(token, "@[Alice](rv:8f3a2b1c0000d4e5)");
        let mentions = parse_mentions(&token);
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].member_id, id);
        assert_eq!(mentions[0].display_name, "Alice");
    }

    #[test]
    fn sanitize_strips_delimiters_and_caps_length() {
        assert_eq!(sanitize_name("a]b)c(d[e"), "abcde");
        assert_eq!(sanitize_name("  spaced  "), "spaced");
        let long: String = "x".repeat(100);
        assert_eq!(sanitize_name(&long).chars().count(), MAX_SNAPSHOT_NAME_LEN);
        // A name laden with the token's own delimiters can't inject a second
        // (fake) mention: stripping `] ( )` removes the `](rv:` sequence needed
        // to break out, so exactly one mention parses and it points at `id`.
        let id = mid(7);
        let token = encode_mention(id, "ev)il](rv:dead)");
        let parsed = parse_mentions(&token);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].member_id, id);
        assert!(!parsed[0].display_name.contains([']', '(', ')']));
    }

    #[test]
    fn parses_multiple_mentions_interleaved_with_text() {
        let a = mid(1);
        let b = mid(2);
        let text = format!(
            "hi {} and {}!",
            encode_mention(a, "Ann"),
            encode_mention(b, "Bob")
        );
        let segs = parse_segments(&text);
        assert_eq!(
            segs,
            vec![
                MentionSegment::Text("hi ".to_string()),
                MentionSegment::Mention(Mention {
                    member_id: a,
                    display_name: "Ann".to_string()
                }),
                MentionSegment::Text(" and ".to_string()),
                MentionSegment::Mention(Mention {
                    member_id: b,
                    display_name: "Bob".to_string()
                }),
                MentionSegment::Text("!".to_string()),
            ]
        );
    }

    #[test]
    fn empty_snapshot_name_is_valid() {
        let id = mid(42);
        let token = format!("@[](rv:{})", member_id_to_hex(id));
        let mentions = parse_mentions(&token);
        assert_eq!(
            mentions,
            vec![Mention {
                member_id: id,
                display_name: String::new()
            }]
        );
    }

    #[test]
    fn non_tokens_are_left_as_text() {
        // Bare `@`, a markdown-ish `[link]` with the wrong scheme, an email-like
        // `a@[b].c`, and unterminated tokens must all stay plain text.
        for s in [
            "just @ a mention symbol",
            "see @[docs](http:01) for details",
            "mail a@[b].c please",
            "@[unterminated",
            "@[name](rv:)",   // empty id
            "@[name](rv:zz)", // non-hex id
            "@[name](xx:01)", // wrong scheme
        ] {
            assert_eq!(
                parse_mentions(s),
                vec![],
                "should not parse a mention from: {s:?}"
            );
            // And the full text survives a segment round-trip.
            let rebuilt: String = parse_segments(s)
                .into_iter()
                .map(|seg| match seg {
                    MentionSegment::Text(t) => t,
                    MentionSegment::Mention(_) => unreachable!(),
                })
                .collect();
            assert_eq!(rebuilt, s);
        }
    }

    #[test]
    fn contains_mention_of_matches_only_the_right_id() {
        let me = mid(100);
        let other = mid(200);
        let text = format!("ping {}", encode_mention(other, "Other"));
        assert!(!contains_mention_of(&text, me));
        let text2 = format!(
            "ping {} and {}",
            encode_mention(other, "Other"),
            encode_mention(me, "Me")
        );
        assert!(contains_mention_of(&text2, me));
    }

    #[test]
    fn render_plaintext_uses_resolver_then_falls_back_to_snapshot() {
        let known = mid(1);
        let unknown = mid(2);
        let text = format!(
            "hey {} and {}",
            encode_mention(known, "OldName"),
            encode_mention(unknown, "Ghost")
        );
        let rendered = render_plaintext(&text, |id| {
            if id == known {
                Some("NewName".to_string()) // current nickname overrides snapshot
            } else {
                None // unknown member -> fall back to snapshot
            }
        });
        assert_eq!(rendered, "hey @NewName and @Ghost");
    }

    #[test]
    fn resolve_typed_mentions_links_known_names_only() {
        let alice = mid(0xa11ce);
        let resolve = |name: &str| -> Option<(MemberId, String)> {
            match name.to_lowercase().as_str() {
                "alice" => Some((alice, "Alice".to_string())),
                _ => None,
            }
        };
        // Known name (with trailing punctuation) becomes a token; the comma is
        // preserved after it. Unknown @name and an email are left untouched.
        let out = resolve_typed_mentions("hi @alice, ping @bob and a@b.com", resolve);
        assert_eq!(
            out,
            format!(
                "hi {}, ping @bob and a@b.com",
                encode_mention(alice, "Alice")
            )
        );
        // The produced token parses back to the right member.
        assert_eq!(
            parse_mentions(&out),
            vec![Mention {
                member_id: alice,
                display_name: "Alice".to_string()
            }]
        );
    }

    #[test]
    fn resolve_typed_mentions_leaves_existing_tokens_untouched() {
        let alice = mid(0xa11ce);
        // Resolver that would match "Alice" — but the bracketed token's run is
        // "[Alice](rv:..)" which never equals "Alice", so it is left as-is.
        let resolve = |name: &str| -> Option<(MemberId, String)> {
            if name.eq_ignore_ascii_case("alice") {
                Some((alice, "Alice".to_string()))
            } else {
                None
            }
        };
        let already = encode_mention(alice, "Alice");
        assert_eq!(resolve_typed_mentions(&already, resolve), already);
    }

    #[test]
    fn token_preserves_surrounding_text_exactly() {
        let id = mid(0xdead_beef);
        let text = format!("(see {}, thanks)", encode_mention(id, "Cat"));
        let rendered = render_plaintext(&text, |_| Some("Cat".to_string()));
        assert_eq!(rendered, "(see @Cat, thanks)");
    }
}
