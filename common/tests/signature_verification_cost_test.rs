//! Regression tests for freenet/river#422 — the room contract must not
//! re-verify the same Ed25519 signature over and over.
//!
//! # What went wrong
//!
//! Production forensics on the nova gateway (freenet/freenet-core#4861) found a
//! room-shaped contract whose EVERY `update_state` merge exceeded
//! freenet-core's 5 s WASM execution budget, so the room stopped converging
//! network-wide while burning 14-29 % of a CPU core on every peer hosting it.
//!
//! Profiling the room contract under wasmtime — with Cranelift at
//! `OptLevel::None`, the setting freenet-core runs contracts under — found two
//! independent multipliers, on top of a 1.6x constant factor from the build
//! profile (see the `curve25519-dalek` opt-level exception in the workspace
//! `Cargo.toml`):
//!
//! 1. **Ban signatures, re-verified once per pass.** `post_apply_cleanup` asks
//!    `ban_signature_matches_current_key` about every stored ban at step 0-cap,
//!    step 0, step 2 and step 5, and `MembersV1::apply_delta` asks once more.
//!    Each ask was an independent verification, so the work was
//!    `O(passes x bans)` on EVERY apply — whether or not the delta touched bans.
//!
//! 2. **Invite chains, re-verified once per descendant.** `MembersV1::verify`
//!    walked from every member up to the owner, verifying each link, so
//!    validating `M` members over a tree of depth `D` cost `O(M x D)`
//!    verifications (up to `O(M^2)`) where `O(M)` suffices — a member's own
//!    invite signature is the same signature no matter who walks through it.
//!    This was the load-bearing one. Measured under wasmtime at production
//!    Cranelift settings, a 234 KB / 400-member room took 4,056 ms to validate
//!    at invite depth 50 and 16,884 ms at depth 200, against a 5,000 ms budget;
//!    after the fix both are ~150 ms and the cost no longer depends on depth
//!    at all.
//!
//! # How these tests pin it
//!
//! Counting verifications, not wall-clock time: the counts are exact and
//! machine-independent, where a timing threshold would be flaky in CI (and this
//! repo treats flaky tests as broken tests). `verifications_performed()` on
//! each cache is there for exactly this.
//!
//! Reverting either memoization makes the corresponding count test fail. The
//! source pins additionally catch a regression that keeps the caches but stops
//! ROUTING through them — the shape of the original bug, where the primitive
//! was fine and the call sites were the problem.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BanSignatureCache, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{
    AuthorizedMember, InviteChainCache, Member, MemberId, MembersV1,
};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use river_core::util::sign_struct;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

fn sk(i: u64) -> SigningKey {
    let mut b = [0u8; 32];
    b[0..8].copy_from_slice(&i.to_le_bytes());
    b[8] = 0xC3;
    SigningKey::from_bytes(&b)
}

fn id_of(k: &SigningKey) -> MemberId {
    MemberId::from(&k.verifying_key())
}

/// Members laid out as one straight invite chain: member 0 invited by the
/// owner, member i invited by member i-1. Chain depth == index + 1, so the
/// uncached walk costs `n * (n + 1) / 2` verifications and the memoized one
/// costs `n`.
fn invite_chain(n: usize) -> (Vec<SigningKey>, Vec<AuthorizedMember>, ChatRoomParametersV1) {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };

    let sks: Vec<SigningKey> = (1..=n as u64).map(sk).collect();
    let mut members = Vec::with_capacity(n);
    for i in 0..n {
        let (inviter_id, inviter_sk) = if i == 0 {
            (owner_id, &owner_sk)
        } else {
            (id_of(&sks[i - 1]), &sks[i - 1])
        };
        members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: inviter_id,
                member_vk: sks[i].verifying_key(),
            },
            inviter_sk,
        ));
    }
    (sks, members, params)
}

fn lookup(members: &[AuthorizedMember]) -> HashMap<MemberId, &AuthorizedMember> {
    members.iter().map(|m| (m.member.id(), m)).collect()
}

// ---------------------------------------------------------------------------
// 1. InviteChainCache: O(M) not O(M x D), and identical verdicts.
// ---------------------------------------------------------------------------

#[test]
fn invite_chain_validation_is_linear_in_members_not_members_times_depth() {
    const N: usize = 40;
    let (_sks, members, params) = invite_chain(N);
    let by_id = lookup(&members);

    let mut cache = InviteChainCache::new(&params, &by_id);
    for m in &members {
        cache
            .validate_chain(m)
            .expect("a well-formed invite chain must validate");
    }

    // Exactly one verification per member: each member's own invite signature,
    // checked once. The pre-#422 walk did 1 + 2 + ... + N = 820 for N = 40.
    assert_eq!(
        cache.verifications_performed(),
        N,
        "invite-chain validation must verify each member's invite signature \
         exactly once; anything more is the O(members x depth) blow-up from \
         freenet/river#422 coming back"
    );
}

#[test]
fn invite_chain_cache_agrees_with_a_fresh_walk_on_every_member() {
    // The memoized verdict must equal the verdict a walk starting at that
    // member would produce — including the error text, which names the failing
    // LINK rather than the starting member.
    const N: usize = 12;
    let (_sks, members, params) = invite_chain(N);
    let by_id = lookup(&members);

    let mut shared = InviteChainCache::new(&params, &by_id);
    for m in &members {
        let memoized = shared.validate_chain(m);
        // A cache used for exactly one member can never take a memo hit, so it
        // is the uncached reference implementation.
        let fresh = InviteChainCache::new(&params, &by_id).validate_chain(m);
        assert_eq!(memoized, fresh, "memoized verdict diverged for {:?}", m);
    }
}

