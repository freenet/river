//! Zoom: Ctrl+= and Ctrl+- (Cmd on macOS) make the whole window bigger or smaller in steps, and
//! Ctrl+0 puts it back (on a layout where 0 needs Shift, such as AZERTY, the badge in the room
//! list heading resets it with a press). The level is kept per device in the preferences file.
//!
//! The UI library has no window-wide scale setting, so the zoom is its own `El::scale` on the
//! whole screen: the screen is laid out at the window size divided by the zoom, so a window
//! zoomed to 200% lays out as one half the size and the narrow layout takes over as it would
//! in a smaller window. Text, icons, hit testing and the popups anchored inside it follow the
//! scale. The level is a whole-percent step from [`STEPS`], so a stored value that is not one
//! (a hand edit) is snapped to the nearest, and one that is not a number at all means 100%.

use rune::{Cx, El, KeyEvent, el};

use super::Chat;
use crate::RiverApp;

/// The preference the level is kept under.
pub const PREF: &str = "zoom_percent";

/// The levels, in percent. The same steps as a web browser's.
pub const STEPS: [u16; 8] = [80, 90, 100, 110, 125, 150, 175, 200];

/// The level with no zoom.
pub const DEFAULT: u16 = 100;

/// The smallest layout the zoom may shrink the window to, in points: the narrowest window the
/// app allows (640 by 480), the layout it was made and tested for (with the search panel and a
/// tall draft open, the history needs about this much height). Above a level that would go
/// below it, the zoom stops at the largest scale that does not, so a 640x480 window cannot be
/// zoomed in at all, and a 1280x800 window up to 167%.
pub const MIN_LAYOUT: [f32; 2] = [640.0, 480.0];

/// The key of the element the zoom reads the window's size from.
pub const FRAME: &str = "zoom-frame";

/// The window size assumed until the first layout has said what it is.
const FIRST_SIZE: [f32; 2] = [crate::WINDOW_SIZE.0, crate::WINDOW_SIZE.1];

/// The zoom level and whether it still has to be saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Zoom {
    percent: u16,
    dirty: bool,
    /// The percent the last frame drew at, which is less than `percent` in a window too small
    /// for it (0 before the first frame).
    drawn: std::cell::Cell<u16>,
}

impl Default for Zoom {
    fn default() -> Self {
        Zoom {
            percent: DEFAULT,
            dirty: false,
            drawn: std::cell::Cell::new(0),
        }
    }
}

/// What a zoom key asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    In,
    Out,
    Reset,
}

/// The step in `STEPS` nearest to `percent`; the lower one when two are as near.
pub fn snap(percent: i64) -> u16 {
    *STEPS
        .iter()
        .min_by_key(|s| (i64::from(**s) - percent).abs())
        .expect("STEPS is not empty")
}

/// The step after `percent` in the direction asked, staying within [`STEPS`].
pub fn stepped(percent: u16, step: Step) -> u16 {
    let at = STEPS
        .iter()
        .position(|s| *s == snap(i64::from(percent)))
        .expect("snap returns a step");
    match step {
        Step::In => STEPS[(at + 1).min(STEPS.len() - 1)],
        Step::Out => STEPS[at.saturating_sub(1)],
        Step::Reset => DEFAULT,
    }
}

/// Whether `ev` is a zoom key, and which: Ctrl and `=` or `+` (the same key, with and without
/// Shift) zoom in, Ctrl and `-` zoom out, Ctrl and `0` reset. Cmd on macOS (rune reports both
/// as the command modifier). Not with Alt held: that is a different chord on some layouts.
pub fn step_of(ev: &KeyEvent) -> Option<Step> {
    if ev.mods.alt {
        return None;
    }
    if ev.is_command('=') || ev.is_command('+') {
        Some(Step::In)
    } else if ev.is_command('-') || ev.is_command('_') {
        Some(Step::Out)
    } else if ev.is_command('0') {
        Some(Step::Reset)
    } else {
        None
    }
}

impl Zoom {
    pub fn percent(&self) -> u16 {
        self.percent
    }

    /// The percent the last frame was drawn at.
    pub fn drawn(&self) -> u16 {
        self.drawn.get()
    }

    /// The scale to draw at.
    pub fn factor(&self) -> f32 {
        f32::from(self.percent) / 100.0
    }

