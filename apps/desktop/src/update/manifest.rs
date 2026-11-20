//! The signed update manifest: its format, its signature, and the rules that decide whether a
//! manifest that verifies is acceptable.
//!
//! # Format
//!
//! The manifest is one JSON object in a fixed, compact form: fields in the order of
//! [`Manifest`], artifact names sorted, no whitespace. `manifest.json.sig` next to it holds
//! the raw 64-byte ed25519 signature over [`SIGN_DOMAIN`] followed by the manifest's exact
//! bytes.
//!
//! The verifier checks the signature over the received bytes first, parses second, and then
//! requires that serializing what it parsed gives back exactly the bytes it received. So a
//! manifest has one spelling: duplicate keys, extra fields, reordered keys, odd whitespace or
//! escapes are all refused, and nothing is trusted that the signature did not cover. The signer
//! (`examples/update_tool.rs`) writes the same form by serializing a [`Manifest`].
//!
//! # Rules a verified manifest must still pass
//!
//! A signature proves Ian's key signed those bytes at some time, not that they are current.
//! [`evaluate`] therefore also applies:
//!
//! - **Channel**: it must equal this build's channel.
//! - **Expiry**: a manifest past its `expires_at` is refused, so an attacker who can only
//!   replay old signed files cannot hold users on an old version forever, nor hide a newer
//!   one. A `released_at` far in the future is refused too (a bad clock cannot make an old
//!   manifest look young, but cannot be told apart from a forged date either).
//! - **Replay**: `sequence` must be at least the highest one this install has accepted. The
//!   signing tool sets it to the unix time of signing, so it rises with every signing. The
//!   same sequence is accepted again (re-fetching the current manifest). A sequence more than a
//!   day above the clock is refused, so a mis-signed huge value cannot wedge every client.
//! - **Build floors**: a build can carry a minimum version and sequence
//!   (`RIVER_UPDATE_MIN_VERSION`, `RIVER_UPDATE_MIN_SEQUENCE`) that start the two floors above
//!   zero, so a fresh install does not accept an old, unexpired manifest.
//! - **Rollback**: `version` must be at least the highest version this install has accepted.
//! - **Downgrade**: a manifest whose version is not above the running one offers nothing.
//! - **Platform**: it must carry an artifact for this platform.
//!
//! The highest accepted sequence and version live in the update state file
//! ([`super::state`]); they only ever rise.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::release::{Version, parse_version};

/// Prefixed to the manifest bytes before signing, so a signature made for this purpose cannot
/// be a valid signature on anything else River's key might sign (and vice versa).
pub const SIGN_DOMAIN: &[u8] = b"river-update-manifest-v1\n";

/// Largest manifest accepted. It is read before it is authenticated.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// The signature is exactly this long.
pub const SIGNATURE_BYTES: usize = 64;
/// Largest artifact a manifest may announce.
pub const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
/// How far ahead of the clock `released_at` may be (clock skew).
pub const MAX_FUTURE_SKEW_SECS: u64 = 24 * 60 * 60;
/// PLACEHOLDER update-signing public key: all zero bytes. That is not "no key": it decodes to
/// a valid point of small order, and a signature that verifies under such a key can be forged.
/// What protects a build that still has it is [`verify_and_parse`] refusing it outright
/// (and `verify_strict`, which rejects small-order keys, behind that). Ian generates the real
/// pair (`update_tool keygen`).
pub const PLACEHOLDER_PUBKEY: [u8; 32] = [0; 32];

/// Whether `key` is the placeholder.
pub fn is_placeholder(key: &[u8; 32]) -> bool {
    *key == PLACEHOLDER_PUBKEY
}

/// The only format this build reads.
pub const FORMAT: u32 = 1;

/// One downloadable build for one platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub url: String,
    /// Lowercase hex SHA-256 of the artifact file.
    pub sha256: String,
    pub size: u64,
}

/// The signed manifest. Field order is the serialized order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub channel: String,
    /// `MAJOR.MINOR.PATCH` of the newest release.
    pub version: String,
    /// Rises with every signing (see the module comment).
    pub sequence: u64,
    /// Unix seconds.
    pub released_at: u64,
    /// Unix seconds; the manifest is refused after this.
    pub expires_at: u64,
    /// Builds older than this are told the update is required (they are never forced).
    pub min_supported: String,
    /// Plain-text release notes, shown to the user.
    pub notes: String,
    /// Platform key (`linux-x86_64`, ...) to build.
    pub artifacts: BTreeMap<String, Artifact>,
}

