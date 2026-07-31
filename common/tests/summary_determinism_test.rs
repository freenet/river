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

use ed25519_dalek::{Signature, SigningKey};
use freenet_scaffold::util::FastHash;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{BanId, BansV1};
use river_core::room_state::direct_messages::{
    DirectMessagesSummary, DmOrderKey, DmPairHorizon, DmRetentionHorizon, SignatureBytes,
};
use river_core::room_state::member::{MemberId, MembersV1};
use river_core::room_state::member_info::{
    AuthorizedMemberInfo, MemberInfo, MemberInfoV1, SigDigest,
};
use river_core::room_state::message::{
    MessageId, MessageOrderKey, MessagesSummary, MessagesV1, RetentionHorizon,
};
use river_core::room_state::secret::SecretsSummary;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Summary};
use std::collections::BTreeMap;
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
/// A distinct signature digest per index, matching the `(u32, SigDigest)` shape
/// the member_info summary carries since freenet/river#571. The tests using this
/// exercise ORDER-independence of the serialized summary, so only distinctness
/// matters here, not that these equal any real `blake3` digest. (The real
/// digest's value and encoding are pinned separately by `sig_digest_golden_vector`
/// in `river_core::room_state::member_info`.)
fn sig_rank(i: i64) -> SigDigest {
    // Spread the bits so a byte-order bug in the summary encoding shows up as a
    // difference rather than cancelling out across entries.
    let a = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let b = (i as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.to_le_bytes());
    out[8..].copy_from_slice(&b.to_le_bytes());
    SigDigest(out)
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
    // MemberInfoV1::Summary = BTreeMap<MemberId, (u32, SigDigest)> — the digest
    // replaced a raw 64-byte Signature in freenet/river#571.
    let fwd: Vec<(MemberId, (u32, SigDigest))> = (0..N)
        .map(|i| (member_id(i), (i as u32, sig_rank(i))))
        .collect();
    let rev: Vec<(MemberId, (u32, SigDigest))> = fwd.iter().rev().cloned().collect();

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
                (member_id(j), (j as u32, sig_rank(j)))
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

/// freenet/river#571 — the member_info summary must stay SMALL.
///
/// This is the whole point of that change and it has no other guard: the summary
/// is re-sent to every interested peer on every state change, and
/// `interest_sync_summaries` was measured as the largest single consumer of
/// outbound bytes on the Freenet network. The entry previously carried a raw
/// ed25519 `Signature`, which is ~92% of it.
///
/// THE BASELINE IS MEASURED HERE, NOT ASSERTED IN PROSE. Both the issue and the
/// first draft of this change quoted 66 bytes for the signature, which is the
/// encoding of River's OWN `SignatureBytes` newtype (a CBOR byte string). It is
/// NOT what `ed25519::Signature` does: `ed25519`'s `Serialize` calls
/// `serialize_tuple(64)`, ciborium maps that to a CBOR ARRAY, and a random byte
/// costs 2 bytes there whenever it is >= 24 — so the real figure is ~124. The
/// wrong number survived an issue, a PR body, a review, and a round of review
/// fixes, so the old shape is now rebuilt from the SAME records and measured
/// alongside the new one. A derived figure is not evidence.
///
/// REALISM MATTERS HERE, because the number this test reports is quoted as the
/// production figure:
///
/// - `MemberId`s come from REAL `VerifyingKey`s, so their `FastHash` values span
///   the `i64` range and CBOR-encode in the ~9 bytes production sees. The obvious
///   shortcut, `MemberId(FastHash(i))` for small `i`, encodes in 1-3 bytes and
///   understates the entry by ~30%.
/// - The summary is produced by calling the REAL `summarize()` on built state,
///   not by hand-constructing `MemberInfoV1::Summary`. Hand-construction can only
///   catch a change to the type's SHAPE, which would be a compile error anyway;
///   it cannot catch `summarize` starting to populate the map differently.
/// - Versions are small (1-3), matching production: a member's `version`
///   increments only when they edit their nickname or deputies, so it is a
///   single CBOR byte for nearly every real record.
///
/// Keys are derived from fixed seeds, so the measurement is exactly reproducible
/// and the assertion cannot flake.
///
/// Asserted as bytes-per-entry rather than a total, so the bound does not need
/// revising when the member count changes.
#[test]
fn member_info_summary_stays_small_per_entry() {
    const MEMBERS: u64 = 470; // the official room's rough membership

    // Deterministic but REAL keys: blake3 of the index seeds the signing key, so
    // the derived MemberId is a full-width FastHash exactly as in production.
    let signing_keys: Vec<SigningKey> = (0..MEMBERS)
        .map(|i| SigningKey::from_bytes(blake3::hash(&i.to_le_bytes()).as_bytes()))
        .collect();

    let member_info: Vec<AuthorizedMemberInfo> = signing_keys
        .iter()
        .enumerate()
        .map(|(i, sk)| {
            let info = MemberInfo::new_public(
                MemberId::from(&sk.verifying_key()),
                1 + (i % 3) as u32,
                format!("member{i}"),
            );
            AuthorizedMemberInfo::new_with_member_key(info, sk)
        })
        .collect();

    let state = MemberInfoV1 { member_info };
    let parent = ChatRoomStateV1::default();
    let parameters = ChatRoomParametersV1 {
        owner: signing_keys[0].verifying_key(),
    };

    // The REAL summarize(), so a change in how it populates the map is caught.
    let summary: MemberInfoSummary = state.summarize(&parent, &parameters);
    assert_eq!(
        summary.len() as u64,
        MEMBERS,
        "precondition: one summary entry per member"
    );

    let bytes = cbor(&summary).len();
    let per_entry = bytes as f64 / MEMBERS as f64;

    // The PRE-CHANGE shape, rebuilt from the same records: the summary value was
    // the raw `Signature` rather than a digest of it. Everything else about the
    // entry is identical, so the difference is exactly what this change bought.
    let before: BTreeMap<MemberId, (u32, Signature)> = state
        .member_info
        .iter()
        .map(|r| {
            (
                r.member_info.member_id,
                (r.member_info.version, r.signature),
            )
        })
        .collect();
    let before_bytes = cbor(&before).len();
    let before_per_entry = before_bytes as f64 / MEMBERS as f64;

    println!(
        "member_info summary over {MEMBERS} members:\n  \
         before (u32, Signature): {before_bytes} B = {before_per_entry:.2} B/entry\n  \
         after  (u32, SigDigest): {bytes} B = {per_entry:.2} B/entry\n  \
         reduction: {:.1}x",
        before_per_entry / per_entry
    );

    // Measured 134.08 → 28.01 B/entry, a 4.8x reduction. A 64-bit digest would
    // be exactly 8 B/entry cheaper than the 128-bit one (a CBOR byte string
    // costs 1 header byte either way, and a random u64 always takes the 8-byte
    // form) — see `SigDigest` for why those 8 bytes are bought deliberately.
    //
    // The bound leaves headroom above 28.01 without approaching the 134 it
    // exists to catch. The measurement is deterministic, so that headroom is for
    // future entry-shape changes, not for run-to-run variance.
    assert!(
        per_entry < 32.0,
        "member_info summary must stay under 32 bytes/entry (got {per_entry:.2}, \
         {bytes} bytes for {MEMBERS} members). This failing likely means the \
         summary regressed to carrying signatures rather than digests \
         (freenet/river#571), or that `SigDigest` lost its hand-written \
         `Serialize` — the serde derive emits a 16-element CBOR array (32 bytes) \
         instead of a byte string (17), which measures 42.53 B/entry."
    );

    // Pin the baseline too. If this ever drops near the bound above, the
    // reduction being claimed for this change has stopped being real — most
    // likely because someone "fixed" the comparison to use a byte-string
    // encoding that `ed25519::Signature` does not actually use.
    assert!(
        before_per_entry > 100.0,
        "the pre-change encoding measured {before_per_entry:.2} B/entry, but \
         `ed25519::Signature` serializes as a 64-element CBOR array (~124 bytes), \
         so this should be ~134. A much lower figure means the baseline is being \
         measured against the wrong encoding — River's `SignatureBytes` newtype \
         uses `serialize_bytes` and costs 66, but the member_info summary never \
         used that type."
    );
}
