//! Gives `river.exe` its icon and version information on Windows. Every other target skips
//! this, so Linux and macOS builds are unaffected (macOS gets its icon and version from the
//! `.app` bundle, see `packaging/package-macos.sh`).

/// The update settings read from the environment at build time are checked here, so a typo
/// fails the build instead of becoming "no key" (a build that silently cannot update).
fn check_update_settings() {
    for var in [
        "RIVER_UPDATE_PUBKEY_HEX",
        "RIVER_UPDATE_MIN_VERSION",
        "RIVER_UPDATE_MIN_SEQUENCE",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    // An empty value is unset: an undefined `${{ vars.X }}` in a workflow expands to "".
    let set = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    if let Some(key) = set("RIVER_UPDATE_PUBKEY_HEX") {
        let valid = key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            panic!(
                "RIVER_UPDATE_PUBKEY_HEX is set but is not 64 hex digits (it has {} characters); \
                 refusing to build a River that cannot verify updates",
                key.len()
            );
        }
        if key.bytes().all(|b| b == b'0') {
            panic!("RIVER_UPDATE_PUBKEY_HEX is all zeros, which is the placeholder key");
        }
    }
    if let Some(v) = set("RIVER_UPDATE_MIN_VERSION") {
        let parts: Vec<&str> = v.split('.').collect();
        if parts.len() != 3
            || !parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            panic!("RIVER_UPDATE_MIN_VERSION must look like 0.1.0, got {v:?}");
        }
    }
    if let Some(n) = set("RIVER_UPDATE_MIN_SEQUENCE")
        && n.parse::<u64>().is_err()
    {
        panic!("RIVER_UPDATE_MIN_SEQUENCE must be a number, got {n:?}");
    }
}

fn main() {
    check_update_settings();
    println!("cargo:rerun-if-changed=packaging/icons/river.ico");
    #[cfg(windows)]
    windows_resource();
}

/// Embeds the icon and a VERSIONINFO resource. The version fields come from
/// `CARGO_PKG_VERSION` (the `[workspace.package]` version in `apps/Cargo.toml`), so they always
/// match `river --version` and the release tag.
#[cfg(windows)]
fn windows_resource() {
    // Build scripts run on the host, so check the target as well: this is a no-op if someone
    // ever cross-compiles a non-Windows target from Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("packaging/icons/river.ico")
        .set("ProductName", "River")
        .set("FileDescription", "River")
        .set("CompanyName", "Freenet Project Inc")
        .set(
            "LegalCopyright",
            "Freenet Project Inc. Licensed under LGPL-3.0-only.",
        )
        .set("OriginalFilename", "river.exe")
        .set("InternalName", "river");
    // A build that silently ships without its icon and version would only be noticed by a
    // user, so fail instead.
    res.compile()
        .expect("embedding the Windows icon and version resource failed");
}
