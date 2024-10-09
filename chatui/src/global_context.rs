use std::collections::HashMap;
use common::room_state::member::MemberId;
use dioxus::prelude::*;

#[derive(Clone)]
pub struct UserInfoModals {
    pub modals: HashMap<MemberId, Signal<bool>>
}

impl PartialEq for UserInfoModals {
    fn eq(&self, other: &Self) -> bool {
        self.modals.len() == other.modals.len() && 
        self.modals.iter().all(|(k, v)| other.modals.get(k).map_or(false, |ov| *v.read() == *ov.read()))
    }
}
