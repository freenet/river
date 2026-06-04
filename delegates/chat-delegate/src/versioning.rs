//! Per-key generation + compare-and-swap (CAS) primitive for the chat
//! delegate's key-value store (freenet/river#345).
//!
//! # Why
//!
//! The plain `StoreRequest` handler is a blind last-writer-wins overwrite.
//! River persists its whole room list as a single value, so two browser
//! tabs each writing their full in-memory snapshot race: a stale tab can
//! silently destroy a room a newer tab just added. To prevent that, every
//! value written through the KV path is wrapped in a small envelope that
//! carries a monotonic **generation** counter, and the client can issue a
//! compare-and-swap store that only succeeds when its `expected_generation`
//! matches what is stored. A stale writer is rejected and re-reads + merges.
//!
//! # Envelope format
//!
//! `[ENVELOPE_TAG: u8][generation: u64 little-endian][data...]`
//!
//! The envelope is an implementation detail of the delegate: the client
//! never sees it. The plain `Get`/`Store` handlers transparently
//! unwrap/wrap it, so existing callers (e.g. the outbound-DM cache) keep
//! working and stay generation-consistent with CAS callers.
//!
//! # Testability
//!
//! In native (non-WASM) tests the delegate's `set_secret`/`get_secret`
//! host functions are no-ops, so the generation logic cannot be exercised
//! end-to-end on the host. Everything here is therefore written as pure
//! functions over `Option<&[u8]>` (the stored bytes) so it has full unit
//! coverage independent of the runtime.

/// Tag byte identifying a versioned-value envelope. Bumped only if the
/// envelope layout ever changes.
pub(crate) const ENVELOPE_TAG: u8 = 1;

/// Size of the fixed envelope header: 1 tag byte + 8 generation bytes.
const HEADER_LEN: usize = 1 + 8;

/// Encode `data` into a versioned envelope at `generation`.
pub(crate) fn encode_versioned(generation: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    out.push(ENVELOPE_TAG);
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Decode a stored value into `(generation, data)`.
///
/// A well-formed envelope returns its embedded generation and payload.
/// Anything else (too short, or a missing/unknown tag) is treated
/// defensively as un-versioned raw data at generation `0`. In the current
/// delegate every KV value is always enveloped, so the defensive branch is
/// belt-and-suspenders for forward-compatibility, not a normal path.
pub(crate) fn decode_versioned(stored: &[u8]) -> (u64, Vec<u8>) {
    if stored.len() >= HEADER_LEN && stored[0] == ENVELOPE_TAG {
        let mut gen_bytes = [0u8; 8];
        gen_bytes.copy_from_slice(&stored[1..HEADER_LEN]);
        (u64::from_le_bytes(gen_bytes), stored[HEADER_LEN..].to_vec())
    } else {
        (0, stored.to_vec())
    }
}

/// The current generation of a stored value (`0` if absent).
pub(crate) fn current_generation(stored: Option<&[u8]>) -> u64 {
    stored.map(|b| decode_versioned(b).0).unwrap_or(0)
}

/// Read a stored value as the client sees it: `(payload, generation)`.
/// A missing key reports `(None, 0)`.
pub(crate) fn read_versioned(stored: Option<&[u8]>) -> (Option<Vec<u8>>, u64) {
    match stored {
        None => (None, 0),
        Some(bytes) => {
            let (generation, data) = decode_versioned(bytes);
            (Some(data), generation)
        }
    }
}

/// Build the envelope for an *unconditional* store (plain `StoreRequest`):
/// bump the current generation by one and wrap `new_value`. Used by the
/// back-compat path so non-CAS writers still advance the generation and
/// stay consistent with CAS readers.
pub(crate) fn apply_unconditional_store(stored: Option<&[u8]>, new_value: &[u8]) -> Vec<u8> {
    let next = current_generation(stored).saturating_add(1);
    encode_versioned(next, new_value)
}

/// Outcome of a compare-and-swap store, computed purely from the stored
/// bytes and the requested write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CasOutcome {
    /// CAS matched: `bytes` is the new envelope to persist, `generation`
    /// is the post-write generation to report to the client.
    Stored { generation: u64, bytes: Vec<u8> },
    /// CAS mismatch: nothing should be written; report the current state
    /// so the client can merge and retry.
    Conflict {
        current_generation: u64,
        current_value: Option<Vec<u8>>,
    },
}

