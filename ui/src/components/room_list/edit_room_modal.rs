use super::room_name_field::RoomNameField;
use crate::components::app::chat_delegate::save_rooms_to_delegate;
use crate::components::app::{CURRENT_ROOM, EDIT_ROOM_MODAL, ROOMS};
use crate::util::ecies::{seal_for_room, unseal_bytes_with_secrets};
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaCopy;
use dioxus_free_icons::Icon;
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::privacy::{PrivacyMode, RoomDisplayMetadata};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::ops::Deref;
use wasm_bindgen_futures::spawn_local;

#[component]
pub fn EditRoomModal() -> Element {
    // State for leave confirmation
    let mut show_leave_confirmation = use_signal(|| false);

    // Memoize the room being edited
    let editing_room = use_memo(move || {
        // freenet/river#555: anchor before the fallible ROOMS read so a contended
        // pass cannot leave this modal (mounted for the whole session) stale.
        crate::util::signal_guard::anchor();
        EDIT_ROOM_MODAL.read().room.and_then(|editing_room_vk| {
            let rooms = match ROOMS.try_read() {
                Ok(rooms) => rooms,
                Err(_) => {
                    crate::util::signal_guard::schedule_nudge();
                    return None;
                }
            };
            rooms.map.iter().find_map(|(room_vk, room_data)| {
                if &editing_room_vk == room_vk {
                    Some(room_data.clone())
                } else {
                    None
                }
            })
        })
    });

    // Memoize the room configuration
    let room_config = use_memo(move || {
        editing_room
            .read()
            .as_ref()
            .map(|room_data| room_data.room_state.configuration.configuration.clone())
    });

    // Memoize if the current user is the owner of the room being edited.
    //
    // Public half only, deliberately: ownership IS a property of the public
    // key. This drives the *informational* surfaces (the leave warning, and
    // whether the owner-only secret row exists at all), never an editing
    // affordance — see `user_can_edit` below.
    let user_is_owner = use_memo(move || {
        editing_room.read().as_ref().is_some_and(|room_data| {
            // Public half only: an unknown local identity is not the owner,
            // which renders the modal read-only.
            let user_vk = room_data.self_verifying_key();
            let room_vk = EDIT_ROOM_MODAL.read().room.unwrap();
            user_vk == Some(room_vk)
        })
    });

    // Whether this device can actually PERFORM an owner edit. Every
    // configuration change is signed, so it needs the private half as well as
    // ownership. Gating the editable affordances on this rather than on
    // `user_is_owner` is what stops a key-less owner being offered controls
    // whose save would then bail with only a log line (review finding R4).
    // Identical to `user_is_owner` whenever the key is held, which is every
    // room written by a shipped River today.
    let user_can_edit = use_memo(move || {
        *user_is_owner.read()
            && editing_room
                .read()
                .as_ref()
                .is_some_and(|room_data| room_data.signing_key().is_some())
    });

    // Render the modal if room configuration is available
    if let Some(config) = room_config.clone().read().deref() {
        rsx! {
            // Modal backdrop
            div {
                class: "fixed inset-0 z-50 flex items-center justify-center",
                // Overlay
                div {
                    class: "absolute inset-0 bg-black/50",
                    // Signal mutation from an event handler must be deferred
                    // (dioxus-signal-safety: direct writes here are the Firefox
                    // mobile RefCell re-entrancy crash path).
                    onclick: move |_| {
                        crate::util::defer(move || {
                            EDIT_ROOM_MODAL.write().room = None;
                        });
                    }
                }
                // Modal content
                div {
                    "data-testid": "edit-room-modal",
                    class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border max-h-[90vh] overflow-y-auto",
                    div {
                        class: "p-6",
                        h1 { class: "text-xl font-semibold text-text mb-4", "Room Details" }

                        // A key-less owner gets the read-only modal. Say why,
                        // rather than leaving every control unexplainedly
                        // disabled (review finding R4). Plain conditional
                        // markup — no new signal, no new modal.
                        if *user_is_owner.read() && !*user_can_edit.read() {
                            p {
                                "data-testid": "edit-room-no-key-notice",
                                class: "mb-4 px-3 py-2 rounded-lg bg-yellow-500/10 border border-yellow-500/20 text-yellow-400 text-sm",
                                "This device doesn't hold your key for this room, so you can't change its settings here."
                            }
                        }

                        RoomNameField {
                            config: config.clone(),
                            is_owner: *user_can_edit.read()
                        }

                        RoomDescriptionField {
                            config: config.clone(),
                            is_owner: *user_can_edit.read()
                        }

                        // Member capacity
                        if let Some(room_data) = editing_room.read().as_ref() {
                            {
                                let member_count = room_data.room_state.members.members.len();
                                let max_members = config.max_members;
                                let is_full = member_count >= max_members;
                                rsx! {
                                    MaxMembersField {
                                        member_count: member_count,
                                        max_members: max_members,
                                        is_full: is_full,
                                        is_owner: *user_can_edit.read(),
                                        config: config.clone(),
                                    }
                                }
                            }
                        }

                        // Numeric configuration fields (owner-only, and only
                        // when this device can sign the resulting config edit)
                        if *user_can_edit.read() {
                            NumericConfigField {
                                label: "Max Recent Messages",
                                value: config.max_recent_messages,
                                config: config.clone(),
                                field: ConfigField::MaxRecentMessages,
                            }
                            NumericConfigField {
                                label: "Max Message Size (bytes)",
                                value: config.max_message_size,
                                config: config.clone(),
                                field: ConfigField::MaxMessageSize,
                            }
                            NumericConfigField {
                                label: "Max User Bans",
                                value: config.max_user_bans,
                                config: config.clone(),
                                field: ConfigField::MaxUserBans,
                            }
                            NumericConfigField {
                                label: "Max Nickname Size",
                                value: config.max_nickname_size,
                                config: config.clone(),
                                field: ConfigField::MaxNicknameSize,
                            }
                            NumericConfigField {
                                label: "Max Room Name Size",
                                value: config.max_room_name,
                                config: config.clone(),
                                field: ConfigField::MaxRoomName,
                            }
                            NumericConfigField {
                                label: "Max Room Description Size",
                                value: config.max_room_description,
                                config: config.clone(),
                                field: ConfigField::MaxRoomDescription,
                            }
                        }

                        // Read-only room info
                        if let Some(room_data) = editing_room.read().as_ref() {
                            // Room Public Key
                            div {
                                class: "mt-4",
                                label {
                                    class: "block text-sm font-medium text-text-muted mb-1",
                                    title: "Ed25519 public key (Curve25519 elliptic curve)",
                                    "Room Public Key"
                                }
                                div {
                                    class: "flex items-center gap-2",
                                    input {
                                        r#type: "text",
                                        readonly: true,
                                        "data-testid": "room-public-key-input",
                                        title: "Ed25519 public key (Curve25519 elliptic curve)",
                                        // `select-text`, NOT `select-all` — see the note on
                                        // `CopyButton` below. `user-select: all` makes this
                                        // field completely unselectable in Firefox.
                                        class: "flex-1 min-w-0 px-3 py-2 bg-surface border border-border rounded-lg text-text-muted text-sm font-mono cursor-text select-text",
                                        value: "{bs58::encode(room_data.owner_vk.as_bytes()).into_string()}"
                                    }
                                    CopyButton {
                                        value: bs58::encode(room_data.owner_vk.as_bytes()).into_string(),
                                        testid: "room-public-key-copy-button",
                                        label: "Copy room public key",
                                    }
                                }
                            }
                            // Contract ID
                            div {
                                class: "mt-4",
                                label {
                                    class: "block text-sm font-medium text-text-muted mb-1",
                                    "Contract ID"
                                }
                                div {
                                    class: "flex items-center gap-2",
                                    input {
                                        r#type: "text",
                                        readonly: true,
                                        "data-testid": "contract-id-input",
                                        // `select-text`, NOT `select-all` — see `CopyButton`.
                                        class: "flex-1 min-w-0 px-3 py-2 bg-surface border border-border rounded-lg text-text-muted text-sm font-mono cursor-text select-text",
                                        value: "{room_data.contract_key.id()}"
                                    }
                                    CopyButton {
                                        value: room_data.contract_key.id().to_string(),
                                        testid: "contract-id-copy-button",
                                        label: "Copy contract ID",
                                    }
                                }
                            }

                            // Secret Version (only for private rooms)
                            {
                                let is_private = room_data.room_state.configuration.configuration.privacy_mode == PrivacyMode::Private;
                                let is_owner = room_data.is_self_owner();
                                // Rotation derives AND signs the new secret, so
                                // it needs the private half, not just ownership
                                // (review finding R4). The button stays visible
                                // for a key-less owner but is disabled with an
                                // explanatory title — this row has no error
                                // surface of its own, and silently offering a
                                // control that only logs on failure is the bug.
                                let can_rotate = is_owner && room_data.signing_key().is_some();
                                let rotate_title = if can_rotate {
                                    "Rotate room secret now (e.g., after suspecting a leak). The delegate also rotates automatically when the member set changes."
                                } else {
                                    "This device doesn't hold your key for this room, so it can't rotate the secret."
                                };
                                let secret_version = room_data.room_state.secrets.current_version;
                                let owner_vk = room_data.owner_vk;

                                if is_private {
                                    Some(rsx! {
                                        div {
                                            class: "mt-4",
                                            label {
                                                class: "block text-sm font-medium text-text-muted mb-1",
                                                "Secret Version"
                                            }
                                            div {
                                                class: "flex items-center gap-2",
                                                input {
                                                    r#type: "text",
                                                    readonly: true,
                                                    "data-testid": "secret-version-input",
                                                    // `select-text`, NOT `select-all` — see `CopyButton`.
                                                    class: "flex-1 min-w-0 px-3 py-2 bg-surface border border-border rounded-lg text-text-muted text-sm font-mono cursor-text select-text",
                                                    value: "{secret_version}"
                                                }
                                                if is_owner {
                                                    // Manual "Rotate" button restored in #228 PR 2 v2.
                                                    // The original PR removed this on the assumption the
                                                    // chat delegate would drive all rotation, but the
                                                    // delegate path runs asynchronously via
                                                    // ContractNotification — too slow for an explicit
                                                    // owner action. Both UI rotate (here) and delegate
                                                    // rotate use `derive_room_secret`, so concurrent
                                                    // rotation produces byte-identical secrets and
                                                    // converges via the contract's duplicate-version
                                                    // dedup.
                                                    button {
                                                        class: "px-3 py-2 bg-accent hover:bg-accent-hover text-white text-sm rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                                                        title: "{rotate_title}",
                                                        disabled: !can_rotate,
                                                        onclick: move |_| {
                                                            crate::util::defer(move || {
                                                                let mut applied = false;
                                                                ROOMS.with_mut(|rooms| {
                                                                    if let Some(room_data_mut) = rooms.map.get_mut(&owner_vk) {
                                                                        let captured_state = room_data_mut.room_state.clone();
                                                                        match room_data_mut.rotate_secret() {
                                                                            Ok(secrets_delta) => {
                                                                                let delta = ChatRoomStateV1Delta {
                                                                                    secrets: Some(secrets_delta),
                                                                                    ..Default::default()
                                                                                };
                                                                                if let Err(e) = ComposableState::apply_delta(
                                                                                    &mut room_data_mut.room_state,
                                                                                    &captured_state,
                                                                                    &ChatRoomParametersV1 { owner: owner_vk },
                                                                                    &Some(delta),
                                                                                ) {
                                                                                    error!("Failed to apply manual rotation delta: {:?}", e);
                                                                                } else {
                                                                                    info!("Manual rotation succeeded");
                                                                                    // #310: apply_delta re-runs the public-only
                                                                                    // actions-state rebuild; re-derive private
                                                                                    // edits/reactions with decryption.
                                                                                    room_data_mut.rebuild_private_actions_state();
                                                                                    applied = true;
                                                                                }
                                                                            }
                                                                            Err(e) => {
                                                                                error!("Manual rotation failed: {}", e);
                                                                            }
                                                                        }
                                                                    }
                                                                });
                                                                if applied {
                                                                    crate::components::app::mark_needs_sync(owner_vk);
                                                                }
                                                            });
                                                        },
                                                        "Rotate"
                                                    }
                                                }
                                            }
                                        }
                                    })
                                } else {
                                    None
                                }
                            }
                        }

                        // Leave Room Section
                        if *show_leave_confirmation.read() {
                            div {
                                class: "bg-yellow-500/10 border border-yellow-500/20 rounded-lg p-4 mt-4",
                                p {
                                    class: "text-yellow-400 mb-3",
                                    if *user_is_owner.read() {
                                        "Warning: You are the owner of this room. Leaving will permanently delete it for you. Other members might retain access if they have the contract key, but coordination will be lost."
                                    } else {
                                        "Are you sure you want to leave this room? This action cannot be undone."
                                    }
                                }
                                div {
                                    class: "flex gap-3",
                                    button {
                                        class: "px-4 py-2 bg-red-500 hover:bg-red-600 text-white font-medium rounded-lg transition-colors",
                                        onclick: move |_| {
                                            // Read the room_vk first and drop the read borrow
                                            let room_vk_to_remove = EDIT_ROOM_MODAL.read().room;

                                            if let Some(room_vk) = room_vk_to_remove {
                                                // Defer signal mutations to a clean execution
                                                // context to prevent RefCell re-entrant borrow panics.
                                                crate::util::defer(move || {
                                                    // `leave_room` removes from `map` AND adds the
                                                    // owner VK to `removed_rooms`. The tombstone
                                                    // is what makes leave survive across reloads /
                                                    // legacy-delegate merges (freenet/river#247).
                                                    ROOMS.write().leave_room(room_vk);

                                                    // Leaving cancels any earlier same-session
                                                    // rejoin intent, so a later background content
                                                    // update can't resurrect this room on a stale
                                                    // rejoin flag (freenet/river#345 round-9,
                                                    // skeptical/big-picture review).
                                                    crate::components::app::chat_delegate::clear_room_rejoined(&room_vk);

                                                    // Check and potentially clear CURRENT_ROOM
                                                    if CURRENT_ROOM.read().owner_key == Some(room_vk) {
                                                        CURRENT_ROOM.write().owner_key = None;
                                                    }

                                                    // Close the modal *last*
                                                    EDIT_ROOM_MODAL.write().room = None;

                                                    // Save updated rooms (including the tombstone)
                                                    // to delegate storage.
                                                    info!("Room removed, saving to delegate");
                                                    spawn(async move {
                                                        if let Err(e) = save_rooms_to_delegate().await {
                                                            error!("Failed to save rooms after removal: {}", e);
                                                        }
                                                    });
                                                });
                                            }
                                            // Reset confirmation state regardless
                                            show_leave_confirmation.set(false);
                                        },
                                        "Confirm Leave"
                                    }
                                    button {
                                        class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                                        onclick: move |_| show_leave_confirmation.set(false),
                                        "Cancel"
                                    }
                                }
                            }
                        } else {
                             // Only show Leave button if not confirming
                            div {
                                class: "mt-4",
                                button {
                                    "data-testid": "edit-room-leave-button",
                                    class: "px-4 py-2 border border-red-500 text-red-500 hover:bg-red-500/10 rounded-lg transition-colors",
                                    onclick: move |_| show_leave_confirmation.set(true),
                                    "Leave Room"
                                }
                            }
                        }
                    }
                    // Close button
                    button {
                        "data-testid": "edit-room-close-button",
                        class: "absolute top-3 right-3 p-1 text-text-muted hover:text-text transition-colors",
                        // Deferred close — same signal-safety rule as the
                        // backdrop handler above.
                        onclick: move |_| {
                            crate::util::defer(move || {
                                EDIT_ROOM_MODAL.write().room = None;
                            });
                        },
                        "✕"
                    }
                }
            }
        }
    } else {
        rsx! {}
    }
}

