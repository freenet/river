pub mod ban;
pub mod ban_evidence;
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

/// Whether the per-delta work bounds (the most new messages, bans, members
/// and ban-evidence records one delta may add) are lifted for the apply in
/// progress. A whole-state merge lifts them: it holds the incoming state to
/// its own owner-signed configuration's caps before anything is checked
/// (`refuse_beyond_own_bounds`), applies every incoming record, and lets the
/// merged room's caps decide what is kept. A network delta, whose sender
/// alone decides its size, is held to the bounds.
pub(crate) mod work_bounds {
    use std::cell::Cell;

    thread_local! {
        static LIFTED: Cell<bool> = const { Cell::new(false) };
    }

    /// True while [`lifted_while`] runs.
    pub(crate) fn lifted() -> bool {
        LIFTED.with(|l| l.get())
    }

    /// Run `f` with the work bounds lifted, restoring the previous setting
    /// afterwards (also if `f` panics).
    pub(crate) fn lifted_while<R>(f: impl FnOnce() -> R) -> R {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                LIFTED.with(|l| l.set(self.0));
            }
        }
        let _restore = Restore(LIFTED.with(|l| l.replace(true)));
        f()
    }
}

use crate::room_state::ban::BansV1;
use crate::room_state::ban_evidence::BanEvidenceV1;
use crate::room_state::configuration::AuthorizedConfigurationV1;
use crate::room_state::direct_messages::DirectMessagesV1;
use crate::room_state::member::{AuthorizedMember, MemberId, MembersV1};
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
    //
    // Pinned by `field_declaration_order_puts_members_before_direct_messages`
    // (freenet/river#671). `direct_messages` must come AFTER `members`, because
    // `DirectMessagesV1::apply_delta` sweeps its held set against
    // `parent_state.members` and the macro derives its apply order from this
    // declaration order — reorder them and the DM field would test its held set
    // against the PRE-update member set, silently restoring the #671 data loss.
    // The comment above has flagged this as fragile since the file was written
    // and was never actually asserted.
    /// Configures things like maximum message length, can be updated by the owner.
    pub configuration: AuthorizedConfigurationV1,

    /// A list of recently banned members, a banned member can't be present in the
    /// members list and will be removed from it ifc necessary.
    pub bans: BansV1,

    /// The members in the chat room along with who invited them
    pub members: MembersV1,

    /// The `AuthorizedMember` records of absent members that a stored ban
    /// still needs to be verified and judged (freenet/river#702). NOT
    /// membership: see [`BanEvidenceV1`]. Must come after `members` (its
    /// apply reads the updated member set) and before `member_info` (whose
    /// records for evidenced members it keeps valid). `#[serde(default)]`
    /// keeps states written before this field backwards-compatible, and an
    /// empty field is not serialized at all, so a state without evidence
    /// keeps the exact bytes it had before this field existed: an old state
    /// re-serialized by the new contract is byte-identical, and merging it
    /// with itself does not rewrite it (merge-law `state_idempotence`).
    #[serde(default, skip_serializing_if = "BanEvidenceV1::is_empty")]
    pub ban_evidence: BanEvidenceV1,

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
    /// Whole-state merge: fold `other_state` into `self`.
    ///
    /// This deliberately SHADOWS [`ComposableState::merge`]. Rust resolves
    /// `room.merge(..)` to an inherent method before a trait method, so every
    /// method-call site (the contract's `update_state`, the UI, riverctl,
    /// tests) runs this one; only an explicit `ComposableState::merge(..)`
    /// reaches the trait default, which is wrong for rooms. Do NOT rename it.
    /// Pinned by `room_merge_reads_the_merged_configuration` and
    /// `room_merge_keeps_what_a_removal_frees_room_for`.
    ///
    /// # Why the trait default is wrong for a room (freenet/river#703)
    ///
    /// The default summarizes the receiver, asks the sender for a delta against
    /// that summary, then applies it. Two summaries carry RETENTION HORIZONS
    /// (recent messages, and the DM per-pair and global caps): "do not offer
    /// me anything older than my oldest, I would prune it". That is right for
    /// delta gossip, where it is what stops the resend loop, and wrong for a
    /// one-shot merge of two whole states, for two reasons:
    ///
    /// * The horizon is computed from the receiver's PRE-merge cap. When the
    ///   sender's configuration outranks it and raises the cap, the merged room
    ///   keeps fewer messages than the merged cap allows.
    /// * The horizon assumes everything the receiver holds survives. When the
    ///   merge removes some of it (a ban from the sender, an inactivity prune),
    ///   the room drops below its cap, and the withheld messages would have
    ///   been kept.
    ///
    /// Either way `merge(A, B)` held fewer messages than `merge(B, A)`: an
    /// enforceable `state_commutativity` violation, even though the next
    /// exchange heals it.
    ///
    /// # What this does instead
    ///
    /// The result must be what applying EVERYTHING the receiver lacks gives
    /// (horizons open): then it is the same function of the union of both
    /// states whichever side merges. Doing that unconditionally costs, on every
    /// merge with a peer holding an older window, a signature check per older
    /// message that is then pruned, and can push a delta over the messages work
    /// bound (`lowering_max_recent_messages_from_above_the_work_bound_converges`).
    /// So the merge first runs with the horizons computed against the MERGED
    /// configuration, and repeats with them open only when that result may
    /// differ: when some capped set it withheld from ended BELOW its cap. A set
    /// that ended AT its cap holds that many messages all newer than anything
    /// withheld (withheld messages order at or below the receiver's oldest held
    /// one, and everything delivered orders above it), so the withheld ones
    /// would have been pruned on arrival and the open merge gives the same
    /// state. See [`merge_may_have_withheld_a_survivor`].
    ///
    /// # Work and the per-delta bounds
    ///
    /// `other_state` is first held to its own owner-signed configuration's
    /// caps, the bounds `verify` holds a state to (`refuse_beyond_own_bounds`,
    /// counts compared before any signature is checked): a state holding more
    /// bans, members or ban evidence than that is refused, and a state
    /// `verify` accepts never is. Within them, every record the receiver
    /// lacks is applied, the per-delta work bounds a network delta is held to
    /// lifted (`work_bounds`), and the merged room's caps in
    /// `post_apply_cleanup` decide what is kept. Trimming what one side
    /// offered to the per-delta bounds instead made the two directions
    /// differ: the trim chose before the merged room's own removals had run.
    ///
    /// The open pass's error is discarded and the first pass's result stands.
    /// The open pass applies the first pass's delta plus older messages, so
    /// any error it returns comes from those older messages; the first pass
    /// is already a complete merge, and failing here would refuse a merge the
    /// trait default completes. No test reaches that branch: it is defensive.
    ///
    /// # Normalization and `merge(S, S)`
    ///
    /// Every merge applies a delta, even an empty one, so the merged state is
    /// always normalized: each field re-applies its admission rules and
    /// `post_apply_cleanup` runs. A state stored by a full-state PUT (which
    /// runs only `verify`) can hold records the rules drop, and
    /// `merge(S, S)` then rewrites it once and is a fixpoint after: the merge
    /// verifier reports that as `state_idempotence` settling after one
    /// rewrite (`SettledAfter(1)`). On every state a merge or delta produced,
    /// `merge(S, S) == S`.
    ///
    /// `_parent_state` is ignored: the receiver's own state is the only correct
    /// parent, and every caller passes it. The signature matches the trait's so
    /// existing call sites keep compiling and pick this up.
    pub fn merge(
        &mut self,
        _parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
        other_state: &ChatRoomStateV1,
    ) -> Result<(), String> {
        self.merge_from(parameters, other_state)
    }

    /// [`Self::merge`] without the ignored parent argument, so a caller
    /// holding only `&mut self` need not clone itself to pass one.
    pub fn merge_from(
        &mut self,
        parameters: &ChatRoomParametersV1,
        other_state: &ChatRoomStateV1,
    ) -> Result<(), String> {
        refuse_beyond_own_bounds(other_state, parameters)?;
        let config_delta = configuration_delta_for_merge(self, parameters, other_state);
        refuse_beyond_receiver_bounds(self, other_state, &config_delta)?;
        // The summary is read against the configuration the merge ends with
        // (a clone only when the configuration changes).
        let summary = self.summarize(
            &self.with_configuration(parameters, &config_delta),
            parameters,
        );
        // Field `delta`s never read their parent; pass the sender's own state,
        // as `get_state_delta` does on the network path. The macro's
        // `apply_delta` ignores its parent (it re-reads `self` per field), so
        // `self` itself is passed rather than a clone.
        //
        // An empty delta is still applied (`Some` with every field `None`)
        // rather than skipped, so the merged state is always normalized: every
        // field re-applies its own admission rules and `post_apply_cleanup`
        // runs. Skipping it left a state holding records the rules drop (one
        // that arrived by a full-state PUT, which runs only `verify`)
        // unchanged when merged with a state holding nothing new, while the
        // other direction dropped them: merge(A, B) != merge(B, A)
        // (freenet/river#703). On a state a merge or delta produced,
        // the extra pass is a no-op.
        let delta_against = |summary: &ChatRoomStateV1Summary| {
            let mut delta = other_state
                .delta(other_state, parameters, summary)
                .unwrap_or_default();
            delta.configuration = config_delta.clone();
            Some(delta)
        };
        // Every incoming record is applied (the per-delta work bounds are
        // lifted: the incoming state was held to its own caps above), and the
        // merged room's caps in `post_apply_cleanup` decide what is kept, so
        // both directions keep the same records.
        let mut merged = self.clone();
        work_bounds::lifted_while(|| {
            merged.apply_delta(self, parameters, &delta_against(&summary))
        })?;
        if merge_may_have_withheld_a_survivor(&summary, &merged) {
            let open = summary.without_retention_horizons();
            let mut open_merged = self.clone();
            // If the open pass fails, keep the first pass's result rather
            // than failing a merge the first pass completed (defensive; see
            // the doc comment above).
            if work_bounds::lifted_while(|| {
                open_merged.apply_delta(self, parameters, &delta_against(&open))
            })
            .is_ok()
            {
                merged = open_merged;
            }
        }
        *self = merged;
        Ok(())
    }

    /// This state's summary, computed against itself with `other_state`'s
    /// configuration merged in: what a receiver that is about to merge
    /// `other_state` and still wants the delta limited by its retention
    /// horizons should publish (the UI's incoming-state path). Without the
    /// merged configuration, a cap the merge raises is read at its old value
    /// and the horizon withholds messages the merged room keeps.
    pub fn summarize_with_merged_configuration(
        &self,
        parameters: &ChatRoomParametersV1,
        other_state: &ChatRoomStateV1,
    ) -> ChatRoomStateV1Summary {
        let config_delta = configuration_delta_for_merge(self, parameters, other_state);
        self.summarize(
            &self.with_configuration(parameters, &config_delta),
            parameters,
        )
    }

    /// Merge `other_state` through this state's own summary, read with the
    /// configuration the merge ends with (as
    /// [`Self::summarize_with_merged_configuration`]), keeping this state's
    /// retention horizons: the receiver's side of an incoming whole state
    /// (the UI's incoming-state path). Held to the same rules as
    /// [`Self::merge_from`]: `other_state` is refused when it holds more
    /// records than its own owner-signed configuration allows, every
    /// incoming record is applied, and the merged room's caps decide what is
    /// kept. Returns the delta applied (`None` when the two already agree).
    pub fn merge_through_summary(
        &mut self,
        parameters: &ChatRoomParametersV1,
        other_state: &ChatRoomStateV1,
    ) -> Result<Option<ChatRoomStateV1Delta>, String> {
        refuse_beyond_own_bounds(other_state, parameters)?;
        let config_delta = configuration_delta_for_merge(self, parameters, other_state);
        refuse_beyond_receiver_bounds(self, other_state, &config_delta)?;
        let summary = self.summarize(
            &self.with_configuration(parameters, &config_delta),
            parameters,
        );
        // The configuration the merge ends with is offered as `merge_from`
        // offers it: the summary alone does not replace a placeholder that
        // outranks the owner's configuration.
        let delta = match other_state.delta(other_state, parameters, &summary) {
            Some(delta) => Some(delta),
            None if config_delta.is_some() => Some(ChatRoomStateV1Delta::default()),
            None => None,
        }
        .map(|mut delta| {
            delta.configuration = config_delta;
            delta
        });
        // `apply_delta` ignores its outer parent (the macro reads `self`
        // per field), so a cheap default stands in for it.
        work_bounds::lifted_while(|| {
            self.apply_delta(&ChatRoomStateV1::default(), parameters, &delta)
        })?;
        Ok(delta)
    }

    /// This state's delta against a receiver's `summary`, trimmed to the
    /// work bounds one delta may carry in the receiver's room: what a sender
    /// that holds only the receiver's summary (the contract's network delta)
    /// sends. See [`Self::trim_network_delta`].
    pub fn delta_within_work_bounds(
        &self,
        parameters: &ChatRoomParametersV1,
        summary: &ChatRoomStateV1Summary,
    ) -> Option<ChatRoomStateV1Delta> {
        let mut delta = self.delta(self, parameters, summary)?;
        self.trim_network_delta(parameters, summary, &mut delta);
        Some(delta)
    }

    /// Trim `delta`, this state's delta against a receiver's `summary`, to
    /// the work bounds one delta may carry in the receiver's room, so the
    /// receiver applies it rather than refusing it; the rest follows in
    /// later rounds (freenet/river#703). The receiver's caps are this
    /// state's when the receiver's configuration does not outrank this one
    /// (the receiver then ends with this one), and otherwise the floors
    /// every room's bounds are at least (the receiver's configuration is not
    /// known here): each cap at its least. Run it after anything that adds
    /// records to the delta.
    pub fn trim_network_delta(
        &self,
        parameters: &ChatRoomParametersV1,
        summary: &ChatRoomStateV1Summary,
        delta: &mut ChatRoomStateV1Delta,
    ) {
        let own = &self.configuration;
        let config = if !crate::room_state::member_info::outranks(
            summary.configuration,
            own.rank(parameters),
        ) {
            own.configuration.clone()
        } else {
            crate::room_state::configuration::Configuration {
                max_recent_messages: 1,
                max_user_bans: 1,
                max_members: 1,
                ..own.configuration.clone()
            }
        };
        trim_to_work_bounds(delta, &config, self, summary, parameters);
    }

    /// This state with `config_delta` applied to its configuration, borrowed
    /// unchanged when there is none.
    fn with_configuration(
        &self,
        parameters: &ChatRoomParametersV1,
        config_delta: &Option<AuthorizedConfigurationV1>,
    ) -> std::borrow::Cow<'_, ChatRoomStateV1> {
        if config_delta.is_none() {
            return std::borrow::Cow::Borrowed(self);
        }
        let mut merged = self.clone();
        // A delta the configuration does not take leaves `merged` as it was,
        // which is exactly the configuration the merge ends with.
        let _ = merged
            .configuration
            .apply_delta(self, parameters, config_delta);
        std::borrow::Cow::Owned(merged)
    }

    /// Post-apply cleanup: prune members who have no recent messages, clean up
    /// member_info for pruned members, remove orphaned bans, and sweep
    /// direct messages whose participants are no longer in the room.
    ///
    /// Members are kept if they have at least one message in recent_messages,
    /// are a sender/recipient of a currently-held direct message (see
    /// [`crate::room_state::direct_messages::DirectMessagesV1::participants_of_surviving_dms`]),
    /// or are in the invite chain of someone who qualifies. The owner is
    /// never in the members list (they're implicit via parameters).
    ///
    /// A member who leaves `members` (banned or pruned) while a stored ban
    /// still references them moves to `ban_evidence`, so the ban stays
    /// verifiable and its validity does not change with the removal it caused
    /// (freenet/river#702). A ban is swept only when its issuer's record is
    /// known nowhere.
    ///
    /// IDEMPOTENCE / CONVERGENCE INVARIANT: this function MUST be idempotent
    /// (`cleanup(S) == cleanup(cleanup(S))`) and a pure function of the converged
    /// state, because Freenet runs it a variable number of times across peers
    /// (and full-state PUTs bypass it via `verify`). The `max_user_bans` cap is
    /// therefore applied at the TOP (step 0-cap) so ban enforcement and the
    /// ban evidence read the FINAL surviving ban set — see the block comments
    /// below and #411 round 7 / Codex P1 #1+#2. Ban validity reads members plus
    /// `ban_evidence`, which removal moves records into rather than deleting
    /// them, so a pass cannot change the next pass's answer (freenet/river#702).
    ///
    /// Direct-message sweep: after pruning, any DM whose sender or
    /// recipient is now non-member or banned is dropped. Without this,
    /// adding a ban for a DM participant would silently make every
    /// peer's verify fail, and members referenced only by a DM would be
    /// pruned (orphaning their DMs). See
    /// `direct_messages.rs` module docs, "Interaction with bans".
    pub fn post_apply_cleanup(&mut self, parameters: &ChatRoomParametersV1) -> Result<(), String> {
        let owner_id = MemberId::from(&parameters.owner);
        let depth_cap = crate::room_state::ban_evidence::evidence_depth_cap(
            self.configuration.configuration.max_members,
        );
        // A lowered `max_members` can leave held evidence deeper than the
        // cap. Drop it before anything reads it, so every step of this pass
        // (and `enforced_ban_set_of`, which drops it the same way) judges the
        // same records the next pass will.
        if let Some(pruned) = self
            .ban_evidence
            .without_too_deep(&self.members, owner_id, depth_cap)
        {
            self.ban_evidence = pruned;
        }
        // Every record known at the start of this pass: members plus ban
        // evidence. Step 5 and the evidence recomputation read THIS, never the
        // pruned member set, so nothing this pass removes can change which
        // bans stay verifiable (freenet/river#702 review round 5).
        let known_at_start: std::collections::HashMap<MemberId, AuthorizedMember> = self
            .ban_evidence
            .lookup(&self.members, owner_id)
            .into_iter()
            .map(|(id, m)| (id, m.clone()))
            .collect();

        // 0-sig. A ban no known key verifies counts for nothing: one whose
        //     known issuer's key does not verify it (skipped on arrival at a
        //     peer that knows the issuer) and one whose issuer is known
        //     nowhere (step 4 sweeps it in this pass) are dropped before the
        //     cap counts them (`BansV1::drop_bans_no_known_key_verifies`; the
        //     DM apply's enforced-ban set drops them the same way). Only the
        //     cap can be swayed by one (resolution ignores such a ban, and
        //     step 4 sweeps it), so this runs only when the bans are over the
        //     cap.
        if self.bans.0.len() > self.configuration.configuration.max_user_bans {
            let known = self.ban_evidence.lookup(&self.members, owner_id);
            let mut bans = std::mem::take(&mut self.bans.0);
            BansV1::drop_bans_no_known_key_verifies(&mut bans, &known, owner_id, &parameters.owner);
            self.bans.0 = bans;
        }

        // Snapshot for the equality assert below. Taken HERE, before step 0-cap
        // mutates `self.bans`, because this is the one point where `self` still
        // holds what `parent_state` held when the `direct_messages` field
        // applied: the five inputs `enforced_ban_set_of` reads
        // (`configuration`, `bans`, `members`, `ban_evidence`, `member_info`)
        // are all final by then, and the only change since is the evidence
        // deeper than the cap dropped above, which `enforced_ban_set_of` drops
        // the same way.
        #[cfg(debug_assertions)]
        let dm_apply_time_ban_set = {
            let snapshot = self.clone();
            crate::room_state::direct_messages::enforced_ban_set_of(&snapshot, parameters)
        };

        // 0-cap. Enforce `max_user_bans` FIRST — BEFORE ban enforcement (step 0)
        //     and the ban evidence (step 4a) — so both read
        //     the FINAL surviving (post-cap) ban set (#411 round 7 / Codex P1
        //     #1+#2). Running the cap here, on the converged pre-enforcement
        //     state, is what keeps `post_apply_cleanup` IDEMPOTENT and identical
        //     across peers:
        //       * #1: an over-cap ban that WILL be evicted must not one-shot a
        //         member removal at step 0 that the capped converged state
        //         cannot reproduce — a peer that only ever sees the capped state
        //         (e.g. via a full-state PUT that bypasses cleanup) would keep
        //         that member, so removing it here would diverge the member set.
        //       * #2: a record the evicted ban was the only reference to must
        //         not survive in `ban_evidence` on pass 1 and vanish on pass 2
        //         (`cleanup(S) != cleanup(cleanup(S))`, which permanently
        //         diverges peers that run cleanup a different number of times).
        //     Eviction drops bans that take no effect first, judged by
        //     `MembersV1::resolve_bans` over the uncapped set, one per
        //     (issuer, target) (#410 review round 1, freenet/river#702), then
        //     evicts the oldest within two pools, owner bans and the rest, each
        //     guaranteed `max_user_bans / 2` (freenet/river#796). A flood of
        //     forged, revoked-deputy or banned-issuer bans, which `verify`
        //     accepts, is dead and goes first. A flood of LIVE far-future-dated
        //     bans, which any member can mint against its own invitees or
        //     against ids known nowhere, evicts older non-owner bans and owner
        //     bans beyond the guaranteed half; see `enforce_user_ban_cap` and
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
        //     delta. (`enforce_user_ban_cap` is independent of the order of the
        //     ban list it is handed; the arrival-order dependence is in the
        //     member set it judges against.)
        //     `verify`'s hard cap ceiling still rejects any stored state left over
        //     the cap. The signature sweep (step 4) still runs AFTER enforcement
        //     to drop bans orphaned by a banner's removal, and only removes bans,
        //     so the ban count stays <= the cap.
        let max_bans = self.configuration.configuration.max_user_bans;
        {
            // The eviction ORDERING lives in `BansV1::enforce_user_ban_cap` —
            // one definition, shared with `DirectMessagesV1::apply_delta`,
            // which needs the same surviving ban set to decide which held DMs
            // the caps may rank (freenet/river#675). A DM swept at apply time
            // against a DIFFERENT surviving ban set than this step produces is
            // data loss, so the two must agree by construction rather than by
            // two copies happening to match — which is exactly what drifted in
            // #671 and in #411 round 4.
            BansV1::enforce_user_ban_cap(
                &mut self.bans.0,
                max_bans,
                &self.members,
                &self.ban_evidence,
                &self.member_info,
                parameters,
            );
        }

        // 0. Enforce bans from the CONVERGED (now capped) state, deputy-aware (#410).
        //
        // The field applies remove no banned member: this step removes every
        // one, owner-banned included. It runs after every field has been
        // applied, so `self.member_info` (which carries deputy grants) is
        // converged. `MembersV1::resolve_bans` decides
        // which bans take effect (a ban whose issuer is removed does not,
        // except the bans of a mutual-ban cycle, whose members are all
        // removed: freenet/river#423, #702). Because the result is a pure
        // function of the converged (members + deputies + bans) state, every
        // peer converges to the same member set regardless of delta order.
        // Kept in post_apply_cleanup (NOT verify) so verify stays stable
        // across ban/deputy changes, mirroring the DM ban-sweep precedent.
        let resolution = self.members.resolve_bans(
            &self.ban_evidence,
            &self.bans,
            &self.member_info,
            parameters,
        );
        let enforced_banned_ids = resolution.removed.clone();

        // THE load-bearing invariant of the freenet/river#675 remedy: the ban
        // set `DirectMessagesV1::apply_delta` derived, before this function
        // ran, must EQUAL the one step 0 computes here. The apply-time sweep
        // deletes DMs against its set and step 6 re-tests against this one; if
        // they diverge, the apply-time sweep can delete a DM step 6 would have
        // kept, which is permanent user data loss with no error anywhere.
        //
        // Asserted rather than argued, because this PR's whole thesis is that
        // predicate agreement must not be left to two copies agreeing in
        // comments. `dm_ban_derivation_must_apply_the_user_ban_cap` catches the
        // known way to break it at one explicit site; this catches ANY way, at
        // every site the existing suite already exercises — including a future
        // change to field-application order, or to when `member_info`
        // converges, which no test would otherwise notice.
        //
        // `debug_assert` so the release WASM pays nothing.
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            dm_apply_time_ban_set, enforced_banned_ids,
            "enforced_ban_set_of (at DM apply) diverged from step 0's enforced_banned_ids; \
             the apply-time DM sweep and the step-6 sweep are no longer the same predicate \
             (freenet/river#675)"
        );
        self.members
            .members
            .retain(|m| !enforced_banned_ids.contains(&m.member.id()));

        // 1-3. Inactivity prune and the `max_members` cap, repeated until a
        //      pass removes no member (`prune_and_cap_members`). The cap
        //      removes the deepest members of the PRUNED set, and a member it
        //      removes can leave an ancestor that only it needed, which the
        //      next pass prunes. Every pass but the last removes at least one
        //      member, so there are at most (members after step 0) + 1
        //      passes. The member, message and DM caps are decided here, after
        //      bans and pruning, never in the field applies, so a member this
        //      cleanup removes never takes a cap slot.
        let members_after_ban_enforcement: HashSet<MemberId> =
            self.members.members.iter().map(|m| m.member.id()).collect();
        let required_ids = loop {
            let before = self.members.members.len();
            let required = self.prune_and_cap_members(parameters, owner_id, &enforced_banned_ids);
            if self.members.members.len() == before {
                break required;
            }
        };

        // 4. Sweep any ban whose issuer is unknown or whose signature does
        //    not match the issuer's key (#411 round 3 item A.3, round 4 item
        //    A; AGENTS.md State Authorization Rule), and every ban by a member
        //    an owner ban removes: the owner's ban wins over everything, so
        //    such a ban can never take effect while it stands.
        //    "Known" means present in `members` now, or known at the START of
        //    this pass and within the evidence depth cap, so a ban whose
        //    issuer this pass removes or prunes stays exactly when the
        //    issuer's record moves to evidence just below (freenet/river#702).
        //    This runs before the evidence recomputation, which reads the
        //    swept ban set, and it only removes bans, so the count stays <=
        //    the cap.
        let all_known: std::collections::HashMap<MemberId, &AuthorizedMember> =
            known_at_start.iter().map(|(id, m)| (*id, m)).collect();
        let present: HashSet<MemberId> =
            self.members.members.iter().map(|m| m.member.id()).collect();
        let known_by_id: std::collections::HashMap<MemberId, &AuthorizedMember> = all_known
            .iter()
            .filter(|(id, m)| {
                present.contains(*id)
                    || crate::room_state::ban_evidence::within_depth(
                        m, &all_known, owner_id, depth_cap,
                    )
            })
            .map(|(id, m)| (*id, *m))
            .collect();
        self.bans.0.retain(|ban| {
            !resolution.owner_removed.contains(&ban.banned_by)
                && BansV1::ban_signature_matches_current_key(
                    ban,
                    &known_by_id,
                    owner_id,
                    &parameters.owner,
                )
        });

        // 4a. Recompute the ban evidence (freenet/river#702): every record
        //     known at the start of this pass whose member a stored ban
        //     references (issuer or target, and their invite ancestors), and
        //     who is no longer in `members`. Ban resolution reads members plus
        //     this set, so a ban judged on this pass is judged identically on
        //     the next one, whoever left `members` in between: that is what
        //     keeps this function idempotent. A record no stored ban needs is
        //     dropped here, and a member re-added to `members` leaves it.
        {
            // An owner ban needs no record: the owner's key and authority
            // are always available, and it removes its target and subtree
            // whatever else is known. A record an owner ban removes is never
            // kept as evidence, even when another ban names it: such a
            // record reaching this point as a member was removed by step 0,
            // so keeping it only when it arrived as evidence made the result
            // depend on how the record arrived (freenet/river#703 review). Nothing needs it: a ban its member issued is swept in
            // step 4, and a ban naming it ranks as one whose target is held
            // nowhere. `BanEvidenceV1::verify` checks stored evidence against
            // `referenced_by_bans`, which this only narrows.
            // Walked through the records that can be kept only, so a record
            // referenced only through an owner-removed descendant is not kept
            // either; `BanEvidenceV1::verify` walks the kept set the same way.
            let referenced =
                crate::room_state::ban_evidence::referenced_by_bans(&self.bans, owner_id, |id| {
                    if resolution.owner_removed.contains(&id) {
                        return None;
                    }
                    known_by_id.get(&id).map(|m| m.member.invited_by)
                });
            let mut evidence: Vec<AuthorizedMember> = known_by_id
                .iter()
                .filter(|(id, _)| {
                    referenced.contains(*id)
                        && !present.contains(*id)
                        && !resolution.owner_removed.contains(*id)
                })
                .map(|(_, m)| (*m).clone())
                .collect();
            evidence.sort_by_key(|m| m.member.id());
            self.ban_evidence.members = evidence;
        }

        // 5. Clean member_info for members who are gone. A member held as
        //    ban evidence keeps its record: a stored ban may read it
        //    (`is_ban_authorized` rule 4 reads the `deputies` of the target's
        //    strict ancestors), and a member who leaves `members` for ban
        //    evidence and later returns must come back with the record they
        //    had, as on a peer where they never left (freenet/river#703
        //    review; keeping only the ancestors a ban reads lost it). Keeping
        //    it regardless of content is what makes the choice independent of
        //    which version of the record a peer holds: a newer record (a
        //    revoked grant) is never dropped and then outranked by an older
        //    one supplied again. Evidence is bounded by the stored bans and
        //    the depth cap. No reader shows an evidence-only member's
        //    nickname. They keep merging like any other record (highest rank
        //    wins); see `BanEvidenceV1`.
        let evidence_ids: HashSet<MemberId> = self
            .ban_evidence
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();
        self.member_info.member_info.retain(|info| {
            let id = info.member_info.member_id;
            id == owner_id || required_ids.contains(&id) || evidence_ids.contains(&id)
        });

        // 5a. Collapse duplicate member_info records to the single canonical
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

        // 5b. Sweep recent messages authored by members removed above (deputy-
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
        // Then the `max_recent_messages` cap over what is left, newest by
        // `(time, id)` kept: the cap step 1 counted, so every author it
        // counted keeps a message.
        {
            let cap = self.configuration.configuration.max_recent_messages;
            let messages = &mut self.recent_messages.messages;
            messages.sort_by_key(|m| m.order_key());
            if messages.len() > cap {
                messages.drain(0..messages.len() - cap);
            }
        }

        // Rebuild the PUBLIC `actions_state` cache now that removed authors'
        // messages are gone (#411 round 7 / Codex P2 #4). `MessagesV1::apply_delta`
        // already rebuilt this cache, but it ran BEFORE the sweep above, so a
        // deputy-banned member's edit/delete/reaction would linger in the cache
        // (e.g. their reaction still rendered on a message). The UI's private
        // rebuild (`rebuild_actions_state_with_decrypted`) is a no-op for a public
        // room, so nothing else recomputes it. This is the same public-only rebuild
        // `apply_delta` runs; the UI re-runs its private rebuild after apply.
        self.recent_messages.rebuild_actions_state();

        // 5c. Drop the secret blobs of members removed above, by the same
        //     rule `RoomSecretsV1::apply_delta` applies (a blob only for the
        //     owner or a current member). That apply runs before this cleanup
        //     in field order, so a member removed HERE would keep their blob
        //     until the next apply dropped it, and the state this apply
        //     produced would not be a fixpoint of `merge(S, S)`
        //     (freenet/river#703).
        self.secrets.encrypted_secrets.retain(|s| {
            s.secret.member_id == owner_id || current_member_ids.contains(&s.secret.member_id)
        });

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
        //    The step-1 DM exemption is only sound while steps 2-5 are
        //    removal-only for members: it counts a DM as surviving using the
        //    post-step-0 member set, and step 6 then re-tests the same DM
        //    against the post-step-3 set. If anything between them ADDED a
        //    member, step 6 could keep a DM step 1 did not count — harmless —
        //    but the reverse reasoning (exemption ⟹ retention) would no longer
        //    be a subset argument, and freenet/river#671 would be back. Nothing
        //    does today; `debug_assert!` states that where an editor of steps
        //    2-5 will read it, and compiles out of the release WASM so the
        //    contract pays nothing. It runs across the whole existing
        //    `post_apply_cleanup`, convergence and deputy-ban suites.
        debug_assert!(
            active_member_ids_for_sweep.is_subset(&members_after_ban_enforcement),
            "steps 2-5 must be removal-only for members: the step-1 DM exemption \
             reads the post-step-0 set and step 6 the post-step-3 one"
        );
        self.direct_messages.sweep_after_membership_change(
            owner_id,
            &active_member_ids_for_sweep,
            &enforced_banned_ids,
        );
        // Then the per-pair and global DM caps over what is left, the caps
        // step 1 counted.
        crate::room_state::direct_messages::enforce_caps_and_sort(
            &mut self.direct_messages,
            self.configuration
                .configuration
                .effective_max_direct_messages(),
        );

        // 7. Re-sort for deterministic ordering
        self.members.members.sort_by_key(|m| m.member.id());
        self.member_info
            .member_info
            .sort_by_key(|info| info.member_info.member_id);

        Ok(())
    }
}

