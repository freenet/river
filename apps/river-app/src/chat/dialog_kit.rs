//! The pieces every dialog is built from, so the dialogs look and behave alike.
//!
//! - [`DialogSpec`]: the frame. A sentence-case title, an optional one-line subtitle (the
//!   object the dialog is about, wrapped inside the body so a long one cannot run past the
//!   edge), the close X, the body, and a footer.
//! - [`footer`]: the one footer rule. Buttons sit at the right edge. The dismiss button
//!   (Cancel, Back or Close) is left of the primary action, and the primary action is the
//!   rightmost button. Any extra action goes at the left edge, apart from those two.
//! - [`notice`]: a box with one of three meanings, each with its own colour and icon:
//!   [`NoticeKind::Info`] for something to know, [`NoticeKind::Warning`] for something that
//!   may go wrong, [`NoticeKind::Danger`] for what a confirm is about to do.
//! - [`danger_button`]: the filled red button for an action that deletes, bans, replaces or
//!   leaves. The safe button next to it stays quiet.
//! - [`label_value`], [`copy_field`] and [`disclosure`]: the label and value row, a value with
//!   its copy button, and the "Details" fold-out.
//!
//! These are plain functions over Rune elements. They change how a dialog is drawn, never what
//! it does: every handler stays with the dialog that passes it in.

use std::rc::Rc;

use rune::{El, col, el, icon, row};
use rune_kit::{Button, ButtonVariant, Dialog, Kit};

use super::icons;
use super::style::{self as st, Palette};
use super::view::label;
use crate::RiverApp;

/// The width of the name column of a [`label_value`] row.
pub const LABEL_W: f32 = 110.0;

/// Key and address text, a little under the body size so a 44-character key fits one line.
pub const KEY_TEXT: rune::tok::TextStyle = rune::tok::TextStyle::new(12.0, 400.0);

/// A dialog being laid out. Build it with [`DialogSpec::new`], add what is in it, and
/// [`DialogSpec::build`] it onto the layer.
pub struct DialogSpec {
    key: &'static str,
    title: String,
    body_key: &'static str,
    width: f32,
    subtitle: Option<String>,
    content: Vec<El>,
    footer: Option<El>,
    gap: f32,
    flush: bool,
    on_close: Rc<dyn Fn(&mut RiverApp)>,
}

impl DialogSpec {
    /// `key` names the dialog (`{key}-dialog`, `{key}-scrim`); `title` is in sentence case: only
    /// its first word and names start with a capital. `on_close` runs for the X, for Escape and
    /// for a press outside.
    pub fn new(
        key: &'static str,
        title: impl Into<String>,
        on_close: impl Fn(&mut RiverApp) + Clone + 'static,
    ) -> Self {
        DialogSpec {
            key,
            title: title.into(),
            body_key: "dialog-body",
            width: 460.0,
            subtitle: None,
            content: Vec::new(),
            footer: None,
            gap: 12.0,
            flush: false,
            on_close: Rc::new(on_close),
        }
    }

