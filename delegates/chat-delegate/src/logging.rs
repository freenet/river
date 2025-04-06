#[cfg(target_arch = "wasm32")]
pub fn info(msg: &str) {
    freenet_stdlib::log::info(msg);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn info(msg: &str) {
    println!("[INFO] {}", msg);
}
