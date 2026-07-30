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
/// `value` on `input` / `textarea` / `select` (and `selected` on `option`) is
/// declared **volatile** in dioxus-html (`elements.rs:1488,1536,1571,1594`).
/// dioxus-core writes volatile attributes to the DOM on EVERY re-render, even
/// when the rendered string is unchanged:
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
/// wiped the owner's half-typed description.
///
/// ## What this audit does and does NOT prove
///
/// It proves an `oninput` handler is PRESENT and is not a no-op closure. It
/// does **not** prove the handler writes the same signal the `value:` binding
/// reads. That dataflow check was tried and rejected: it false-positives on
/// two legitimate existing handlers, `nickname_field.rs` (which passes a named
/// handler, `oninput: on_input`) and `invite_via_dm_picker_modal.rs` (whose
/// `value:` binds a local derived from the signal the handler writes). A
/// reviewer must still confirm the handler updates the bound value.
///
/// It also does not cover the OTHER route to the same symptom: if a parent
/// unmounts the field (e.g. `EditRoomModal` rendering `rsx!{}` when its
/// `ROOMS.try_read()` memo is contended), the remount re-seeds `use_signal`
/// and the draft is lost regardless of `oninput`. That is the #499/#555
/// contention family, tracked separately.
///
/// ## What is exempt
///
/// Display-only controls (invite link, invite code, room public key, contract
/// ID, member ID, export token) legitimately bind `value:` with no `oninput`,
/// because nothing types into them. They are recognised by a literal
/// `readonly: true` / `readonly: "true"` on the element itself. A CONDITIONAL
/// readonly (`readonly: !is_owner`) is NOT an exemption: it is editable for
/// somebody, and that somebody is exactly who loses their typing.
///
/// `type="checkbox"` / `type="radio"` are also exempt. dioxus reports
/// `evt.value()` as `"true"`/`"false"` for a checkbox
/// (`dioxus-web-0.7.9/src/events/form.rs`), which is never equal to the DOM's
/// `node.value` (`"on"`), so wiring `oninput` into a `value:` binding there
/// would CAUSE a rewrite every render rather than prevent one. Bind `checked:`
/// instead, which dioxus does not mark volatile.
#[cfg(test)]
mod volatile_value_binding_audit {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// Elements whose `value` (or `option`'s `selected`) dioxus marks volatile.
    const VOLATILE_ELEMENTS: [&str; 4] = ["input", "textarea", "select", "option"];

    /// Every editable volatile binding in `ui/src`, as
    /// `path <element> value-expression`.
    ///
    /// This is pinned as an EXACT SET, not a count with slack. A loose
    /// threshold is how a scanner rots silently: if a future edit makes a file
    /// (or a whole region of one) invisible to the scan, its entries simply
    /// disappear and a `>=` floor keeps passing with the fields it was written
    /// to protect no longer covered. An exact set turns that into a loud diff.
    ///
    /// Adding a form control to the UI is expected to fail this test. Add the
    /// line, having first confirmed the control handles `oninput`.
    const EXPECTED_EDITABLE: [&str; 14] = [
        r#"components/conversation.rs <textarea> "{edit_text}""#,
        r#"components/conversation/message_input.rs <textarea> "{message_text}""#,
        r#"components/direct_messages/dm_thread_modal.rs <textarea> "{draft.read()}""#,
        r#"components/direct_messages/invite_via_dm_picker_modal.rs <textarea> "{personal_message_value}""#,
        r#"components/members.rs <textarea> "{token_input}""#,
        r#"components/members/member_info_modal/nickname_field.rs <input> "{temp_nickname}""#,
        r#"components/room_list/create_room_modal.rs <input> "{nickname}""#,
        r#"components/room_list/create_room_modal.rs <input> "{room_name}""#,
        r#"components/room_list/edit_room_modal.rs <input> "{input_value}""#,
        r#"components/room_list/edit_room_modal.rs <input> "{max_members_input}""#,
        r#"components/room_list/edit_room_modal.rs <textarea> "{description}""#,
        r#"components/room_list/join_with_code_modal.rs <textarea> "{code_input}""#,
        r#"components/room_list/receive_invitation_modal.rs <input> "{nickname}""#,
        r#"components/room_list/room_name_field.rs <input> "{room_name}""#,
    ];

