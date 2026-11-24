//! What may go into the diagnostics log.
//!
//! River's diagnostics are lines on stderr that the desktop shell keeps in a file the user is
//! asked to attach to bug reports (`apps/desktop/src/log.rs`). They may say which room, which
//! contract and what went wrong. They must not carry message text, nicknames, signing keys,
//! secrets or invitation codes.
//!
//! Most lines are fixed words. The exception is an error from a decoder, a room's merge or the
//! node, which a line prints for its cause: serde's decode messages quote the value that was
//! the wrong type (`invalid type: string "hello", expected u32`), and those values are what
//! people wrote. [`scrub`] is how such an error is printed.

use std::fmt::Display;

/// The longest error text a log line keeps.
const MAX_CHARS: usize = 240;

/// `error` as a log line may carry it: everything inside double quotes or backticks replaced by
/// `…`, and cut to [`MAX_CHARS`] characters. The quotes are matched the way `Debug` writes them
/// (a backslash escapes the next character), so a value that holds a quote does not end the
/// quoted run early.
pub fn scrub(error: impl Display) -> String {
    let text = error.to_string();
    let mut out = String::with_capacity(text.len().min(MAX_CHARS + 4));
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '"' || c == '`' {
            // `…` stands for the quoted run, which may end the text unclosed.
            while let Some(d) = chars.next() {
                if d == '\\' {
                    chars.next();
                } else if d == c {
                    break;
                }
            }
            out.push('"');
            out.push('…');
            out.push('"');
        } else {
            out.push(c);
        }
    }
    if out.chars().count() > MAX_CHARS {
        out = out.chars().take(MAX_CHARS).collect();
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_values_are_removed() {
        assert_eq!(
            scrub(r#"invalid type: string "my secret nickname", expected u32"#),
            r#"invalid type: string "…", expected u32"#
        );
        assert_eq!(
            scrub("unknown field `password`, expected one of `a`, `b`"),
            r#"unknown field "…", expected one of "…", "…""#
        );
    }

    #[test]
    fn an_escaped_quote_inside_a_value_does_not_end_it() {
        let shown = format!("{:?}", "he said \"hi\" and then the secret");
        let line = scrub(format!("invalid type: string {shown}, expected u32"));
        assert_eq!(line, r#"invalid type: string "…", expected u32"#);
        assert!(!line.contains("secret"));
    }

    #[test]
    fn an_unclosed_quote_hides_the_rest() {
        assert_eq!(scrub(r#"bad: "starts and never ends"#), r#"bad: "…""#);
    }

    #[test]
    fn plain_text_is_kept_and_long_text_is_cut() {
        assert_eq!(
            scrub("the connection was reset (os error 104)"),
            "the connection was reset (os error 104)"
        );
        let long = scrub("x".repeat(1000));
        assert_eq!(long.chars().count(), MAX_CHARS + 1);
    }
}
