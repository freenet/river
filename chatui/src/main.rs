#![allow(non_snake_case)]

use dioxus::prelude::*;
use dioxus_logger::tracing::{Level, info};


fn main() {
    // Init logger
    dioxus_logger::init(Level::INFO).expect("failed to init logger");
    info!("starting app");
    launch(App);
}


#[component]
fn App() -> Element {
    // Build cool things ✌️

    rsx! {
        link { rel: "stylesheet", href: "bulma.min.css" }
        link { rel: "stylesheet", href: "main.css" }
        div { id: "links",
            a { target: "_blank", href: "https://dioxuslabs.com/learn/0.5/", "📚 Learn Dioxus" }
            a { target: "_blank", href: "https://dioxuslabs.com/awesome", "🚀 Awesome Dioxus" }
            a { target: "_blank", href: "https://github.com/dioxus-community/", "📡 Community Libraries" }
            a { target: "_blank", href: "https://github.com/DioxusLabs/dioxus-std", "⚙️ Dioxus Standard Library" }
            a { target: "_blank", href: "https://marketplace.visualstudio.com/items?itemName=DioxusLabs.dioxus", "💫 VSCode Extension" }
            a { target: "_blank", href: "https://discord.gg/XgGxMSkvUM", "👋 Community Discord" }
        }
    }
}

