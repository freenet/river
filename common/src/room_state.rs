pub mod ban;
pub mod configuration;
pub mod content;
pub mod direct_messages;
pub mod dm_body;
pub mod identity;
pub mod member;
pub mod member_info;
pub mod message;
pub mod privacy;
pub mod secret;
pub mod upgrade;
pub mod version;

use crate::room_state::ban::BansV1;
use crate::room_state::configuration::AuthorizedConfigurationV1;
use crate::room_state::direct_messages::DirectMessagesV1;
use crate::room_state::member::{MemberId, MembersV1};
use crate::room_state::member_info::MemberInfoV1;
use crate::room_state::message::MessagesV1;
use crate::room_state::secret::RoomSecretsV1;
use crate::room_state::upgrade::OptionalUpgradeV1;
use crate::room_state::version::StateVersion;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold_macro::composable;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[composable(post_apply_delta = "post_apply_cleanup")]
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomStateV1 {
    // WARNING: The order of these fields is important for the purposes of the #[composable] macro.
    // `configuration` must be first, followed by `bans`, `members`, `member_info`, `secrets`,
    // and then `recent_messages`.
    // This is due to interdependencies between the fields and the order in which they must be applied in
    // the `apply_delta` function. DO NOT reorder fields without fully understanding the implications.
    /// Configures things like maximum message length, can be updated by the owner.
    pub configuration: AuthorizedConfigurationV1,

    /// A list of recently banned members, a banned member can't be present in the
    /// members list and will be removed from it ifc necessary.
    pub bans: BansV1,

    /// The members in the chat room along with who invited them
    pub members: MembersV1,

    /// Metadata about members like their nickname, can be updated by members themselves.
    pub member_info: MemberInfoV1,

    /// Secret distribution for private rooms. Must come before recent_messages so message
    /// validation can check secret version consistency.
    pub secrets: RoomSecretsV1,

    /// The most recent messages in the chat room, the number is limited by the room configuration.
    pub recent_messages: MessagesV1,

    /// In-room encrypted direct messages between members (#230 Phase 1).
    /// `#[serde(default)]` keeps states written before this field was added
    /// backwards-compatible.
    #[serde(default)]
    pub direct_messages: DirectMessagesV1,

    /// If this contract has been replaced by a new contract this will contain the new contract address.
    /// This can only be set by the owner.
    pub upgrade: OptionalUpgradeV1,

    /// State format version for migration compatibility.
    /// Defaults to 0 for backward compatibility with states created before versioning.
    #[serde(default)]
    pub version: StateVersion,
}