impl ChatRoomStateV1 {
    /// One pass of `post_apply_cleanup` steps 1-3: keep the members something
    /// live needs, with their invite chains, then apply the `max_members`
    /// cap to what is kept. Returns the ids the prune kept (before the cap).
    ///
    /// A member is needed as:
    /// - the author of one of the newest `max_recent_messages` messages whose
    ///   authors are live in THIS pass (the owner, or a member not
    ///   enforced-banned), so a message of a member this cleanup removes
    ///   never takes a slot;
    /// - a participant of a DM that survives step 6 and the DM caps, judged
    ///   against the members of THIS pass (`participants_of_surviving_dms`,
    ///   the predicate step 6 applies, freenet/river#671);
    /// - a recipient of a secret blob at the current secret version
    ///   (issue #110; scoped to the current version so members whose blobs
    ///   are only at older versions are still pruned);
    /// - the issuer of a stored ban whose signature matches the issuer's
    ///   current key (#411 round 3 item B; kept so stored rooms stay
    ///   fixpoints, see the history in freenet/river#702);
    /// - an invite ancestor of any of these.
    fn prune_and_cap_members(
        &mut self,
        parameters: &ChatRoomParametersV1,
        owner_id: MemberId,
        enforced_banned_ids: &HashSet<MemberId>,
    ) -> HashSet<MemberId> {
        let members_now: HashSet<MemberId> =
            self.members.members.iter().map(|m| m.member.id()).collect();
        let message_authors: HashSet<MemberId> = {
            let mut live: Vec<&crate::room_state::message::AuthorizedMessageV1> = self
                .recent_messages
                .messages
                .iter()
                .filter(|m| {
                    m.message.author == owner_id
                        || (members_now.contains(&m.message.author)
                            && !enforced_banned_ids.contains(&m.message.author))
                })
                .collect();
            live.sort_by_key(|m| std::cmp::Reverse(m.order_key()));
            live.truncate(self.configuration.configuration.max_recent_messages);
            live.into_iter().map(|m| m.message.author).collect()
        };
        let dm_participants: HashSet<MemberId> = {
            let mut dms = self.direct_messages.clone();
            dms.sweep_after_membership_change(owner_id, &members_now, enforced_banned_ids);
            crate::room_state::direct_messages::enforce_caps_and_sort(
                &mut dms,
                self.configuration
                    .configuration
                    .effective_max_direct_messages(),
            );
            dms.participants_of_surviving_dms(owner_id, &members_now, enforced_banned_ids)
        };
        let current_secret_version = self.secrets.current_version;
        let secret_recipients = self
            .secrets
            .encrypted_secrets
            .iter()
            .filter(|s| s.secret.secret_version == current_secret_version)
            .map(|s| s.secret.member_id);

        let members_by_id = self.members.members_by_member_id();
        let mut required_ids: HashSet<MemberId> = message_authors
            .into_iter()
            .chain(dm_participants)
            .chain(secret_recipients)
            .filter(|id| *id != owner_id && members_by_id.contains_key(id))
            .collect();
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
        // Walk invite chains upward, adding all ancestors (stop at owner).
        let mut to_process: Vec<MemberId> = required_ids.iter().cloned().collect();
        while let Some(member_id) = to_process.pop() {
            if let Some(member) = members_by_id.get(&member_id) {
                let inviter_id = member.member.invited_by;
                if inviter_id != owner_id && required_ids.insert(inviter_id) {
                    to_process.push(inviter_id);
                }
            }
        }

        self.members
            .members
            .retain(|m| required_ids.contains(&m.member.id()));
        let max_members = self.configuration.configuration.max_members;
        self.members.remove_excess_members(parameters, max_members);
        required_ids
    }
}

