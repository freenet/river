//! Short room handles (freenet/river#973): a 16-byte handle derived from a room
//! owner's key, resolved to the full key through the `freenet-shortkey` resolver
//! contract (repo `freenet/freenet-shortkey`, generation 1).
//!
//! The handle is `BLAKE3("freenet-shortkey/v1" || owner_vk)[..16]`, written as 21
//! or 22 Bitcoin-alphabet base58 characters. The resolver contract is
//! parameterised by those 16 bytes, so its address is computable from the handle
//! alone, and its state holds the owner key. The contract rejects a state whose key
//! does not hash to the handle, and every reader repeats that check, so a
//! resolver can never point a handle at a different key (short of a 2^64-work
//! collision, which is why the Official room's UI also compares the full key).
//!
//! The pure parts (derive, encode, verify, state bytes, links) are free of I/O and
//! of freenet-stdlib, so the UI can lift them unchanged. Only the node access at
//! the bottom touches the stdlib. The derivation is implemented here, not taken
//! from `freenet-shortkey-common`, to keep the dependency set unchanged; the
//! golden vectors below are copied from that crate's tests and pin the two
//! together.
//!
//! Generation 1 is frozen: changing the resolver WASM changes every resolver
//! address, so a different resolver is a new generation with a new domain string,
//! never an edit of this file's constants.

use std::fmt;
use std::time::Duration;

use anyhow::{anyhow, Result};
use freenet_stdlib::client_api::{
    ContractError, ContractRequest, ContractResponse, ErrorKind, HostResponse, RequestError,
};
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion,
    Parameters, WrappedContract, WrappedState,
};
use std::sync::Arc;

use crate::api::{classify_put_response, ApiClient, PutAck, WaitError};

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

/// BLAKE3 of the generation 1 resolver WASM, lowercase hex. The Freenet code hash
/// is the base58 form of the same 32 bytes: [`RESOLVER_CODE_HASH_B58`].
pub const RESOLVER_CODE_HASH_HEX: &str =
    "7fd0f944cb9d9cb04673288846c4434525d5d103deebcde257e290c1900d04c5";
/// The generation 1 code hash as the freenet CLI prints it.
pub const RESOLVER_CODE_HASH_B58: &str = "9bwbWefShTXmVrizwn4XQ3NdT1Zjm7rm1uWLUAwqewuW";

/// The committed generation 1 resolver WASM (`freenet_shortkey_v1.wasm` at
/// freenet/freenet-shortkey `b967e538`). Never rebuild or replace it.
// bundled-wasm-ok: this is the resolver, not the room contract.
const RESOLVER_WASM: &[u8] = include_bytes!("../contracts/freenet_shortkey_v1.wasm");

/// The River web container id: the app a share link opens.
/// `ui/src/public_room.rs` reads the same value from `published-contract/contract-id.txt`;
/// a test pins the two together.
pub const WEB_APP_ID: &str = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";

/// Whether the shipped River UI opens `?room=<handle>` links. While this is false, `room
/// short-link` says such links open only in a River release with short-link support. The full
/// link is always the canonical one to share; a short link is an optional extra, shown only when
/// this is true and its entry resolves. The UI resolve PR (freenet/river#973) flips it.
pub const UI_RESOLVES_SHORT_LINKS: bool = false;

/// How long to wait for the node to answer a resolver GET.
pub const GET_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the node to answer a resolver PUT.
pub const PUT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortError {
    /// Not valid base58, wrong length, or a non-canonical spelling.
    BadHandle(String),
    /// A resolver state that does not parse.
    BadState(&'static str),
    /// The key does not hash to the handle.
    Mismatch,
    /// Not an acceptable Ed25519 owner key (non-canonical, small-order or mixed-order),
    /// which the resolver contract refuses.
    BadKey,
}

impl fmt::Display for ShortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShortError::BadHandle(s) => write!(f, "invalid short handle: {s}"),
            ShortError::BadState(s) => write!(f, "malformed resolver state: {s}"),
            ShortError::BadKey => write!(
                f,
                "not an acceptable Ed25519 owner key (non-canonical, small-order or mixed-order); \
                 the resolver contract would refuse it"
            ),
            ShortError::Mismatch => write!(
                f,
                "the resolver returned a key that does not hash to this handle; refusing it"
            ),
        }
    }
}

