//! Short room handles (freenet/river#973): the pure derivation, base58, state
//! parsing and verification the UI needs to open `?room=<handle>`.
//!
//! The handle is `BLAKE3("freenet-shortkey/v1" || owner_vk)[..16]`, written as 21
//! or 22 Bitcoin-alphabet base58 characters. The generation 1 resolver contract
//! (`freenet/freenet-shortkey`) is parameterised by those 16 bytes, so its
//! address is computable from the handle alone, and its state holds the owner
//! key. Every reader repeats the contract's own check: the key must hash to the
//! handle. A resolver can therefore never send a reader to a different room
//! (short of a 2^64-work collision, which is why the Official room is also
//! compared with its known full key: [`OFFICIAL_ROOM_OWNER_KEY`]).
//!
//! This is a copy of `cli/src/shortkey.rs` (riverctl, #1043) restricted to the
//! pure parts, kept in `ui/` and not lifted to `common/`: `common/` is compiled
//! into the room contract and chat delegate WASM, so touching it re-keys them.
//! The golden vectors below are the same ones riverctl and the resolver repo
//! pin, so the three implementations cannot drift apart unnoticed.
//!
//! Generation 1 is frozen: a different resolver is a new generation with a new
//! domain string, never an edit of the constants here.

use freenet_stdlib::prelude::ContractInstanceId;
use std::fmt;

/// Domain separation prefix for the handle derivation (generation 1).
pub const DERIVE_DOMAIN: &[u8] = b"freenet-shortkey/v1";
/// Handle length in bytes (128 bits).
pub const HANDLE_LEN: usize = 16;
/// Ed25519 verifying key length in bytes.
pub const KEY_LEN: usize = 32;
/// Version byte of the resolver's state encoding.
const WIRE_VERSION: u8 = 1;
/// Largest optional signed block the resolver accepts, in bytes.
const MAX_SIGNED_DATA: usize = 16 * 1024;

/// The generation 1 resolver's code hash (BLAKE3 of its WASM), as the freenet
/// CLI prints it.
pub const RESOLVER_CODE_HASH_B58: &str = "9bwbWefShTXmVrizwn4XQ3NdT1Zjm7rm1uWLUAwqewuW";

/// The Official River room's owner key. Its handle is checked against this key
/// as well as against the hash, so a collision against the one room everybody
/// is sent to cannot redirect it.
pub const OFFICIAL_ROOM_OWNER_KEY: &str = "4uNUKFzZQCnzo4K2ecZ16cMsYEEfoaRS35z6exEsbvm4";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortError {
    /// Not valid base58, wrong length, or a non-canonical spelling.
    BadHandle,
    /// A resolver state that does not parse.
    BadState(&'static str),
    /// The key does not hash to the handle.
    Mismatch,
    /// Not an acceptable Ed25519 owner key (non-canonical, small-order or
    /// mixed-order), which the resolver contract refuses.
    BadKey,
    /// The handle is the Official room's, and the resolver returned another key.
    OfficialMismatch,
}

/// A 128-bit short handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShortHandle(pub [u8; HANDLE_LEN]);

impl ShortHandle {
    /// `BLAKE3(DERIVE_DOMAIN || vk)[..16]`.
    pub fn derive(vk: &[u8; KEY_LEN]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(DERIVE_DOMAIN);
        h.update(vk);
        let mut out = [0u8; HANDLE_LEN];
        out.copy_from_slice(&h.finalize().as_bytes()[..HANDLE_LEN]);
        ShortHandle(out)
    }

    /// Bitcoin-alphabet base58 text (21 or 22 characters).
    pub fn to_base58(&self) -> String {
        bs58::encode(self.0)
            .with_alphabet(bs58::Alphabet::BITCOIN)
            .into_string()
    }

    /// Strict inverse of [`Self::to_base58`]: rejects bad characters, a length
    /// other than 16 bytes and any non-canonical spelling, so each handle has
    /// one accepted text form.
    pub fn from_base58(s: &str) -> Result<Self, ShortError> {
        let bytes = bs58::decode(s)
            .with_alphabet(bs58::Alphabet::BITCOIN)
            .into_vec()
            .map_err(|_| ShortError::BadHandle)?;
        let arr: [u8; HANDLE_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| ShortError::BadHandle)?;
        let handle = ShortHandle(arr);
        if handle.to_base58() != s {
            return Err(ShortError::BadHandle);
        }
        Ok(handle)
    }

    /// Check that `vk` is the key this handle names.
    pub fn verify(&self, vk: &[u8; KEY_LEN]) -> Result<(), ShortError> {
        if Self::derive(vk) == *self {
            Ok(())
        } else {
            Err(ShortError::Mismatch)
        }
    }

    /// The generation 1 resolver contract's instance id for this handle:
    /// `BLAKE3(code hash || handle bytes)`, the same derivation freenet-stdlib
    /// applies to a contract's code and parameters, computed from the code hash
    /// so the resolver's WASM need not be bundled.
    pub fn resolver_instance_id(&self) -> ContractInstanceId {
        let code_hash = bs58::decode(RESOLVER_CODE_HASH_B58)
            .with_alphabet(bs58::Alphabet::BITCOIN)
            .into_vec()
            .expect("the resolver code hash constant is valid base58");
        let mut h = blake3::Hasher::new();
        h.update(&code_hash);
        h.update(&self.0);
        ContractInstanceId::new(*h.finalize().as_bytes())
    }
}

impl fmt::Display for ShortHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

/// The minimal resolver state for `vk`: `[1][vk][0]`, no signed block.
#[cfg_attr(not(any(test, feature = "no-sync")), allow(dead_code))]
pub fn minimal_state(vk: &[u8; KEY_LEN]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + KEY_LEN);
    out.push(WIRE_VERSION);
    out.extend_from_slice(vk);
    out.push(0);
    out
}

