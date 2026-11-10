//! Gives `river.exe` its icon and version information on Windows. Every other target skips
//! this, so Linux and macOS builds are unaffected (macOS gets its icon and version from the
//! `.app` bundle, see `packaging/package-macos.sh`).

fn main() {
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