/// The configuration `other_state` offers `receiver` in a whole-state merge:
/// its configuration if that outranks the receiver's, as the summary decides,
/// and also whenever the receiver's own configuration is not signed by the
/// room owner (an imported room's placeholder) while the other's is. Such a
/// placeholder can win the signature-digest tie-break against a real
/// configuration at the same version, and must never survive a merge with
/// one; `AuthorizedConfigurationV1::apply_delta` then replaces it. Only a
/// client's local state can hold one: the contract's `validate_state`
/// rejects it.
fn configuration_delta_for_merge(
    receiver: &ChatRoomStateV1,
    parameters: &ChatRoomParametersV1,
    other_state: &ChatRoomStateV1,
) -> Option<AuthorizedConfigurationV1> {
    let summary = receiver.configuration.summarize(receiver, parameters);
    other_state
        .configuration
        .delta(other_state, parameters, &summary)
        .or_else(|| {
            if receiver.configuration == other_state.configuration {
                return None;
            }
            let receiver_is_placeholder = receiver
                .configuration
                .verify_signature(&parameters.owner)
                .is_err();
            let other_is_the_owners = other_state
                .configuration
                .verify_signature(&parameters.owner)
                .is_ok();
            (receiver_is_placeholder && other_is_the_owners)
                .then(|| other_state.configuration.clone())
        })
}