#[test]
fn invite_chain_cache_rejects_the_same_malformed_chains_a_fresh_walk_does() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };
    let a_sk = sk(1);
    let b_sk = sk(2);
    let c_sk = sk(3);
    let (a_id, b_id, _c_id) = (id_of(&a_sk), id_of(&b_sk), id_of(&c_sk));

    // `AuthorizedMember::new` asserts the signer IS the claimed inviter, so the
    // forged cases have to be assembled with an explicit signature.
    let mk = |member_sk: &SigningKey, invited_by: MemberId, signer: &SigningKey| {
        let member = Member {
            owner_member_id: owner_id,
            invited_by,
            member_vk: member_sk.verifying_key(),
        };
        let signature = sign_struct(&member, signer);
        AuthorizedMember::with_signature(member, signature)
    };

    // (a) forged link: A claims the owner invited them, signed by B.
    let forged = vec![mk(&a_sk, owner_id, &b_sk)];
    // (b) missing inviter: A says B invited them, B is absent.
    let orphan = vec![mk(&a_sk, b_id, &b_sk)];
    // (c) cycle: A <- B, B <- A.
    let cycle = vec![mk(&a_sk, b_id, &b_sk), mk(&b_sk, a_id, &a_sk)];
    // (d) self-invitation deeper in the chain: C <- B, B <- B.
    let self_invite = vec![mk(&c_sk, b_id, &b_sk), mk(&b_sk, b_id, &b_sk)];
    // (e) good chain, for the Ok side.
    let good = vec![mk(&a_sk, owner_id, &owner_sk), mk(&b_sk, a_id, &a_sk)];

    for (label, members) in [
        ("forged", forged),
        ("orphan", orphan),
        ("cycle", cycle),
        ("self_invite", self_invite),
        ("good", good),
    ] {
        let by_id = lookup(&members);
        let mut shared = InviteChainCache::new(&params, &by_id);
        for m in &members {
            let memoized = shared.validate_chain(m);
            let fresh = InviteChainCache::new(&params, &by_id).validate_chain(m);
            assert_eq!(
                memoized, fresh,
                "{label}: memoized verdict diverged from a fresh walk"
            );
        }
    }
}

/// `MembersV1::verify` must keep rejecting every malformed member set it
/// rejected before the memoization landed. This is the accept/reject decision
/// the whole contract rests on, so it is pinned at the `verify` level too, not
/// only at the cache level.
#[test]
fn members_verify_still_rejects_malformed_sets() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };
    let a_sk = sk(1);
    let b_sk = sk(2);
    let (a_id, b_id) = (id_of(&a_sk), id_of(&b_sk));

    // `AuthorizedMember::new` asserts the signer IS the claimed inviter, so the
    // forged case has to be assembled with an explicit signature.
    let mk = |member_sk: &SigningKey, invited_by: MemberId, signer: &SigningKey| {
        let member = Member {
            owner_member_id: owner_id,
            invited_by,
            member_vk: member_sk.verifying_key(),
        };
        let signature = sign_struct(&member, signer);
        AuthorizedMember::with_signature(member, signature)
    };

    let parent = |members: Vec<AuthorizedMember>| ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                max_members: 100,
                ..Default::default()
            },
            &owner_sk,
        ),
        members: MembersV1 { members },
        ..Default::default()
    };

    // Valid two-link chain verifies.
    let ok = parent(vec![mk(&a_sk, owner_id, &owner_sk), mk(&b_sk, a_id, &a_sk)]);
    assert!(
        ok.members.verify(&ok, &params).is_ok(),
        "a valid invite chain must verify: {:?}",
        ok.members.verify(&ok, &params)
    );

    // Forged signature on the deeper link.
    let forged = parent(vec![
        mk(&a_sk, owner_id, &owner_sk),
        mk(&b_sk, a_id, &owner_sk), // signed by owner, but claims A invited them
    ]);
    assert!(
        forged.members.verify(&forged, &params).is_err(),
        "a member whose invite signature does not match its claimed inviter \
         must be rejected"
    );

    // Inviter absent from the member set.
    let orphan = parent(vec![mk(&b_sk, a_id, &a_sk)]);
    assert!(
        orphan.members.verify(&orphan, &params).is_err(),
        "a member whose inviter is absent must be rejected"
    );

    // Cycle with no path to the owner.
    let cycle = parent(vec![mk(&a_sk, b_id, &b_sk), mk(&b_sk, a_id, &a_sk)]);
    assert!(
        cycle.members.verify(&cycle, &params).is_err(),
        "a circular invite chain must be rejected"
    );
}

// ---------------------------------------------------------------------------
// 2. BanSignatureCache: one verification per (ban, key), and identical answers.
// ---------------------------------------------------------------------------

fn ban_by(
    banner_id: MemberId,
    banner_sk: &SigningKey,
    owner_id: MemberId,
    i: u64,
) -> AuthorizedUserBan {
    AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + i),
            banned_user: id_of(&sk(900_000 + i)),
        },
        banner_id,
        banner_sk,
    )
}

