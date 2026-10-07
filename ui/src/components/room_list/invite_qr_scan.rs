//! QR scan for "Enter Invite Code" (freenet/river#741).
//!
//! On a top-level page, `getUserMedia` is started from the button's click,
//! before any timeout, so the permission prompt stays attached to the tap.
//! The preview element is mounted on the next render; [`capture_code`] waits
//! for it, then polls `BarcodeDetector`.
//!
//! The deployed page does not get that prompt. The Freenet shell loads River
//! in a sandboxed iframe (`allow-scripts allow-forms allow-popups`, no
//! `allow-same-origin`), so the document's origin is opaque. `getUserMedia`
//! rejects with `SecurityError` and the browser never asks. Adding
//! `allow-same-origin` would make the contract same-origin with the node, so
//! the scan takes one photo through a file input instead and reads that still
//! with the same detector. Browsers without `BarcodeDetector` (and native
//! test builds) report that scanning is unavailable. The paste box stays
//! either way.

use std::cell::Cell;
#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;
#[cfg(target_arch = "wasm32")]
use std::time::Duration;

pub(crate) const PREVIEW_ID: &str = "invite-qr-preview";
pub(crate) const STILL_INPUT_ID: &str = "invite-qr-still";

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) enum Capture {
    Code(String),
    Cancelled,
    Failed(String),
}

pub(crate) struct ScanSession {
    generation: Cell<u32>,
    #[cfg(target_arch = "wasm32")]
    stream: RefCell<Option<web_sys::MediaStream>>,
}

impl ScanSession {
    pub(crate) fn new() -> Self {
        Self {
            generation: Cell::new(0),
            #[cfg(target_arch = "wasm32")]
            stream: RefCell::new(None),
        }
    }

