//! Recognise Freenet share links in message text.
//!
//! A share link names a contract webapp without naming a gateway:
//!
//! - `https://freenet.org/open#<contract-id><rest>` (also `.../open/#...`),
//!   the web page that offers "open on your node" buttons, and
//! - `freenet:<contract-id><rest>` / `freenet://<contract-id><rest>`, the
//!   OS link handler's scheme.
//!
//! River turns a valid one into a link to the same webapp on the reader's own
//! node (`/v1/contract/web/<id><rest>`, resolved against River's origin), so
//! one click opens the app instead of going through the freenet.org page.
//!
//! Message text is attacker-controlled, so validation here is a port of the
//! two existing validators, and MUST stay identical to them:
//!
//! - the freenet.org/open page (freenet/web,
//!   `hugo-site/themes/freenet/layouts/shortcodes/open-link.html`), and
//! - the `freenet:` handler (freenet/freenet-core,
//!   `crates/core/src/bin/commands/open_link.rs`).
//!
//! Both are checked against a shared vector file, a copy of which lives next
//! to this module (`share-link-vectors.json`) and drives the tests below. Change
//! a rule here only together with that file and the other two sides.

/// Longest contract-id candidate considered at all (a real id is 43-44 chars).
const MAX_CANDIDATE_ID_LEN: usize = 64;
/// Longest `<rest>` (path, query and app fragment) accepted.
const MAX_REST_LEN: usize = 2000;
/// A contract instance id is a 32-byte hash.
const CONTRACT_KEY_BYTES: usize = 32;
/// Longest link any form can validate: `freenet://` + id + rest.
pub(crate) const MAX_SHARE_LINK_LEN: usize =
    "freenet://".len() + MAX_CANDIDATE_ID_LEN + MAX_REST_LEN;

/// A validated share-link destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareTarget {
    contract_id: String,
    rest: String,
}

impl ShareTarget {
    /// The gateway path of the webapp: `/v1/contract/web/<id><rest>`.
    ///
    /// Only built from a validated id and rest, so it always stays under the
    /// `/v1/contract/web/<id>` prefix once a browser resolves it.
    pub fn local_path(&self) -> String {
        format!("/v1/contract/web/{}{}", self.contract_id, self.rest)
    }
}

/// Parse any share-link form River recognises: a freenet.org/open URL or a
/// `freenet:` link. `None` for anything else, including every invalid link.
pub fn parse_share_link(text: &str) -> Option<ShareTarget> {
    parse_freenet_org_open_url(text).or_else(|| parse_freenet_link(text))
}

/// Parse `https://freenet.org/open#<raw>` or `https://freenet.org/open/#<raw>`.
///
/// The scheme and host are matched case-insensitively (a browser treats them
/// that way, so both reach the page); the path is matched exactly. Everything
/// after the FIRST `#` is `<raw>`, exactly what the page reads from
/// `location.hash`; a second `#` inside it is the app's own fragment.
pub fn parse_freenet_org_open_url(url: &str) -> Option<ShareTarget> {
    const ORIGIN: &str = "https://freenet.org";
    let after_origin = match url.get(..ORIGIN.len()) {
        Some(p) if p.eq_ignore_ascii_case(ORIGIN) => &url[ORIGIN.len()..],
        _ => return None,
    };
    let raw = after_origin
        .strip_prefix("/open#")
        .or_else(|| after_origin.strip_prefix("/open/#"))?;
    parse_share_fragment(raw)
}

/// Parse a full `freenet://<id><rest>` or `freenet:<id><rest>` link. Port of
/// the handler's `parse_freenet_link`.
///
/// The scheme is matched case-insensitively. In the authority-less form the id
/// must follow the colon directly, so `freenet:/x` and `freenet:///x` are
/// refused rather than guessed at. In the `//` form the id sits in the URL's
/// host position, which some desktops lowercase (KDE, via Qt's `QUrl`); about a
/// quarter of lowercased ids still decode to a valid but DIFFERENT id, so an id
/// with no uppercase letter is refused there as a lowercased link.
pub fn parse_freenet_link(link: &str) -> Option<ShareTarget> {
    const SCHEME: &str = "freenet:";
    let rest = match link.get(..SCHEME.len()) {
        Some(p) if p.eq_ignore_ascii_case(SCHEME) => &link[SCHEME.len()..],
        _ => return None,
    };
    match rest.strip_prefix("//") {
        Some(authority_form) => {
            let target = parse_share_fragment(authority_form)?;
            if !target.contract_id.bytes().any(|b| b.is_ascii_uppercase()) {
                return None;
            }
            Some(target)
        }
        None if rest.starts_with('/') => None,
        None => parse_share_fragment(rest),
    }
}