/// Copy-to-clipboard button for the read-only key fields above.
///
/// # Why those inputs are `select-text` and must never be `select-all`
///
/// The Room Public Key / Contract ID / Secret Version inputs used Tailwind's
/// `select-all` (`user-select: all`), which made them impossible to select or
/// copy by hand in Firefox: click-drag selected nothing, double-click selected
/// nothing (freenet/river#537). Firefox parses the declaration — the computed
/// value really is `all` — but selecting inside an `<input>` under it yields a
/// zero-length selection, so `Ctrl+C` copies nothing. Chromium and WebKit
/// instead select the whole value, which is why this looked Firefox-specific.
///
/// Measured with a standalone repro driven by Playwright (characters selected
/// by a click-drag across the field, then by a double-click):
///
/// | input rule                | Firefox | Chromium | WebKit |
/// |---------------------------|---------|----------|--------|
/// | `user-select: all`        |   **0** |       44 |     44 |
/// | `user-select: text`       |      44 |       44 |     44 |
///
/// So the fields declare `select-text` explicitly rather than relying on the
/// `auto` default. Do NOT "restore" `select-all` — it re-breaks Firefox.
/// Pinned by `ui/tests/room-info-key-selection.spec.ts`, which runs against
/// Firefox in CI.
///
/// This is a child component rather than inline markup because the "Copied!"
/// feedback needs `use_signal`, and the fields render inside an `if let`
/// branch where a hook call would be conditional.
#[component]
fn CopyButton(value: String, testid: String, label: String) -> Element {
    let mut copied = use_signal(|| false);
    let value_for_clipboard = value.clone();

    rsx! {
        button {
            r#type: "button",
            "data-testid": "{testid}",
            "aria-label": "{label}",
            title: "{label}",
            class: "flex-shrink-0 px-3 py-2 bg-surface hover:bg-surface-hover border border-border rounded-lg text-text-muted hover:text-text text-sm transition-colors flex items-center gap-1.5",
            onclick: move |_| {
                crate::util::copy_to_clipboard(&value_for_clipboard);
                copied.set(true);
            },
            Icon { icon: FaCopy, width: 12, height: 12 }
            span { if *copied.read() { "Copied!" } else { "Copy" } }
        }
    }
}