impl std::error::Error for ShortError {}

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
        bs58::encode(self.0).into_string()
    }

    /// Strict inverse of [`Self::to_base58`]: rejects bad characters, a length
    /// other than 16 bytes and any non-canonical spelling, so each handle has one
    /// accepted text form.
    pub fn from_base58(s: &str) -> Result<Self, ShortError> {
        let bytes = bs58::decode(s)
            .into_vec()
            .map_err(|e| ShortError::BadHandle(e.to_string()))?;
        let arr: [u8; HANDLE_LEN] = bytes.as_slice().try_into().map_err(|_| {
            ShortError::BadHandle(format!("decodes to {} bytes, expected 16", bytes.len()))
        })?;
        let handle = ShortHandle(arr);
        if handle.to_base58() != s {
            return Err(ShortError::BadHandle("non-canonical spelling".into()));
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
}

impl fmt::Display for ShortHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

/// The base58 text of a full owner key, as a long-form link carries it.
pub fn owner_key_b58(vk: &[u8; KEY_LEN]) -> String {
    bs58::encode(vk).into_string()
}

/// Parse a full owner key from base58 text.
pub fn parse_owner_key(s: &str) -> Result<[u8; KEY_LEN], ShortError> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|e| ShortError::BadHandle(e.to_string()))?;
    bytes.as_slice().try_into().map_err(|_| {
        ShortError::BadHandle(format!("decodes to {} bytes, expected 32", bytes.len()))
    })
}

/// `https://freenet.org/open#<web container>/?room=<handle>`.
pub fn short_link(handle: &ShortHandle) -> String {
    format!(
        "https://freenet.org/open#{WEB_APP_ID}/?room={}",
        handle.to_base58()
    )
}

/// `https://freenet.org/open#<web container>/?room=<owner key>`: the long form,
/// which keeps working without a resolver.
pub fn long_link(vk: &[u8; KEY_LEN]) -> String {
    format!(
        "https://freenet.org/open#{WEB_APP_ID}/?room={}",
        owner_key_b58(vk)
    )
}

/// The minimal resolver state for `vk`: `[1][vk][0]`, no signed block. Anyone can
/// publish it, because the contract checks the key against the handle itself.
pub fn minimal_state(vk: &[u8; KEY_LEN]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + KEY_LEN);
    out.push(WIRE_VERSION);
    out.extend_from_slice(vk);
    out.push(0);
    out
}

/// Read the owner key out of a resolver state, checking the whole encoding
/// (including the optional signed block's lengths) but not the block's signature:
/// riverctl does not use the block.
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

/// The resolver contract's `validate_key`: the canonical encoding of a point in the prime-order
/// subgroup that is not the identity. Checked here too so `short-link` refuses early a key the
/// contract would refuse on PUT.
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

/// Read a resolver state and verify it against `handle`: the key it holds must
/// hash to the handle, and must be a well-formed Ed25519 key.
pub fn resolve_state(handle: &ShortHandle, state: &[u8]) -> Result<[u8; KEY_LEN], ShortError> {
    let vk = state_owner_key(state)?;
    handle.verify(&vk)?;
    validate_owner_key(&vk)?;
    Ok(vk)
}

/// Fail unless the embedded WASM is exactly generation 1. Checked before every
/// publish: a wrong resolver would PUT to an address nobody looks up.
pub fn check_embedded_resolver() -> Result<()> {
    let hex: String = blake3::hash(RESOLVER_WASM)
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if hex != RESOLVER_CODE_HASH_HEX {
        return Err(anyhow!(
            "the embedded short-handle resolver WASM has BLAKE3 {hex}, expected generation 1 \
             ({RESOLVER_CODE_HASH_HEX}); refusing to use it"
        ));
    }
    Ok(())
}