/// Refuse `state` as the incoming side of a whole-state merge when it holds
/// more records than its own configuration allows: more bans than
/// `max_user_bans`, more members than `max_members`, or more ban evidence
/// than those two caps bound (`evidence_bound`). These are `verify`'s own
/// bounds, so a state `verify` accepts is never refused here. The counts are
/// compared before any signature is checked; caps above the per-delta floors
/// count only when the room owner signed the configuration (one signature
/// check), so the bans, members and evidence a merge then checks are
/// bounded by what the owner signed. Messages have no count bound in
/// `verify`, so none is applied here: a whole-state merge checks as many
/// messages as `verify` of the same state does.
///
/// Only the incoming side is held to this. A receiver that holds such a
/// state merges into it (merge(S, base) succeeds where merge(base, S) is
/// refused); only a client's local state can hold one, since the contract
/// validates every state it stores and no apply or merge produces one.
fn refuse_beyond_own_bounds(
    state: &ChatRoomStateV1,
    parameters: &ChatRoomParametersV1,
) -> Result<(), String> {
    use crate::room_state::ban::MIN_BANS_DELTA_BOUND;
    use crate::room_state::ban_evidence::{evidence_bound, evidence_depth_cap};
    use crate::room_state::member::members_work_bound;
    let over = |max_user_bans: usize, max_members: usize, evidence: usize| {
        if state.bans.0.len() > max_user_bans {
            Some(format!(
                "{} bans, more than {max_user_bans}",
                state.bans.0.len()
            ))
        } else if state.members.members.len() > max_members {
            Some(format!(
                "{} members, more than {max_members}",
                state.members.members.len()
            ))
        } else if state.ban_evidence.members.len() > evidence {
            Some(format!(
                "{} ban evidence records, more than {evidence}",
                state.ban_evidence.members.len()
            ))
        } else {
            None
        }
    };
    let config = &state.configuration.configuration;
    if let Some(why) = over(
        config.max_user_bans,
        config.max_members,
        evidence_bound(config.max_user_bans, evidence_depth_cap(config.max_members)),
    ) {
        return Err(format!(
            "Refusing a state that holds {why} (its own configuration's limit)"
        ));
    }
    let floors = over(
        MIN_BANS_DELTA_BOUND,
        members_work_bound(1),
        evidence_bound(MIN_BANS_DELTA_BOUND, evidence_depth_cap(1)),
    );
    if let Some(why) = floors {
        if state
            .configuration
            .verify_signature(&parameters.owner)
            .is_err()
        {
            return Err(format!(
                "Refusing a state that holds {why} under a configuration the room owner did not sign"
            ));
        }
    }
    Ok(())
}