/// Read the owner key out of a resolver state, checking the whole encoding
/// (including the optional signed block's lengths) but not the block's
/// signature: the UI does not use the block.
pub fn state_owner_key(bytes: &[u8]) -> Result<[u8; KEY_LEN], ShortError> {
    if bytes.len() < 2 + KEY_LEN {
        return Err(ShortError::BadState("too short"));
    }
    if bytes[0] != WIRE_VERSION {
        return Err(ShortError::BadState("unknown version"));
    }
    let mut vk = [0u8; KEY_LEN];
    vk.copy_from_slice(&bytes[1..1 + KEY_LEN]);
    let rest = &bytes[1 + KEY_LEN..];
    match rest[0] {
        0 if rest.len() == 1 => Ok(vk),
        0 => Err(ShortError::BadState("trailing bytes")),
        1 => {
            // revision u64, format u16, len u32, data, signature 64
            let header = 1 + 8 + 2 + 4;
            if rest.len() < header {
                return Err(ShortError::BadState("truncated signed block"));
            }
            let len = u32::from_be_bytes(rest[11..15].try_into().expect("4 bytes")) as usize;
            if len > MAX_SIGNED_DATA {
                return Err(ShortError::BadState("signed data too large"));
            }
            if rest.len() != header + len + 64 {
                return Err(ShortError::BadState("signed block length mismatch"));
            }
            Ok(vk)
        }
        _ => Err(ShortError::BadState("unknown block flag")),
    }
}

/// The resolver contract's `validate_key`: the canonical encoding of a point in
/// the prime-order subgroup that is not the identity.
pub fn validate_owner_key(vk: &[u8; KEY_LEN]) -> Result<(), ShortError> {
    let key = ed25519_dalek::VerifyingKey::from_bytes(vk).map_err(|_| ShortError::BadKey)?;
    let point = key.to_edwards();
    if point.compress().to_bytes() != *vk
        || point.is_small_order()
        || !point.is_torsion_free()
        || key.is_weak()
    {
        return Err(ShortError::BadKey);
    }
    Ok(())
}

/// Whether `handle` is the Official room's, and if so whether `vk` is its key.
fn check_official(handle: &ShortHandle, vk: &[u8; KEY_LEN]) -> Result<(), ShortError> {
    let official = bs58::decode(OFFICIAL_ROOM_OWNER_KEY)
        .with_alphabet(bs58::Alphabet::BITCOIN)
        .into_vec()
        .ok()
        .and_then(|b| <[u8; KEY_LEN]>::try_from(b).ok())
        .expect("the Official room key constant is a valid 32-byte key");
    if ShortHandle::derive(&official) == *handle && official != *vk {
        return Err(ShortError::OfficialMismatch);
    }
    Ok(())
}

