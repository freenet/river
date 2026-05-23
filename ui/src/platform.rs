//! Thin platform-abstraction layer.
//!
//! River's UI was originally written for the web renderer, where the browser
//! `Window` is always available via `web_sys::window()`. On the mobile
//! (Android/iOS) renderer the Rust code runs natively inside a webview host,
//! and `web_sys`'s imported JS functions are NOT callable — invoking one
//! aborts the process with "function not implemented on non-wasm32 targets".
//!
//! Almost every browser-API call site in this crate is already written
//! defensively as `if let Some(window) = web_sys::window() { ... }`. Routing
//! those reads through [`window`] below — which simply returns `None` off
//! wasm — lets all of that code compile AND degrade gracefully on Android:
//! the browser-specific branch is skipped instead of crashing.
//!
//! Genuinely browser-only features (clipboard, notifications, the Freenet
//! WebSocket sync, document-title bridge) are additionally `#[cfg]`-gated at
//! their call sites; this module only covers the ubiquitous `window()` probe.

/// The browser `Window`, or `None` when not running under the web renderer.
///
/// On wasm this is exactly `web_sys::window()`. On every other target it is
/// `None`, so callers fall through their non-browser branch.
#[cfg(target_arch = "wasm32")]
#[inline]
pub fn window() -> Option<web_sys::Window> {
    web_sys::window()
}

/// Native builds have no browser `Window`. Returning `None` (rather than
/// calling the panicking `web_sys` shim) is what keeps the app alive on
/// Android.
#[cfg(not(target_arch = "wasm32"))]
#[inline]
pub fn window() -> Option<web_sys::Window> {
    None
}

/// Spawn a fire-and-forget async task that runs on whatever scheduler is
/// appropriate for the current target.
///
/// On wasm this is `wasm_bindgen_futures::spawn_local` (single-threaded
/// browser scheduler, no `Send` requirement). On native it's
/// `tokio::spawn` (multi-threaded, requires `Send`). The synchronizer
/// futures in River are written to be portable: they own their captured
/// values rather than borrowing across `.await`, so they satisfy `Send`
/// when the closure's captures themselves are `Send` (signing keys,
/// strings, owned protocol messages — all `Send`).
///
/// Prefer this over either platform-specific spawn directly so the
/// sync layer compiles for both the web and native (Android/desktop)
/// targets.
#[cfg(target_arch = "wasm32")]
pub fn spawn_local<F: std::future::Future<Output = ()> + 'static>(future: F) {
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_local<F: std::future::Future<Output = ()> + 'static>(future: F) {
    // Use Dioxus's own task spawner rather than `tokio::spawn`. The
    // synchronizer's futures capture `!Send` types (Dioxus signal write
    // guards, `Rc` references, JS-event closures), so a multi-threaded
    // spawn rejects them at compile time. `dioxus::prelude::spawn` is
    // single-threaded conceptually on every platform and requires only
    // `'static`. It needs a Dioxus runtime to be active — the entry
    // point (the spawn from `App()`) provides that, and tasks spawned
    // from inside other Dioxus-spawned tasks inherit the runtime.
    let _ = dioxus::prelude::spawn(future);
}
