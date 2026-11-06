//! Finding, downloading and authenticating a Freenet release.
//!
//! This mirrors `freenet update` in freenet-core (`crates/core/src/bin/commands/update.rs`) with
//! one deliberate difference: a release with no `SHA256SUMS.txt.sig` is REFUSED. `freenet update`
//! still tolerates a missing signature for old releases; an installer has no such releases to
//! support, and every current release publishes one.

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

/// How often download progress reaches the UI.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

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

fn parse_manifest(manifest: &[u8]) -> Result<Checksums, String> {
    let text =
        std::str::from_utf8(manifest).map_err(|_| "checksum file is not UTF-8".to_string())?;
    let mut sums = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (hash, name) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("bad checksum line: {line}"))?;
        let name = name.trim_start().trim_start_matches('*');
        let mut digest = [0u8; 32];
        hex::decode_to_slice(hash, &mut digest).map_err(|_| format!("bad checksum for {name}"))?;
        sums.insert(name.to_string(), digest);
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

/// The release asset that installs Freenet on this platform, for release `version` (no `v`).
pub fn asset_name(version: &str) -> Result<String, String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => return Err(format!("Freenet has no release for {other} processors")),
    };
    match std::env::consts::OS {
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
}

impl Release {
    /// The latest release's tag, read from the `releases/latest` redirect so it needs no
    /// GitHub API quota (the same approach as freenet-core's auto-update).
    pub fn latest() -> Result<Release, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .redirects(0)
            .build();
        let resp = agent
            .get(&format!("{REPO}/releases/latest"))
            .call()
            .map_err(|e| format!("couldn't reach GitHub: {e}"))?;
        let location = resp
            .header("location")
            .ok_or("GitHub didn't say which release is latest")?;
        let tag = location
            .rsplit_once("/releases/tag/")
            .map(|(_, t)| t.to_string())
            .ok_or("unexpected latest-release redirect")?;
        if !tag.starts_with('v') || tag.contains('/') {
            return Err(format!("unexpected release tag {tag:?}"));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(60))
            .build();
        Ok(Release { agent, tag })
    }

    pub fn version(&self) -> &str {
        self.tag.trim_start_matches('v')
    }

    pub fn checksums(&self) -> Result<Checksums, String> {
        let manifest = self.fetch("SHA256SUMS.txt", &mut |_, _| {})?;
        let signature = self.fetch("SHA256SUMS.txt.sig", &mut |_, _| {})?;
        verify_manifest(&manifest, &signature)
    }

    /// Download one asset into memory, reporting `(received, total)` at most every 100 ms.
    pub fn fetch(
        &self,
        asset: &str,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        let url = format!("{REPO}/releases/download/{}/{asset}", self.tag);
        let resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| format!("couldn't download {asset}: {e}"))?;
        let total = resp.header("content-length").and_then(|v| v.parse().ok());
        let mut reader = resp.into_reader();
        let mut bytes = Vec::with_capacity(total.unwrap_or(0).min(256 << 20) as usize);
        let mut buf = [0u8; 64 * 1024];
        let mut last = Instant::now();
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| format!("download of {asset} was interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            if last.elapsed() >= PROGRESS_EVERY {
                progress(bytes.len() as u64, total);
                last = Instant::now();
            }
        }
        progress(bytes.len() as u64, total);
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &[u8] = include_bytes!("../tests/fixtures/SHA256SUMS-v0.2.155.txt");
    const SIGNATURE: &[u8] = include_bytes!("../tests/fixtures/SHA256SUMS-v0.2.155.txt.sig");

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
        assert!(sums.get("Freenet-0.2.155.dmg").is_ok());
    }

    #[test]
    fn a_tampered_manifest_is_refused() {
        let mut tampered = MANIFEST.to_vec();
        tampered[0] ^= 1;
        assert!(verify_manifest(&tampered, SIGNATURE).is_err());
    }

    #[test]
    fn a_wrong_key_or_truncated_signature_is_refused() {
        assert!(verify_manifest_with(MANIFEST, SIGNATURE, &[7u8; 32]).is_err());
        assert!(verify_manifest(MANIFEST, &SIGNATURE[..63]).is_err());
        assert!(verify_manifest(MANIFEST, &[]).is_err());
    }

    #[test]
    fn an_asset_must_match_its_checksum_and_be_listed() {
        let sums = parse_manifest(
            format!("{}  hello.txt\n", hex::encode(Sha256::digest(b"hello"))).as_bytes(),
        )
        .unwrap();
        assert!(verify_asset(&sums, "hello.txt", b"hello").is_ok());
        assert!(verify_asset(&sums, "hello.txt", b"hellp").is_err());
        assert!(verify_asset(&sums, "other.txt", b"hello").is_err());
    }

    #[test]
    fn this_platform_has_an_asset_in_the_release() {
        let sums = verify_manifest(MANIFEST, SIGNATURE).unwrap();
        let asset = asset_name("0.2.155").unwrap();
        assert!(sums.get(&asset).is_ok(), "{asset} should be in v0.2.155");
    }
}
