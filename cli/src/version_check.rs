//! Best-effort "you're running an old riverctl" nag.
//!
//! Checks the crates.io **sparse index** for the newest published `riverctl`
//! version and, if the running binary is older, returns a short nudge. Design
//! constraints (all deliberate):
//!
//! - **crates.io, not Freenet.** riverctl is installed via `cargo install
//!   riverctl`, so crates.io is the authoritative "is my binary current?"
//!   source. A Freenet-hosted version record would be dogfood-nice but could
//!   disagree with the actual install channel.
//! - **Never blocks meaningfully / never fails loudly.** Bounded network
//!   timeout, all errors swallowed to `None`. A down crates.io, no network, or
//!   a parse hiccup must never break a command.
//! - **Once per day.** The result is cached in the user's cache dir with a
//!   timestamp; only the first invocation in a 24h window touches the network.
//! - **Opt-out.** `--no-version-check` / `RIVERCTL_NO_VERSION_CHECK=true`.
//! - **stderr only.** The caller prints the nudge to stderr so `--format json`
//!   stdout and downstream scripts are never polluted.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// crates.io sparse index path for `riverctl` (5+ char crate → `ri/ve/<name>`).
const SPARSE_INDEX_URL: &str = "https://index.crates.io/ri/ve/riverctl";
/// Re-check the network at most once per this window.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Hard cap on the network call so a slow/hung crates.io never stalls the CLI.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

/// If a newer `riverctl` than `current` is published on crates.io, return
/// `Some(latest_version_string)`; otherwise `None`. Blocking (run it on a
/// `spawn_blocking` thread). Never panics, never errors — any failure yields
/// `None`.
///
/// Uses a once-per-day on-disk cache so most invocations do zero network I/O.
pub fn latest_if_outdated(current: &str) -> Option<String> {
    let latest = cached_or_fetch_latest()?;
    match (parse_semver(current), parse_semver(&latest)) {
        (Some(cur), Some(new)) if new > cur => Some(latest),
        _ => None,
    }
}

/// Format the user-facing nudge (stderr). Kept here so the wording has one home.
pub fn update_message(current: &str, latest: &str) -> String {
    format!(
        "A newer riverctl is available: {current} -> {latest}. \
         Update with `cargo install riverctl --force`."
    )
}

/// Read the cached latest version if the cache is fresh (< 24h); otherwise hit
/// the network, refresh the cache, and return the fetched value. On a network
/// failure, fall back to a stale cached value if one exists.
fn cached_or_fetch_latest() -> Option<String> {
    let now = unix_now();
    let cache = cache_path();

    if let Some((checked_at, latest)) = cache.as_ref().and_then(read_cache) {
        if now.saturating_sub(checked_at) < CHECK_INTERVAL.as_secs() {
            return Some(latest);
        }
    }

    match fetch_latest_from_index() {
        Some(latest) => {
            if let Some(p) = cache.as_ref() {
                write_cache(p, now, &latest);
            }
            Some(latest)
        }
        // Network failed: better a possibly-stale nag than none. Reuse whatever
        // the cache holds (even if past the interval).
        None => cache.as_ref().and_then(read_cache).map(|(_, v)| v),
    }
}

/// GET the sparse-index file and return the highest non-yanked `vers`.
fn fetch_latest_from_index() -> Option<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(NETWORK_TIMEOUT)
        .user_agent(concat!(
            "riverctl-version-check/",
            env!("CARGO_PKG_VERSION")
        ))
        .build();
    let body = agent
        .get(SPARSE_INDEX_URL)
        .call()
        .ok()?
        .into_string()
        .ok()?;
    highest_non_yanked(&body)
}

/// Parse the newline-delimited JSON sparse-index body and return the highest
/// non-yanked version string. Each line is a JSON object with at least `vers`
/// and `yanked`. Pure, so it's unit-testable without network.
fn highest_non_yanked(body: &str) -> Option<String> {
    body.lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            if v.get("yanked").and_then(|y| y.as_bool()).unwrap_or(false) {
                return None;
            }
            v.get("vers").and_then(|s| s.as_str()).map(str::to_owned)
        })
        .filter(|v| parse_semver(v).is_some())
        .max_by_key(|v| parse_semver(v).unwrap())
}

