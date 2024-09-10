#![allow(non_snake_case)]

use dioxus::prelude::*;
use dioxus_web;
use dioxus_logger::tracing::{Level, info};

fn main() {
    // Init logger
    dioxus_logger::init(Level::INFO).expect("failed to init logger");
    info!("starting app");
    dioxus_web::launch(App);
}

#[component(inline_props)]
fn App() -> Element {
    let rooms = use_signal(|| vec!["General", "Random", "Tech"]);
    let current_room = use_signal(|| "General".to_string());

    rsx! {
        link { rel: "stylesheet", href: "css/bulma.min.css" }
        link { rel: "stylesheet", href: "css/main.css" }
        div { class: "columns is-gapless",
            // Rooms list
            div { class: "column is-one-quarter",
                div { class: "menu",
                    p { class: "menu-label", "Rooms" }
                    ul { class: "menu-list",
                        rooms.iter().map(|room| {
                            rsx! {
                                li {
                                    a {
                                        class: if current_room.get() == *room { "is-active" } else { "" },
                                        onclick: move |_| current_room.set(room.to_string()),
                                        "{room}"
                                    }
                                }
                            }
                        }),
                    }
                    button { class: "button is-fullwidth", "Add Room" }
                }
            }
            // Chat area
            div { class: "column",
                div { class: "box", style: "height: 80vh; display: flex; flex-direction: column;",
                    // Chat history
                    div { class: "content", style: "flex-grow: 1; overflow-y: auto;",
                        h4 { "Chat History for {current_room.get()}" }
                        // Here you would render actual chat messages
                    }
                    // Message input
                    div { class: "field has-addons",
                        div { class: "control is-expanded",
                            input { class: "input", type: "text", placeholder: "Type a message..." }
                        }
                        div { class: "control",
                            button { class: "button is-info", "Send" }
                        }
                    }
                }
            }
        }
    }
}

