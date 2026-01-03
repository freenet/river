//! Tab detection module using BroadcastChannel API
//!
//! Prevents multiple tabs from editing simultaneously to avoid delegate state conflicts.
//! Uses a leader election pattern where the first tab becomes the "primary" tab.

use dioxus::logger::tracing::{info, warn};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BroadcastChannel, MessageEvent};

const CHANNEL_NAME: &str = "river-tab-sync";
const LEADER_CLAIM: &str = "claim-leader";
const LEADER_EXISTS: &str = "leader-exists";
const LEADER_ABDICATE: &str = "leader-abdicate";

// Global channel reference for sending messages
thread_local! {
    static CHANNEL: RefCell<Option<BroadcastChannel>> = const { RefCell::new(None) };
}

/// Result of tab detection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabRole {
    /// This is the primary (first) tab - can edit freely
    Primary,
    /// This is a secondary tab - should be read-only
    Secondary,
}

/// Force this tab to become the primary tab
/// Broadcasts an abdication message to tell other tabs to step down
pub fn force_become_primary() {
    CHANNEL.with(|cell| {
        if let Some(channel) = cell.borrow().as_ref() {
            info!("Forcing this tab to become primary - broadcasting abdication request");
            let _ = channel.post_message(&JsValue::from_str(LEADER_ABDICATE));
        }
    });
}


/// Callback type for role change notifications
type RoleChangeCallback = Rc<RefCell<Option<Box<dyn Fn(TabRole)>>>>;

// Global callback for role changes
thread_local! {
    static ROLE_CHANGE_CALLBACK: RoleChangeCallback = Rc::new(RefCell::new(None));
}

/// Set a callback to be notified when this tab's role changes
pub fn set_role_change_callback<F: Fn(TabRole) + 'static>(callback: F) {
    ROLE_CHANGE_CALLBACK.with(|cb| {
        *cb.borrow_mut() = Some(Box::new(callback));
    });
}

fn notify_role_change(role: TabRole) {
    ROLE_CHANGE_CALLBACK.with(|cb| {
        if let Some(callback) = cb.borrow().as_ref() {
            callback(role);
        }
    });
}

/// Async version that properly waits for leader response
pub async fn check_for_existing_tabs() -> TabRole {
    use js_sys::Promise;
    use wasm_bindgen_futures::JsFuture;

    let window = match web_sys::window() {
        Some(w) => w,
        None => {
            warn!("No window object available, assuming primary tab");
            return TabRole::Primary;
        }
    };

    let channel = match BroadcastChannel::new(CHANNEL_NAME) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to create BroadcastChannel: {:?}, assuming primary tab", e);
            return TabRole::Primary;
        }
    };

    // Store channel globally for later use (e.g., force_become_primary)
    CHANNEL.with(|cell| {
        *cell.borrow_mut() = Some(channel.clone());
    });

    // Create a promise that resolves when we get a response or timeout
    let received_response = Rc::new(std::cell::Cell::new(false));
    let received_response_clone = received_response.clone();

    let channel_clone = channel.clone();

    // Set up message handler for initial check
    let onmessage = Closure::<dyn Fn(MessageEvent)>::new(move |event: MessageEvent| {
        if let Some(data) = event.data().as_string() {
            match data.as_str() {
                LEADER_EXISTS => {
                    received_response_clone.set(true);
                }
                LEADER_CLAIM => {
                    // Another tab is also starting up - respond that we exist
                    let _ = channel_clone.post_message(&JsValue::from_str(LEADER_EXISTS));
                }
                _ => {}
            }
        }
    });

    channel.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

    // Send our claim
    if let Err(e) = channel.post_message(&JsValue::from_str(LEADER_CLAIM)) {
        warn!("Failed to post leader claim: {:?}", e);
        return TabRole::Primary;
    }

    // Wait a short time for response (100ms should be plenty for local broadcast)
    let promise = Promise::new(&mut |resolve, _reject| {
        let received = received_response.clone();
        let cb = Closure::once(Box::new(move || {
            resolve
                .call1(&JsValue::NULL, &JsValue::from_bool(received.get()))
                .unwrap();
        }) as Box<dyn FnOnce()>);

        window
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                100, // 100ms timeout
            )
            .unwrap();
        cb.forget();
    });

    let result = JsFuture::from(promise).await;

    let is_secondary = matches!(result, Ok(val) if val.as_bool().unwrap_or(false));

    // Set up permanent handler for ongoing communication
    let channel_for_permanent = channel.clone();
    let is_primary = Rc::new(std::cell::Cell::new(!is_secondary));
    let is_primary_clone = is_primary.clone();

    let permanent_handler = Closure::<dyn Fn(MessageEvent)>::new(move |event: MessageEvent| {
        if let Some(data) = event.data().as_string() {
            match data.as_str() {
                LEADER_CLAIM => {
                    // Only respond if we're currently primary
                    if is_primary_clone.get() {
                        info!("Received leader claim from new tab, responding");
                        let _ = channel_for_permanent.post_message(&JsValue::from_str(LEADER_EXISTS));
                    }
                }
                LEADER_ABDICATE => {
                    // Another tab wants to take over - we should become secondary
                    if is_primary_clone.get() {
                        info!("Received abdication request - becoming secondary");
                        is_primary_clone.set(false);
                        notify_role_change(TabRole::Secondary);
                    }
                }
                _ => {}
            }
        }
    });

    channel.set_onmessage(Some(permanent_handler.as_ref().unchecked_ref()));
    permanent_handler.forget();
    onmessage.forget();

    if is_secondary {
        info!("Another River tab detected - this tab will be read-only");
        TabRole::Secondary
    } else {
        info!("No other River tabs detected - this is the primary tab");
        TabRole::Primary
    }
}
