pub mod app;
pub mod back_to_chat;
pub mod conversation;
pub mod direct_messages;
pub mod foreground;
pub(crate) mod icons;
pub mod invite_click_interceptor;
pub mod members;
pub mod mention_click_interceptor;
pub mod room_list;
pub mod scroll_to_latest;

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

    /// Elements carrying a volatile attribute. Which attribute differs:
    /// `input`/`select`/`textarea` mark `value`, `option` marks `selected`
    /// (its `value` is NOT volatile) - dioxus-html `elements.rs:1488,1536,1571,1594`.
    const VOLATILE_ELEMENTS: [&str; 4] = ["input", "textarea", "select", "option"];

    /// Every editable volatile binding in `ui/src`, as
    /// `path <element> bound-expression`.
    ///
    /// Pinned as an EXACT SET, not a count with slack. A loose threshold is how
    /// a scanner rots silently: if a future edit makes a file (or a region of
    /// one) invisible to the scan, its entries simply disappear and a `>=`
    /// floor keeps passing with the fields it was written to protect no longer
    /// covered. An exact set turns that into a loud diff.
    ///
    /// Adding a form control is EXPECTED to fail this test. Add the line, once
    /// you have confirmed the control handles `oninput`.
    const EXPECTED_EDITABLE: &[&str] = &[
        r#"components/conversation.rs <textarea> "{edit_text}""#,
        r#"components/conversation/message_input.rs <textarea> "{message_text}""#,
        r#"components/direct_messages/dm_thread_modal.rs <textarea> "{draft.read()}""#,
        r#"components/direct_messages/invite_via_dm_picker_modal.rs <textarea> "{personal_message_value}""#,
        r#"components/members.rs <input> "{member_query}""#,
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

    /// Read-only display controls, pinned exactly for the same reason: a bare
    /// count cannot say WHICH one changed.
    const EXPECTED_DISPLAY_ONLY: &[&str] = &[
        r#"components/conversation/not_member_notification.rs <input> "{encoded_key}""#,
        r#"components/members.rs <textarea> "{token_text}""#,
        r#"components/members/invite_member_modal.rs <input> invitation_code"#,
        r#"components/members/invite_member_modal.rs <input> invitation_url"#,
        r#"components/members/member_info_modal.rs <input> "{member_id_str}""#,
        r#"components/members/member_info_modal.rs <input> "{vk_str}""#,
        r#"components/room_list/edit_room_modal.rs <input> "{bs58::encode(room_data.owner_vk.as_bytes()).into_string()}""#,
        r#"components/room_list/edit_room_modal.rs <input> "{room_data.contract_key.id()}""#,
        r#"components/room_list/edit_room_modal.rs <input> "{secret_version}""#,
    ];

    // ---------------------------------------------------------------- lexing

    /// Per-character classification of a Rust source file.
    ///
    /// Brace depth must be tracked outside comments, string literals AND char
    /// literals, or a `{` in a class string, a `//` in a URL, or a `'"'` in a
    /// match arm corrupts every block boundary after it. A missing char-literal
    /// arm is not hypothetical: `conversation.rs` matches on `'"'`, which flips
    /// string/code polarity for the rest of the file and silently un-audits it.
    ///
    /// Attribute TEXT keeps string literals (they are the values) but drops
    /// comments, or a doc comment above an attribute is glued onto the front of
    /// it and the attribute name no longer parses.
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
                    // Rust block comments nest.
                    let mut depth = 0usize;
                    let mut j = i;
                    while j < n {
                        if chars[j] == '/' && j + 1 < n && chars[j + 1] == '*' {
                            depth += 1;
                            in_comment[j] = true;
                            in_comment[j + 1] = true;
                            j += 2;
                            continue;
                        }
                        if chars[j] == '*' && j + 1 < n && chars[j + 1] == '/' {
                            in_comment[j] = true;
                            in_comment[j + 1] = true;
                            j += 2;
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            continue;
                        }
                        in_comment[j] = true;
                        j += 1;
                    }
                    i = j;
                } else if (c == 'r' || c == 'b')
                    && i + 1 < n
                    && (chars[i + 1] == '"' || chars[i + 1] == '#')
                {
                    // Raw / byte string: r"..", r#".."#, b"..".
                    let mut hashes = 0;
                    let mut k = i + 1;
                    while k < n && chars[k] == '#' {
                        hashes += 1;
                        k += 1;
                    }
                    if k < n && chars[k] == '"' {
                        let mut j = k + 1;
                        while j < n {
                            if chars[j] == '"'
                                && (1..=hashes).all(|h| chars.get(j + h) == Some(&'#'))
                            {
                                j += hashes + 1;
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
                } else if c == '\'' {
                    // Char literal, or a lifetime (`&'static str`), which is
                    // ordinary code. An escape always means a literal;
                    // otherwise it is only a literal if a quote closes it
                    // immediately.
                    let escaped = chars.get(i + 1) == Some(&'\\');
                    let single = chars.get(i + 2) == Some(&'\'');
                    if escaped || single {
                        let mut j = i + 1;
                        while j < n {
                            if chars[j] == '\\' {
                                j += 2;
                                continue;
                            }
                            if chars[j] == '\'' {
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

        /// The identifier immediately preceding `i` in CODE (comments and
        /// strings skipped), used to tell rsx `input {` from `match input {`.
        fn prev_code_ident(&self, i: usize) -> String {
            let mut j = i;
            while j > 0 {
                j -= 1;
                if self.is_code(j) && !self.chars[j].is_whitespace() {
                    break;
                }
                if j == 0 {
                    return String::new();
                }
            }
            let mut ident: Vec<char> = Vec::new();
            let mut k = j as isize;
            while k >= 0 {
                let idx = k as usize;
                let ch = self.chars[idx];
                if self.is_code(idx) && (ch.is_alphanumeric() || ch == '_') {
                    ident.push(ch);
                    k -= 1;
                } else {
                    break;
                }
            }
            ident.reverse();
            ident.into_iter().collect()
        }
    }

    // -------------------------------------------------------- cfg(test) cull

    /// Blank out `#[cfg(test)]` ITEMS wherever they appear, PRESERVING LINE
    /// NUMBERS so reported positions match the real file.
    ///
    /// Deliberately not `src.find("#[cfg(test)]")` and not
    /// `find("#[cfg(test)]\nmod tests {")`. The first is the trap
    /// `conversation.rs` and `members.rs` both document: the first occurrence
    /// there is a test-only HELPER a thousand lines up, so cutting at it
    /// discards most of the file. Those pins survive it because they assert a
    /// needle is PRESENT, so an over-short slice fails loudly; this audit
    /// asserts a needle is ABSENT, where an over-short slice passes SILENTLY.
    /// The polarity is reversed and the heuristic does not carry over.
    ///
    /// The `mod tests {` variant is wrong here too: this repo has a mid-file
    /// `mod tests` (`chat_delegate.rs`) whose production code would be lost.
    ///
    /// Removing each annotated item individually is exact, and keeps production
    /// rsx that follows a test module. It also removes THIS module, so the
    /// fixtures below cannot satisfy the audit's own needles.
    pub(super) fn strip_cfg_test_items(src: &str) -> String {
        const NEEDLE: &str = "#[cfg(test)]";
        let lx = Lexed::new(src);
        let n = lx.chars.len();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        while i < n {
            if lx.is_code(i) && lx.starts_with_at(i, NEEDLE) {
                let mut j = i + NEEDLE.chars().count();
                let mut braces = 0usize;
                let mut brackets = 0usize;
                let mut opened = false;
                while j < n {
                    if lx.is_code(j) {
                        match lx.chars[j] {
                            '{' => {
                                braces += 1;
                                opened = true;
                            }
                            '}' => {
                                // A `}` before any `{` means the annotation sat
                                // on a struct field / enum variant / match arm,
                                // and we have run into the enclosing block's
                                // close. Stop rather than underflow.
                                if braces == 0 {
                                    break;
                                }
                                braces -= 1;
                                if opened && braces == 0 {
                                    j += 1;
                                    break;
                                }
                            }
                            '[' | '(' => brackets += 1,
                            ']' | ')' => brackets = brackets.saturating_sub(1),
                            // Only a `;` at top level ends the item: one inside
                            // brackets is part of a type like `[u8; 4]`.
                            ';' if !opened && brackets == 0 => {
                                j += 1;
                                break;
                            }
                            _ => {}
                        }
                    }
                    j += 1;
                }
                // Preserve newlines so line numbers stay truthful.
                for k in i..j.min(n) {
                    if lx.chars[k] == '\n' {
                        out.push('\n');
                    }
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
    /// mis-reads two attributes written on one line. Commas inside `(...)` or
    /// `[...]` do not separate attributes.
    fn parse_attrs(lx: &Lexed, open: usize, close: usize) -> Vec<(String, String)> {
        let mut segments: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut braces = 0usize;
        let mut brackets = 0usize;
        for (i, ch) in lx.chars.iter().enumerate().take(close + 1).skip(open) {
            if lx.is_code(i) {
                match ch {
                    '{' => {
                        braces += 1;
                        if braces == 1 {
                            continue;
                        }
                    }
                    '}' => {
                        braces -= 1;
                        if braces == 0 {
                            break;
                        }
                    }
                    '(' | '[' => brackets += 1,
                    ')' | ']' => brackets = brackets.saturating_sub(1),
                    ',' if braces == 1 && brackets == 0 => {
                        segments.push(std::mem::take(&mut cur));
                        continue;
                    }
                    _ => {}
                }
            }
            if braces >= 1 && !lx.in_comment[i] {
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
                let mut j = i + len;
                while j < n && lx.chars[j].is_whitespace() {
                    j += 1;
                }
                if j >= n || lx.chars[j] != '{' {
                    i += 1;
                    continue;
                }
                // Reject `match input {`, `if input {`, ... which are not rsx.
                // Matched as a whole preceding CODE identifier: a substring
                // test over raw text would also fire on a comment ending in
                // "...for" and silently drop a real element.
                let prev_ident = lx.prev_code_ident(i);
                if ["match", "if", "while", "for", "let", "in", "else"]
                    .contains(&prev_ident.as_str())
                {
                    i += 1;
                    continue;
                }
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
                v == "true" || v == "\"true\""
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

    /// The VOLATILE binding this element carries, if any.
    ///
    /// `option` is the odd one out: dioxus marks its `selected` volatile and
    /// its `value` NOT volatile (`elements.rs:1536`). Consulting `value` first
    /// for every element both flagged ordinary `option { value: "us" }` as a
    /// violation and hid the one attribute on `option` that actually is.
    fn volatile_binding(el: &Element) -> Option<&str> {
        if el.name == "option" {
            el.attr("selected")
        } else {
            el.attr("value")
        }
    }

    /// An `oninput` that provably does nothing is worse than none: it satisfies
    /// a presence check while the field keeps losing keystrokes.
    fn is_noop_handler(body: &str) -> bool {
        let squished: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        squished.contains('|') && (squished.ends_with("{}") || squished.ends_with("|()"))
    }

    // ----------------------------------------------------------------- files

    fn ui_src_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    struct Audit {
        editable: BTreeSet<String>,
        display_only: BTreeSet<String>,
        violations: Vec<String>,
    }

    fn run_audit() -> Audit {
        let src_dir = ui_src_dir();
        let mut files = crate::util::source_scan::rust_files(&src_dir);
        assert!(
            files.len() > 40,
            "the audit must walk the whole ui/src tree; found only {} files",
            files.len()
        );
        files.sort();

        let mut audit = Audit {
            editable: BTreeSet::new(),
            display_only: BTreeSet::new(),
            violations: Vec::new(),
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
                let id = format!("{rel} <{}> {binding}", el.name);
                if is_display_only(&el) {
                    audit.display_only.insert(id);
                    continue;
                }
                audit.editable.insert(id);

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
            }
        }
        audit
    }

    fn diff_sets(expected: &[&str], actual: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
        let expected: BTreeSet<String> = expected.iter().map(|s| s.to_string()).collect();
        (
            expected.difference(actual).cloned().collect(),
            actual.difference(&expected).cloned().collect(),
        )
    }

    // ----------------------------------------------------------------- tests

    #[test]
    fn every_editable_value_binding_tracks_input() {
        let audit = run_audit();
        assert!(
            audit.violations.is_empty(),
            "freenet/river#564: these editable controls bind a volatile attribute \
             without tracking it in `oninput`, so any unrelated re-render will \
             re-write the attribute and wipe whatever the user has typed but not \
             yet committed:\n  {}\n\nAdd `oninput` so the bound signal tracks the \
             live value (making the re-write a no-op). `onchange` alone is NOT \
             enough: it fires on blur, long after the damage. If the control is \
             genuinely display-only, mark it `readonly: true`.",
            audit.violations.join("\n  ")
        );
    }

    /// Anti-vacuity. Pinned as exact sets so a scan that stops seeing a file
    /// fails loudly instead of quietly auditing less.
    #[test]
    fn the_audit_still_sees_every_known_binding() {
        let audit = run_audit();

        let (missing, unexpected) = diff_sets(EXPECTED_EDITABLE, &audit.editable);
        assert!(
            missing.is_empty(),
            "the audit STOPPED SEEING these bindings, so they are no longer \
             protected against freenet/river#564. Either they were removed (drop \
             them from EXPECTED_EDITABLE) or, far more likely, the scanner no \
             longer matches this tree:\n  {}",
            missing.join("\n  ")
        );
        assert!(
            unexpected.is_empty(),
            "new editable volatile bindings found. Confirm each handles \
             `oninput` (see the module docs), then add it to \
             EXPECTED_EDITABLE:\n  {}",
            unexpected.join("\n  ")
        );

        let (missing_ro, unexpected_ro) = diff_sets(EXPECTED_DISPLAY_ONLY, &audit.display_only);
        assert!(
            missing_ro.is_empty() && unexpected_ro.is_empty(),
            "the set of readonly display bindings changed; update \
             EXPECTED_DISPLAY_ONLY deliberately.\n  no longer seen: {}\n  newly \
             seen: {}",
            missing_ro.join(", "),
            unexpected_ro.join(", ")
        );
    }

    /// A char literal must not flip string/code polarity. `conversation.rs`
    /// matches on `'"'`, which without a char-literal arm silently un-audits
    /// the rest of the file - the same blindness class as the naive cfg(test)
    /// cut, reached a different way.
    #[test]
    fn char_literals_do_not_blind_the_scanner() {
        let src = "fn esc(c: char) -> &'static str {\n    match c {\n        '\"' => \"&quot;\",\n        '{' => \"brace\",\n        _ => \"\",\n    }\n}\nfn later() { textarea { value: \"{draft}\" } }\n";
        let els = scan_elements(src);
        let ta = els.iter().find(|e| e.name == "textarea");
        assert!(
            ta.is_some(),
            "an element after a `'\"'` / `'{{'` char literal must still be found"
        );
        assert_eq!(volatile_binding(ta.unwrap()), Some("\"{draft}\""));
        assert!(!ta.unwrap().has("oninput"), "and must be flagged");
    }

    /// Lifetimes are not char literals and must stay ordinary code.
    #[test]
    fn lifetimes_are_not_char_literals() {
        let src = "fn f(s: &'static str) -> &'a str { textarea { value: \"{d}\" } }";
        let els = scan_elements(src);
        assert_eq!(els.len(), 1, "the lifetime must not swallow the element");
        assert_eq!(volatile_binding(&els[0]), Some("\"{d}\""));
    }

    /// The cull must remove test items WHEREVER they appear, must not discard
    /// production code that follows one, and must keep line numbers truthful.
    #[test]
    fn cfg_test_cull_keeps_production_code_and_line_numbers() {
        let src = "fn before() {}\n#[cfg(test)]\nfn helper() { let x = 1; }\n#[cfg(test)]\nmod tests { fn t() {} }\nfn keep_me() { textarea { value: \"{v}\" } }\n";
        let out = strip_cfg_test_items(src);
        assert!(
            out.contains("keep_me"),
            "production code must survive: {out}"
        );
        assert!(!out.contains("helper"), "test helper must go: {out}");
        assert!(!out.contains("fn t()"), "test module must go: {out}");
        assert_eq!(
            out.matches('\n').count(),
            src.matches('\n').count(),
            "line numbering must be preserved so reported positions are real"
        );
        let el = &scan_elements(&out)[0];
        assert_eq!(el.line, 6, "the textarea is on line 6 of the ORIGINAL file");

        // The naive heuristics really would have lost it.
        assert!(!src[..src.find("#[cfg(test)]").unwrap()].contains("keep_me"));
        assert!(!src[..src.find("#[cfg(test)]\nmod tests {").unwrap()].contains("keep_me"));
    }

    /// `#[cfg(test)]` on a struct field, enum variant or match arm reaches the
    /// enclosing block's `}` before any `{`. That must not underflow.
    #[test]
    fn cfg_test_on_a_field_does_not_underflow() {
        for src in [
            "struct S {\n    #[cfg(test)]\n    probe: u8,\n}\nfn f() { input { value: \"{v}\" } }",
            "enum E {\n    #[cfg(test)]\n    Probe,\n}\nfn f() { input { value: \"{v}\" } }",
            "fn m(x: u8) { match x {\n    #[cfg(test)]\n    0 => {}\n    _ => {}\n} }\nfn f() { input { value: \"{v}\" } }",
        ] {
            let out = strip_cfg_test_items(src);
            assert!(
                scan_elements(&out).iter().any(|e| e.name == "input"),
                "must not lose the element after a field-level cfg(test): {out}"
            );
        }
    }

    /// Brace char literals (and escaped ones) inside a test module are not
    /// braces: counted as code they would end the cull early (leaking the rest
    /// of the module) or late (eating the production code after it), which is
    /// how a `'}'` cut `chat_delegate.rs`'s test module short under the plain
    /// `util::lex` scan. Lifetimes in the module must not be taken for literals.
    #[test]
    fn cfg_test_cull_skips_brace_char_literals() {
        // Each fixture holds an UNBALANCED brace literal: a lexer that counted
        // it would end the cull early (`'}'`) or late (`'{'`). Lifetimes and
        // escaped literals sit beside them.
        for literal in ["'}'", "'{'", "'\\u{7D}'"] {
            let src = format!(
                "fn before() {{}}\n#[cfg(test)]\nmod tests {{\n    fn t<'a>(s: &'a str) -> Option<usize> {{ s.find({literal}) }}\n    fn u() {{ let _ = ['\\'', '\\\\']; let _ = input {{ value: \"{{leaked}}\" }}; }}\n}}\nfn keep_me() {{ textarea {{ value: \"{{v}}\" }} }}\n"
            );
            let out = strip_cfg_test_items(&src);
            assert!(
                out.contains("keep_me"),
                "{literal}: production code after must survive: {out}"
            );
            assert!(
                !out.contains("leaked"),
                "{literal}: the rest of the test module must go: {out}"
            );
            assert!(
                !out.contains("fn t<"),
                "{literal}: the test module must go: {out}"
            );
            assert!(
                scan_elements(&out).iter().any(|e| e.name == "textarea"),
                "{literal}"
            );
        }
    }

    /// A `;` inside a type like `[u8; 4]` must not end the item early and leak
    /// test-only rsx into the audited source.
    #[test]
    fn cfg_test_item_with_a_semicolon_in_its_signature_is_fully_removed() {
        let src = "#[cfg(test)]\nfn fixture() -> [u8; 4] { let _ = input { value: \"{leaked}\" }; [0; 4] }\nfn keep() {}";
        let out = strip_cfg_test_items(src);
        assert!(
            !out.contains("leaked"),
            "test-only rsx must not leak: {out}"
        );
        assert!(out.contains("keep"));
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
        let fixed_el = scan_elements(&fixed)
            .into_iter()
            .find(|e| e.name == "textarea")
            .unwrap();
        assert!(fixed_el.has("oninput"), "the fixed field must pass");
        assert!(!is_noop_handler(fixed_el.attr("oninput").unwrap()));
    }

    /// Shapes the previous line-based scanner got wrong.
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
        assert!(
            is_display_only(&scan_elements(readonly_below)[0]),
            "readonly must be seen wherever it sits"
        );
    }

    /// A `//` inside a string literal must not corrupt brace matching, and a
    /// comment above an attribute must not be glued onto its name.
    #[test]
    fn strings_and_comments_do_not_corrupt_parsing() {
        let tricky = "input {\n    onclick: move |_| { open(\"https://freenet.org\") },\n    value: \"{room_name}\",\n}";
        assert_eq!(
            volatile_binding(&scan_elements(tricky)[0]),
            Some("\"{room_name}\""),
            "a `//` inside a string literal must not unbalance the block"
        );

        let commented = "input {\n    value: \"{n}\",\n    // Track the live value so Enter can commit it.\n    oninput: move |e| n.set(e.value()),\n}";
        assert!(
            scan_elements(commented)[0].has("oninput"),
            "a comment above an attribute must not be glued onto its name"
        );
    }

    /// The keyword guard must key on the preceding CODE identifier. Matching
    /// raw text would let a comment ending in "...for" / "...in" delete a real
    /// element from the audit, silently.
    #[test]
    fn a_comment_above_an_element_does_not_suppress_it() {
        let src = "// Only visible to the room admin\ninput { value: \"{secret_draft}\" }";
        let els = scan_elements(src);
        assert_eq!(
            els.len(),
            1,
            "comment ending in `in` must not hide the input"
        );
        assert_eq!(volatile_binding(&els[0]), Some("\"{secret_draft}\""));

        let real_match = "match input {\n    _ => {}\n}";
        assert!(
            scan_elements(real_match).is_empty(),
            "`match input {{` is still not an rsx element"
        );
    }

    /// `option`'s VALUE is not volatile; its `selected` is.
    #[test]
    fn option_is_audited_on_selected_not_value() {
        let plain = r#"option { value: "us", "United States" }"#;
        assert_eq!(
            volatile_binding(&scan_elements(plain)[0]),
            None,
            "an ordinary option value must not be reported as a #564 violation"
        );

        let bound = r#"option { value: "uk", selected: "{is_uk}" }"#;
        assert_eq!(
            volatile_binding(&scan_elements(bound)[0]),
            Some("\"{is_uk}\""),
            "option's volatile `selected` must be audited even when `value` is present"
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
        assert!(is_noop_handler("move |_| ()"));
        assert!(!is_noop_handler("move |e| n.set(e.value())"));
        assert!(!is_noop_handler("on_input"));
    }

    /// Toggles are exempt for a REASON, and the reason must stay checked:
    /// dioxus reports a checkbox's `evt.value()` as "true"/"false", never the
    /// DOM's "on", so an `oninput`-tracked `value:` binding there would cause
    /// the rewrite it is meant to prevent.
    #[test]
    fn checkbox_and_radio_are_exempt() {
        assert!(is_toggle(
            &scan_elements(r#"input { r#type: "checkbox", value: "{flag}" }"#)[0]
        ));
        assert!(is_toggle(
            &scan_elements(r#"input { r#type: "radio", value: "{choice}" }"#)[0]
        ));
        assert!(!is_toggle(
            &scan_elements(r#"input { r#type: "text", value: "{name}" }"#)[0]
        ));
    }

    /// `readonly:` must be an exemption only when it is literally true.
    #[test]
    fn conditional_readonly_is_not_an_exemption() {
        for src in [
            r#"input { readonly: !is_owner, value: "{n}" }"#,
            r#"input { readonly: true_when_locked, value: "{n}" }"#,
        ] {
            assert!(
                !is_display_only(&scan_elements(src)[0]),
                "only a literal `true` exempts a field: {src}"
            );
        }
        assert!(is_display_only(
            &scan_elements(r#"input { readonly: true, value: "{n}" }"#)[0]
        ));
    }
}

/// The accent is one token set in `assets/tailwind.css` (`--color-accent`,
/// `--color-accent-hover`, `--color-link`, `--color-link-hover`,
/// `--color-accent-soft`), so a new accent is a change to those lines alone.
/// This fails on an accent written anywhere else: a Tailwind palette blue in
/// a class string, or one of the hex values the app has used for the accent.
#[cfg(test)]
mod accent_token_audit {
    use std::path::{Path, PathBuf};

    fn files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read ui/src") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// `bg-blue-500`, `text-sky-400/50`, …: a palette hue followed by a shade.
    fn palette_blues(src: &str) -> Vec<String> {
        let mut found = Vec::new();
        for hue in ["-blue-", "-sky-", "-indigo-"] {
            for (at, _) in src.match_indices(hue) {
                if src[at + hue.len()..].starts_with(|c: char| c.is_ascii_digit()) {
                    let start = src[..at].rfind([' ', '"', ':']).map_or(0, |i| i + 1);
                    found.push(src[start..at + hue.len() + 3].to_string());
                }
            }
        }
        found
    }

    /// The accent and the link colour, which IS the accent (`--color-link`
    /// resolves to `--color-accent`), with their hover shades. Not `-soft`.
    const ACCENT_NAMES: [&str; 4] = ["accent-hover", "accent", "link-hover", "link"];

    /// Whether `value` (a CSS colour) paints the accent nearly solid: a bare
    /// `var(--color-accent)` (or link, or a hover shade), or a `color-mix`
    /// giving it half or more. Lower-case, whitespace already removed.
    fn paints_accent(value: &str) -> bool {
        // The length of an accent reference at the start of `v`: complete
        // `var(...)` / `(...)` forms (their closing paren rules out `-soft`),
        // or a bare `--color-*` token not followed by more of a name.
        let reference = |v: &str| -> Option<usize> {
            ACCENT_NAMES.iter().find_map(|name| {
                let token = format!("--color-{name}");
                for form in [format!("var({token})"), format!("({token})")] {
                    if v.starts_with(&form) {
                        return Some(form.len());
                    }
                }
                (v.starts_with(&token)
                    && !v[token.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '-'))
                .then_some(token.len())
            })
        };
        if reference(value).is_some() {
            return true;
        }
        // color-mix(in <space>, <a> [p%], <b> [q%]): the accent's share,
        // 50% when neither side gives one.
        let Some(args) = value
            .strip_prefix("color-mix(")
            .and_then(|v| v.split_once(',').map(|(_, rest)| rest))
        else {
            return false;
        };
        // Only color-mix's own closing paren: a `var(...)` keeps its own.
        let args = args.strip_suffix(')').unwrap_or(args);
        let parts: Vec<&str> = args.splitn(2, ',').collect();
        let share = |part: &str| -> Option<f64> {
            let p = part.strip_suffix('%')?;
            let start = p
                .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
                .map_or(0, |i| i + 1);
            p[start..].parse().ok()
        };
        parts.iter().enumerate().any(|(i, part)| {
            reference(part).is_some() && {
                let theirs = parts.get(1 - i).and_then(|p| share(p));
                share(part).or(theirs.map(|t| 100.0 - t)).unwrap_or(50.0) >= 50.0
            }
        })
    }

    /// Solid accent fills: the accent (or the link colour, which is the
    /// accent) as a background, other than the soft fill (`-soft`) or a faint
    /// tint (`/N` below 50). White text on the dark theme's accent is 2.15:1,
    /// and the design keeps the accent for text, lines and faint tints.
    /// Covers Tailwind classes in any variant (`hover:`, `!`, v4 `(--x)` and
    /// v3 `[...]` arbitrary values) and CSS or inline `background[-color]`
    /// declarations, whitespace ignored.
    fn solid_accent_fills(src: &str) -> Vec<String> {
        let mut found = Vec::new();
        // Classes: split on anything a class cannot contain.
        let is_class_char = |c: char| c.is_ascii_alphanumeric() || "-_:/[]().%!#,".contains(c);
        for raw in src.split(|c: char| !is_class_char(c)) {
            // Punctuation around a class is not part of it: commas, and
            // parens that open before it or close unmatched after it.
            let mut token = raw.trim_matches(',').trim_start_matches('(');
            while token.ends_with(')') && token.matches(')').count() > token.matches('(').count() {
                token = &token[..token.len() - 1];
            }
            // Drop variants (`hover:`, `md:`), outside brackets/parens.
            let mut depth = 0i32;
            let mut cut = 0;
            for (i, c) in token.char_indices() {
                match c {
                    '[' | '(' => depth += 1,
                    ']' | ')' => depth -= 1,
                    ':' if depth == 0 => cut = i + 1,
                    _ => {}
                }
            }
            let utility = token[cut..].trim_start_matches('!').trim_end_matches('!');
            let Some(value) = utility.strip_prefix("bg-") else {
                continue;
            };
            let (colour, alpha) = match value.rsplit_once('/') {
                Some((c, a)) if !c.ends_with('(') && !a.contains(')') => (c, Some(a)),
                _ => (value, None),
            };
            // The alpha as a fraction; not a number, not a class at all.
            let alpha = match alpha.map(|a| a.trim_start_matches('[').trim_end_matches(']')) {
                None => 1.0,
                Some(a) => match a.strip_suffix('%') {
                    Some(p) => match p.parse::<f64>() {
                        Ok(p) => p / 100.0,
                        Err(_) => continue,
                    },
                    None => match a.parse::<f64>() {
                        Ok(n) if a.contains('.') => n,
                        Ok(n) => n / 100.0,
                        Err(_) => continue,
                    },
                },
            };
            if alpha < 0.5 {
                continue;
            }
            let colour = colour.to_lowercase();
            let solid = ACCENT_NAMES.contains(&colour.as_str())
                || colour
                    .strip_prefix('[')
                    .and_then(|v| v.strip_suffix(']'))
                    .or_else(|| {
                        colour
                            .strip_prefix('(')
                            .and_then(|v| v.strip_suffix(')'))
                            .map(|v| v)
                    })
                    .is_some_and(|v| {
                        let v = v.replace('_', "");
                        paints_accent(&v) || paints_accent(&format!("var({v})"))
                    });
            if solid {
                found.push(token.to_string());
            }
        }
        // Declarations: whitespace out, then every background value.
        let flat: String = src
            .to_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        for prop in ["background-color:", "background:"] {
            for (at, _) in flat.match_indices(prop) {
                // `--x-background-color:` is a custom property, not a fill.
                if flat[..at].ends_with('-') {
                    continue;
                }
                let value = &flat[at + prop.len()..];
                let end = value.find([';', '"', '}', '\'']).unwrap_or(value.len());
                let value = value[..end].trim_end_matches("!important");
                if paints_accent(value) {
                    found.push(format!("{prop}{value}"));
                }
            }
        }
        found
    }

    /// Raw palette status colours (`text-red-400`, `bg-yellow-500/10`,
    /// `dark:text-amber-200`, …): every status colour goes through a token
    /// (`text-danger`, `bg-danger`, `bg-warning-bg`, `border-warning`,
    /// `text-warning-text`, `text-success`, the `bg-success`/`bg-warning`/
    /// `bg-error` dots), which are checked for contrast in both themes. Raw
    /// shades were picked for one theme and failed the other (light yellow on
    /// light yellow in the leave-room confirmation).
    fn raw_status_colours(src: &str) -> Vec<String> {
        const HUES: [&str; 9] = [
            "red", "yellow", "amber", "orange", "green", "emerald", "lime", "rose", "pink",
        ];
        let mut found = Vec::new();
        for raw in src.split(|c: char| !(c.is_ascii_alphanumeric() || "-_:/[]!".contains(c))) {
            // The utility after any variant (`hover:`, `dark:`, `focus:`).
            let utility = raw.rsplit(':').next().unwrap_or(raw).trim_matches('!');
            let mut parts = utility.splitn(3, '-');
            let (Some(_prop), Some(hue), Some(shade)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if HUES.contains(&hue) && shade.starts_with(|c: char| c.is_ascii_digit()) {
                found.push(raw.to_string());
            }
        }
        found
    }

    /// Solid fills reviewed and kept, as (file suffix, what the scan
    /// reports, exactly how many times, why). The count is exact, so a new
    /// fill of the same kind in the same file is not hidden by the entry.
    const ALLOWED_FILLS: [(&str, &str, usize, &str); 2] = [
        (
            "components/back_to_chat.rs",
            "bg-[var(--color-link)]",
            1,
            "the hamburger's unread count: a small badge, panel-coloured text on the link colour, clearing contrast in both themes",
        ),
        (
            "components/room_list/receive_invitation_modal.rs",
            "bg-[var(--color-link)]",
            2,
            "the two subscribe progress bars: an 8px indicator with no text on it",
        ),
    ];

    /// `src` without its `#[cfg(test)]` items (their assertions name the
    /// patterns the scan forbids), however many there are and wherever they
    /// sit, so production code after one is still scanned. Lexer-aware (the
    /// volatile-binding audit's stripper): braces in strings and comments,
    /// and a `#[cfg(test)]` written in a doc comment, do not move the cut.
    fn without_test_modules(src: &str) -> String {
        super::volatile_value_binding_audit::strip_cfg_test_items(src)
    }

    const OLD_ACCENTS: [&str; 7] = [
        "#0265e7",
        "#0255c7",
        "#2563eb",
        "#024cb0",
        "#5c9dff",
        "rgba(2, 101, 231",
        "rgba(59, 130, 246",
    ];

    #[test]
    fn no_accent_colour_is_written_outside_the_tokens() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut paths = Vec::new();
        files(&root.join("src"), &mut paths);
        paths.push(root.join("assets/main.css"));
        paths.push(root.join("assets/tailwind.css"));
        let mut bad = Vec::new();
        for path in &paths {
            let src = std::fs::read_to_string(path).expect("read source");
            // Test modules (this one included) name the patterns in their
            // assertions: scan production code only.
            let src = without_test_modules(&src);
            let src = src.as_str();
            for hit in raw_status_colours(src) {
                bad.push(format!(
                    "{}: raw palette status colour `{hit}`",
                    path.display()
                ));
            }
            let hits = solid_accent_fills(src);
            for (file, fill, count, _) in ALLOWED_FILLS {
                if path.ends_with(file) {
                    let seen = hits.iter().filter(|h| h.as_str() == fill).count();
                    if seen != count {
                        bad.push(format!(
                            "{}: `{fill}` appears {seen} times, the allowlist says {count}",
                            path.display()
                        ));
                    }
                }
            }
            for hit in hits {
                let allowed = ALLOWED_FILLS
                    .iter()
                    .any(|(file, fill, _, _)| path.ends_with(file) && hit == *fill);
                if !allowed {
                    bad.push(format!("{}: solid accent fill `{hit}`", path.display()));
                }
            }
            for hit in palette_blues(src) {
                bad.push(format!("{}: {hit}", path.display()));
            }
            for old in OLD_ACCENTS {
                if src.to_lowercase().contains(old) {
                    bad.push(format!("{}: {old}", path.display()));
                }
            }
        }
        assert!(paths.len() > 50, "premise: the scan found the sources");
        assert!(
            bad.is_empty(),
            "accent colours outside the tokens:\n  {}",
            bad.join("\n  ")
        );
    }

    #[test]
    fn the_scan_sees_a_solid_accent_fill() {
        let sees = |src: &str| !solid_accent_fills(src).is_empty();
        for src in [
            r#"class: "px-3 bg-accent text-white""#,
            r#"class: "hover:bg-accent-hover text-white""#,
            r#"class: "md:hover:bg-accent""#,
            // The link colour is the accent.
            r#"class: "bg-link text-white""#,
            r#"class: "bg-link-hover""#,
            // Near-solid alpha.
            r#"class: "bg-accent/50""#,
            r#"class: "bg-accent/90""#,
            r#"class: "bg-link/[0.6]""#,
            // Important, either side (v3 prefix, v4 suffix).
            r#"class: "bg-accent!""#,
            r#"class: "!bg-accent""#,
            // Arbitrary values, v3 and v4.
            "x bg-[var(--color-accent)] y",
            "x bg-(--color-accent) y",
            "x bg-[--color-link] y",
            "x bg-[color-mix(in_oklab,var(--color-accent)_60%,transparent)] y",
            // Any terminator.
            "{bg-accent}",
            "(bg-accent)",
            "'bg-accent'",
            "`bg-accent`",
            "bg-accent\n",
            // Declarations, with and without spaces, and the shorthand.
            r#"style: "background-color: var(--color-accent); color: white;""#,
            r#"style: "background-color:var(--color-accent)""#,
            "background:   var( --color-link );",
            ".x { background-color: var(--color-accent-hover) }",
            "background-color: color-mix(in oklab, var(--color-accent) 60%, transparent);",
            "background-color: color-mix(in oklab, var(--color-link), transparent);",
            "background-color: color-mix(in oklab, white 30%, var(--color-accent));",
        ] {
            assert!(sees(src), "the scan should see: {src}");
        }
        // The soft fill, faint tints and other colours are allowed.
        for src in [
            r#"class: "bg-accent-soft bg-accent/20 hover:bg-accent/10 bg-link/[0.15]""#,
            r#"class: "text-accent border-accent bg-surface bg-linked-thing""#,
            "background-color: var(--color-accent-soft);",
            "background-color: color-mix(in oklab, var(--color-accent) 15%, transparent);",
            "background-color: color-mix(in oklab, white 60%, var(--color-accent));",
            "--color-link: var(--color-accent);",
            "--my-background-color: var(--color-accent);",
        ] {
            assert!(
                !sees(src),
                "the scan should allow: {src} ({:?})",
                solid_accent_fills(src)
            );
        }
    }

    /// A real file that interleaves test modules with production code:
    /// chat_delegate.rs has a test module from about line 1,300, full of
    /// braces in strings, and production code thousands of lines after it.
    /// A brace counter that ignored strings cut from there to the end of the
    /// file, so none of that code was scanned.
    #[test]
    fn production_code_after_a_real_test_module_is_scanned() {
        let src = include_str!("components/app/chat_delegate.rs");
        let first_test = src
            .find("#[cfg(test)]\nmod ")
            .expect("premise: a mid-file test module");
        let marker = "async fn fire_list_rooms_request()";
        assert!(
            src.find(marker).expect("premise: the marker exists") > first_test,
            "premise: the marker sits after a test module"
        );
        let kept = without_test_modules(src);
        assert!(
            kept.contains(marker),
            "production code after a test module was cut"
        );
        assert!(
            kept.len() < src.len() / 2 && kept.len() > src.len() / 10,
            "the test modules, and only they, were cut ({} of {} bytes kept)",
            kept.len(),
            src.len()
        );
        // A `#[cfg(test)]` in a doc comment is text, not an attribute.
        let doc = "/// Not code: #[cfg(test)] mod x { }\nfn kept() {}\n";
        assert!(without_test_modules(doc).contains("fn kept()"));
    }

    #[test]
    fn the_scan_sees_a_raw_status_colour() {
        for src in [
            r#"class: "text-yellow-400 mb-3""#,
            r#"class: "bg-yellow-500/10 border border-yellow-500/20""#,
            r#"class: "text-red-700 dark:text-red-400""#,
            r#"class: "hover:bg-red-600 bg-red-500""#,
            r#"class: "focus:ring-red-500/50""#,
            r#"class: "text-amber-200""#,
            r#"class: "text-emerald-400""#,
            r#"class: "bg-green-500""#,
            "x !text-orange-300 y",
        ] {
            assert!(
                !raw_status_colours(src).is_empty(),
                "the scan should see: {src}"
            );
        }
        for src in [
            r#"class: "text-danger bg-danger hover:bg-danger-hover border-danger""#,
            r#"class: "bg-warning-bg border-l-[3px] border-warning text-warning-text""#,
            r#"class: "text-success bg-success bg-error bg-warning""#,
            r#"class: "text-text-muted bg-red-ish""#,
        ] {
            assert!(
                raw_status_colours(src).is_empty(),
                "the scan should allow: {src}"
            );
        }
    }

    /// Production code after a test module is still scanned, and every test
    /// module is skipped, not just the first.
    #[test]
    fn test_modules_are_cut_out_and_nothing_else() {
        let src = "fn a() {}\n#[cfg(test)]\nmod tests { fn t() { let s = \"bg-accent\"; } }\nfn b() { let c = \"bg-accent\"; }\n#[cfg(test)]\nmod more { fn u() { \"bg-link\"; } }\n";
        let kept = without_test_modules(src);
        assert!(kept.contains("fn b()"), "{kept}");
        assert!(
            !kept.contains("fn t()") && !kept.contains("fn u()"),
            "{kept}"
        );
        assert_eq!(solid_accent_fills(&kept), vec!["bg-accent"]);
    }

    #[test]
    fn the_scan_sees_a_palette_blue() {
        assert_eq!(
            palette_blues(r#"class: "p-1 bg-blue-500/20""#),
            vec!["bg-blue-500"]
        );
        assert!(palette_blues("text-blue-gray").is_empty());
    }

    #[test]
    fn each_accent_token_is_defined_once_per_theme() {
        let css = include_str!("../assets/tailwind.css");
        let definitions = |token: &str| {
            css.lines()
                .filter(|l| l.trim_start().starts_with(token))
                .count()
        };
        for token in [
            "--color-accent:",
            "--color-accent-hover:",
            "--color-link:",
            "--color-link-hover:",
            "--color-accent-soft:",
        ] {
            assert_eq!(definitions(token), 2, "{token}: light and dark");
        }
    }
}
