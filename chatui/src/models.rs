use std::collections::HashMap;
use dioxus::prelude::{Signal, Readable, SyncStorage};
use std::cell::BorrowError;
use common::ChatRoomStateV1;
use ed25519_dalek::VerifyingKey;
use crate::example_data;

#[derive(Clone, Debug, Default)]
pub struct ChatState {
    pub rooms: HashMap<VerifyingKey, Signal<ChatRoomStateV1>>,
    pub current_room: Option<VerifyingKey>,
}

impl ChatState {
    pub fn new() -> Self {
        let mut state = Self::default();
        let (owner_vk, room_state) = example_data::create_example_room();
        state.rooms.insert(owner_vk, Signal::new(room_state));
        state.current_room = Some(owner_vk);
        state
    }
}

impl Readable for ChatState {
    type Target = Self;
    type Storage = SyncStorage;

    fn read(&self) -> &Self::Target {
        self
    }

    fn try_read_unchecked(&self) -> Result<<Self::Storage as dioxus::prelude::AnyStorage>::Ref<'static, Self::Target>, BorrowError> {
        Ok(self)
    }

    fn peek_unchecked(&self) -> <Self::Storage as dioxus::prelude::AnyStorage>::Ref<'static, Self::Target> {
        self
    }
}