/// Refuse `state` as the incoming side of a whole-state merge into
/// `receiver` when the merge ends with the receiver's configuration
/// (`config_delta` is `None`: the incoming configuration does not outrank it)
/// and `state` holds more bans, members or ban evidence than one delta may
/// carry in the receiver's room (each cap at least its per-delta floor).
/// Counts only, before any signature is checked: without it, a replayed
/// older configuration with a high cap would admit that many records, each
/// checked here. An honest peer still on an older, higher cap is refused
/// once; it takes the receiver's configuration from the other direction's
/// merge and trims, and the next exchange goes through.
fn refuse_beyond_receiver_bounds(
    receiver: &ChatRoomStateV1,
    state: &ChatRoomStateV1,
    config_delta: &Option<AuthorizedConfigurationV1>,
) -> Result<(), String> {
    use crate::room_state::ban::MIN_BANS_DELTA_BOUND;
    use crate::room_state::ban_evidence::{evidence_bound, evidence_depth_cap};
    use crate::room_state::member::members_work_bound;
    if config_delta.is_some() {
        return Ok(());
    }
    let c = &receiver.configuration.configuration;
    let bans = c.max_user_bans.max(MIN_BANS_DELTA_BOUND);
    let members = members_work_bound(c.max_members);
    let evidence = evidence_bound(bans, evidence_depth_cap(c.max_members));
    let why = if state.bans.0.len() > bans {
        format!("{} bans, more than {bans}", state.bans.0.len())
    } else if state.members.members.len() > members {
        format!(
            "{} members, more than {members}",
            state.members.members.len()
        )
    } else if state.ban_evidence.members.len() > evidence {
        format!(
            "{} ban evidence records, more than {evidence}",
            state.ban_evidence.members.len()
        )
    } else {
        return Ok(());
    };
    Err(format!(
        "Refusing a state that holds {why} under a configuration that does not outrank this room's"
    ))
}

/// Trim each part of a network `delta` (sent against a receiver's summary,
/// the receiver's state unknown) to the work bound one delta may carry in a
/// room with `config`'s caps, so the receiver applies it rather than
/// refusing it; the rest follows in later rounds (freenet/river#703). Each
/// part keeps first what the caps keep first:
/// - messages: the newest (the `max_recent_messages` trim keeps the newest);
/// - bans: what `BansV1::enforce_user_ban_cap` keeps at the bound, judged in
///   the sender's state;
/// - members: first the authors of the messages kept and their invite
///   chains, so the delta always delivers messages with their authors;
///   then the shortest invite chains, then the lowest id (the `max_members`
///   trim removes the longest chains, then the highest id); a member's
///   inviter is always shallower, so no kept member loses its inviter to the
///   trim;
/// - ban evidence: only records a ban the receiver will hold references
///   (the sender's bans less those trimmed here), then the shallowest, then
///   by digest.
///
/// A message whose author is neither the owner, a member the receiver's
/// summary lists, nor a member the trimmed delta carries is dropped, so the
/// delta never carries a message its receiver cannot verify. Run it after
/// anything that adds records to the delta.
fn trim_to_work_bounds(
    delta: &mut ChatRoomStateV1Delta,
    config: &crate::room_state::configuration::Configuration,
    sender: &ChatRoomStateV1,
    summary: &ChatRoomStateV1Summary,
    parameters: &ChatRoomParametersV1,
) {
    use crate::room_state::ban::{ban_digest, MIN_BANS_DELTA_BOUND};
    use crate::room_state::ban_evidence::{
        evidence_bound, evidence_depth_cap, evidence_digest, referenced_by_bans,
    };
    use crate::room_state::member::{members_work_bound, MembersDelta};
    use crate::room_state::message::messages_work_bound;
    use std::collections::HashMap;

    if let Some(msgs) = delta.recent_messages.as_mut() {
        let bound = messages_work_bound(config.max_recent_messages);
        if msgs.len() > bound {
            msgs.sort_by_key(|m| std::cmp::Reverse(m.order_key()));
            msgs.truncate(bound);
        }
    }

    let owner_id = parameters.owner_id();
    let bans_bound = config.max_user_bans.max(MIN_BANS_DELTA_BOUND);
    let mut trimmed_bans = HashSet::new();
    if let Some(bans) = delta.bans.as_mut() {
        if bans.len() > bans_bound {
            let before: Vec<_> = bans.iter().map(ban_digest).collect();
            crate::room_state::ban::BansV1::enforce_user_ban_cap(
                bans,
                bans_bound,
                &sender.members,
                &sender.ban_evidence,
                &sender.member_info,
                parameters,
            );
            let kept: HashSet<_> = bans.iter().map(ban_digest).collect();
            trimmed_bans = before.into_iter().filter(|d| !kept.contains(d)).collect();
        }
    }

    // Invite depth in the sender's state (members and evidence), the owner
    // at 0, computed for every id at once by one breadth-first walk (linear
    // in the records); a record whose chain does not reach the owner sorts
    // last.
    let known = sender.ban_evidence.lookup(&sender.members, owner_id);
    let depth_in_sender = || {
        let mut children: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
        for (id, m) in &known {
            children.entry(m.member.invited_by).or_default().push(*id);
        }
        let mut id_depth: HashMap<MemberId, usize> = HashMap::from([(owner_id, 0)]);
        let mut frontier = vec![owner_id];
        let mut level = 0;
        while !frontier.is_empty() {
            level += 1;
            let mut next = Vec::new();
            for inviter in &frontier {
                for child in children.get(inviter).into_iter().flatten() {
                    if !id_depth.contains_key(child) {
                        id_depth.insert(*child, level);
                        next.push(*child);
                    }
                }
            }
            frontier = next;
        }
        move |m: &AuthorizedMember| -> usize {
            id_depth
                .get(&m.member.invited_by)
                .map_or(usize::MAX, |d| d + 1)
        }
    };

    let members_bound = members_work_bound(config.max_members);
    if let Some(members) = delta.members.as_mut() {
        if members.added().len() > members_bound {
            let depth = depth_in_sender();
            // The authors of the messages the delta keeps, and their invite
            // chains, go first, so a trimmed delta always delivers messages
            // with their authors (otherwise the two trims can choose disjoint
            // sets, every message is then left out, and the receiver prunes
            // the members it took: the same delta forever).
            let mut needed: HashSet<MemberId> = HashSet::new();
            for author in delta
                .recent_messages
                .iter()
                .flatten()
                .map(|m| m.message.author)
            {
                let mut current = author;
                while current != owner_id
                    && !summary.members.contains(&current)
                    && needed.insert(current)
                {
                    match known.get(&current) {
                        Some(m) => current = m.member.invited_by,
                        None => break,
                    }
                }
            }
            // Rank by (not needed, depth, id), but keep the kept records in
            // the order the delta lists them: placing new records reads that
            // order (the last record for an id names the sender's key).
            let mut ranked: Vec<(bool, usize, MemberId, usize)> = members
                .added()
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let id = m.member.id();
                    (!needed.contains(&id), depth(m), id, i)
                })
                .collect();
            ranked.sort();
            let keep: HashSet<usize> = ranked
                .into_iter()
                .take(members_bound)
                .map(|(_, _, _, i)| i)
                .collect();
            let added: Vec<AuthorizedMember> = members
                .added()
                .iter()
                .enumerate()
                .filter(|(i, _)| keep.contains(i))
                .map(|(_, m)| m.clone())
                .collect();
            *members = MembersDelta::new(added);
        }
    }

    if let Some(evidence) = delta.ban_evidence.as_mut() {
        if !trimmed_bans.is_empty() {
            let still_held = crate::room_state::ban::BansV1(
                sender
                    .bans
                    .0
                    .iter()
                    .filter(|b| !trimmed_bans.contains(&ban_digest(b)))
                    .cloned()
                    .collect(),
            );
            let referenced = referenced_by_bans(&still_held, owner_id, |id| {
                known.get(&id).map(|m| m.member.invited_by)
            });
            evidence.retain(|m| referenced.contains(&m.member.id()));
        }
        let evidence_cap = evidence_bound(bans_bound, evidence_depth_cap(config.max_members));
        if evidence.len() > evidence_cap {
            let depth = depth_in_sender();
            evidence.sort_by_cached_key(|m| (depth(m), evidence_digest(m)));
            evidence.truncate(evidence_cap);
        }
    }

    if let Some(msgs) = delta.recent_messages.as_mut() {
        let carried: HashSet<MemberId> = delta
            .members
            .as_ref()
            .map(|m| m.added().iter().map(|a| a.member.id()).collect())
            .unwrap_or_default();
        msgs.retain(|m| {
            let author = m.message.author;
            author == owner_id || summary.members.contains(&author) || carried.contains(&author)
        });
    }
}