/// The resolver contract key for `handle`: `BLAKE3(BLAKE3(wasm) || handle bytes)`.
pub fn resolver_contract_key(handle: &ShortHandle) -> ContractKey {
    ContractKey::from_params_and_code(
        Parameters::from(handle.0.to_vec()),
        ContractCode::from(RESOLVER_WASM),
    )
}

/// What a resolver GET found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The contract exists; this is its state (not yet verified).
    Found(Vec<u8>),
    /// The node answered that there is no such contract.
    NotFound(String),
    /// No answer in time: the entry may or may not exist.
    TimedOut,
    /// The lookup itself failed (connection closed, node error): says nothing about the entry.
    Error(String),
}

/// What publishing did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// The entry was already there; nothing was sent.
    AlreadyPublished,
    /// The entry was created.
    Created,
}

/// A request the resolver flow may send: a GET or a PUT, by construction. `ApiClient::send_and_await`
/// takes only this, so it cannot be used to send an Update or any other operation.
pub enum ResolverRequest {
    Get(ContractInstanceId),
    Put(ShortHandle, [u8; KEY_LEN]),
}

impl ResolverRequest {
    pub(crate) fn into_request(self) -> ContractRequest<'static> {
        match self {
            ResolverRequest::Get(id) => ContractRequest::Get {
                key: id,
                return_contract_code: false,
                subscribe: false,
                blocking_subscribe: false,
            },
            ResolverRequest::Put(handle, vk) => ContractRequest::Put {
                contract: resolver_container(&handle),
                state: resolver_state(&vk),
                related_contracts: Default::default(),
                subscribe: false,
                blocking_subscribe: false,
            },
        }
    }
}

/// How a resolver PUT answer counts: `Merged` means the entry already existed.
pub(crate) fn published_from_ack(ack: PutAck) -> Option<Published> {
    match ack {
        PutAck::NotYet => None,
        PutAck::Stored | PutAck::Acknowledged => Some(Published::Created),
        PutAck::Merged => Some(Published::AlreadyPublished),
    }
}

/// Classify the error from a failed wait into a [`Lookup`].
pub fn lookup_from_error(e: &anyhow::Error) -> Lookup {
    match e.downcast_ref::<WaitError>() {
        Some(WaitError::Timeout(_)) => Lookup::TimedOut,
        Some(WaitError::Recv(client)) => match client.kind() {
            ErrorKind::RequestError(RequestError::ContractError(
                ContractError::MissingContract { .. } | ContractError::Get { .. },
            )) => Lookup::NotFound(e.to_string()),
            _ => Lookup::Error(e.to_string()),
        },
        None => Lookup::Error(e.to_string()),
    }
}

/// The resolver contract container for `handle`: generation 1 code, the handle bytes as parameters.
pub fn resolver_container(handle: &ShortHandle) -> ContractContainer {
    ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        Arc::new(ContractCode::from(RESOLVER_WASM)),
        Parameters::from(handle.0.to_vec()),
    )))
}

/// The state a resolver PUT carries for `vk`.
pub fn resolver_state(vk: &[u8; KEY_LEN]) -> WrappedState {
    WrappedState::new(minimal_state(vk))
}

/// Resolve `handle` to the full owner key from a lookup, verifying the key against the handle.
pub fn resolve_lookup(handle: &ShortHandle, lookup: Lookup) -> Result<[u8; KEY_LEN]> {
    match lookup {
        Lookup::Found(state) => Ok(resolve_state(handle, &state)?),
        Lookup::NotFound(why) => Err(anyhow!(
            "no resolver entry for handle {handle}: the room owner has not published one \
             (node said: {why})"
        )),
        Lookup::Error(why) => Err(anyhow!(
            "could not look up handle {handle}: {why}. This says nothing about whether the \
             entry exists; check the node connection (--node-url) and retry"
        )),
        Lookup::TimedOut => Err(anyhow!(
            "timed out looking up handle {handle}; the entry may not exist or the network \
             may be slow"
        )),
    }
}

