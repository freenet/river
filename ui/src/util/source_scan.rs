use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, recursively.
pub(crate) fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("readable source dir") {
        let path = entry.expect("readable dir entry").path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}

/// Cut production source at the test module so a needle appearing only in a
/// test cannot satisfy the pin. Splits on `mod tests`, not
/// `#[cfg(test)]`, because attributes also decorate non-test items.
pub(crate) fn production_only(src: &str) -> &str {
    match src.find("\nmod tests") {
        Some(i) => &src[..i],
        None => src,
    }
}
