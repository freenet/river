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
use river_core::room_state::direct_messages::{
    DirectMessagesSummary, DmOrderKey, DmPairHorizon, DmRetentionHorizon, SignatureBytes,
};
use river_core::room_state::member::{MemberId, MembersV1};
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::message::{
    MessageId, MessageOrderKey, MessagesSummary, MessagesV1, RetentionHorizon,
};
use river_core::room_state::secret::SecretsSummary;
use river_core::room_state::ChatRoomStateV1Summary;
use std::time::{Duration, SystemTime};

// Reference the ACTUAL associated `Summary` types (not a hard-coded `BTreeSet`),
// so these tests FAIL if a field regresses back to `HashSet`/`HashMap`.
type BanSummary = <BansV1 as ComposableState>::Summary;
type MemberSummary = <MembersV1 as ComposableState>::Summary;
type MemberInfoSummary = <MemberInfoV1 as ComposableState>::Summary;
type MessageSummary = <MessagesV1 as ComposableState>::Summary;

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
fn messages_summary_serialization_is_order_independent() {
    let ids_fwd: Vec<MessageId> = (0..N).map(|i| MessageId(FastHash(i))).collect();
    let ids_rev: Vec<MessageId> = ids_fwd.iter().rev().cloned().collect();
    // The horizon is a single key, identical on both sides — only the id
    // collection can be order-sensitive.
    let horizon = RetentionHorizon::OldestRetained(MessageOrderKey {
        time: SystemTime::UNIX_EPOCH + Duration::from_secs(42),
        id: MessageId(FastHash(0)),
    });

    let s_fwd = MessageSummary {
        message_ids: ids_fwd.into_iter().collect(),
        horizon: horizon.clone(),
    };
    let s_rev = MessageSummary {
        message_ids: ids_rev.into_iter().collect(),
        horizon,
    };

    assert_eq!(
        cbor(&s_fwd),
        cbor(&s_rev),
        "messages summary must serialize identically regardless of insertion order"
    );
}

/// The id half of the messages summary is a `BTreeSet`, so it canonicalises on
/// its own. This pins the OTHER half: [`MessagesV1::retention_horizon`] must not
/// depend on where in `self.messages` the oldest message happens to sit.
/// Computing it by INDEX (e.g. `messages[len - max]`) would read a different key
/// out of a differently-ordered state and diverge the summary bytes; taking the
/// minimum does not.
#[test]
fn messages_horizon_is_independent_of_state_vec_order() {
    use ed25519_dalek::SigningKey;
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};

    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let owner_id = MemberId(FastHash(0));
    let mut messages = MessagesV1::default();
    for i in 0..N as u64 {
        messages.messages.push(AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: owner_id,
                time: SystemTime::UNIX_EPOCH + Duration::from_secs(100 + i),
                content: RoomMessageBody::public(format!("m{i}")),
            },
            &sk,
        ));
    }

    // At capacity, so the horizon is a real key rather than `Open`.
    let horizon_fwd = messages.retention_horizon(N as usize);
    messages.messages.reverse();
    let horizon_rev = messages.retention_horizon(N as usize);

    assert!(
        matches!(horizon_fwd, RetentionHorizon::OldestRetained(_)),
        "test premise: {} messages against a cap of {} must be at capacity",
        N,
        N
    );
    assert_eq!(
        horizon_fwd, horizon_rev,
        "the same held messages in a different Vec order must produce the same horizon"
    );
}

#[test]
fn direct_messages_summary_serialization_is_order_independent() {
    let sigs_fwd: Vec<SignatureBytes> = (0..N).map(|i| SignatureBytes([i as u8; 64])).collect();
    let sigs_rev: Vec<SignatureBytes> = sigs_fwd.iter().rev().copied().collect();
    // purge_versions is already a sorted Vec in `summarize`; identical on both.
    let purge_versions: Vec<(MemberId, u64)> = (0..N).map(|i| (member_id(i), i as u64)).collect();

    // pair_horizons is already a sorted Vec in `summarize`; identical on both.
    let pair_horizons: Vec<DmPairHorizon> = (0..N)
        .map(|i| DmPairHorizon {
            sender: member_id(i),
            recipient: member_id(i + 1),
            oldest_retained: DmOrderKey {
                timestamp: i as u64,
                signature: SignatureBytes([i as u8; 64]),
            },
        })
        .collect();

    // global_horizon is a single value derived from the held set, so it is
    // identical on both sides; carried here so the field is covered by the
    // serialization comparison rather than defaulted away.
    let global_horizon = DmRetentionHorizon::OldestRetained(DmOrderKey {
        timestamp: 7,
        signature: SignatureBytes([7u8; 64]),
    });

    let s_fwd = DirectMessagesSummary {
        message_signatures: sigs_fwd.into_iter().collect(),
        purge_versions: purge_versions.clone(),
        pair_horizons: pair_horizons.clone(),
        global_horizon: global_horizon.clone(),
    };
    let s_rev = DirectMessagesSummary {
        message_signatures: sigs_rev.into_iter().collect(),
        purge_versions,
        pair_horizons,
        global_horizon,
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
        // recent_messages carries a BTreeSet of ids plus a retention horizon;
        // the BTreeSet canonicalises whatever order the ids arrive in.
        let recent_messages = MessagesSummary {
            message_ids: (0..N).map(|i| MessageId(FastHash(order(i)))).collect(),
            horizon: RetentionHorizon::OldestRetained(MessageOrderKey {
                time: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
                id: MessageId(FastHash(0)),
            }),
        };
        let direct_messages = DirectMessagesSummary {
            message_signatures: (0..N)
                .map(|i| SignatureBytes([order(i) as u8; 64]))
                .collect(),
            purge_versions: (0..N).map(|i| (member_id(i), i as u64)).collect(),
            pair_horizons: (0..N)
                .map(|i| DmPairHorizon {
                    sender: member_id(i),
                    recipient: member_id(i + 1),
                    oldest_retained: DmOrderKey {
                        timestamp: i as u64,
                        signature: SignatureBytes([i as u8; 64]),
                    },
                })
                .collect(),
            global_horizon: DmRetentionHorizon::OldestRetained(DmOrderKey {
                timestamp: 1,
                signature: SignatureBytes([1u8; 64]),
            }),
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