    /// Take the stored level (snapped to a step). Not marked to save.
    pub fn load(&mut self, stored: i64) {
        self.percent = snap(stored);
    }

    /// The level to save, once, if it changed since the last call.
    pub fn take_dirty(&mut self) -> Option<u16> {
        std::mem::take(&mut self.dirty).then_some(self.percent)
    }

    /// Set the level to `percent` (snapped to a step), saved like a key press.
    pub fn set(&mut self, percent: u16) {
        let next = snap(i64::from(percent));
        if next != self.percent {
            self.percent = next;
            self.dirty = true;
        }
    }

    pub fn apply(&mut self, step: Step) {
        let next = stepped(self.percent, step);
        if next != self.percent {
            self.percent = next;
            self.dirty = true;
        }
    }
}

impl Chat {
    /// A zoom key: acts and returns whether `ev` was one.
    pub fn zoom_key(&mut self, ev: &KeyEvent, tx: &mut rune::Tx) -> bool {
        let Some(step) = step_of(ev) else {
            return false;
        };
        // The new layout lands at once: a key commit animates by default, and the inner
        // frame's size changes with the zoom, so it would be drawn at in-between sizes.
        tx.instant();
        self.zoom.apply(step);
        true
    }
}

/// The scale to draw at in a window of `size` points: the zoom, or less when the window is so
/// small that the zoomed layout would fall below [`MIN_LAYOUT`]. Never below 1 for a level of
/// 100% or more, so a small window is not shrunk to make room.
pub fn drawn_factor(zoom: f32, size: [f32; 2]) -> f32 {
    let cap = (size[0] / MIN_LAYOUT[0]).min(size[1] / MIN_LAYOUT[1]);
    zoom.min(cap.max(1.0))
}

/// Draw `build`'s screen at the zoom. The screen is always inside the same keyed frame, at 100%
/// too (drawn at scale 1, sized to the frame), so the tree's paths do not change when the zoom
/// crosses 100% and scroll positions, focus and hover survive it. The scale change itself is
/// instant: the library would otherwise glide it on the element's layout spring and draw soft
/// text in between.
pub fn apply(
    chat: &Chat,
    cx: &mut Cx<RiverApp>,
    build: impl FnOnce(&mut Cx<RiverApp>) -> El,
) -> El {
    let zoom = &chat.zoom;
    // The frame is the window, whatever the zoom, so its size is known from the first frame
    // after the first one, before any zoom can be asked for.
    let size = cx.layout_size(FRAME);
    let [w, h] = size.unwrap_or(FIRST_SIZE);
    let f = if zoom.percent() == DEFAULT {
        1.0
    } else {
        drawn_factor(zoom.factor(), [w, h])
    };
    zoom.drawn.set((f * 100.0).round() as u16);
    let inner = build(cx);
    let inner = if f == 1.0 {
        inner.w_full().h_full()
    } else {
        inner.size(w / f, h / f)
    };
    el().key(FRAME).w_full().h_full().child(
        inner
            .scale(f)
            .scale_motion(rune::Anim::INSTANT)
            .origin(0.0, 0.0),
    )
}