#[test]
fn repeated_ban_signature_questions_cost_one_verification() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let owner_vk = owner_sk.verifying_key();
    let members: Vec<AuthorizedMember> = vec![];
    let by_id = lookup(&members);

    let bans: Vec<AuthorizedUserBan> = (0..25)
        .map(|i| ban_by(owner_id, &owner_sk, owner_id, i))
        .collect();

    let mut cache = BanSignatureCache::new();
    // Ask four times over, exactly as post_apply_cleanup's four passes do.
    for _pass in 0..4 {
        for ban in &bans {
            assert!(cache.ban_signature_matches_current_key(ban, &by_id, owner_id, &owner_vk));
        }
    }

    assert_eq!(
        cache.verifications_performed(),
        bans.len(),
        "four passes over {} bans must cost {} verifications, not {}; the \
         per-pass re-verification is what put update_state over freenet-core's \
         5s budget in freenet/river#422",
        bans.len(),
        bans.len(),
        bans.len() * 4
    );
}

#[test]
fn ban_signature_cache_agrees_with_the_uncached_check() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let owner_vk = owner_sk.verifying_key();

    let m_sk = sk(1);
    let m_id = id_of(&m_sk);
    let member = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: m_sk.verifying_key(),
        },
        &owner_sk,
    );
    let members = vec![member];
    let by_id = lookup(&members);
    let empty: HashMap<MemberId, &AuthorizedMember> = HashMap::new();

    // Genuine owner ban, genuine member ban, and a ban whose signature is
    // attributed to the member but was actually produced by the owner's key.
    let owner_ban = ban_by(owner_id, &owner_sk, owner_id, 1);
    let member_ban = ban_by(m_id, &m_sk, owner_id, 2);
    let forged = AuthorizedUserBan::with_signature(
        member_ban.ban.clone(),
        m_id,
        ban_by(owner_id, &owner_sk, owner_id, 2).signature,
    );

    for (label, ban, map) in [
        ("owner ban / members present", &owner_ban, &by_id),
        ("member ban / members present", &member_ban, &by_id),
        ("forged member ban", &forged, &by_id),
        // Banner absent: unverifiable, must answer false without caching.
        ("member ban / banner absent", &member_ban, &empty),
    ] {
        let mut cache = BanSignatureCache::new();
        let cached = cache.ban_signature_matches_current_key(ban, map, owner_id, &owner_vk);
        let uncached = BansV1::ban_signature_matches_current_key(ban, map, owner_id, &owner_vk);
        assert_eq!(cached, uncached, "{label}: cached answer diverged");
        // Asking again must not change the answer.
        let again = cache.ban_signature_matches_current_key(ban, map, owner_id, &owner_vk);
        assert_eq!(again, uncached, "{label}: second ask diverged");
    }
}

/// The member set — and therefore which key a banner resolves to — CHANGES
/// between `post_apply_cleanup`'s passes. A cache shared across those passes
/// must never answer for a key the caller is no longer asking about.
#[test]
fn ban_signature_cache_is_keyed_on_the_resolved_key_not_just_the_ban() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let owner_vk = owner_sk.verifying_key();

    // A member whose id is claimed as the banner, and an impostor entry that
    // occupies the same slot in a DIFFERENT member map with a different key.
    let real_sk = sk(1);
    let real_id = id_of(&real_sk);
    let real = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: real_sk.verifying_key(),
        },
        &owner_sk,
    );
    let other_sk = sk(2);
    let other = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: other_sk.verifying_key(),
        },
        &owner_sk,
    );

    let ban = ban_by(real_id, &real_sk, owner_id, 7);

    let with_real: HashMap<MemberId, &AuthorizedMember> = [(real_id, &real)].into_iter().collect();
    // Same banner id, different verifying key behind it.
    let with_other: HashMap<MemberId, &AuthorizedMember> =
        [(real_id, &other)].into_iter().collect();

    let mut cache = BanSignatureCache::new();
    assert!(
        cache.ban_signature_matches_current_key(&ban, &with_real, owner_id, &owner_vk),
        "the ban is genuinely signed by the real key"
    );
    assert!(
        !cache.ban_signature_matches_current_key(&ban, &with_other, owner_id, &owner_vk),
        "asking about a DIFFERENT key must re-verify and answer false, not \
         return the cached answer for the previous key"
    );
    assert_eq!(
        cache.verifications_performed(),
        2,
        "two distinct (ban, key) questions are two verifications"
    );
}

// ---------------------------------------------------------------------------
// 3. Whole-state pin: cleanup cost must not scale with (bans x passes).
// ---------------------------------------------------------------------------