    /// Invalidate any scan already running and return the id the new scan
    /// must echo back. A later [`Self::stop`] makes that scan unwind.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn begin(&self) -> u32 {
        self.stop();
        self.generation.get()
    }

    pub(crate) fn stop(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        #[cfg(target_arch = "wasm32")]
        self.stop_tracks();
    }

    /// False once [`Self::stop`] or a newer [`Self::begin`] has run. A task
    /// that no longer owns the generation must not stop tracks or accept a
    /// code: those belong to the scan that replaced it.
    pub(crate) fn generation_is(&self, generation: u32) -> bool {
        self.generation.get() == generation
    }

    #[cfg(target_arch = "wasm32")]
    fn store_stream(&self, stream: web_sys::MediaStream) {
        self.stop_tracks();
        *self.stream.borrow_mut() = Some(stream);
    }

    #[cfg(target_arch = "wasm32")]
    fn stop_tracks(&self) {
        use wasm_bindgen::JsCast;
        let Some(stream) = self.stream.borrow_mut().take() else {
            return;
        };
        let tracks = stream.get_tracks();
        for index in 0..tracks.length() {
            if let Ok(track) = tracks.get(index).dyn_into::<web_sys::MediaStreamTrack>() {
                track.stop();
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn detector_available() -> bool {
    false
}

/// The shell iframe's origin is the string `"null"`. A top-level page has a
/// real origin and can show the camera prompt.
pub(crate) fn camera_prompt_unavailable() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .map(|window| window.origin() == "null")
            .unwrap_or(true)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn detector_available() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    js_sys::Reflect::get(&window, &wasm_bindgen::JsValue::from_str("BarcodeDetector"))
        .ok()
        .is_some_and(|value| value.is_function())
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn request_rear_camera() -> Result<js_sys::Promise, String> {
    use wasm_bindgen::JsValue;

    let window = web_sys::window().ok_or_else(|| "No window.".to_string())?;
    let devices = window
        .navigator()
        .media_devices()
        .map_err(js_error)
        .map_err(|_| "This browser has no camera.".to_string())?;
    let video = js_sys::Object::new();
    let facing = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &facing,
        &JsValue::from_str("ideal"),
        &JsValue::from_str("environment"),
    );
    let _ = js_sys::Reflect::set(&video, &JsValue::from_str("facingMode"), &facing);
    let constraints = web_sys::MediaStreamConstraints::new();
    constraints.set_video(&video);
    devices
        .get_user_media_with_constraints(&constraints)
        .map_err(js_error)
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn open_still_capture() -> Result<(), String> {
    let input = still_input().ok_or_else(|| "The camera control is not on screen.".to_string())?;
    // Clearing first lets a second photo of the same name fire `change`.
    input.set_value("");
    input.click();
    Ok(())
}

/// Read a QR from one photo. Used when the page cannot call `getUserMedia`.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn decode_still(file: web_sys::File) -> Result<String, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or_else(|| "No window.".to_string())?;
    let blob: &web_sys::Blob = file.unchecked_ref();
    let bitmap = JsFuture::from(
        window
            .create_image_bitmap_with_blob(blob)
            .map_err(|_| "Could not read that photo. Try again.".to_string())?,
    )
    .await
    .map_err(|_| "Could not read that photo. Try again.".to_string())?;
    let detector = barcode_detector()?;
    match detect_codes(&detector, &bitmap).await? {
        Some(text) => Ok(text),
        None => {
            Err("No QR code was found in that photo. Try again, or paste the code.".to_string())
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn still_input() -> Option<web_sys::HtmlInputElement> {
    use wasm_bindgen::JsCast;
    web_sys::window()?
        .document()?
        .get_element_by_id(STILL_INPUT_ID)?
        .dyn_into()
        .ok()
}

#[cfg(target_arch = "wasm32")]
pub(crate) async fn capture_code(
    session: Rc<ScanSession>,
    generation: u32,
    promise: js_sys::Promise,
) -> Capture {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let stream = match JsFuture::from(promise).await {
        Ok(value) => match value.dyn_into::<web_sys::MediaStream>() {
            Ok(stream) => stream,
            Err(_) => {
                return Capture::Failed("The camera did not return a video stream.".to_string())
            }
        },
        Err(err) => return cancelled_or(&session, generation, js_error(err)),
    };
    if !session.generation_is(generation) {
        stop_stream(&stream);
        return Capture::Cancelled;
    }
    session.store_stream(stream.clone());

    let video = match wait_for_preview(&session, generation).await {
        Ok(video) => video,
        Err(capture) => {
            // The stream is already stored. Leaving it here keeps the camera
            // light on after "Camera preview is not on screen." Only stop it
            // when this task still owns the session; a newer scan's tracks
            // must stay up.
            if session.generation_is(generation) {
                session.stop_tracks();
            }
            return capture;
        }
    };
    video.set_src_object(Some(&stream));
    video.set_muted(true);
    if let Ok(play) = video.play() {
        let _ = JsFuture::from(play).await;
    }
    if !session.generation_is(generation) {
        return Capture::Cancelled;
    }

    let detector = match barcode_detector() {
        Ok(detector) => detector,
        Err(message) => {
            session.stop_tracks();
            return Capture::Failed(message);
        }
    };

    // Ten seconds of a live preview with no frame, then give up. After the
    // first frame, keep polling until the user stops or a code is read.
    let mut attempts_without_frame = 0u32;
    let mut transient_errors = 0u32;
    loop {
        if !session.generation_is(generation) {
            return Capture::Cancelled;
        }
        if video.ready_state() >= 2 {
            attempts_without_frame = 0;
            match detect_once(&detector, &video).await {
                Ok(Some(text)) => {
                    if !session.generation_is(generation) {
                        return Capture::Cancelled;
                    }
                    session.stop_tracks();
                    return Capture::Code(text);
                }
                Ok(None) => transient_errors = 0,
                Err(err) if transient_detect_error(&err) && transient_errors < 15 => {
                    transient_errors += 1;
                }
                Err(err) => {
                    if session.generation_is(generation) {
                        session.stop_tracks();
                        return Capture::Failed(err);
                    }
                    return Capture::Cancelled;
                }
            }
        } else {
            attempts_without_frame += 1;
            if attempts_without_frame > 50 {
                if session.generation_is(generation) {
                    session.stop_tracks();
                    return Capture::Failed("The camera preview did not start.".to_string());
                }
                return Capture::Cancelled;
            }
        }
        futures_timer::Delay::new(Duration::from_millis(200)).await;
    }
}

#[cfg(target_arch = "wasm32")]
fn cancelled_or(session: &ScanSession, generation: u32, message: String) -> Capture {
    if session.generation_is(generation) {
        Capture::Failed(message)
    } else {
        Capture::Cancelled
    }
}

#[cfg(target_arch = "wasm32")]
fn stop_stream(stream: &web_sys::MediaStream) {
    use wasm_bindgen::JsCast;
    let tracks = stream.get_tracks();
    for index in 0..tracks.length() {
        if let Ok(track) = tracks.get(index).dyn_into::<web_sys::MediaStreamTrack>() {
            track.stop();
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn wait_for_preview(
    session: &ScanSession,
    generation: u32,
) -> Result<web_sys::HtmlVideoElement, Capture> {
    for _ in 0..30 {
        if !session.generation_is(generation) {
            return Err(Capture::Cancelled);
        }
        if let Some(video) = preview_element() {
            return Ok(video);
        }
        futures_timer::Delay::new(Duration::from_millis(100)).await;
    }
    Err(Capture::Failed(
        "Camera preview is not on screen.".to_string(),
    ))
}

#[cfg(target_arch = "wasm32")]
fn preview_element() -> Option<web_sys::HtmlVideoElement> {
    use wasm_bindgen::JsCast;
    let document = web_sys::window()?.document()?;
    document
        .get_element_by_id(PREVIEW_ID)?
        .dyn_into::<web_sys::HtmlVideoElement>()
        .ok()
}

#[cfg(target_arch = "wasm32")]
fn barcode_detector() -> Result<BarcodeDetector, String> {
    use wasm_bindgen::JsValue;
    let formats = js_sys::Array::of1(&JsValue::from_str("qr_code"));
    let options = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&options, &JsValue::from_str("formats"), &formats);
    BarcodeDetector::new(&options)
        .map_err(|_| "This browser cannot scan QR codes. Paste the code instead.".to_string())
}

#[cfg(target_arch = "wasm32")]
async fn detect_once(
    detector: &BarcodeDetector,
    video: &web_sys::HtmlVideoElement,
) -> Result<Option<String>, String> {
    detect_codes(detector, video.as_ref()).await
}

#[cfg(target_arch = "wasm32")]
async fn detect_codes(
    detector: &BarcodeDetector,
    source: &wasm_bindgen::JsValue,
) -> Result<Option<String>, String> {
    use wasm_bindgen::JsValue;
    use wasm_bindgen_futures::JsFuture;

    let promise = detector.detect(source).map_err(js_error)?;
    let value = JsFuture::from(promise).await.map_err(js_error)?;
    let list = js_sys::Array::from(&value);
    for index in 0..list.length() {
        let item = list.get(index);
        let raw = js_sys::Reflect::get(&item, &JsValue::from_str("rawValue")).map_err(js_error)?;
        if let Some(text) = raw.as_string() {
            if !text.trim().is_empty() {
                return Ok(Some(text));
            }
        }
    }
    Ok(None)
}

#[cfg(target_arch = "wasm32")]
fn transient_detect_error(message: &str) -> bool {
    message.contains("InvalidStateError") || message.contains("NotReadableError")
}

#[cfg(target_arch = "wasm32")]
fn js_error(err: wasm_bindgen::JsValue) -> String {
    use wasm_bindgen::JsValue;
    if let Some(text) = err.as_string() {
        return text;
    }
    let name = js_sys::Reflect::get(&err, &JsValue::from_str("name"))
        .ok()
        .and_then(|value| value.as_string());
    let message = js_sys::Reflect::get(&err, &JsValue::from_str("message"))
        .ok()
        .and_then(|value| value.as_string());
    match name.as_deref() {
        Some("NotAllowedError") => {
            "Camera permission was denied. Paste the code, or allow the camera and try again."
                .to_string()
        }
        Some("NotFoundError") => "No camera was found on this device.".to_string(),
        Some("NotReadableError") => "The camera is in use by another app.".to_string(),
        Some("SecurityError") => "The camera is blocked for this page.".to_string(),
        Some(name) => match message {
            Some(message) if !message.is_empty() => format!("{name}: {message}"),
            _ => name.to_string(),
        },
        None => message.unwrap_or_else(|| "Could not open the camera.".to_string()),
    }
}

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
extern "C" {
    type BarcodeDetector;

    #[wasm_bindgen(constructor, catch)]
    fn new(options: &JsValue) -> Result<BarcodeDetector, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn detect(this: &BarcodeDetector, source: &JsValue) -> Result<js_sys::Promise, JsValue>;
}
