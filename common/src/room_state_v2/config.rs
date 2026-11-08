//! The v2 room configuration: v1's settings plus the caps and authority
//! lists the layered retention reads. Every cap has a ceiling; a
//! configuration outside them is not valid, so `f` never chooses one and the
//! state byte ceiling holds whatever the owner signs.

use super::encoding::{compact_struct, Digest, Key};
use super::records::Sealed;

/// Hours since 2026-01-01 UTC; the unit of `Clock`, activity and `clock_ref`.
pub const EPOCH_2026_SECS: u64 = 1_767_225_600;

/// The ceilings: no configuration may set a cap above these, so they bound
/// the largest valid state, and with it the work of every contract call.
/// Sized from the WASM measurement (`tools/room-state-v2-measure`) so that
/// `validate_state`, an update and the merge of two largest states each stay
/// within 2.5 s on nova under freenet-core's engine settings (the signature
/// and hash crates at opt-level 3): 400 members, of whom at most 300 hold
/// legacy (migrated v1) links, since legacy records cost more to decode;
/// the per-member counts (links, child quotas, blobs) at 4/3 of the members.
/// 400 rather than the most that fit on nova (450 measured) leaves margin
/// for slower devices such as phones (Ian, 2026-10-26).
/// The design's
/// placeholder ceilings took over 60 s and did not fit in the 256 MiB
/// contract memory. Raising them needs signature verification cheaper than
/// in-guest WASM (a later re-key). Pinned by `largest_state_at_the_ceilings`
/// and the measurement tool's `check` mode.
pub mod ceiling {
    #[cfg(not(river_v2_wide_ceilings))]
    pub const MAX_MEMBERS: u32 = 400;
    #[cfg(not(river_v2_wide_ceilings))]
    pub const MAX_LINKS: u32 = 534;
    /// Legacy links `f` admits, counted in L2's admission order. Not a
    /// configuration value: the bound every migrated room shares.
    pub const MAX_LEGACY_LINKS: u32 = 300;
    // Measurement only (`--cfg river_v2_wide_ceilings` in RUSTFLAGS, never a
    // Cargo feature, so no published build can turn it on): room for the
    // member-scaled states `bench::Caps::members` builds above the ceilings.
    #[cfg(river_v2_wide_ceilings)]
    pub const MAX_MEMBERS: u32 = 4_000;
    #[cfg(river_v2_wide_ceilings)]
    pub const MAX_LINKS: u32 = 6_000;
    pub const MAX_LINKS_PER_MEMBER: u32 = 4;
    #[cfg(not(river_v2_wide_ceilings))]
    pub const MAX_CHILDREN_PER_LINK: u32 = 534;
    #[cfg(river_v2_wide_ceilings)]
    pub const MAX_CHILDREN_PER_LINK: u32 = 6_000;
    pub const MAX_CHAIN_DEPTH: u32 = 8;
    pub const MAX_OWNER_BANS: u32 = 250;
    pub const MAX_DEPUTY_BANS: u32 = 250;
    pub const MAX_BANS_PER_DEPUTY: u32 = 150;
    pub const MAX_MEMBER_BANS: u32 = 50;
    pub const MAX_BANS_PER_ISSUER: u32 = 10;
    #[cfg(not(river_v2_wide_ceilings))]
    pub const MAX_SECRET_BLOBS: u32 = 534;
    #[cfg(river_v2_wide_ceilings)]
    pub const MAX_SECRET_BLOBS: u32 = 6_000;
    pub const MAX_SECRET_VERSIONS: u32 = 256;
    pub const MAX_RECENT_MESSAGES: u32 = 200;
    pub const MAX_MESSAGE_SIZE: u32 = 2_000;
    pub const MAX_DIRECT_MESSAGES: u32 = 300;
    pub const MAX_ROOM_DESCRIPTION: u32 = 2_000;
    pub const MAX_ROOM_NAME: u32 = 200;
    pub const MAX_NICKNAME_SIZE: u32 = 100;
    pub const AUTHORITY_KEYS: usize = 64;
    pub const CHILD_EXCEPTIONS: usize = 64;
    /// v1's per-pair DM cap.
    pub const DMS_PER_PAIR: usize = 100;
    /// v1 allowed 32 KiB.
    pub const DM_CIPHERTEXT: usize = 4_096;
    /// As v1 (`MAX_PURGED_TOMBSTONES_PER_RECIPIENT`).
    pub const PURGE_TOKENS: usize = 1_000;
    /// A legacy purge envelope's tokens, as v1.
    pub const LEGACY_PURGE_TOKENS: usize = 1_000;
    /// Tokens of all kept purge envelopes together (v2 and legacy): 200 per
    /// member at the member ceiling, so 1,000-token envelopes cost no more
    /// than 200-token ones did (decision (t)).
    pub const PURGE_TOKEN_BUDGET: usize = 80_200;
    /// A purge envelope's bytes besides its tokens, and per token (pinned by
    /// `tests::sizes`).
    pub const PURGE_ENVELOPE_BYTES: usize = 400;
    pub const PURGE_TOKEN_BYTES: usize = 18;
    pub const SECRET_BLOB_CIPHERTEXT: usize = 512;
    pub const BAN_REASON: usize = 256;
    /// Roster batches kept. The distinct entries the roster may hold
    /// (`retain::roster_ceiling`, 5,672 at the ceilings) fill 6 batches of
    /// 1,000, attested at conversion and again at window close, so 32
    /// leaves room for partial batches.
    pub const ROSTER_BATCHES: usize = 32;
    pub const ROSTER_BATCH_ENTRIES: usize = 1_000;
    /// Restore requests carry chains of up to `MAX_CHAIN_DEPTH` links of
    /// three signatures each, so 10 (the design: 1,000) keeps their checks
    /// within the timing bound.
    pub const RESTORE_REQUESTS: usize = 10;
    /// Restore requests attached within one subtree (see `retain`).
    pub const RESTORES_PER_SUBTREE: usize = 2;
    /// Links other than rostered legacy ones within one subtree: a root is
    /// one of the owner's direct invitees, or a child of a link holding a
    /// child exception. Bounds what one member can force the room to check.
    pub const MAX_LINKS_PER_SUBTREE: u32 = 100;
    /// Legacy v1 records encode byte vectors as CBOR arrays, which cost
    /// about four times as much to decode in WASM as a v2 byte string, so the
    /// legacy kinds that carry content have smaller bounds than their v2
    /// counterparts: a legacy message or activity record holds at most 1,000
    /// bytes of content (v1's default `max_message_size`; v2: 2,000) and a
    /// legacy DM at most 2,560 bytes of ciphertext (v2: 4,096). The Official
    /// room's largest message on 2026-10-26 was 613 bytes and its largest DM
    /// 2,179. Measured with the opt-level 3 build, largest legacy-heavy state,
    /// update and untrusted merge against the 2.5 s bound: 1,000 / 2,560:
    /// 1.8 to 2.0 s; 1,000 / 4,096: 2.1 to 2.9 s; 1,250 / 4,096: 2.2 to
    /// 2.3 s; 1,500 / 3,072: 2.1 to 2.2 s; 1,500 / 4,096: 2.4 to 2.9 s;
    /// 2,000 / 2,560: 3.2 to 4.3 s; 2,000 / 4,096: 2.5 to 2.8 s.
    pub const LEGACY_MESSAGE_SIZE: usize = 1_000;
    pub const LEGACY_DM_CIPHERTEXT: usize = 2_560;
}