fn official_room_shape(n_members: usize, n_bans: usize) -> (ChatRoomStateV1, ChatRoomParametersV1) {
    // Owner -> "invite bot" (member 0) -> everyone else, which is how the live
    // Freenet Official room is shaped.
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };
    let sks: Vec<SigningKey> = (1..=n_members as u64).map(sk).collect();
    let ids: Vec<MemberId> = sks.iter().map(id_of).collect();

    let members: Vec<AuthorizedMember> = (0..n_members)
        .map(|i| {
            let (inviter_id, inviter_sk) = if i == 0 {
                (owner_id, &owner_sk)
            } else {
                (ids[0], &sks[0])
            };
            AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: inviter_id,
                    member_vk: sks[i].verifying_key(),
                },
                inviter_sk,
            )
        })
        .collect();

    let member_info: Vec<AuthorizedMemberInfo> = (0..n_members)
        .map(|i| {
            AuthorizedMemberInfo::new_with_member_key(
                MemberInfo::new_public(ids[i], 1, format!("m{i}")),
                &sks[i],
            )
        })
        .collect();

    // Bans issued by the invite bot against users who are no longer members —
    // what a moderated room accumulates.
    let bans: Vec<AuthorizedUserBan> = (0..n_bans as u64)
        .map(|i| ban_by(ids[0], &sks[0], owner_id, i))
        .collect();

    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                max_members: 400,
                max_user_bans: n_bans.max(1),
                ..Default::default()
            },
            &owner_sk,
        ),
        bans: BansV1(bans),
        members: MembersV1 { members },
        member_info: MemberInfoV1 { member_info },
        ..Default::default()
    };
    state.members.members.sort_by_key(|m| m.member.id());
    state
        .member_info
        .member_info
        .sort_by_key(|i| i.member_info.member_id);
    (state, params)
}

/// `post_apply_cleanup` must stay idempotent and ban-preserving at the live
/// room's scale. Cheap correctness backstop for the memoization: if the shared
/// cache ever answered a stale question, bans or members would be swept.
#[test]
fn cleanup_at_official_room_scale_is_stable_and_idempotent() {
    let (state, params) = official_room_shape(60, 40);

    let mut once = state.clone();
    once.post_apply_cleanup(&params).unwrap();
    let mut twice = once.clone();
    twice.post_apply_cleanup(&params).unwrap();

    assert_eq!(
        once, twice,
        "post_apply_cleanup must stay idempotent with the shared ban-signature \
         cache in place"
    );
    assert_eq!(
        once.bans.0.len(),
        40,
        "every genuinely-signed ban by a current member must survive cleanup"
    );
    assert!(
        once.members
            .members
            .iter()
            .any(|m| m.member.id() == once.bans.0[0].banned_by),
        "the banner must survive inactivity-prune via the step-2 exemption"
    );
}

// ---------------------------------------------------------------------------
// 4. Source pins — the caches must actually be USED.
// ---------------------------------------------------------------------------
//
// The count tests above prove the caches memoize. They cannot see a regression
// that keeps the caches but reverts a CALL SITE to the uncached helper, which
// is exactly the shape of the original bug: the primitives were correct and the
// call sites did the redundant work. These pins read the real source.

/// Whitespace-insensitive substring search, so `cargo fmt` re-wrapping a call
/// cannot silently disarm a pin.
fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Production source only: everything before the file's `mod tests`. Cutting at
/// `#[cfg(test)]` instead would stop at the first cfg-gated item and leave most
/// of the file unscanned.
fn production_source(src: &str) -> String {
    let cut = src
        .find("\nmod tests")
        .or_else(|| src.find("\n#[cfg(test)]\nmod tests"))
        .unwrap_or(src.len());
    squeeze(&src[..cut])
}

#[test]
fn post_apply_cleanup_routes_every_ban_check_through_the_shared_cache() {
    let src = production_source(include_str!("../src/room_state.rs"));

    assert!(
        src.contains("let mut ban_sigs = BanSignatureCache::new();"),
        "post_apply_cleanup must build ONE BanSignatureCache for the whole \
         cleanup (freenet/river#422)"
    );

    // Every ban-signature question in the cleanup must go through it.
    for uncached in [
        "BansV1::ban_signature_matches_current_key(",
        "BansV1::ban_is_enforcing(",
        ".banned_member_ids(",
    ] {
        assert!(
            !src.contains(uncached),
            "room_state.rs still calls the UNCACHED `{uncached}`. Each such \
             call re-verifies every stored ban's Ed25519 signature; four such \
             passes at the Freenet Official room's 200 bans is what pushed \
             update_state past freenet-core's 5s budget (freenet/river#422). \
             Use the `ban_sigs` cache instead."
        );
    }

    for cached in [
        "BansV1::ban_is_enforcing_cached( &mut ban_sigs,",
        ".banned_member_ids_cached( &mut ban_sigs,",
        "ban_sigs.ban_signature_matches_current_key(",
    ] {
        assert!(
            src.contains(cached),
            "expected post_apply_cleanup to use `{cached}`"
        );
    }
}

#[test]
fn members_verify_and_apply_delta_memoize_invite_chains() {
    let src = production_source(include_str!("../src/room_state/member.rs"));

    assert_eq!(
        src.matches("let mut chains = InviteChainCache::new(")
            .count(),
        2,
        "MembersV1::verify and MembersV1::apply_delta must EACH build their own \
         InviteChainCache, so a member's invite signature is verified once \
         rather than once per descendant (freenet/river#422) — and so neither \
         inherits the other's verdicts"
    );
    // Both contexts must BIND the cache: an unbound cache could be shared
    // across rooms (inheriting an `Ok` earned under a different owner) or
    // across verify/apply_delta (leaking apply_delta's weaker verdict, which is
    // granted against a map that includes not-yet-verified delta members).
    // Whitespace-insensitive and argument-NAME-insensitive: a pin that spells
    // out locals fails open the moment someone renames one. What must hold is
    // that each construction passes two arguments (the binding) rather than
    // none, and that the walk still consults the cache.
    assert_eq!(
        src.matches("InviteChainCache::new()").count(),
        0,
        "an UNBOUND InviteChainCache could be shared across rooms, or across \
         verify and apply_delta whose lookups differ, and hand out an `Ok` \
         earned elsewhere (freenet/river#422)"
    );
    assert!(
        src.contains("chains.validate_chain(member)?;"),
        "both call sites must validate through the cache"
    );
    assert!(
        !src.contains("self.get_invite_chain_with_lookup(member, parameters, &members_by_id)?;"),
        "MembersV1::verify must not fall back to the unmemoized full-chain \
         walk — that is the O(members x depth) blow-up from freenet/river#422"
    );
}

