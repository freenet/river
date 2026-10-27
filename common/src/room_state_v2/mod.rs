//! River room state v2: a room state that is merge-lawful by construction.
//!
//! A state is a set of signed records. `merge(A, B) = f(A ∪ B)` and
//! `apply_delta(S, D) = f(S ∪ D)`, where `f` ([`retain::retain`]) runs these
//! layers in order, each reading only the layers above it:
//!
//! | layer | records | reads |
//! |---|---|---|
//! | L0 | the owner key (contract parameters) | |
//! | L1 | configuration, `Clock`, link checkpoint, upgrade pointer, legacy roster, secret versions | own records |
//! | L1b | owner bans | L1 |
//! | L1c | deputy bans | L1, L1b |
//! | L2 | links (invite proofs), depth by depth | L1, L1b, L1c, shallower links |
//! | L2b | activity, restore requests | L1, L2 |
//! | L3 | member bans | L1, L2, L2b |
//! | L4 | members (derived entries and the member cap) | L1 to L3 |
//! | L5 | member info, messages, DMs, purges, secret blobs | L1, L4 |
//!
//! A record a layer admits stays in the output; nothing is kept because
//! something depends on it, and removals cascade downward only.
//! `validate_state` requires `f(S) == S`, so every state the contract
//! accepts is one `f` emits.
//!
//! # Decisions taken in implementation
//!
//! Each choice below is a rule of `f` or of the encoding that a reviewer
//! should read as deliberate. Every one is lawful by construction (it reads
//! only the record's own bytes and the layers above it).
//!
//! - **(a) Legacy configuration.** A room whose owner never signs a v2
//!   configuration still needs its v1 settings, so the owner's v1
//!   configuration is carried as [`Record::LegacyConfiguration`] at L1. It
//!   counts only while no valid v2 configuration exists, and maps to v2 by
//!   [`ConfigurationV2::from_v1`]. Without either, the defaults apply.
//! - **(b) Window-close time.** Legacy times are capped at the hour the
//!   legacy window closed. That hour is a field of the closing configuration
//!   (`legacy_window_closed_at`), so the cap is fixed by L1. While the window
//!   is open the cap is `Clock + 24h`; with the window open and no `Clock`,
//!   legacy times are uncapped, as v2 times are without a `Clock`.
//! - **(c) Chain depth.** `max_chain_depth` limits accepted links only.
//!   Legacy links are limited by the depth ceiling, so a migrated room keeps
//!   every v1 member however deep its invite chain.
//! - **(d) Member-ban issuer key.** A v2 member ban names its issuing link
//!   and that link's member key, and is valid only if the two agree. The
//!   signature can then be checked without the rest of the state, which the
//!   contract-boundary refusal of a bad delta needs.
//! - **(e) Named legacy signer.** Every legacy record names the key that
//!   signed it, including a legacy link's inviter, and the key must hash to
//!   the v1 id the record claims. `f` never searches keys by their 64-bit id,
//!   so a colliding key costs no failed signature check.
//! - **(f) Roster entries carry their kind.** A roster batch lists
//!   `(kind, digest)` pairs, so the per-kind roster ceilings can be counted
//!   from the batch alone.
//! - **(g) Blob precedence.** Blobs are admitted by (version descending, v2
//!   before legacy, recipient bytes, signature). Putting v2 blobs first at
//!   each version is what makes "a v2 blob for a key takes precedence over a
//!   legacy blob that matches it" a greedy rule over admitted blobs.
//! - **(h) Legacy DM pairs.** A legacy DM's per-pair group is (sender,
//!   recipient's 64-bit id), and it is dropped if the purge envelope of any
//!   kept key matching that id lists its token. Two keys match one id only
//!   in crafted states.
//! - **(i) Ceilings v1 never had.** v1 bounded neither the message cap, nor
//!   message size, nor the DM cap, nor room name or description length; the
//!   ceilings in [`config::ceiling`] set them.
//! - **(j) Restore requests.** A restore request is valid if its embedded
//!   chain verifies and ends at an admitted link or the owner. Whether its
//!   first link is in fact retired is not checked: a request for a link that
//!   is not retired confers nothing.
//! - **(k) Strict signatures everywhere.** Legacy v1 records are checked with
//!   `verify_strict`, as v2 records are, though v1 used `verify`. A v1 record
//!   whose signature only the lenient check accepts is dropped.
//! - **(l) Size bounds that follow from the byte ceiling.** A sealed value's
//!   ciphertext may exceed its declared length by at most the 16-byte AEAD
//!   tag, and a legacy member info lists at most v1's 64 deputies. Without
//!   these, a record's declared limits would not bound its size.
//! - **(m) Legacy content bounds.** A legacy message or activity record holds
//!   at most 1,000 bytes of content and a legacy DM at most 2,560 bytes of
//!   ciphertext ([`config::ceiling::LEGACY_MESSAGE_SIZE`]), below the v2
//!   bounds of 2,000 and 4,096. v1 encodes bytes as CBOR arrays, which are
//!   slow to decode in WASM; at the v2 bounds the largest legacy-heavy state
//!   exceeds the 2.5 s timing bound. A migrated room therefore loses v1
//!   messages over 1,000 bytes and DMs over 2,560 (none in the Official room
//!   on 2026-10-26), an exception to keeping every v1 record.
//! - **(n) Delta refusal.** [`apply_delta`] refuses a delta holding a record
//!   whose signature fails. It checks after running `f`, memoized by digest,
//!   so a record `f` already verified is not decoded or checked again.
//! - **(o) Migrated quotas.** [`ConfigurationV2::from_v1`], which decides a
//!   migrated room's configuration (the owner's starting point, and the
//!   configuration of a room whose owner never converts), sets the link
//!   quotas and the depth limit to their ceilings. v1 had neither, so a v1
//!   inviter's children are never dropped by a quota v1 could not have
//!   known about. This matters most for a room whose owner never converts:
//!   its legacy links are unrostered, and unrostered links count against
//!   quotas.
//! - **(p) Owner-appointed v1 deputies.** v1 keeps deputy lists in member
//!   info: the owner's own list is owner-appointed, every other list is
//!   member-appointed, and the v1 configuration names none. In a room with
//!   no valid v2 configuration, L1 also chooses the owner's own legacy member
//!   info (highest version, ties by lower signature; window open or
//!   rostered) and keeps it, and its deputy list names deputies by v1 id: a
//!   key whose id it lists, and that no owner ban targets, is a deputy at
//!   L1c, and a deputy ban may not target it. Member-appointed lists confer
//!   nothing (decision Q6). L1 keeps the record it read even when L5 prefers
//!   another owner member info, which tiny family U7 pins.
//! - **(q) Legacy links capped at 300.** L2 admits at most
//!   [`config::ceiling::MAX_LEGACY_LINKS`] legacy links, rostered or not,
//!   counted in its admission order (depth, parent position, rostered legacy
//!   first, seq, digest), which is also the member ranking's depth-first
//!   order. It is one more group limit of L2's greedy chooser, reading only
//!   the links' own fields and shallower links, so it is lawful as the total
//!   cap is. A legacy activity record counts only for a key with an admitted
//!   legacy link, so their number is bounded too. Legacy records cost more to
//!   decode, and this is what lets the member ceiling exceed 300.
//!
//! Not yet wired into the room contract; compiled only for tests and under
//! the `room-state-v2` feature, so the deployed contract's WASM is unchanged.

pub mod bench;
pub mod config;
pub mod convert;
pub mod encoding;
pub mod legacy;
pub mod records;
pub mod retain;
pub mod state;

#[cfg(test)]
mod tests;

pub use config::ConfigurationV2;
pub use records::Record;
pub use retain::{retain, Retained, Verifier};
pub use state::{apply_delta, max_state_bytes, merge, normalize, validate_state, RoomStateV2};
