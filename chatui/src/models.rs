use std::collections::HashMap;
use dioxus::prelude::Signal;
use common::ChatRoomStateV1;

#[derive(Clone, Debug)]
#[derive(Default)]
pub struct ChatState {
    pub rooms: HashMap<String, Signal<ChatRoomStateV1>>,
    pub current_room: String,
}