/// Read a resolver state and verify it against `handle`: the key it holds must
/// hash to the handle and be a well-formed Ed25519 key, and for the Official
/// room's handle it must be the Official room's key. Every refusal here means
/// the link opens nothing.
pub fn resolve_state(handle: &ShortHandle, state: &[u8]) -> Result<[u8; KEY_LEN], ShortError> {
    let vk = state_owner_key(state)?;
    handle.verify(&vk)?;
    validate_owner_key(&vk)?;
    check_official(handle, &vk)?;
    Ok(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors copied from freenet-shortkey crates/common/tests/protocol.rs
    // (and cli/src/shortkey.rs in riverctl).
    const VECTORS: [([u8; 32], &str, &str); 3] = [
        (
            [0u8; 32],
            "65602aff592963c36beeaa812aa12b83",
            "DX4ZCXQ4NZgiw1Cuh8YjuC",
        ),
        (
            [1u8; 32],
            "8c35f5bdd1081c19e80c6e1e3af9817d",
            "JKCi2BXwJLh46QKm3Sf46p",
        ),
        (
            [0xffu8; 32],
            "562bd5927a6025d29a6f5cdd4eaefc17",
            "BeAcviiYQKaBSC15JkK1TL",
        ),
    ];

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn key_of(seed: u8) -> [u8; 32] {
        *ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
            .verifying_key()
            .as_bytes()
    }

    #[test]
    fn derive_matches_the_resolver_repo_vectors() {
        for (k, h, b58) in VECTORS {
            let id = ShortHandle::derive(&k);
            assert_eq!(hex(&id.0), h);
            assert_eq!(id.to_base58(), b58);
            assert_eq!(ShortHandle::from_base58(b58).unwrap(), id);
        }
    }

    #[test]
    fn minimal_state_matches_the_resolver_repo_vector() {
        let vk: [u8; 32] = {
            let mut v = [0u8; 32];
            let h = "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c";
            for (i, b) in v.iter_mut().enumerate() {
                *b = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
            }
            v
        };
        assert_eq!(
            hex(&minimal_state(&vk)),
            "018a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c00"
        );
        assert_eq!(state_owner_key(&minimal_state(&vk)).unwrap(), vk);
    }

    #[test]
    fn signed_state_vector_parses_and_malformed_ones_are_refused() {
        let h = "018a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c01000000000000000701020000000568656c6c6f06c8882da21b217f86411af088be78fb28514d12b860e5e832ad4e455fbc4d270f315f6a2734df066c496cccd798b4283bfc71473613e6dee540228b7bc9eb0e";
        let bytes: Vec<u8> = (0..h.len() / 2)
            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let vk = state_owner_key(&bytes).unwrap();
        assert_eq!(hex(&vk), &h[2..66]);
        assert!(state_owner_key(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(state_owner_key(&longer).is_err());
        assert!(state_owner_key(&[]).is_err());
        assert!(state_owner_key(&[2u8; 40]).is_err());
    }

    #[test]
    fn base58_round_trip_and_strictness() {
        for seed in 0u8..=255 {
            let id = ShortHandle::derive(&[seed; 32]);
            let s = id.to_base58();
            assert!(s.len() == 21 || s.len() == 22, "{s}");
            assert_eq!(ShortHandle::from_base58(&s).unwrap(), id);
        }
        // A full key is 32 bytes, not a handle.
        let full = bs58::encode([7u8; 32]).into_string();
        assert!(ShortHandle::from_base58(&full).is_err());
        assert!(ShortHandle::from_base58("0OIl").is_err());
        assert!(ShortHandle::from_base58("").is_err());
        // A leading '1' is a zero byte: this decodes to 17 bytes.
        assert!(ShortHandle::from_base58("1DX4ZCXQ4NZgiw1Cuh8YjuC").is_err());
    }

    #[test]
    fn verify_rejects_a_different_key() {
        let id = ShortHandle::derive(&[1u8; 32]);
        assert!(id.verify(&[1u8; 32]).is_ok());
        assert_eq!(id.verify(&[2u8; 32]), Err(ShortError::Mismatch));
    }

    #[test]
    fn resolve_state_accepts_the_matching_key() {
        let vk = key_of(9);
        let handle = ShortHandle::derive(&vk);
        assert_eq!(resolve_state(&handle, &minimal_state(&vk)), Ok(vk));
    }

    #[test]
    fn resolve_state_refuses_a_key_that_does_not_hash_to_the_handle() {
        let vk = key_of(9);
        let other_vk = key_of(10);
        let handle = ShortHandle::derive(&vk);
        assert_eq!(
            resolve_state(&handle, &minimal_state(&other_vk)),
            Err(ShortError::Mismatch)
        );
    }

    #[test]
    fn resolve_state_refuses_malformed_state() {
        let handle = ShortHandle::derive(&key_of(9));
        assert!(matches!(
            resolve_state(&handle, b"\x02junk"),
            Err(ShortError::BadState(_))
        ));
        assert!(matches!(
            resolve_state(&handle, &[]),
            Err(ShortError::BadState(_))
        ));
    }

    #[test]
    fn resolve_state_refuses_a_key_that_is_not_a_good_ed25519_key() {
        // The identity point hashes fine to its own handle but the resolver
        // contract would never have accepted it.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let handle = ShortHandle::derive(&identity);
        assert_eq!(
            resolve_state(&handle, &minimal_state(&identity)),
            Err(ShortError::BadKey)
        );
    }

    fn official() -> [u8; 32] {
        let b = bs58::decode(OFFICIAL_ROOM_OWNER_KEY).into_vec().unwrap();
        b.try_into().unwrap()
    }

    #[test]
    fn the_official_key_constant_is_a_valid_key_and_resolves_to_itself() {
        let vk = official();
        assert!(validate_owner_key(&vk).is_ok());
        let handle = ShortHandle::derive(&vk);
        assert_eq!(resolve_state(&handle, &minimal_state(&vk)), Ok(vk));
    }

    #[test]
    fn the_official_handle_refuses_any_other_key() {
        // check_official is separate from the hash check: a key that hashed to
        // the Official handle (a collision) would pass `verify` and must still
        // be refused.
        let handle = ShortHandle::derive(&official());
        assert_eq!(
            check_official(&handle, &key_of(3)),
            Err(ShortError::OfficialMismatch)
        );
        assert_eq!(check_official(&handle, &official()), Ok(()));
        // A handle that is not the Official room's is not constrained by it.
        let other = ShortHandle::derive(&key_of(3));
        assert_eq!(check_official(&other, &key_of(3)), Ok(()));
    }

    #[test]
    fn resolver_id_is_the_stdlib_derivation_from_the_code_hash() {
        use freenet_stdlib::prelude::{ContractCode, Parameters};
        // Any code will do: the derivation is BLAKE3(code hash || params), so
        // build a code whose hash we then feed to `resolver_instance_id`'s
        // formula and compare with the stdlib's own result.
        let code = ContractCode::from(b"some wasm".to_vec());
        let handle = ShortHandle::derive(&key_of(1));
        let expected = ContractInstanceId::from_params_and_code(
            Parameters::from(handle.0.to_vec()),
            &code,
        );
        let mut h = blake3::Hasher::new();
        h.update(&**code.hash());
        h.update(&handle.0);
        assert_eq!(ContractInstanceId::new(*h.finalize().as_bytes()), expected);
    }

    #[test]
    fn resolver_id_golden_vector() {
        // Computed by riverctl's `resolver_contract_key` from the committed
        // generation 1 resolver WASM (cli/contracts/freenet_shortkey_v1.wasm,
        // BLAKE3 7fd0f944...), handle of key [1; 32].
        let handle = ShortHandle::from_base58("JKCi2BXwJLh46QKm3Sf46p").unwrap();
        assert_eq!(
            handle.resolver_instance_id().to_string(),
            "2zqex5TMenbwzbK91c73XFqw3qZTmcurdBPb5nSAbdra"
        );
        let code_hash = bs58::decode(RESOLVER_CODE_HASH_B58).into_vec().unwrap();
        assert_eq!(
            hex(&code_hash),
            "7fd0f944cb9d9cb04673288846c4434525d5d103deebcde257e290c1900d04c5"
        );
    }
}
