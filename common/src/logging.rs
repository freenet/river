use once_cell::sync::Lazy;
use std::sync::Mutex;

pub static LOG: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::logging::LOG.lock().unwrap().push(format!($($arg)*));
    };
}

pub fn clear_log() {
    LOG.lock().unwrap().clear();
}

pub fn get_log() -> String {
    LOG.lock().unwrap().join("\n")
}
