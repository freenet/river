//! The tray (status bar) icon: "Open River" and "Quit" in its menu, a click to show the window.
//!
//! Linux uses the StatusNotifierItem protocol (`ksni`), which KDE, Xfce, Cinnamon, MATE and
//! GNOME with its AppIndicator extension all speak. A desktop without it has no tray, which
//! [`start`] reports, and River then quits when its window closes. Windows and macOS use
//! `tray-icon`, which must be made on the thread that runs the window system's events, so
//! [`start`] is called from the app's init there. Both draw the unread count on the icon (dimmed
//! while do not disturb is on), say it and the connection in the tooltip, toggle the window on a
//! left click, and build the menu from [`menu_model`].

#[cfg(any(test, not(target_os = "macos")))]
use std::time::{Duration, Instant};

use river_app::RiverApp;
use river_app::chat::node_status::Connection;
use river_app::notify::TrayMenu;
use rune::AppHandle;

const TOOLTIP: &str = "River";
const LOGO: &[u8] = include_bytes!("../packaging/icons/river.png");
/// The icon's side in pixels: trays draw it at 16 to 32.
const SIDE: usize = 64;

/// The logo shrunk to [`SIDE`] pixels square, as RGBA bytes. `None` if the PNG is not 8-bit RGBA
/// with a side [`SIDE`] divides.
fn icon_rgba() -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(LOGO));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba
        || info.bit_depth != png::BitDepth::Eight
        || info.width != info.height
    {
        return None;
    }
    shrink(&buf[..info.buffer_size()], info.width as usize, SIDE)
}

/// Average `from`-pixel-square RGBA into `to` pixels square, weighting colour by alpha so a
/// transparent edge does not darken it.
fn shrink(rgba: &[u8], from: usize, to: usize) -> Option<Vec<u8>> {
    if to == 0 || !from.is_multiple_of(to) || rgba.len() != from * from * 4 {
        return None;
    }
    let k = from / to;
    let mut out = Vec::with_capacity(to * to * 4);
    for y in 0..to {
        for x in 0..to {
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for dy in 0..k {
                for dx in 0..k {
                    let i = ((y * k + dy) * from + x * k + dx) * 4;
                    let alpha = u32::from(rgba[i + 3]);
                    r += u32::from(rgba[i]) * alpha;
                    g += u32::from(rgba[i + 1]) * alpha;
                    b += u32::from(rgba[i + 2]) * alpha;
                    a += alpha;
                }
            }
            match r.checked_div(a) {
                // Nothing drawn in this block.
                None => out.extend_from_slice(&[0, 0, 0, 0]),
                Some(r) => out.extend_from_slice(&[
                    r as u8,
                    (g / a) as u8,
                    (b / a) as u8,
                    (a / (k * k) as u32) as u8,
                ]),
            }
        }
    }
    Some(out)
}

/// What the tray shows: the unread count the window title and the Dock or taskbar badge show
/// (it comes from the same `Notifier::set_unread` call), how the connection to the node
/// stands, and what the menu lists besides.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub unread: usize,
    pub connection: Option<Connection>,
    pub menu: TrayMenu,
}

/// What choosing a menu item does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Open,
    /// Show the window with this room (by key) open.
    OpenRoom(String),
    MarkAllRead,
    ToggleDnd,
    ToggleLogin,
    /// Restart into the downloaded update.
    Update,
    Quit,
}

/// One line of the tray menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Text that does nothing when chosen.
    Status(String),
    Item {
        label: String,
        action: Action,
        /// `Some` for an item with a tick.
        checked: Option<bool>,
    },
    Separator,
}

/// The longest room name the menu shows before cutting it, in characters.
const ROOM_NAME_CHARS: usize = 40;

