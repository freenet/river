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
//! Not yet wired into the room contract; compiled only for tests and under
//! the `room-state-v2` feature, so the deployed contract's WASM is unchanged.

pub mod config;
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
