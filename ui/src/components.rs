pub mod app;
pub mod conversation;
pub mod direct_messages;
pub mod invite_click_interceptor;
pub mod members;
pub mod mention_click_interceptor;
pub mod room_list;

/// Tree-wide audit for freenet/river#564: every EDITABLE form control that
/// binds `value:` must also handle `oninput`.
///
/// ## Why this is a whole-class rule, not a per-field style preference
///
/// `value` on `input` / `textarea` / `select` is declared **volatile** in
/// dioxus-html (`elements.rs:1488,1571,1594`). dioxus-core writes volatile
/// attributes to the DOM on EVERY re-render, even when the rendered string is
/// unchanged:
///
/// ```text
/// // dioxus-core-0.7.9/src/diff/node.rs:463
/// if volatile || attribute_changed { self.write_attribute(...) }
/// ```
///
/// and the interpreter then assigns the value whenever the LIVE DOM value
/// differs from the VDOM value:
///
/// ```text
/// // dioxus-interpreter-js-0.7.9/src/ts/set_attribute.ts:31-33
/// case "value": ... else if (node.value !== value) node.value = value;
/// ```
///
/// A field whose signal is written only in `onchange` (which fires on
/// blur/commit, not per keystroke) therefore holds the PRE-TYPING text for as
/// long as the user is typing. Any re-render in that window sees
/// `node.value !== value` and resets the control, silently discarding the
/// in-progress edit. Nothing about the field looks wrong at the call site,
/// and no amount of local reasoning about that component reveals it: the
/// trigger is whatever unrelated signal the component happens to read.
///
/// That is how #564 shipped. `RoomDescriptionField` reads `CURRENT_ROOM` and
/// `ROOMS` in its render body, so it re-rendered on every room-state write and
/// wiped the owner's half-typed description every few seconds.
///
/// ## What is exempt
///
/// Display-only controls (invite link, invite code, room public key, contract
/// ID, member ID, export token) legitimately bind `value:` with no `oninput`,
/// because nothing types into them. They are recognised by a literal
/// `readonly: true` / `readonly: "true"` on the element itself. A CONDITIONAL
/// readonly (`readonly: !is_owner`) is NOT an exemption: it is editable for
/// somebody, and that somebody is exactly who loses their typing.
#[cfg(test)]
mod volatile_value_binding_audit {
    use std::path::{Path, PathBuf};

    /// Elements whose `value` attribute dioxus marks volatile.
    const VOLATILE_VALUE_ELEMENTS: [&str; 3] = ["input", "textarea", "select"];

    fn ui_src_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("ui/src must be readable") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Production source only.
    ///
    /// Test modules are cut away, both so this audit cannot flag rsx written
    /// inside a test and so it cannot flag ITSELF: this very module contains
    /// the string `input {` in its fixtures, and a scanner that matched its
    /// own needle would be a permanently-green pin
    /// (`feedback_source_pin_needle_matches_itself`).
    fn production_source(path: &Path) -> String {
        let src = std::fs::read_to_string(path).expect("source file must be readable");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        strip_line_comments(&src[..cut])
    }

    /// Drop `//` comments so prose mentioning `input {` cannot trip the scan.
    /// Deliberately naive about `//` inside string literals: over-stripping can
    /// only cause a MISSED violation in a line that also opens an element,
    /// which does not occur in this tree, and never a false positive.
    fn strip_line_comments(src: &str) -> String {
        src.lines()
            .map(|line| match line.find("//") {
                Some(idx) => &line[..idx],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The `{ ... }` block opened at `open_idx`, brace-matched.
    fn balanced_block(src: &str, open_idx: usize) -> &str {
        let bytes = src.as_bytes();
        let mut depth = 0usize;
        let mut i = open_idx;
        while i < bytes.len() {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &src[open_idx..=i];
                    }
                }
                _ => {}
            }
            i += 1;
        }
        &src[open_idx..]
    }

