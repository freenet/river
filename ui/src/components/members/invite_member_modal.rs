use crate::components::members::Invitation;
use crate::room_data::{CurrentRoom, RoomData, Rooms};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_common::crypto_values::CryptoValue;
use river_common::room_state::member::{AuthorizedMember, Member, MembersDelta};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};

#[component]
pub fn InviteMemberModal(is_active: Signal<bool>) -> Element {
    let mut rooms_signal = use_context::<Signal<Rooms>>();
    let current_room_signal = use_context::<Signal<CurrentRoom>>();
    let current_room_data_signal: Memo<Option<RoomData>> = use_memo(move || {
        let rooms = rooms_signal.read();
        let current_room = current_room_signal.read();
        current_room
            .owner_key
            .as_ref()
            .and_then(|key| rooms.map.get(key).cloned())
    });
    let mut invitation : Signal<InvitationStatus> = use_signal(|| InvitationStatus::ModalClosed);


    rsx! {
        div {
            class: if *is_active.read() { "modal is-active" } else { "modal" },
            div {
                class: "modal-background",
                onclick: move |_| {
                    is_active.set(false);
                }
            }
            button {
                class: "modal-close is-large",
                onclick: move |_| is_active.set(false)
            }
        }
    }
}

enum InvitationStatus {
    ModalClosed,
    Generating,
    Generated(Invitation),
}