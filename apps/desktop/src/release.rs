//! Finding, downloading and authenticating a Freenet release.
//!
//! This follows `freenet update` in freenet-core (`crates/core/src/bin/commands/update.rs`), with
//! two deliberate differences:
//!
//! - A release with no `SHA256SUMS.txt.sig` is REFUSED. `freenet update` still tolerates a
//!   missing signature for old releases; an installer has none to support.
//! - There is no running version to compare against, so a compiled-in [`MIN_VERSION`] stops
//!   anyone who can edit GitHub releases (but not sign them) from steering a new user to an
//!   old signed release.
//!
//! freenet-core also reserves an offline revocation key. Nothing checks a revocation yet, here
//! or in `freenet update`.

use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

const REPO: &str = "https://github.com/freenet/freenet-core";

/// Freenet's release-signing key, copied from `FREENET_RELEASE_PUBKEY` in freenet-core's
/// `update.rs`. Hex: `eb2986604e34a3f985279a7e70852f3764d3347fe82074d92b1e4bc6336f8664`.
const RELEASE_PUBKEY: [u8; 32] = [
    0xeb, 0x29, 0x86, 0x60, 0x4e, 0x34, 0xa3, 0xf9, 0x85, 0x27, 0x9a, 0x7e, 0x70, 0x85, 0x2f, 0x37,
    0x64, 0xd3, 0x34, 0x7f, 0xe8, 0x20, 0x74, 0xd9, 0x2b, 0x1e, 0x4b, 0xc6, 0x33, 0x6f, 0x86, 0x64,
];

/// The oldest Freenet this installer will install. Raise it when River needs a newer node.
pub const MIN_VERSION: Version = Version(0, 2, 155);

/// How often download progress reaches the UI.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Size limits. The manifest and signature are read before they are authenticated, so they
/// are capped tightly; the largest asset (the macOS DMG) is about 85 MB.
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_SIGNATURE: u64 = 1024;
const MAX_ASSET: u64 = 512 * 1024 * 1024;

/// Give up on a download that hasn't finished in this long, however slowly bytes trickle in.
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(20 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Parse a release tag. Only `vMAJOR.MINOR.PATCH` is accepted, so a tag can't carry anything
/// into the download URL.
pub fn parse_tag(tag: &str) -> Result<Version, String> {
    let bad = || format!("unexpected release tag {tag:?}");
    let mut parts = tag.strip_prefix('v').ok_or_else(bad)?.split('.');
    let mut next = || -> Result<u64, String> {
        let part = parts.next().ok_or_else(bad)?;
        if part.is_empty() || part.len() > 9 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        part.parse().map_err(|_| bad())
    };
    let version = Version(next()?, next()?, next()?);
    if parts.next().is_some() {
        return Err(bad());
    }
    Ok(version)
}

/// The tag from the `Location` of a `releases/latest` redirect.
fn tag_from_latest_redirect(location: &str) -> Result<(String, Version), String> {
    let tag = location
        .rsplit_once("/releases/tag/")
        .map(|(_, t)| t)
        .ok_or("unexpected latest-release redirect")?;
    let version = parse_tag(tag)?;
    if version < MIN_VERSION {
        return Err(format!(
            "the latest release is {version}, older than {MIN_VERSION}, the oldest this \
             version of River will install"
        ));
    }
    Ok((tag.to_string(), version))
}

/// Asset name -> SHA-256, from a manifest whose signature has been checked.
pub struct Checksums(HashMap<String, [u8; 32]>);

impl Checksums {
    pub fn get(&self, asset: &str) -> Result<[u8; 32], String> {
        self.0
            .get(asset)
            .copied()
            .ok_or_else(|| format!("{asset} is not listed in the release's signed checksums"))
    }
}

/// Check `manifest` against `signature` with the release key, then parse it.
/// The signature covers the raw bytes, so nothing is parsed until it verifies.
pub fn verify_manifest(manifest: &[u8], signature: &[u8]) -> Result<Checksums, String> {
    verify_manifest_with(manifest, signature, &RELEASE_PUBKEY)
}

fn verify_manifest_with(
    manifest: &[u8],
    signature: &[u8],
    pubkey: &[u8; 32],
) -> Result<Checksums, String> {
    let signature: [u8; 64] = signature.try_into().map_err(|_| {
        format!(
            "release signature is {} bytes, expected 64",
            signature.len()
        )
    })?;
    let key = VerifyingKey::from_bytes(pubkey).map_err(|e| format!("bad release key: {e}"))?;
    key.verify_strict(manifest, &Signature::from_bytes(&signature))
        .map_err(|_| {
            "the release's checksum file is not signed by Freenet's release key".to_string()
        })?;
    parse_manifest(manifest)
}

/// Check that a verified manifest belongs to release `version`.
///
/// The signature binds asset names to hashes, but most asset names carry no version, so an
/// older signed manifest would also verify. Every release lists its versioned DMG, which ties
/// the manifest to its own tag.
pub fn check_manifest_version(checksums: &Checksums, version: Version) -> Result<(), String> {
    checksums
        .get(&format!("Freenet-{version}.dmg"))
        .map(|_| ())
        .map_err(|_| format!("the signed checksums are not for release {version}"))
}

fn parse_manifest(manifest: &[u8]) -> Result<Checksums, String> {
    let text =
        std::str::from_utf8(manifest).map_err(|_| "checksum file is not UTF-8".to_string())?;
    let mut sums = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (hash, name) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("bad checksum line: {line}"))?;
        let name = name.trim().trim_start_matches('*');
        let mut digest = [0u8; 32];
        hex::decode_to_slice(hash, &mut digest).map_err(|_| format!("bad checksum for {name}"))?;
        if sums.insert(name.to_string(), digest).is_some() {
            return Err(format!("{name} is listed twice in the checksum file"));
        }
    }
    Ok(Checksums(sums))
}