    /// The key of the body column, for the dialogs whose tests and scenes look it up.
    pub fn body_key(mut self, key: &'static str) -> Self {
        self.body_key = key;
        self
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// The space between the body's children (12 unless set).
    pub fn gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    /// No padding round the body: for a dialog whose parts pad themselves, such as a scrolling
    /// body over a pinned footer.
    pub fn flush(mut self) -> Self {
        self.flush = true;
        self
    }

    /// One line under the title naming what the dialog is about, such as a room.
    pub fn subtitle(mut self, text: impl Into<String>) -> Self {
        self.subtitle = Some(text.into());
        self
    }

    pub fn child(mut self, e: El) -> Self {
        self.content.push(e);
        self
    }

    pub fn children(mut self, es: impl IntoIterator<Item = El>) -> Self {
        self.content.extend(es);
        self
    }

    /// The footer, as [`footer`] makes it. A dialog with nothing to press has none.
    pub fn footer(mut self, f: El) -> Self {
        self.footer = Some(f);
        self
    }

    pub fn build(self, k: &Kit, layer: &mut Vec<El>) {
        let p = st::palette();
        // Shrinks with Rune's dialog body (which is bounded by the window), so a scroll view
        // inside it that also shrinks gives up height before the footer is cut off.
        let mut body = col()
            .key(self.body_key)
            .w_full()
            .gap(self.gap)
            .shrink()
            .min_h(0.0);
        if !self.flush {
            body = body.px(16.0).pb(16.0);
        }
        if let Some(subtitle) = self.subtitle {
            // In the body, where it wraps: Rune's dialog description is one line that does not
            // wrap, and it cannot be set without being drawn. The body carries it for assistive
            // technology instead.
            body = body.description(subtitle.clone());
            body = body.child(
                label(subtitle, st::BODY, p.text_secondary)
                    .wrap()
                    .w_full()
                    .key("dialog-subtitle"),
            );
        }
        body = body.children(self.content);
        if let Some(f) = self.footer {
            body = body.child(f);
        }
        let on_close = self.on_close;
        Dialog::new(self.key, self.title)
            .width(self.width)
            .fit()
            .open(true)
            .on_close(move |s: &mut RiverApp| on_close(s))
            .child(body)
            .build(k, layer);
    }
}

/// The footer row. `leading` goes at the left edge, apart from the rest (an extra action such
/// as "Quit River"). `trailing` goes at the right edge in the order it is given, which is the
/// dismiss button first and the primary action last.
pub fn footer(leading: Vec<El>, trailing: Vec<El>) -> El {
    if leading.is_empty() {
        return row()
            .w_full()
            .gap(8.0)
            .flex_wrap()
            .justify_end()
            .children(trailing);
    }
    // The trailing group fills what is left of the line and keeps its buttons at the right
    // edge; when it does not fit beside the leading action it wraps to a line of its own,
    // still right-aligned.
    row()
        .w_full()
        .gap(8.0)
        .flex_wrap()
        .children(leading)
        .child(row().grow(1.0).gap(8.0).justify_end().children(trailing))
}

/// A footer with only a dismiss button, for a dialog whose controls all act at once.
pub fn close_footer(k: &Kit, key: &'static str, on_press: fn(&mut RiverApp)) -> El {
    footer(Vec::new(), vec![quiet_button(k, "Close", key, on_press)])
}

/// A quiet button: Cancel, Back, Close, or any action that is not the one the dialog is for.
pub fn quiet_button(
    k: &Kit,
    text: impl Into<String>,
    key: &'static str,
    on_press: fn(&mut RiverApp),
) -> El {
    Button::new(text)
        .variant(ButtonVariant::Ghost)
        .on_press(on_press)
        .build(k)
        .key(key)
}

/// The primary button: the one action the dialog is for.
pub fn primary_button(
    k: &Kit,
    text: impl Into<String>,
    key: &'static str,
    on_press: fn(&mut RiverApp),
) -> El {
    Button::new(text)
        .variant(ButtonVariant::Contrast)
        .on_press(on_press)
        .build(k)
        .key(key)
}

/// The danger button: filled red, white text. For the action that bans, deletes, replaces or
/// leaves. It takes the primary slot, rightmost, with the safe button on its left.
pub fn danger_button(
    k: &Kit,
    text: impl Into<String>,
    key: &'static str,
    on_press: fn(&mut RiverApp),
) -> El {
    let p = st::palette();
    let text = text.into();
    let shown = text.clone();
    Button::new(text)
        .variant(ButtonVariant::Danger)
        .label_with(move |font, _| {
            rune::text(shown)
                .font(font)
                .color(rune::color::hex("#ffffff").expect("a valid colour"))
        })
        .on_press(on_press)
        .build(k)
        .key(key)
        .fill(p.danger_fill)
        .hover(p.danger_fill_hover)
}

/// The danger button's quieter form: red outline and red text. For the button that opens a
/// confirmation ("Ban user", "Leave room"); the confirmation's own button is [`danger_button`].
pub fn danger_outline_button(
    k: &Kit,
    text: impl Into<String>,
    key: &'static str,
    on_press: fn(&mut RiverApp),
) -> El {
    let p = st::palette();
    let text = text.into();
    let shown = text.clone();
    let ink = p.danger;
    Button::new(text)
        .variant(ButtonVariant::Outline)
        .label_with(move |font, _| rune::text(shown).font(font).color(ink))
        .on_press(on_press)
        .build(k)
        .key(key)
        .border(1.0, p.danger)
        // Return presses it as well as Space: the "Leave room" button always did, and a test
        // pins that Return opens the question exactly once.
        .on_key(move |s: &mut RiverApp, ev, _| {
            let enter = ev.key == rune::Key::Enter;
            if enter {
                on_press(s);
            }
            enter
        })
}

/// [`danger_outline_button`] for an action that needs the runtime's [`rune::Tx`], such as
/// quitting the app.
pub fn danger_outline_button_tx(
    k: &Kit,
    text: impl Into<String>,
    key: &'static str,
    on_press: fn(&mut RiverApp, &mut rune::Tx),
) -> El {
    let p = st::palette();
    let text = text.into();
    let shown = text.clone();
    let ink = p.danger;
    Button::new(text)
        .variant(ButtonVariant::Outline)
        .label_with(move |font, _| rune::text(shown).font(font).color(ink))
        .on_press_tx(on_press)
        .build(k)
        .key(key)
        .border(1.0, p.danger)
        .on_key(move |s: &mut RiverApp, ev, tx| {
            let enter = ev.key == rune::Key::Enter;
            if enter {
                on_press(s, tx);
            }
            enter
        })
}

/// What a [`notice`] means. Each has its own colour and icon, so "for your information" and
/// "stop" do not look alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// Something worth knowing; nothing is wrong.
    Info,
    /// Something that may go wrong or that is easy to miss.
    Warning,
    /// What a confirmation is about to do and cannot take back.
    Danger,
}

