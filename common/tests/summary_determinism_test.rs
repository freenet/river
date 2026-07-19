//! Determinism tests for contract `Summary` serialization.
//!
//! freenet-core byte-compares `summarize_state` output to decide peer
//! staleness (`is_stale`). A `HashMap`/`HashSet` in a `Summary` serializes in a
//! per-process-random order, so two peers holding IDENTICAL state produce
//! DIFFERENT summary bytes — the equal-summary skip never fires, and the
//! anti-entropy heartbeat fires spurious full-state heals for every room. This
//! also feeds the update-drop divergence in freenet/freenet-core#4857.
//!
//! Every `Summary` collection must therefore serialize deterministically
//! (order-independently): `BTreeMap`/`BTreeSet`/sorted `Vec`. These tests pin
//! that property by building each summary with the SAME logical contents in
//! two different insertion orders and asserting the ciborium bytes (the exact
//! bytes `summarize_state` emits and freenet-core compares) are byte-identical.
//!
//! See `.claude/rules/contract-summary-determinism.md`.

use ed25519_dalek::Signature;
use freenet_scaffold::util::FastHash;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{BanId, BansV1};
use river_core::room_state::direct_messages::{DirectMessagesSummary, SignatureBytes};
use river_core::room_state::member::{MemberId, MembersV1};
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::message::MessageId;
use river_core::room_state::secret::SecretsSummary;
use river_core::room_state::ChatRoomStateV1Summary;

// Reference the ACTUAL associated `Summary` types (not a hard-coded `BTreeSet`),
// so these tests FAIL if a field regresses back to `HashSet`/`HashMap`.
type BanSummary = <BansV1 as ComposableState>::Summary;
type MemberSummary = <MembersV1 as ComposableState>::Summary;
type MemberInfoSummary = <MemberInfoV1 as ComposableState>::Summary;

/// Serialize exactly as the room contract's `summarize_state` does.
fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(v, &mut buf).expect("ciborium serialize");
    buf
}

/// Enough distinct elements that two independently-seeded `HashSet`s would
/// (essentially) never iterate in the same order — so this test reliably
/// FAILS if a summary field regresses back to `HashSet`/`HashMap`.
const N: i64 = 24;

fn ban_id(i: i64) -> BanId {
    BanId(FastHash(i))
}
fn member_id(i: i64) -> MemberId {
    MemberId(FastHash(i))
}
fn sig(i: i64) -> Signature {
    Signature::from_bytes(&[i as u8; 64])
}

#[test]
fn bans_summary_serialization_is_order_independent() {
    // Both collect into `BansV1::Summary` (BTreeSet<BanId>).
    let s_fwd: BanSummary = (0..N).map(ban_id).collect();
    let s_rev: BanSummary = (0..N).rev().map(ban_id).collect();

    assert_eq!(
        cbor(&s_fwd),
        cbor(&s_rev),
        "ban summary must serialize identically regardless of insertion order"
    );
}

#[test]
fn members_summary_serialization_is_order_independent() {
    let s_fwd: MemberSummary = (0..N).map(member_id).collect();
    let s_rev: MemberSummary = (0..N).rev().map(member_id).collect();

    assert_eq!(
        cbor(&s_fwd),
        cbor(&s_rev),
        "member summary must serialize identically regardless of insertion order"
    );
}

#[test]
fn member_info_summary_serialization_is_order_independent() {
    // MemberInfoV1::Summary = BTreeMap<MemberId, (u32, Signature)>.
    let fwd: Vec<(MemberId, (u32, Signature))> =
        (0..N).map(|i| (member_id(i), (i as u32, sig(i)))).collect();
    let rev: Vec<(MemberId, (u32, Signature))> = fwd.iter().rev().cloned().collect();

    let s_fwd: MemberInfoSummary = fwd.into_iter().collect();
    let s_rev: MemberInfoSummary = rev.into_iter().collect();

    assert_eq!(
        cbor(&s_fwd),
        cbor(&s_rev),
        "member_info summary must serialize identically regardless of insertion order"
    );
}