impl ApiClient {
    /// GET the resolver contract for `handle`.
    pub async fn short_handle_lookup(&self, handle: &ShortHandle) -> Lookup {
        let id = *resolver_contract_key(handle).id();
        let answer = self
            .send_and_await(
                ResolverRequest::Get(id),
                GET_TIMEOUT,
                "short handle GET response",
                |response| match response {
                    HostResponse::ContractResponse(ContractResponse::GetResponse {
                        key,
                        state,
                        ..
                    }) if *key.id() == id => Some(Ok(Lookup::Found(state.to_vec()))),
                    HostResponse::ContractResponse(ContractResponse::NotFound { instance_id })
                        if *instance_id == id =>
                    {
                        Some(Ok(Lookup::NotFound("the node could not find it".into())))
                    }
                    _ => None,
                },
            )
            .await;
        answer.unwrap_or_else(|e| lookup_from_error(&e))
    }

    /// PUT the minimal resolver entry for `vk`, unless it is already there.
    pub async fn short_handle_publish(&self, vk: &[u8; KEY_LEN]) -> Result<Published> {
        check_embedded_resolver()?;
        validate_owner_key(vk)?;
        let handle = ShortHandle::derive(vk);
        if let Lookup::Found(state) = self.short_handle_lookup(&handle).await {
            // The contract rejects a key that does not hash to the handle, so a found state
            // for another key cannot exist; error rather than silently skip.
            if resolve_state(&handle, &state)? == *vk {
                return Ok(Published::AlreadyPublished);
            }
        }
        let contract_key = resolver_contract_key(&handle);
        self.send_and_await(
            ResolverRequest::Put(handle, *vk),
            PUT_TIMEOUT,
            "short handle PUT response",
            |response| published_from_ack(classify_put_response(&contract_key, response)).map(Ok),
        )
        .await
    }