    /// Attribute lines belonging to the element ITSELF (brace depth 1), so a
    /// nested child element's handlers can never be mistaken for the parent's.
    fn own_attribute_lines(block: &str) -> Vec<&str> {
        let mut depth = 0usize;
        let mut lines = Vec::new();
        for line in block.lines() {
            let starts_at_depth_one = depth == 1;
            for ch in line.chars() {
                match ch {
                    '{' => depth += 1,
                    '}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
            if starts_at_depth_one {
                lines.push(line);
            }
        }
        lines
    }

    fn has_attribute(attrs: &[&str], name: &str) -> bool {
        let needle = format!("{name}:");
        attrs.iter().any(|l| l.trim_start().starts_with(&needle))
    }

    fn is_display_only(attrs: &[&str]) -> bool {
        attrs.iter().any(|l| {
            let t = l.trim_start();
            t.starts_with("readonly: true") || t.starts_with("readonly: \"true\"")
        })
    }

    /// Locate rsx element openings: an element name at a token boundary,
    /// followed by `{`.
    fn element_openings(src: &str, element: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut from = 0usize;
        while let Some(rel) = src[from..].find(element) {
            let start = from + rel;
            from = start + element.len();

            let preceded_by_ident = src[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == ':');
            if preceded_by_ident {
                continue;
            }
            let rest = &src[from..];
            let trimmed = rest.trim_start();
            if !trimmed.starts_with('{') {
                continue;
            }
            // Only same-line `{`, which is how every rsx element in this tree
            // is written; requiring it keeps `let input = ...\n{` out.
            if rest[..rest.len() - trimmed.len()].contains('\n') {
                continue;
            }
            out.push(from + (rest.len() - trimmed.len()));
        }
        out
    }

    #[test]
    fn every_editable_value_binding_tracks_input() {
        let src_dir = ui_src_dir();
        let mut files = Vec::new();
        rust_sources(&src_dir, &mut files);
        assert!(
            files.len() > 20,
            "the audit must actually walk the ui/src tree; found only {} files",
            files.len()
        );

        let mut violations = Vec::new();
        let mut editable_checked = 0usize;
        let mut display_only = 0usize;

        for path in &files {
            let src = production_source(path);
            for element in VOLATILE_VALUE_ELEMENTS {
                for open_idx in element_openings(&src, element) {
                    let block = balanced_block(&src, open_idx);
                    let attrs = own_attribute_lines(block);
                    if !has_attribute(&attrs, "value") {
                        continue;
                    }
                    if is_display_only(&attrs) {
                        display_only += 1;
                        continue;
                    }
                    editable_checked += 1;
                    if !has_attribute(&attrs, "oninput") {
                        let line = src[..open_idx].matches('\n').count() + 1;
                        let rel = path.strip_prefix(&src_dir).unwrap_or(path);
                        violations.push(format!("ui/src/{}:{line} <{element}>", rel.display()));
                    }
                }
            }
        }

        // Anti-vacuity: the scanner must be finding real bindings of both
        // kinds. A refactor that renames attributes or reformats rsx could
        // otherwise leave this test green while inspecting nothing.
        assert!(
            editable_checked >= 8,
            "expected the audit to inspect the known editable value-bound \
             controls; only found {editable_checked}. The scanner is probably \
             no longer matching this tree's rsx"
        );
        assert!(
            display_only >= 4,
            "expected to find the readonly display fields (invite link, invite \
             code, public key, contract id, ...); only found {display_only}"
        );

        assert!(
            violations.is_empty(),
            "freenet/river#564: these editable controls bind `value:` with no \
             `oninput:`, so any unrelated re-render will re-write the volatile \
             `value` attribute and wipe whatever the user has typed but not yet \
             committed:\n  {}\n\nAdd `oninput` so the bound signal tracks the \
             live value (making the re-write a no-op). `onchange` alone is NOT \
             enough: it fires on blur, long after the damage. If the control is \
             genuinely display-only, mark it `readonly: true`.",
            violations.join("\n  ")
        );
    }

    /// The audit is worthless if it cannot see the bug it was written for, so
    /// run the scanner over the pre-fix shape of the field from #564 and
    /// require a hit. This is the mutation the real fix removed.
    #[test]
    fn audit_detects_the_original_regression() {
        let pre_fix = r#"
            rsx! {
                div { class: "mb-4",
                    label { class: "block", "Room Description" }
                    textarea {
                        class: "w-full",
                        rows: "3",
                        value: "{description}",
                        readonly: !is_owner,
                        disabled: !is_owner,
                        onchange: update_description,
                    }
                }
            }
        "#;
        let openings = element_openings(pre_fix, "textarea");
        assert_eq!(openings.len(), 1, "scanner must locate the textarea");

        let attrs = own_attribute_lines(balanced_block(pre_fix, openings[0]));
        assert!(has_attribute(&attrs, "value"), "must see the value binding");
        assert!(
            !is_display_only(&attrs),
            "`readonly: !is_owner` must NOT count as display-only: the field is \
             editable for the owner, who is exactly who loses their typing"
        );
        assert!(
            !has_attribute(&attrs, "oninput"),
            "the pre-fix field had no oninput, so the audit must flag it"
        );

        // ...and the fixed shape must pass.
        let fixed = pre_fix.replace(
            "onchange: update_description,",
            "oninput: move |evt| description.set(evt.value().to_string()),\n\
             onchange: update_description,",
        );
        let fixed_attrs = own_attribute_lines(balanced_block(
            &fixed,
            element_openings(&fixed, "textarea")[0],
        ));
        assert!(
            has_attribute(&fixed_attrs, "oninput"),
            "the fixed field must satisfy the audit"
        );
    }

    /// A nested child's `oninput` must not satisfy the parent's requirement.
    #[test]
    fn nested_child_handler_does_not_satisfy_parent() {
        let nested = r#"
            select {
                value: "{choice}",
                option {
                    oninput: move |_| {},
                }
            }
        "#;
        let attrs = own_attribute_lines(balanced_block(
            nested,
            element_openings(nested, "select")[0],
        ));
        assert!(has_attribute(&attrs, "value"));
        assert!(
            !has_attribute(&attrs, "oninput"),
            "depth-1 scoping must ignore the nested option's handler"
        );
    }
}
