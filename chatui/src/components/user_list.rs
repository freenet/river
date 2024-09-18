use dioxus::prelude::*;

#[component]
pub fn MemberList() -> Element {
    let users = vec!["Alice", "Bob", "Charlie"];

    rsx! {
        aside { class: "user-list has-background-light",
            div { class: "menu p-4", style: "height: 100%; display: flex; flex-direction: column;",
                p { class: "menu-label", "Users in Room" }
                ul { class: "menu-list", style: "flex-grow: 1; overflow-y: auto;",
                    {users.iter().map(|user| {
                        rsx! {
                            li {
                                div {
                                    class: "is-flex is-justify-content-space-between",
                                    span { "{user}" }
                                    span {
                                        class: "more-info",
                                        onclick: move |_| {
                                            // TODO: Implement user modal opening logic
                                        },
                                        i { class: "fas fa-ellipsis-h" }
                                    }
                                }
                            }
                        }
                    })}
                }
                div { class: "add-button",
                    button {
                        onclick: move |_| {
                            // TODO: Implement invite user modal opening logic
                        },
                        span { class: "icon is-small", i { class: "fas fa-user-plus" } }
                        span { "Invite User" }
                    }
                }
            }
        }
    }
}
