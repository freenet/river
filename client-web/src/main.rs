use dioxus::prelude::*;

fn main() {
    dioxus_web::launch(app);
}

fn app() -> Element {
    rsx! {
        div {
            h1 { "Hello, Freenet Chat!" }
        }
    }
}