/// Check `bytes` against the signed manifest's entry for `asset`.
pub fn verify_asset(checksums: &Checksums, asset: &str, bytes: &[u8]) -> Result<(), String> {
    let expected = checksums.get(asset)?;
    let actual: [u8; 32] = Sha256::digest(bytes).into();
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{asset} doesn't match its signed checksum"))
    }
}

/// The release asset that installs Freenet on this platform.
pub fn asset_name(version: Version) -> Result<String, String> {
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH, version)
}

fn asset_name_for(os: &str, arch: &str, version: Version) -> Result<String, String> {
    let arch = match arch {
        "x86_64" | "aarch64" => arch,
        other => return Err(format!("Freenet has no release for {other} processors")),
    };
    match os {
        "linux" => Ok(format!("freenet-{arch}-unknown-linux-musl.tar.gz")),
        // The DMG is universal, signed and notarized, so Gatekeeper accepts it.
        "macos" => Ok(format!("Freenet-{version}.dmg")),
        // The signed exe is its own installer: run with no arguments it opens a setup wizard.
        "windows" if arch == "x86_64" => Ok("freenet.exe".to_string()),
        os => Err(format!("Freenet has no release for {os} on {arch}")),
    }
}

pub struct Release {
    agent: ureq::Agent,
    pub tag: String,
    pub version: Version,
}

impl Release {
    /// The latest release, read from the `releases/latest` redirect so it needs no GitHub API
    /// quota (the same approach as freenet-core's auto-update).
    pub fn latest() -> Result<Release, String> {
        let probe = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .redirects(0)
            .build();
        let resp = probe
            .get(&format!("{REPO}/releases/latest"))
            .call()
            .map_err(|e| format!("couldn't reach GitHub: {e}"))?;
        let location = resp
            .header("location")
            .ok_or("GitHub didn't say which release is latest")?;
        let (tag, version) = tag_from_latest_redirect(location)?;
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(60))
            .build();
        Ok(Release {
            agent,
            tag,
            version,
        })
    }

    /// The release's signed checksums, checked to belong to this release.
    pub fn checksums(&self) -> Result<Checksums, String> {
        let manifest = self.fetch_limited("SHA256SUMS.txt", MAX_MANIFEST, &mut |_, _| {})?;
        let signature = self.fetch_limited("SHA256SUMS.txt.sig", MAX_SIGNATURE, &mut |_, _| {})?;
        let checksums = verify_manifest(&manifest, &signature)?;
        check_manifest_version(&checksums, self.version)?;
        Ok(checksums)
    }

    /// Download one installable asset into memory, reporting `(received, total)` as it goes.
    pub fn fetch(
        &self,
        asset: &str,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        self.fetch_limited(asset, MAX_ASSET, progress)
    }

    fn fetch_limited(
        &self,
        asset: &str,
        max: u64,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        let url = format!("{REPO}/releases/download/{}/{asset}", self.tag);
        let resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| format!("couldn't download {asset}: {e}"))?;
        let total = resp.header("content-length").and_then(|v| v.parse().ok());
        read_with_progress(
            resp.into_reader(),
            total,
            max,
            Instant::now() + DOWNLOAD_DEADLINE,
            PROGRESS_EVERY,
            progress,
        )
        .map_err(|e| format!("download of {asset} failed: {e}"))
    }
}

