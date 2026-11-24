//! A minimal mirror of the parts of Atlas's index that Discover Rooms reads
//! (freenet/river#975, step R1 of #972).
//!
//! # Why a mirror and not a dependency
//!
//! `fn-atlas-common` and `atlas-rooms` are not published, and a git dependency
//! would change River's root `Cargo.lock`, which risks re-keying the room
//! contract and chat delegate WASM. So the wire types are written out here,
//! using only crates the UI already depends on.
//!
//! # What is mirrored, and what guards it
//!
//! * The CBOR shape of `IndexState` and everything a signature covers
//!   (`RecordBody`, `KeyAuthBody`, `AppRegistryBody`). A signature is checked
//!   over the RE-SERIALISED struct, so the mirror must reproduce Atlas's
//!   encoding byte for byte, including `skip_serializing_if` on the optional
//!   `IndexEntry` fields.
//! * [`IndexParams::to_bytes`], which is the index's identity.
//! * [`IndexState::verify`]: root-signed `KeyAuth`, app registry signature and
//!   slug binding, record keying, record authorisation, record signatures.
//!   Not mirrored: the per-field structural checks (`check_structure`) and the
//!   registry version check. So this reader accepts a SUPERSET of what Atlas's
//!   `verify` accepts. That is safe only while the contract enforces those
//!   bounds on every write and the online key is the curator's; to limit what
//!   a mistake there could do, [`search`] strips control and bidirectional
//!   characters from the text it returns and truncates it to Atlas's bounds.
//! * [`search`]: `atlas_rooms::search::search`, with the same safe-search rule.
//!
//! # What guards the mirror
//!
//! The golden fixture `testdata/atlas_index_golden.json` is a STATIC snapshot:
//! a signed `IndexState` built once with the real `fn-atlas-common` and
//! `atlas-rooms` crates (atlas `origin/main` cf93c24, test keys only), plus the
//! hits `atlas_rooms::search` returned for it under eight filters. The test in
//! `mod.rs` decodes, verifies and filters those bytes and compares. It catches
//! an edit to THIS file that breaks agreement with Atlas as of that commit. It
//! does not notice a later change in Atlas: nothing regenerates or compares the
//! fixture automatically. To refresh it after an Atlas change, rebuild the
//! sample with the Atlas crates at the new commit and replace the file (the
//! generator used for cf93c24 is a throwaway `main.rs` that depends on the
//! two Atlas crates by path, builds the state with test keys, runs `verify`
//! and `search`, and prints the JSON).
//!
//! A new field on a SIGNED Atlas type that this mirror does not declare makes
//! every signature fail. That reads as "Discover unavailable" with a
//! verification error, never as an empty list.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Atlas's `MAX_ENTRIES`.
const MAX_ENTRIES: usize = 20_000;
/// Atlas's `MAX_AUTHORIZED`.
const MAX_AUTHORIZED: usize = 32;

/// The app slug River's rooms are listed under.
pub const RIVER_APP: &str = "river";
/// `IndexEntry::kind` of a listed room.
pub const ROOM_KIND: &str = "Room";
/// The `landing` value safe search lets through.
pub const LANDING_GENERAL: &str = "general";

const EXT_SNIPPET_SRC: &str = "ss";
const EXT_CONFIG_VERSION: &str = "cv";

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SubjectId(String);

