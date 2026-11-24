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
//!
//! `eprintln!` is not used in `apps/`: [`diag!`] is, so each line carries a UTC time.

use std::fmt::{Arguments, Display};
use std::io::Write;

/// A diagnostic line on stderr: the UTC time (ISO 8601), then the message. Used in place of
/// `eprintln!` everywhere in `apps/` (a test pins that), so the log file has a time on each line
/// without anything between the program and the file. Writes through `writeln!` and ignores the
/// result: a closed or broken stderr must not panic a diagnostic.
#[macro_export]
macro_rules! diag {
    ($($arg:tt)*) => {
        $crate::diag::line(::std::format_args!($($arg)*))
    };
}

/// What [`diag!`] calls.
pub fn line(message: Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "{} {message}", iso8601(now_millis()));
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `2026-11-23T14:05:09.123Z` for `millis` since the Unix epoch.
pub fn iso8601(millis: u64) -> String {
    let secs = millis / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        millis % 1000
    )
}

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
    fn timestamps_are_utc_iso_8601() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso8601(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(iso8601(1_795_000_000_999), "2026-11-18T11:06:40.999Z");
    }

    /// Every line the app prints goes through `diag!`, and the places an error's own text is
    /// printed (a decoder, a room merge, the node) go through `scrub`. Reads the sources, so a
    /// new `eprintln!` or a raw `{e}` in a `diag!` fails here.
    #[test]
    fn no_eprintln_and_every_error_text_is_scrubbed() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        for dir in ["river-app/src", "desktop/src"] {
            walk(&root.join(dir), &mut files);
        }
        let mut scrubbed = 0;
        for f in &files {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(f).unwrap();
            // Test modules and files may print what they like.
            let code = text.split("\n#[cfg(test)]").next().unwrap();
            if name.ends_with("tests.rs") || name == "diag.rs" {
                continue;
            }
            assert!(
                !code.contains("eprintln!") && !code.contains("eprint!"),
                "{}: use diag!",
                f.display()
            );
            scrubbed += code.matches("diag::scrub(").count();
            for (n, l) in code.lines().enumerate() {
                if l.contains("diag!(") && (l.contains("{e}") || l.contains("{why}")) {
                    panic!(
                        "{}:{}: an error printed raw, use diag::scrub",
                        f.display(),
                        n + 1
                    );
                }
            }
        }
        assert_eq!(
            scrubbed, 17,
            "the scrubbed sites changed: update this count on purpose"
        );
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

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