    /// Resolve `handle` to the full owner key, verifying the key against the handle.
    pub async fn short_handle_resolve(&self, handle: &ShortHandle) -> Result<[u8; KEY_LEN]> {
        resolve_lookup(handle, self.short_handle_lookup(handle).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors copied from freenet-shortkey crates/common/tests/protocol.rs.
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
        // MINIMAL_STATE_HEX in freenet-shortkey crates/common/src/tests.rs, key from seed [1;32].
        let vk: [u8; 32] = {
            let mut v = [0u8; 32];
            let h = "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c";
            for i in 0..32 {
                v[i] = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
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
    fn signed_state_vector_parses() {
        // SIGNED_STATE_HEX from the same file: owner key plus a signed block.
        let h = "018a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c01000000000000000701020000000568656c6c6f06c8882da21b217f86411af088be78fb28514d12b860e5e832ad4e455fbc4d270f315f6a2734df066c496cccd798b4283bfc71473613e6dee540228b7bc9eb0e";
        let bytes: Vec<u8> = (0..h.len() / 2)
            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let vk = state_owner_key(&bytes).unwrap();
        assert_eq!(hex(&vk), &h[2..66]);
        // truncation and trailing bytes are refused
        assert!(state_owner_key(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(state_owner_key(&longer).is_err());
    }

    #[test]
    fn base58_round_trip_and_strictness() {
        for seed in 0u8..=255 {
            let id = ShortHandle::derive(&[seed; 32]);
            let s = id.to_base58();
            assert!(s.len() == 21 || s.len() == 22, "{s}");
            assert_eq!(ShortHandle::from_base58(&s).unwrap(), id);
        }
        // a full key is 32 bytes, not a handle
        let full = owner_key_b58(&[7u8; 32]);
        assert!(ShortHandle::from_base58(&full).is_err());
        // characters outside the alphabet, empty text
        assert!(ShortHandle::from_base58("0OIl").is_err());
        assert!(ShortHandle::from_base58("").is_err());
        // a leading '1' is a zero byte: a 21-char handle with an extra '1' decodes to 17 bytes
        assert!(ShortHandle::from_base58("1DX4ZCXQ4NZgiw1Cuh8YjuC").is_err());
    }

    #[test]
    fn verify_rejects_a_different_key() {
        let id = ShortHandle::derive(&[1u8; 32]);
        assert!(id.verify(&[1u8; 32]).is_ok());
        assert_eq!(id.verify(&[2u8; 32]), Err(ShortError::Mismatch));
    }

    #[test]
    fn resolve_state_refuses_a_key_that_does_not_hash_to_the_handle() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let vk = *sk.verifying_key().as_bytes();
        let other = ed25519_dalek::SigningKey::from_bytes(&[10u8; 32]);
        let other_vk = *other.verifying_key().as_bytes();
        let id = ShortHandle::derive(&vk);
        assert_eq!(resolve_state(&id, &minimal_state(&vk)).unwrap(), vk);
        assert_eq!(
            resolve_state(&id, &minimal_state(&other_vk)),
            Err(ShortError::Mismatch)
        );
        assert!(resolve_state(&id, b"\x02junk").is_err());
    }

    #[test]
    fn links_use_the_short_and_long_forms() {
        let vk = [3u8; 32];
        let id = ShortHandle::derive(&vk);
        assert_eq!(
            short_link(&id),
            format!(
                "https://freenet.org/open#{WEB_APP_ID}/?room={}",
                id.to_base58()
            )
        );
        assert_eq!(
            long_link(&vk),
            format!(
                "https://freenet.org/open#{WEB_APP_ID}/?room={}",
                owner_key_b58(&vk)
            )
        );
        assert_eq!(parse_owner_key(&owner_key_b58(&vk)).unwrap(), vk);
    }

    #[test]
    fn web_app_id_matches_the_published_container() {
        let published = include_str!("../../published-contract/contract-id.txt");
        assert_eq!(WEB_APP_ID, published.trim());
    }

    #[test]
    fn embedded_resolver_is_generation_one() {
        check_embedded_resolver().unwrap();
        // The Freenet code hash is the base58 of the same 32 bytes.
        let hash = blake3::hash(RESOLVER_WASM);
        assert_eq!(
            bs58::encode(hash.as_bytes()).into_string(),
            RESOLVER_CODE_HASH_B58
        );
        // And the stdlib agrees with how it derives the code hash.
        let code = ContractCode::from(RESOLVER_WASM);
        assert_eq!(code.hash().to_string(), RESOLVER_CODE_HASH_B58);
    }

    #[test]
    fn resolver_address_depends_on_the_handle() {
        let a = resolver_contract_key(&ShortHandle::derive(&[1u8; 32]));
        let b = resolver_contract_key(&ShortHandle::derive(&[2u8; 32]));
        assert_ne!(a.id(), b.id());
        assert_eq!(a.code_hash().to_string(), RESOLVER_CODE_HASH_B58);
    }

    #[test]
    fn short_links_stay_unadvertised_until_the_ui_resolves_them() {
        // The UI resolve PR flips the constant and this test.
        assert!(!UI_RESOLVES_SHORT_LINKS);
    }

    // ---- review fixes (river#1043) ----

    #[test]
    fn owner_key_check_is_as_strict_as_the_contract() {
        let good = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        assert!(validate_owner_key(good.verifying_key().as_bytes()).is_ok());
        // identity point y = 1
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert_eq!(validate_owner_key(&identity), Err(ShortError::BadKey));
        // (0, -1): order 2
        let mut order2 = [0xffu8; 32];
        order2[0] = 0xec;
        order2[31] = 0x7f;
        assert_eq!(validate_owner_key(&order2), Err(ShortError::BadKey));
        // non-canonical alias of the identity, y = p + 1
        let mut alias = [0xffu8; 32];
        alias[0] = 0xee;
        alias[31] = 0x7f;
        assert_eq!(validate_owner_key(&alias), Err(ShortError::BadKey));
        // resolve_state applies it too (a state whose key is small-order but hashes right)
        let id = ShortHandle::derive(&identity);
        assert_eq!(
            resolve_state(&id, &minimal_state(&identity)),
            Err(ShortError::BadKey)
        );
    }

    fn client_error(kind: ErrorKind) -> anyhow::Error {
        anyhow::Error::new(WaitError::Recv(
            freenet_stdlib::client_api::ClientError::from(kind),
        ))
    }

    #[test]
    fn lookup_errors_are_told_apart() {
        let timeout = anyhow::Error::new(WaitError::Timeout("Timeout waiting for x".into()));
        assert_eq!(lookup_from_error(&timeout), Lookup::TimedOut);

        let id = *resolver_contract_key(&ShortHandle::derive(&[1u8; 32])).id();
        let missing = client_error(ErrorKind::RequestError(RequestError::ContractError(
            ContractError::MissingContract { key: id },
        )));
        assert!(matches!(lookup_from_error(&missing), Lookup::NotFound(_)));

        // A dead connection is NOT "not found".
        for dead in [
            ErrorKind::Disconnect,
            ErrorKind::ChannelClosed,
            ErrorKind::NodeUnavailable,
        ] {
            assert!(matches!(
                lookup_from_error(&client_error(dead)),
                Lookup::Error(_)
            ));
        }
        assert!(matches!(
            lookup_from_error(&anyhow!("Failed to send the x request: closed")),
            Lookup::Error(_)
        ));
    }

    #[test]
    fn each_failure_has_its_own_message() {
        let vk = [3u8; 32];
        let ed = ed25519_dalek::SigningKey::from_bytes(&vk);
        let vk = *ed.verifying_key().as_bytes();
        let id = ShortHandle::derive(&vk);
        let msg = |l: Lookup| resolve_lookup(&id, l).unwrap_err().to_string();
        let not_found = msg(Lookup::NotFound("x".into()));
        let transport = msg(Lookup::Error("connection closed".into()));
        let timed_out = msg(Lookup::TimedOut);
        let malformed = msg(Lookup::Found(b"\x02junk".to_vec()));
        let other = ed25519_dalek::SigningKey::from_bytes(&[8u8; 32]);
        let mismatch = msg(Lookup::Found(minimal_state(
            other.verifying_key().as_bytes(),
        )));
        assert!(not_found.contains("has not published"));
        assert!(
            transport.contains("could not look up") && !transport.contains("has not published")
        );
        assert!(timed_out.contains("timed out"));
        assert!(malformed.contains("malformed resolver state"));
        assert!(mismatch.contains("does not hash to this handle"));
        let all = [not_found, transport, timed_out, malformed, mismatch];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn an_existing_entry_is_not_reported_as_created() {
        assert_eq!(published_from_ack(PutAck::Stored), Some(Published::Created));
        assert_eq!(
            published_from_ack(PutAck::Merged),
            Some(Published::AlreadyPublished)
        );
        assert_eq!(published_from_ack(PutAck::NotYet), None);
    }

    /// The resolver seam in `api.rs` is outside that file's response-multiplexing pin, so pin
    /// its use here: only this module calls it, and the PUT keeps its classifier.
    #[test]
    fn the_resolver_seam_is_used_only_here_and_keeps_its_classifier() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root.clone()];
        let mut seen = 0;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    seen += 1;
                    let text = std::fs::read_to_string(&path).unwrap();
                    let name = path.file_name().unwrap().to_str().unwrap();
                    if name != "api.rs" && name != "shortkey.rs" {
                        assert!(
                            !text.contains("send_and_await("),
                            "{name} calls send_and_await; only shortkey.rs may"
                        );
                    }
                }
            }
        }
        assert!(seen >= 8, "the source walk found only {seen} files");
        let src = include_str!("shortkey.rs");
        let body = src
            .split("pub async fn short_handle_publish")
            .nth(1)
            .and_then(|b| b.split("pub async fn short_handle_resolve").next())
            .expect("short_handle_publish exists");
        let squashed: String = body.split_whitespace().collect();
        assert!(squashed.contains("classify_put_response(&contract_key,response)"));
        assert!(squashed.contains("published_from_ack("));
        assert!(squashed.contains("ResolverRequest::Put("));
    }
}