fn shortened(name: &str) -> String {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.chars().count() <= ROOM_NAME_CHARS {
        return name;
    }
    let cut: String = name.chars().take(ROOM_NAME_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The menu, top to bottom: the connection; the rooms with unread messages and Mark all as read
/// (when there are any); Open River, Do not disturb, Start River when I log in (where the shell
/// has it), Update available (when one is ready); Quit.
pub fn menu_model(view: &View) -> Vec<Entry> {
    let item = |label: &str, action: Action, checked: Option<bool>| Entry::Item {
        label: label.to_owned(),
        action,
        checked,
    };
    let m = &view.menu;
    let mut out = vec![Entry::Status(
        view.connection
            .map_or("Connecting", Connection::text)
            .to_owned(),
    )];
    if !m.unread_rooms.is_empty() {
        for r in &m.unread_rooms {
            out.push(item(
                &format!("{} ({})", shortened(&r.name), r.count),
                Action::OpenRoom(r.key.clone()),
                None,
            ));
        }
        out.push(item("Mark all as read", Action::MarkAllRead, None));
    }
    out.push(Entry::Separator);
    out.push(item("Open River", Action::Open, None));
    out.push(item("Do Not Disturb", Action::ToggleDnd, Some(m.dnd)));
    if let Some(on) = m.start_at_login {
        out.push(item(
            "Start River when I log in",
            Action::ToggleLogin,
            Some(on),
        ));
    }
    if m.update_ready {
        out.push(item("Update available - restart", Action::Update, None));
    }
    out.push(Entry::Separator);
    out.push(item("Quit", Action::Quit, None));
    out
}

/// Do what a chosen item says. Never blocks: the app's work runs on its UI thread.
fn run(handle: &AppHandle<RiverApp>, action: Action) {
    match action {
        Action::Open => handle.open(),
        Action::OpenRoom(key) => {
            handle.update(move |app, _| app.open_room(&key));
            handle.open();
        }
        Action::MarkAllRead => handle.update(|app, _| app.tray_mark_all_read()),
        Action::ToggleDnd => {
            let window = handle.clone();
            handle.update(move |app, _| {
                if app.tray_toggle_dnd() {
                    window.open();
                }
            });
        }
        Action::ToggleLogin => handle.update(|app, _| app.tray_toggle_start_at_login()),
        Action::Update => {
            // A restart that would lose something asks first, in the window.
            handle.open();
            handle.update(|app, _| app.tray_restart_to_update());
        }
        Action::Quit => handle.quit(),
    }
}

/// The tooltip: "River", with the count when there is one and the connection state, in the
/// words of the node status dialog, when it is not live.
pub fn tooltip(view: &View) -> String {
    let mut text = String::from(TOOLTIP);
    match view.unread {
        0 => {}
        n => text.push_str(&format!(": {n} unread")),
    }
    // Example rooms and a live link need no remark; everything else is a state worth saying.
    if let Some(c) = view.connection
        && !matches!(c, Connection::Connected | Connection::Example)
    {
        text.push_str(&format!(" ({})", c.text()));
    }
    text
}

#[cfg(any(test, not(target_os = "macos")))]
/// What the tray does with a left click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toggle {
    Show,
    Hide,
}

#[cfg(any(test, not(target_os = "macos")))]
/// A click hides the window (to the tray, as closing it does) when it is on screen and in
/// front, and shows it otherwise: hidden, minimised, or behind another window.
pub fn toggle_action(visible: bool, in_front: bool) -> Toggle {
    if visible && in_front {
        Toggle::Hide
    } else {
        Toggle::Show
    }
}

/// The text in the badge: the count up to 9, then "9+". Nothing for 0.
fn badge_text(count: usize) -> Option<&'static str> {
    const DIGITS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
    match count {
        0 => None,
        1..=9 => Some(DIGITS[count - 1]),
        _ => Some("9+"),
    }
}

/// A 3 by 5 glyph, one bit per pixel, rows top to bottom, the leftmost pixel the highest bit.
fn glyph(c: char) -> [u8; 5] {
    match c {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '+' => [0b000, 0b010, 0b111, 0b010, 0b000],
        _ => [0; 5],
    }
}

/// The badge's red, and the digits' white.
const BADGE_RED: [u8; 3] = [211, 47, 47];
/// The badge's height in pixels, and so its radius times two. It sits in the lower right.
const BADGE_H: f32 = 34.0;
/// Each font pixel is this many icon pixels, so a digit is 15 by 25.
const GLYPH_SCALE: usize = 5;
/// The icon pixels between two glyphs.
const GLYPH_GAP: usize = 3;

/// `plain` (the logo, [`SIDE`] pixels square RGBA) with the count in a red badge in its lower
/// right corner: the digit, or "9+" from 10 on. 0 is the plain logo, unchanged. Nothing else
/// marks the icon: only what notifies is counted.
fn badged(plain: &[u8], count: usize) -> Vec<u8> {
    let mut out = plain.to_vec();
    let Some(text) = badge_text(count) else {
        return out;
    };
    let r = BADGE_H / 2.0;
    let side = SIDE as f32;
    let (cy, right) = (side - r - 0.5, side - 0.5);
    let glyph_w = |n: usize| n * 3 * GLYPH_SCALE + n.saturating_sub(1) * GLYPH_GAP;
    let text_w = glyph_w(text.len());
    // A single digit sits in a circle; two glyphs in a pill as wide as they need.
    let left = if text.len() == 1 {
        right - BADGE_H
    } else {
        right - (text_w + 2 * (GLYPH_SCALE + GLYPH_GAP)) as f32
    };
    let (seg_a, seg_b) = (left + r, right - r);
    let mid = (left + right) / 2.0;
    let origin_x = (mid - text_w as f32 / 2.0).round() as usize;
    let origin_y = (cy - (5 * GLYPH_SCALE) as f32 / 2.0).round() as usize;
    for y in 0..SIDE {
        for x in 0..SIDE {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // Distance from the pill's centre line, less its radius: negative inside.
            let dx = px.clamp(seg_a, seg_b) - px;
            let dist = (dx * dx + (py - cy) * (py - cy)).sqrt() - r;
            let cover = (0.5 - dist).clamp(0.0, 1.0);
            if cover == 0.0 {
                continue;
            }
            let i = (y * SIDE + x) * 4;
            let base_a = f32::from(out[i + 3]) / 255.0;
            let a = cover + base_a * (1.0 - cover);
            for k in 0..3 {
                let base = f32::from(out[i + k]) * base_a * (1.0 - cover);
                out[i + k] = ((f32::from(BADGE_RED[k]) * cover + base) / a).round() as u8;
            }
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    for (n, c) in text.chars().enumerate() {
        let gx = origin_x + n * (3 * GLYPH_SCALE + GLYPH_GAP);
        for (row, bits) in glyph(c).into_iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) == 0 {
                    continue;
                }
                for dy in 0..GLYPH_SCALE {
                    for dx in 0..GLYPH_SCALE {
                        let (x, y) = (
                            gx + col * GLYPH_SCALE + dx,
                            origin_y + row * GLYPH_SCALE + dy,
                        );
                        if x < SIDE && y < SIDE {
                            let i = (y * SIDE + x) * 4;
                            out[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                        }
                    }
                }
            }
        }
    }
    out
}