impl Manifest {
    /// The canonical bytes: what gets signed and served.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a Manifest always serializes")
    }
}

/// The bytes the signature covers.
pub fn signed_bytes(manifest: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(SIGN_DOMAIN.len() + manifest.len());
    bytes.extend_from_slice(SIGN_DOMAIN);
    bytes.extend_from_slice(manifest);
    bytes
}

/// Why a manifest was refused. `Display` is for logs and the settings screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    TooLarge,
    BadSignature,
    BadKey,
    Malformed(String),
    /// Verified, but not in canonical form.
    NotCanonical,
    WrongChannel {
        got: String,
    },
    Expired,
    FromTheFuture,
    /// The sequence is the unix time of signing, so one far above the clock is a mistake (or
    /// an attack) that would otherwise make every client refuse all real manifests.
    SequenceFromTheFuture,
    Replay {
        sequence: u64,
        highest: u64,
    },
    Rollback {
        version: Version,
        highest: Version,
    },
    NoArtifactForPlatform(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::TooLarge => write!(f, "the update manifest is too large"),
            Refusal::BadSignature => write!(f, "the update manifest's signature is not valid"),
            Refusal::BadKey => write!(f, "this build has no valid update key"),
            Refusal::Malformed(why) => write!(f, "the update manifest is malformed: {why}"),
            Refusal::NotCanonical => write!(f, "the update manifest is not in canonical form"),
            Refusal::WrongChannel { got } => {
                write!(f, "the update manifest is for channel {got:?}")
            }
            Refusal::Expired => write!(
                f,
                "the update manifest has expired (River's publisher has not signed a new one \
                 recently)"
            ),
            Refusal::SequenceFromTheFuture => {
                write!(
                    f,
                    "the update manifest's sequence number is far ahead of the clock"
                )
            }
            Refusal::FromTheFuture => write!(f, "the update manifest is dated in the future"),
            Refusal::Replay { sequence, highest } => write!(
                f,
                "the update manifest (sequence {sequence}) is older than one already seen \
                 ({highest})"
            ),
            Refusal::Rollback { version, highest } => write!(
                f,
                "the update manifest names {version}, older than {highest}, which was already \
                 offered"
            ),
            Refusal::NoArtifactForPlatform(p) => {
                write!(f, "the update manifest has no build for {p}")
            }
        }
    }
}

/// Check the signature over `manifest`, then parse it and require canonical form.
/// Nothing is parsed before the signature verifies.
pub fn verify_and_parse(
    manifest: &[u8],
    signature: &[u8],
    pubkey: &[u8; 32],
) -> Result<Manifest, Refusal> {
    if is_placeholder(pubkey) {
        return Err(Refusal::BadKey);
    }
    if manifest.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(Refusal::TooLarge);
    }
    let signature: [u8; SIGNATURE_BYTES] =
        signature.try_into().map_err(|_| Refusal::BadSignature)?;
    let key = VerifyingKey::from_bytes(pubkey).map_err(|_| Refusal::BadKey)?;
    key.verify_strict(&signed_bytes(manifest), &Signature::from_bytes(&signature))
        .map_err(|_| Refusal::BadSignature)?;
    let parsed: Manifest =
        serde_json::from_slice(manifest).map_err(|e| Refusal::Malformed(e.to_string()))?;
    if parsed.canonical_bytes() != manifest {
        return Err(Refusal::NotCanonical);
    }
    validate(&parsed)?;
    Ok(parsed)
}