/// Validate `<contract-id><rest>`, the part of a share link after
/// `https://freenet.org/open#` or `freenet:`. Port of the page's
/// `splitFragment` + `parseContractId` + `validateRest`.
pub fn parse_share_fragment(raw: &str) -> Option<ShareTarget> {
    if raw.is_empty() {
        return None;
    }
    let split = raw.find(['/', '?', '#']).unwrap_or(raw.len());
    let (id, rest) = raw.split_at(split);
    if !is_valid_contract_id(id) || !is_valid_rest(rest) {
        return None;
    }
    Some(ShareTarget {
        contract_id: id.to_string(),
        rest: rest.to_string(),
    })
}

/// Strict, round-trip validation of a base58 contract id.
///
/// A length/charset check is not enough: freenet-stdlib's decoder zero-pads
/// short input rather than rejecting it, so the decoded bytes must re-encode
/// to the identical text. An id made only of `'1'`s is refused to match the
/// page, whose decoder turns 32 `'1'`s into 33 bytes.
fn is_valid_contract_id(candidate: &str) -> bool {
    if candidate.is_empty() || candidate.len() > MAX_CANDIDATE_ID_LEN {
        return false;
    }
    if !candidate.bytes().all(|b| {
        matches!(b, b'1'..=b'9' | b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'Z' | b'a'..=b'k' | b'm'..=b'z')
    }) {
        return false;
    }
    if candidate.bytes().all(|b| b == b'1') {
        return false;
    }
    let Ok(decoded) = bs58::decode(candidate)
        .with_alphabet(bs58::Alphabet::BITCOIN)
        .into_vec()
    else {
        return false;
    };
    if decoded.len() != CONTRACT_KEY_BYTES {
        return false;
    }
    bs58::encode(&decoded)
        .with_alphabet(bs58::Alphabet::BITCOIN)
        .into_string()
        == candidate
}

/// RFC 3986 unreserved + reserved characters, `%`, and `{}|^` (which WHATWG
/// leaves unencoded in a fragment). Everything else, including space, `"`,
/// `<`, `>`, `\`, backtick, control characters and non-ASCII, is refused.
fn is_allowed_rest_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b':'
                | b'/'
                | b'?'
                | b'#'
                | b'['
                | b']'
                | b'@'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b'%'
                | b'{'
                | b'}'
                | b'|'
                | b'^'
        )
}

/// Validate everything after the id, so it can only ever be a sub-path (plus
/// query and fragment) of `/v1/contract/web/<id>`.
fn is_valid_rest(rest: &str) -> bool {
    if rest.len() > MAX_REST_LEN {
        return false;
    }
    let bytes = rest.as_bytes();
    if !bytes.iter().copied().all(is_allowed_rest_byte) {
        return false;
    }
    // A "//host/..." rest could be read as a new authority component.
    if rest.starts_with("//") {
        return false;
    }
    // Every '%' must start a well-formed escape that does not encode a
    // control character.
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'%' {
            continue;
        }
        let Some(hex) = rest.get(i + 1..i + 3) else {
            return false;
        };
        if !hex.bytes().all(|h| h.is_ascii_hexdigit()) {
            return false;
        }
        let Ok(code) = u8::from_str_radix(hex, 16) else {
            return false;
        };
        if code <= 0x1f || code == 0x7f {
            return false;
        }
    }
    // No dot segment in the path, including percent-encoded ones, which a
    // browser still resolves: `/%2e%2e/<other-id>/` would open a different
    // contract. Dots after `?` or `#` are not path segments.
    let path_part = rest.split(['?', '#']).next().unwrap_or("");
    !path_part.split('/').any(is_dot_segment)
}

