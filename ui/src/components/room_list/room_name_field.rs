use crate::components::app::{CURRENT_ROOM, EDIT_ROOM_MODAL, ROOMS};
use crate::util::ecies::{seal_for_room, unseal_bytes_with_secrets};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::privacy::RoomDisplayMetadata;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use wasm_bindgen_futures::spawn_local;

/// Whether a keydown in the room-name input should commit the value and close
/// the edit-room dialog (freenet/river#21).
///
/// Only the plain `Enter` key triggers it. In particular this returns `false`
/// for `Key::Process`, which is what browsers report for the `Enter` keystroke
/// that *confirms an IME composition* (e.g. selecting a CJK candidate) — we must
/// not close the dialog on that keystroke. Extracted as a pure function so the
/// IME-vs-submit decision is unit-testable without a Dioxus runtime.
fn enter_commits_and_closes(key: &Key) -> bool {
    key == &Key::Enter
}

#[component]
pub fn RoomNameField(config: Configuration, is_owner: bool) -> Element {
    // Extract and decrypt the room name using version-aware decryption
    let initial_name = {
        let owner_key = CURRENT_ROOM.read().owner_key;
        let secrets = ROOMS
            .try_read()
            .ok()
            .and_then(|rooms| {
                owner_key
                    .and_then(|key| rooms.map.get(&key))
                    .map(|room_data| room_data.secrets.clone())
            })
            .unwrap_or_default();
        match unseal_bytes_with_secrets(&config.display.name, &secrets) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => config.display.name.to_string_lossy(),
        }
    };
    let initial_name_for_revert = initial_name.clone();
    let mut room_name = use_signal(|| initial_name);

    // Save the room name. Takes the value as a `String` (rather than a raw
    // form event) so the same logic can be driven from both `onchange` (commit
    // on blur / native Enter) and the explicit `onkeydown` Enter handler below,
    // which needs to commit the current value *before* closing the modal —
    // closing unmounts the input, so a deferred `onchange` would never fire and
    // the edit would be lost.
    let mut save_room_name = move |new_name: String| {
        if !is_owner {
            return;
        }

        info!("Updating room name");
        if !new_name.is_empty() {
            room_name.set(new_name.clone());

            // Get the owner key first
            let owner_key = CURRENT_ROOM.read().owner_key.expect("No owner key");

            // Get signing data and encryption info from room
            let signing_data = ROOMS.with(|rooms| {
                if let Some(room_data) = rooms.map.get(&owner_key) {
                    Some((
                        room_data.room_key(),
                        room_data.self_sk.clone(),
                        room_data.room_state.clone(),
                        room_data.is_private(),
                        room_data.get_secret().map(|(s, v)| (*s, v)),
                    ))
                } else {
                    error!("Room state not found for current room");
                    None
                }
            });

            let Some((room_key, self_sk, room_state_clone, is_private, room_secret_opt)) =
                signing_data
            else {
                return;
            };

            // Privacy guard for freenet/river#299: a private room with no
            // locally-available secret MUST NOT publish a plaintext room name
            // into the configuration. `seal_for_room` returns `None` in that
            // case so we defer — the owner can retry once the secret has
            // arrived. Revert the input so the UI doesn't silently lie about
            // what was saved.
            let room_secret_ref = room_secret_opt.as_ref().map(|(s, v)| (s, *v));
            let Some(sealed_name) =
                seal_for_room(is_private, room_secret_ref, new_name.clone().into_bytes())
            else {
                warn!(
                    "Private room secret not yet available locally — \
                     room name edit deferred to avoid leaking a plaintext \
                     configuration delta (freenet/river#299)."
                );
                room_name.set(initial_name_for_revert.clone());
                return;
            };

            let mut new_config = config.clone();
            new_config.display = RoomDisplayMetadata {
                name: sealed_name,
                description: new_config.display.description.clone(),
            };
            new_config.configuration_version += 1;

            spawn_local(async move {
                // Serialize config to CBOR for signing
                let mut config_bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(&new_config, &mut config_bytes) {
                    error!("Failed to serialize config for signing: {:?}", e);
                    return;
                }

                // Sign using delegate with fallback to local signing
                let signature =
                    crate::signing::sign_config_with_fallback(room_key, config_bytes, &self_sk)
                        .await;

                let new_authorized_config =
                    AuthorizedConfigurationV1::with_signature(new_config, signature);

                let delta = ChatRoomStateV1Delta {
                    configuration: Some(new_authorized_config),
                    ..Default::default()
                };

                // Defer ROOMS mutation to a clean execution context to
                // prevent RefCell re-entrant borrow panics.
                crate::util::defer(move || {
                    let applied = ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&owner_key) {
                            info!("Applying delta to room state");
                            match ComposableState::apply_delta(
                                &mut room_data.room_state,
                                &room_state_clone,
                                &ChatRoomParametersV1 { owner: owner_key },
                                &Some(delta),
                            ) {
                                Ok(_) => {
                                    info!("Delta applied successfully");
                                    // #310: apply_delta re-runs the public-only
                                    // actions-state rebuild; re-derive private
                                    // edits/reactions with decryption. No-op on
                                    // public rooms.
                                    room_data.rebuild_private_actions_state();
                                    true
                                }
                                Err(e) => {
                                    error!("Failed to apply delta: {:?}", e);
                                    false
                                }
                            }
                        } else {
                            false
                        }
                    });
                    if applied {
                        crate::components::app::mark_needs_sync(owner_key);
                    }
                });
            });
        } else {
            error!("Room name is empty");
        }
    };

    rsx! {
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "Room Name" }
            input {
                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent disabled:opacity-50 disabled:cursor-not-allowed",
                value: "{room_name}",
                readonly: !is_owner,
                disabled: !is_owner,
                // Track the live value so the Enter handler can commit it
                // before the modal (and this input) unmount.
                oninput: move |evt: dioxus_core::Event<FormData>| room_name.set(evt.value().to_string()),
                onchange: {
                    let mut save_room_name = save_room_name.clone();
                    move |evt: dioxus_core::Event<FormData>| save_room_name(evt.value().to_string())
                },
                onkeydown: move |evt: dioxus_core::Event<KeyboardData>| {
                    // Commit + close the dialog on Enter (freenet/river#21).
                    // The dialog auto-saves on change, so this just makes Enter
                    // a one-keystroke "done" for a rename. IME composition
                    // reports `Key::Process` (not `Key::Enter`) for the confirm
                    // keystroke, so this does not fire mid-composition.
                    // Non-owners can't edit, so closing on Enter for them is
                    // just a quick dismiss.
                    if enter_commits_and_closes(&evt.key()) {
                        evt.prevent_default();
                        let value = room_name.read().clone();
                        save_room_name(value);
                        crate::util::defer(move || {
                            EDIT_ROOM_MODAL.write().room = None;
                        });
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Pin the Enter-commits-and-closes semantics for the edit-room dialog
    //! (freenet/river#21), including the IME-composition carve-out: the
    //! `Enter` keystroke that confirms an IME candidate is reported by the
    //! browser as `Key::Process`, and must NOT close the dialog.
    use super::*;

    #[test]
    fn enter_commits_and_closes_the_dialog() {
        assert!(enter_commits_and_closes(&Key::Enter));
    }

    #[test]
    fn ime_composition_confirm_does_not_close() {
        // Browsers report `Process` (DOM `keyCode` 229) for the Enter that
        // confirms an IME composition. Closing on it would discard a
        // half-composed name, so it must be a no-op here.
        assert!(!enter_commits_and_closes(&Key::Process));
    }

    #[test]
    fn other_keys_do_not_close() {
        for key in [
            Key::Escape,
            Key::Tab,
            Key::Backspace,
            Key::Character("a".to_string()),
            Key::ArrowDown,
        ] {
            assert!(
                !enter_commits_and_closes(&key),
                "{key:?} should not commit-and-close"
            );
        }
    }
}
