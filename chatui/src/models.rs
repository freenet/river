use std::collections::HashMap;
use dioxus::prelude::Signal;
use common::ChatRoomStateV1;
use ed25519_dalek::VerifyingKey;

#[derive(Clone, Debug)]
#[derive(Default)]
pub struct ChatState {
    pub rooms: HashMap<VerifyingKey, Signal<ChatRoomStateV1>>,
    pub current_room: Option<VerifyingKey>,
}

