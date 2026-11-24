//! The tray (status bar) icon: "Open River" and "Quit" in its menu, a click to show the window.
//!
//! Linux uses the StatusNotifierItem protocol (`ksni`), which KDE, Xfce, Cinnamon, MATE and
//! GNOME with its AppIndicator extension all speak. A desktop without it has no tray, which
//! [`start`] reports, and River then quits when its window closes. Windows and macOS use
//! `tray-icon`, which must be made on the thread that runs the window system's events, so
//! [`start`] is called from the app's init there.

use river_app::RiverApp;
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

#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use ksni::blocking::TrayMethods;

    use super::*;
    use crate::background;

    struct RiverTray {
        handle: AppHandle<RiverApp>,
        /// ARGB32, as the protocol wants.
        pixels: Vec<u8>,
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
                data: self.pixels.clone(),
            }]
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.handle.open();
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            vec![
                StandardItem {
                    label: "Open River".into(),
                    activate: Box::new(|t: &mut Self| t.handle.open()),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: "Quit".into(),
                    activate: Box::new(|t: &mut Self| t.handle.quit()),
                    ..Default::default()
                }
                .into(),
            ]
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
        // RGBA to ARGB.
        let pixels = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[3], p[0], p[1], p[2]])
            .collect();
        match (RiverTray {
            handle: handle.clone(),
            pixels,
        })
        .spawn()
        {
            Ok(_) => {
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

    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
    #[cfg(windows)]
    use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

    use super::*;
    use crate::background;

    thread_local! {
        /// The icon, kept alive on the thread that made it: dropping it takes it off the bar.
        static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    }

    /// Show the icon. Call on the thread that runs the window system's events.
    pub fn start(handle: &AppHandle<RiverApp>) -> bool {
        let Some(rgba) = icon_rgba() else {
            river_app::diag!("river: the tray icon could not be read");
            return false;
        };
        let icon = match Icon::from_rgba(rgba, SIDE as u32, SIDE as u32) {
            Ok(i) => i,
            Err(e) => {
                river_app::diag!("river: the tray icon could not be made: {e}");
                return false;
            }
        };
        let open = MenuItem::new("Open River", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        if let Err(e) = menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit]) {
            river_app::diag!("river: the tray menu could not be made: {e}");
            return false;
        }
        let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
        let on_menu = handle.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == open_id {
                on_menu.open();
            } else if event.id == quit_id {
                on_menu.quit();
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
                    on_click.open();
                }
            }));
        }
        let built = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(cfg!(target_os = "macos"))
            .with_tooltip(TOOLTIP)
            .with_icon(icon)
            .build();
        match built {
            Ok(tray) => {
                TRAY.with(|t| *t.borrow_mut() = Some(tray));
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
        TRAY.with(|t| *t.borrow_mut() = None);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub use linux::start;
#[cfg(any(windows, target_os = "macos"))]
pub use native::{start, stop};

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
}
