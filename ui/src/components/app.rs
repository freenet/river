use super::{chat_rooms::ChatRooms, main_chat::MainChat, members::MemberList};
use crate::components::chat_rooms::edit_room_modal::EditRoomModal;
use crate::example_data::create_example_rooms;
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use common::room_state::member::MemberId;
use crate::components::members::member_info_modal::MemberInfoModal;
use crate::room_data::{CurrentRoom};

pub fn App() -> Element {
    use_context_provider(|| Signal::new(create_example_rooms()));
    use_context_provider(|| Signal::new(CurrentRoom { owner_key: None }));
    use_context_provider(|| Signal::new(MemberInfoModalSignal { member: None }));
    use_context_provider(|| Signal::new(EditRoomModalSignal { room: None }));
    
    rsx! {
        div { class: "chat-container",
            ChatRooms {}
            MainChat {}
            MemberList {}
        }
        EditRoomModal {}
        MemberInfoModal {}

    }
}

pub struct EditRoomModalSignal {
    pub room : Option<VerifyingKey>,
}

pub struct MemberInfoModalSignal {
    pub member: Option<MemberId>
}
