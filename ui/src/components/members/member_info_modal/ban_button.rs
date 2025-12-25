use crate::components::app::{CURRENT_ROOM, MEMBER_INFO_MODAL, NEEDS_SYNC, ROOMS};
use crate::room_data::RoomData;
use crate::util::get_current_system_time;
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::member::MemberId;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};

#[component]
pub fn BanButton(member_to_ban: MemberId, is_downstream: bool, nickname: String) -> Element {
    // Memos
    let current_room_data_signal: Memo<Option<RoomData>> = use_memo(move || {
        CURRENT_ROOM
            .read()
            .owner_key
            .as_ref()
            .and_then(|key| ROOMS.read().map.get(key).cloned())
    });

    let mut show_confirmation = use_signal(|| false);

    let execute_ban = move |_| {
        if let (Some(current_room), Some(room_data)) = (
            CURRENT_ROOM.read().owner_key,
            current_room_data_signal.read().as_ref(),
        ) {
            let user_signing_key = &room_data.self_sk;
            let ban = UserBan {
                owner_member_id: MemberId::from(&current_room),
                banned_at: get_current_system_time(),
                banned_user: member_to_ban,
            };

            let authorized_ban = AuthorizedUserBan::new(
                ban,
                MemberId::from(&user_signing_key.verifying_key()),
                user_signing_key,
            );

            let delta = ChatRoomStateV1Delta {
                bans: Some(vec![authorized_ban]),
                ..Default::default()
            };

            MEMBER_INFO_MODAL.with_mut(|modal| {
                modal.member = None;
            });

            ROOMS.with_mut(|rooms| {
                if let Some(room_data_mut) = rooms.map.get_mut(&current_room) {
                    if let Err(e) = room_data_mut.room_state.apply_delta(
                        &room_data.room_state,
                        &ChatRoomParametersV1 {
                            owner: current_room,
                        },
                        &Some(delta),
                    ) {
                        error!("Failed to apply ban delta: {:?}", e);
                    } else {
                        info!("Successfully applied ban delta for member {:?}", member_to_ban);

                        // If this is a private room and we're the owner, rotate the secret
                        // This ensures the banned member cannot decrypt future messages
                        if room_data_mut.is_private() && room_data_mut.owner_vk == room_data_mut.self_sk.verifying_key() {
                            info!("Private room - rotating secret after ban to ensure forward secrecy");

                            match room_data_mut.rotate_secret() {
                                Ok(secrets_delta) => {
                                    info!("Secret rotated successfully after ban, applying delta");

                                    // Apply the secrets delta
                                    let current_state = room_data_mut.room_state.clone();
                                    let rotation_delta = ChatRoomStateV1Delta {
                                        secrets: Some(secrets_delta),
                                        ..Default::default()
                                    };

                                    if let Err(e) = room_data_mut.room_state.apply_delta(
                                        &current_state,
                                        &ChatRoomParametersV1 { owner: current_room },
                                        &Some(rotation_delta),
                                    ) {
                                        error!("Failed to apply rotation delta after ban: {}", e);
                                    } else {
                                        info!("Secret rotation applied after ban");
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to rotate secret after ban: {}", e);
                                }
                            }
                        }
                    }
                }
            });

            // Mark room as needing sync to propagate ban and rotation
            NEEDS_SYNC.write().insert(current_room);
            info!("Marked room for synchronization after ban");
        }
    };

    if is_downstream {
        rsx! {
            div { class: "mt-4",
                button {
                    class: "px-4 py-2 bg-red-500 hover:bg-red-600 text-white font-medium rounded-lg transition-colors",
                    onclick: move |_| show_confirmation.set(true),
                    "Ban User"
                }

                if *show_confirmation.read() {
                    // Confirmation modal
                    div {
                        class: "fixed inset-0 z-50 flex items-center justify-center",
                        // Overlay
                        div {
                            class: "absolute inset-0 bg-black/50",
                            onclick: move |_| show_confirmation.set(false)
                        }
                        // Modal content
                        div {
                            class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border",
                            // Header
                            div {
                                class: "px-6 py-4 border-b border-border flex items-center justify-between",
                                h2 { class: "text-lg font-semibold text-text", "Confirm Ban" }
                                button {
                                    class: "p-1 text-text-muted hover:text-text transition-colors",
                                    onclick: move |_| show_confirmation.set(false),
                                    "✕"
                                }
                            }

                            // Body
                            div {
                                class: "px-6 py-4",
                                p { class: "text-text",
                                    "Are you sure you want to ban "
                                    span { class: "font-semibold", "{nickname}" }
                                    " (ID: "
                                    code { class: "text-sm bg-surface px-1 rounded", "{member_to_ban}" }
                                    ")? This action cannot be undone."
                                }
                            }

                            // Footer
                            div {
                                class: "px-6 py-4 border-t border-border flex justify-end gap-3",
                                button {
                                    class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                                    onclick: move |_| show_confirmation.set(false),
                                    "Cancel"
                                }
                                button {
                                    class: "px-4 py-2 bg-red-500 hover:bg-red-600 text-white font-medium rounded-lg transition-colors",
                                    onclick: execute_ban,
                                    "Yes, Ban User"
                                }
                            }
                        }
                    }
                }
            }
        }
    } else {
        rsx! { "" }
    }
}