    /// Read-only display controls, same pinning rationale.
    const EXPECTED_DISPLAY_ONLY: usize = 8;

    // ---------------------------------------------------------------- lexing

    /// Per-character classification of a Rust source file.
    ///
    /// Brace depth must be tracked outside comments AND string literals, or a
    /// `{` in a class string or a `//` in a URL corrupts every block boundary
    /// after it. Attribute TEXT keeps string literals (they are the values)
    /// but drops comments, or a doc comment sitting above an attribute is
    /// glued onto the front of it and the attribute name no longer parses.
    struct Lexed {
        chars: Vec<char>,
        in_comment: Vec<bool>,
        in_string: Vec<bool>,
    }

    impl Lexed {
        fn new(src: &str) -> Self {
            let chars: Vec<char> = src.chars().collect();
            let n = chars.len();
            let mut in_comment = vec![false; n];
            let mut in_string = vec![false; n];
            let mut i = 0;
            while i < n {
                let c = chars[i];
                if c == '/' && i + 1 < n && chars[i + 1] == '/' {
                    let mut j = i;
                    while j < n && chars[j] != '\n' {
                        in_comment[j] = true;
                        j += 1;
                    }
                    i = j;
                } else if c == '/' && i + 1 < n && chars[i + 1] == '*' {
                    let mut j = i;
                    while j < n {
                        in_comment[j] = true;
                        if j > i && chars[j - 1] == '*' && chars[j] == '/' {
                            j += 1;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                } else if c == 'r' && i + 1 < n && (chars[i + 1] == '"' || chars[i + 1] == '#') {
                    // Raw string: r"..." / r#"..."# / r##"..."##
                    let mut hashes = 0;
                    let mut k = i + 1;
                    while k < n && chars[k] == '#' {
                        hashes += 1;
                        k += 1;
                    }
                    if k < n && chars[k] == '"' {
                        let mut j = k + 1;
                        loop {
                            if j >= n {
                                break;
                            }
                            if chars[j] == '"' {
                                let closed = (1..=hashes).all(|h| chars.get(j + h) == Some(&'#'));
                                if closed {
                                    j += hashes + 1;
                                    break;
                                }
                            }
                            j += 1;
                        }
                        for s in in_string.iter_mut().take(j.min(n)).skip(i) {
                            *s = true;
                        }
                        i = j;
                    } else {
                        i += 1;
                    }
                } else if c == '"' {
                    let mut j = i + 1;
                    while j < n {
                        if chars[j] == '\\' {
                            j += 2;
                            continue;
                        }
                        if chars[j] == '"' {
                            j += 1;
                            break;
                        }
                        j += 1;
                    }
                    for s in in_string.iter_mut().take(j.min(n)).skip(i) {
                        *s = true;
                    }
                    i = j;
                } else {
                    i += 1;
                }
            }
            Lexed {
                chars,
                in_comment,
                in_string,
            }
        }

        fn is_code(&self, i: usize) -> bool {
            !self.in_comment[i] && !self.in_string[i]
        }

        fn starts_with_at(&self, i: usize, needle: &str) -> bool {
            needle
                .chars()
                .enumerate()
                .all(|(k, c)| self.chars.get(i + k) == Some(&c))
        }
    }

    // -------------------------------------------------------- cfg(test) cull

    /// Remove `#[cfg(test)]` ITEMS, wherever they appear.
    ///
    /// Deliberately not `src.find("#[cfg(test)]")` and not
    /// `find("#[cfg(test)]\nmod tests {")`. The first is the trap
    /// `conversation.rs` and `members.rs` both document: the first occurrence
    /// there is a test-only HELPER function a thousand lines up, so cutting at
    /// it discards most of the file. Those pins assert a needle is PRESENT, so
    /// an over-short slice fails loudly for them; this audit asserts a needle
    /// is ABSENT, so an over-short slice passes SILENTLY. The polarity is
    /// reversed and the heuristic does not carry over.
    ///
    /// The `mod tests {` variant is also wrong here: this repo has at least
    /// one mid-file `mod tests` (`chat_delegate.rs`), so cutting there would
    /// discard production code below it.
    ///
    /// Removing each annotated item individually is exact, and keeps
    /// production rsx that lives after a test module. It also removes THIS
    /// module, so the fixtures below cannot satisfy the audit's own needles.
    fn strip_cfg_test_items(src: &str) -> String {
        const NEEDLE: &str = "#[cfg(test)]";
        let lx = Lexed::new(src);
        let n = lx.chars.len();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        while i < n {
            if lx.is_code(i) && lx.starts_with_at(i, NEEDLE) {
                // Skip forward past the annotated item: either a brace-delimited
                // body, or a `;`-terminated one (`#[cfg(test)] use foo::Bar;`).
                let mut j = i + NEEDLE.chars().count();
                let mut depth = 0usize;
                let mut opened = false;
                while j < n {
                    if lx.is_code(j) {
                        match lx.chars[j] {
                            '{' => {
                                depth += 1;
                                opened = true;
                            }
                            '}' => {
                                depth -= 1;
                                if opened && depth == 0 {
                                    j += 1;
                                    break;
                                }
                            }
                            ';' if !opened => {
                                j += 1;
                                break;
                            }
                            _ => {}
                        }
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            out.push(lx.chars[i]);
            i += 1;
        }
        out
    }

    // -------------------------------------------------------------- scanning

    struct Element {
        line: usize,
        name: &'static str,
        attrs: Vec<(String, String)>,
    }

    impl Element {
        fn attr(&self, name: &str) -> Option<&str> {
            self.attrs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
        fn has(&self, name: &str) -> bool {
            self.attrs.iter().any(|(k, _)| k == name)
        }
    }

    /// Split an rsx element block into its OWN attributes (brace depth 1).
    ///
    /// Character-based rather than line-based: a line-based split cannot see
    /// the opening line's attributes, silently skips single-line elements, and
    /// mis-reads two attributes written on one line.
    fn parse_attrs(lx: &Lexed, open: usize, close: usize) -> Vec<(String, String)> {
        let mut segments: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut depth = 0usize;
        for (i, ch) in lx.chars.iter().enumerate().take(close + 1).skip(open) {
            if lx.is_code(i) {
                match ch {
                    '{' => {
                        depth += 1;
                        if depth == 1 {
                            continue;
                        }
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    ',' if depth == 1 => {
                        segments.push(std::mem::take(&mut cur));
                        continue;
                    }
                    _ => {}
                }
            }
            if depth >= 1 && !lx.in_comment[i] {
                cur.push(*ch);
            }
        }
        segments.push(cur);

        let mut attrs = Vec::new();
        for seg in segments {
            let t = seg.trim();
            if t.is_empty() {
                continue;
            }
            // Quoted attribute name, e.g. `"data-testid": "x"`.
            if let Some(rest) = t.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    if rest[end + 1..].starts_with(':') {
                        attrs.push((rest[..end].to_string(), rest[end + 2..].trim().to_string()));
                        continue;
                    }
                }
            }
            if let Some(colon) = t.find(':') {
                let name = t[..colon].trim();
                let bare = name.strip_prefix("r#").unwrap_or(name);
                if !bare.is_empty()
                    && bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !bare.starts_with(|c: char| c.is_ascii_digit())
                {
                    attrs.push((bare.to_string(), t[colon + 1..].trim().to_string()));
                }
            }
        }
        attrs
    }

    fn scan_elements(src: &str) -> Vec<Element> {
        let lx = Lexed::new(src);
        let n = lx.chars.len();
        let mut found = Vec::new();
        for name in VOLATILE_ELEMENTS {
            let len = name.chars().count();
            let mut i = 0;
            while i < n {
                if !lx.is_code(i) || !lx.starts_with_at(i, name) {
                    i += 1;
                    continue;
                }
                // Token boundary before the name.
                let prev = if i == 0 { ' ' } else { lx.chars[i - 1] };
                if prev.is_alphanumeric()
                    || prev == '_'
                    || prev == '.'
                    || prev == ':'
                    || prev == '#'
                {
                    i += 1;
                    continue;
                }
                // `{` after optional whitespace (including newlines).
                let mut j = i + len;
                while j < n && lx.chars[j].is_whitespace() {
                    j += 1;
                }
                if j >= n || lx.chars[j] != '{' {
                    i += 1;
                    continue;
                }
                // Reject `match input {`, `if input {`, ... which are not rsx.
                let head: String = lx.chars[..i].iter().rev().take(24).collect::<String>();
                let head: String = head.chars().rev().collect();
                let head = head.trim_end();
                if ["match", "if", "while", "for", "let", "in"]
                    .iter()
                    .any(|kw| head.ends_with(kw))
                {
                    i += 1;
                    continue;
                }
                // Balanced block.
                let mut depth = 0usize;
                let mut k = j;
                let mut close = j;
                while k < n {
                    if lx.is_code(k) {
                        if lx.chars[k] == '{' {
                            depth += 1;
                        } else if lx.chars[k] == '}' {
                            depth -= 1;
                            if depth == 0 {
                                close = k;
                                break;
                            }
                        }
                    }
                    k += 1;
                }
                let line = lx.chars[..i].iter().filter(|c| **c == '\n').count() + 1;
                found.push(Element {
                    line,
                    name,
                    attrs: parse_attrs(&lx, j, close),
                });
                i = j;
            }
        }
        found
    }

    // ------------------------------------------------------------ predicates

    fn is_display_only(el: &Element) -> bool {
        el.attr("readonly")
            .map(|v| {
                let v = v.trim();
                v.starts_with("true") || v.starts_with("\"true\"")
            })
            .unwrap_or(false)
    }

    /// Checkboxes and radios bind `checked:`, not `value:` (see the module doc).
    fn is_toggle(el: &Element) -> bool {
        el.attr("type")
            .map(|v| {
                let v = v.trim().trim_matches('"');
                v == "checkbox" || v == "radio"
            })
            .unwrap_or(false)
    }

    /// The volatile binding this element carries, if any.
    fn volatile_binding(el: &Element) -> Option<&str> {
        el.attr("value").or_else(|| {
            if el.name == "option" {
                el.attr("selected")
            } else {
                None
            }
        })
    }

    /// An `oninput` that provably does nothing is worse than none: it satisfies
    /// a presence check while the field keeps losing keystrokes.
    fn is_noop_handler(body: &str) -> bool {
        let squished: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        squished.ends_with("{}") && squished.contains('|')
    }

    // ----------------------------------------------------------------- files

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

    struct Audit {
        editable: BTreeSet<String>,
        violations: Vec<String>,
        display_only: usize,
    }

    fn run_audit() -> Audit {
        let src_dir = ui_src_dir();
        let mut files = Vec::new();
        rust_sources(&src_dir, &mut files);
        assert!(
            files.len() > 40,
            "the audit must walk the whole ui/src tree; found only {} files",
            files.len()
        );
        files.sort();

        let mut audit = Audit {
            editable: BTreeSet::new(),
            violations: Vec::new(),
            display_only: 0,
        };

        for path in &files {
            let raw = std::fs::read_to_string(path).expect("source file must be readable");
            let production = strip_cfg_test_items(&raw);
            let rel = path
                .strip_prefix(&src_dir)
                .unwrap_or(path)
                .display()
                .to_string();

            for el in scan_elements(&production) {
                let Some(binding) = volatile_binding(&el) else {
                    continue;
                };
                if is_toggle(&el) {
                    continue;
                }
                if is_display_only(&el) {
                    audit.display_only += 1;
                    continue;
                }
                let id = format!("{rel} <{}> {binding}", el.name);
                audit.editable.insert(id.clone());

                match el.attr("oninput") {
                    None => audit
                        .violations
                        .push(format!("{rel}:{} <{}> has no `oninput`", el.line, el.name)),
                    Some(body) if is_noop_handler(body) => audit.violations.push(format!(
                        "{rel}:{} <{}> has a no-op `oninput`",
                        el.line, el.name
                    )),
                    Some(_) => {}
                }
                let _ = el.has("oninput");
            }
        }
        audit
    }

    // ----------------------------------------------------------------- tests

    #[test]
    fn every_editable_value_binding_tracks_input() {
        let audit = run_audit();

        assert!(
            audit.violations.is_empty(),
            "freenet/river#564: these editable controls bind a volatile `value:` \
             without tracking it in `oninput`, so any unrelated re-render will \
             re-write the attribute and wipe whatever the user has typed but not \
             yet committed:\n  {}\n\nAdd `oninput` so the bound signal tracks the \
             live value (making the re-write a no-op). `onchange` alone is NOT \
             enough: it fires on blur, long after the damage. If the control is \
             genuinely display-only, mark it `readonly: true`.",
            audit.violations.join("\n  ")
        );
    }

    /// Anti-vacuity. Pinned as an exact set so a scan that stops seeing a file
    /// fails loudly instead of quietly auditing less.
    #[test]
    fn the_audit_still_sees_every_known_binding() {
        let audit = run_audit();
        let expected: BTreeSet<String> = EXPECTED_EDITABLE.iter().map(|s| s.to_string()).collect();

        let missing: Vec<_> = expected.difference(&audit.editable).collect();
        let unexpected: Vec<_> = audit.editable.difference(&expected).collect();

        assert!(
            missing.is_empty(),
            "the audit STOPPED SEEING these bindings, so they are no longer \
             protected against freenet/river#564. Either they were removed (drop \
             them from EXPECTED_EDITABLE) or, far more likely, the scanner no \
             longer matches this tree:\n  {:?}",
            missing
        );
        assert!(
            unexpected.is_empty(),
            "new editable volatile bindings found. Confirm each one handles \
             `oninput` (see the module docs), then add it to \
             EXPECTED_EDITABLE:\n  {:?}",
            unexpected
        );
        assert_eq!(
            audit.display_only, EXPECTED_DISPLAY_ONLY,
            "the number of readonly display bindings changed; update \
             EXPECTED_DISPLAY_ONLY deliberately"
        );
    }

    /// The cull must remove test items WHEREVER they appear, and must not
    /// discard production code that follows one. Cutting at the first
    /// `#[cfg(test)]` (the trap documented in `conversation.rs`) would drop
    /// `KEEP_ME` here; cutting at `#[cfg(test)]\nmod tests {` would drop it too.
    #[test]
    fn cfg_test_cull_keeps_production_code_after_a_test_item() {
        let src = r#"
fn before() {}
#[cfg(test)]
fn helper() { let x = 1; }
#[cfg(test)]
mod tests { fn t() {} }
fn KEEP_ME() { textarea { value: "{v}" } }
"#;
        let out = strip_cfg_test_items(src);
        assert!(
            out.contains("KEEP_ME"),
            "production code after a test item must survive: {out}"
        );
        assert!(
            !out.contains("helper"),
            "test-only helper must be removed: {out}"
        );
        assert!(
            !out.contains("fn t()"),
            "test module must be removed: {out}"
        );

        // And the naive heuristics really would have lost it, so this test is
        // guarding something real.
        let naive = &src[..src.find("#[cfg(test)]").unwrap()];
        assert!(!naive.contains("KEEP_ME"));
        let mod_needle = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        assert!(!mod_needle.contains("KEEP_ME"));
    }

    /// The audit must detect the shape #564 actually shipped.
    #[test]
    fn audit_detects_the_original_regression() {
        let pre_fix = r#"
            rsx! {
                textarea {
                    class: "w-full",
                    value: "{description}",
                    readonly: !is_owner,
                    onchange: update_description,
                }
            }
        "#;
        let els = scan_elements(pre_fix);
        let el = els
            .iter()
            .find(|e| e.name == "textarea")
            .expect("finds textarea");
        assert_eq!(volatile_binding(el), Some("\"{description}\""));
        assert!(
            !is_display_only(el),
            "`readonly: !is_owner` must NOT count as display-only: the field is \
             editable for the owner, who is exactly who loses their typing"
        );
        assert!(!el.has("oninput"), "the pre-fix field must be flagged");

        let fixed = pre_fix.replace(
            "onchange: update_description,",
            "oninput: move |e| description.set(e.value()), onchange: update_description,",
        );
        let fixed_els = scan_elements(&fixed);
        let fixed_el = fixed_els.iter().find(|e| e.name == "textarea").unwrap();
        assert!(fixed_el.has("oninput"), "the fixed field must pass");
        assert!(!is_noop_handler(fixed_el.attr("oninput").unwrap()));
    }

    /// Shapes the previous line-based scanner got wrong: attributes on the
    /// element's opening line, a whole element on one line, and two attributes
    /// sharing a line. Each was silently skipped (a missed violation) or
    /// mis-read as readonly (a false alarm).
    #[test]
    fn attributes_are_parsed_regardless_of_line_layout() {
        let one_line = r#"input { r#type: "text", value: "{n}" }"#;
        let el = &scan_elements(one_line)[0];
        assert_eq!(
            volatile_binding(el),
            Some("\"{n}\""),
            "single-line element must be seen"
        );
        assert!(!el.has("oninput"), "and must be flagged as a violation");

        let opening_line = "input { value: \"{n}\",\n    oninput: move |e| n.set(e.value()),\n}";
        let el = &scan_elements(opening_line)[0];
        assert_eq!(volatile_binding(el), Some("\"{n}\""));
        assert!(
            el.has("oninput"),
            "attribute on the opening line must be seen"
        );

        let readonly_below = "input {\n    readonly: true,\n    value: \"{link}\",\n}";
        let el = &scan_elements(readonly_below)[0];
        assert!(
            is_display_only(el),
            "readonly must be seen wherever it sits"
        );
    }

    /// A `//` inside a string literal must not corrupt brace matching, and a
    /// comment above an attribute must not be glued onto its name.
    #[test]
    fn strings_and_comments_do_not_corrupt_parsing() {
        let tricky = "input {\n    onclick: move |_| { open(\"https://freenet.org\") },\n    value: \"{room_name}\",\n}";
        let el = &scan_elements(tricky)[0];
        assert_eq!(
            volatile_binding(el),
            Some("\"{room_name}\""),
            "a `//` inside a string literal must not unbalance the block"
        );

        let commented = "input {\n    value: \"{n}\",\n    // Track the live value so Enter can commit it.\n    oninput: move |e| n.set(e.value()),\n}";
        let el = &scan_elements(commented)[0];
        assert!(
            el.has("oninput"),
            "a comment above an attribute must not be glued onto its name"
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
        let els = scan_elements(nested);
        let sel = els.iter().find(|e| e.name == "select").unwrap();
        assert!(sel.has("value"));
        assert!(
            !sel.has("oninput"),
            "depth-1 scoping must ignore the nested option's handler"
        );
    }

    /// A handler that exists but does nothing must not satisfy the audit.
    #[test]
    fn noop_handlers_are_rejected() {
        assert!(is_noop_handler("move |_| {}"));
        assert!(is_noop_handler("move |evt| { }"));
        assert!(!is_noop_handler("move |e| n.set(e.value())"));
        assert!(!is_noop_handler("on_input"));
    }

    /// Toggles are exempt for a REASON, and the reason must stay checked:
    /// dioxus reports a checkbox's `evt.value()` as "true"/"false", never the
    /// DOM's "on", so an `oninput`-tracked `value:` binding there would cause
    /// the rewrite it is meant to prevent.
    #[test]
    fn checkbox_and_radio_are_exempt() {
        let cb = r#"input { r#type: "checkbox", value: "{flag}" }"#;
        assert!(is_toggle(&scan_elements(cb)[0]));
        let radio = r#"input { r#type: "radio", value: "{choice}" }"#;
        assert!(is_toggle(&scan_elements(radio)[0]));
        let text = r#"input { r#type: "text", value: "{name}" }"#;
        assert!(!is_toggle(&scan_elements(text)[0]));
    }
}