#[test]
fn crypto_is_exempt_from_the_size_optimized_profile() {
    // `opt-level = 'z'` leaves curve25519-dalek's field arithmetic un-inlined,
    // costing 1.6x on every Ed25519 verification the contract does (0.4224 ms
    // vs 0.2622 ms, wasm32 under wasmtime at freenet-core's `OptLevel::None`).
    // Losing this exception is a silent performance regression with no source
    // change anywhere, so it is pinned here.
    let manifest = squeeze(include_str!("../../Cargo.toml"));
    assert!(
        manifest.contains("[profile.release.package.curve25519-dalek] opt-level = 3"),
        "the workspace Cargo.toml must keep curve25519-dalek at opt-level 3; \
         under the workspace-wide `opt-level = 'z'` every Ed25519 verification \
         in the room contract costs 1.6x more (freenet/river#422)"
    );
}

// ---------------------------------------------------------------------------
// 5. Memo keys must be the OBJECTS, not their 64-bit ids.
// ---------------------------------------------------------------------------
//
// `BanId` is `fast_hash(signature)` and `MemberId` is `fast_hash(verifying
// key)` — both 64-bit hashes of attacker-chosen bytes. Keying either memo on
// the id would let a colliding forgery inherit a genuine object's "signature
// verified" verdict and skip its own check. An attacker does not need a 2^64
// second preimage for that, only a ~2^33 birthday search over candidates they
// generate themselves.
//
// These tests do not grind anything: the collisions are constructed exactly,
// because two bans that share a signature necessarily share a `BanId`, and two
// members that share a verifying key necessarily share a `MemberId`.

#[test]
fn a_ban_id_collision_cannot_inherit_a_genuine_bans_verified_verdict() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let owner_vk = owner_sk.verifying_key();

    let m_sk = sk(1);
    let m_id = id_of(&m_sk);
    let member = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: m_sk.verifying_key(),
        },
        &owner_sk,
    );
    let members = vec![member];
    let by_id = lookup(&members);

    // Genuine ban by the member.
    let genuine = ban_by(m_id, &m_sk, owner_id, 1);

    // A DIFFERENT ban body carrying the genuine ban's signature. Its `BanId` is
    // `fast_hash(signature)`, so it collides with `genuine` exactly — no
    // grinding required — while its signature does not cover this body.
    let forged = AuthorizedUserBan::with_signature(
        UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_900_000_000),
            banned_user: id_of(&sk(4242)),
        },
        m_id,
        genuine.signature,
    );
    assert_eq!(
        genuine.id(),
        forged.id(),
        "test setup: the two bans must share a BanId"
    );
    assert_ne!(genuine.ban, forged.ban, "test setup: bodies must differ");

    let mut cache = BanSignatureCache::new();
    assert!(
        cache.ban_signature_matches_current_key(&genuine, &by_id, owner_id, &owner_vk),
        "the genuine ban verifies"
    );
    assert!(
        !cache.ban_signature_matches_current_key(&forged, &by_id, owner_id, &owner_vk),
        "a ban that merely SHARES a BanId with a verified ban must be checked \
         on its own merits and rejected. Keying the memo on BanId instead of \
         the ban itself would hand it an enforceable forged ban and reopen the \
         forged-ban enforcement hole from freenet/river#411 round 4 A."
    );
    // And it must agree with the uncached reference.
    assert!(!BansV1::ban_signature_matches_current_key(
        &forged, &by_id, owner_id, &owner_vk
    ));
}

#[test]
fn a_member_id_collision_cannot_inherit_a_genuine_members_chain_verdict() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };

    // A privileged member whose subtree an attacker would like to join.
    let mod_sk = sk(1);
    let mod_id = id_of(&mod_sk);
    let moderator = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: mod_sk.verifying_key(),
        },
        &owner_sk,
    );

    // The attacker's genuine, owner-invited entry.
    let att_sk = sk(2);
    let genuine = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: att_sk.verifying_key(),
        },
        &owner_sk,
    );

    // A second entry for the SAME verifying key — so necessarily the same
    // MemberId — claiming a different, unearned inviter, with a signature that
    // does not authorise it.
    let forged_member = Member {
        owner_member_id: owner_id,
        invited_by: mod_id,
        member_vk: att_sk.verifying_key(),
    };
    let forged = AuthorizedMember::with_signature(
        forged_member,
        sign_struct(&genuine.member, &owner_sk), // signature over the OTHER body
    );
    assert_eq!(
        genuine.member.id(),
        forged.member.id(),
        "test setup: the two entries must share a MemberId"
    );
    assert_ne!(genuine, forged, "test setup: the entries must differ");

    // Genuine first, so a MemberId-keyed memo would already hold its `Ok`.
    let members = vec![moderator, genuine, forged];
    let by_id = lookup(&members);

    let mut cache = InviteChainCache::new(&params, &by_id);
    for m in &members[..2] {
        cache
            .validate_chain(m)
            .expect("the genuine entries validate");
    }
    assert!(
        cache.validate_chain(&members[2]).is_err(),
        "an entry that merely SHARES a MemberId with a validated member must \
         have its OWN invite signature checked. Keying the memo on MemberId \
         instead of the member itself would let an attacker forge their \
         position in the invite tree (freenet/river#422)."
    );
}

