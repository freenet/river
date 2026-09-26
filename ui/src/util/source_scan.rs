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

/// Production source with comments stripped, cut at the test module so a
/// needle appearing only in a test cannot satisfy the pin. Splits on
/// `mod tests`, not `#[cfg(test)]`, because attributes also decorate non-test
/// items. A `mod tests` line inside a comment or string is not the test module,
/// so it cannot end the scan early, and stripping comments means a
/// commented-out block cannot count as production code (freenet/river#716).
pub(crate) fn production_only(src: &str) -> String {
    let kinds = crate::util::lex(src);
    let end = src
        .match_indices("\nmod tests")
        .find(|&(i, _)| kinds[i + 1] == crate::util::Lexeme::Code)
        .map_or(src.len(), |(i, _)| i);
    crate::util::strip_comments(&src[..end])
}

#[cfg(test)]
mod tests {
    use super::production_only;

    /// freenet/river#716: only a real `mod tests` item ends production code.
    #[test]
    fn production_only_ignores_mod_tests_in_comments_and_strings() {
        let src = "fn a() {}\n/*\nmod tests {\n    fn old() {}\n}\n*/\nfn b() {}\nconst S: &str = \"\nmod tests\";\nfn c() {}\nmod tests {\n    fn d() {}\n}\n";
        let prod = production_only(src);
        assert!(prod.ends_with("fn c() {}"), "{prod}");
        assert!(!prod.contains("fn d()"), "{prod}");
        assert!(
            !prod.contains("fn old()"),
            "commented-out code is not production: {prod}"
        );
        assert_eq!(production_only("fn a() {}\n"), "fn a() {}\n");
    }
}