/// Whether a merge that sent `summary` (horizons intact) and ended in `merged`
/// may have withheld a message the merged room would keep: some capped set
/// whose horizon withheld anything ended BELOW its cap. See
/// [`ChatRoomStateV1::merge`] for why a set that ended AT its cap is safe.
fn merge_may_have_withheld_a_survivor(
    summary: &ChatRoomStateV1Summary,
    merged: &ChatRoomStateV1,
) -> bool {
    use crate::room_state::direct_messages::{DmRetentionHorizon, MAX_DM_MESSAGES_PER_PAIR};
    use crate::room_state::message::RetentionHorizon;
    let config = &merged.configuration.configuration;
    let messages = !matches!(summary.recent_messages.horizon, RetentionHorizon::Open)
        && merged.recent_messages.messages.len() < config.max_recent_messages;
    let dms = &merged.direct_messages.messages;
    let dm_global = !matches!(
        summary.direct_messages.global_horizon,
        DmRetentionHorizon::Open
    ) && dms.len() < config.effective_max_direct_messages();
    let dm_pair = summary.direct_messages.pair_horizons.iter().any(|h| {
        dms.iter()
            .filter(|m| m.message.sender == h.sender && m.message.recipient == h.recipient)
            .count()
            < MAX_DM_MESSAGES_PER_PAIR
    });
    messages || dm_global || dm_pair
}

impl ChatRoomStateV1Summary {
    /// This summary with every retention horizon opened, so a `delta` against
    /// it is the plain id-set difference: everything the summarizing state
    /// lacks, whether or not its caps would keep it.
    ///
    /// Used where the horizons must not filter: a one-shot whole-state merge
    /// ([`ChatRoomStateV1::merge`]) and an outgoing update computed against the
    /// sender's OWN baseline (the UI's `outbound_summary`).
    ///
    /// EXHAUSTIVE DESTRUCTURE, deliberately: adding a field to this summary,
    /// to `MessagesSummary` or to `DirectMessagesSummary` is a compile error
    /// here, at the one place that has to decide whether it is horizon-shaped
    /// (makes a sender withhold something it holds and the receiver lacks).
    /// The other leaf summaries are bound whole and are not guarded.
    pub fn without_retention_horizons(self) -> Self {
        use crate::room_state::direct_messages::{DirectMessagesSummary, DmRetentionHorizon};
        use crate::room_state::message::{MessagesSummary, RetentionHorizon};
        let ChatRoomStateV1Summary {
            configuration,
            bans,
            members,
            ban_evidence,
            member_info,
            secrets,
            recent_messages,
            direct_messages,
            upgrade,
            version,
        } = self;
        let MessagesSummary {
            message_ids,
            horizon: _open,
        } = recent_messages;
        let DirectMessagesSummary {
            message_signatures,
            purge_versions,
            pair_horizons: _cleared,
            global_horizon: _opened,
        } = direct_messages;
        ChatRoomStateV1Summary {
            configuration,
            bans,
            members,
            ban_evidence,
            member_info,
            secrets,
            recent_messages: MessagesSummary {
                message_ids,
                horizon: RetentionHorizon::Open,
            },
            direct_messages: DirectMessagesSummary {
                message_signatures,
                purge_versions,
                pair_horizons: Vec::new(),
                global_horizon: DmRetentionHorizon::Open,
            },
            upgrade,
            version,
        }
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
    use crate::room_state::ban::{AuthorizedUserBan, UserBan, MIN_BANS_DELTA_BOUND};
    use crate::room_state::configuration::Configuration;
    use crate::room_state::member::{AuthorizedMember, Member};
    use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use ed25519_dalek::SigningKey;
    use std::fmt::Debug;
    use std::time::SystemTime;

    /// Over its bound, a network delta's ban evidence keeps the shallowest
    /// records, its bans keep what the ban cap keeps at the bound (here the
    /// owner's live ban and the newest of the rest), and evidence only a
    /// trimmed ban references is not sent (freenet/river#703).
    #[test]
    fn a_network_delta_trim_keeps_what_the_caps_keep() {
        let owner_sk = SigningKey::from_bytes(&[1; 32]);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let config = Configuration {
            owner_member_id: owner_id,
            max_members: 1,
            max_user_bans: 3,
            ..Default::default()
        };
        // Evidence bound: 2 x max(3, MIN_BANS_DELTA_BOUND) x min(1, 4) = 200. The
        // two caps differ, so reading one for the other changes the bound.
        let invite = |inviter: MemberId, inviter_sk: &SigningKey, seed: u32| {
            let mut bytes = [0u8; 32];
            bytes[..4].copy_from_slice(&seed.to_le_bytes());
            bytes[31] = 7;
            let sk = SigningKey::from_bytes(&bytes);
            let m = AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: inviter,
                    member_vk: sk.verifying_key(),
                },
                inviter_sk,
            );
            (m, sk)
        };
        let mut shallow = Vec::new();
        let mut deep = Vec::new();
        for i in 0..150 {
            let (m, sk) = invite(owner_id, &owner_sk, i);
            let (child, _) = invite(m.member.id(), &sk, 1_000 + i);
            shallow.push(m);
            deep.push(child);
        }
        let mut sender = ChatRoomStateV1::default();
        sender.ban_evidence.members = deep.iter().chain(&shallow).cloned().collect();