// ---------------------------------------------------------------------------
// 6. An InviteChainCache verdict is meaningful ONLY in its own context.
// ---------------------------------------------------------------------------
//
// `InviteChainCache::new` takes the room parameters and the member lookup and
// holds them for the cache's life, so one instance cannot span two contexts —
// the borrow checker enforces it in every build, where a stored-owner +
// `debug_assert` would be compiled out of the release WASM the contract ships
// as. These two tests establish that the binding is load-bearing rather than
// decorative, by showing the verdict genuinely differs across each context a
// shared cache could have spanned.

#[test]
fn a_chain_verdict_is_owner_specific() {
    // `Ok` means "this chain reaches THE OWNER". Under a different owner the
    // same member must NOT validate — so a cache shared across two rooms would
    // hand out an `Ok` no key in the second room ever signed for.
    let owner_a_sk = sk(0);
    let owner_a_id = id_of(&owner_a_sk);
    let params_a = ChatRoomParametersV1 {
        owner: owner_a_sk.verifying_key(),
    };
    let params_b = ChatRoomParametersV1 {
        owner: sk(99).verifying_key(),
    };

    let m_sk = sk(1);
    let member = AuthorizedMember::new(
        Member {
            owner_member_id: owner_a_id,
            invited_by: owner_a_id,
            member_vk: m_sk.verifying_key(),
        },
        &owner_a_sk,
    );
    let members = vec![member];
    let by_id = lookup(&members);

    assert!(
        InviteChainCache::new(&params_a, &by_id)
            .validate_chain(&members[0])
            .is_ok(),
        "the member's chain reaches owner A"
    );
    assert!(
        InviteChainCache::new(&params_b, &by_id)
            .validate_chain(&members[0])
            .is_err(),
        "the SAME member must not validate under a different owner; a cache \
         shared across rooms would let this inherit owner A's verdict"
    );
}

#[test]
fn a_chain_verdict_is_specific_to_the_lookup_it_was_earned_against() {
    // `apply_delta` resolves inviters through a map that also holds the delta's
    // members, which `verify`'s map does not. A member can therefore earn `Ok`
    // there that `verify`'s stricter map would refuse, so a cache shared across
    // the two would leak the weaker verdict into the stronger context.
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };

    let a_sk = sk(1);
    let a_id = id_of(&a_sk);
    let b_sk = sk(2);
    let b_id = id_of(&b_sk);
    let c_sk = sk(3);

    let a = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: a_sk.verifying_key(),
        },
        &owner_sk,
    );
    // B and C arrive in the delta; C's inviter B is not yet in the state.
    let b = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: a_id,
            member_vk: b_sk.verifying_key(),
        },
        &a_sk,
    );
    let c = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: b_id,
            member_vk: c_sk.verifying_key(),
        },
        &b_sk,
    );

    let stored = vec![a.clone()];
    let combined = vec![a, b, c.clone()];

    let combined_by_id = lookup(&combined);
    assert!(
        InviteChainCache::new(&params, &combined_by_id)
            .validate_chain(&c)
            .is_ok(),
        "against apply_delta's combined lookup, C resolves through the delta's B"
    );

    let stored_by_id = lookup(&stored);
    assert!(
        InviteChainCache::new(&params, &stored_by_id)
            .validate_chain(&c)
            .is_err(),
        "against verify's stored-members-only lookup, C's inviter is absent and \
         C must be refused; a cache shared between apply_delta and verify would \
         leak the weaker verdict into the stronger context"
    );
}

/// `AuthorizedUserBan`'s `Hash` must cover the whole struct, not the signature
/// alone.
///
/// Hashing the signature alone satisfied the `Hash`/`Eq` contract and was
/// harmless while nothing hashed a ban. `BanSignatureCache` now does, which
/// makes bucketing load-bearing: a signature is attacker-supplied and
/// replayable across different ban bodies (the #411 round 4 replay shape), so
/// signature-only hashing lets an attacker pile distinct bans into one bucket
/// and turn cache lookups linear. Correctness never depended on it — `Eq`
/// compares every field — but the influence is free to remove.
#[test]
fn ban_hash_covers_the_whole_struct_not_just_the_signature() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);

    let genuine = ban_by(owner_id, &owner_sk, owner_id, 1);
    // Same signature, different body — the replayable shape.
    let replayed = AuthorizedUserBan::with_signature(
        UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_900_000_000),
            banned_user: id_of(&sk(4242)),
        },
        owner_id,
        genuine.signature,
    );
    assert_ne!(genuine, replayed, "test setup: the bans must differ");

    let hash_of = |b: &AuthorizedUserBan| {
        let mut h = DefaultHasher::new();
        b.hash(&mut h);
        h.finish()
    };
    assert_ne!(
        hash_of(&genuine),
        hash_of(&replayed),
        "two bans sharing a signature but differing in body must land in \
         different buckets; hashing the signature alone lets an attacker choose \
         BanSignatureCache's bucket distribution"
    );
}

