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

            // Re-read ROOMS at save time so the privacy guard, the sealing
            // secret, and the rejoin probe all see the SAME consistent
            // snapshot. The component captured `self_signing_key` at mount,
            // but `current_secret_opt` and `members` state can change between
            // mount and save (the room secret arrives asynchronously after a
            // private-room join), and using a stale `current_secret_opt =
            // None` is exactly the freenet/river#299 leak.
            let (is_private, current_secret_opt, members_delta) = {
                let Ok(rooms) = ROOMS.try_read() else {
                    warn!(
                        "ROOMS.try_read() returned Err during nickname save — \
                         a concurrent write is in flight; the edit is dropped \
                         and the user will need to retry"
                    );
                    return;
                };
                let current_room = CURRENT_ROOM.read();
                let Some(room_data) = current_room.owner_key.and_then(|k| rooms.map.get(&k)) else {
                    warn!("No room data available for the current room — nickname edit dropped");
                    return;
                };
                (
                    room_data.is_private(),
                    room_data.get_secret().map(|(s, v)| (*s, v)),
                    room_data.build_rejoin_delta().0,
                )
            };

            // Carried into the deferred ROOMS mutation so the cached `self_*`
            // fields can be kept in step with this edit — `new_value` itself
            // is moved into the sealed-nickname construction below.
            let nickname_for_self = new_value.clone();

            let delta = if let Some(signing_key) = self_signing_key.clone() {
                // Privacy guard for freenet/river#299: a private room with no
                // locally-available secret MUST NOT publish a plaintext
                // nickname into `member_info`. `seal_for_room` returns `None`
                // in that case so we defer — the user can retry once the
                // secret has arrived. Revert the input to the on-network
                // value so the UI doesn't silently lie about what was saved.
                let current_secret_ref = current_secret_opt.as_ref().map(|(s, v)| (s, *v));
                let Some(sealed_nickname) =
                    seal_for_room(is_private, current_secret_ref, new_value.into_bytes())
                else {
                    warn!(
                        "Private room secret not yet available locally — \
                         nickname edit deferred to avoid leaking a plaintext \
                         member_info delta (freenet/river#299)."
                    );
                    temp_nickname.set(initial_nickname_for_revert.clone());
                    return;
                };
                let new_member_info = MemberInfo {
                    member_id: member_info.member_info.member_id,
                    version: member_info.member_info.version + 1,
                    preferred_nickname: sealed_nickname,
                };
                let new_authorized_member_info =
                    AuthorizedMemberInfo::new_with_member_key(new_member_info, &signing_key);
                // Re-add ourselves if pruned for inactivity. The member_info
                // delta is the nickname change itself — no extra entry needed.
                Some((
                    ChatRoomStateV1Delta {
                        member_info: Some(vec![new_authorized_member_info.clone()]),
                        members: members_delta,
                        ..Default::default()
                    },
                    new_authorized_member_info,
                ))
            } else {
                warn!("No signing key available");
                None
            };

            if let Some((delta, edited_member_info)) = delta {
                info!("Saving changes to nickname with delta: {:?}", delta);

                // Get the owner key first
                let owner_key = CURRENT_ROOM.read().owner_key;

                if let Some(owner_key) = owner_key {
                    // Defer ROOMS mutation to a clean execution context to
                    // prevent RefCell re-entrant borrow panics.
                    crate::util::defer(move || {
                        let applied = ROOMS.with_mut(|rooms| {
                            if let Some(room_data) = rooms.map.get_mut(&owner_key) {
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
                                    false
                                } else {
                                    info!(
                                        "State after applying nickname delta: {:?}",
                                        room_data.room_state
                                    );
                                    // Keep the cached self_* fields in step
                                    // with the edit so a self-heal or an
                                    // inactivity-rejoin before the next sync
                                    // republishes the edited nickname, not
                                    // the pre-edit one. No-op when the edited
                                    // member is not the local user.
                                    room_data.record_self_nickname_edit(
                                        member_id,
                                        edited_member_info,
                                        nickname_for_self,
                                    );
                                    true
                                }
                            } else {
                                warn!("Room state not found for current room");
                                false
                            }
                        });
                        if applied {
                            crate::components::app::mark_needs_sync(owner_key);
                        }
                    });
                }
            }
        }
    };

    let on_input = move |evt: dioxus_core::Event<FormData>| {
        temp_nickname.set(evt.value().clone());
    };

    let on_blur = {
        let mut save_changes = save_changes.clone();
        move |_| {
            let new_value = temp_nickname();
            save_changes(new_value);
        }
    };

    let on_keydown = {
        let mut save_changes = save_changes.clone();
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
