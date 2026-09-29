use super::edit_room_modal::sign_and_apply_configuration;
use crate::components::app::{CURRENT_ROOM, EDIT_ROOM_MODAL, ROOMS};
use crate::util::ecies::{seal_for_room, unseal_text_or_placeholder};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use river_core::room_state::configuration::Configuration;
use river_core::room_state::privacy::RoomDisplayMetadata;

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

/// The room's stored name, decrypted with whatever room secrets are available
/// locally.
///
/// Kept out of the render body deliberately, for the same reason as
/// `edit_room_modal::stored_description`: it clones the room's whole secret set
/// and runs an ECIES unseal, and `oninput` re-renders this field on every
/// keystroke, so in the render body that is per-character work on the typing
/// path. It is needed only to seed the editing signal at mount and to revert a
/// save the privacy guard refuses.
///
/// Must not call any hook: it runs inside `use_signal`'s initializer, which
/// evaluates while the scope's hook list is mutably borrowed.
fn stored_room_name(config: &Configuration) -> String {
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
    unseal_text_or_placeholder(&config.display.name, &secrets)
}

/// `is_owner` means "may edit this room's name", which the caller
/// (`EditRoomModal::user_can_edit`) computes as ownership AND holding the local
/// signing key — the rename is signed, so the public half alone is not enough.
#[component]
pub fn RoomNameField(config: Configuration, is_owner: bool) -> Element {
    let seed_config = config.clone();
    let mut room_name = use_signal(move || stored_room_name(&seed_config));

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
                    // The rename is signed, so the private half is required.
                    // A blob that keeps its key elsewhere degrades exactly
                    // like a missing room: the edit is dropped, with a log.
                    // Backstop only. `is_owner` is the caller's key-gated
                    // `user_can_edit`, so a key-less owner gets a disabled
                    // input and the modal's own "this device doesn't hold your
                    // key" notice instead of an edit that only logs (R4).
                    let Some(self_sk) = room_data.signing_key().cloned() else {
                        error!("No local signing key for the current room; room name edit dropped");
                        return None;
                    };
                    Some((
                        room_data.room_key(),
                        self_sk,
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
                room_name.set(stored_room_name(&config));
                return;
            };

            let mut new_config = config.clone();
            new_config.display = RoomDisplayMetadata {
                name: sealed_name,
                description: new_config.display.description.clone(),
            };
            new_config.configuration_version += 1;

            sign_and_apply_configuration(
                owner_key,
                room_key,
                self_sk,
                room_state_clone,
                new_config,
                "Room name",
            );
        } else {
            error!("Room name is empty");
        }
    };

    rsx! {
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "Room Name" }
            input {
                "data-testid": "room-name-input",
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
