//! `@mention` codec — the inline wire token that references a member by their
//! stable [`MemberId`] while displaying their *current* nickname.
//!
//! ## Wire format
//!
//! A mention is stored inline in the plaintext message `text` field as:
//!
//! ```text
//! @[Display Name](rv:FPVN6PUN)
//! ```
//!
//! - `rv:<ref>` is the **authoritative** reference. The current form is the
//!   member's 8-char truncated-base32 [`MemberId`] `Display` string (e.g.
//!   `FPVN6PUN`) — the *same* short label used everywhere else in the UI and
//!   CLI to name a member, so mentions read consistently with the rest of the
//!   app. Clients re-resolve the member's *current* nickname from `member_info`
//!   by matching this label, so the rendered chip follows renames. The label is
//!   *lossy* (40 of the id's 64 bits), so callers resolve it against the room's
//!   known members rather than decoding it back to a [`MemberId`] directly.
//! - `Display Name` is a **fallback snapshot** captured at send time, used only
//!   when re-resolution is impossible (member not in `member_info`, an
//!   undecryptable private-room nickname, or a client too old to parse the
//!   token). Old clients render the raw token via markdown as a plain
//!   `@Display Name` link — acceptable graceful degradation.
//!
//! ### Legacy reference form (compat — remove eventually)
//!
//! Messages sent before this change carry the reference as 16 lowercase hex
//! digits (the *lossless* full 64-bit id), e.g. `@[Name](rv:8f3a2b1c0000d4e5)`.
//! The parser still accepts that form so old messages keep resolving. New
//! tokens are never emitted in hex. **TODO(mentions): once no message in
//! circulation still carries a legacy `rv:<hex>` token, drop the hex branch of
//! [`member_ref_from_str`] and the [`MemberRef::Legacy`] variant.** The two
//! forms are unambiguous: the current form is exactly 8 base32 chars, the
//! legacy form is up to 16 hex chars, and the parser tries base32 first.
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

/// Scheme prefix inside the mention token's reference, e.g. `rv:FPVN6PUN`.
pub const REF_SCHEME: &str = "rv:";

/// Maximum number of characters retained from the snapshot display name. The
/// snapshot is only a fallback, so a hard cap keeps a hostile/huge nickname
/// from bloating the token. Authoritative resolution uses the id, not this.
pub const MAX_SNAPSHOT_NAME_LEN: usize = 64;

/// A parsed mention: the authoritative member reference plus the snapshot name
/// carried in the token (which may be empty, or stale relative to the member's
/// current nickname — callers should prefer a fresh lookup by `member_ref`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mention {
    pub member_ref: MemberRef,
    pub display_name: String,
}

/// The member a mention token points at. The current wire form carries the
/// member's truncated-base32 `Display` label, which is *lossy* and so cannot be
/// turned back into a full [`MemberId`] on its own — it is matched against the
/// room's known members. The legacy hex form carries the full id directly.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MemberRef {
    /// Current form: the member's 8-char truncated-base32 `Display` label
    /// (e.g. `FPVN6PUN`). Resolve it against the room via [`MemberRef::resolve`].
    Short(String),
    /// Legacy form: a full 64-bit id recovered from a 16-hex-digit token.
    /// TODO(mentions): remove once no legacy `rv:<hex>` token remains in
    /// circulation (see the module-level note).
    Legacy(MemberId),
}

impl MemberRef {
    /// Whether this reference names `id`. The current (short) form compares the
    /// truncated-base32 label the rest of the app uses to identify a member
    /// (`MemberId`'s `Display`); the legacy form compares the full 64-bit id.
    pub fn matches(&self, id: MemberId) -> bool {
        match self {
            MemberRef::Short(label) => id.to_string() == *label,
            MemberRef::Legacy(full) => *full == id,
        }
    }