/// Field-level checks that hold for every manifest, whatever the running build.
fn validate(m: &Manifest) -> Result<(), Refusal> {
    let bad = |why: &str| Refusal::Malformed(why.to_string());
    if m.format != FORMAT {
        return Err(bad("unknown format"));
    }
    if m.channel.is_empty()
        || m.channel.len() > 32
        || !m.channel.bytes().all(|b| b.is_ascii_lowercase())
    {
        return Err(bad("bad channel"));
    }
    parse_version(&m.version).map_err(|e| bad(&e))?;
    parse_version(&m.min_supported).map_err(|e| bad(&e))?;
    if m.expires_at <= m.released_at {
        return Err(bad("expires_at is not after released_at"));
    }
    if m.notes.len() > 8 * 1024 {
        return Err(bad("notes are too long"));
    }
    if m.artifacts.is_empty() || m.artifacts.len() > 16 {
        return Err(bad("wrong number of artifacts"));
    }
    for (name, a) in &m.artifacts {
        if name.is_empty()
            || name.len() > 32
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(bad("bad artifact name"));
        }
        if a.url.len() > 2048 || !a.url.is_ascii() || a.url.bytes().any(|b| b <= b' ' || b == 0x7f)
        {
            return Err(bad("bad artifact url"));
        }
        if a.sha256.len() != 64
            || !a
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(bad("bad artifact sha256"));
        }
        if a.size == 0 || a.size > MAX_ARTIFACT_BYTES {
            return Err(bad("bad artifact size"));
        }
    }
    Ok(())
}

/// What this install has already accepted. Both only rise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trust {
    pub highest_sequence: u64,
    pub highest_version: Option<Version>,
}

impl Trust {
    /// The state after accepting `m`.
    pub fn accepting(self, m: &Manifest) -> Trust {
        let version = parse_version(&m.version).ok();
        Trust {
            highest_sequence: self.highest_sequence.max(m.sequence),
            highest_version: self.highest_version.max(version),
        }
    }
}

/// An update the manifest offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub version: Version,
    pub notes: String,
    pub artifact: Artifact,
    /// The running build is older than `min_supported`.
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The manifest is current and names nothing newer than what runs.
    UpToDate,
    Available(Offer),
}