impl ChatRoomStateV1 {
    /// Post-apply cleanup: prune members who have no recent messages, clean up
    /// member_info for pruned members, remove orphaned bans, and sweep
    /// direct messages whose participants are no longer in the room.
    ///
    /// Members are kept if they have at least one message in recent_messages,
    /// are a sender/recipient of a currently-held direct message (see
    /// [`crate::room_state::direct_messages::DirectMessagesV1::active_participants`]),
    /// or are in the invite chain of someone who qualifies. The owner is
    /// never in the members list (they're implicit via parameters).
    ///
    /// Bans are only removed if the banner was themselves BANNED (orphaned ban).
    /// If the banner was merely pruned for inactivity, their bans persist.
    ///
    /// IDEMPOTENCE / CONVERGENCE INVARIANT: this function MUST be idempotent
    /// (`cleanup(S) == cleanup(cleanup(S))`) and a pure function of the converged
    /// state, because Freenet runs it a variable number of times across peers
    /// (and full-state PUTs bypass it via `verify`). The `max_user_bans` cap is
    /// therefore applied at the TOP (step 0-cap) so ban enforcement and the
    /// banner-prune exemption read the FINAL surviving ban set — see the block
    /// comments below and #411 round 7 / Codex P1 #1+#2.
    ///
    /// Direct-message sweep: after pruning, any DM whose sender or
    /// recipient is now non-member or banned is dropped. Without this,
    /// adding a ban for a DM participant would silently make every
    /// peer's verify fail, and members referenced only by a DM would be
    /// pruned (orphaning their DMs). See
    /// `direct_messages.rs` module docs, "Interaction with bans".
    pub fn post_apply_cleanup(&mut self, parameters: &ChatRoomParametersV1) -> Result<(), String> {
        let owner_id = MemberId::from(&parameters.owner);

        // 0-cap. Enforce `max_user_bans` FIRST — BEFORE ban enforcement (step 0)
        //     and the banner inactivity-prune exemption (step 2) — so both read
        //     the FINAL surviving (post-cap) ban set (#411 round 7 / Codex P1
        //     #1+#2). Running the cap here, on the converged pre-enforcement
        //     state, is what keeps `post_apply_cleanup` IDEMPOTENT and identical
        //     across peers:
        //       * #1: an over-cap ban that WILL be evicted must not one-shot a
        //         member removal at step 0 that the capped converged state
        //         cannot reproduce — a peer that only ever sees the capped state
        //         (e.g. via a full-state PUT that bypasses cleanup) would keep
        //         that member, so removing it here would diverge the member set.
        //       * #2: a banner whose ban the cap evicts must NOT be exempted
        //         from inactivity-prune at step 2. If it were (as when the cap
        //         ran last), the banner is kept on pass 1 but its ban is then
        //         evicted, so pass 2 prunes it — `cleanup(S) != cleanup(cleanup(S))`,
        //         which permanently diverges peers that run cleanup a different
        //         number of times.
        //     Eviction drops INERT (currently-unauthorized) bans before
        //     enforcing ones (#410 review round 1). This bounds an INERT flood
        //     (forged / revoked-deputy bans, which `verify` accepts) — those are
        //     evicted first, so a flood of them cannot push real moderator bans
        //     out of the cap. It does NOT fully defend the un-ban DoS: an
        //     ENFORCING-absent-target flood still evicts real bans, because a ban
        //     by a current member of an ABSENT target classifies "enforcing"
        //     WITHOUT any authorization check (`ban_is_enforcing` returns true for
        //     a member-banner + absent target), and `banned_at` is an
        //     attacker-signed, future-datable field — so a member can mint many
        //     newest-dated "enforcing" bans that outrank and evict genuine ones.
        //     The substantive fix (an authorization-aware / non-attacker-ordered
        //     cap) is deferred pending Ian's decision; tracked in
        //     freenet/river#413 (Limitation 2).
        //
        //     CONVERGENCE HONESTY: the enforcing/inert classification is a
        //     deterministic function of the member set it runs against, but that
        //     member set is the INTERMEDIATE (pre-step-0) set as `apply_delta`
        //     left it, which is order-dependent (cascade removal is arrival-order
        //     sensitive). So two peers applying the same delta multiset in a
        //     different order can evict DIFFERENT bans and NOT be byte-equal
        //     without a further exchange; anti-entropy `merge` reconciles them to
        //     the same top-set (River's ban summary is a full BanId set, so
        //     anti-entropy always converges — pinned by
        //     `cap_eviction_reconciles_via_merge`; tracked in freenet/river#413,
        //     Limitation 1). Do NOT claim the eviction is order-independent per
        //     delta.
        //     `verify`'s hard cap ceiling still rejects any stored state left over
        //     the cap. The signature sweep (step 5) still runs AFTER enforcement
        //     to drop bans orphaned by a banner's removal, and only removes bans,
        //     so the ban count stays <= the cap.
        let max_bans = self.configuration.configuration.max_user_bans;
        if self.bans.0.len() > max_bans {
            let members_by_id = self.members.members_by_member_id();
            // Order so the entries to DROP come first: inert-before-enforcing,
            // then oldest-before-newest, then ban id (fully deterministic).
            // `sort_by_cached_key` computes `ban_is_enforcing` at most ONCE per
            // ban (not O(n log n) times inside a comparator) — #411 round 3 C.
            self.bans.0.sort_by_cached_key(|ban| {
                (
                    BansV1::ban_is_enforcing(
                        ban,
                        &members_by_id,
                        &self.member_info,
                        owner_id,
                        &parameters.owner,
                    ),
                    ban.ban.banned_at,
                    ban.id(),
                )
            });
            let to_remove = self.bans.0.len() - max_bans;
            self.bans.0.drain(0..to_remove);
            // Restore the canonical (banned_at, id) stored order.
            self.bans.0.sort_by(|a, b| {
                a.ban
                    .banned_at
                    .cmp(&b.ban.banned_at)
                    .then_with(|| a.id().cmp(&b.id()))
            });
        }

        // 0. Enforce bans from the CONVERGED (now capped) state, deputy-aware (#410).
        //
        // `MembersV1::apply_delta` already removed members banned by the owner
        // or an ancestor, but it ran BEFORE the sibling `member_info` field
        // (which carries deputy grants) was applied, so it could not evaluate
        // deputy authority. This pass runs after every field has been applied,
        // so `self.member_info` is converged: it removes members banned by a
        // currently-authorized deputy, and — crucially — does NOT remove
        // members whose deputy was revoked (the deputizer removed them from
        // `MemberInfo.deputies` at a higher version). Because the removal set
        // is a pure function of the converged (members + deputies + bans)
        // state, and bans stay an add-only CRDT (never pruned here), every peer
        // converges to the same member set regardless of delta order. Kept in
        // post_apply_cleanup (NOT verify) so verify stays stable across
        // ban/deputy changes — mirrors the DM ban-sweep precedent.
        let enforced_banned_ids =
            self.members
                .banned_member_ids(&self.bans, &self.member_info, parameters);
        self.members
            .members
            .retain(|m| !enforced_banned_ids.contains(&m.member.id()));

        // 1. Collect message author IDs + DM participants + secret recipients.
        //
        // Secret recipients (i.e. members for whom the owner has issued an
        // `encrypted_secrets` blob AT THE CURRENT VERSION) are exempt
        // from inactivity-prune. The owner explicitly chose to issue
        // them a per-version room secret, so the owner clearly considers
        // them a member — and post_apply cleanup running on an
        // invitee's first state ingestion (which arrives before the
        // invitee has authored any join_event) must not silently delete
        // that membership. See issue #110 / Bug #3 PR B (Ivvor
        // 2026-05-17).
        //
        // The exemption is restricted to recipients at `current_version`
        // so cleanup still prunes genuinely-inactive members whose
        // blobs are only present at older versions (a member who joined,
        // received v0, never authored anything, and was never re-issued
        // a blob at v1+ is "stale" by the same definition as a member
        // who joined and never authored). Without this scoping the
        // exemption would keep every ever-recipient + their entire
        // invite chain ancestor set exempt from cleanup forever,
        // defeating the prune. See IMPORTANT item #5 on PR #272
        // review round 2.
        let message_authors: HashSet<MemberId> = self
            .recent_messages
            .messages
            .iter()
            .map(|m| m.message.author)
            .collect();
        let dm_participants: HashSet<MemberId> = self.direct_messages.active_participants();
        let current_secret_version = self.secrets.current_version;
        let secret_recipients: HashSet<MemberId> = self
            .secrets
            .encrypted_secrets
            .iter()
            .filter(|s| s.secret.secret_version == current_secret_version)
            .map(|s| s.secret.member_id)
            .collect();

        // 2. Compute required members: authors + DM participants + secret
        //    recipients + their invite chains.
        let required_ids = {
            let members_by_id = self.members.members_by_member_id();
            let mut required_ids: HashSet<MemberId> = HashSet::new();

            for author_id in &message_authors {
                if *author_id != owner_id && members_by_id.contains_key(author_id) {
                    required_ids.insert(*author_id);
                }
            }

            for participant_id in &dm_participants {
                if *participant_id != owner_id && members_by_id.contains_key(participant_id) {
                    required_ids.insert(*participant_id);
                }
            }

            for recipient_id in &secret_recipients {
                if *recipient_id != owner_id && members_by_id.contains_key(recipient_id) {
                    required_ids.insert(*recipient_id);
                }
            }

            // A member who is the BANNER of a ban that will SURVIVE the step-5
            // sweep is exempt from inactivity-prune (#411 round 3 item B).
            // Otherwise an inactive moderator's bans would vanish (a banner pruned
            // to non-member has their bans swept in step 5). Mirrors the
            // `encrypted_secrets` exemption and is a pure function of converged
            // state. `self.bans.0` was ALREADY capped to `max_user_bans` at step
            // 0-cap (top of this function), so this loop iterates only the
            // surviving bans: a banner whose ban the cap evicted is NOT exempted
            // here, so it is not kept on pass 1 and then pruned on pass 2 (#411
            // round 7 / Codex P1 #2 — the cap MUST precede this exemption).
            // Runs BEFORE the invite-chain walk so the banner's ancestors are
            // kept too (a kept member needs a valid chain). The owner can still
            // explicitly ban an abusive banner — banning is separate from
            // inactivity-prune, and a banned banner's bans are then swept in step 5.
            //
            // IDEMPOTENCE (#411 round 4/5): the exemption MUST use the SAME
            // predicate as the step-5 sweep — `ban_signature_matches_current_key`,
            // NOT a bare `contains_key`. Round 4 made the sweep drop a
            // current-member-banner ban whose signature FAILS; if the exemption
            // still kept the banner on a bare membership check, a content-free
            // member P held solely by a garbage-sig ban Z would be KEPT on pass 1
            // (exempted) while Z is swept, then PRUNED on pass 2 (no ban left) —
            // so `cleanup(S) != cleanup(cleanup(S))`. Because Freenet runs
            // post_apply_cleanup a variable number of times (and full-state PUTs
            // bypass it via verify), that non-idempotence diverges the member set
            // permanently. Gating on the sweep predicate makes exemption ⟺
            // retention: a banner is exempted iff its ban actually survives. No
            // circularity — a sig-matching banner is added to `required_ids`, so it
            // survives step 3 to step 5, where the same predicate keeps its ban.
            for ban in &self.bans.0 {
                let banner = ban.banned_by;
                if banner != owner_id
                    && BansV1::ban_signature_matches_current_key(
                        ban,
                        &members_by_id,
                        owner_id,
                        &parameters.owner,
                    )
                {
                    required_ids.insert(banner);
                }
            }

            // Walk invite chains upward, adding all ancestors (stop at owner)
            let mut to_process: Vec<MemberId> = required_ids.iter().cloned().collect();
            while let Some(member_id) = to_process.pop() {
                if let Some(member) = members_by_id.get(&member_id) {
                    let inviter_id = member.member.invited_by;
                    if inviter_id != owner_id && !required_ids.contains(&inviter_id) {
                        required_ids.insert(inviter_id);
                        to_process.push(inviter_id);
                    }
                }
            }

            required_ids
        };

        // 3. Prune members not in required set
        self.members
            .members
            .retain(|m| required_ids.contains(&m.member.id()));

        // 4. Clean member_info for pruned members
        self.member_info.member_info.retain(|info| {
            info.member_info.member_id == owner_id
                || required_ids.contains(&info.member_info.member_id)
        });

        // 4a. Collapse duplicate member_info records to the single canonical
        //     (highest-rank) one per member (#411 round 8 item C / security
        //     FINDING 2+3). `verify` accepts duplicate records (migration-safety),
        //     so a peer can hold several records for one member; without this,
        //     two peers with different duplicate SETS would diverge byte-for-byte
        //     forever (the raw `member_info` vectors differ even though every
        //     canonical read agrees). Dedup is a pure function of the converged
        //     state, so it is deterministic, idempotent, and order-independent,
        //     and bounds stored `member_info` to <= one record per member. Runs
        //     here (post_apply_cleanup), never in verify/validate_state, so the
        //     permissionless migration PUT is unaffected.
        self.member_info.dedup_to_canonical();

        // 4b. Sweep recent messages authored by members removed above (deputy-
        //     authorized ban cascade or inactivity prune).
        //     `MessagesV1::apply_delta` already drops non-member-authored
        //     messages, but it runs BEFORE this cleanup in field order — so a
        //     member removed HERE (a deputy-authorized ban, #410, is only
        //     enforceable once the converged member_info is available, which is
        //     after the recent_messages field has been applied) would otherwise
        //     leave orphaned messages that fail `MessagesV1::verify`
        //     ("Message author not found"). Owner-authored messages are always
        //     valid.
        let current_member_ids: HashSet<MemberId> =
            self.members.members.iter().map(|m| m.member.id()).collect();
        self.recent_messages.messages.retain(|m| {
            m.message.author == owner_id || current_member_ids.contains(&m.message.author)
        });

        // Rebuild the PUBLIC `actions_state` cache now that removed authors'
        // messages are gone (#411 round 7 / Codex P2 #4). `MessagesV1::apply_delta`
        // already rebuilt this cache, but it ran BEFORE the sweep above, so a
        // deputy-banned member's edit/delete/reaction would linger in the cache
        // (e.g. their reaction still rendered on a message). The UI's private
        // rebuild (`rebuild_actions_state_with_decrypted`) is a no-op for a public
        // room, so nothing else recomputes it. This is the same public-only rebuild
        // `apply_delta` runs; the UI re-runs its private rebuild after apply.
        self.recent_messages.rebuild_actions_state();

        // 5. Sweep any ban that is not backed by a signature-verified authority
        //    (#411 round 3 item A.3 + round 4 item A). Nothing unvalidated stays
        //    in state (AGENTS.md State Authorization Rule). A ban is kept only if
        //    `ban_signature_matches_current_key` holds: the banner is the OWNER or
        //    a CURRENT member AND the stored signature verifies against that
        //    banner's CURRENT converged key. This drops two classes:
        //    (a) non-member banners — a stale/pruned deputy ID or forged banner,
        //        whose signature `verify` skipped and whom `is_ban_authorized`
        //        grants nothing (round 3); and
        //    (b) current-member banners whose signature does NOT match the
        //        converged key — the same-delta pruned-deputy REPLAY forgery,
        //        where a public `AuthorizedMember` was replayed to make the banner
        //        a member while `verify` skipped the garbage ban signature at
        //        apply time (round 4). Enforcement (step 0 / `banned_member_ids`)
        //        already refuses to act on such a ban; this sweeps it from state.
        //    Real member-banners with valid signatures were kept present by the
        //    item-B exemption above, so their bans survive. Runs against CONVERGED
        //    state, keeping `verify` stable (migration-safe). `members_by_id` is
        //    rebuilt here because the sweep needs each banner's `member_vk`.
        let members_by_id_for_ban_sweep = self.members.members_by_member_id();
        self.bans.0.retain(|ban| {
            BansV1::ban_signature_matches_current_key(
                ban,
                &members_by_id_for_ban_sweep,
                owner_id,
                &parameters.owner,
            )
        });

        // (The `max_user_bans` cap runs at the TOP of this function now — step
        // "0-cap" — so ban enforcement and the banner exemption read the final
        // surviving ban set. This signature sweep only shrinks the set further,
        // so the count stays <= the cap. See #411 round 7 / Codex P1 #1+#2.)

        // 6. Sweep DMs whose participants are no longer current members
        //    or are ENFORCED-banned. Without this, a fresh ban (or member-prune)
        //    would leave the DMs in state but break `verify` because the
        //    sender/recipient can no longer be resolved.
        //
        //    We use the enforced-ban set from step 0 rather than every ban
        //    target: a member whose ban is inert (e.g. a revoked deputy's ban,
        //    #410) is still a current member and their DMs must survive.
        //    Enforced-banned members were already removed above, so the
        //    active-member check alone would sweep them, but passing the set is
        //    harmless and keeps the intent explicit.
        let active_member_ids_for_sweep: HashSet<MemberId> =
            self.members.members.iter().map(|m| m.member.id()).collect();
        self.direct_messages.sweep_after_membership_change(
            owner_id,
            &active_member_ids_for_sweep,
            &enforced_banned_ids,
        );

        // 7. Re-sort for deterministic ordering
        self.members.members.sort_by_key(|m| m.member.id());
        self.member_info
            .member_info
            .sort_by_key(|info| info.member_info.member_id);

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct ChatRoomParametersV1 {
    pub owner: VerifyingKey,
}

impl ChatRoomParametersV1 {
    pub fn owner_id(&self) -> MemberId {
        self.owner.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::ban::{AuthorizedUserBan, UserBan};
    use crate::room_state::configuration::Configuration;
    use crate::room_state::member::{AuthorizedMember, Member};
    use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use ed25519_dalek::SigningKey;
    use std::fmt::Debug;
    use std::time::SystemTime;

    #[test]
    fn test_state() {
        let (state, parameters, owner_signing_key) = create_empty_chat_room_state();

        assert!(
            state.verify(&state, &parameters).is_ok(),
            "Empty state should verify"
        );

        // Test that the configuration can be updated
        let mut new_cfg = state.configuration.configuration.clone();
        new_cfg.configuration_version += 1;
        new_cfg.max_recent_messages = 10; // Change from default of 100 to 10
        let new_cfg = AuthorizedConfigurationV1::new(new_cfg, &owner_signing_key);

        let mut cfg_modified_state = state.clone();
        cfg_modified_state.configuration = new_cfg;
        test_apply_delta(state.clone(), cfg_modified_state, &parameters);
    }

    fn test_apply_delta<CS>(orig_state: CS, modified_state: CS, parameters: &CS::Parameters)
    where
        CS: ComposableState<ParentState = CS> + Clone + PartialEq + Debug,
    {
        let orig_verify_result = orig_state.verify(&orig_state, parameters);
        assert!(
            orig_verify_result.is_ok(),
            "Original state verification failed: {:?}",
            orig_verify_result.err()
        );

        let modified_verify_result = modified_state.verify(&modified_state, parameters);
        assert!(
            modified_verify_result.is_ok(),
            "Modified state verification failed: {:?}",
            modified_verify_result.err()
        );

        let delta = modified_state.delta(
            &orig_state,
            parameters,
            &orig_state.summarize(&orig_state, parameters),
        );

        println!("Delta: {:?}", delta);

        let mut new_state = orig_state.clone();
        let apply_delta_result = new_state.apply_delta(&orig_state, parameters, &delta);
        assert!(
            apply_delta_result.is_ok(),
            "Applying delta failed: {:?}",
            apply_delta_result.err()
        );

        assert_eq!(new_state, modified_state);
    }
    fn create_empty_chat_room_state() -> (ChatRoomStateV1, ChatRoomParametersV1, SigningKey) {
        // Create a test room_state with a single member and two messages, one written by
        // the owner and one by the member - the member must be invited by the owner
        let rng = &mut rand::thread_rng();
        let owner_signing_key = SigningKey::generate(rng);
        let owner_verifying_key = owner_signing_key.verifying_key();

        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_signing_key);

        (
            ChatRoomStateV1 {
                configuration: config,
                bans: BansV1::default(),
                members: MembersV1::default(),
                member_info: MemberInfoV1::default(),
                secrets: RoomSecretsV1::default(),
                recent_messages: MessagesV1::default(),
                upgrade: OptionalUpgradeV1(None),
                ..Default::default()
            },
            ChatRoomParametersV1 {
                owner: owner_verifying_key,
            },
            owner_signing_key,
        )
    }

    /// Regression test: when a member who issued bans is subsequently banned themselves,
    /// their bans become orphaned (banning member no longer in members list and not owner).
    /// The post_apply_delta hook post_apply_cleanup must remove these to prevent verify() failure.
    /// See: technic corrupted state incident (Feb 2026)
    #[test]
    fn test_orphaned_ban_cleanup_after_cascade_removal() {
        let rng = &mut rand::thread_rng();

        // Create owner
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        // Configuration allowing bans and members
        let config = Configuration {
            max_user_bans: 10,
            max_members: 10,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // Create member A (invited by owner) and member B (invited by A)
        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let b_sk = SigningKey::generate(rng);
        let b_vk = b_sk.verifying_key();
        let b_id = MemberId::from(&b_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A bans B (authorized because A is in B's invite chain)
        let ban_b_by_a = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: std::time::SystemTime::now(),
                banned_user: b_id,
            },
            a_id,
            &a_sk,
        );

        // Initial state: A is a member, B already removed (ban took effect)
        let initial_state = ChatRoomStateV1 {
            configuration: auth_config.clone(),
            bans: BansV1(vec![ban_b_by_a.clone()]),
            members: MembersV1 {
                members: vec![member_a.clone()],
            },
            ..Default::default()
        };

        assert!(
            initial_state.verify(&initial_state, &params).is_ok(),
            "Initial state should verify: {:?}",
            initial_state.verify(&initial_state, &params)
        );

        // Now owner bans A — this will cascade-remove A from members,
        // making A's ban of B orphaned (A is no longer in members and not owner)
        let ban_a_by_owner = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: std::time::SystemTime::now() + std::time::Duration::from_secs(1),
                banned_user: a_id,
            },
            owner_id,
            &owner_sk,
        );

        // Modified state for delta computation: add owner's ban of A
        let modified_for_delta = ChatRoomStateV1 {
            configuration: auth_config,
            bans: BansV1(vec![ban_b_by_a.clone(), ban_a_by_owner.clone()]),
            members: MembersV1 {
                members: vec![member_a.clone()],
            },
            ..Default::default()
        };

        // Compute and apply delta
        let summary = initial_state.summarize(&initial_state, &params);
        let delta = modified_for_delta.delta(&initial_state, &params, &summary);

        let mut result_state = initial_state.clone();
        let apply_result = result_state.apply_delta(&initial_state, &params, &delta);
        assert!(
            apply_result.is_ok(),
            "apply_delta should succeed: {:?}",
            apply_result
        );

        // A should be removed (banned by owner)
        assert!(
            result_state.members.members.is_empty(),
            "A should be removed from members: {:?}",
            result_state.members.members
        );

        // Only owner's ban should remain — A's ban of B is orphaned and cleaned
        assert_eq!(
            result_state.bans.0.len(),
            1,
            "Only owner's ban should remain, orphaned ban cleaned: {:?}",
            result_state.bans.0
        );
        assert_eq!(
            result_state.bans.0[0].banned_by, owner_id,
            "Remaining ban should be by owner"
        );

        // Result state should pass verification
        assert!(
            result_state.verify(&result_state, &params).is_ok(),
            "Result state should verify after orphaned ban cleanup: {:?}",
            result_state.verify(&result_state, &params)
        );
    }

    #[test]
    fn test_member_pruned_when_no_messages() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let b_sk = SigningKey::generate(rng);
        let b_vk = b_sk.verifying_key();

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );
        let member_b = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: b_vk,
            },
            &owner_sk,
        );

        // Only A has a message
        let msg_a = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: a_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("Hello from A".to_string()),
            },
            &a_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a, member_b],
            },
            recent_messages: MessagesV1 {
                messages: vec![msg_a],
                ..Default::default()
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        assert_eq!(state.members.members.len(), 1, "Only A should remain");
        assert_eq!(state.members.members[0].member.id(), a_id);
    }

    #[test]
    fn test_member_with_join_event_not_pruned() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A has only a join event (no regular messages)
        let join_msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: a_id,
                time: SystemTime::now(),
                content: RoomMessageBody::join_event(),
            },
            &a_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            recent_messages: MessagesV1 {
                messages: vec![join_msg],
                ..Default::default()
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        assert_eq!(
            state.members.members.len(),
            1,
            "Member with join event should not be pruned"
        );
        assert_eq!(state.members.members[0].member.id(), a_id);
    }

    /// Test that the atomic join delta (members + member_info + join event)
    /// as produced by accept_invitation applies correctly and passes verify().
    #[test]
    fn test_atomic_join_delta_applies_and_verifies() {
        use crate::room_state::member::MembersDelta;
        use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
        use crate::room_state::privacy::SealedBytes;

        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        // Create a room with owner config
        let config = Configuration {
            owner_member_id: owner_id,
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);
        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            ..Default::default()
        };

        // New member accepts an invitation
        let joiner_sk = SigningKey::generate(rng);
        let joiner_vk = joiner_sk.verifying_key();
        let joiner_id = MemberId::from(&joiner_vk);

        let authorized_member = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: joiner_vk,
            },
            &owner_sk,
        );

        let member_info = MemberInfo {
            member_id: joiner_id,
            version: 0,
            preferred_nickname: SealedBytes::public("NewUser".to_string().into_bytes()),
            deputies: Vec::new(),
        };
        let authorized_info = AuthorizedMemberInfo::new_with_member_key(member_info, &joiner_sk);

        let join_message = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: joiner_id,
                content: RoomMessageBody::join_event(),
                time: SystemTime::now(),
            },
            &joiner_sk,
        );

        // Build the atomic delta (same as accept_invitation produces)
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![join_message]),
            members: Some(MembersDelta::new(vec![authorized_member])),
            member_info: Some(vec![authorized_info]),
            ..Default::default()
        };

        // Apply delta
        let old_state = state.clone();
        state
            .apply_delta(&old_state, &params, &Some(delta))
            .expect("atomic join delta should apply cleanly");

        // Verify state is valid
        state
            .verify(&state, &params)
            .expect("state should verify after join delta");

        // Member should be present
        assert!(
            state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == joiner_id),
            "Joiner should be in members list"
        );

        // Member info should be present
        assert!(
            state
                .member_info
                .member_info
                .iter()
                .any(|i| i.member_info.member_id == joiner_id),
            "Joiner should have member_info"
        );

        // Join event message should be present
        assert_eq!(state.recent_messages.messages.len(), 1);
        assert!(state.recent_messages.messages[0].message.content.is_event());

        // Should survive post_apply_cleanup
        state.post_apply_cleanup(&params).unwrap();
        assert!(
            state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == joiner_id),
            "Joiner should survive cleanup"
        );
    }

    #[test]
    fn test_invite_chain_preserved_for_active_member() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let b_sk = SigningKey::generate(rng);
        let b_vk = b_sk.verifying_key();
        let b_id = MemberId::from(&b_vk);

        // Owner → A → B
        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );
        let member_b = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: a_id,
                member_vk: b_vk,
            },
            &a_sk,
        );

        // Only B has a message
        let msg_b = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: b_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("Hello from B".to_string()),
            },
            &b_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a, member_b],
            },
            recent_messages: MessagesV1 {
                messages: vec![msg_b],
                ..Default::default()
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        // Both A and B should remain (A is in B's invite chain)
        assert_eq!(state.members.members.len(), 2);
        let member_ids: HashSet<MemberId> = state
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();
        assert!(
            member_ids.contains(&a_id),
            "A should be kept (in B's invite chain)"
        );
        assert!(
            member_ids.contains(&b_id),
            "B should be kept (has messages)"
        );
    }

    /// #411 round 3 item B: a member who is the banner of a retained ban is
    /// EXEMPT from inactivity-pruning, so their ban does not vanish. (Before
    /// round 3 the banner was pruned and the ban persisted anyway; now the ban
    /// persists BECAUSE the banner is kept present, which is what keeps it valid
    /// under the round-3 "banner must be a current member" sweep.)
    #[test]
    fn test_banner_exempt_from_inactivity_prune_so_ban_persists() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let c_sk = SigningKey::generate(rng);
        let c_vk = c_sk.verifying_key();
        let c_id = MemberId::from(&c_vk);

        // A is a member (invited by owner)
        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A bans C
        let ban_c_by_a = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: c_id,
            },
            a_id,
            &a_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_user_bans: 10,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // A has no messages, but A is the banner of a retained ban → exempt
        // from inactivity-prune (round 3 item B).
        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            bans: BansV1(vec![ban_c_by_a]),
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        // A is KEPT (exempt as a banner), not pruned.
        assert_eq!(
            state.members.members.len(),
            1,
            "A should be exempt from prune"
        );
        assert_eq!(state.members.members[0].member.id(), a_id);

        // A's ban of C persists (banner A is still a current member).
        assert_eq!(state.bans.0.len(), 1, "Ban should persist");
        assert_eq!(state.bans.0[0].ban.banned_user, c_id);
        assert_eq!(state.bans.0[0].banned_by, a_id);
    }

    /// #411 round 7 / Codex P1 #1: an over-cap ban that WILL be evicted by the
    /// `max_user_bans` cap must NOT one-shot-remove its target. If enforcement
    /// ran before the cap (the bug), the evicted ban would still have removed a
    /// member the capped converged state cannot reproduce — so a peer that only
    /// ever sees the capped state keeps that member and the two diverge.
    #[test]
    fn over_cap_ban_does_not_one_shot_remove() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        // Only ONE ban may survive.
        let config = Configuration {
            max_members: 10,
            max_user_bans: 1,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // Present member T with a message, so T is retained unless banned.
        let t_sk = SigningKey::generate(rng);
        let t_vk = t_sk.verifying_key();
        let t_id = MemberId::from(&t_vk);
        let member_t = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: t_vk,
            },
            &owner_sk,
        );
        let msg_t = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: t_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("hi".to_string()),
            },
            &t_sk,
        );

        let base = SystemTime::now();
        // Ban of the PRESENT member T, OLDEST → evicted by the cap.
        let ban_t = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: base,
                banned_user: t_id,
            },
            owner_id,
            &owner_sk,
        );
        // Ban of an ABSENT user, NEWER → survives the cap.
        let absent = MemberId::from(&SigningKey::generate(rng).verifying_key());
        let ban_absent = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: base + std::time::Duration::from_secs(10),
                banned_user: absent,
            },
            owner_id,
            &owner_sk,
        );

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_t],
            },
            recent_messages: MessagesV1 {
                messages: vec![msg_t],
                ..Default::default()
            },
            bans: BansV1(vec![ban_t, ban_absent]),
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        assert!(
            state.members.members.iter().any(|m| m.member.id() == t_id),
            "an over-cap ban that gets evicted must NOT one-shot-remove its target"
        );
        assert_eq!(state.bans.0.len(), 1, "capped to max_user_bans");
        assert!(
            state.bans.0.iter().any(|b| b.ban.banned_user == absent),
            "the surviving ban is the newer absent-target one"
        );
    }

    /// #411 round 7 / Codex P1 #2: a banner whose ban the `max_user_bans` cap
    /// evicts must lose their prune exemption on the SAME pass. If the cap ran
    /// AFTER the exemption (the bug), the banner is kept on pass 1 and pruned on
    /// pass 2 — `cleanup(S) != cleanup(cleanup(S))` — permanently diverging peers
    /// that run cleanup a different number of times.
    #[test]
    fn cleanup_is_idempotent_over_cap_evicted_ban() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let config = Configuration {
            max_members: 10,
            max_user_bans: 2,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // Member B: a current member with NO message/DM/secret — only being a
        // retained banner could keep them from the inactivity prune.
        let b_sk = SigningKey::generate(rng);
        let b_vk = b_sk.verifying_key();
        let b_id = MemberId::from(&b_vk);
        let member_b = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: b_vk,
            },
            &owner_sk,
        );

        let base = SystemTime::now();
        // Two owner bans against ABSENT targets, NEWER → survive the cap.
        let absent1 = MemberId::from(&SigningKey::generate(rng).verifying_key());
        let absent2 = MemberId::from(&SigningKey::generate(rng).verifying_key());
        let owner_ban1 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: base + std::time::Duration::from_secs(10),
                banned_user: absent1,
            },
            owner_id,
            &owner_sk,
        );
        let owner_ban2 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: base + std::time::Duration::from_secs(11),
                banned_user: absent2,
            },
            owner_id,
            &owner_sk,
        );
        // B's ban against an ABSENT target, OLDEST → evicted by the cap. It is
        // "enforcing" (member banner, absent target) so ONLY the cap removes it.
        let c_id = MemberId::from(&SigningKey::generate(rng).verifying_key());
        let ban_c_by_b = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: base,
                banned_user: c_id,
            },
            b_id,
            &b_sk,
        );

        let state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_b],
            },
            bans: BansV1(vec![ban_c_by_b, owner_ban1, owner_ban2]),
            ..Default::default()
        };

        let mut once = state.clone();
        once.post_apply_cleanup(&params).unwrap();

        // B's ban is cap-evicted, so B is NOT exempted and is pruned on pass 1.
        assert!(
            !once.members.members.iter().any(|m| m.member.id() == b_id),
            "over-cap-evicted banner B must be pruned on the FIRST cleanup pass"
        );
        assert_eq!(once.bans.0.len(), 2, "capped to max_user_bans");

        // Idempotence: a second pass changes nothing.
        let mut twice = once.clone();
        twice.post_apply_cleanup(&params).unwrap();
        assert_eq!(once, twice, "post_apply_cleanup must be idempotent");
    }

    /// #411 round 7 / Codex P2 #4: after a deputy-banned member's messages are
    /// swept in cleanup, the PUBLIC `actions_state` cache must be rebuilt so
    /// their reaction no longer lingers (the UI's private rebuild is a no-op for
    /// a public room).
    #[test]
    fn banned_member_reaction_removed_from_actions_state_cache() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let config = Configuration {
            max_members: 10,
            max_user_bans: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // M authors a message; R reacts to it. Both are members.
        let m_sk = SigningKey::generate(rng);
        let m_vk = m_sk.verifying_key();
        let m_id = MemberId::from(&m_vk);
        let r_sk = SigningKey::generate(rng);
        let r_vk = r_sk.verifying_key();
        let r_id = MemberId::from(&r_vk);
        let member_m = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: m_vk,
            },
            &owner_sk,
        );
        let member_r = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: r_vk,
            },
            &owner_sk,
        );

        let msg1 = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: m_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("hello".to_string()),
            },
            &m_sk,
        );
        let msg1_id = msg1.id();
        let reaction = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: r_id,
                time: SystemTime::now() + std::time::Duration::from_secs(1),
                content: RoomMessageBody::reaction(msg1_id.clone(), "👍".to_string()),
            },
            &r_sk,
        );

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_m, member_r],
            },
            recent_messages: MessagesV1 {
                messages: vec![msg1.clone(), reaction],
                ..Default::default()
            },
            ..Default::default()
        };

        // Populate the actions_state cache as `apply_delta` would, WITH R present.
        state.recent_messages.rebuild_actions_state();
        assert!(
            state
                .recent_messages
                .reactions(&msg1_id)
                .and_then(|r| r.get("👍"))
                .is_some_and(|v| v.contains(&r_id)),
            "sanity: R's reaction is present in the cache before the ban"
        );

        // Owner bans R; cleanup removes R + their reaction message.
        state.bans.0.push(AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: r_id,
            },
            owner_id,
            &owner_sk,
        ));

        state.post_apply_cleanup(&params).unwrap();

        assert!(
            !state.members.members.iter().any(|m| m.member.id() == r_id),
            "R is banned and removed"
        );
        let lingers = state
            .recent_messages
            .reactions(&msg1_id)
            .and_then(|r| r.get("👍"))
            .is_some_and(|v| v.contains(&r_id));
        assert!(
            !lingers,
            "the banned member's reaction must be gone from the rebuilt public \
             actions_state cache"
        );
    }

    /// #411 round 7: `post_apply_cleanup` must be idempotent on adversarial
    /// states (running it twice yields the same state), otherwise peers that run
    /// cleanup a different number of times diverge.
    #[test]
    fn post_apply_cleanup_is_idempotent_on_adversarial_states() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let cfg = |max_bans: usize| {
            AuthorizedConfigurationV1::new(
                Configuration {
                    max_members: 50,
                    max_user_bans: max_bans,
                    max_recent_messages: 100,
                    ..Default::default()
                },
                &owner_sk,
            )
        };
        let owner_member = |vk| {
            AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: vk,
                },
                &owner_sk,
            )
        };
        let assert_idem = |state: &ChatRoomStateV1, label: &str| {
            let mut once = state.clone();
            once.post_apply_cleanup(&params).unwrap();
            let mut twice = once.clone();
            twice.post_apply_cleanup(&params).unwrap();
            assert_eq!(once, twice, "post_apply_cleanup not idempotent: {}", label);
        };

        // State A: five member-banners of ABSENT targets, over a cap of 2. Three
        // are cap-evicted (losing their exemption); two survive.
        {
            let banners: Vec<SigningKey> = (0..5).map(|_| SigningKey::generate(rng)).collect();
            let members: Vec<AuthorizedMember> = banners
                .iter()
                .map(|sk| owner_member(sk.verifying_key()))
                .collect();
            let base = SystemTime::now();
            let bans: Vec<AuthorizedUserBan> = banners
                .iter()
                .enumerate()
                .map(|(i, sk)| {
                    let absent = MemberId::from(&SigningKey::generate(rng).verifying_key());
                    AuthorizedUserBan::new(
                        UserBan {
                            owner_member_id: owner_id,
                            banned_at: base + std::time::Duration::from_secs(i as u64),
                            banned_user: absent,
                        },
                        MemberId::from(&sk.verifying_key()),
                        sk,
                    )
                })
                .collect();
            let state = ChatRoomStateV1 {
                configuration: cfg(2),
                members: MembersV1 { members },
                bans: BansV1(bans),
                ..Default::default()
            };
            assert_idem(&state, "member-banners over cap");
        }

        // State B: owner cascade-bans A (who had banned B), plus an over-cap
        // flood of inert member bans against a present member X.
        {
            let a_sk = SigningKey::generate(rng);
            let a_id = MemberId::from(&a_sk.verifying_key());
            let b_id = MemberId::from(&SigningKey::generate(rng).verifying_key());
            let x_sk = SigningKey::generate(rng);
            let x_id = MemberId::from(&x_sk.verifying_key());
            let flood_banners: Vec<SigningKey> =
                (0..4).map(|_| SigningKey::generate(rng)).collect();

            let mut members = vec![
                owner_member(a_sk.verifying_key()),
                owner_member(x_sk.verifying_key()),
            ];
            for sk in &flood_banners {
                members.push(owner_member(sk.verifying_key()));
            }

            let base = SystemTime::now();
            let mut bans = vec![
                // A banned B (A is a member banner; B absent).
                AuthorizedUserBan::new(
                    UserBan {
                        owner_member_id: owner_id,
                        banned_at: base,
                        banned_user: b_id,
                    },
                    a_id,
                    &a_sk,
                ),
                // Owner bans A (cascade removes A → A's ban of B is orphaned).
                AuthorizedUserBan::new(
                    UserBan {
                        owner_member_id: owner_id,
                        banned_at: base + std::time::Duration::from_secs(1),
                        banned_user: a_id,
                    },
                    owner_id,
                    &owner_sk,
                ),
            ];
            // Inert flood: each floods a ban of present member X (no authority).
            for (i, sk) in flood_banners.iter().enumerate() {
                bans.push(AuthorizedUserBan::new(
                    UserBan {
                        owner_member_id: owner_id,
                        banned_at: base + std::time::Duration::from_secs(100 + i as u64),
                        banned_user: x_id,
                    },
                    MemberId::from(&sk.verifying_key()),
                    sk,
                ));
            }

            let state = ChatRoomStateV1 {
                configuration: cfg(3),
                members: MembersV1 { members },
                bans: BansV1(bans),
                ..Default::default()
            };
            assert_idem(&state, "owner cascade + inert flood over cap");
        }
    }

    #[test]
    fn test_member_re_added_with_message() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // State with A but no messages
        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a.clone()],
            },
            ..Default::default()
        };

        // Cleanup prunes A
        state.post_apply_cleanup(&params).unwrap();
        assert!(state.members.members.is_empty(), "A should be pruned");

        // Re-add A with a message
        state.members.members.push(member_a);
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: a_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("Hello again!".to_string()),
            },
            &a_sk,
        );
        state.recent_messages.messages.push(msg);

        // Cleanup should keep A now
        state.post_apply_cleanup(&params).unwrap();
        assert_eq!(state.members.members.len(), 1, "A should be kept");
        assert_eq!(state.members.members[0].member.id(), a_id);
    }

    #[test]
    fn test_member_info_cleaned_after_pruning() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // Create member_info for A and owner
        let a_info = AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(a_id, 1, "Alice".to_string()),
            &a_sk,
        );
        let owner_info = AuthorizedMemberInfo::new(
            MemberInfo::new_public(owner_id, 1, "Owner".to_string()),
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            member_info: MemberInfoV1 {
                member_info: vec![owner_info, a_info],
            },
            ..Default::default()
        };

        // A has no messages → gets pruned along with their member_info
        state.post_apply_cleanup(&params).unwrap();

        assert!(state.members.members.is_empty(), "A should be pruned");
        assert_eq!(
            state.member_info.member_info.len(),
            1,
            "Only owner's info should remain"
        );
        assert_eq!(
            state.member_info.member_info[0].member_info.member_id, owner_id,
            "Remaining info should be owner's"
        );
    }

    /// Regression test for issue #110 / Bug #3 PR B:
    ///
    /// A member with an `encrypted_secrets` entry (i.e. the owner has
    /// issued them a per-version room-secret blob) must survive
    /// `post_apply_cleanup` even if they have not yet authored any
    /// messages and have no active DMs. The owner-issued blob is proof
    /// that the owner considers them a member, and pruning them on the
    /// invitee's first state ingestion is the underlying cause of the
    /// "DM to inactive member fails" / "newly-invited member silently
    /// pruned" symptom Ivvor reported in Bug #3.
    #[test]
    fn test_member_with_encrypted_secret_survives_cleanup() {
        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A has NO messages and NO DMs — under the pre-fix rules they
        // would be pruned by post_apply_cleanup. The owner-issued
        // encrypted secret is the only evidence of membership.
        let secret_for_a = crate::room_state::secret::EncryptedSecretForMemberV1 {
            member_id: a_id,
            secret_version: 0,
            ciphertext: vec![0u8; 16], // dummy ciphertext — signature is what counts
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: owner_id,
        };
        let authorized_secret = crate::room_state::secret::AuthorizedEncryptedSecretForMember::new(
            secret_for_a,
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 0,
                versions: vec![],
                encrypted_secrets: vec![authorized_secret],
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        assert_eq!(
            state.members.members.len(),
            1,
            "A should survive cleanup because they have an encrypted_secrets entry"
        );
        assert_eq!(state.members.members[0].member.id(), a_id);
    }

    /// IMPORTANT #4 (PR #272 review round 2): a member who is BOTH
    /// banned AND has a stale `encrypted_secrets` blob must still be
    /// pruned by `post_apply_cleanup`. The exemption introduced for
    /// issue #110 grants survival on the strength of the owner's
    /// blob, but bans must override — a ban is the owner's later,
    /// authoritative statement that this member is no longer trusted.
    ///
    /// The `members_by_id.contains_key(recipient_id)` guard at the
    /// cleanup site keeps this safe: the ban delta runs through the
    /// member-prune path before `post_apply_cleanup`'s `required_ids`
    /// collection, so by the time we check the exemption set, the
    /// banned member is no longer in `members_by_id` and the
    /// exemption clause is short-circuited. This test pins that
    /// behaviour against any future regression that loosens the
    /// guard.
    #[test]
    fn test_banned_member_with_encrypted_secret_is_still_pruned() {
        use crate::room_state::ban::{AuthorizedUserBan, UserBan};
        use std::time::SystemTime;

        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        let ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: a_id,
            },
            owner_id,
            &owner_sk,
        );

        // Owner issued a v0 blob for A, then banned A. The blob
        // outlives the ban in the state (a peer might receive both
        // deltas in one batch). Without proper handling, the
        // exemption would resurrect A.
        let secret_for_a = crate::room_state::secret::EncryptedSecretForMemberV1 {
            member_id: a_id,
            secret_version: 0,
            ciphertext: vec![0u8; 16],
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: owner_id,
        };
        let authorized_secret = crate::room_state::secret::AuthorizedEncryptedSecretForMember::new(
            secret_for_a,
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            bans: crate::room_state::ban::BansV1(vec![ban]),
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 0,
                versions: vec![],
                encrypted_secrets: vec![authorized_secret],
            },
            ..Default::default()
        };

        // The owner-side flow is: apply ban delta -> members.apply_delta
        // removes A from members -> post_apply_cleanup runs. We
        // simulate the post-ban-prune state by manually removing A
        // from members (matching what `MembersV1::apply_delta` does
        // when it sees the ban), then run cleanup.
        state.members.members.retain(|m| m.member.id() != a_id);

        state.post_apply_cleanup(&params).unwrap();

        assert!(
            state.members.members.is_empty(),
            "banned member A must NOT be resurrected by post_apply_cleanup's \
             encrypted_secrets exemption — see IMPORTANT #4 on PR #272 review round 2"
        );
        // The ban itself must persist.
        assert_eq!(state.bans.0.len(), 1);
        assert_eq!(state.bans.0[0].ban.banned_user, a_id);
    }

    /// IMPORTANT #5 (PR #272 review round 2): the
    /// `encrypted_secrets` exemption from `post_apply_cleanup` must
    /// be SCOPED to the current secret version. A member who has
    /// only old-version blobs and hasn't been re-issued at
    /// `current_version` is "stale" by the same definition as a
    /// member who joined and never authored, and must be pruned.
    ///
    /// Without this TTL, every ever-recipient + their entire
    /// invite-chain ancestor set would be exempt from cleanup
    /// forever — defeating the whole point of the inactivity prune.
    #[test]
    fn test_stale_secret_recipient_is_pruned_after_rotation() {
        use crate::room_state::privacy::RoomCipherSpec;
        use crate::room_state::secret::{AuthorizedSecretVersionRecord, SecretVersionRecordV1};
        use std::time::SystemTime;

        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let a_sk = SigningKey::generate(rng);
        let a_vk = a_sk.verifying_key();
        let a_id = MemberId::from(&a_vk);

        let member_a = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: a_vk,
            },
            &owner_sk,
        );

        // A only has a v0 blob. The room has since rotated to v1
        // and A was not re-issued (e.g. they left / were
        // implicitly inactive at rotation time).
        let secret_for_a = crate::room_state::secret::EncryptedSecretForMemberV1 {
            member_id: a_id,
            secret_version: 0,
            ciphertext: vec![0u8; 16],
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: owner_id,
        };
        let authorized_secret_v0 =
            crate::room_state::secret::AuthorizedEncryptedSecretForMember::new(
                secret_for_a,
                &owner_sk,
            );

        let v1_record = AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 1,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::now(),
            },
            &owner_sk,
        );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_a],
            },
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 1,
                versions: vec![v1_record],
                encrypted_secrets: vec![authorized_secret_v0],
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        assert!(
            state.members.members.is_empty(),
            "member A with ONLY a stale v0 blob (no v1 re-issue, no messages, no \
             DMs) must be pruned — see IMPORTANT #5 on PR #272 review round 2"
        );
    }

    /// IMPORTANT #6 (PR #272 review round 2): ban-race convergence
    /// across peers receiving deltas in different orders. Both
    /// orderings — (add-X, ban-X) and (ban-X, add-X) — must
    /// converge with X removed, regardless of whether the
    /// owner-issued `encrypted_secret` for X arrives before or
    /// after the ban.
    ///
    /// This is the same convergence test pattern PR #240 used for
    /// DMs but applied to the new encrypted_secrets exemption.
    /// Without this test, a future regression that loosens the
    /// "members_by_id.contains_key" guard could leak X back into
    /// state via the exemption when the deltas land in the
    /// "wrong" order.
    #[test]
    fn test_ban_race_with_encrypted_secret_converges_to_pruned() {
        use crate::room_state::ban::{AuthorizedUserBan, UserBan};
        use std::time::SystemTime;

        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let x_sk = SigningKey::generate(rng);
        let x_vk = x_sk.verifying_key();
        let x_id = MemberId::from(&x_vk);

        let member_x = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: x_vk,
            },
            &owner_sk,
        );

        let ban_x = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: x_id,
            },
            owner_id,
            &owner_sk,
        );

        let secret_for_x = crate::room_state::secret::EncryptedSecretForMemberV1 {
            member_id: x_id,
            secret_version: 0,
            ciphertext: vec![0u8; 16],
            nonce: [0u8; 12],
            sender_ephemeral_public_key: [0u8; 32],
            provider: owner_id,
        };
        let authorized_secret_x =
            crate::room_state::secret::AuthorizedEncryptedSecretForMember::new(
                secret_for_x,
                &owner_sk,
            );

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // Build the FINAL converged state both peers should arrive
        // at: X is banned, X is not in members, the v0
        // encrypted_secret for X may or may not be present
        // depending on whether peer's secrets state pruned it.
        // We simulate the post-merge state where both deltas have
        // landed; the in-flight blob for X is still in state when
        // post_apply_cleanup runs.
        //
        // Peer A: applied [add-X@t0, ban-X@t1] — members.apply_delta
        // saw the ban and removed X from members. Then the
        // secrets delta arrived with a v0 blob for X. Final state:
        // members = [], bans = [ban-X], encrypted_secrets = [(x, 0)].
        let mut peer_a_state = ChatRoomStateV1 {
            configuration: auth_config.clone(),
            members: MembersV1 { members: vec![] },
            bans: crate::room_state::ban::BansV1(vec![ban_x.clone()]),
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 0,
                versions: vec![],
                encrypted_secrets: vec![authorized_secret_x.clone()],
            },
            ..Default::default()
        };
        peer_a_state.post_apply_cleanup(&params).unwrap();
        assert!(
            peer_a_state.members.members.is_empty(),
            "peer A: X must remain pruned despite the encrypted_secret being present"
        );

        // Peer B: applied [ban-X@t1, add-X@t0]. ban-X was applied
        // first; add-X arrived later but was rejected by
        // `MembersV1::apply_delta` because X is in the ban list.
        // Then the secrets delta arrived with a v0 blob for X.
        // Final state matches peer A's.
        let mut peer_b_state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 { members: vec![] },
            bans: crate::room_state::ban::BansV1(vec![ban_x]),
            secrets: crate::room_state::secret::RoomSecretsV1 {
                current_version: 0,
                versions: vec![],
                encrypted_secrets: vec![authorized_secret_x],
            },
            ..Default::default()
        };
        peer_b_state.post_apply_cleanup(&params).unwrap();
        assert!(
            peer_b_state.members.members.is_empty(),
            "peer B: X must remain pruned despite the encrypted_secret being present"
        );

        // The two peers must converge to byte-identical members /
        // bans / encrypted_secrets state.
        assert_eq!(peer_a_state.members, peer_b_state.members);
        assert_eq!(peer_a_state.bans, peer_b_state.bans);
        assert_eq!(peer_a_state.secrets, peer_b_state.secrets);

        // Suppress unused-variable lints — `member_x` is the seed
        // we used to derive `x_id` / `x_vk`; the convergence test
        // checks the AFTER-merge state where members is already
        // empty by construction.
        let _ = member_x;
    }

    #[test]
    fn test_state_with_none_deltas() {
        let (state, parameters, owner_signing_key) = create_empty_chat_room_state();

        // Create a modified room_state with no changes (all deltas should be None)
        let modified_state = state.clone();

        // Apply the delta
        let summary = state.summarize(&state, &parameters);
        let delta = modified_state.delta(&state, &parameters, &summary);

        assert!(
            delta.is_none(),
            "Delta should be None when no changes are made"
        );

        // Now, let's modify only one field and check if other deltas are None
        let mut partially_modified_state = state.clone();
        let new_config = Configuration {
            configuration_version: 2,
            ..partially_modified_state.configuration.configuration.clone()
        };
        partially_modified_state.configuration =
            AuthorizedConfigurationV1::new(new_config, &owner_signing_key);

        let summary = state.summarize(&state, &parameters);
        let delta = partially_modified_state
            .delta(&state, &parameters, &summary)
            .unwrap();

        // Check that only the configuration delta is Some, and others are None
        assert!(
            delta.configuration.is_some(),
            "Configuration delta should be Some"
        );
        assert!(delta.bans.is_none(), "Bans delta should be None");
        assert!(delta.members.is_none(), "Members delta should be None");
        assert!(
            delta.member_info.is_none(),
            "Member info delta should be None"
        );
        assert!(
            delta.recent_messages.is_none(),
            "Recent messages delta should be None"
        );
        assert!(delta.upgrade.is_none(), "Upgrade delta should be None");

        // Apply the partial delta
        let mut new_state = state.clone();
        new_state
            .apply_delta(&state, &parameters, &Some(delta))
            .unwrap();

        assert_eq!(
            new_state, partially_modified_state,
            "State should be partially modified"
        );
    }

    /// Regression test for freenet/river#127: the contract-migration upgrade
    /// pointer is delivered to the old contract as a minimal `apply_delta`,
    /// NOT as a full-state UPDATE.
    ///
    /// A full `UpdateData::State` is run through the old contract's
    /// `validate_state` -> `ChatRoomStateV1::verify`. The old code built that
    /// state with `..Default::default()`, whose `configuration` is unsigned,
    /// so verification failed with "Invalid signature" and the pointer never
    /// landed. Applying only the `upgrade` field as a delta runs
    /// `OptionalUpgradeV1::apply_delta`, which validates just the upgrade
    /// signature against the contract's owner parameter.
    #[test]
    fn test_upgrade_pointer_applies_as_delta() {
        use crate::room_state::upgrade::{AuthorizedUpgradeV1, UpgradeV1};

        let (state, parameters, owner_signing_key) = create_empty_chat_room_state();
        // Sanity: the baseline room state is itself valid and has no pointer.
        assert!(
            state.verify(&state, &parameters).is_ok(),
            "baseline room state should verify"
        );
        assert!(
            state.upgrade.0.is_none(),
            "baseline room state should have no upgrade pointer"
        );

        let upgrade = UpgradeV1 {
            owner_member_id: MemberId::from(&parameters.owner),
            version: 1,
            new_chatroom_address: blake3::Hash::from([7u8; 32]),
        };
        let authorized = AuthorizedUpgradeV1::new(upgrade, &owner_signing_key);

        // FIX: apply the upgrade pointer as a minimal delta — the path the old
        // contract takes for `UpdateData::Delta`. The pointer must land and the
        // resulting state must still verify.
        let delta = ChatRoomStateV1Delta {
            upgrade: Some(authorized.clone()),
            ..Default::default()
        };
        let mut updated = state.clone();
        updated
            .apply_delta(&state, &parameters, &Some(delta))
            .expect("applying the upgrade-pointer delta must succeed");
        assert_eq!(
            updated.upgrade.0.as_ref(),
            Some(&authorized),
            "the upgrade pointer must land on the old contract's state"
        );
        assert!(
            updated.verify(&updated, &parameters).is_ok(),
            "the state with the upgrade pointer applied must still verify"
        );

        // Why not a full-state UPDATE: a `..Default::default()` state — the old
        // (#127) approach — fails `verify` because its default `configuration`
        // is unsigned, so the runtime's `validate_state` rejected it.
        let buggy_full_state = ChatRoomStateV1 {
            upgrade: OptionalUpgradeV1(Some(authorized)),
            ..Default::default()
        };
        assert!(
            buggy_full_state
                .verify(&buggy_full_state, &parameters)
                .is_err(),
            "a `..Default::default()` full-state upgrade must fail verification (the #127 bug)"
        );
    }

    /// #411 round 8 item C: a state carrying DUPLICATE member_info records for a
    /// member (which `verify` accepts) is collapsed by `post_apply_cleanup` to
    /// exactly one canonical (highest-rank) record per member — killing the
    /// duplicate-SET byte-divergence between peers and bounding stored size.
    #[test]
    fn post_apply_cleanup_dedups_member_info_to_canonical() {
        use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};

        let rng = &mut rand::thread_rng();
        let owner_sk = SigningKey::generate(rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let config = Configuration {
            max_members: 10,
            max_recent_messages: 100,
            ..Default::default()
        };
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        // Member M kept present (has a message), with TWO member_info records:
        // grant @ v1 (a deputy) and revoke @ v2 (empty).
        let m_sk = SigningKey::generate(rng);
        let m_vk = m_sk.verifying_key();
        let m_id = MemberId::from(&m_vk);
        let member_m = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: m_vk,
            },
            &owner_sk,
        );
        let msg_m = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: m_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("hi".to_string()),
            },
            &m_sk,
        );

        let dep = MemberId::from(&SigningKey::generate(rng).verifying_key());
        let mut g = MemberInfo::new_public(m_id, 1, "nick".to_string());
        g.deputies = vec![dep];
        let grant = AuthorizedMemberInfo::new_with_member_key(g, &m_sk);
        let mut r = MemberInfo::new_public(m_id, 2, "nick".to_string());
        r.deputies = vec![];
        let revoke = AuthorizedMemberInfo::new_with_member_key(r, &m_sk);

        let mut state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 {
                members: vec![member_m],
            },
            member_info: MemberInfoV1 {
                // Duplicate: grant FIRST so a naive first-match would keep it.
                member_info: vec![grant, revoke],
            },
            recent_messages: MessagesV1 {
                messages: vec![msg_m],
                ..Default::default()
            },
            ..Default::default()
        };

        state.post_apply_cleanup(&params).unwrap();

        let recs: Vec<_> = state
            .member_info
            .member_info
            .iter()
            .filter(|i| i.member_info.member_id == m_id)
            .collect();
        assert_eq!(
            recs.len(),
            1,
            "exactly one member_info record for M after cleanup"
        );
        assert_eq!(
            recs[0].member_info.version, 2,
            "the surviving record is the v2 revoke"
        );
        assert!(
            recs[0].member_info.deputies.is_empty(),
            "the canonical (revoke) record has no deputies — revoked authority is not resurrected"
        );
        assert_eq!(
            state.member_info.deputies_of(m_id),
            &[] as &[MemberId],
            "deputies_of agrees with the deduped canonical record"
        );
    }
}