    /// The full [`MemberId`] this reference denotes, if recoverable. A legacy
    /// ref carries it directly; a short ref is matched against `candidates` (the
    /// room's known members) and yields `None` when this client doesn't know the
    /// named member — in which case the mention degrades to its snapshot name.
    ///
    /// If two members in `candidates` share the same 8-char label (a ~2⁻⁴⁰
    /// truncation collision — astronomically rare, and the same limit the rest
    /// of the app already accepts when naming a member by short id), the lowest
    /// id wins, so resolution is deterministic and reproducible rather than
    /// dependent on candidate iteration order.
    pub fn resolve(&self, candidates: impl IntoIterator<Item = MemberId>) -> Option<MemberId> {
        match self {
            MemberRef::Legacy(full) => Some(*full),
            // `min()` (not `find()`) so a short-label collision resolves to one
            // well-defined member regardless of candidate iteration order.
            MemberRef::Short(_) => candidates.into_iter().filter(|id| self.matches(*id)).min(),
        }
    }
}

/// One piece of a message body: either a run of plain text or a mention token.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MentionSegment {
    Text(String),
    Mention(Mention),
}

/// Encode a [`MemberId`] as the 16-char lowercase-hex form. This is the
/// *lossless* full-id encoding. It is NOT the wire-token reference (that is the
/// truncated-base32 [`member_id_to_short`]); it is used only for in-session,
/// full-precision handoffs that never persist — e.g. the rendered chip's
/// `data-member-id` DOM attribute, read straight back by the click interceptor.
pub fn member_id_to_hex(id: MemberId) -> String {
    // MemberId(FastHash(i64)); encode the raw 64 bits.
    format!("{:016x}", id.0 .0 as u64)
}

/// Parse a 1..=16 digit hex string back into a [`MemberId`]. Returns `None` for
/// empty input, over-long input, or non-hex characters. Counterpart of
/// [`member_id_to_hex`] (the lossless full-id encoding), and also the decoder
/// for the legacy `rv:<hex>` wire-token form (see [`member_ref_from_str`]).
pub fn member_id_from_hex(s: &str) -> Option<MemberId> {
    if s.is_empty() || s.len() > 16 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let raw = u64::from_str_radix(s, 16).ok()?;
    Some(MemberId(FastHash(raw as i64)))
}

/// Encode a [`MemberId`] as its canonical short reference: the 8-char
/// truncated-base32 `Display` label (e.g. `FPVN6PUN`) used everywhere else to
/// name a member. This is the current wire-token reference form. It is *lossy*
/// (40 of 64 bits), so it round-trips through [`MemberRef`] (resolved against
/// the room's members), not back into a [`MemberId`] directly.
pub fn member_id_to_short(id: MemberId) -> String {
    id.to_string()
}

/// Parse a mention reference body (the part after `rv:`) into a [`MemberRef`].
///
/// Accepts both wire forms, current first: an 8-char truncated-base32 label
/// (the [`member_id_to_short`] form) becomes [`MemberRef::Short`]; otherwise a
/// 1..=16 hex-digit legacy id becomes [`MemberRef::Legacy`]. Returns `None` for
/// anything else. The two forms can't collide — the current form is exactly 8
/// base32 chars and is tried first, the legacy form is up to 16 hex chars.
pub fn member_ref_from_str(s: &str) -> Option<MemberRef> {
    if is_short_ref(s) {
        return Some(MemberRef::Short(s.to_string()));
    }
    // TODO(mentions): remove this legacy hex fallback once no message in
    // circulation still carries an `rv:<hex>` token (see module-level note).
    member_id_from_hex(s).map(MemberRef::Legacy)
}

/// Whether `s` is a current-form short reference: exactly 8 characters from the
/// RFC4648 base32 alphabet (uppercase `A`–`Z` and digits `2`–`7`), matching the
/// `truncated_base32` output that backs `MemberId`'s `Display`.
fn is_short_ref(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| matches!(b, b'A'..=b'Z' | b'2'..=b'7'))
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