// ---------------------------------------------------------------------------
// 7. Differential against the implementation this PR replaced.
// ---------------------------------------------------------------------------
//
// `InviteChainCache::validate_chain` is a hand re-transcription of the loop in
// `MembersV1::get_invite_chain_with_lookup`, not a wrapper around it. Every
// other equivalence test in this file compares the new implementation against
// ITSELF with an empty cache, which cannot see a dropped, reordered or
// re-scoped check — both sides would be wrong identically.
//
// `MembersV1::get_invite_chain` is still live and `pub` and IS the untouched
// original, so it is the real oracle. This is the test that caught the
// non-canonical-start divergence; it must keep passing.

/// The oracle: the pre-existing full-chain walk, verdict only.
fn oracle(
    members: &[AuthorizedMember],
    m: &AuthorizedMember,
    params: &ChatRoomParametersV1,
) -> Result<(), String> {
    MembersV1 {
        members: members.to_vec(),
    }
    .get_invite_chain(m, params)
    .map(|_| ())
}

fn differential(members: &[AuthorizedMember], params: &ChatRoomParametersV1) {
    let by_id = lookup(members);
    // One SHARED cache across all members, which is how `MembersV1::verify`
    // uses it — a per-member cache would hide every memo-hit divergence.
    let mut shared = InviteChainCache::new(params, &by_id);
    for m in members {
        assert_eq!(
            shared.validate_chain(m),
            oracle(members, m, params),
            "memoized verdict diverged from the original walk for {:?}",
            m.member.id()
        );
    }
}

#[test]
fn differential_against_the_original_walk_on_a_straight_chain() {
    let (_sks, members, params) = invite_chain(8);
    differential(&members, &params);
}

/// A member below the impersonated one mints a duplicate entry for that
/// member's key, claiming the minter as its own inviter. The duplicate's
/// signature is genuine (it really was signed by the minter), so nothing
/// rejects it up front — but its chain loops back into itself.
///
/// This is the freenet/river#422 review counterexample: walking B memoizes
/// B -> Ok, so a later walk from the duplicate takes a memo hit at B and
/// short-circuits, where the original walked on and hit `id_a` already in its
/// visited set. The memo lookup sits BEFORE the cycle guard, and the guard
/// never fires because the hit lands on B before the walk reaches A again —
/// so moving the lookup below the guard does NOT fix this.
#[test]
fn differential_when_a_walk_starts_at_a_non_canonical_duplicate() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };

    let a_sk = sk(1);
    let a_id = id_of(&a_sk);
    let b_sk = sk(2);
    let b_id = id_of(&b_sk);

    // A invited by the owner; B invited by A.
    let a = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: a_sk.verifying_key(),
        },
        &owner_sk,
    );
    let b = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: a_id,
            member_vk: b_sk.verifying_key(),
        },
        &a_sk,
    );
    // B mints a second entry for A's key, claiming B invited A.
    let a_prime = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: b_id,
            member_vk: a_sk.verifying_key(),
        },
        &b_sk,
    );
    assert_eq!(
        a.member.id(),
        a_prime.member.id(),
        "test setup: the duplicate must share A's MemberId"
    );
    assert_ne!(a, a_prime, "test setup: the entries must differ");

    // Order matters: B is walked first so its Ok is memoized, and A is LAST so
    // `members_by_member_id`'s last-wins makes A canonical and A' the
    // non-canonical duplicate.
    differential(&[b, a_prime, a], &params);
}

/// Same shape through `MembersV1::verify`, which is the production entry point.
#[test]
fn verify_agrees_with_the_original_walk_on_a_non_canonical_duplicate() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };
    let a_sk = sk(1);
    let a_id = id_of(&a_sk);
    let b_sk = sk(2);
    let b_id = id_of(&b_sk);

    let mk = |member_sk: &SigningKey, invited_by: MemberId, signer: &SigningKey| {
        AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by,
                member_vk: member_sk.verifying_key(),
            },
            signer,
        )
    };
    let members = vec![
        mk(&b_sk, a_id, &a_sk),
        mk(&a_sk, b_id, &b_sk),
        mk(&a_sk, owner_id, &owner_sk),
    ];

    let state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                max_members: 100,
                ..Default::default()
            },
            &owner_sk,
        ),
        members: MembersV1 {
            members: members.clone(),
        },
        ..Default::default()
    };

    // Whatever the right answer is, it must be the one the untouched walk gives.
    let oracle_verdict: Result<(), String> = members
        .iter()
        .try_for_each(|m| oracle(&members, m, &params));
    assert_eq!(
        state.members.verify(&state, &params).is_ok(),
        oracle_verdict.is_ok(),
        "MembersV1::verify must agree with the original full-chain walk; \
         a duplicate member entry whose chain loops back through its minter \
         must not be accepted just because the minter was memoized Ok"
    );
}

// ---------------------------------------------------------------------------
// 8. Gaps the independent testing review identified.
// ---------------------------------------------------------------------------