#[test]
fn secrets_summary_serialization_is_order_independent() {
    let version_ids_fwd: Vec<u32> = (0..N as u32).collect();
    let version_ids_rev: Vec<u32> = (0..N as u32).rev().collect();
    let member_secrets_fwd: Vec<(u32, MemberId)> =
        (0..N).map(|i| (i as u32, member_id(i))).collect();
    let member_secrets_rev: Vec<(u32, MemberId)> =
        member_secrets_fwd.iter().rev().copied().collect();

    let s_fwd = SecretsSummary {
        current_version: N as u32,
        version_ids: version_ids_fwd.into_iter().collect(),
        member_secrets: member_secrets_fwd.into_iter().collect(),
    };
    let s_rev = SecretsSummary {
        current_version: N as u32,
        version_ids: version_ids_rev.into_iter().collect(),
        member_secrets: member_secrets_rev.into_iter().collect(),
    };

    assert_eq!(
        cbor(&s_fwd),
        cbor(&s_rev),
        "secrets summary must serialize identically regardless of insertion order"
    );
}

#[test]
fn direct_messages_summary_serialization_is_order_independent() {
    let sigs_fwd: Vec<SignatureBytes> = (0..N).map(|i| SignatureBytes([i as u8; 64])).collect();
    let sigs_rev: Vec<SignatureBytes> = sigs_fwd.iter().rev().copied().collect();
    // purge_versions is already a sorted Vec in `summarize`; identical on both.
    let purge_versions: Vec<(MemberId, u64)> = (0..N).map(|i| (member_id(i), i as u64)).collect();

    let s_fwd = DirectMessagesSummary {
        message_signatures: sigs_fwd.into_iter().collect(),
        purge_versions: purge_versions.clone(),
    };
    let s_rev = DirectMessagesSummary {
        message_signatures: sigs_rev.into_iter().collect(),
        purge_versions,
    };

    assert_eq!(
        cbor(&s_fwd),
        cbor(&s_rev),
        "direct-messages summary must serialize identically regardless of insertion order"
    );
}

/// The macro-generated top-level `ChatRoomStateV1Summary` is what
/// `summarize_state` actually serializes. It embeds every leaf summary, so this
/// asserts the whole thing is order-independent end-to-end.
#[test]
fn top_level_summary_serialization_is_order_independent() {
    fn build(reversed: bool) -> ChatRoomStateV1Summary {
        let order = |i: i64| if reversed { N - 1 - i } else { i };
        let bans = (0..N).map(|i| ban_id(order(i))).collect();
        let members = (0..N).map(|i| member_id(order(i))).collect();
        let member_info = (0..N)
            .map(|i| {
                let j = order(i);
                (member_id(j), (j as u32, sig(j)))
            })
            .collect();
        let secrets = SecretsSummary {
            current_version: N as u32,
            version_ids: (0..N as u32)
                .map(|i| if reversed { N as u32 - 1 - i } else { i })
                .collect(),
            member_secrets: (0..N)
                .map(|i| {
                    let j = order(i);
                    (j as u32, member_id(j))
                })
                .collect(),
        };
        // recent_messages is a Vec<MessageId> kept sorted by (time, id) in
        // apply_delta, so it is deterministic already; both orders use the same
        // sorted Vec.
        let recent_messages: Vec<MessageId> = (0..N).map(|i| MessageId(FastHash(i))).collect();
        let direct_messages = DirectMessagesSummary {
            message_signatures: (0..N)
                .map(|i| SignatureBytes([order(i) as u8; 64]))
                .collect(),
            purge_versions: (0..N).map(|i| (member_id(i), i as u64)).collect(),
        };

        ChatRoomStateV1Summary {
            configuration: 7,
            bans,
            members,
            member_info,
            secrets,
            recent_messages,
            direct_messages,
            upgrade: None,
            version: 3,
        }
    }

    assert_eq!(
        cbor(&build(false)),
        cbor(&build(true)),
        "top-level ChatRoomStateV1Summary must serialize identically regardless of \
         the order its elements were inserted"
    );
}