/// Build the wire token `@[name](rv:FPVN6PUN)` for a mention, using the
/// member's short (truncated-base32) reference. The name is sanitized.
pub fn encode_mention(id: MemberId, display_name: &str) -> String {
    format!(
        "@[{}]({}{})",
        sanitize_name(display_name),
        REF_SCHEME,
        member_id_to_short(id)
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
    // The reference body runs to the closing `)`.
    let id_start = after + prefix_len;
    let mut k = id_start;
    while k < bytes.len() && bytes[k] != b')' {
        k += 1;
    }
    if k >= bytes.len() {
        return None; // unterminated `(`
    }
    let member_ref = member_ref_from_str(&text[id_start..k])?;
    Some((
        Mention {
            member_ref,
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
    parse_mentions(text)
        .iter()
        .any(|m| m.member_ref.matches(id))
}

/// Render the text for a plain-text surface (e.g. the CLI), replacing each
/// mention token with `@<name>`. `resolve` maps the token's [`MemberRef`] to the
/// member's *current* display name (typically by scanning the room's members
/// with [`MemberRef::matches`]); when it returns `None` the snapshot name in the
/// token is used.
pub fn render_plaintext<F>(text: &str, mut resolve: F) -> String
where
    F: FnMut(&MemberRef) -> Option<String>,
{
    let mut out = String::with_capacity(text.len());
    for seg in parse_segments(text) {
        match seg {
            MentionSegment::Text(t) => out.push_str(&t),
            MentionSegment::Mention(m) => {
                let name = resolve(&m.member_ref).unwrap_or(m.display_name);
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
        // The current form is the member's truncated-base32 Display label, not
        // hex — consistent with how members are named everywhere else.
        assert_eq!(token, format!("@[Alice](rv:{})", member_id_to_short(id)));
        assert!(!token.contains(&member_id_to_hex(id)), "must not emit hex");
        let mentions = parse_mentions(&token);
        assert_eq!(mentions.len(), 1);
        assert_eq!(
            mentions[0].member_ref,
            MemberRef::Short(member_id_to_short(id))
        );
        assert!(mentions[0].member_ref.matches(id));
        assert_eq!(mentions[0].display_name, "Alice");
    }

    #[test]
    fn current_token_uses_truncated_base32_matching_display() {
        // The token reference is byte-identical to the member's Display label
        // (`MemberId`'s `Display`), which is exactly what the rest of the app
        // shows. 8 base32 chars, no lowercase hex.
        let id = mid(0x422a_2a8d_3edf_ea2bu64 as i64);
        let short = member_id_to_short(id);
        assert_eq!(short, id.to_string());
        assert_eq!(short.len(), 8);
        let token = encode_mention(id, "Ivvor");
        assert!(token.contains(&format!("(rv:{short})")), "token: {token}");
    }

    #[test]
    fn legacy_hex_token_still_parses_and_matches() {
        // A token sent by an old client carries the lossless 16-hex-digit id.
        let id = mid(0x8f3a_2b1c_0000_d4e5u64 as i64);
        let legacy = format!("@[Name](rv:{})", member_id_to_hex(id));
        let mentions = parse_mentions(&legacy);
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].member_ref, MemberRef::Legacy(id));
        assert!(
            mentions[0].member_ref.matches(id),
            "legacy ref resolves to its id"
        );
    }

    #[test]
    fn member_ref_from_str_disambiguates_base32_from_hex() {
        let id = mid(0x422a_2a8d_3edf_ea2bu64 as i64);
        // 8-char base32 -> Short (tried first).
        assert_eq!(
            member_ref_from_str(&member_id_to_short(id)),
            Some(MemberRef::Short(member_id_to_short(id)))
        );
        // 16-char hex -> Legacy.
        assert_eq!(
            member_ref_from_str(&member_id_to_hex(id)),
            Some(MemberRef::Legacy(id))
        );
        // An 8-char body of only base32 digits (2-7) is the current form, NOT
        // mis-read as hex — base32 is tried first and these are valid base32.
        assert_eq!(
            member_ref_from_str("23456723"),
            Some(MemberRef::Short("23456723".to_string()))
        );
        // Garbage / empty / wrong length -> no ref.
        assert_eq!(member_ref_from_str(""), None);
        assert_eq!(member_ref_from_str("zz"), None);
    }

    #[test]
    fn member_ref_resolve_recovers_member_id() {
        let id = mid(0x1234_5678_9abc_def0u64 as i64);
        let other = mid(99);
        // Short ref recovers the full id only when the member is among candidates.
        let short = MemberRef::Short(member_id_to_short(id));
        assert_eq!(short.resolve([other, id]), Some(id));
        assert_eq!(short.resolve([other]), None);
        // Legacy ref carries the id directly, no candidates needed.
        assert_eq!(MemberRef::Legacy(id).resolve(std::iter::empty()), Some(id));
    }

    #[test]
    fn resolve_is_deterministic_on_truncated_label_collision() {
        // The short label encodes the low 40 bits of the id, so two ids that
        // share those bits collide. `resolve` must pick one well-defined member
        // (lowest id) regardless of candidate order — not whatever iterates
        // first. (~2⁻⁴⁰ event; this asserts the behaviour is at least defined.)
        let low = mid(0x42);
        let high = mid(0x42 + (1i64 << 40));
        assert_eq!(
            member_id_to_short(low),
            member_id_to_short(high),
            "low-40-bit-equal ids must share a label"
        );
        let r = MemberRef::Short(member_id_to_short(low));
        assert_eq!(r.resolve([low, high]), Some(low));
        assert_eq!(r.resolve([high, low]), Some(low), "order-independent");
    }

    #[test]
    fn parses_mixed_legacy_and_current_tokens_in_one_body() {
        // A single message may interleave an old hex token and a new base32 one
        // (e.g. an edit, or a quote of an old message). Each parses to the right
        // form independently and segment ordering is preserved.
        let a = mid(0x0a);
        let b = mid(0x0b);
        let text = format!(
            "x @[A](rv:{}) y {} z",
            member_id_to_hex(a),    // legacy hex form
            encode_mention(b, "B")  // current base32 form
        );
        let segs = parse_segments(&text);
        assert_eq!(
            segs,
            vec![
                MentionSegment::Text("x ".to_string()),
                MentionSegment::Mention(Mention {
                    member_ref: MemberRef::Legacy(a),
                    display_name: "A".to_string()
                }),
                MentionSegment::Text(" y ".to_string()),
                MentionSegment::Mention(Mention {
                    member_ref: MemberRef::Short(member_id_to_short(b)),
                    display_name: "B".to_string()
                }),
                MentionSegment::Text(" z".to_string()),
            ]
        );
    }

    #[test]
    fn contains_mention_of_matches_legacy_hex_token() {
        // A self-mention arriving in an OLD (hex) message must still be detected
        // so it drives a notification. Hand-build the legacy form.
        let me = mid(0x8f3a_2b1c_0000_d4e5u64 as i64);
        let other = mid(7);
        let text = format!("ping @[Me](rv:{})", member_id_to_hex(me));
        assert!(contains_mention_of(&text, me));
        assert!(!contains_mention_of(&text, other));
    }

    #[test]
    fn multibyte_snapshot_name_round_trips_around_base32_ref() {
        // The byte-scanning parser must handle multibyte text around a token and
        // a multibyte snapshot name without splitting a codepoint.
        let id = mid(0x1234_5678_9abc_def0u64 as i64);
        let token = encode_mention(id, "名前🎉");
        let segs = parse_segments(&format!("こんにちは {token}!"));
        let m = segs
            .iter()
            .find_map(|s| match s {
                MentionSegment::Mention(m) => Some(m),
                MentionSegment::Text(_) => None,
            })
            .expect("exactly one mention");
        assert!(m.member_ref.matches(id));
        assert_eq!(m.display_name, "名前🎉");
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
        assert!(parsed[0].member_ref.matches(id));
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
                    member_ref: MemberRef::Short(member_id_to_short(a)),
                    display_name: "Ann".to_string()
                }),
                MentionSegment::Text(" and ".to_string()),
                MentionSegment::Mention(Mention {
                    member_ref: MemberRef::Short(member_id_to_short(b)),
                    display_name: "Bob".to_string()
                }),
                MentionSegment::Text("!".to_string()),
            ]
        );
    }

    #[test]
    fn empty_snapshot_name_is_valid() {
        let id = mid(42);
        let token = format!("@[](rv:{})", member_id_to_short(id));
        let mentions = parse_mentions(&token);
        assert_eq!(
            mentions,
            vec![Mention {
                member_ref: MemberRef::Short(member_id_to_short(id)),
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
        let rendered = render_plaintext(&text, |r| {
            if r.matches(known) {
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
                member_ref: MemberRef::Short(member_id_to_short(alice)),
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