/// `AuthorizedMember`'s structural `Eq` is what makes the memo key safe, and
/// nothing else in this file pins it.
///
/// The ban side gets this for free: `a_ban_id_collision_...` puts a genuine and
/// a forged ban in the SAME hash bucket (the old `Hash` covered only the
/// signature), so it exercised `Eq` too. The member-collision test does not —
/// its two entries differ in `invited_by`, which `AuthorizedMember`'s `Hash`
/// covers, so they never collide and `Eq` is never consulted. Weakening `Eq` to
/// compare only `member` therefore passed the whole suite.
#[test]
fn authorized_member_equality_covers_the_signature() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let m_sk = sk(1);

    let member = Member {
        owner_member_id: owner_id,
        invited_by: owner_id,
        member_vk: m_sk.verifying_key(),
    };
    let genuine = AuthorizedMember::new(member.clone(), &owner_sk);
    // Identical body, different signature.
    let garbage = AuthorizedMember::with_signature(member, sign_struct(&genuine.member, &sk(77)));

    assert_ne!(
        genuine, garbage,
        "AuthorizedMember equality MUST cover the signature. InviteChainCache \
         keys its memo on the whole struct precisely so a garbage-signature \
         entry cannot collide with a validated one; if `Eq` ignored the \
         signature, that entry would inherit the genuine verdict with its own \
         signature never checked (freenet/river#422)."
    );
}

/// The `Err` memo path is never exercised by the other fixtures — they are
/// straight chains or 1-3 members, so no second walk takes a memo hit on a
/// failed verdict. The doc on `InviteChainCache` spends a bullet justifying
/// Err-memoization soundness; this is the fan-out shape that uses it.
#[test]
fn an_err_verdict_is_reused_correctly_by_siblings_of_a_broken_ancestor() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };

    let e_sk = sk(1);
    let e_id = id_of(&e_sk);
    let x_sk = sk(2);
    let y_sk = sk(3);

    // E claims the owner invited it, but the signature is by the wrong key.
    let e_member = Member {
        owner_member_id: owner_id,
        invited_by: owner_id,
        member_vk: e_sk.verifying_key(),
    };
    let e = AuthorizedMember::with_signature(e_member, sign_struct(e_sk.verifying_key(), &sk(88)));
    // X and Y are both genuinely invited by E, so both chains run through it.
    let mk_child = |child: &SigningKey| {
        AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: e_id,
                member_vk: child.verifying_key(),
            },
            &e_sk,
        )
    };
    let members = vec![e, mk_child(&x_sk), mk_child(&y_sk)];
    let by_id = lookup(&members);

    // Both siblings must fail, and identically — the second takes an Err memo
    // hit at E. And both must match the untouched original walk.
    let mut shared = InviteChainCache::new(&params, &by_id);
    let first = shared.validate_chain(&members[1]);
    let second = shared.validate_chain(&members[2]);
    assert!(
        first.is_err() && second.is_err(),
        "both chains run through a broken ancestor"
    );
    for m in &members {
        assert_eq!(
            InviteChainCache::new(&params, &by_id).validate_chain(m),
            oracle(&members, m, &params),
            "Err-memo reuse must match the original walk for {:?}",
            m.member.id()
        );
    }
    differential(&members, &params);
}

/// The whole safety argument for sharing ONE `BanSignatureCache` across
/// `post_apply_cleanup`'s passes is that it survives the member-set mutations
/// those passes perform. `cleanup_at_official_room_scale_...` uses a fixture
/// where nothing is removed, so the cache never actually spans a mutation.
///
/// Here the banner IS pruned partway through cleanup, so the step-5 sweep asks
/// about a ban whose banner no longer resolves. That must answer `false`
/// without consulting the cache — the early return a future refactor is most
/// likely to "optimize" into it.
#[test]
fn shared_ban_cache_survives_a_banner_being_pruned_mid_cleanup() {
    let owner_sk = sk(0);
    let owner_id = id_of(&owner_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };

    // The banner is a content-free member: no messages, no DMs, no secrets. Its
    // only claim to survival is the step-2 banner exemption.
    let banner_sk = sk(1);
    let banner_id = id_of(&banner_sk);
    let banner = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: banner_sk.verifying_key(),
        },
        &owner_sk,
    );
    // A ban by that member with a GARBAGE signature: the step-2 exemption and
    // the step-5 sweep must agree it does not hold, so the banner is pruned AND
    // the ban swept — and they must agree via the shared cache.
    let good = ban_by(banner_id, &banner_sk, owner_id, 1);
    let forged = AuthorizedUserBan::with_signature(
        good.ban.clone(),
        banner_id,
        ban_by(owner_id, &owner_sk, owner_id, 9).signature,
    );

    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                max_members: 100,
                max_user_bans: 100,
                ..Default::default()
            },
            &owner_sk,
        ),
        bans: BansV1(vec![forged]),
        members: MembersV1 {
            members: vec![banner],
        },
        member_info: MemberInfoV1::default(),
        ..Default::default()
    };

    state.post_apply_cleanup(&params).unwrap();

    assert!(
        state.members.members.is_empty(),
        "a banner whose only ban has a garbage signature is not exempt from \
         inactivity-prune and must be removed"
    );
    assert!(
        state.bans.0.is_empty(),
        "and its ban must be swept at step 5 — the shared cache must not have \
         handed step 5 a verdict for a banner that no longer resolves"
    );

    // Idempotent across the mutation, which is the invariant the whole cleanup
    // rests on.
    let mut again = state.clone();
    again.post_apply_cleanup(&params).unwrap();
    assert_eq!(state, again, "cleanup must stay idempotent");
}