/// How much of its opacity the logo keeps while do not disturb is on.
const DIMMED_ALPHA: f32 = 0.4;

/// `plain` with its opacity reduced to [`DIMMED_ALPHA`], colours unchanged.
fn dimmed(plain: &[u8]) -> Vec<u8> {
    let mut out = plain.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        px[3] = (f32::from(px[3]) * DIMMED_ALPHA).round() as u8;
    }
    out
}

/// The icon for `view`: the logo, dimmed while do not disturb is on, with the unread count in
/// its badge (drawn after dimming, so the count stays fully visible).
fn icon_for(plain: &[u8], view: &View) -> Vec<u8> {
    if view.menu.dnd {
        badged(&dimmed(plain), view.unread)
    } else {
        badged(plain, view.unread)
    }
}

/// The view the tray last showed, so it redraws only on a change. Cloned handles share it.
#[derive(Clone, Default)]
pub struct Tracker(std::sync::Arc<std::sync::Mutex<View>>);

impl Tracker {
    /// Fold a change in; the new view if anything changed, which is when the tray redraws.
    fn change(&self, f: impl FnOnce(&mut View)) -> Option<View> {
        let mut view = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let before = view.clone();
        f(&mut view);
        (*view != before).then(|| view.clone())
    }

    /// The unread count, from `Notifier::set_unread` on the UI thread.
    pub fn set_unread(&self, total: usize) {
        if let Some(view) = self.change(|v| v.unread = total) {
            refresh(view);
        }
    }

    /// The connection state, from `Notifier::set_connection` on the UI thread.
    pub fn set_connection(&self, connection: Connection) {
        if let Some(view) = self.change(|v| v.connection = Some(connection)) {
            refresh(view);
        }
    }

    /// What the menu lists, from `Notifier::set_menu` on the UI thread.
    pub fn set_menu(&self, menu: TrayMenu) {
        if let Some(view) = self.change(|v| v.menu = menu) {
            refresh(view);
        }
    }

    #[cfg(test)]
    pub(crate) fn view(&self) -> View {
        self.0.lock().unwrap().clone()
    }
}

#[cfg(any(test, not(target_os = "macos")))]
/// How soon after the window lost focus a click on the icon still finds it in front. Pressing
/// a tray icon takes focus from the window on Windows and on some Linux panels before the
/// click arrives. Where focus is never reported at all, a window the tray (or the start)
/// showed counts as in front until the first focus event, so a click hides it. macOS has no
/// click toggle: its click opens the menu.
// Accepted edge: a window that is visible behind another app, and lost focus less than this long
// before the click, is hidden by that click instead of brought forward; the next click shows it.
const CLICK_GRACE: Duration = Duration::from_millis(500);

#[cfg(any(test, not(target_os = "macos")))]
/// The window, as the toggle drives it.
trait Window {
    fn visible(&self) -> bool;
    /// Hide it without running the close policy: closing quits when "closing hides to the
    /// tray" is off, and shows the first-time notice.
    fn hide(&self);
    fn open(&self);
}

#[cfg(any(test, not(target_os = "macos")))]
impl Window for AppHandle<RiverApp> {
    fn visible(&self) -> bool {
        self.is_visible()
    }
    fn hide(&self) {
        AppHandle::hide(self);
    }
    fn open(&self) {
        AppHandle::open(self);
    }
}

#[cfg(any(test, not(target_os = "macos")))]
/// Decide and act on a click, on the UI thread, where the app knows about focus.
fn apply_toggle(app: &mut RiverApp, window: &impl Window, now: Instant) {
    match toggle_action(window.visible(), app.recently_in_front(now, CLICK_GRACE)) {
        Toggle::Hide => {
            window.hide();
            app.hidden_by_tray();
        }
        Toggle::Show => {
            window.open();
            app.shown_by_tray();
        }
    }
}

// macOS has no click toggle (its click opens the menu), so nothing calls this there.
#[cfg(not(target_os = "macos"))]
/// A click on the icon: hide the window if it is showing and in front, else show it.
fn toggle(handle: &AppHandle<RiverApp>) {
    let window = handle.clone();
    handle.update(move |app, _| apply_toggle(app, &window, Instant::now()));
}