impl SubjectId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Kind(String);

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Audience(String);

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Volatility(String);

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct VerifyStatus(String);

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Locator {
    Freenet {
        contract_id: String,
        path: String,
    },
    External {
        url: String,
    },
    AppResource {
        app: String,
        resource: String,
        path: String,
    },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Classification {
    pub landing: Audience,
    pub has_adult_sections: bool,
    pub volatility: Volatility,
    pub classifier: u16,
    pub classified_at: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Verification {
    pub last_verified_at: u64,
    pub status: VerifyStatus,
}

/// The optional fields are `skip_serializing_if` for the same reason Atlas has
/// it: the signed bytes of an entry that lacks them must be reproduced exactly.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct IndexEntry {
    pub subject_id: SubjectId,
    pub version: u64,
    pub kind: Kind,
    pub title: String,
    pub snippet: String,
    pub tags: Vec<String>,
    pub locator: Locator,
    pub featured: bool,
    pub added_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<Classification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<Verification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ext: Option<BTreeMap<String, String>>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Tombstone {
    pub subject_id: SubjectId,
    pub version: u64,
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum RecordBody {
    Live(IndexEntry),
    Tomb(Tombstone),
}

impl RecordBody {
    fn subject_id(&self) -> &SubjectId {
        match self {
            RecordBody::Live(e) => &e.subject_id,
            RecordBody::Tomb(t) => &t.subject_id,
        }
    }

    fn version(&self) -> u64 {
        match self {
            RecordBody::Live(e) => e.version,
            RecordBody::Tomb(t) => t.version,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct SignedRecord {
    pub body: RecordBody,
    pub by: VerifyingKey,
    pub sig: Signature,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct KeyAuthBody {
    pub version: u64,
    pub authorized: Vec<VerifyingKey>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct KeyAuth {
    pub body: KeyAuthBody,
    pub sig: Signature,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct AppRecord {
    pub contract_id: String,
    pub name: String,
    pub link_template: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct AppRegistryBody {
    pub version: u64,
    pub index_slug: String,
    pub apps: BTreeMap<String, AppRecord>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct AppRegistry {
    pub body: AppRegistryBody,
    pub sig: Signature,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct IndexState {
    pub key_auth: Option<KeyAuth>,
    pub records: BTreeMap<SubjectId, SignedRecord>,
    #[serde(default)]
    pub apps: Option<AppRegistry>,
}

/// The index's identity: `root_vk (32 bytes) || slug (UTF-8)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexParams {
    pub root_vk: VerifyingKey,
    pub slug: String,
}

impl IndexParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.slug.len());
        out.extend_from_slice(self.root_vk.as_bytes());
        out.extend_from_slice(self.slug.as_bytes());
        out
    }
}

/// Canonical bytes a signature covers: the CBOR of the value.
pub(crate) fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf).map_err(|e| format!("cannot re-encode: {e}"))?;
    Ok(buf)
}

/// Atlas verifies with `verify_strict`, which rejects small-order keys and
/// non-canonical signatures.
fn verify_sig<T: Serialize>(value: &T, sig: &Signature, vk: &VerifyingKey) -> Result<(), String> {
    vk.verify_strict(&canonical(value)?, sig)
        .map_err(|e| e.to_string())
}

/// How new an index state is: the key_auth version, then the sum of every
/// record's version. States merge per subject by highest version, so the sum
/// never drops as a state gains updates, and a lagging peer's older state
/// stamps lower.
pub type Stamp = (u64, u128);

impl IndexState {
    /// Decode the CBOR state a node serves.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        ciborium::de::from_reader::<IndexState, _>(bytes).map_err(|e| format!("{e}"))
    }

    /// Whether this state is the index named by `params` and every record in
    /// it is signed by a key the root authorised. See the module docs for what
    /// is deliberately not re-checked.
    pub fn verify(&self, params: &IndexParams) -> Result<(), String> {
        let ka = self.key_auth.as_ref().ok_or("index has no key_auth")?;
        verify_sig(&ka.body, &ka.sig, &params.root_vk)
            .map_err(|e| format!("bad key_auth signature: {e}"))?;
        if ka.body.authorized.len() > MAX_AUTHORIZED {
            return Err(format!(
                "key_auth authorizes too many keys: {}",
                ka.body.authorized.len()
            ));
        }
        if let Some(apps) = &self.apps {
            verify_sig(&apps.body, &apps.sig, &params.root_vk)
                .map_err(|e| format!("bad app registry signature: {e}"))?;
            if apps.body.index_slug != params.slug {
                return Err(format!(
                    "app registry is signed for index slug {:?}, not {:?}",
                    apps.body.index_slug, params.slug
                ));
            }
        }
        if self.records.len() > MAX_ENTRIES {
            return Err(format!("too many records: {}", self.records.len()));
        }
        for (sid, rec) in &self.records {
            if rec.body.subject_id() != sid {
                return Err("record stored under the wrong subject id".to_string());
            }
            if !ka.body.authorized.iter().any(|k| k == &rec.by) {
                return Err("record signed by an unauthorized key".to_string());
            }
            verify_sig(&rec.body, &rec.sig, &rec.by)
                .map_err(|e| format!("bad record signature: {e}"))?;
        }
        Ok(())
    }

    pub fn stamp(&self) -> Stamp {
        (
            self.key_auth.as_ref().map_or(0, |k| k.body.version),
            self.records
                .values()
                .map(|r| u128::from(r.body.version()))
                .sum(),
        )
    }

    fn live_entries(&self) -> impl Iterator<Item = &IndexEntry> {
        self.records.values().filter_map(|r| match &r.body {
            RecordBody::Live(e) => Some(e),
            RecordBody::Tomb(_) => None,
        })
    }
}

/// Who wrote a card's `snippet`. Never the owner: the owner's words are read
/// from the room itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSource {
    Curator,
    Llm,
}

/// What Atlas last learned about whether the room can be joined. Not a
/// takedown notice when it is anything but `Live`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Live,
    Unreachable,
    Stale,
    Unknown,
}

/// One card's worth of an Atlas room listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomHit {
    /// Full owner verifying key, base58: the room's identity.
    pub owner_key: String,
    pub subject_id: String,
    /// The owner's room name as Atlas last saw it.
    pub title: String,
    /// Atlas's own characterisation, or empty.
    pub snippet: String,
    pub snippet_source: Option<TextSource>,
    pub tags: Vec<String>,
    pub featured: bool,
    pub added_at: u64,
    pub availability: Availability,
    pub last_verified_at: Option<u64>,
    pub landing: Option<String>,
    pub has_adult_sections: Option<bool>,
    pub config_version: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct RoomFilter {
    pub query: Option<String>,
    pub tags: Vec<String>,
    /// Hide anything whose landing is not exactly `general`, and everything
    /// unclassified.
    pub safe_search: bool,
    pub only_live: bool,
    pub stale_after_secs: Option<u64>,
    pub now: u64,
}

impl Default for RoomFilter {
    fn default() -> Self {
        Self {
            query: None,
            tags: Vec::new(),
            safe_search: true,
            only_live: false,
            stale_after_secs: None,
            now: 0,
        }
    }
}

/// Parse an owner key the way Atlas does: base58, 32 bytes, a valid and
/// non-weak ed25519 key.
fn parse_owner(s: &str) -> Option<VerifyingKey> {
    let bytes = bs58::decode(s).into_vec().ok()?;
    let arr: [u8; 32] = bytes.as_slice().try_into().ok()?;
    let vk = VerifyingKey::from_bytes(&arr).ok()?;
    if vk.is_weak() {
        return None;
    }
    Some(vk)
}

fn owner_of_locator(loc: &Locator) -> Option<VerifyingKey> {
    match loc {
        Locator::AppResource { app, resource, .. } if app == RIVER_APP => parse_owner(resource),
        _ => None,
    }
}

fn availability(e: &IndexEntry, f: &RoomFilter) -> Availability {
    let Some(v) = &e.verified else {
        return Availability::Unknown;
    };
    match v.status.0.as_str() {
        "unreachable" => Availability::Unreachable,
        "live" => {
            let stale = f
                .stale_after_secs
                .is_some_and(|s| f.now.saturating_sub(v.last_verified_at) > s);
            if stale {
                Availability::Stale
            } else {
                Availability::Live
            }
        }
        // `changed` or an unknown future value: not presented as joinable.
        _ => Availability::Unknown,
    }
}

fn snippet_source(e: &IndexEntry) -> Option<TextSource> {
    match e.ext.as_ref()?.get(EXT_SNIPPET_SRC)?.as_str() {
        "c" => Some(TextSource::Curator),
        "l" => Some(TextSource::Llm),
        _ => None,
    }
}

fn passes_safe(h: &RoomHit) -> bool {
    // The exact permissive value only, so an unknown future landing fails closed.
    h.landing.as_deref() == Some(LANDING_GENERAL)
}

/// Atlas's `MAX_TITLE`, `MAX_SNIPPET`, `MAX_TAG_LEN` (bytes).
const MAX_TITLE: usize = 200;
const MAX_SNIPPET: usize = 500;
const MAX_TAG_LEN: usize = 40;

fn is_bidi_or_invisible(c: char) -> bool {
    matches!(c,
        '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}')
}

/// Text from the index as shown: control, bidirectional and zero-width
/// characters removed, cut to `max` bytes on a character boundary.
fn clean_text(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    for c in s.chars() {
        if c.is_control() || is_bidi_or_invisible(c) {
            continue;
        }
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

fn hit(e: &IndexEntry, f: &RoomFilter) -> Option<RoomHit> {
    if e.kind.0 != ROOM_KIND {
        return None;
    }
    // A room entry whose locator does not name a valid owner key is malformed:
    // skipped, not shown with a key nobody can use.
    let owner = owner_of_locator(&e.locator)?;
    Some(RoomHit {
        owner_key: bs58::encode(owner.to_bytes()).into_string(),
        subject_id: e.subject_id.0.clone(),
        title: clean_text(&e.title, MAX_TITLE),
        snippet: clean_text(&e.snippet, MAX_SNIPPET),
        snippet_source: snippet_source(e),
        tags: e.tags.iter().map(|t| clean_text(t, MAX_TAG_LEN)).collect(),
        featured: e.featured,
        added_at: e.added_at,
        availability: availability(e, f),
        last_verified_at: e.verified.as_ref().map(|v| v.last_verified_at),
        landing: e.class.as_ref().map(|c| c.landing.0.clone()),
        has_adult_sections: e.class.as_ref().map(|c| c.has_adult_sections),
        config_version: e
            .ext
            .as_ref()
            .and_then(|m| m.get(EXT_CONFIG_VERSION))
            .and_then(|v| v.parse().ok()),
    })
}

fn score(h: &RoomHit, q: &str) -> u8 {
    let q = q.to_lowercase();
    if h.title.to_lowercase().contains(&q) {
        3
    } else if h.tags.iter().any(|t| t.to_lowercase().contains(&q)) {
        2
    } else if h.snippet.to_lowercase().contains(&q) {
        1
    } else {
        0
    }
}

/// Live room entries matching `filter`, best first: query relevance, featured,
/// newest listing, then owner key. Duplicate listings of one owner collapse to
/// the highest-versioned entry. Equivalent to `atlas_rooms::search::search`.
pub fn search(state: &IndexState, filter: &RoomFilter) -> Vec<RoomHit> {
    let mut best: BTreeMap<String, (u64, RoomHit)> = BTreeMap::new();
    for e in state.live_entries() {
        let Some(h) = hit(e, filter) else { continue };
        match best.get(&h.owner_key) {
            Some((v, _)) if *v >= e.version => {}
            _ => {
                best.insert(h.owner_key.clone(), (e.version, h));
            }
        }
    }
    let wanted: Vec<String> = filter.tags.iter().map(|t| t.to_lowercase()).collect();
    let mut hits: Vec<(u8, RoomHit)> = best
        .into_values()
        .map(|(_, h)| h)
        .filter(|h| !filter.safe_search || passes_safe(h))
        .filter(|h| !filter.only_live || h.availability == Availability::Live)
        .filter(|h| {
            wanted
                .iter()
                .all(|w| h.tags.iter().any(|t| t.to_lowercase() == *w))
        })
        .filter_map(
            |h| match filter.query.as_deref().filter(|q| !q.is_empty()) {
                None => Some((0, h)),
                Some(q) => {
                    let s = score(&h, q);
                    (s > 0).then_some((s, h))
                }
            },
        )
        .collect();
    hits.sort_by(|(sa, a), (sb, b)| {
        sb.cmp(sa)
            .then(b.featured.cmp(&a.featured))
            .then(b.added_at.cmp(&a.added_at))
            .then(a.owner_key.cmp(&b.owner_key))
    });
    hits.into_iter().map(|(_, h)| h).collect()
}

/// Builders for tests that need a state signed with keys of their own.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub fn sk(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    pub fn owner_b58(n: u8) -> String {
        bs58::encode(sk(n).verifying_key().to_bytes()).into_string()
    }

    pub fn sid(n: u8) -> SubjectId {
        SubjectId(bs58::encode([n; 9]).into_string())
    }

    pub fn sid_str(s: &str) -> SubjectId {
        SubjectId(s.to_string())
    }

    pub fn sign<T: Serialize>(v: &T, k: &SigningKey) -> Signature {
        k.sign(&canonical(v).unwrap())
    }

    /// A general-audience, live room listing for `resource` (an owner key in
    /// base58).
    pub fn room(n: u8, version: u64, title: &str, resource: String) -> IndexEntry {
        IndexEntry {
            subject_id: sid(n),
            version,
            kind: Kind(ROOM_KIND.to_string()),
            title: title.to_string(),
            snippet: String::new(),
            tags: vec![],
            locator: Locator::AppResource {
                app: RIVER_APP.to_string(),
                resource,
                path: String::new(),
            },
            featured: false,
            added_at: 0,
            class: Some(Classification {
                landing: Audience(LANDING_GENERAL.to_string()),
                has_adult_sections: false,
                volatility: Volatility("static".to_string()),
                classifier: 1,
                classified_at: 0,
            }),
            verified: Some(Verification {
                last_verified_at: 1,
                status: VerifyStatus("live".to_string()),
            }),
            ext: None,
        }
    }

    pub struct Built {
        pub state: IndexState,
        pub params: IndexParams,
        pub root: SigningKey,
        pub online: SigningKey,
    }

    pub fn params() -> IndexParams {
        IndexParams {
            root_vk: sk(3).verifying_key(),
            slug: "default".to_string(),
        }
    }

    pub fn key_auth(root: &SigningKey, online: &[SigningKey], version: u64) -> KeyAuth {
        let body = KeyAuthBody {
            version,
            authorized: online.iter().map(|k| k.verifying_key()).collect(),
        };
        KeyAuth {
            sig: sign(&body, root),
            body,
        }
    }

    /// A verifying state: root key 3, online key 4, slug `default`.
    pub fn build(entries: Vec<IndexEntry>) -> Built {
        let root = sk(3);
        let online = sk(4);
        let mut state = IndexState {
            key_auth: Some(key_auth(&root, std::slice::from_ref(&online), 1)),
            ..IndexState::default()
        };
        for e in entries {
            let body = RecordBody::Live(e);
            let sid = body.subject_id().clone();
            state.records.insert(
                sid,
                SignedRecord {
                    sig: sign(&body, &online),
                    by: online.verifying_key(),
                    body,
                },
            );
        }
        let b = Built {
            state,
            params: params(),
            root,
            online,
        };
        b.state.verify(&b.params).expect("fixture state verifies");
        b
    }

    pub fn encode(state: &IndexState) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(state, &mut buf).unwrap();
        buf
    }
}