        let ban = |by: &SigningKey, secs: u64| {
            AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                    banned_user: MemberId::from(&SigningKey::from_bytes(&[9; 32]).verifying_key()),
                },
                MemberId::from(&by.verifying_key()),
                by,
            )
        };
        let member_sk = SigningKey::from_bytes(&[2; 32]);
        let mut bans: Vec<_> = (0..MIN_BANS_DELTA_BOUND as u64 + 10)
            .map(|t| ban(&member_sk, 100 + t))
            .collect();
        let owner_ban = ban(&owner_sk, 0);
        bans.push(owner_ban.clone());

        let summary =
            ChatRoomStateV1::default().summarize(&ChatRoomStateV1::default(), &parameters);

        // Evidence over the bound, no ban trimmed: the shallowest are kept.
        let mut delta = ChatRoomStateV1Delta {
            ban_evidence: Some(sender.ban_evidence.members.clone()),
            bans: Some(vec![owner_ban.clone()]),
            ..Default::default()
        };
        trim_to_work_bounds(&mut delta, &config, &sender, &summary, &parameters);
        let evidence = delta.ban_evidence.unwrap();
        assert_eq!(evidence.len(), 200);
        assert!(shallow.iter().all(|m| evidence.contains(m)));

        // Bans over the bound: the cap's choice at the bound (the owner's
        // ban, then the newest others) is kept, and evidence only a trimmed
        // ban references is not sent.
        let target_ban = |by: &SigningKey, secs: u64, target: &AuthorizedMember| {
            AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                    banned_user: target.member.id(),
                },
                MemberId::from(&by.verifying_key()),
                by,
            )
        };
        bans.truncate(MIN_BANS_DELTA_BOUND + 10 - 2);
        bans.push(target_ban(&member_sk, 1, &shallow[0]));
        bans.push(target_ban(&member_sk, 10_000, &shallow[1]));
        bans.push(owner_ban.clone());
        let mut sender = sender.clone();
        sender.bans = BansV1(bans.clone());
        let mut delta = ChatRoomStateV1Delta {
            ban_evidence: Some(vec![shallow[0].clone(), shallow[1].clone()]),
            bans: Some(bans),
            ..Default::default()
        };
        trim_to_work_bounds(&mut delta, &config, &sender, &summary, &parameters);
        let bans = delta.bans.unwrap();
        assert_eq!(bans.len(), MIN_BANS_DELTA_BOUND);
        assert!(bans.contains(&owner_ban));
        let oldest_kept = bans
            .iter()
            .filter(|b| b.banned_by != owner_id)
            .map(|b| b.ban.banned_at)
            .min()
            .unwrap();
        assert_eq!(
            oldest_kept,
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(110)
        );
        assert_eq!(delta.ban_evidence.unwrap(), vec![shallow[1].clone()]);
    }

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

        let config = AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_verifying_key.into(),
                ..Configuration::default()
            },
            &owner_signing_key,
        );

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

    /// freenet/river#703 defect 3, and the pin for the inherent
    /// `ChatRoomStateV1::merge` shadowing `ComposableState::merge`.
    ///
    /// A holds 2 messages at cap 2; B's newer configuration raises the cap to
    /// 4 and B holds 4 older messages. The merge ends with cap 4, so A must
    /// keep the 4 newest of the 6. The trait default summarized A against its
    /// own cap 2, withheld B's messages older than A's oldest, and kept only
    /// A's 2 plus whatever was newer: `merge(A, B) != merge(B, A)`.
    ///
    /// The method-call form is asserted to differ from the trait default, so
    /// renaming or deleting the inherent `merge` (which would make
    /// `a.merge(..)` silently resolve to the trait) fails here.
    #[test]
    fn room_merge_reads_the_merged_configuration() {
        let (base, params, owner_sk) = create_empty_chat_room_state();
        let owner_id = params.owner_id();
        let msg = |t: u64| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author: owner_id,
                    time: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + t),
                    content: RoomMessageBody::public(format!("m{t}")),
                },
                &owner_sk,
            )
        };
        let with_cap = |cap: usize, version: u32| {
            AuthorizedConfigurationV1::new(
                Configuration {
                    configuration_version: version,
                    max_recent_messages: cap,
                    ..base.configuration.configuration.clone()
                },
                &owner_sk,
            )
        };
        let mut a = base.clone();
        a.configuration = with_cap(2, 2);
        a.recent_messages.messages = vec![msg(10), msg(11)];
        let mut b = base.clone();
        b.configuration = with_cap(4, 3);
        b.recent_messages.messages = vec![msg(1), msg(2), msg(3), msg(4)];

        let held = |s: &ChatRoomStateV1| -> Vec<String> {
            s.recent_messages
                .messages
                .iter()
                .map(|m| format!("{:?}", m.message.time))
                .collect()
        };

        let mut ab = a.clone();
        ab.merge(&a, &params, &b).unwrap();
        let mut ba = b.clone();
        ba.merge(&b, &params, &a).unwrap();
        assert_eq!(ab, ba, "merge(A, B) must equal merge(B, A)");
        assert_eq!(ab.recent_messages.messages.len(), 4);
        assert_eq!(
            ab.recent_messages.messages,
            vec![msg(3), msg(4), msg(10), msg(11)]
        );

        let mut trait_ab = a.clone();
        ComposableState::merge(&mut trait_ab, &a, &params, &b).unwrap();
        assert_ne!(
            held(&trait_ab),
            held(&ab),
            "the trait default no longer differs here, so this test no longer \
             proves `a.merge(..)` resolves to the inherent room merge"
        );
    }

    /// The UI's incoming-state merge keeps the horizons but must read them
    /// against the configuration the merge ends with: a cap the incoming
    /// configuration raises opens a horizon the receiver's own cap would close.
    #[test]
    fn summary_with_merged_configuration_reads_the_raised_cap() {
        use crate::room_state::message::RetentionHorizon;
        let (base, params, owner_sk) = create_empty_chat_room_state();
        let owner_id = params.owner_id();
        let with_cap = |cap: usize, version: u32| {
            AuthorizedConfigurationV1::new(
                Configuration {
                    configuration_version: version,
                    max_recent_messages: cap,
                    ..base.configuration.configuration.clone()
                },
                &owner_sk,
            )
        };
        let mut a = base.clone();
        a.configuration = with_cap(2, 2);
        a.recent_messages.messages = (10..12u64)
            .map(|t| {
                AuthorizedMessageV1::new(
                    MessageV1 {
                        room_owner: owner_id,
                        author: owner_id,
                        time: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t),
                        content: RoomMessageBody::public(format!("m{t}")),
                    },
                    &owner_sk,
                )
            })
            .collect();
        let mut raised = base.clone();
        raised.configuration = with_cap(4, 3);
        let mut lower_rank = base.clone();
        lower_rank.configuration = with_cap(4, 1);

        assert!(matches!(
            a.summarize(&a, &params).recent_messages.horizon,
            RetentionHorizon::OldestRetained(_)
        ));
        assert_eq!(
            a.summarize_with_merged_configuration(&params, &raised)
                .recent_messages
                .horizon,
            RetentionHorizon::Open
        );
        assert!(matches!(
            a.summarize_with_merged_configuration(&params, &lower_rank)
                .recent_messages
                .horizon,
            RetentionHorizon::OldestRetained(_)
        ));
    }

    /// freenet/river#703: when the merge's open-horizon pass would
    /// carry more older messages than one messages delta may (the
    /// `max(max_recent_messages, 1000)` work bound), the merge must still
    /// succeed, and in both directions end in the same state. Here S holds
    /// the higher-ranked configuration (cap 2) and two messages by P; O holds
    /// 1001 older owner messages and the owner's ban on P, which empties S.
    #[test]
    fn room_merge_past_the_messages_work_bound_succeeds_and_commutes() {
        let (base, params, owner_sk) = create_empty_chat_room_state();
        let owner_id = params.owner_id();
        let p_sk = SigningKey::generate(&mut rand::thread_rng());
        let p = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: p_sk.verifying_key(),
            },
            &owner_sk,
        );
        let at = |t: u64| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + t);
        let msg = |author: MemberId, sk: &SigningKey, t: u64| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author,
                    time: at(t),
                    content: RoomMessageBody::public(format!("m{t}")),
                },
                sk,
            )
        };
        let cfg = |version: u32, cap: usize| {
            AuthorizedConfigurationV1::new(
                Configuration {
                    configuration_version: version,
                    max_recent_messages: cap,
                    max_members: 10,
                    ..base.configuration.configuration.clone()
                },
                &owner_sk,
            )
        };
        let mut s = base.clone();
        s.configuration = cfg(3, 2);
        s.members.members = vec![p.clone()];
        s.recent_messages.messages = vec![
            msg(p.member.id(), &p_sk, 5000),
            msg(p.member.id(), &p_sk, 5001),
        ];
        let mut o = base.clone();
        o.configuration = cfg(2, 1500);
        o.recent_messages.messages = (0..1001).map(|t| msg(owner_id, &owner_sk, t)).collect();
        o.bans = BansV1(vec![AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: at(4000),
                banned_user: p.member.id(),
            },
            owner_id,
            &owner_sk,
        )]);
        s.verify(&s, &params).unwrap();
        o.verify(&o, &params).unwrap();

        let mut so = s.clone();
        so.merge(&s, &params, &o)
            .expect("merge(S, O) must not fail on the work bound");
        let mut os = o.clone();
        os.merge(&o, &params, &s).expect("merge(O, S)");
        assert_eq!(so, os, "merge(S, O) must equal merge(O, S)");
        assert_eq!(
            so.recent_messages.messages,
            vec![
                msg(owner_id, &owner_sk, 999),
                msg(owner_id, &owner_sk, 1000)
            ]
        );
    }

    /// freenet/river#703: the retention horizon assumes everything the
    /// receiver holds survives the merge. Here A is at its cap with two
    /// messages by member P, and B carries the owner's ban on P plus an older
    /// owner message. The ban removes P's messages, so the merged room has
    /// room for the owner message, and both merge directions must keep it.
    #[test]
    fn room_merge_keeps_what_a_removal_frees_room_for() {
        let (mut base, params, owner_sk) = create_empty_chat_room_state();
        let owner_id = params.owner_id();
        let mut cfg = base.configuration.configuration.clone();
        cfg.owner_member_id = owner_id;
        cfg.max_recent_messages = 2;
        base.configuration = AuthorizedConfigurationV1::new(cfg, &owner_sk);
        let p_sk = SigningKey::generate(&mut rand::thread_rng());
        let p = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: p_sk.verifying_key(),
            },
            &owner_sk,
        );
        let at = |t: u64| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + t);
        let msg = |author: MemberId, sk: &SigningKey, t: u64| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author,
                    time: at(t),
                    content: RoomMessageBody::public(format!("m{t}")),
                },
                sk,
            )
        };
        let mut a = base.clone();
        a.members.members = vec![p.clone()];
        a.recent_messages.messages =
            vec![msg(p.member.id(), &p_sk, 10), msg(p.member.id(), &p_sk, 11)];
        let mut b = base.clone();
        b.recent_messages.messages = vec![msg(owner_id, &owner_sk, 1)];
        b.bans = BansV1(vec![AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: at(2),
                banned_user: p.member.id(),
            },
            owner_id,
            &owner_sk,
        )]);
        a.verify(&a, &params).unwrap();
        b.verify(&b, &params).unwrap();

        let mut ab = a.clone();
        ab.merge(&a, &params, &b).unwrap();
        let mut ba = b.clone();
        ba.merge(&b, &params, &a).unwrap();
        assert_eq!(ab, ba, "merge(A, B) must equal merge(B, A)");
        assert_eq!(
            ab.recent_messages.messages,
            vec![msg(owner_id, &owner_sk, 1)]
        );

        let mut trait_ab = a.clone();
        ComposableState::merge(&mut trait_ab, &a, &params, &b).unwrap();
        assert!(
            trait_ab.recent_messages.messages.is_empty(),
            "premise: the trait default withholds the owner message"
        );
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
    /// evicts must not be kept for that ban on the SAME pass (originally as a
    /// prune exemption, since freenet/river#702 as ban evidence). If the cap
    /// ran after that decision, pass 1 and pass 2 would differ —
    /// `cleanup(S) != cleanup(cleanup(S))` — permanently diverging peers that
    /// run cleanup a different number of times.
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

        // Member B: a current member with NO message/DM/secret, so B is pruned;
        // only a surviving ban of B's keeps B's record, as ban evidence.
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

        // Member D: a current member kept by a message, issuing two NEWER
        // bans against ABSENT targets. All three bans are in the non-owner
        // pool, which with no owner bans gets the whole cap, so the two newest
        // (D's) survive and B's oldest is evicted (freenet/river#796).
        let d_sk = SigningKey::generate(rng);
        let d_vk = d_sk.verifying_key();
        let d_id = MemberId::from(&d_vk);
        let member_d = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: d_vk,
            },
            &owner_sk,
        );
        let msg_d = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: d_id,
                time: SystemTime::now(),
                content: RoomMessageBody::public("hi".to_string()),
            },
            &d_sk,
        );

        let base = SystemTime::now();
        let absent1 = MemberId::from(&SigningKey::generate(rng).verifying_key());
        let absent2 = MemberId::from(&SigningKey::generate(rng).verifying_key());
        let d_ban1 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: base + std::time::Duration::from_secs(10),
                banned_user: absent1,
            },
            d_id,
            &d_sk,
        );
        let d_ban2 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: base + std::time::Duration::from_secs(11),
                banned_user: absent2,
            },
            d_id,
            &d_sk,
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
                members: vec![member_b, member_d],
            },
            recent_messages: MessagesV1 {
                messages: vec![msg_d],
                ..Default::default()
            },
            bans: BansV1(vec![ban_c_by_b, d_ban1, d_ban2]),
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

        // State C: a member held alive ONLY by a DM with a banned counterparty.
        // The step-1 exemption must not count it (freenet/river#671) — if it
        // does, M is kept on pass 1 and pruned on pass 2 by step 6's sweep.
        {
            let m_sk = SigningKey::generate(rng);
            let q_sk = SigningKey::generate(rng);
            let m_id = MemberId::from(&m_sk.verifying_key());
            let q_id = MemberId::from(&q_sk.verifying_key());
            let state = ChatRoomStateV1 {
                configuration: cfg(10),
                members: MembersV1 {
                    members: vec![
                        owner_member(m_sk.verifying_key()),
                        owner_member(q_sk.verifying_key()),
                    ],
                },
                bans: BansV1(vec![AuthorizedUserBan::new(
                    UserBan {
                        owner_member_id: owner_id,
                        banned_at: SystemTime::now(),
                        banned_user: q_id,
                    },
                    owner_id,
                    &owner_sk,
                )]),
                direct_messages: crate::room_state::direct_messages::DirectMessagesV1 {
                    messages: vec![crate::room_state::direct_messages::sign_direct_message(
                        &m_sk,
                        m_id,
                        q_id,
                        &owner_vk,
                        1_700_000_000,
                        vec![7u8; 8],
                    )
                    .expect("sign dm")],
                    purges: vec![],
                },
                ..Default::default()
            };
            assert_idem(
                &state,
                "member held only by a DM with a banned counterparty",
            );
        }

        // State D: the same shape, but the counterparty is banned by a DEPUTY
        // rather than the owner. `post_apply_cleanup` sees the converged
        // `member_info`, so step 0 DOES enforce this ban (freenet/river#675).
        // Cleanup must
        // still be a fixpoint.
        {
            let dep_sk = SigningKey::generate(rng);
            let m_sk = SigningKey::generate(rng);
            let q_sk = SigningKey::generate(rng);
            let dep_id = MemberId::from(&dep_sk.verifying_key());
            let m_id = MemberId::from(&m_sk.verifying_key());
            let q_id = MemberId::from(&q_sk.verifying_key());
            let mut owner_info = MemberInfo::new_public(owner_id, 1, "owner".to_string());
            owner_info.deputies = vec![dep_id];
            let state = ChatRoomStateV1 {
                configuration: cfg(10),
                members: MembersV1 {
                    members: vec![
                        owner_member(dep_sk.verifying_key()),
                        owner_member(m_sk.verifying_key()),
                        owner_member(q_sk.verifying_key()),
                    ],
                },
                member_info: MemberInfoV1 {
                    member_info: vec![AuthorizedMemberInfo::new(owner_info, &owner_sk)],
                },
                bans: BansV1(vec![AuthorizedUserBan::new(
                    UserBan {
                        owner_member_id: owner_id,
                        banned_at: SystemTime::now(),
                        banned_user: q_id,
                    },
                    dep_id,
                    &dep_sk,
                )]),
                direct_messages: crate::room_state::direct_messages::DirectMessagesV1 {
                    messages: vec![crate::room_state::direct_messages::sign_direct_message(
                        &m_sk,
                        m_id,
                        q_id,
                        &owner_vk,
                        1_700_000_100,
                        vec![9u8; 8],
                    )
                    .expect("sign dm")],
                    purges: vec![],
                },
                ..Default::default()
            };
            assert_idem(
                &state,
                "member held only by a DM with a DEPUTY-banned counterparty",
            );
        }
    }

    /// `direct_messages` MUST be declared after `members`.
    ///
    /// The whole placement argument for the #671 apply-time sweep rests on
    /// `parent_state.members` being the UPDATED member set when the DM field's
    /// `apply_delta` runs, and that holds only because the `#[composable]`
    /// macro applies fields in DECLARATION order. Serde emits struct fields in
    /// declaration order and ciborium writes them as a map in that order, so
    /// the serialized key order is a faithful proxy for it.
    ///
    /// Reorder the two and the DM sweep would test its held set against the
    /// PRE-update member set — silently restoring the data loss.
    ///
    /// THIS ASSERTS A PROXY, and that is worth naming given what this PR is
    /// about. The property that actually matters is the order in which
    /// `#[composable]` emits the per-field `apply_delta` calls; what this test
    /// reads is serialized CBOR key order. The proxy holds today because serde
    /// emits fields in declaration order, ciborium preserves it, and the macro
    /// derives its order from the same declaration — but it is one step
    /// removed. **A macro change that sorted or reordered field application
    /// would leave this test green while the invariant it guards broke.** This
    /// whole fix exists because a guard resembled the thing it guarded instead
    /// of being it; this one has a milder version of that shape, so the gap is
    /// stated rather than left for a reader to assume away.
    ///
    /// Coincidental coverage exists but is SHRINKING, which is the argument for
    /// keeping this pin rather than against it. The field-reorder mutation
    /// currently fails three tests: this one,
    /// `pruned_sender_can_dm_when_bundling_rejoin_delta` (the #291 rejoin pin),
    /// and `whole_state_merge_is_associative_for_the_671_shape`.
    ///
    /// It was FOUR before the #675 remedy.
    /// `global_cap_must_not_evict_legitimate_dms_for_doomed_ones` stopped
    /// detecting the reorder, and the reason is worth recording rather than
    /// treating as loss: its doomed DMs have an OWNER-banned endpoint, and the
    /// now-ban-aware apply-time sweep removes those on the ban axis whether or
    /// not `parent_state.members` is the updated set. The remedy made that
    /// test robust to field order, so it stopped being a witness to it. The
    /// member axis still depends on declaration order — which is why the other
    /// two still fire, and why this pin is not redundant.
    ///
    /// That is exactly the drift this pin exists for: every other detector is
    /// incidental to some particular DM shape a future author may legitimately
    /// rework, and one of them already has. This is the only one whose failure
    /// message names the reason and the issue number.
    #[test]
    fn field_declaration_order_puts_members_before_direct_messages() {
        // An empty `ban_evidence` is not serialized, so give it one record.
        let mut state = ChatRoomStateV1::default();
        let owner_sk = SigningKey::from_bytes(&[1u8; 32]);
        let member_sk = SigningKey::from_bytes(&[2u8; 32]);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        state.ban_evidence.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member_sk.verifying_key(),
            },
            &owner_sk,
        ));
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&state, &mut bytes).expect("state serializes");
        let value: ciborium::value::Value =
            ciborium::de::from_reader(&bytes[..]).expect("round-trips as a CBOR value");
        let map = value
            .as_map()
            .expect("ChatRoomStateV1 serializes as a CBOR map");
        let index_of = |name: &str| {
            map.iter()
                .position(|(k, _)| k.as_text() == Some(name))
                .unwrap_or_else(|| panic!("field `{name}` missing from the serialized state"))
        };
        assert!(
            index_of("members") < index_of("direct_messages"),
            "`direct_messages` must be declared AFTER `members`: the #[composable] macro \
             applies fields in declaration order, and DirectMessagesV1::apply_delta sweeps \
             its held set against parent_state.members (freenet/river#671)"
        );
        // `MessagesV1::apply_delta` calls `enforced_ban_set_of(parent_state)`,
        // which equals post_apply_cleanup's step-0 set only if every field it
        // reads is applied before `recent_messages`.
        for input in [
            "configuration",
            "bans",
            "members",
            "ban_evidence",
            "member_info",
        ] {
            assert!(
                index_of(input) < index_of("recent_messages"),
                "`{input}` must be declared BEFORE `recent_messages`: \
                 MessagesV1::apply_delta reads it through enforced_ban_set_of"
            );
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