/// What a verified manifest means for the running build. Pure: the clock and the trust state
/// come in as arguments.
pub fn evaluate(
    m: &Manifest,
    running: Version,
    channel: &str,
    platform: &str,
    now: u64,
    trust: Trust,
) -> Result<Verdict, Refusal> {
    if m.channel != channel {
        return Err(Refusal::WrongChannel {
            got: m.channel.clone(),
        });
    }
    if now > m.expires_at {
        return Err(Refusal::Expired);
    }
    if m.released_at > now.saturating_add(MAX_FUTURE_SKEW_SECS) {
        return Err(Refusal::FromTheFuture);
    }
    if m.sequence > now.saturating_add(MAX_FUTURE_SKEW_SECS) {
        return Err(Refusal::SequenceFromTheFuture);
    }
    if m.sequence < trust.highest_sequence {
        return Err(Refusal::Replay {
            sequence: m.sequence,
            highest: trust.highest_sequence,
        });
    }
    let version = parse_version(&m.version).map_err(Refusal::Malformed)?;
    if let Some(highest) = trust.highest_version
        && version < highest
    {
        return Err(Refusal::Rollback { version, highest });
    }
    if version <= running {
        return Ok(Verdict::UpToDate);
    }
    let artifact = m
        .artifacts
        .get(platform)
        .ok_or_else(|| Refusal::NoArtifactForPlatform(platform.to_string()))?
        .clone();
    let min = parse_version(&m.min_supported).map_err(Refusal::Malformed)?;
    Ok(Verdict::Available(Offer {
        version,
        notes: m.notes.clone(),
        artifact,
        required: running < min,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub const NOW: u64 = 1_800_000_000;

    pub fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub fn pubkey(seed: u8) -> [u8; 32] {
        key(seed).verifying_key().to_bytes()
    }

    pub fn manifest(version: &str, sequence: u64) -> Manifest {
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "linux-x86_64".to_string(),
            Artifact {
                url: "https://example.org/river.tar.gz".into(),
                sha256: "ab".repeat(32),
                size: 1000,
            },
        );
        Manifest {
            format: FORMAT,
            channel: "stable".into(),
            version: version.into(),
            sequence,
            released_at: NOW - 100,
            expires_at: NOW + 100_000,
            min_supported: "0.1.0".into(),
            notes: "notes".into(),
            artifacts,
        }
    }

    pub fn sign(seed: u8, bytes: &[u8]) -> Vec<u8> {
        key(seed).sign(&signed_bytes(bytes)).to_bytes().to_vec()
    }

    pub fn signed(m: &Manifest) -> (Vec<u8>, Vec<u8>) {
        let bytes = m.canonical_bytes();
        let sig = sign(1, &bytes);
        (bytes, sig)
    }

    fn v(s: &str) -> Version {
        parse_version(s).unwrap()
    }

    #[test]
    fn a_good_manifest_verifies_and_round_trips() {
        let m = manifest("0.2.0", 5);
        let (bytes, sig) = signed(&m);
        assert_eq!(verify_and_parse(&bytes, &sig, &pubkey(1)).unwrap(), m);
    }

    #[test]
    fn hostile_signatures_are_refused() {
        let m = manifest("0.2.0", 5);
        let (bytes, sig) = signed(&m);
        let mut flipped = sig.clone();
        flipped[0] ^= 1;
        let mut tampered = bytes.clone();
        let at = tampered.iter().position(|&b| b == b'5').unwrap();
        tampered[at] = b'6';
        // Signed over the bytes without the domain prefix.
        let bare = key(1).sign(&bytes).to_bytes().to_vec();
        type Case = (&'static str, Vec<u8>, Vec<u8>, [u8; 32]);
        let cases: Vec<Case> = vec![
            ("empty signature", bytes.clone(), vec![], pubkey(1)),
            (
                "short signature",
                bytes.clone(),
                sig[..63].to_vec(),
                pubkey(1),
            ),
            (
                "long signature",
                bytes.clone(),
                [sig.clone(), vec![0]].concat(),
                pubkey(1),
            ),
            ("flipped bit", bytes.clone(), flipped, pubkey(1)),
            ("tampered manifest", tampered, sig.clone(), pubkey(1)),
            ("other key", bytes.clone(), sig.clone(), pubkey(2)),
            ("no domain separation", bytes.clone(), bare, pubkey(1)),
            ("unsigned", bytes.clone(), vec![0; 64], pubkey(1)),
        ];
        for (name, bytes, sig, key) in cases {
            assert_eq!(
                verify_and_parse(&bytes, &sig, &key),
                Err(Refusal::BadSignature),
                "{name}"
            );
        }
    }

    /// Small-order public keys (the all-zero placeholder, the identity point, others) with the
    /// signatures that would verify under a lax check: R the identity, S zero, for many
    /// different manifests. Nothing may verify, and the placeholder is refused outright.
    #[test]
    fn small_order_keys_verify_nothing_for_any_manifest() {
        let identity = {
            let mut p = [0u8; 32];
            p[0] = 1;
            p
        };
        let minus_one = {
            let mut p = [0xffu8; 32];
            p[0] = 0xec;
            p[31] = 0x7f;
            p
        };
        let mut forged = identity.to_vec();
        forged.extend_from_slice(&[0u8; 32]);
        for n in 0..200u64 {
            let m = manifest(&format!("0.{}.{}", n % 7, n % 13), 5 + n);
            let (bytes, _) = signed(&m);
            for (name, key) in [
                ("placeholder", PLACEHOLDER_PUBKEY),
                ("identity", identity),
                ("minus one", minus_one),
            ] {
                for sig in [forged.clone(), vec![0u8; 64]] {
                    let r = verify_and_parse(&bytes, &sig, &key);
                    let want = if name == "placeholder" {
                        Refusal::BadKey
                    } else {
                        Refusal::BadSignature
                    };
                    assert_eq!(r, Err(want), "{name} #{n}");
                }
            }
        }
    }

    /// The signature is checked before anything is parsed: bytes that are not JSON with a bad
    /// signature are a bad signature, not a malformed manifest.
    #[test]
    fn the_signature_is_checked_before_parsing() {
        for garbage in [&b"not json at all"[..], b"{", b"\xff\xfe\x00", b""] {
            assert_eq!(
                verify_and_parse(garbage, &[7u8; 64], &pubkey(1)),
                Err(Refusal::BadSignature)
            );
        }
    }

    #[test]
    fn a_sequence_far_ahead_of_the_clock_is_refused() {
        let fresh = Trust::default();
        let ok = manifest("0.2.0", NOW + MAX_FUTURE_SKEW_SECS);
        assert!(eval(&ok, "0.1.0", NOW, fresh).is_ok());
        for sequence in [NOW + MAX_FUTURE_SKEW_SECS + 1, u64::MAX] {
            let bad = manifest("0.2.0", sequence);
            assert_eq!(
                eval(&bad, "0.1.0", NOW, fresh),
                Err(Refusal::SequenceFromTheFuture)
            );
        }
    }

    #[test]
    fn build_floors_refuse_older_manifests_on_a_fresh_install() {
        let floor = Trust {
            highest_sequence: 1000,
            highest_version: Some(v("0.3.0")),
        };
        let m = manifest("0.2.0", 900);
        assert!(matches!(
            eval(&m, "0.1.0", NOW, floor),
            Err(Refusal::Replay { .. })
        ));
        let m = manifest("0.2.0", 1000);
        assert!(matches!(
            eval(&m, "0.1.0", NOW, floor),
            Err(Refusal::Rollback { .. })
        ));
    }

    #[test]
    fn an_oversized_manifest_is_refused_before_anything_else() {
        let big = vec![b' '; MAX_MANIFEST_BYTES as usize + 1];
        assert_eq!(
            verify_and_parse(&big, &sign(1, &big), &pubkey(1)),
            Err(Refusal::TooLarge)
        );
    }

    /// Correctly signed, but not what the signer should have produced.
    #[test]
    fn signed_but_non_canonical_manifests_are_refused() {
        let m = manifest("0.2.0", 5);
        let canonical = String::from_utf8(m.canonical_bytes()).unwrap();
        let cases: Vec<(&str, String)> = vec![
            ("whitespace", format!(" {canonical}")),
            ("pretty", serde_json::to_string_pretty(&m).unwrap()),
            ("extra field", canonical.replacen('{', "{\"extra\":1,", 1)),
            (
                "duplicate key",
                canonical.replacen("\"format\":1", "\"format\":1,\"format\":1", 1),
            ),
            (
                "reordered",
                format!(
                    "{{\"channel\":\"stable\",{}",
                    canonical
                        .replacen("\"channel\":\"stable\",", "", 1)
                        .trim_start_matches('{')
                ),
            ),
            ("escaped", canonical.replacen("notes", "n\\u006ftes", 1)),
            ("not json", "river".to_string()),
            ("not an object", "[]".to_string()),
            ("empty", String::new()),
        ];
        for (name, text) in cases {
            let bytes = text.into_bytes();
            let r = verify_and_parse(&bytes, &sign(1, &bytes), &pubkey(1));
            assert!(
                matches!(r, Err(Refusal::NotCanonical | Refusal::Malformed(_))),
                "{name}: {r:?}"
            );
        }
    }

    #[test]
    fn field_validation_refuses_hostile_values() {
        type Edit = (&'static str, fn(&mut Manifest));
        let edits: Vec<Edit> = vec![
            ("format 2", |m| m.format = 2),
            ("empty channel", |m| m.channel.clear()),
            ("upper channel", |m| m.channel = "Stable".into()),
            ("version 1.2", |m| m.version = "1.2".into()),
            ("version leading zero", |m| m.version = "0.02.0".into()),
            ("version suffix", |m| m.version = "0.2.0-rc1".into()),
            ("min version bad", |m| m.min_supported = "x".into()),
            ("expiry before release", |m| m.expires_at = m.released_at),
            ("huge notes", |m| m.notes = "x".repeat(9000)),
            ("no artifacts", |m| m.artifacts.clear()),
            ("space in url", |m| {
                m.artifacts.get_mut("linux-x86_64").unwrap().url = "https://a/b c".into()
            }),
            ("newline in url", |m| {
                m.artifacts.get_mut("linux-x86_64").unwrap().url = "https://a/b\nc".into()
            }),
            ("non-ascii url", |m| {
                m.artifacts.get_mut("linux-x86_64").unwrap().url = "https://a/é".into()
            }),
            ("upper sha", |m| {
                m.artifacts.get_mut("linux-x86_64").unwrap().sha256 = "AB".repeat(32)
            }),
            ("short sha", |m| {
                m.artifacts.get_mut("linux-x86_64").unwrap().sha256 = "ab".repeat(31)
            }),
            ("zero size", |m| {
                m.artifacts.get_mut("linux-x86_64").unwrap().size = 0
            }),
            ("huge size", |m| {
                m.artifacts.get_mut("linux-x86_64").unwrap().size = MAX_ARTIFACT_BYTES + 1
            }),
            ("bad artifact name", |m| {
                let a = m.artifacts.remove("linux-x86_64").unwrap();
                m.artifacts.insert("../etc".into(), a);
            }),
        ];
        for (name, edit) in edits {
            let mut m = manifest("0.2.0", 5);
            edit(&mut m);
            let (bytes, sig) = signed(&m);
            assert!(
                matches!(
                    verify_and_parse(&bytes, &sig, &pubkey(1)),
                    Err(Refusal::Malformed(_))
                ),
                "{name}"
            );
        }
    }

    fn eval(m: &Manifest, running: &str, now: u64, trust: Trust) -> Result<Verdict, Refusal> {
        evaluate(m, v(running), "stable", "linux-x86_64", now, trust)
    }

    #[test]
    fn rules_table() {
        let fresh = Trust::default();
        let seen = |seq, ver| Trust {
            highest_sequence: seq,
            highest_version: Some(v(ver)),
        };
        let m = manifest("0.2.0", 10);
        // Newer than running: offered.
        let Ok(Verdict::Available(o)) = eval(&m, "0.1.0", NOW, fresh) else {
            panic!("expected an offer")
        };
        assert_eq!(o.version, v("0.2.0"));
        assert!(!o.required);
        // Equal and older: nothing.
        assert_eq!(eval(&m, "0.2.0", NOW, fresh), Ok(Verdict::UpToDate));
        assert_eq!(eval(&m, "0.3.0", NOW, fresh), Ok(Verdict::UpToDate));
        // The same manifest again is fine.
        assert!(matches!(
            eval(&m, "0.1.0", NOW, seen(10, "0.2.0")),
            Ok(Verdict::Available(_))
        ));
        // Replay of an older sequence, even for a higher version than running.
        assert_eq!(
            eval(&m, "0.1.0", NOW, seen(11, "0.1.0")),
            Err(Refusal::Replay {
                sequence: 10,
                highest: 11
            })
        );
        // Rollback: a higher sequence cannot carry a lower version than was seen.
        let older = manifest("0.1.5", 20);
        assert_eq!(
            eval(&older, "0.1.0", NOW, seen(10, "0.2.0")),
            Err(Refusal::Rollback {
                version: v("0.1.5"),
                highest: v("0.2.0")
            })
        );
        // Expiry, and the boundary.
        assert_eq!(
            eval(&m, "0.1.0", m.expires_at + 1, fresh),
            Err(Refusal::Expired)
        );
        assert!(eval(&m, "0.1.0", m.expires_at, fresh).is_ok());
        // Released far in the future.
        assert_eq!(
            eval(&m, "0.1.0", m.released_at - MAX_FUTURE_SKEW_SECS - 1, fresh),
            Err(Refusal::FromTheFuture)
        );
        assert!(eval(&m, "0.1.0", m.released_at - MAX_FUTURE_SKEW_SECS, fresh).is_ok());
        // Channel.
        assert_eq!(
            evaluate(&m, v("0.1.0"), "beta", "linux-x86_64", NOW, fresh),
            Err(Refusal::WrongChannel {
                got: "stable".into()
            })
        );
        // Platform.
        assert_eq!(
            evaluate(&m, v("0.1.0"), "stable", "windows-x86_64", NOW, fresh),
            Err(Refusal::NoArtifactForPlatform("windows-x86_64".into()))
        );
        // min_supported marks the update required.
        let mut req = manifest("0.3.0", 10);
        req.min_supported = "0.2.0".into();
        let Ok(Verdict::Available(o)) = eval(&req, "0.1.0", NOW, fresh) else {
            panic!()
        };
        assert!(o.required);
    }

    #[test]
    fn trust_only_rises() {
        let t = Trust::default().accepting(&manifest("0.2.0", 10));
        assert_eq!(
            (t.highest_sequence, t.highest_version),
            (10, Some(v("0.2.0")))
        );
        let t = t.accepting(&manifest("0.2.0", 10));
        assert_eq!(t.highest_sequence, 10);
        let t = t.accepting(&manifest("0.3.0", 12));
        assert_eq!(
            (t.highest_sequence, t.highest_version),
            (12, Some(v("0.3.0")))
        );
    }
}
