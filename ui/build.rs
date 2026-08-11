use chrono::Utc;
use freenet_migrate_build::Component;
use std::path::Path;
use std::process::Command;

fn main() {
    // This script emits explicit `cargo:rerun-if-changed` directives, which
    // disables Cargo's default re-run-on-any-package-change heuristic — so
    // EVERY input the script reads must be declared, here or in
    // `generate_legacy_delegates`. The previous revision instead emitted NO
    // directives and relied on the default heuristic, but that heuristic only
    // watches files inside this package: `../legacy_delegates.toml` lives at
    // the workspace root, so editing it (exactly what `scripts/add-migration.sh`
    // does) did NOT re-run this script, and a rebuild could ship a bundle whose
    // baked `LEGACY_DELEGATES` omitted the outgoing delegate. The startup sweep
    // then probes nobody and every returning user loses their rooms, silently —
    // the delta#52 failure mode. See freenet/delta's `ui/build.rs` for the
    // original of this shape.
    println!("cargo:rerun-if-changed=build.rs");
    generate_build_info();
    generate_legacy_delegates();
}

fn generate_build_info() {
    // `GIT_COMMIT_HASH` depends on git state, so declare the two files that
    // decide it. `BUILD_TIMESTAMP_ISO` refreshes whenever this script re-runs
    // (any declared input changed) and on demand via FORCE_REBUILD_TIMESTAMP —
    // it is no longer stamped on literally every compile, but every publish
    // follows a commit (HEAD moves), so a published build is always freshly
    // stamped.
    emit_git_ref_rerun_paths();
    println!("cargo:rerun-if-env-changed=FORCE_REBUILD_TIMESTAMP");

    // Get the current UTC date and time
    let now = Utc::now();
    // Use ISO 8601 format (UTC) e.g., "2023-10-27T10:30:00Z"
    // This is easily parseable by JavaScript's Date object.
    let build_timestamp_iso = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Set the BUILD_TIMESTAMP_ISO environment variable for the main crate compilation
    println!(
        "cargo:rustc-env=BUILD_TIMESTAMP_ISO={}",
        build_timestamp_iso
    );

    // Get git commit hash (short). Check the exit status, not just that the
    // process spawned: git can spawn fine, exit non-zero, and write nothing,
    // which would bake an empty string rather than "unknown".
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_COMMIT_HASH={}", git_hash);
}

/// Emit `rerun-if-changed` for the two git files that decide `GIT_COMMIT_HASH`:
/// `HEAD` (which commit / branch) and `index` (staging, which moves on commit).
///
/// Do NOT hardcode `../.git/HEAD`: **in a git worktree `.git` is a FILE**
/// holding `gitdir: …/.git/worktrees/<name>`, so a literal path does not
/// resolve — and Cargo treats an unresolvable `rerun-if-changed` target as
/// permanently dirty, re-running the script on every build. `git rev-parse
/// --git-path` resolves correctly in both layouts.
fn emit_git_ref_rerun_paths() {
    for spec in ["HEAD", "index"] {
        if let Some(path) = git_ref_path(spec) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// Resolve a git metadata file, or `None` if it cannot be resolved.
///
/// Returning `None` (no directive) is deliberate for the no-git case, e.g.
/// building from a source tarball: emitting a nonexistent path would make the
/// script permanently dirty, and `GIT_COMMIT_HASH` is already `"unknown"`
/// whenever git cannot resolve HEAD, so there is nothing for a stale
/// fingerprint to misreport.
fn git_ref_path(spec: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", spec])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    // The existence check is the actual guard: `--git-path` returns a path
    // whether or not the file is present, and a nonexistent path would
    // re-introduce the always-dirty behaviour this function exists to avoid.
    if path.is_empty() || !Path::new(&path).exists() {
        return None;
    }
    Some(path)
}

/// A docs.rs build, the one environment where the registry legitimately does
/// not exist (docs.rs builds the packaged crate, and the TOML lives at the
/// workspace root, outside this package).
fn docs_rs_build() -> bool {
    std::env::var_os("DOCS_RS").is_some()
}

/// Generate the `LEGACY_DELEGATES` const from `../legacy_delegates.toml` via
/// `freenet-migrate-build` (freenet/river#398): same `&[([u8; 32], [u8; 32])]`
/// shape and (delegate_key, code_hash) order this script used to hand-roll,
/// with every hash — and each row's `delegate_key` derivation — validated at
/// build time.
///
/// * `.rerun_if_changed(true)`: the crate declares the registry itself as an
///   input, so `scripts/add-migration.sh` editing it invalidates the cached
///   build script. This is load-bearing — see the comment in `main`.
/// * `.allow_missing_registry(docs_rs_build())`: a missing registry is a HARD
///   BUILD FAILURE except on docs.rs. The blanket `true` this replaces meant a
///   missing file yielded an empty table with at most a `cargo:warning` — the
///   same silent-total-migration-failure outcome as the staleness bug, by
///   another route.
fn generate_legacy_delegates() {
    let dest = freenet_migrate_build::codegen()
        .entry_registry("../legacy_delegates.toml", Component::Delegate)
        .canonical_consts(false)
        .delegate_pair_view("LEGACY_DELEGATES")
        .out_file("legacy_delegates.rs")
        .rerun_if_changed(true)
        .allow_missing_registry(docs_rs_build())
        .emit()
        .expect("freenet-migrate-build: generate legacy delegates");

    // This registry is append-only and can never legitimately be empty: River
    // re-keys its delegate regularly, so predecessors always exist and must be
    // retained. An empty `LEGACY_DELEGATES` is silent total migration failure —
    // the startup sweep asks no legacy delegate at all — so fail the build
    // rather than ship it, whatever produced it (`freenet-migrate-build`
    // accepts an entry-less TOML as a valid empty registry). Asserted on the
    // GENERATED output, i.e. what actually ships, not on the input file.
    if !docs_rs_build() {
        let generated = std::fs::read_to_string(&dest)
            .expect("read generated legacy_delegates.rs for the emptiness gate");
        let rows = generated.matches("\n    (").count();
        assert!(
            rows > 0,
            "generated LEGACY_DELEGATES has ZERO entries. The migration table \
             would ship EMPTY, the startup sweep would ask no legacy delegate, \
             and every returning user's rooms would be unrecoverable. Check \
             ../legacy_delegates.toml — this registry is append-only and must \
             never be empty."
        );
    }
}
