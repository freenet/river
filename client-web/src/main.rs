use dioxus::prelude::*;

fn main() {
    dioxus_web::launch(app);
}

fn app(cx: Scope) -> Element {
    cx.render(rsx! {
        div {
            h1 { "Hello, Freenet Chat!" }
        }
    })
}
