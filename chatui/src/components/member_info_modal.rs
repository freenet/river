use common::state::member::MemberId;
use common::state::member_info::MemberInfo;
use dioxus::prelude::*;
use crate::components::app::{CurrentRoom, Rooms};

#[component]
pub fn MemberInfoModal(
    member_id: UseState<Option<MemberId>>,
    on_close: EventHandler<()>,
) -> Element {
    let is_active = member_id.get().is_some();
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();

    let member_info = use_memo(move || {
        member_id.get().and_then(|id| {
            current_room.read().owner_key.and_then(|owner_key| {
                rooms.read().map.get(&owner_key).and_then(|room_data| {
                    room_data.room_state.member_info.member_info.iter().find_map(|info| {
                        if info.member_info.member_id == *id {
                            Some(info.member_info.clone())
                        } else {
                            None
                        }
                    })
                })
            })
        })
    });

    let nickname = use_state(|| member_info.as_ref().map_or_else(String::new, |info| info.preferred_nickname.clone()));

    let on_save = move |_| {
        // TODO: Implement saving changes to the member info
        log::info!("Saving changes for member: {:?}", member_id.get());
        log::info!("New nickname: {}", nickname.get());
        on_close.call(());
    };

    rsx! {
        div { 
            class: "modal {if is_active { \"is-active\" } else { \"\" }}",
            div { 
                class: "modal-background", 
                onclick: move |_| on_close.call(()) 
            }
            div { 
                class: "modal-card",
                header { 
                    class: "modal-card-head",
                    p { 
                        class: "modal-card-title", 
                        "Member Information" 
                    }
                    button { 
                        class: "delete", 
                        "aria-label": "close", 
                        onclick: move |_| on_close.call(()) 
                    }
                }
                section { 
                    class: "modal-card-body",
                    div { class: "field",
                        label { class: "label", "Member ID" }
                        div { class: "control",
                            input {
                                class: "input",
                                "type": "text",
                                value: "{member_id.get().map_or_else(|| \"None\".to_string(), |id| id.to_string())}",
                                readonly: true
                            }
                        }
                    }
                    div { class: "field",
                        label { class: "label", "Nickname" }
                        div { class: "control",
                            input {
                                class: "input",
                                "type": "text",
                                value: "{nickname}",
                                oninput: move |evt| nickname.set(evt.value.clone())
                            }
                        }
                    }
                    div { class: "field",
                        label { class: "label", "Version" }
                        div { class: "control",
                            input {
                                class: "input",
                                "type": "text",
                                value: "{member_info.as_ref().map_or_else(|| \"N/A\".to_string(), |info| info.version.to_string())}",
                                readonly: true
                            }
                        }
                    }
                }
                footer { 
                    class: "modal-card-foot",
                    button { 
                        class: "button is-success", 
                        onclick: on_save,
                        "Save changes" 
                    }
                    button { 
                        class: "button", 
                        onclick: move |_| on_close.call(()), 
                        "Cancel" 
                    }
                }
            }
        }
    }
}