/// The newest value put in, for a thread that applies it: older ones are replaced, so the
/// thread never works through a backlog and the putter never waits.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
struct Latest<T> {
    slot: std::sync::Mutex<Option<T>>,
    wake: std::sync::Condvar,
}

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
impl<T> Latest<T> {
    const fn new() -> Self {
        Latest {
            slot: std::sync::Mutex::new(None),
            wake: std::sync::Condvar::new(),
        }
    }

    fn put(&self, value: T) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
        self.wake.notify_one();
    }

    /// Wait for a value and take it.
    fn take(&self) -> T {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(v) = slot.take() {
                return v;
            }
            slot = self.wake.wait(slot).unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use ksni::blocking::TrayMethods;

    use super::*;
    use crate::background;

    struct RiverTray {
        handle: AppHandle<RiverApp>,
        /// The logo, RGBA, which the count is drawn on.
        plain: Vec<u8>,
        view: View,
    }

    impl RiverTray {
        /// ARGB32, as the protocol wants.
        fn argb(&self) -> Vec<u8> {
            icon_for(&self.plain, &self.view)
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[3], p[0], p[1], p[2]])
                .collect()
        }
    }

    /// A label as the panel shows it: D-Bus menus read `_` as the mark of a shortcut letter, and
    /// `__` is a literal one.
    fn mnemonic_safe(label: &str) -> String {
        label.replace('_', "__")
    }

    /// The view waiting to be shown. ksni's blocking `update` waits for the tray service's
    /// D-Bus round trip, which must not hold up a frame, so a thread of its own applies the
    /// newest view.
    static LATEST: Latest<View> = Latest::new();

    /// Show a new view: ksni sends the new icon and tooltip to the panel. Never waits.
    pub fn refresh(view: View) {
        LATEST.put(view);
    }

    impl ksni::Tray for RiverTray {
        fn id(&self) -> String {
            "org.freenet.River".into()
        }

        fn title(&self) -> String {
            TOOLTIP.into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![ksni::Icon {
                width: SIDE as i32,
                height: SIDE as i32,
                data: self.argb(),
            }]
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: tooltip(&self.view),
                ..Default::default()
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            toggle(&self.handle);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::{CheckmarkItem, StandardItem};
            menu_model(&self.view)
                .into_iter()
                .map(|entry| match entry {
                    Entry::Separator => ksni::MenuItem::Separator,
                    Entry::Status(label) => StandardItem {
                        label: mnemonic_safe(&label),
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                    Entry::Item {
                        label,
                        action,
                        checked: None,
                    } => StandardItem {
                        label: mnemonic_safe(&label),
                        activate: Box::new(move |t: &mut Self| run(&t.handle, action.clone())),
                        ..Default::default()
                    }
                    .into(),
                    Entry::Item {
                        label,
                        action,
                        checked: Some(checked),
                    } => CheckmarkItem {
                        label: mnemonic_safe(&label),
                        checked,
                        activate: Box::new(move |t: &mut Self| run(&t.handle, action.clone())),
                        ..Default::default()
                    }
                    .into(),
                })
                .collect()
        }

        fn watcher_online(&self) {
            background::set_tray_up(true);
        }

        /// The desktop lost its tray while River runs (an extension turned off, a panel
        /// restarted). A hidden window could not be brought back, so show it.
        fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
            background::set_tray_up(false);
            self.handle.open();
            true
        }
    }

    /// Show the icon. False when this desktop has no tray to show it in.
    pub fn start(handle: &AppHandle<RiverApp>) -> bool {
        let Some(rgba) = icon_rgba() else {
            river_app::diag!("river: the tray icon could not be read");
            return false;
        };
        let view = View::default();
        match (RiverTray {
            handle: handle.clone(),
            plain: rgba,
            view,
        })
        .spawn()
        {
            Ok(h) => {
                let applier =
                    std::thread::Builder::new()
                        .name("river-tray".into())
                        .spawn(move || {
                            loop {
                                let view = LATEST.take();
                                if h.update(|t| t.view = view).is_none() {
                                    // The tray service has shut down.
                                    return;
                                }
                            }
                        });
                if let Err(e) = applier {
                    river_app::diag!("river: the tray icon will not show the unread count: {e}");
                }
                background::set_tray_up(true);
                true
            }
            Err(e) => {
                river_app::diag!("river: no tray icon ({e}); closing the window will quit River");
                false
            }
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod native {
    use std::cell::RefCell;
    use std::sync::Mutex;

    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
    #[cfg(windows)]
    use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

    use super::*;
    use crate::background;

    thread_local! {
        /// The icon, kept alive on the thread that made it: dropping it takes it off the bar.
        static TRAY: RefCell<(Option<TrayIcon>, Vec<u8>)> = const { RefCell::new((None, Vec::new())) };
    }

    /// What each item of the menu now showing does, for the menu event handler.
    static ACTIONS: Mutex<Vec<(MenuId, Action)>> = Mutex::new(Vec::new());

    /// A label as the menu shows it: Windows reads `&` as the mark of a shortcut letter, and `&&`
    /// is a literal one.
    fn label_safe(label: &str) -> String {
        if cfg!(windows) {
            label.replace('&', "&&")
        } else {
            label.to_owned()
        }
    }

    /// The menu for `view`, and what its items do.
    fn build_menu(view: &View) -> Result<(Menu, Vec<(MenuId, Action)>), tray_icon::menu::Error> {
        let menu = Menu::new();
        let mut actions = Vec::new();
        for entry in menu_model(view) {
            match entry {
                Entry::Separator => menu.append(&PredefinedMenuItem::separator())?,
                Entry::Status(label) => {
                    menu.append(&MenuItem::new(label_safe(&label), false, None))?
                }
                Entry::Item {
                    label,
                    action,
                    checked: None,
                } => {
                    let item = MenuItem::new(label_safe(&label), true, None);
                    menu.append(&item)?;
                    actions.push((item.id().clone(), action));
                }
                Entry::Item {
                    label,
                    action,
                    checked: Some(on),
                } => {
                    let item = CheckMenuItem::new(label_safe(&label), true, on, None);
                    menu.append(&item)?;
                    actions.push((item.id().clone(), action));
                }
            }
        }
        Ok((menu, actions))
    }

    /// Show a new view: the icon, the tooltip and the menu. Runs on the UI thread, which is the
    /// thread that made the icon (see [`TRAY`]); anywhere else there is no icon to change and
    /// this does nothing.
    pub fn refresh(view: View) {
        TRAY.with(|t| {
            let (tray, plain) = &*t.borrow();
            let Some(tray) = tray else { return };
            if let Ok(icon) = Icon::from_rgba(icon_for(plain, &view), SIDE as u32, SIDE as u32) {
                let _ = tray.set_icon(Some(icon));
            }
            let _ = tray.set_tooltip(Some(tooltip(&view)));
            match build_menu(&view) {
                Ok((menu, actions)) => {
                    *ACTIONS.lock().unwrap_or_else(|e| e.into_inner()) = actions;
                    tray.set_menu(Some(Box::new(menu)));
                }
                Err(e) => river_app::diag!("river: the tray menu could not be updated: {e}"),
            }
        });
    }

    /// Show the icon. Call on the thread that runs the window system's events.
    pub fn start(handle: &AppHandle<RiverApp>) -> bool {
        let Some(rgba) = icon_rgba() else {
            river_app::diag!("river: the tray icon could not be read");
            return false;
        };
        let view = View::default();
        let icon = match Icon::from_rgba(icon_for(&rgba, &view), SIDE as u32, SIDE as u32) {
            Ok(i) => i,
            Err(e) => {
                river_app::diag!("river: the tray icon could not be made: {e}");
                return false;
            }
        };
        let (menu, actions) = match build_menu(&view) {
            Ok(m) => m,
            Err(e) => {
                river_app::diag!("river: the tray menu could not be made: {e}");
                return false;
            }
        };
        *ACTIONS.lock().unwrap_or_else(|e| e.into_inner()) = actions;
        let on_menu = handle.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = ACTIONS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|(id, _)| *id == event.id)
                .map(|(_, a)| a.clone());
            if let Some(action) = action {
                run(&on_menu, action);
            }
        }));
        // Windows shows the window on a left click and the menu on a right one. The Mac's
        // menu bar opens the menu on a click, so there the click is the menu's, and the window
        // is not also opened.
        #[cfg(windows)]
        {
            let on_click = handle.clone();
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    toggle(&on_click);
                }
            }));
        }
        let built = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(cfg!(target_os = "macos"))
            .with_tooltip(tooltip(&view))
            .with_icon(icon)
            .build();
        match built {
            Ok(tray) => {
                TRAY.with(|t| *t.borrow_mut() = (Some(tray), rgba));
                background::set_tray_up(true);
                true
            }
            Err(e) => {
                river_app::diag!("river: no tray icon ({e}); closing the window will quit River");
                false
            }
        }
    }

    /// Take the icon off the bar, on the thread that made it (a Windows icon would otherwise
    /// stay until the pointer crosses it).
    pub fn stop() {
        background::set_tray_up(false);
        TRAY.with(|t| t.borrow_mut().0 = None);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub use linux::{refresh, start};
#[cfg(any(windows, target_os = "macos"))]
pub use native::{refresh, start, stop};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logo_becomes_a_small_rgba_icon() {
        let px = icon_rgba().expect("the packaged logo is 8-bit RGBA");
        assert_eq!(px.len(), SIDE * SIDE * 4);
        assert!(
            px.as_chunks::<4>().0.iter().any(|p| p[3] > 0),
            "something is drawn"
        );
    }

    #[test]
    fn shrinking_averages_by_alpha_and_refuses_odd_sizes() {
        // A 2x2 with one opaque red pixel and three transparent black ones, to one pixel:
        // the colour stays red (not darkened by the transparent ones) at a quarter alpha.
        let px = [255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(shrink(&px, 2, 1), Some(vec![255, 0, 0, 63]));
        assert_eq!(shrink(&px, 2, 3), None);
        assert_eq!(shrink(&px[..8], 2, 1), None);
    }

    /// The Linux menu entry: named for the window's app id so panels match the two, with the
    /// same id as `StartupWMClass` and the icon the packaging installs.
    #[test]
    fn the_desktop_entry_matches_the_window_app_id() {
        let entry = include_str!("../packaging/linux/org.freenet.River.desktop");
        let has = |line: &str| entry.lines().any(|l| l == line);
        assert!(has("Name=River"));
        assert!(has("Icon=river"));
        assert!(has("Exec=river"));
        assert!(has("StartupWMClass=org.freenet.River"));
        // Notifications name the same entry, or the shell cannot attribute them to it.
        let xdg = include_str!("notify/xdg.rs");
        assert!(xdg.contains("Hint::DesktopEntry(\"org.freenet.River\".into())"));
        let main = include_str!("main.rs");
        assert!(main.contains("rune::set_app_id(\"org.freenet.River\")"));
    }

    fn plain() -> Vec<u8> {
        icon_rgba().unwrap()
    }

    /// Whether any pixel of `a` and `b` differ inside (`inside`) or outside the badge's corner.
    fn differs(a: &[u8], b: &[u8], inside: bool) -> bool {
        (0..SIDE * SIDE).any(|p| {
            let (x, y) = (p % SIDE, p / SIDE);
            let corner = x >= SIDE - 52 && y >= SIDE - BADGE_H as usize - 1;
            corner == inside && a[p * 4..p * 4 + 4] != b[p * 4..p * 4 + 4]
        })
    }

    #[test]
    fn zero_unread_is_the_plain_logo() {
        assert_eq!(badged(&plain(), 0), plain());
    }

    #[test]
    fn a_count_draws_a_badge_in_the_corner_and_touches_nothing_else() {
        let plain = plain();
        for n in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 99, 1000] {
            let out = badged(&plain, n);
            assert_eq!(out.len(), plain.len(), "size unchanged for {n}");
            assert!(differs(&out, &plain, true), "{n} changes the corner");
            assert!(!differs(&out, &plain, false), "{n} leaves the rest alone");
            // The badge is opaque red/white in its middle: a pixel there is fully drawn.
            let mid = (SIDE - 18) * SIDE + SIDE - 18;
            assert_eq!(out[mid * 4 + 3], 255, "{n}: badge centre is opaque");
        }
    }

    #[test]
    fn each_count_looks_different_up_to_nine_and_ten_up_is_nine_plus() {
        let plain = plain();
        let shown: Vec<Vec<u8>> = (1..=9).map(|n| badged(&plain, n)).collect();
        for i in 0..shown.len() {
            for j in i + 1..shown.len() {
                assert_ne!(shown[i], shown[j], "{} and {} differ", i + 1, j + 1);
            }
        }
        assert_eq!(badged(&plain, 10), badged(&plain, 5000));
        assert!(shown.iter().all(|s| *s != badged(&plain, 10)));
        assert_eq!(badge_text(0), None);
        assert_eq!(badge_text(9), Some("9"));
        assert_eq!(badge_text(10), Some("9+"));
    }

    #[test]
    fn the_badge_has_white_digit_pixels_on_red() {
        let out = badged(&plain(), 8);
        let px = |i: usize| &out[i * 4..i * 4 + 3];
        let n = SIDE * SIDE;
        assert!((0..n).any(|i| px(i) == [255, 255, 255]), "a white digit");
        assert!((0..n).any(|i| px(i) == BADGE_RED), "a red field");
    }

    #[test]
    fn the_tray_hears_the_count_the_title_shows_and_redraws_only_on_a_change() {
        let t = Tracker::default();
        assert_eq!(
            t.change(|v| v.unread = 0),
            None,
            "0 is what it already shows"
        );
        assert_eq!(t.change(|v| v.unread = 3).map(|v| v.unread), Some(3));
        assert_eq!(t.change(|v| v.unread = 3), None, "an unchanged count");
        assert_eq!(t.change(|v| v.unread = 0).map(|v| v.unread), Some(0));
        let c = Some(Connection::Connected);
        assert!(t.change(|v| v.connection = c).is_some());
        assert_eq!(t.change(|v| v.connection = c), None);
    }

    #[test]
    fn the_tooltip_says_the_count_and_the_connection_when_it_is_not_live() {
        let v = |unread, connection| View {
            unread,
            connection,
            ..Default::default()
        };
        assert_eq!(tooltip(&v(0, None)), "River");
        assert_eq!(tooltip(&v(0, Some(Connection::Connected))), "River");
        assert_eq!(tooltip(&v(0, Some(Connection::Example))), "River");
        assert_eq!(
            tooltip(&v(1, Some(Connection::Connected))),
            "River: 1 unread"
        );
        assert_eq!(
            tooltip(&v(3, Some(Connection::Connected))),
            "River: 3 unread"
        );
        assert_eq!(tooltip(&v(12, None)), "River: 12 unread");
        assert_eq!(
            tooltip(&v(0, Some(Connection::Connecting))),
            "River (Connecting)"
        );
        assert_eq!(
            tooltip(&v(2, Some(Connection::Unreachable))),
            "River: 2 unread (Disconnected: nothing answers)"
        );
    }

    #[test]
    fn a_click_hides_a_window_in_front_and_shows_it_otherwise() {
        assert_eq!(toggle_action(true, true), Toggle::Hide);
        assert_eq!(toggle_action(true, false), Toggle::Show);
        assert_eq!(toggle_action(false, true), Toggle::Show);
        assert_eq!(toggle_action(false, false), Toggle::Show);
    }

    struct Spy(std::cell::RefCell<Vec<&'static str>>, bool);
    impl Window for Spy {
        fn visible(&self) -> bool {
            self.1
        }
        fn hide(&self) {
            self.0.borrow_mut().push("hide");
        }
        fn open(&self) {
            self.0.borrow_mut().push("open");
        }
    }

    /// The window driver has no way to close or quit, so a click can only hide or open it, even
    /// with "closing hides to the tray" off. Hiding also tells the app it lost focus.
    #[test]
    fn a_click_on_a_focused_window_hides_it_without_the_close_policy() {
        let mut app = RiverApp::detached();
        app.focus_changed(true);
        let spy = Spy(Default::default(), true);
        apply_toggle(&mut app, &spy, Instant::now());
        assert_eq!(*spy.0.borrow(), ["hide"]);
        assert!(!app.window_in_front(), "hiding clears the focus");
        // Pinned: the toggle never goes through `close`, which is what quits with the setting off.
        let src = include_str!("tray.rs");
        let body = &src[src.find("fn apply_toggle").unwrap()..src.find("fn toggle(").unwrap()];
        assert!(!body.contains(".close()") && !body.contains("quit"));
    }

    #[test]
    fn a_click_a_moment_after_the_icon_took_focus_still_hides() {
        let mut app = RiverApp::detached();
        app.focus_changed(true);
        app.focus_changed(false);
        let spy = Spy(Default::default(), true);
        apply_toggle(&mut app, &spy, Instant::now());
        assert_eq!(*spy.0.borrow(), ["hide"]);
        // Long after, a window that is visible but not focused is brought forward instead.
        let mut app = RiverApp::detached();
        app.focus_changed(true);
        app.focus_changed(false);
        let spy = Spy(Default::default(), true);
        apply_toggle(&mut app, &spy, Instant::now() + Duration::from_secs(5));
        assert_eq!(*spy.0.borrow(), ["open"]);
    }

    #[test]
    fn a_window_the_tray_showed_hides_on_the_next_click_even_if_no_focus_event_comes() {
        let mut app = RiverApp::detached();
        let hidden = Spy(Default::default(), false);
        apply_toggle(&mut app, &hidden, Instant::now());
        assert_eq!(*hidden.0.borrow(), ["open"]);
        let shown = Spy(Default::default(), true);
        apply_toggle(&mut app, &shown, Instant::now());
        assert_eq!(*shown.0.borrow(), ["hide"]);
    }

    #[test]
    fn only_the_newest_pending_view_is_applied() {
        let latest = Latest::new();
        for unread in 1..=5 {
            latest.put(View {
                unread,
                ..Default::default()
            });
        }
        assert_eq!(latest.take().unread, 5);
        latest.put(View::default());
        assert_eq!(latest.take(), View::default());
    }

    struct Flip(std::cell::Cell<bool>, std::cell::RefCell<Vec<&'static str>>);
    impl Window for Flip {
        fn visible(&self) -> bool {
            self.0.get()
        }
        fn hide(&self) {
            self.0.set(false);
            self.1.borrow_mut().push("hide");
        }
        fn open(&self) {
            self.0.set(true);
            self.1.borrow_mut().push("open");
        }
    }

    /// A platform that never reports focus: show, hide, show, hide, each click doing the
    /// opposite of the one before, and a hidden window never counted as in front.
    #[test]
    fn repeated_clicks_alternate_without_any_focus_event() {
        let mut app = RiverApp::detached();
        let w = Flip(std::cell::Cell::new(false), Default::default());
        for _ in 0..3 {
            apply_toggle(&mut app, &w, Instant::now());
            assert!(app.window_in_front(), "shown");
            apply_toggle(&mut app, &w, Instant::now());
            assert!(!app.window_in_front(), "a hidden window is not in front");
        }
        assert_eq!(
            *w.1.borrow(),
            ["open", "hide", "open", "hide", "open", "hide"]
        );
    }

    use river_app::notify::UnreadRoom;

    fn room(key: &str, name: &str, count: usize) -> UnreadRoom {
        UnreadRoom {
            key: key.into(),
            name: name.into(),
            count,
        }
    }

    fn labels(view: &View) -> Vec<String> {
        menu_model(view)
            .into_iter()
            .map(|e| match e {
                Entry::Status(t) => format!("status:{t}"),
                Entry::Separator => "---".into(),
                Entry::Item { label, checked, .. } => match checked {
                    None => label,
                    Some(on) => format!("{label}[{}]", if on { "x" } else { " " }),
                },
            })
            .collect()
    }

    fn live() -> View {
        View {
            connection: Some(Connection::Connected),
            menu: TrayMenu {
                start_at_login: Some(false),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn nothing_unread_has_no_room_lines_and_no_mark_all() {
        assert_eq!(
            labels(&live()),
            [
                "status:Connected",
                "---",
                "Open River",
                "Do Not Disturb[ ]",
                "Start River when I log in[ ]",
                "---",
                "Quit",
            ]
        );
    }

    #[test]
    fn unread_rooms_come_first_each_with_its_count_then_mark_all_read() {
        let mut v = live();
        v.menu.unread_rooms = vec![room("a", "Alpha", 3), room("b", "Beta", 12)];
        let l = labels(&v);
        assert_eq!(
            l[..5],
            [
                "status:Connected",
                "Alpha (3)",
                "Beta (12)",
                "Mark all as read",
                "---"
            ]
        );
        let m = menu_model(&v);
        assert!(matches!(&m[1], Entry::Item { action: Action::OpenRoom(k), .. } if k == "a"));
        assert!(matches!(
            &m[3],
            Entry::Item {
                action: Action::MarkAllRead,
                ..
            }
        ));
    }

    #[test]
    fn a_long_room_name_is_cut() {
        let mut v = live();
        v.menu.unread_rooms = vec![room("a", &"x".repeat(100), 1)];
        let Entry::Item { label, .. } = &menu_model(&v)[1] else {
            panic!("a room line")
        };
        assert!(label.chars().count() <= ROOM_NAME_CHARS + 4, "{label}");
        assert!(label.contains('…') && label.ends_with(" (1)"));
    }

    #[test]
    fn do_not_disturb_and_login_show_their_state_and_the_update_item_appears_when_ready() {
        let mut v = live();
        v.menu.dnd = true;
        v.menu.start_at_login = Some(true);
        v.menu.update_ready = true;
        assert_eq!(
            labels(&v),
            [
                "status:Connected",
                "---",
                "Open River",
                "Do Not Disturb[x]",
                "Start River when I log in[x]",
                "Update available - restart",
                "---",
                "Quit",
            ]
        );
        // Where the shell has no login item, there is no such line.
        v.menu.start_at_login = None;
        assert!(!labels(&v).iter().any(|l| l.starts_with("Start River")));
    }

    #[test]
    fn the_status_line_says_the_connection_in_the_tooltips_words() {
        for c in [
            Connection::Connecting,
            Connection::Reconnecting,
            Connection::Unreachable,
            Connection::NotApproved,
        ] {
            let mut v = live();
            v.connection = Some(c);
            assert_eq!(labels(&v)[0], format!("status:{}", c.text()));
            assert!(
                tooltip(&v).contains(c.text()),
                "same wording as the tooltip"
            );
        }
        let mut v = live();
        v.connection = None;
        assert_eq!(labels(&v)[0], "status:Connecting");
    }

    #[test]
    fn the_status_line_is_text_and_quit_is_last() {
        let m = menu_model(&live());
        assert!(matches!(m[0], Entry::Status(_)));
        assert!(matches!(
            m.last(),
            Some(Entry::Item {
                action: Action::Quit,
                ..
            })
        ));
    }

    #[test]
    fn the_menu_is_rebuilt_only_when_something_it_shows_changes() {
        let t = Tracker::default();
        let menu = |n| TrayMenu {
            unread_rooms: vec![room("a", "A", n)],
            ..Default::default()
        };
        assert!(t.change(|v| v.menu = menu(1)).is_some());
        assert_eq!(t.change(|v| v.menu = menu(1)), None, "the same menu");
        assert!(t.change(|v| v.menu = menu(2)).is_some());
        assert!(t.change(|v| v.menu.dnd = true).is_some());
        assert_eq!(t.change(|v| v.menu.dnd = true), None);
    }

    #[test]
    fn do_not_disturb_dims_the_logo_and_keeps_the_count_visible() {
        let plain = plain();
        let on = View {
            menu: TrayMenu {
                dnd: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let dim = icon_for(&plain, &on);
        assert_ne!(dim, plain);
        for (d, p) in dim.as_chunks::<4>().0.iter().zip(plain.as_chunks::<4>().0) {
            assert_eq!(d[..3], p[..3], "colour unchanged");
            assert!(d[3] < p[3] || p[3] == 0, "more transparent");
            assert!(f32::from(d[3]) <= f32::from(p[3]) * DIMMED_ALPHA + 1.0);
        }
        let off = View::default();
        assert_eq!(icon_for(&plain, &off), plain, "not dimmed without it");
        // The count stays as bright as on a plain icon.
        let counted = View {
            unread: 4,
            ..on.clone()
        };
        let badge = icon_for(&plain, &counted);
        let mid = (SIDE - 18) * SIDE + SIDE - 18;
        assert_eq!(badge[mid * 4 + 3], 255, "the badge is opaque");
        assert_eq!(
            badge[mid * 4..mid * 4 + 4],
            badged(&plain, 4)[mid * 4..mid * 4 + 4]
        );
    }
}
