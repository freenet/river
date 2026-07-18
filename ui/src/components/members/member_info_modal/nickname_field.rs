use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::util::ecies::{seal_for_room, unseal_bytes_with_secrets};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaPencil;
use dioxus_free_icons::Icon;
use freenet_scaffold::ComposableState;
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::rc::Rc;

#[component]
pub fn NicknameField(member_info: AuthorizedMemberInfo) -> Element {
    // Compute values — `self_signing_key` and `room_secrets` are read once
    // at mount for `is_self` gating and version-aware display decryption.
    // The room's secret (`current_secret_opt`) is intentionally NOT captured
    // here: it can arrive after the modal opens, and `save_changes` re-reads
    // it fresh from ROOMS so the freenet/river#299 privacy guard sees the
    // current state, not a stale snapshot.
    let (self_signing_key, room_secrets) = {
        let current_room = CURRENT_ROOM.read();
        if let Some(key) = current_room.owner_key.as_ref() {
            ROOMS
                .try_read()
                .ok()
                .and_then(|rooms| {
                    rooms.map.get(key).map(|room_data| {
                        (Some(room_data.self_sk.clone()), room_data.secrets.clone())
                    })
                })
                .unwrap_or((None, HashMap::new()))
        } else {
            (None, HashMap::new())
        }
    };

    let self_member_id = self_signing_key
        .as_ref()
        .map(|sk| MemberId::from(&sk.verifying_key()));

    let member_id = member_info.member_info.member_id;
    let is_self = self_member_id
        .as_ref()
        .map(|smi| smi == &member_id)
        .unwrap_or(false);

    // Decrypt nickname for display (version-aware)
    let initial_nickname =
        match unseal_bytes_with_secrets(&member_info.member_info.preferred_nickname, &room_secrets)
        {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => member_info.member_info.preferred_nickname.to_string_lossy(),
        };
    let initial_nickname_for_revert = initial_nickname.clone();
    let mut temp_nickname = use_signal(|| initial_nickname);
    let mut input_element = use_signal(|| None as Option<Rc<MountedData>>);

    let save_changes = {
        info!("Saving nickname changes");

        let self_signing_key = self_signing_key.clone();
        let member_info = member_info.clone();
        let initial_nickname_for_revert = initial_nickname_for_revert.clone();

        move |new_value: String| {
            if new_value.is_empty() {
                warn!("Nickname cannot be empty");
                return;
            }

            let Some(signing_key) = self_signing_key.clone() else {
                warn!("No signing key available");
                return;
            };

            // Resolve the owner key once, synchronously, so the deferred
            // closure mutates the same room the user is looking at.
            let Some(owner_key) = CURRENT_ROOM.read().owner_key else {
                warn!("No room data available for the current room — nickname edit dropped");
                return;
            };

            let member_info = member_info.clone();
            let initial_nickname_for_revert = initial_nickname_for_revert.clone();

            // freenet/river#318: the privacy read (`is_private` /
            // `get_secret`), the seal, the delta build, AND `apply_delta`
            // all happen INSIDE this single deferred `ROOMS.with_mut`
            // block. Reading privacy state and applying the delta in the
            // same borrow makes them atomic with respect to the event loop,
            // so a public→private reconfiguration that lands between the
            // user's keystroke and this closure firing cannot slip a
            // plaintext `SealedBytes::public` nickname into a now-private
            // room. (Previously the seal was built synchronously, before
            // the `setTimeout(0)`, leaving a one-tick TOCTOU window.)
            //
            // Deferring the whole operation also preserves the original
            // RefCell-reentrancy guard: building and applying off the
            // current call stack keeps signal borrows from overlapping.
            crate::util::defer(move || {
                enum SaveOutcome {
                    Applied,
                    /// Private room whose secret hasn't arrived yet: the
                    /// edit is dropped and the input reverted to the
                    /// on-network value so the UI doesn't lie about what
                    /// was saved (freenet/river#299).
                    DeferredNoSecret,
                    NotApplied,
                }

                let outcome = ROOMS.with_mut(|rooms| {
                    let Some(room_data) = rooms.map.get_mut(&owner_key) else {
                        warn!("Room state not found for current room");
                        return SaveOutcome::NotApplied;
                    };

                    // Read privacy mode + sealing secret from the SAME
                    // `room_data` we are about to mutate. `get_secret`
                    // borrows `room_data.secrets`, so copy the secret out
                    // before we take `&mut` below.
                    let is_private = room_data.is_private();
                    let current_secret_opt = room_data.get_secret().map(|(s, v)| (*s, v));
                    // Re-add ourselves if pruned for inactivity. Only the
                    // members element (`.0`) is used — the nickname change
                    // itself is the member_info delta. `.0` carries no
                    // plaintext nickname, so it is privacy-independent.
                    let members_delta = room_data.build_rejoin_delta().0;

                    // Privacy guard (freenet/river#299): a private room with
                    // no locally-available secret MUST NOT publish a
                    // plaintext nickname into `member_info`. `seal_for_room`
                    // returns `None` in that case.
                    let current_secret_ref = current_secret_opt.as_ref().map(|(s, v)| (s, *v));
                    let Some(sealed_nickname) = seal_for_room(
                        is_private,
                        current_secret_ref,
                        new_value.clone().into_bytes(),
                    ) else {
                        warn!(
                            "Private room secret not yet available locally — \
                             nickname edit deferred to avoid leaking a plaintext \
                             member_info delta (freenet/river#299)."
                        );
                        return SaveOutcome::DeferredNoSecret;
                    };

                    // Read the CANONICAL current member_info record fresh from
                    // `room_data`, rather than trusting the (possibly stale)
                    // `member_info` prop captured at render/mount time —
                    // `verify` accepts duplicate member_info records per
                    // member_id (migration safety), so a first-match/prop-only
                    // base can seed this edit from a LOSING (e.g. a
                    // just-revoked deputy grant) record and republish it at a
                    // higher version, reactivating revoked authority
                    // (freenet/river#411 round 8). Fall back to the prop only
                    // if the canonical lookup comes up empty (e.g. self was
                    // pruned and no record is in room_state at all yet).
                    let canonical_base = room_data
                        .room_state
                        .member_info
                        .canonical(member_info.member_info.member_id)
                        .cloned()
                        .unwrap_or_else(|| member_info.clone());

                    // Derive the republished version from the higher of the
                    // canonical room_state version and the cached
                    // `self_member_info` version — not from the canonical
                    // base alone. On a stale/reset client the room_state max
                    // can collide at the SAME version as a still-propagating
                    // record and lose the signature tiebreak, silently
                    // no-op'ing the edit (freenet/river#411 round 8).
                    let cached_version = room_data
                        .self_member_info
                        .as_ref()
                        .filter(|cached| {
                            cached.member_info.member_id == canonical_base.member_info.member_id
                        })
                        .map(|cached| cached.member_info.version)
                        .unwrap_or(0);
                    let next_version = canonical_base.member_info.version.max(cached_version) + 1;

                    let new_member_info = MemberInfo {
                        member_id: canonical_base.member_info.member_id,
                        version: next_version,
                        preferred_nickname: sealed_nickname,
                        // Preserve existing deputy grants (#410): republishing
                        // member_info replaces the whole signed record, so
                        // dropping deputies would silently revoke them.
                        // Preserved from the CANONICAL base, not the stale
                        // prop, for the same reason as the version above.
                        deputies: canonical_base.member_info.deputies.clone(),
                    };
                    let new_authorized_member_info =
                        AuthorizedMemberInfo::new_with_member_key(new_member_info, &signing_key);
                    let delta = ChatRoomStateV1Delta {
                        member_info: Some(vec![new_authorized_member_info.clone()]),
                        members: members_delta,
                        ..Default::default()
                    };

                    info!("Saving changes to nickname with delta: {:?}", delta);
                    info!(
                        "State before applying nickname delta: {:?}",
                        room_data.room_state
                    );
                    if let Err(e) = room_data.room_state.apply_delta(
                        &room_data.room_state.clone(),
                        &ChatRoomParametersV1 { owner: owner_key },
                        &Some(delta),
                    ) {
                        error!("Failed to apply delta: {:?}", e);
                        return SaveOutcome::NotApplied;
                    }
                    info!(
                        "State after applying nickname delta: {:?}",
                        room_data.room_state
                    );
                    // Keep the cached self_* fields in step with the edit so
                    // a self-heal or an inactivity-rejoin before the next
                    // sync republishes the edited nickname, not the pre-edit
                    // one. No-op when the edited member is not the local
                    // user.
                    room_data.record_self_nickname_edit(
                        member_id,
                        new_authorized_member_info,
                        new_value.clone(),
                    );
                    // #310: apply_delta's MessagesV1 step re-runs the
                    // public-only rebuild_actions_state, which wipes private
                    // edits/reactions. Re-derive them with decryption so
                    // changing a nickname doesn't transiently revert an
                    // edited message. No-op on public rooms.
                    room_data.rebuild_private_actions_state();
                    SaveOutcome::Applied
                });

                match outcome {
                    SaveOutcome::Applied => crate::components::app::mark_needs_sync(owner_key),
                    SaveOutcome::DeferredNoSecret => temp_nickname.set(initial_nickname_for_revert),
                    SaveOutcome::NotApplied => {}
                }
            });
        }
    };

    let on_input = move |evt: dioxus_core::Event<FormData>| {
        temp_nickname.set(evt.value().clone());
    };

    let on_blur = {
        let save_changes = save_changes.clone();
        move |_| {
            let new_value = temp_nickname();
            save_changes(new_value);
        }
    };

    let on_keydown = {
        let save_changes = save_changes.clone();
        move |evt: dioxus_core::Event<KeyboardData>| {
            if evt.key() == Key::Enter {
                let new_value = temp_nickname();
                save_changes(new_value);

                // Blur the input element
                if let Some(element) = input_element() {
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = element.set_focus(false).await;
                    });
                }
            }
        }
    };

    rsx! {
        div {
            class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "Nickname" }
            div {
                class: "relative",
                input {
                    class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent",
                    value: "{temp_nickname}",
                    readonly: !is_self,
                    oninput: on_input,
                    onblur: on_blur,
                    onkeydown: on_keydown,
                    onmounted: move |cx| input_element.set(Some(cx.data())),
                }
                if is_self {
                    span {
                        class: "absolute right-3 top-1/2 -translate-y-1/2 text-text-muted",
                        Icon { icon: FaPencil, width: 14, height: 14 }
                    }
                }
            }
        }
    }
}