compact_struct! {
    /// Owner-signed; the highest valid `version` is the room's configuration.
    pub struct ConfigurationV2 {
        pub version: u32,
        /// 0 public, 1 private.
        pub privacy_mode: u8,
        pub display_name: Sealed,
        pub display_description: Option<Sealed>,
        pub max_room_name: u32,
        pub max_room_description: u32,
        pub max_recent_messages: u32,
        pub max_message_size: u32,
        pub max_nickname_size: u32,
        pub max_members: u32,
        pub max_direct_messages: u32,
        pub max_chain_depth: u32,
        pub max_links: u32,
        pub max_links_per_member: u32,
        pub max_children_per_link: u32,
        /// The owner's own child quota.
        pub max_owner_children: u32,
        /// Per-link child quotas (sorted by link, no duplicates).
        pub child_exceptions: Vec<(Digest, u32)>,
        pub max_owner_bans: u32,
        pub max_deputy_bans: u32,
        pub max_bans_per_deputy: u32,
        pub max_member_bans: u32,
        pub max_bans_per_issuer: u32,
        /// Sorted, no duplicates, never the owner.
        pub deputies: Vec<Key>,
        /// Sorted, no duplicates.
        pub protected: Vec<Key>,
        pub max_secret_versions: u32,
        pub max_secret_blobs: u32,
        pub legacy_window_open: bool,
        /// The `Clock` hours at which the window closed (read only while
        /// `legacy_window_open` is false): legacy times are capped at it.
        pub legacy_window_closed_at: u32,
        /// Hours; read by the owner's maintenance client, not by `f`.
        pub link_retention: u32,
        /// Hours.
        pub member_ban_activity_window: u32,
    }
}