/// The room's stored description, decrypted with whatever room secrets are
/// available locally.
///
/// Kept out of the render body deliberately. It clones the room's whole secret
/// set and runs an ECIES unseal, and now that `oninput` re-renders the field on
/// every keystroke (freenet/river#564) that would be per-character work on the
/// typing path. It is only ever needed twice: to seed the editing signal when
/// the field mounts, and to revert a save the privacy guard refuses.
///
/// Must not call any hook: it runs inside `use_signal`'s initializer, which
/// evaluates while the scope's hook list is mutably borrowed.
fn stored_description(config: &Configuration) -> String {
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
    config
        .display
        .description
        .as_ref()
        .map(|sealed| match unseal_bytes_with_secrets(sealed, &secrets) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => sealed.to_string_lossy(),
        })
        .unwrap_or_default()
}

#[component]
fn RoomDescriptionField(config: Configuration, is_owner: bool) -> Element {
    let seed_config = config.clone();
    let mut description = use_signal(move || stored_description(&seed_config));

    let update_description = move |evt: Event<FormData>| {
        if !is_owner {
            return;
        }

        let new_desc = evt.value().to_string();
        description.set(new_desc.clone());

        let owner_key = CURRENT_ROOM.read().owner_key.expect("No owner key");

        // A configuration edit is signed, so it needs the private half. Fold
        // the key into the same `Option` the "room not found" case already
        // uses: a room whose blob carries no local signing key simply cannot
        // publish a config change.
        let signing_data = ROOMS.with(|rooms| {
            rooms.map.get(&owner_key).and_then(|room_data| {
                room_data.signing_key().cloned().map(|self_sk| {
                    (
                        room_data.room_key(),
                        self_sk,
                        room_data.room_state.clone(),
                        room_data.is_private(),
                        room_data.get_secret().map(|(s, v)| (*s, v)),
                    )
                })
            })
        });

        let Some((room_key, self_sk, room_state_clone, is_private, room_secret_opt)) = signing_data
        else {
            // Backstop only. `is_owner` is now the key-gated `user_can_edit`,
            // so a key-less owner never gets an enabled textarea to reach this
            // from; the modal explains the refusal up front instead (R4).
            warn!("Cannot update the room description: room or local signing key unavailable");
            return;
        };

        // Privacy guard for freenet/river#299: a private room with no
        // locally-available secret MUST NOT publish a plaintext description
        // into the configuration. `seal_for_room` returns `None` in that
        // case so we defer — the owner can retry once the secret has
        // arrived. Revert the input so the UI doesn't silently lie about
        // what was saved. An empty description is published as `None`
        // (clears the field) and is intentionally exempt from the guard:
        // there's nothing to leak.
        let sealed_desc = if new_desc.is_empty() {
            None
        } else {
            let room_secret_ref = room_secret_opt.as_ref().map(|(s, v)| (s, *v));
            let Some(sealed) = seal_for_room(is_private, room_secret_ref, new_desc.into_bytes())
            else {
                warn!(
                    "Private room secret not yet available locally — \
                     room description edit deferred to avoid leaking a \
                     plaintext configuration delta (freenet/river#299)."
                );
                description.set(stored_description(&config));
                return;
            };
            Some(sealed)
        };

        let mut new_config = config.clone();
        new_config.display = RoomDisplayMetadata {
            name: new_config.display.name.clone(),
            description: sealed_desc,
        };
        new_config.configuration_version += 1;

        spawn_local(async move {
            let mut config_bytes = Vec::new();
            if let Err(e) = ciborium::ser::into_writer(&new_config, &mut config_bytes) {
                error!("Failed to serialize config for signing: {:?}", e);
                return;
            }

            let signature =
                crate::signing::sign_config_with_fallback(room_key, config_bytes, &self_sk).await;

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
                        match ComposableState::apply_delta(
                            &mut room_data.room_state,
                            &room_state_clone,
                            &ChatRoomParametersV1 { owner: owner_key },
                            &Some(delta),
                        ) {
                            Ok(_) => {
                                info!("Room description updated successfully");
                                // #310: apply_delta re-runs the public-only
                                // actions-state rebuild; re-derive private
                                // edits/reactions with decryption. No-op on
                                // public rooms.
                                room_data.rebuild_private_actions_state();
                                true
                            }
                            Err(e) => {
                                error!("Failed to apply description delta: {:?}", e);
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
    };

    rsx! {
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "Room Description" }
            textarea {
                "data-testid": "room-description-input",
                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent disabled:opacity-50 disabled:cursor-not-allowed resize-y",
                rows: "3",
                placeholder: "Optional room description",
                value: "{description}",
                readonly: !is_owner,
                disabled: !is_owner,
                // Track the live value on every keystroke (freenet/river#564).
                // `value` is a VOLATILE attribute in dioxus-html, so dioxus
                // re-writes it to the DOM on every re-render even when the
                // rendered string is unchanged (dioxus-core
                // `diff/node.rs:463`), and the interpreter then assigns
                // `node.value = value` whenever the live DOM value differs
                // (`set_attribute.ts:31`). With `onchange` alone the signal
                // still held the pre-typing text, so each re-render reset the
                // textarea and the owner lost whatever they had typed but not
                // yet committed. Tracking the live value makes the re-write a
                // no-op, which is why the sibling name field never had this.
                //
                // As shipped, the re-render came on every room-state write:
                // this component read CURRENT_ROOM and ROOMS in its render
                // body. It no longer does (that moved into
                // `stored_description`, called from the `use_signal` seed), so
                // today the trigger is a `config`/`is_owner` prop change. The
                // handler is required either way: `oninput` makes the field
                // correct under ANY re-render, which is the point, since the
                // trigger is never local to the component.
                // Guarded like `update_description` below. The field is
                // `disabled` for non-owners so this cannot fire from real
                // input, but leaving the signal un-tracked for them is the
                // correct behaviour: the volatile re-write then restores the
                // stored value rather than letting a synthetic event drift the
                // display away from what is actually saved.
                oninput: move |evt: Event<FormData>| {
                    if is_owner {
                        description.set(evt.value().to_string());
                    }
                },
                onchange: update_description,
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
#[allow(clippy::enum_variant_names)]
enum ConfigField {
    MaxRecentMessages,
    MaxMessageSize,
    MaxUserBans,
    MaxNicknameSize,
    MaxRoomName,
    MaxRoomDescription,
}

impl ConfigField {
    fn get(self, cfg: &Configuration) -> usize {
        match self {
            Self::MaxRecentMessages => cfg.max_recent_messages,
            Self::MaxMessageSize => cfg.max_message_size,
            Self::MaxUserBans => cfg.max_user_bans,
            Self::MaxNicknameSize => cfg.max_nickname_size,
            Self::MaxRoomName => cfg.max_room_name,
            Self::MaxRoomDescription => cfg.max_room_description,
        }
    }

    fn set(self, cfg: &mut Configuration, val: usize) {
        match self {
            Self::MaxRecentMessages => cfg.max_recent_messages = val,
            Self::MaxMessageSize => cfg.max_message_size = val,
            Self::MaxUserBans => cfg.max_user_bans = val,
            Self::MaxNicknameSize => cfg.max_nickname_size = val,
            Self::MaxRoomName => cfg.max_room_name = val,
            Self::MaxRoomDescription => cfg.max_room_description = val,
        }
    }
}

#[component]
fn NumericConfigField(
    label: &'static str,
    value: usize,
    config: Configuration,
    field: ConfigField,
) -> Element {
    let mut input_value = use_signal(|| value.to_string());

    let update_value = move |evt: Event<FormData>| {
        let new_val_str = evt.value().to_string();
        input_value.set(new_val_str.clone());

        let Ok(new_val) = new_val_str.parse::<usize>() else {
            return;
        };
        if new_val == 0 || new_val == field.get(&config) {
            return;
        }

        info!("Updating {label} to {new_val}");

        let owner_key = CURRENT_ROOM.read().owner_key.expect("No owner key");

        // Signed configuration edit: fold the private key into the same
        // `Option` the "room not found" case already uses, so a blob without
        // a local signing key degrades to "cannot edit" rather than panicking.
        let signing_data = ROOMS.with(|rooms| {
            rooms.map.get(&owner_key).and_then(|room_data| {
                room_data
                    .signing_key()
                    .cloned()
                    .map(|self_sk| (room_data.room_key(), self_sk, room_data.room_state.clone()))
            })
        });

        let Some((room_key, self_sk, room_state_clone)) = signing_data else {
            // Backstop only: this whole field is rendered behind the key-gated
            // `user_can_edit`, so a key-less owner never sees it (R4).
            warn!("Cannot update the room configuration: room or local signing key unavailable");
            return;
        };

        let mut new_config = config.clone();
        field.set(&mut new_config, new_val);
        new_config.configuration_version += 1;

        spawn_local(async move {
            let mut config_bytes = Vec::new();
            if let Err(e) = ciborium::ser::into_writer(&new_config, &mut config_bytes) {
                error!("Failed to serialize config: {:?}", e);
                return;
            }

            let signature =
                crate::signing::sign_config_with_fallback(room_key, config_bytes, &self_sk).await;

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
                        match ComposableState::apply_delta(
                            &mut room_data.room_state,
                            &room_state_clone,
                            &ChatRoomParametersV1 { owner: owner_key },
                            &Some(delta),
                        ) {
                            Ok(_) => {
                                info!("{label} updated successfully");
                                // #310: apply_delta re-runs the public-only
                                // actions-state rebuild; re-derive private
                                // edits/reactions with decryption. No-op on
                                // public rooms.
                                room_data.rebuild_private_actions_state();
                                true
                            }
                            Err(e) => {
                                error!("Failed to apply {label} delta: {:?}", e);
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
    };

    rsx! {
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "{label}" }
            input {
                r#type: "number",
                min: "1",
                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent",
                value: "{input_value}",
                // Track the live value so a re-render cannot reset the field
                // mid-typing (freenet/river#564). See the RoomDescriptionField
                // textarea above for the full mechanism: `value` is volatile,
                // so it is re-written to the DOM on every re-render, and this
                // field re-renders whenever its `config` prop changes.
                oninput: move |evt: Event<FormData>| input_value.set(evt.value().to_string()),
                onchange: update_value,
            }
        }
    }
}

#[component]
fn MaxMembersField(
    member_count: usize,
    max_members: usize,
    is_full: bool,
    is_owner: bool,
    config: Configuration,
) -> Element {
    let mut max_members_input = use_signal(|| max_members.to_string());

    let update_max_members = move |evt: Event<FormData>| {
        if !is_owner {
            return;
        }
        let new_val_str = evt.value().to_string();
        max_members_input.set(new_val_str.clone());

        let Ok(new_max) = new_val_str.parse::<usize>() else {
            return;
        };
        if new_max == 0 || new_max == config.max_members {
            return;
        }

        info!("Updating max_members to {new_max}");

        let owner_key = CURRENT_ROOM.read().owner_key.expect("No owner key");

        // Signed configuration edit: fold the private key into the same
        // `Option` the "room not found" case already uses, so a blob without
        // a local signing key degrades to "cannot edit" rather than panicking.
        let signing_data = ROOMS.with(|rooms| {
            rooms.map.get(&owner_key).and_then(|room_data| {
                room_data
                    .signing_key()
                    .cloned()
                    .map(|self_sk| (room_data.room_key(), self_sk, room_data.room_state.clone()))
            })
        });

        let Some((room_key, self_sk, room_state_clone)) = signing_data else {
            // Backstop only: the input is rendered behind the key-gated
            // `user_can_edit`, so a key-less owner never sees it (R4).
            warn!("Cannot update the room configuration: room or local signing key unavailable");
            return;
        };

        let mut new_config = config.clone();
        new_config.max_members = new_max;
        new_config.configuration_version += 1;

        wasm_bindgen_futures::spawn_local(async move {
            let mut config_bytes = Vec::new();
            if let Err(e) = ciborium::ser::into_writer(&new_config, &mut config_bytes) {
                error!("Failed to serialize config: {:?}", e);
                return;
            }

            let signature =
                crate::signing::sign_config_with_fallback(room_key, config_bytes, &self_sk).await;

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
                        match ComposableState::apply_delta(
                            &mut room_data.room_state,
                            &room_state_clone,
                            &ChatRoomParametersV1 { owner: owner_key },
                            &Some(delta),
                        ) {
                            Ok(_) => {
                                info!("max_members updated successfully");
                                // #310: apply_delta re-runs the public-only
                                // actions-state rebuild; re-derive private
                                // edits/reactions with decryption. No-op on
                                // public rooms.
                                room_data.rebuild_private_actions_state();
                                true
                            }
                            Err(e) => {
                                error!("Failed to apply max_members delta: {:?}", e);
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
    };

    rsx! {
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2",
                "Members ({member_count}/{max_members})"
            }
            if is_full {
                p { class: "text-xs text-red-400 mb-1",
                    "Room is full — new members will be rejected."
                }
            }
            if is_owner {
                input {
                    "data-testid": "max-members-input",
                    r#type: "number",
                    min: "1",
                    class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text focus:outline-none focus:ring-2 focus:ring-accent focus:border-transparent",
                    value: "{max_members_input}",
                    // Track the live value so a re-render cannot reset the
                    // field mid-typing (freenet/river#564). This one re-renders
                    // on its `member_count` prop too, so a member joining while
                    // the owner was editing the cap was enough to wipe it.
                    oninput: move |evt: Event<FormData>| max_members_input.set(evt.value().to_string()),
                    onchange: update_max_members,
                }
            }
        }
    }
}
