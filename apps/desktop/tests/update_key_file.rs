// Test code stamps arbitrary times; the rule is for what production signs (freenet/river#971).
#![allow(clippy::disallowed_methods)]
//! The update key River's releases trust is committed in `update-pubkey.hex` and compiled in
//! by the release workflow. These tests fail a pull request that breaks either half.

const KEY_FILE: &str = include_str!("../update-pubkey.hex");
const WORKFLOW: &str = include_str!("../../../.github/workflows/release-desktop.yml");

#[test]
fn the_committed_update_key_is_a_real_key() {
    let key = KEY_FILE.replace("\r\n", "\n");
    let key = key.strip_suffix('\n').unwrap_or(&key);
    assert_eq!(key.len(), 64, "update-pubkey.hex must hold 64 hex digits");
    assert!(
        key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "update-pubkey.hex must be lowercase hex"
    );
    assert!(
        key.bytes().any(|b| b != b'0'),
        "update-pubkey.hex is the all-zero placeholder key"
    );
}

/// Every release job that builds river-desktop reads the committed key into the build's
/// environment before it builds, and the tag check compares against the same file.
#[test]
fn every_release_build_job_reads_the_committed_key() {
    let wf = WORKFLOW.replace("\r\n", "\n");
    let jobs = wf.split("\njobs:\n").nth(1).expect("a jobs: section");
    let mut checked = 0;
    let mut starts: Vec<usize> = jobs
        .match_indices("\n  ")
        .filter(|(i, _)| {
            let rest = &jobs[i + 3..];
            !rest.starts_with(' ') && rest.lines().next().is_some_and(|l| l.ends_with(':'))
        })
        .map(|(i, _)| i)
        .collect();
    starts.push(jobs.len());
    for w in starts.windows(2) {
        let job = &jobs[w[0]..w[1]];
        let Some(build) = job.find("cargo build --release --locked -p river-desktop") else {
            continue;
        };
        let name = job.lines().nth(1).unwrap_or("").trim();
        let read = job
            .find("desktop/update-pubkey.hex")
            .unwrap_or_else(|| panic!("{name} never reads update-pubkey.hex"));
        assert!(read < build, "{name} reads the key after it builds");
        let export = job
            .find("RIVER_UPDATE_PUBKEY_HEX=$key\" >> \"$GITHUB_ENV\"")
            .unwrap_or_else(|| panic!("{name} does not export the key to the build"));
        assert!(export < build, "{name} exports the key after it builds");
        assert!(
            job[build..].contains("expected=$(cat desktop/update-pubkey.hex)"),
            "{name}: the tag check must compare with the committed file"
        );
        assert!(
            !job.contains("vars.RIVER_UPDATE_PUBKEY_HEX"),
            "{name} must not take the key from a repository variable"
        );
        checked += 1;
    }
    assert_eq!(
        checked, 3,
        "expected the linux, macos and windows build jobs"
    );
}