/// Compute the outcome of a compare-and-swap store.
///
/// Stores `new_value` iff `expected_generation == current_generation`
/// (where an absent key has generation `0`, so a first write uses
/// `expected_generation = 0`). On a match the generation is incremented.
pub(crate) fn apply_cas_store(
    stored: Option<&[u8]>,
    new_value: &[u8],
    expected_generation: u64,
) -> CasOutcome {
    let current = current_generation(stored);
    if expected_generation == current {
        let next = current.saturating_add(1);
        CasOutcome::Stored {
            generation: next,
            bytes: encode_versioned(next, new_value),
        }
    } else {
        let (current_value, _) = read_versioned(stored);
        CasOutcome::Conflict {
            current_generation: current,
            current_value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let data = b"hello world".to_vec();
        let enc = encode_versioned(13, &data);
        assert_eq!(enc[0], ENVELOPE_TAG);
        let (gen, dec) = decode_versioned(&enc);
        assert_eq!(gen, 13);
        assert_eq!(dec, data);
    }

    #[test]
    fn encode_empty_payload_round_trips() {
        let enc = encode_versioned(1, &[]);
        assert_eq!(enc.len(), HEADER_LEN);
        let (gen, dec) = decode_versioned(&enc);
        assert_eq!(gen, 1);
        assert!(dec.is_empty());
    }

    #[test]
    fn decode_too_short_is_treated_as_raw_gen_zero() {
        let (gen, dec) = decode_versioned(&[1, 2, 3]);
        assert_eq!(gen, 0);
        assert_eq!(dec, vec![1, 2, 3]);
    }

    #[test]
    fn decode_wrong_tag_is_treated_as_raw_gen_zero() {
        // 9+ bytes but tag byte != ENVELOPE_TAG → raw.
        let raw = vec![0xFF; 20];
        let (gen, dec) = decode_versioned(&raw);
        assert_eq!(gen, 0);
        assert_eq!(dec, raw);
    }

    #[test]
    fn read_versioned_absent_is_none_gen_zero() {
        assert_eq!(read_versioned(None), (None, 0));
    }

    #[test]
    fn read_versioned_present_unwraps() {
        let enc = encode_versioned(4, b"abc");
        assert_eq!(read_versioned(Some(&enc)), (Some(b"abc".to_vec()), 4));
    }

    #[test]
    fn unconditional_store_bumps_generation() {
        // First write onto an absent key → generation 1.
        let first = apply_unconditional_store(None, b"v1");
        assert_eq!(decode_versioned(&first), (1, b"v1".to_vec()));
        // Second write onto generation 1 → generation 2.
        let second = apply_unconditional_store(Some(&first), b"v2");
        assert_eq!(decode_versioned(&second), (2, b"v2".to_vec()));
    }

    #[test]
    fn cas_first_write_with_expected_zero_succeeds() {
        match apply_cas_store(None, b"v1", 0) {
            CasOutcome::Stored { generation, bytes } => {
                assert_eq!(generation, 1);
                assert_eq!(decode_versioned(&bytes), (1, b"v1".to_vec()));
            }
            other => panic!("expected Stored, got {other:?}"),
        }
    }

    #[test]
    fn cas_matching_generation_succeeds_and_increments() {
        let stored = encode_versioned(5, b"old");
        match apply_cas_store(Some(&stored), b"new", 5) {
            CasOutcome::Stored { generation, bytes } => {
                assert_eq!(generation, 6);
                assert_eq!(decode_versioned(&bytes), (6, b"new".to_vec()));
            }
            other => panic!("expected Stored, got {other:?}"),
        }
    }

    #[test]
    fn cas_stale_generation_conflicts_and_returns_current() {
        // The core anti-clobber guarantee: a stale writer (expected 5)
        // against a value already advanced to generation 7 is rejected,
        // and the current value is returned so it can merge + retry.
        let stored = encode_versioned(7, b"current");
        match apply_cas_store(Some(&stored), b"stale", 5) {
            CasOutcome::Conflict {
                current_generation,
                current_value,
            } => {
                assert_eq!(current_generation, 7);
                assert_eq!(current_value, Some(b"current".to_vec()));
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn cas_first_write_with_nonzero_expected_conflicts() {
        // A client that thinks the key is at generation 3 but it's
        // actually absent must be told the truth (absent → gen 0).
        match apply_cas_store(None, b"v", 3) {
            CasOutcome::Conflict {
                current_generation,
                current_value,
            } => {
                assert_eq!(current_generation, 0);
                assert_eq!(current_value, None);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
}
