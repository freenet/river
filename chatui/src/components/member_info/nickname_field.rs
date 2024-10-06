use dioxus::prelude::*;
use dioxus_logger::tracing::{error, info, warn};
use common::state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use common::state::member::{AuthorizedMember, MemberId};
use common::state::member_info::{AuthorizedMemberInfo, MemberInfo};
use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use crate::components::app::{CurrentRoom, Rooms};
use crate::util::get_current_room_data;

#[component]
pub fn NicknameField(
    member: AuthorizedMember,
    member_info: AuthorizedMemberInfo
) -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_data = get_current_room_data(rooms, current_room);

    let self_signing_key = use_memo(move || {
        current_room_data
            .read()
            .as_ref()
            .and_then(|room_state| room_state.user_signing_key.clone())
    });

    let self_member_id = use_memo(move || {
        self_signing_key
            .read()
            .as_ref()
            .map(|sk| MemberId::new(&sk.verifying_key()))
    });

    let is_self = use_memo(move || {
        self_member_id
            .read()
            .as_ref()
            .map(|smi| smi == &member.member.id())
            .unwrap_or(false)
    });

    let mut nickname = use_signal(|| member_info.member_info.preferred_nickname.clone());

    let update_nickname = move |evt: Event<FormData>| {
        info!("Updating nickname");
        let new_nickname = evt.value().to_string();
        if !new_nickname.is_empty() { // TODO: Verify nickname doesn't exceed max length per room config
            nickname.set(new_nickname.clone());
            let self_member_id = member_info.member_info.member_id.clone();
            let new_member_info = MemberInfo {
                member_id: self_member_id,
                version: member_info.member_info.version + 1,
                preferred_nickname: new_nickname,
            };
            let owner_key = current_room.read().owner_key.expect("No owner key");
            let signing_key = if member.member.member_vk == owner_key {
                // If the member is the room owner, use the room owner's key
                self_signing_key.read().as_ref().expect("No signing key").clone()
            } else {
                // Otherwise, use the member's key
                match SigningKey::from_bytes(&member.member.member_vk.to_bytes()) {
                    Ok(key) => key,
                    Err(e) => {
                        error!("Failed to create SigningKey from VerifyingKey: {}", e);
                        return;
                    }
                }
            };
            info!("Creating new authorized member info using signing key for member: {:?}", member.member.id());
            let new_authorized_member_info = AuthorizedMemberInfo::new(
                new_member_info,
                &signing_key
            );
            let delta = ChatRoomStateV1Delta {
                recent_messages: None,
                configuration: None,
                bans: None,
                members: None,
                member_info: Some(vec![new_authorized_member_info]),
                upgrade: None,
            };
            
            let mut rooms_write_guard = rooms.write();
            let owner_key = match current_room.read().owner_key {
                Some(key) => key,
                None => {
                    error!("Owner key is None");
                    return;
                }
            };

            if let Some(room_data) = rooms_write_guard.map.get_mut(&owner_key) {
                info!("Applying delta to room state");
                match room_data.room_state.apply_delta(
                    &room_data.room_state.clone(), // Clone the room_state for parent_state
                    &ChatRoomParametersV1 { owner: owner_key },
                    &delta
                ) {
                    Ok(_) => info!("Delta applied successfully"),
                    Err(e) => error!("Failed to apply delta: {:?}", e),
                }
            } else {
                warn!("Room state not found for current room");
            }
        } else {
            warn!("Nickname is empty");
        }
    };
    
    rsx! {
        div { class: "field",
            label { class: "label", "Nickname" }
            div { class: if is_self() { "control has-icons-right" } else { "control" },
                input {
                    class: "input",
                    value: "{nickname}",
                    readonly: !is_self(),
                    oninput: update_nickname,
                }
                if is_self() {
                    span {
                        class: "icon is-right",
                        i {
                            class: "fa-solid fa-pencil"
                        }
                    }
                }
            }
        }
    }
}
