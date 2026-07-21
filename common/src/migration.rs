//! Legacy room-contract migration registry (freenet/river#292).
//!
//! The room contract key is `BLAKE3(room_contract.wasm, params)` with
//! `params = { owner: VerifyingKey }`, so every room-contract WASM upgrade
//! moves the key for every owner. A client only has the *current* WASM
//! compiled in, so on its own it can derive exactly one key per owner — the
//! current one.
//!
//! [`LEGACY_ROOM_CONTRACT_CODE_HASHES`] records the BLAKE3 code hash of every
//! previous room-contract WASM generation (generated at build time from
//! `legacy_room_contracts.toml`). Combined with an owner's verifying key, each
//! hash yields the contract key that owner's room used under that generation,
//! which lets a client probe older generations newest-to-oldest to recover a
//! room that has been dormant across several WASM upgrades.
//!
//! This is the room-contract analogue of the chat delegate's
//! `legacy_delegates.toml` registry.
//!
//! Gated behind the `migration` cargo feature so the room-contract and
//! chat-delegate WASM builds never compile it — that keeps their WASM bytes
//! (and therefore their contract/delegate keys) byte-identical. The recovery
//! logic is a pure client concern; the contract itself never derives legacy
//! keys.
//!
//! This byte-identity guarantee holds because the committed room-contract /
//! chat-delegate WASM is built with a package-scoped `cargo build -p
//! room-contract` / `-p chat-delegate` (see `scripts/sync-wasm.sh` and the
//! `build-room-contract` task in `Makefile.toml`). Cargo only unifies features
//! across packages built in the *same* invocation, so a package-scoped build
//! never turns `migration` on for the contract. A whole-workspace
//! `cargo build` WOULD unify `migration` onto `river-core` everywhere — so the
//! contract WASM must never be built that way. The `check-room-contract-
//! migration` CI workflow is the backstop if it ever is.

use crate::room_state::ChatRoomParametersV1;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::{ContractKey, Parameters};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/legacy_room_contracts.rs"));
}

/// BLAKE3 code hashes of every *previous* room-contract WASM generation,
/// ordered oldest-first. Generated at build time from
/// `legacy_room_contracts.toml`. The current generation's hash is intentionally
/// absent — it is computed at runtime from the bundled WASM bytes.
pub use generated::LEGACY_ROOM_CONTRACT_CODE_HASHES;

/// CBOR-encode the room contract parameters for `owner_vk`.
///
/// This must match exactly how the UI (`owner_vk_to_contract_key`) and CLI
/// encode `ChatRoomParametersV1` — otherwise derived legacy keys would not
/// match the keys real rooms were stored under.
fn encode_params(owner_vk: &VerifyingKey) -> Vec<u8> {
    let params = ChatRoomParametersV1 { owner: *owner_vk };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&params, &mut buf)
        .expect("CBOR serialization of ChatRoomParametersV1 cannot fail");
    buf
}

/// Derive the room contract key for `owner_vk` under a specific WASM code hash.
///
/// Reproduces what `ContractKey::from_params_and_code` computes, but from the
/// 32-byte code hash alone — no WASM bytes required. The actual hashing is
/// delegated to freenet-stdlib's `ContractKey::from_params` so the algorithm
/// stays in lock-step with the library rather than being re-implemented here.
pub fn contract_key_for_code_hash(owner_vk: &VerifyingKey, code_hash: &[u8; 32]) -> ContractKey {
    let code_hash_b58 = bs58::encode(code_hash)
        .with_alphabet(bs58::Alphabet::BITCOIN)
        .into_string();
    ContractKey::from_params(code_hash_b58, Parameters::from(encode_params(owner_vk)))
        .expect("base58 encoding of a 32-byte array always decodes back to 32 bytes")
}

/// All legacy room-contract keys for `owner_vk`, ordered **newest-first**.
///
/// A backward-search probe walks this list in order so it locates the most
/// recent dormant generation before older ones — the newest generation with
/// live state is the one whose snapshot is least stale.
pub fn legacy_contract_keys_for_owner(owner_vk: &VerifyingKey) -> Vec<ContractKey> {
    LEGACY_ROOM_CONTRACT_CODE_HASHES
        .iter()
        .rev()
        .map(|code_hash| contract_key_for_code_hash(owner_vk, code_hash))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    /// Value-pin for the generated registry (the room-contract analogue of
    /// river-ui's `legacy_set_fingerprint_is_stable_across_codegen_changes`):
    /// blake3 over the code hashes in registry order. Codegen/tooling changes
    /// (freenet/river#398 moved generation to `freenet-migrate-build`) must
    /// reproduce the const's exact values AND order — a drift would misdirect
    /// the dormant-room backward probe. This SHOULD change when a genuinely
    /// new generation is registered — update the constant then — but must
    /// NEVER change from a codegen swap.
    #[test]
    fn registry_values_and_order_are_stable_across_codegen_changes() {
        let mut hasher = blake3::Hasher::new();
        for hash in LEGACY_ROOM_CONTRACT_CODE_HASHES {
            hasher.update(hash);
        }
        assert_eq!(LEGACY_ROOM_CONTRACT_CODE_HASHES.len(), 27);
        assert_eq!(&hasher.finalize().to_hex()[..16], "d931340e569e9c74");
    }

    #[test]
    fn registry_is_non_empty_and_hashes_are_distinct() {
        // The registry must carry every historical generation; a regression
        // that wiped it would silently disable backward recovery.
        assert!(
            !LEGACY_ROOM_CONTRACT_CODE_HASHES.is_empty(),
            "legacy room-contract registry is empty"
        );
        let mut seen = std::collections::HashSet::new();
        for hash in LEGACY_ROOM_CONTRACT_CODE_HASHES {
            assert!(
                seen.insert(*hash),
                "duplicate code hash in legacy room-contract registry: {}",
                hex::encode(hash)
            );
        }
    }

    #[test]
    fn legacy_keys_are_newest_first_and_complete() {
        let owner = SigningKey::generate(&mut OsRng).verifying_key();
        let keys = legacy_contract_keys_for_owner(&owner);
        assert_eq!(keys.len(), LEGACY_ROOM_CONTRACT_CODE_HASHES.len());
        // Newest-first: index 0 corresponds to the last registry entry.
        let newest = LEGACY_ROOM_CONTRACT_CODE_HASHES.last().unwrap();
        assert_eq!(keys[0], contract_key_for_code_hash(&owner, newest));
    }

    #[test]
    fn keys_are_owner_specific() {
        // Every owner gets a different key for the same code hash, so a probe
        // can never accidentally read another owner's room.
        let owner_a = SigningKey::generate(&mut OsRng).verifying_key();
        let owner_b = SigningKey::generate(&mut OsRng).verifying_key();
        let code_hash = LEGACY_ROOM_CONTRACT_CODE_HASHES[0];
        assert_ne!(
            contract_key_for_code_hash(&owner_a, &code_hash),
            contract_key_for_code_hash(&owner_b, &code_hash),
        );
    }

    #[test]
    fn derivation_is_deterministic() {
        let owner = SigningKey::generate(&mut OsRng).verifying_key();
        let code_hash = LEGACY_ROOM_CONTRACT_CODE_HASHES[0];
        assert_eq!(
            contract_key_for_code_hash(&owner, &code_hash),
            contract_key_for_code_hash(&owner, &code_hash),
        );
    }
}