/// Read `reader` to the end, refusing more than `max` bytes or running past `deadline`, and
/// reporting progress at most every `every` plus once at the end.
fn read_with_progress(
    mut reader: impl Read,
    total: Option<u64>,
    max: u64,
    deadline: Instant,
    every: Duration,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, String> {
    if total.is_some_and(|t| t > max) {
        return Err(format!("it is larger than the {max}-byte limit"));
    }
    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut buf = [0u8; 64 * 1024];
    let mut last = Instant::now();
    loop {
        if Instant::now() > deadline {
            return Err("it took too long".into());
        }
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if bytes.len() as u64 + n as u64 > max {
            return Err(format!("it is larger than the {max}-byte limit"));
        }
        bytes.extend_from_slice(&buf[..n]);
        if last.elapsed() >= every {
            progress(bytes.len() as u64, total);
            last = Instant::now();
        }
    }
    progress(bytes.len() as u64, total);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const MANIFEST: &[u8] = include_bytes!("../tests/fixtures/SHA256SUMS-v0.2.155.txt");
    const SIGNATURE: &[u8] = include_bytes!("../tests/fixtures/SHA256SUMS-v0.2.155.txt.sig");
    const V155: Version = Version(0, 2, 155);

    #[test]
    fn a_real_signed_release_manifest_verifies() {
        let sums = verify_manifest(MANIFEST, SIGNATURE)
            .expect("v0.2.155's manifest is signed by the release key");
        let linux = sums
            .get("freenet-x86_64-unknown-linux-musl.tar.gz")
            .unwrap();
        assert_eq!(
            hex::encode(linux),
            "5090e2df844a1296b42e337b1ae7d6a63cef92881b5462a9611b3d52643c4aee"
        );
        assert!(sums.get("freenet.exe").is_ok());
        check_manifest_version(&sums, V155).unwrap();
        assert!(check_manifest_version(&sums, Version(0, 2, 156)).is_err());
    }

    #[test]
    fn a_tampered_manifest_is_refused() {
        let mut tampered = MANIFEST.to_vec();
        tampered[0] ^= 1;
        assert!(verify_manifest(&tampered, SIGNATURE).is_err());
    }

    #[test]
    fn a_manifest_signed_by_another_key_is_refused() {
        let other = SigningKey::from_bytes(&[1u8; 32]);
        let signature = other.sign(MANIFEST).to_bytes();
        // Sanity: the signature is valid for its own key, so only the key is wrong.
        assert!(
            verify_manifest_with(MANIFEST, &signature, &other.verifying_key().to_bytes()).is_ok()
        );
        assert_eq!(
            verify_manifest(MANIFEST, &signature).err().as_deref(),
            Some("the release's checksum file is not signed by Freenet's release key")
        );
    }

    #[test]
    fn a_truncated_or_missing_signature_is_refused() {
        assert!(verify_manifest(MANIFEST, &SIGNATURE[..63]).is_err());
        assert!(verify_manifest(MANIFEST, &[]).is_err());
    }

    #[test]
    fn an_asset_must_match_its_checksum_and_be_listed() {
        let line = format!("{}  hello.txt\n", hex::encode(Sha256::digest(b"hello")));
        let sums = parse_manifest(line.as_bytes()).unwrap();
        assert!(verify_asset(&sums, "hello.txt", b"hello").is_ok());
        assert!(verify_asset(&sums, "hello.txt", b"hellp").is_err());
        assert!(verify_asset(&sums, "other.txt", b"hello").is_err());
    }

    #[test]
    fn manifest_parsing_handles_the_formats_and_refuses_the_rest() {
        let h = hex::encode([7u8; 32]);
        let sums =
            parse_manifest(format!("{h} *binary.bin\r\n\n{h}  text.txt\n").as_bytes()).unwrap();
        assert!(sums.get("binary.bin").is_ok());
        assert!(sums.get("text.txt").is_ok());
        assert!(parse_manifest(b"not-a-line").is_err());
        assert!(parse_manifest(format!("{}  short.bin", &h[..10]).as_bytes()).is_err());
        assert!(parse_manifest(format!("{h}  a\n{h}  a\n").as_bytes()).is_err());
    }

    #[test]
    fn every_platform_maps_to_an_asset_in_the_release() {
        let sums = verify_manifest(MANIFEST, SIGNATURE).unwrap();
        for (os, arch, expected) in [
            (
                "linux",
                "x86_64",
                "freenet-x86_64-unknown-linux-musl.tar.gz",
            ),
            (
                "linux",
                "aarch64",
                "freenet-aarch64-unknown-linux-musl.tar.gz",
            ),
            ("macos", "x86_64", "Freenet-0.2.155.dmg"),
            ("macos", "aarch64", "Freenet-0.2.155.dmg"),
            ("windows", "x86_64", "freenet.exe"),
        ] {
            let asset = asset_name_for(os, arch, V155).unwrap();
            assert_eq!(asset, expected);
            assert!(sums.get(&asset).is_ok(), "{asset} should be in v0.2.155");
        }
        assert!(asset_name_for("windows", "aarch64", V155).is_err());
        assert!(asset_name_for("linux", "riscv64", V155).is_err());
        assert!(asset_name_for("freebsd", "x86_64", V155).is_err());
        assert!(
            asset_name(V155).is_ok(),
            "the platform running these tests is supported"
        );
    }

    #[test]
    fn only_plain_version_tags_are_accepted() {
        assert_eq!(parse_tag("v0.2.155"), Ok(V155));
        for bad in [
            "0.2.155",
            "v0.2",
            "v0.2.155.1",
            "v0.2.x",
            "v0..155",
            "v0.2.155/../../x",
            r"v\..\..\attacker\repo",
            "v0.2.155?x",
            "v0.2.155#x",
            "v+1.2.3",
            "v1.2.%2e",
        ] {
            assert!(parse_tag(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn the_latest_redirect_must_name_a_new_enough_release() {
        let base = "https://github.com/freenet/freenet-core/releases/tag/";
        assert_eq!(
            tag_from_latest_redirect(&format!("{base}v0.2.155")),
            Ok(("v0.2.155".into(), V155))
        );
        assert!(tag_from_latest_redirect(&format!("{base}v0.3.0")).is_ok());
        assert!(tag_from_latest_redirect(&format!("{base}v0.2.154")).is_err());
        assert!(tag_from_latest_redirect("https://github.com/login").is_err());
        assert!(tag_from_latest_redirect(&format!(r"{base}v\..\x")).is_err());
    }

    #[test]
    fn downloads_report_progress_and_stop_at_their_limits() {
        let data = vec![9u8; 300_000];
        let mut reports = Vec::new();
        let far = Instant::now() + Duration::from_secs(60);
        let got = read_with_progress(
            &data[..],
            Some(300_000),
            1 << 20,
            far,
            Duration::ZERO,
            &mut |n, t| reports.push((n, t)),
        )
        .unwrap();
        assert_eq!(got, data);
        assert!(reports.len() > 1, "progress while downloading: {reports:?}");
        assert_eq!(reports.last(), Some(&(300_000, Some(300_000))));

        // Throttled: with a long interval only the final report is made.
        let mut count = 0;
        read_with_progress(
            &data[..],
            None,
            1 << 20,
            far,
            Duration::from_secs(3600),
            &mut |_, _| count += 1,
        )
        .unwrap();
        assert_eq!(count, 1);

        // Too big, whether announced or not.
        assert!(
            read_with_progress(
                &data[..],
                Some(300_000),
                1000,
                far,
                Duration::ZERO,
                &mut |_, _| {}
            )
            .is_err()
        );
        assert!(
            read_with_progress(&data[..], None, 1000, far, Duration::ZERO, &mut |_, _| {}).is_err()
        );

        // Past the deadline.
        let past = Instant::now() - Duration::from_secs(1);
        assert!(
            read_with_progress(
                &data[..],
                None,
                1 << 20,
                past,
                Duration::ZERO,
                &mut |_, _| {}
            )
            .is_err()
        );
    }
}