impl ConfigurationV2 {
    /// The defaults of the design's bounds table for a room of
    /// `max_members`, as used when a room has no configuration at all.
    pub fn defaults(max_members: u32) -> Self {
        use ceiling::*;
        let max_members = max_members.min(MAX_MEMBERS);
        ConfigurationV2 {
            version: 0,
            privacy_mode: 0,
            display_name: Sealed::Public(Default::default()),
            display_description: None,
            max_room_name: 100,
            max_room_description: 500,
            max_recent_messages: 100,
            max_message_size: 1_000,
            max_nickname_size: 50,
            max_members,
            max_direct_messages: 300,
            max_chain_depth: 6,
            max_links: (2 * max_members).min(MAX_LINKS),
            max_links_per_member: 2,
            max_children_per_link: 50,
            max_owner_children: MAX_CHILDREN_PER_LINK,
            child_exceptions: vec![],
            max_owner_bans: 200,
            max_deputy_bans: 200,
            max_bans_per_deputy: 100,
            max_member_bans: MAX_MEMBER_BANS,
            max_bans_per_issuer: MAX_BANS_PER_ISSUER,
            deputies: vec![],
            protected: vec![],
            max_secret_versions: MAX_SECRET_VERSIONS,
            max_secret_blobs: (4 * max_members).min(MAX_SECRET_BLOBS),
            legacy_window_open: true,
            legacy_window_closed_at: 0,
            link_retention: 365 * 24,
            member_ban_activity_window: 30 * 24,
        }
    }

    /// The starting point of the configuration a converting owner signs:
    /// each v1 value clamped to its v2 ceiling (min(v1 value, ceiling)), the
    /// defaults for everything v1 lacks (the link quotas among them),
    /// `max_links` and the depth limit at their ceilings, and the legacy
    /// window open. Ban caps take the v1
    /// `max_user_bans`, clamped, so no v1 ban is dropped by a cap the
    /// ceilings allow keeping.
    pub fn from_v1(v1: &crate::room_state::configuration::Configuration) -> Self {
        use ceiling::*;
        let clamp = |v: usize, c: u32| (v.min(c as usize)) as u32;
        let mut c = Self::defaults(clamp(v1.max_members, MAX_MEMBERS));
        c.version = 0;
        c.privacy_mode = match v1.privacy_mode {
            crate::room_state::privacy::PrivacyMode::Public => 0,
            crate::room_state::privacy::PrivacyMode::Private => 1,
        };
        c.max_recent_messages = clamp(v1.max_recent_messages, MAX_RECENT_MESSAGES);
        c.max_message_size = clamp(v1.max_message_size, MAX_MESSAGE_SIZE);
        c.max_nickname_size = clamp(v1.max_nickname_size, MAX_NICKNAME_SIZE);
        c.max_room_name = clamp(v1.max_room_name, MAX_ROOM_NAME);
        c.max_room_description = clamp(v1.max_room_description, MAX_ROOM_DESCRIPTION);
        c.max_direct_messages = clamp(v1.effective_max_direct_messages(), MAX_DIRECT_MESSAGES);
        c.max_owner_bans = clamp(v1.max_user_bans, MAX_OWNER_BANS);
        c.max_deputy_bans = clamp(v1.max_user_bans, MAX_DEPUTY_BANS);
        c.max_bans_per_deputy = clamp(v1.max_user_bans, MAX_BANS_PER_DEPUTY);
        c.max_member_bans = clamp(v1.max_user_bans, MAX_MEMBER_BANS);
        // The link quotas stay at the defaults: the links the owner rosters
        // are exempt from them, so they bound only links made after the
        // conversion. The total and the depth limit take their ceilings, so
        // the roster's links fit and a v1 member deep in its chain can still
        // invite (decision (o)).
        c.max_links = MAX_LINKS;
        c.max_chain_depth = MAX_CHAIN_DEPTH;
        c
    }

