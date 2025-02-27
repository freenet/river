mod ecies;

use std::time::*;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(inline_js = "
export function get_current_time() {
    return Date.now();
}
")]
extern "C" {
    fn get_current_time() -> f64;
}

pub fn get_current_system_time() -> SystemTime {
    #[cfg(target_arch = "wasm32")]
    {
        // Convert milliseconds since epoch to a Duration
        let millis = get_current_time();
        let duration_since_epoch = Duration::from_millis(millis as u64);
        UNIX_EPOCH + duration_since_epoch
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        SystemTime::now()
    }
}

// Helper function to create a Duration from seconds
pub fn seconds(s: u64) -> Duration {
    Duration::from_secs(s)
}

// Helper function to create a Duration from milliseconds
pub fn millis(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// A WASM-compatible sleep function that works in both browser and native environments
pub async fn sleep(duration: Duration) {
    #[cfg(target_arch = "wasm32")]
    {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            let window = web_sys::window().unwrap();
            window.set_timeout_with_callback_and_timeout_and_arguments_0(
                &resolve,
                duration.as_millis() as i32,
            ).unwrap();
        });
        wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::sleep(duration);
    }
}

#[cfg(feature = "example-data")]
mod name_gen;
#[cfg(feature = "example-data")]
pub use name_gen::random_full_name;

pub fn to_cbor_vec<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut buffer = Vec::new();
    ciborium::ser::into_writer(value, &mut buffer).unwrap();
    buffer
}