/// A single- or double-dot segment once percent-decoded, per the WHATWG URL
/// Standard (compared against the raw segment, case-insensitively, exactly as
/// a browser does).
pub(crate) fn is_dot_segment(segment: &str) -> bool {
    let s = segment.to_ascii_lowercase();
    matches!(s.as_str(), "." | "%2e" | ".." | ".%2e" | "%2e." | "%2e%2e")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copy of the shared vector file used by freenet.org/open and the
    /// freenet-core `freenet:` handler. Kept byte-identical to freenet-core
    /// main (`crates/core/tests/data/share-link-vectors.json`).
    const VECTORS_JSON: &str = include_str!("share-link-vectors.json");

    struct Vector {
        raw: String,
        valid: bool,
        local_path: Option<String>,
        note: String,
    }

    fn vectors() -> Vec<Vector> {
        let parsed: serde_json::Value =
            serde_json::from_str(VECTORS_JSON).expect("vector file is valid JSON");
        let list = parsed["vectors"].as_array().expect("`vectors` array");
        assert!(list.len() >= 50, "vector file looks truncated");
        list.iter()
            .map(|v| Vector {
                raw: v["raw"].as_str().expect("raw").to_string(),
                valid: v["valid"].as_bool().expect("valid"),
                local_path: v["local_path"].as_str().map(str::to_string),
                note: v["note"].as_str().unwrap_or("").to_string(),
            })
            .collect()
    }

    /// Every form River recognises must agree with the shared vectors. The
    /// `freenet://` form additionally refuses an id with no uppercase letter;
    /// every valid vector's id has one, so its expected result is the same.
    #[test]
    fn shared_vectors_all_forms() {
        let prefixes = [
            "https://freenet.org/open#",
            "https://freenet.org/open/#",
            "freenet:",
            "freenet://",
        ];
        for v in vectors() {
            for prefix in prefixes {
                let link = format!("{prefix}{}", v.raw);
                let got = parse_share_link(&link).map(|t| t.local_path());
                if v.valid {
                    assert_eq!(
                        got.as_deref(),
                        v.local_path.as_deref(),
                        "valid vector ({}) via {prefix:?} must map to its local_path",
                        v.note
                    );
                } else {
                    assert_eq!(
                        got, None,
                        "invalid vector ({}) via {prefix:?} must be refused",
                        v.note
                    );
                }
            }
        }
    }

    #[test]
    fn scheme_and_host_are_case_insensitive_path_is_not() {
        let id = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
        let expected = Some(format!("/v1/contract/web/{id}/"));
        for link in [
            format!("HTTPS://FreeNet.ORG/open#{id}/"),
            format!("FREENET:{id}/"),
            format!("Freenet://{id}/"),
        ] {
            assert_eq!(parse_share_link(&link).map(|t| t.local_path()), expected);
        }
        assert_eq!(
            parse_share_link(&format!("https://freenet.org/Open#{id}/")),
            None
        );
    }

    #[test]
    fn other_origins_and_paths_are_refused() {
        let id = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
        for link in [
            format!("http://freenet.org/open#{id}/"),
            format!("https://www.freenet.org/open#{id}/"),
            format!("https://freenet.org.evil.example/open#{id}/"),
            format!("https://freenet.org@evil.example/open#{id}/"),
            format!("https://freenet.org:8443/open#{id}/"),
            format!("https://evil.example/open#{id}/"),
            format!("https://freenet.org/opener#{id}/"),
            format!("https://freenet.org/open?x=1#{id}/"),
            format!("https://freenet.org/other/open#{id}/"),
            "https://freenet.org/open".to_string(),
            "https://freenet.org/open#".to_string(),
        ] {
            assert_eq!(parse_share_link(&link), None, "{link} must be refused");
        }
    }

    #[test]
    fn freenet_scheme_edge_forms() {
        let id = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
        // Authority-less form refuses a leading slash rather than guessing.
        assert_eq!(parse_freenet_link(&format!("freenet:/{id}/")), None);
        assert_eq!(parse_freenet_link(&format!("freenet:///{id}/")), None);
        // A lowercased id in the host position is refused even when it
        // happens to decode to a valid (different) id.
        let lower = id.to_ascii_lowercase();
        assert_eq!(parse_freenet_link(&format!("freenet://{lower}/")), None);
        // Not the scheme at all.
        assert_eq!(parse_freenet_link(&format!("freenetx:{id}")), None);
        assert_eq!(parse_freenet_link(&format!("xfreenet:{id}")), None);
    }

    /// A lowercased id that still decodes to a valid (different) 32-byte id
    /// is refused in the `//` form, where desktops lowercase the host, but,
    /// as on the handler, accepted in the authority-less form.
    #[test]
    fn lowercased_valid_id_is_refused_only_in_authority_form() {
        let lower = (0u8..=255)
            .map(|seed| bs58::encode([seed; 32]).into_string().to_ascii_lowercase())
            .find(|s| is_valid_contract_id(s))
            .expect("some lowercased id round-trips");
        assert_eq!(parse_freenet_link(&format!("freenet://{lower}/")), None);
        assert!(parse_freenet_link(&format!("freenet:{lower}/")).is_some());
    }
}