/// The indicator while the level is not 100%: the level, in the room list's heading (where it
/// covers no control), and a press on it puts the zoom back. Ctrl+0 also does, but on a layout
/// where 0 needs Shift (AZERTY) the press is the reset that always works. `None` at 100%.
pub(super) fn badge(chat: &Chat) -> Option<El> {
    let zoom = &chat.zoom;
    let percent = zoom.percent();
    if percent == DEFAULT {
        return None;
    }
    let drawn = zoom.drawn().min(percent);
    let p = super::style::palette();
    let reset = super::shortcuts::shown("Mod+0", cfg!(target_os = "macos"));
    // In a window too small for the level, say what is drawn instead.
    let help = if drawn < percent {
        format!(
            "Zoom is {percent}% but the window is too small for it, so River shows {drawn}%. \
             Press to reset ({reset})."
        )
    } else {
        format!("Zoom {percent}%. Press to reset ({reset}).")
    };
    Some(
        rune::row()
            .key("zoom-badge")
            .role("button")
            .label(format!("Zoom {drawn}%. Press to reset."))
            .help(help)
            .px(8.0)
            .h(26.0)
            .radius(13.0)
            .center()
            .border(1.0, p.line_strong)
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press_tx(|s: &mut RiverApp, tx: &mut rune::Tx| {
                tx.instant();
                s.chat.zoom.apply(Step::Reset);
            })
            .child(super::view::label(
                format!("{drawn}%"),
                super::style::SMALL,
                p.text_secondary,
            )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Key, Mods};

    fn ev(c: char, shift: bool, alt: bool) -> KeyEvent {
        KeyEvent {
            key: Key::Command(c),
            mods: Mods {
                cmd: true,
                ctrl: !cfg!(target_os = "macos"),
                logo: cfg!(target_os = "macos"),
                shift,
                alt,
            },
        }
    }

    #[test]
    fn steps_are_ascending_and_include_the_default() {
        assert!(STEPS.windows(2).all(|w| w[0] < w[1]));
        assert!(STEPS.contains(&DEFAULT));
        assert_eq!(STEPS[0], 80);
        assert_eq!(STEPS[STEPS.len() - 1], 200);
    }

    #[test]
    fn stepping_stops_at_the_ends_and_reset_returns_to_100() {
        assert_eq!(stepped(100, Step::In), 110);
        assert_eq!(stepped(100, Step::Out), 90);
        assert_eq!(stepped(80, Step::Out), 80);
        assert_eq!(stepped(200, Step::In), 200);
        assert_eq!(stepped(175, Step::Reset), 100);
        // Walking all the way up and down visits every step once.
        let mut p = 80;
        let mut seen = vec![p];
        loop {
            let n = stepped(p, Step::In);
            if n == p {
                break;
            }
            p = n;
            seen.push(p);
        }
        assert_eq!(seen, STEPS);
    }

    #[test]
    fn a_stored_value_that_is_not_a_step_snaps_to_the_nearest() {
        assert_eq!(snap(100), 100);
        assert_eq!(snap(0), 80);
        assert_eq!(snap(-50), 80);
        assert_eq!(snap(99999), 200);
        assert_eq!(snap(118), 125);
        assert_eq!(snap(105), 100, "equally near: the lower step");
        let mut z = Zoom::default();
        z.load(3);
        assert_eq!(z.percent(), 80);
        assert_eq!(z.take_dirty(), None, "loading is not a change to save");
    }

    #[test]
    fn a_small_window_caps_the_scale_so_the_layout_keeps_the_minimum_window_size() {
        // A roomy window takes any level.
        assert_eq!(drawn_factor(2.0, [1920.0, 1080.0]), 2.0);
        // 1280x800: 800 / 480 leaves 1.67.
        let f = drawn_factor(2.0, [1280.0, 800.0]);
        assert!((f - 800.0 / 480.0).abs() < 1e-6, "{f}");
        // The smallest window cannot be zoomed in at all.
        assert_eq!(drawn_factor(2.0, [640.0, 480.0]), 1.0);
        // The width decides when it is the tighter of the two.
        assert_eq!(drawn_factor(2.0, [960.0, 2000.0]), 1.5);
        // A level below 100% is never raised, and a window below the floor still shows 1.
        assert_eq!(drawn_factor(0.8, [640.0, 480.0]), 0.8);
        assert_eq!(drawn_factor(1.5, [400.0, 300.0]), 1.0);
    }

    #[test]
    fn the_keys() {
        assert_eq!(step_of(&ev('=', false, false)), Some(Step::In));
        assert_eq!(step_of(&ev('+', true, false)), Some(Step::In));
        assert_eq!(step_of(&ev('-', false, false)), Some(Step::Out));
        assert_eq!(step_of(&ev('_', true, false)), Some(Step::Out));
        assert_eq!(step_of(&ev('0', false, false)), Some(Step::Reset));
        assert_eq!(
            step_of(&ev('=', false, true)),
            None,
            "Alt makes it another chord"
        );
        assert_eq!(step_of(&ev('k', false, false)), None);
        let bare = KeyEvent {
            key: Key::Text("=".into()),
            mods: Mods::default(),
        };
        assert_eq!(step_of(&bare), None, "a typed = is text");
    }

    #[test]
    fn a_change_is_saved_once_and_a_no_op_is_not() {
        let mut z = Zoom::default();
        z.apply(Step::Reset);
        assert_eq!(z.take_dirty(), None);
        z.apply(Step::In);
        assert_eq!(z.percent(), 110);
        assert_eq!(z.take_dirty(), Some(110));
        assert_eq!(z.take_dirty(), None);
        assert!((z.factor() - 1.1).abs() < 1e-6);
    }
}
