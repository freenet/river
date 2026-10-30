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
//! - **(a) Owner-only migration.** A room migrates only when its owner
//!   converts: signs a v2 configuration (and the v1 upgrade pointer). Legacy
//!   records count only in a state holding a valid v2 configuration (window
//!   open, or rostered after it closes); otherwise the room stays on v1.
//!   There is no v1 configuration record (record tag 1 is reserved) and no
//!   path for a room whose owner never converts (Ian, 2026-10-28). A state
//!   without a configuration holds v2 records under the defaults.
//! - **(b) Window-close time.** Legacy times are capped at the hour the
//!   legacy window closed. That hour is a field of the closing configuration
//!   (`legacy_window_closed_at`), so the cap is fixed by L1. While the window
//!   is open the cap is `Clock + 24h`; with the window open and no `Clock`,
//!   legacy times are uncapped, as v2 times are without a `Clock` (a member
//!   could then pin the newest slots with far-future times, as in v1). A
//!   converting owner signs a `Clock` with its configuration, so a migrated
//!   room has one from the start.
//! - **(c) Chain depth.** `max_chain_depth` limits every link but a
//!   rostered legacy one. A legacy link the owner rosters is admitted at any
//!   depth (v1 had no depth limit), so a migrated room keeps every member
//!   the owner attests however deep its invite chain; the legacy link cap
//!   bounds their number, and so the work. An unrostered legacy link (one
//!   minted while the window is open) is held to the depth limit, so the
//!   roster that closes the window can only attest links within it. A
//!   legacy member deeper than the limit cannot invite, and a restore chain
//!   is at most `MAX_CHAIN_DEPTH` links, so a retired legacy member whose
//!   chain back to an admitted link is longer cannot be restored.
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
//!   in crafted states. v1 ids (here and in legacy ban targets) are
//!   `fast_hash`, a polynomial hash: a key colliding with a chosen member's
//!   id costs about 2^64 key generations, a colliding pair of one's own
//!   keys about 2^32. v2 records name keys, never ids.
//! - **(i) Ceilings, and what they cost a migrated room.** v1 bounded
//!   neither the message cap, nor message size, nor the DM cap, nor room
//!   name or description length; the ceilings in [`config::ceiling`] set
//!   them. Where a ceiling is below what a v1 room may hold, the excess is
//!   dropped at migration (the conversion report counts it): more than 400
//!   members or 300 legacy links (decision (q)); more than 200 messages or
//!   300 DMs (the oldest go); more than 534 secret blobs (the oldest
//!   versions' go first, so a member on a new device cannot read the oldest
//!   messages); more than 256 secret versions (messages and blobs under the
//!   oldest go); v1 ban counts above 250 owner, 250 deputy (150 per deputy)
//!   or 50 member bans (the oldest go); nicknames over 100 bytes; and v1's
//!   values of `max_recent_messages`, `max_message_size`,
//!   `max_nickname_size`, `max_room_name`, `max_room_description` above the
//!   ceilings are clamped (a v1 room name or description longer than the v2
//!   ceiling refuses the conversion, [`convert::ConvertError`]). Purge
//!   envelopes keep v1's 1,000 tokens, within a room-wide budget (t).
//! - **(j) Restore requests.** A restore request is valid if its embedded
//!   chain verifies and ends at an admitted link or the owner. Whether its
//!   first link is in fact retired is not checked: a request for a link that
//!   is not retired confers nothing. At most 10 are kept (the design: 1,000;
//!   each carries up to 25 signatures), and at most
//!   [`config::ceiling::RESTORES_PER_SUBTREE`] of those whose chain ends
//!   anywhere in the subtree of one of the owner's direct invitees, so a
//!   member signing chains of its own keys, or admitting keys of its own to
//!   sign them, cannot take every slot. The subtree root is read from L2.
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
//!   on 2026-10-26), an exception to keeping every v1 record. A legacy
//!   activity record is held to the 1,000 bytes only, not to the room's
//!   `max_message_size`: it stands for the member's activity, not a message
//!   shown.
//! - **(n) Refusal at the contract boundary.** An incoming state or delta
//!   holding more records of a kind than a valid state can hold is refused
//!   before any record is decoded: the kinds are counted from the encoded
//!   bytes, reading each record's tag and skipping the rest
//!   ([`state::within_kind_counts_encoded`]; the contract also refuses one
//!   larger than [`max_state_bytes`] before reading it). [`validate_state`] and
//!   [`apply_delta`] then refuse at the first signature check that fails:
//!   a fixpoint has none, and a generated delta has none. A record `f`
//!   drops without examining it is dropped unchecked, so the signature
//!   checks of one call are at most the records kept plus one, plus the
//!   purge recipients whose envelope does not fit the budget (t).
//! - **(o) Migrated quotas.** [`ConfigurationV2::from_v1`], the starting
//!   point of the configuration a converting owner signs, keeps the default
//!   link quotas (2 links per member, 50 children per link) and sets
//!   `max_links` and the depth limit to their ceilings. The links the owner
//!   rosters are exempt from the quotas, so v1's tree is kept whatever its
//!   shape, and the quotas bound only links made after the conversion. Every
//!   link but a rostered legacy one shares `max_links` less the roster's
//!   link count (an L1 value), so no link at any depth can push an attested
//!   legacy link out through the total cap. A group limit of L2's greedy
//!   chooser, lawful as the total cap is. A roster entry for a link not
//!   present still holds its slot.
//! - **(p) No migration without the owner.** See (a): a v1 member info's
//!   deputy list confers nothing in v2, and v1 deputies become v2 deputies
//!   only as (r) says.
//! - **(q) Legacy links capped at 300.** L2 admits at most
//!   [`config::ceiling::MAX_LEGACY_LINKS`] legacy links. Rostered ones (the
//!   owner attested them) are admitted ahead of every other link at their
//!   depth, against the roster's own count; unrostered ones, minted while
//!   the window is open, share only what the roster leaves (the cap less
//!   the roster's link count, an L1 value), so they can never displace an
//!   attested link. Each is a group limit of L2's greedy chooser over the
//!   links' own fields, the roster and shallower links, so it is lawful as
//!   the total cap is. Which 300 a larger room keeps is the owner's choice:
//!   [`convert::owner_payloads`] rosters the best ranked by depth and then
//!   newest v1 message (the member ranking), keeping each one's chain to
//!   the owner. Unrostered links within their share are admitted in BFS
//!   position order, not by activity: L2 cannot read activity (L2b). A
//!   roster entry for a link not present still holds its share. A legacy
//!   activity record counts only for a key with an admitted legacy link,
//!   so their number is bounded too. Legacy records cost more to decode,
//!   and this is what lets the member ceiling exceed 300.
//! - **(r) Deputies at migration.** The converting owner confirms the
//!   deputy list. [`convert::deputy_candidates`] proposes the owner's own v1
//!   deputies and those listed by current, unbanned members (who acted only
//!   in that member's subtree in v1, and would act room wide in v2), each
//!   with who listed it; the v2 configuration holds only the keys the owner
//!   passes in, and a `protected` list the owner chooses (a deputy ban
//!   cannot target a protected key; the Invite Bot belongs there). Nothing
//!   is promoted automatically (Ian, 2026-10-28). The bans of a v1 deputy
//!   the owner declines carry over as member bans, which act only in its
//!   own invite subtree: each candidate shows how many of its v1 bans reach
//!   outside it (what declining un-bans), and the conversion report counts
//!   such bans as narrowed, not kept. The report also lists configuration
//!   values lowered to the ceilings, secret versions beyond the cap, blobs
//!   beyond the blob cap, and links whose v1 chain does not reach the owner.
//! - **(s) Roster budget.** The roster counts distinct attested records per
//!   kind, so attesting a record again costs nothing against that kind's
//!   ceiling: twice the kind's cap (links: the legacy link cap), so the
//!   owner can attest what the room holds at conversion and again what it
//!   holds when the window closes. Records written during the window beyond
//!   that are lost when it closes. Separately, the entries of all kept
//!   batches, repeats included, are held to twice the distinct ceilings
//!   together, and at most 32 batches are kept, so repeats cost bytes and
//!   signatures that `max_state_bytes` counts.
//! - **(t) Purge token budget.** A purge envelope holds up to 1,000 tokens,
//!   as in v1, and the envelopes kept hold at most
//!   [`config::ceiling::PURGE_TOKEN_BUDGET`] (80,200, 200 per member at the
//!   member ceiling) together (Ian, 2026-10-29). For each recipient the
//!   newest envelope whose signature checks is its candidate; candidates
//!   are admitted by (token count, recipient key) within the budget, so the
//!   budget binds on the largest envelopes and a key ground to sort low
//!   gains nothing. A recipient whose envelope does not fit keeps none: an
//!   older envelope is never kept in its place (a valid state holds one
//!   envelope per recipient, so there would be none to fall back to). A
//!   group limit of the greedy chooser over the envelopes' own fields and
//!   L4, so it is lawful as the other caps are. The candidates' signatures
//!   are checked before the budget is applied, so the checks of one call
//!   are at most the records kept plus the recipients whose envelope does
//!   not fit (at most `MAX_MEMBERS + 1`), plus one. 1,000 tokens for every
//!   member would double the largest state and add about 0.5 s to the worst
//!   merge; real rooms are far below the budget (the Official room: 19
//!   envelopes of 1 to 40 tokens).

//! Not yet wired into the room contract; compiled only for tests and under
//! the `room-state-v2` feature, so the deployed contract's WASM is unchanged.

pub mod bench;
pub mod config;
pub mod convert;
pub mod encoding;
pub mod fastcbor;
pub mod legacy;
pub mod official_shaped;
pub mod records;
pub mod retain;
pub mod state;

#[cfg(test)]
mod tests;

pub use config::ConfigurationV2;
pub use records::Record;
pub use retain::{retain, Retained, Verifier};
pub use state::{apply_delta, max_state_bytes, merge, normalize, validate_state, RoomStateV2};