/// Parse a plain `MAJOR.MINOR.PATCH` version into a comparable tuple, ignoring
/// any pre-release / build suffix (`-rc1`, `+meta`). Returns `None` if the
/// three core numbers aren't present. riverctl only ever publishes plain
/// semver, so this is sufficient and avoids a `semver` dependency.
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None; // more than 3 components — not a plain semver
    }
    Some((major, minor, patch))
}

/// `~/.cache/river/version-check.json` (platform cache dir). `None` if the
/// cache dir can't be resolved — the check then just always hits the network
/// (still bounded and best-effort).
fn cache_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "freenet", "river")
        .map(|d| d.cache_dir().join("version-check.json"))
}

/// Cache shape: `{"checked_at": <unix secs>, "latest": "x.y.z"}`.
fn read_cache(path: &PathBuf) -> Option<(u64, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let checked_at = v.get("checked_at")?.as_u64()?;
    let latest = v.get("latest")?.as_str()?.to_owned();
    Some((checked_at, latest))
}

fn write_cache(path: &PathBuf, checked_at: u64, latest: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::json!({ "checked_at": checked_at, "latest": latest });
    let _ = std::fs::write(path, json.to_string());
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semver_basic_and_suffixes() {
        assert_eq!(parse_semver("0.1.72"), Some((0, 1, 72)));
        assert_eq!(parse_semver("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("1.2.3+build9"), Some((1, 2, 3)));
        assert_eq!(parse_semver(" 10.20.30 "), Some((10, 20, 30)));
        assert_eq!(parse_semver("0.1"), None);
        assert_eq!(parse_semver("0.1.2.3"), None);
        assert_eq!(parse_semver("not-a-version"), None);
    }

    #[test]
    fn semver_ordering_is_numeric_not_lexical() {
        // The bug a string compare would hit: "0.1.9" vs "0.1.10".
        assert!(parse_semver("0.1.10").unwrap() > parse_semver("0.1.9").unwrap());
        assert!(parse_semver("0.2.0").unwrap() > parse_semver("0.1.99").unwrap());
        assert!(parse_semver("1.0.0").unwrap() > parse_semver("0.99.99").unwrap());
    }

    #[test]
    fn highest_non_yanked_picks_max_and_skips_yanked() {
        let body = r#"{"name":"riverctl","vers":"0.1.70","yanked":false}
{"name":"riverctl","vers":"0.1.71","yanked":false}
{"name":"riverctl","vers":"0.1.73","yanked":true}
{"name":"riverctl","vers":"0.1.72","yanked":false}"#;
        // 0.1.73 is yanked, so the newest usable is 0.1.72.
        assert_eq!(highest_non_yanked(body), Some("0.1.72".to_string()));
    }

    #[test]
    fn highest_non_yanked_out_of_order_lines() {
        // Sparse index is publish-ordered, but don't rely on it — take the max.
        let body = r#"{"vers":"0.1.9","yanked":false}
{"vers":"0.1.10","yanked":false}
{"vers":"0.1.2","yanked":false}"#;
        assert_eq!(highest_non_yanked(body), Some("0.1.10".to_string()));
    }

    #[test]
    fn highest_non_yanked_empty_or_garbage_is_none() {
        assert_eq!(highest_non_yanked(""), None);
        assert_eq!(highest_non_yanked("not json\n{broken"), None);
        assert_eq!(
            highest_non_yanked(r#"{"vers":"1.0.0","yanked":true}"#),
            None,
            "all-yanked yields no candidate"
        );
    }

    #[test]
    fn latest_if_outdated_compares_correctly() {
        // Drive the pure comparison via highest_non_yanked + parse_semver by
        // reconstructing the decision (latest_if_outdated itself does I/O).
        let newer = highest_non_yanked(r#"{"vers":"0.1.73","yanked":false}"#).unwrap();
        assert!(parse_semver(&newer).unwrap() > parse_semver("0.1.72").unwrap());
        assert!(!(parse_semver(&newer).unwrap() > parse_semver("0.1.73").unwrap()));
        assert!(!(parse_semver(&newer).unwrap() > parse_semver("0.2.0").unwrap()));
    }
}