/// A box of `kind` holding `children`, under `key`, with the kind's icon at its left.
pub fn notice(kind: NoticeKind, key: &str, children: Vec<El>) -> El {
    let p = st::palette();
    let (glyph, ink, fill, line) = match kind {
        NoticeKind::Info => (&icons::INFO, p.accent, p.accent_soft, p.accent_line),
        NoticeKind::Warning => (&icons::WARNING, p.warning_text, p.warning_bg, p.warning),
        NoticeKind::Danger => (&icons::DANGER, p.danger, p.danger_bg, p.error),
    };
    row()
        .key(key.to_owned())
        .w_full()
        .padding(12.0)
        .gap(10.0)
        .radius(8.0)
        .fill(fill)
        .border(1.0, line)
        .child(icon(glyph, 16.0).color(ink).a11y_hidden())
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(8.0)
                .children(children),
        )
}

/// A [`notice`] that is one wrapped paragraph.
pub fn notice_text(kind: NoticeKind, key: &str, text: &str) -> El {
    notice_text_in(kind, key, text, st::SMALL)
}

/// A [`notice_text`] in a given type size, for a notice that carries the dialog's main point.
pub fn notice_text_in(kind: NoticeKind, key: &str, text: &str, style: rune::tok::TextStyle) -> El {
    let p = st::palette();
    notice(
        kind,
        key,
        vec![label(text.to_owned(), style, p.text).wrap().w_full()],
    )
}

/// A divider line.
pub fn rule(p: &Palette) -> El {
    el().w_full().h(1.0).fill(p.line)
}

/// A name column and its value on the same line.
pub fn label_value(key: &str, name: &str, value: El, p: &Palette) -> El {
    row()
        .key(key.to_owned())
        .w_full()
        .gap(8.0)
        .child(
            label(name.to_owned(), st::SMALL_STRONG, p.text_muted)
                .w(LABEL_W)
                .shrink(),
        )
        .child(value.grow(1.0).shrink().min_w(0.0))
}

/// A value that is plain text.
pub fn value_text(key: &str, text: String, p: &Palette) -> El {
    label(text, st::BODY, p.text).wrap().key(key.to_owned())
}

/// A small square button holding the copy icon, which puts `text` on the clipboard. `what`
/// is its name for assistive technology and its tooltip.
pub fn copy_button(key: String, what: &'static str, text: &str, p: &Palette) -> El {
    let copied = text.to_owned();
    row()
        .key(key)
        .role("button")
        .label(what)
        .help(what)
        .focusable(true)
        .size(24.0, 24.0)
        .radius(6.0)
        .center()
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press_tx(move |_: &mut RiverApp, tx: &mut rune::Tx| tx.copy(copied.clone()))
        .child(icon(&icons::COPY, 14.0).color(p.text_muted))
}