    /// Whether every value is within its ceiling and every list is sorted
    /// without duplicates. Only valid configurations compete.
    pub fn check(&self, owner: &Key) -> bool {
        use ceiling::*;
        let sorted_unique = |v: &[Key]| v.windows(2).all(|w| w[0] < w[1]);
        let within = [
            (self.max_members, MAX_MEMBERS),
            (self.max_links, MAX_LINKS),
            (self.max_links_per_member, MAX_LINKS_PER_MEMBER),
            (self.max_children_per_link, MAX_CHILDREN_PER_LINK),
            (self.max_owner_children, MAX_CHILDREN_PER_LINK),
            (self.max_chain_depth, MAX_CHAIN_DEPTH),
            (self.max_owner_bans, MAX_OWNER_BANS),
            (self.max_deputy_bans, MAX_DEPUTY_BANS),
            (self.max_bans_per_deputy, MAX_BANS_PER_DEPUTY),
            (self.max_member_bans, MAX_MEMBER_BANS),
            (self.max_bans_per_issuer, MAX_BANS_PER_ISSUER),
            (self.max_secret_blobs, MAX_SECRET_BLOBS),
            (self.max_secret_versions, MAX_SECRET_VERSIONS),
            (self.max_recent_messages, MAX_RECENT_MESSAGES),
            (self.max_message_size, MAX_MESSAGE_SIZE),
            (self.max_direct_messages, MAX_DIRECT_MESSAGES),
            (self.max_room_description, MAX_ROOM_DESCRIPTION),
            (self.max_room_name, MAX_ROOM_NAME),
            (self.max_nickname_size, MAX_NICKNAME_SIZE),
        ]
        .iter()
        .all(|(v, c)| v <= c);
        // The v1 settings v1 refused at zero stay refused at zero.
        let nonzero = [
            self.max_recent_messages,
            self.max_message_size,
            self.max_nickname_size,
            self.max_members,
            self.max_room_name,
            self.max_room_description,
            self.max_direct_messages,
        ]
        .iter()
        .all(|&v| v > 0);
        within
            && nonzero
            && self.privacy_mode <= 1
            && !(self.privacy_mode == 1 && matches!(self.display_name, Sealed::Public(_)))
            && self.display_name.size_ok()
            && self
                .display_description
                .as_ref()
                .is_none_or(|d| d.size_ok())
            && self.display_name.declared_len() <= self.max_room_name as usize
            && self
                .display_description
                .as_ref()
                .is_none_or(|d| d.declared_len() <= self.max_room_description as usize)
            && self.deputies.len() <= AUTHORITY_KEYS
            && self.protected.len() <= AUTHORITY_KEYS
            && sorted_unique(&self.deputies)
            && sorted_unique(&self.protected)
            && !self.deputies.contains(owner)
            && self.child_exceptions.len() <= CHILD_EXCEPTIONS
            && self.child_exceptions.windows(2).all(|w| w[0].0 < w[1].0)
            && self
                .child_exceptions
                .iter()
                .all(|(_, q)| *q <= MAX_CHILDREN_PER_LINK)
    }

    /// Whether the owner set a child limit for this link.
    pub fn has_child_exception(&self, link: &Digest) -> bool {
        self.child_exceptions
            .binary_search_by(|(d, _)| d.cmp(link))
            .is_ok()
    }

    pub fn child_quota(&self, link: &Digest) -> u32 {
        match self.child_exceptions.binary_search_by(|(d, _)| d.cmp(link)) {
            Ok(i) => self.child_exceptions[i].1,
            Err(_) => self.max_children_per_link,
        }
    }
}