/// A name, optionally a sentence on what it is for, its value on one line in `style`, and a
/// copy button at the end of the line.
pub fn copy_field(
    key: &str,
    name: &str,
    what: Option<&str>,
    value: &str,
    copy_what: &'static str,
    style: rune::tok::TextStyle,
    p: &Palette,
) -> El {
    let mut field = col().key(key.to_owned()).w_full().gap(2.0).child(label(
        name.to_owned(),
        st::SMALL_STRONG,
        p.text_muted,
    ));
    if let Some(what) = what {
        field = field.child(
            label(what.to_owned(), st::SMALL, p.text_muted)
                .wrap()
                .w_full()
                .key(format!("{key}-what")),
        );
    }
    field.child(
        row()
            .w_full()
            .gap(8.0)
            .center_y()
            .child(
                label(value.to_owned(), style, p.text)
                    .wrap_anywhere()
                    .grow(1.0)
                    .shrink()
                    .min_w(0.0)
                    .key(format!("{key}-value")),
            )
            .child(copy_button(format!("{key}-copy"), copy_what, value, p)),
    )
}

/// A heading that folds `body` in and out, such as "Technical details". Closed by default;
/// the heading is a button that Enter, Space and a click all press.
pub fn disclosure(
    key: &str,
    title: &str,
    open: bool,
    toggle: fn(&mut RiverApp),
    body: Vec<El>,
    p: &Palette,
) -> El {
    let header = row()
        .key(format!("{key}-toggle"))
        .role("button")
        .label(format!(
            "{title}, {}",
            if open { "shown" } else { "hidden" }
        ))
        .focusable(true)
        .w_full()
        .h(32.0)
        .gap(6.0)
        .center_y()
        .radius(6.0)
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(toggle)
        .on_key(move |s: &mut RiverApp, ev, _| {
            let enter = ev.key == rune::Key::Enter;
            if enter {
                toggle(s);
            }
            enter
        })
        .child(
            icon(&icons::CHEVRON_DOWN, 12.0)
                .color(p.text_muted)
                .rotate(if open {
                    0.0
                } else {
                    -std::f32::consts::FRAC_PI_2
                }),
        )
        .child(label(title.to_owned(), st::SMALL_STRONG, p.text_secondary));
    let mut out = col().key(key.to_owned()).w_full().gap(4.0).child(header);
    if open {
        out = out.child(
            row().w_full().child(el().w(18.0).shrink()).child(
                col()
                    .key(format!("{key}-body"))
                    .grow(1.0)
                    .shrink()
                    .min_w(0.0)
                    .gap(8.0)
                    .children(body),
            ),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// `text` with comments removed (line, trailing and block comments), keeping string and
    /// character literals whole, so a `//` inside a URL is not taken for a comment.
    fn strip_comments(text: &str) -> String {
        let b: Vec<char> = text.chars().collect();
        let (mut out, mut i) = (String::new(), 0);
        while i < b.len() {
            match (b[i], b.get(i + 1)) {
                ('/', Some('/')) => {
                    while i < b.len() && b[i] != '\n' {
                        i += 1;
                    }
                }
                ('/', Some('*')) => {
                    i += 2;
                    while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                        i += 1;
                    }
                    i += 2;
                }
                ('"', _) => {
                    out.push('"');
                    i += 1;
                    while i < b.len() && b[i] != '"' {
                        if b[i] == '\\' && i + 1 < b.len() {
                            out.push(b[i]);
                            i += 1;
                        }
                        out.push(b[i]);
                        i += 1;
                    }
                    out.push('"');
                    i += 1;
                }
                (c, _) => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    /// Every Rust source file under `dir`, with line endings normalised and comments removed.
    fn sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).expect("a readable source directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path)
                    .expect("a readable source file")
                    .replace("\r\n", "\n");
                out.push((path, strip_comments(&text)));
            }
        }
    }

    fn app_sources() -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
        out
    }

    fn is_kit(path: &Path) -> bool {
        path.ends_with("chat/dialog_kit.rs")
    }

    /// Whether `word` occurs in `code` as a whole word.
    fn has_word(code: &str, word: &str) -> bool {
        code.match_indices(word).any(|(at, _)| {
            let before = code[..at].chars().next_back();
            let after = code[at + word.len()..].chars().next();
            let part = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
            !part(before) && !part(after)
        })
    }

    /// The first two arguments of the call whose text follows its opening parenthesis.
    fn two_args(rest: &str) -> Vec<String> {
        let (mut args, mut cur, mut depth, mut in_str) = (Vec::new(), String::new(), 0, false);
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            if in_str {
                cur.push(c);
                if c == '\\' {
                    cur.extend(chars.next());
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => {
                    in_str = true;
                    cur.push(c);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' | ']' | '}' if depth > 0 => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' | ')' if depth == 0 => {
                    args.push(cur.trim().to_owned());
                    cur.clear();
                    if c == ')' || args.len() == 2 {
                        break;
                    }
                }
                _ => cur.push(c),
            }
        }
        args
    }

    /// Dialogs are built by [`super::DialogSpec`] and nothing else, so a new dialog cannot
    /// bring its own title style, close rule or footer. The word `Dialog` (the Rune kit's,
    /// however it is imported or renamed) appears only in the kit.
    #[test]
    fn only_the_kit_builds_a_dialog() {
        for (path, code) in app_sources() {
            if is_kit(&path) {
                continue;
            }
            assert!(
                !has_word(&code, "Dialog"),
                "{} uses rune_kit's Dialog instead of DialogSpec",
                path.display()
            );
        }
    }

    /// A destructive action is a danger button, and a box that says something is a notice.
    /// Neither is restyled by hand (a quiet pink button, an amber box for "stop").
    #[test]
    fn danger_buttons_and_notices_come_from_the_kit() {
        for (path, code) in app_sources() {
            if is_kit(&path) || path.ends_with("chat/style.rs") {
                continue;
            }
            for banned in ["ButtonVariant::Danger", "warning_bg", "danger_bg"] {
                assert!(
                    !code.contains(banned),
                    "{} uses {banned} instead of the dialog kit",
                    path.display()
                );
            }
        }
    }

    /// The titles given to `DialogSpec::new` are in sentence case: the first word and names
    /// start with a capital, nothing else does. A title that is not a string literal cannot be
    /// read here, so each one is listed, and a new one fails until it is listed (and its text
    /// checked by hand).
    #[test]
    fn dialog_titles_are_sentence_case() {
        const NAMES: &[&str] = &["Freenet", "River", "I"];
        // Computed titles. `title` in view.rs is the Create/Join match ("Create a room",
        // "Join a room"); `screen.title` is "Use a Freenet node I already have" or
        // "Connect to {address}?"; the third names the person.
        const COMPUTED: &[&str] = &[
            "title",
            "screen.title",
            "format!(\"Invite {name} to another room\")",
        ];
        let (mut literal, mut computed) = (0, 0);
        for (path, code) in app_sources() {
            if is_kit(&path) {
                continue;
            }
            let mut rest = code.as_str();
            while let Some(at) = rest.find("DialogSpec::new(") {
                rest = &rest[at + "DialogSpec::new(".len()..];
                let args = two_args(rest);
                let title = args.get(1).map(String::as_str).unwrap_or_default();
                let Some(title) = title.strip_prefix('"').and_then(|t| t.strip_suffix('"')) else {
                    assert!(
                        COMPUTED.contains(&title),
                        "{}: the dialog title {title:?} is not a literal and is not listed in COMPUTED",
                        path.display()
                    );
                    computed += 1;
                    continue;
                };
                literal += 1;
                for word in title.split_whitespace().skip(1) {
                    let word = word.trim_matches(|c: char| !c.is_alphanumeric());
                    assert!(
                        !word.chars().next().is_some_and(char::is_uppercase)
                            || NAMES.contains(&word),
                        "{}: the title {title:?} is not sentence case",
                        path.display()
                    );
                }
            }
        }
        assert!(literal >= 10, "the scan found {literal} literal titles");
        assert!(computed >= 3, "the scan found {computed} computed titles");
    }
}
