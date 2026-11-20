//! River's native UI, shared by every platform shell.
//!
//! A shell (today `apps/desktop`) supplies a [`NodeHost`] that knows how to find, install and
//! start Freenet on its platform; everything the user sees lives here.

pub mod address;
pub mod chat;
#[cfg(test)]
mod keys_ui_tests;
pub mod node;
#[cfg(test)]
mod node_ui_tests;
pub mod notify;
mod notify_actions;
pub mod rooms;
pub mod setup;
pub mod update;

/// Where the web UI's code, copied verbatim into `chat/visual_fold.rs`, finds its neighbours.
mod util {
    pub(crate) mod confusable {
        pub(crate) use crate::chat::visual_fold::comparison_folds;
    }
}

use rune::{AppHandle, AppSpec, Cx, El, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Kit};

use address::NodeChoice;

pub use chat::Chat;
pub use node::{InstallStep, MANUAL_INSTALL_URL, NodeHost, NodeProbe};
pub use rooms::{Access, RoomsUpdate};
pub use setup::{Controller, Setup};
pub use update::{
    UpdateCheck, UpdateController, UpdateEvent, UpdateHost, UpdateInfo, UpdateStatus, UpdateSupport,
};

pub struct RiverApp {
    pub setup: Setup,
    /// The chat screen, shown once setup reaches [`Setup::Connected`] and the rooms are read
    /// ([`Access::Ready`]). Example data in snapshots and tests ([`chat::Source::Example`]);
    /// the user's rooms from the node in the live app ([`chat::Source::Node`]).
    pub chat: Chat,
    /// Getting at the user's rooms once the node answers: the screens before the chat.
    pub access: Access,
    /// `None` in headless snapshots and tests, where nothing runs in the background.
    ctl: Option<Controller>,
    /// Reads the rooms from the node. `None` in headless snapshots and tests.
    link: Option<rooms::Link>,
    /// Checks for, downloads and installs new versions of River. `None` in headless snapshots
    /// and tests, and in shells without a self-update.
    updates: Option<UpdateController>,
    /// Where the "Update River automatically" choice is kept. In memory in tests.
    update_prefs: rune::prefs::Prefs,
    /// River's logo, rasterized once by [`load_logo`]. `None` until then, and in tests, which
    /// draw an empty square in its place.
    logo: Option<Logo>,
    /// Shows notifications and the unread count with the operating system. [`notify::Silent`]
    /// in snapshots, tests and shells without one.
    notifier: Box<dyn notify::Notifier>,
    /// Whether the window has focus. False until the first focus event, so a window that never
    /// reports one errs toward notifying.
    focused: bool,
    /// Whether a focus event has arrived. Until one has, the title treats the window as
    /// focused, so a focused window never shows an unread prefix it should not.
    focus_known: bool,
    /// Messages that arrived in the open room while the window was not focused. The room counts
    /// them as read the moment they land, so the title adds them back until the person returns.
    missed: (String, usize),
    /// What holds notifications back: cooldowns, the per-minute cap, rooms still filling.
    limiter: notify::Limiter,
    /// Whether do not disturb is kept in this device's preferences. Off in snapshots and tests,
    /// which must not write the real ones.
    dnd_persist: bool,
    /// An update notice do not disturb held back, shown once it is off (and the window is not
    /// in front, which counts as told).
    pending_notice: Option<(String, String)>,
    /// Whether a private room's notification may show the message text. Off unless the
    /// `notification_previews` preference is set (there is no switch for it yet).
    private_previews: bool,
    /// The unread count last given to the notifier, so it hears only changes.
    reported_unread: Option<usize>,
    /// The number of the node in use ([`rooms::Link::generation`]). What a node the app has left
    /// still delivers carries an older number and is dropped.
    node_generation: u64,
    /// A node the person typed, kept until it has answered as Freenet: a typo must not become
    /// the saved choice.
    pending_save: Option<NodeChoice>,
}

/// How often the running node's version is looked at, to notice that it updated itself.
const NODE_VERSION_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// The saved "Update River automatically" choice.
pub const UPDATE_AUTO_PREF: &str = "update_automatically";

/// The logo and the cache that keeps its pixels alive.
struct Logo {
    _cache: rune::image::ImageCache,
    image: rune::image::Image,
}

/// The web UI's logo, so both apps carry the same mark. It is shared: an edit to it changes
/// this app too, which is why `apps.yml` runs for that file.
const LOGO_SVG: &[u8] = include_bytes!("../../../ui/assets/river_logo.svg");
const LOGO_PATH: &str = "ui/assets/river_logo.svg";
/// The logo's largest drawn size in points (the setup card draws it at this size, the room
/// list smaller), and the scale it rasterizes at. Smaller draws use its mips.
const LOGO_PT: f32 = 36.0;
const LOGO_SCALE: f32 = 2.0;

/// `AppSpec::setup`: start rasterizing the logo on the image cache's worker. A headless run
/// (`--snap`) waits for it, so snapshots show it, and fails if it does not rasterize, so a
/// broken logo stops CI's render step instead of rendering as an empty square.
fn load_logo(s: &mut RiverApp, waker: rune::Waker) {
    let cache = rune::image::ImageCache::new(waker);
    let image = cache.svg(LOGO_SVG, LOGO_PT, LOGO_SCALE);
    if rune::is_headless()
        && let Err(e) = image.wait()
    {
        panic!("River's logo ({LOGO_PATH}) did not rasterize: {e}");
    }
    s.logo = Some(Logo {
        _cache: cache,
        image,
    });
}

/// Save one preference into this app's preferences file (none: nothing is saved). The file is
/// read again, merged with the key and written back atomically, so a key saved by another
/// part of the app since startup is kept. If that read fails (a sharing violation, say) it
/// writes nothing, because writing then would erase the other keys. (`Prefs::open`, used to
/// find the path, has already moved a file that does not parse to `prefs.json.bad`, so a
/// corrupt file is not met here.) There is no lock against another River process writing at
/// the same moment. The crate has no `tracing`, so failures go to stderr as elsewhere in it.
fn save_pref<T: serde::Serialize + ?Sized>(key: &str, value: &T) {
    let Some(path) = rune::prefs::Prefs::open("org.freenet.River")
        .path()
        .map(std::path::Path::to_path_buf)
    else {
        return;
    };
    if let Err(e) = merge_pref(&path, key, value) {
        eprintln!("River: couldn't save {key}: {e}");
    }
}

fn merge_pref<T: serde::Serialize + ?Sized>(
    path: &std::path::Path,
    key: &str,
    value: &T,
) -> Result<(), String> {
    use std::io::Write;
    let mut map: serde_json::Map<String, serde_json::Value> = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} is not readable as preferences: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(format!("couldn't read {}: {e}", path.display())),
    };
    map.insert(
        key.to_owned(),
        serde_json::to_value(value).map_err(|e| e.to_string())?,
    );
    let json = serde_json::to_vec_pretty(&map).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut tmp = path.as_os_str().to_owned();
    static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tmp.push(format!(".{}.{seq}.tmp", std::process::id()));
    let tmp = std::path::PathBuf::from(tmp);
    let done = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = done {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

impl RiverApp {
    /// A window with nothing running behind it, for snapshots and tests. It shows the example
    /// rooms.
    pub fn detached() -> Self {
        RiverApp {
            setup: Setup::Checking,
            chat: Chat::example(),
            access: Access::Example,
            ctl: None,
            link: None,
            updates: None,
            update_prefs: rune::prefs::Prefs::memory(),
            logo: None,
            notifier: Box::new(notify::Silent),
            focused: false,
            focus_known: false,
            missed: Default::default(),
            reported_unread: None,
            limiter: Default::default(),
            dnd_persist: false,
            pending_notice: None,
            private_previews: false,
            node_generation: 0,
            pending_save: None,
        }
    }

    /// The live app: starts checking for a node straight away, and reads the user's rooms
    /// through `link` once one answers.
    pub fn attached(ctl: Controller, link: rooms::Link) -> Self {
        let startup = address::Startup {
            choice: ctl.node(),
            notice: None,
            problem: None,
        };
        RiverApp::attached_at_startup(ctl, link, startup)
    }

    /// [`attached`](RiverApp::attached) with what resolving the node at start found: a problem
    /// (the app waits for the person to choose a node) or a notice.
    pub fn attached_at_startup(
        ctl: Controller,
        link: rooms::Link,
        startup: address::Startup,
    ) -> Self {
        let prefs = rune::prefs::Prefs::open("org.freenet.River");
        let node = startup.choice;
        address::set_current(&node.address);
        let waiting = startup.problem.is_some();
        if !waiting {
            ctl.check();
            ctl.watch_version(NODE_VERSION_EVERY);
        }
        let mut chat = Chat::new(
            chat::Source::Node(chat::LinkStatus::Loading),
            Vec::new(),
            0,
            0,
            0,
        );
        chat.dnd
            .load(prefs.get(chat::dnd::PREF).unwrap_or_default());
        chat.zoom.load(
            prefs
                .get(chat::zoom::PREF)
                .unwrap_or(i64::from(chat::zoom::DEFAULT)),
        );
        chat.node.choice = node;
        chat.node.notice = startup.notice;
        chat.node.problem = startup.problem;
        // What the user does in a room goes to the background work, which sends it.
        let sink = link.clone();
        chat.set_sink(Box::new(move |command| sink.send(command)));
        let link_generation = link.generation();
        RiverApp {
            setup: if waiting {
                Setup::Failed(String::new())
            } else {
                Setup::Checking
            },
            chat,
            access: Access::Connecting,
            ctl: Some(ctl),
            link: Some(link),
            updates: None,
            update_prefs: rune::prefs::Prefs::memory(),
            logo: None,
            notifier: Box::new(notify::Silent),
            focused: false,
            focus_known: false,
            missed: Default::default(),
            reported_unread: None,
            limiter: Default::default(),
            dnd_persist: true,
            pending_notice: None,
            private_previews: prefs.get("notification_previews").unwrap_or(false),
            node_generation: link_generation,
            pending_save: None,
        }
    }

    /// Show notifications and the unread count through `notifier`.
    pub fn with_notifier(mut self, notifier: Box<dyn notify::Notifier>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Keep River itself up to date through `updates`. "Update River automatically" is on
    /// unless the person turned it off.
    pub fn with_updates(mut self, updates: UpdateController) -> Self {
        self.update_prefs = rune::prefs::Prefs::open("org.freenet.River");
        let auto = self.update_prefs.get(UPDATE_AUTO_PREF).unwrap_or(true);
        let host = updates.host().clone();
        self.chat.update.version = host.version();
        self.chat.update.support = host.support();
        self.chat.update.auto = auto;
        updates.set_auto(auto);
        updates.startup();
        updates.start_schedule();
        self.updates = Some(updates);
        self
    }

    /// Take word from the update controller.
    pub fn apply_update(&mut self, event: UpdateEvent) {
        match event {
            UpdateEvent::Notice(text) => self.chat.update.notice = Some(text),
            UpdateEvent::Status(status) => {
                let notice = self.chat.update.apply(status);
                if notice.is_some() {
                    // A newer version replaces one held back.
                    self.pending_notice = None;
                }
                if let Some((title, body)) = notice
                    && !self.focused
                {
                    if self.dnd_allows(false) {
                        self.send_update_notice(title, body);
                    } else {
                        // `Ready` is sent once per download, so there may be no later status
                        // to announce it: keep it until do not disturb is off.
                        self.pending_notice = Some((title, body));
                    }
                }
            }
        }
    }

    /// Check for a newer River now (the Check now button).
    pub(crate) fn check_for_update(&mut self) {
        if let Some(u) = &self.updates {
            u.check_now();
        }
    }

    pub(crate) fn start_update_download(&mut self) {
        if let Some(u) = &self.updates {
            u.download();
        }
    }

    /// Restart into the downloaded version, asking first when a restart would lose something:
    /// the app exits without flushing, and messages waiting to be sent and text in the boxes
    /// live only in memory. The default of the question is to wait.
    pub(crate) fn restart_to_update(&mut self) {
        match self.chat.unsent_summary() {
            Some(what) => self.chat.update.confirm_restart = Some(what),
            None => self.restart_now(),
        }
    }

    /// Restart without asking.
    pub(crate) fn restart_now(&mut self) {
        self.chat.update.confirm_restart = None;
        self.chat.update.restart_error = None;
        if let Some(u) = &self.updates {
            u.restart();
        }
    }

    /// The "Update River automatically" choice, kept across runs.
    pub(crate) fn set_auto_update(&mut self, auto: bool) {
        self.chat.update.auto = auto;
        if let Some(u) = &self.updates {
            u.set_auto(auto);
            if auto {
                // Turning it on fetches what is already known to be available.
                u.check_now();
            }
        }
        if self.update_prefs.path().is_some() {
            save_pref(UPDATE_AUTO_PREF, &auto);
        } else {
            // In memory (tests, no config directory): nothing on disk to protect.
            let _ = self.update_prefs.set(UPDATE_AUTO_PREF, &auto);
        }
    }

    /// Offer "Background and startup" in the room list, backed by `host` (the desktop shell's
    /// tray icon and login item).
    pub fn with_background(mut self, host: std::sync::Arc<dyn chat::BackgroundHost>) -> Self {
        self.chat.background = Some(chat::BackgroundUi::new(host));
        self
    }

    /// The window gained or lost focus.
    pub fn focus_changed(&mut self, focused: bool) {
        self.focused = focused;
        self.focus_known = true;
        if focused {
            self.missed = Default::default();
            // The update block in front counts as told.
            self.pending_notice = None;
            self.drop_read_held();
        }
    }

    /// Open the room a notification was about. The shell calls this when it is clicked.
    pub fn open_room(&mut self, key: &str) {
        self.chat.open_room(key);
        self.limiter.retain_held(|k| k != key);
    }

    /// Forget held-back notifications for what has been read since: the open room while the
    /// window is focused, or any other room with nothing unread.
    fn drop_read_held(&mut self) {
        let (chat, focused) = (&self.chat, self.focused);
        let open = chat.room().map(|r| r.key.as_str());
        self.limiter.retain_held(|k| {
            // A room muted since its notification was held back has nothing to show.
            if chat.mode_of(k) == rooms::modes::NotificationMode::Muted {
                return false;
            }
            if open == Some(k) {
                !focused
            } else {
                chat.rooms()
                    .iter()
                    .any(|r| r.key == k && chat.unread_of(r) > 0)
            }
        });
    }

    /// Not a room's notification: nothing to open when it is clicked.
    fn send_update_notice(&mut self, title: String, body: String) {
        self.notifier.notify(notify::Notification {
            room_key: "river-update".into(),
            room_name: "River".into(),
            title,
            body,
            mention: false,
            count: 1,
            message_key: String::new(),
            actions: false,
        });
    }

    /// Show the update notice do not disturb held back, once, when it allows. A window in
    /// front shows the update block itself, which counts as told.
    fn flush_pending_notice(&mut self) {
        if self.pending_notice.is_some()
            && self.dnd_allows(false)
            && let Some((title, body)) = self.pending_notice.take()
            && !self.focused
        {
            self.send_update_notice(title, body);
        }
    }

    /// Whether do not disturb lets a notification through (`mention`: it is about a message
    /// that mentions you). Unread counts do not depend on this.
    fn dnd_allows(&mut self, mention: bool) -> bool {
        self.refresh_dnd_clock();
        self.chat.dnd.settings.allows(mention, self.chat.dnd.now)
    }

    fn refresh_dnd_clock(&mut self) {
        if !self.chat.dnd.pinned {
            self.chat.dnd.now = chat::dnd::local_minute();
        }
    }

    /// Keep do not disturb in step: the clock it is judged by, the times typed in its dialog,
    /// the saved copy, and a wake-up while a schedule is set so the indicator changes on time.
    fn sync_dnd(&mut self, tx: &mut Tx) {
        self.refresh_dnd_clock();
        self.flush_pending_notice();
        self.chat.dnd.sync_inputs();
        if let Some(settings) = self.chat.dnd.take_dirty()
            && self.dnd_persist
        {
            save_pref(chat::dnd::PREF, &settings);
        }
        self.chat.close_shortcuts_under_dialog();
        if let Some(percent) = self.chat.zoom.take_dirty()
            && self.dnd_persist
        {
            save_pref(chat::zoom::PREF, &percent);
        }
        let d = &self.chat.dnd;
        // The switch on means the schedule changes nothing, so nothing to wake for.
        if d.settings.schedule.is_some() && !d.settings.manual && !d.pinned {
            tx.wake_after(std::time::Duration::from_secs(30));
        }
    }

    /// What do not disturb lets through of `a`, if anything. Run before the limiter, so what
    /// is held back neither spends its budget nor comes back later.
    fn dnd_filter(&mut self, a: notify::Arrival) -> Option<notify::Arrival> {
        self.refresh_dnd_clock();
        self.chat.dnd.settings.filter(a, self.chat.dnd.now)
    }

    /// Apply do not disturb to what the limiter is holding back from before it came on.
    fn dnd_filter_held(&mut self) {
        self.refresh_dnd_clock();
        let (settings, now) = (self.chat.dnd.settings.clone(), self.chat.dnd.now);
        if settings.active(now) {
            self.limiter.filter_held(|a| settings.filter(a, now));
        }
    }

    /// Show what the limits let through.
    fn show(&mut self, admitted: Vec<notify::Arrival>, now: std::time::Instant) {
        for (n, from) in notify::compose_from(admitted, self.private_previews) {
            if !self.notifier.notify(n) {
                self.limiter.refund(from, now);
            }
        }
    }

    /// Show held-back notifications whose cooldown has ended, for a room that went quiet.
    pub(crate) fn flush_held(&mut self, now: std::time::Instant) {
        self.drop_read_held();
        self.flush_pending_notice();
        self.dnd_filter_held();
        let due = self.limiter.admit(Vec::new(), now);
        self.show(due, now);
    }

    /// Unread messages across all rooms, counting the open room's while the window was away.
    pub fn unread(&self) -> usize {
        self.chat.unread_total() + self.missed.1
    }

    /// Notify about `arrived`, unless the person is looking at that room right now.
    fn notify_arrivals(&mut self, arrived: Vec<notify::Arrival>, now: std::time::Instant) {
        self.limiter
            .observe(self.chat.rooms(), now, self.chat.list_is_full());
        let open = self.chat.room().map(|r| r.key.clone());
        let mut shown = Vec::new();
        for a in arrived {
            // What fills a room that just appeared is history: not news, not unread.
            if self.limiter.settling(&a.room_key, now) {
                continue;
            }
            let is_open = open.as_deref() == Some(a.room_key.as_str());
            if self.focused && is_open {
                continue;
            }
            if is_open {
                if self.missed.0 != a.room_key {
                    self.missed = (a.room_key.clone(), 0);
                }
                self.missed.1 += a.count;
            }
            if !a.quiet
                && let Some(a) = self.dnd_filter(a)
            {
                shown.push(a);
            }
        }
        self.drop_read_held();
        self.dnd_filter_held();
        let admitted = self.limiter.admit(shown, now);
        self.show(admitted, now);
    }

    /// The window title: the open room, and the unread count while the window is in the
    /// background (an unknown focus counts as focused).
    pub(crate) fn window_title(&self, unread: usize) -> String {
        notify::window_title(
            unread,
            self.chat.room().map(|r| r.name.as_str()),
            self.focused || !self.focus_known,
        )
    }

    /// Keep the window title and the notifier's unread count in step with the rooms. Runs at
    /// the start of every frame, so it sees every change however it came about.
    fn sync_unread(&mut self, tx: &mut Tx) {
        self.sync_node_status(tx);
        self.sync_dnd(tx);
        // Both run every frame, so neither is short-circuited away.
        self.chat.close_search_under_dialog();
        let (revealing, flashing) = (
            self.chat.apply_reveal(tx),
            self.chat.tick_flash(std::time::Instant::now()),
        );
        if revealing || flashing {
            tx.wake_after(std::time::Duration::from_millis(30));
        }
        if !matches!(self.chat.source, chat::Source::Node(_)) {
            return;
        }
        let now = std::time::Instant::now();
        self.flush_held(now);
        if let Some(wait) = self.limiter.next_due(now) {
            tx.wake_after(wait);
        }
        let unread = self.unread();
        tx.set_title(self.window_title(unread));
        if self.reported_unread != Some(unread) {
            self.reported_unread = Some(unread);
            self.notifier.set_unread(unread);
        }
    }

    /// Keep the node status dialog current while it is open. Does nothing, and schedules
    /// nothing, while it is closed.
    fn sync_node_status(&mut self, tx: &mut Tx) {
        if !self.chat.status.open {
            return;
        }
        // Unknown until the rooms are loaded: "0 rooms" while loading would be wrong.
        let rooms = match self.chat.source {
            chat::Source::Node(chat::LinkStatus::Loading) | chat::Source::Example => None,
            chat::Source::Node(link) => {
                let all = self.chat.rooms().len();
                let behind = self.chat.issues.not_following.len().min(all);
                let following = (link == chat::LinkStatus::Live).then_some(all - behind);
                Some((all, following))
            }
        };
        let status = &mut self.chat.status;
        status.connection =
            chat::node_status::connection_of(&self.setup, &self.access, self.chat.source);
        status.address = self.chat.node.choice.address.authority();
        status.remote = !self.chat.node.choice.address.is_loopback();
        status.rooms = rooms;
        if let Some(ctl) = &self.ctl {
            status.latest_enabled = ctl.looks_up_latest();
            if status.should_refresh(std::time::Instant::now()) {
                status.refresh_started();
                ctl.probe_status();
            }
            if status.wants_latest() {
                status.latest_started();
                ctl.fetch_latest();
            }
        }
        tx.wake_after(chat::node_status::REFRESH_EVERY);
    }

    /// Take a new setup state. Reaching a node starts reading the rooms.
    pub fn set_setup(&mut self, setup: Setup) {
        let connected = matches!(setup, Setup::Connected { .. });
        let was = match &self.setup {
            Setup::Connected { version } => Some(version.clone()),
            _ => None,
        };
        if let Setup::Failed(why) = &setup {
            let at = self.chat.node.choice.address.authority();
            self.chat.status.note(
                chat::node_status::ErrorKind::SetupFailed,
                &at,
                chat::node_status::now_secs(),
                why,
            );
        }
        if let Setup::Connected { version } = &setup {
            // The node updates itself. When its version changes under the open app, say so.
            if let Some(old) = was.as_ref().filter(|old| *old != version) {
                self.chat.update.node_note = Some(format!(
                    "Freenet updated itself from {old} to {version} while River was open."
                ));
            }
            self.chat.update.node_version = Some(version.clone());
        }
        // The first connection to the node is the proof that a just-installed version works.
        if connected
            && was.is_none()
            && let Some(u) = &self.updates
        {
            u.host().confirm_started();
        }
        self.setup = setup;
        // The node answered as Freenet: now the address is worth remembering.
        if connected
            && let Some(choice) = self.pending_save.take()
            && choice == self.chat.node.choice
            && let Some(ctl) = &self.ctl
            && let Err(e) = ctl.save_node(&choice)
        {
            // Used for this run; chosen again next time.
            eprintln!("River: {e}");
        }
        // A change of the node's version is not a new connection: the rooms link already
        // follows the node through its restart.
        if connected
            && was.is_none()
            && let Some(link) = &self.link
        {
            link.start();
        }
    }

    /// Take an update from the rooms link. The approval page is NOT opened here: the person
    /// presses a button for it, so they are looking at the app when the browser window
    /// appears (an automatic open can land on another workspace or fail unseen).
    pub fn apply_rooms_from(&mut self, update: RoomsUpdate, generation: u64, tx: &mut Tx) {
        // Read under a node the app has since left: it says nothing about this one.
        if generation == self.node_generation {
            self.apply_rooms(update, tx);
        }
    }

    pub fn apply_rooms(&mut self, update: RoomsUpdate, tx: &mut Tx) {
        match update {
            RoomsUpdate::Access(access) => {
                let at = self.chat.node.choice.address.authority();
                self.chat
                    .status
                    .note_access(&access, &at, chat::node_status::now_secs());
                self.access = access;
            }
            RoomsUpdate::Rooms {
                rooms,
                open,
                modes,
                settled,
                status,
                now,
                issues,
                work,
            } => {
                let at = self.chat.node.choice.address.authority();
                self.chat
                    .status
                    .note_link(status, &at, chat::node_status::now_secs());
                let was_stale = self.chat.issues.update_required;
                self.chat.issues = issues;
                // River moved on and this build stopped writing: look for the update now,
                // rather than at the next scheduled check.
                if !was_stale && self.chat.issues.update_required {
                    self.check_for_update();
                }
                // Before the rooms, so what arrives in them is judged by their modes.
                self.chat.apply_modes(modes, &settled);
                let arrived = self
                    .chat
                    .show_node_rooms(rooms, open.as_deref(), status, now);
                self.chat.show_work(work);
                self.notify_arrivals(arrived, std::time::Instant::now());
                // Also here: `poll` is not run for an update that arrives while the window is
                // hidden, and the title and count must still follow.
                self.sync_unread(tx);
            }
            RoomsUpdate::Status(status) => {
                let at = self.chat.node.choice.address.authority();
                self.chat
                    .status
                    .note_link(status, &at, chat::node_status::now_secs());
                self.chat.note_status(status);
            }
            // The link is copied as the web UI's invite dialog copies it: to the clipboard as
            // it is. It is a bearer credential, which the members panel says.
            RoomsUpdate::Invite { room, link } => {
                if let Ok(l) = &link {
                    tx.copy(l.clone());
                }
                self.chat.invitation_made(room, link);
            }
            // Not copied: the user presses Copy in the dialog, as in the web UI.
            RoomsUpdate::Export { room, token } => {
                self.chat.identity_exported(room, token);
            }
        }
    }

    fn retry_rooms(&mut self) {
        if let Some(link) = &self.link {
            link.start();
        }
    }

    fn can_install(&self) -> bool {
        self.ctl.as_ref().is_none_or(Controller::can_install)
    }

    /// Whether the rooms are on screen, so a change of node would replace them.
    fn rooms_shown(&self) -> bool {
        matches!(self.setup, Setup::Connected { .. })
            && matches!(self.access, Access::Example | Access::Ready)
    }

    /// Open the form that picks the node to use.
    pub fn node_open(&mut self) {
        self.chat.node.open();
    }

    /// Close the form without changing anything.
    pub fn node_cancel(&mut self) {
        self.chat.node.close();
    }

    /// Connect was pressed: check the address, and ask first when something needs saying.
    pub fn node_submit(&mut self) {
        let shown = self.rooms_shown();
        if let Some(address) = self.chat.node.submit(shown) {
            self.apply_node(NodeChoice::saved(address));
        }
    }

    /// The person confirmed the warning.
    pub fn node_confirm(&mut self) {
        if let Some(address) = self.chat.node.confirm() {
            self.apply_node(NodeChoice::saved(address));
        }
    }

    /// Leave the problem panel for the Freenet on this computer.
    pub fn node_dismiss_problem(&mut self) {
        self.chat.node.problem = None;
        self.retry();
    }

    /// Go back to the Freenet on this computer.
    pub fn node_use_local(&mut self) {
        self.apply_node(NodeChoice::local());
    }

    /// Use `choice` from now on: remember it, drop what was read from the previous node, and
    /// check the new one, which then goes through the same approval as any node. Nothing here
    /// starts, stops or installs a Freenet on this computer.
    pub(crate) fn apply_node(&mut self, choice: NodeChoice) {
        let current = self.chat.node.choice.clone();
        self.chat.node.close();
        if choice == current {
            return;
        }
        if let Some(ctl) = &self.ctl {
            if choice.is_existing_node() {
                // Saved once the node has answered as Freenet (see `set_setup`).
                self.pending_save = Some(choice.clone());
            } else {
                self.pending_save = None;
                if let Err(e) = ctl.save_node(&choice) {
                    eprintln!("River: {e}");
                }
            }
            ctl.set_node(choice.clone());
        }
        let authority = choice.address.authority();
        address::set_current(&choice.address);
        self.node_generation = match &self.link {
            Some(link) => link.set_node(&authority, choice.is_existing_node()),
            None => self.node_generation + 1,
        };
        // The rooms, their read marks and modes, and the open room are the previous node's.
        let mut chat = Chat::new(
            chat::Source::Node(chat::LinkStatus::Loading),
            Vec::new(),
            0,
            0,
            0,
        );
        chat.node.choice = choice;
        if let Some(link) = &self.link {
            let sink = link.clone();
            chat.set_sink(Box::new(move |command| sink.send(command)));
        }
        self.chat = chat;
        self.access = Access::Connecting;
        self.setup = Setup::Checking;
        self.missed = Default::default();
        self.limiter = Default::default();
        self.reported_unread = None;
        if let Some(ctl) = &self.ctl {
            ctl.check();
        }
    }

    fn install(&mut self) {
        if let Some(ctl) = &self.ctl {
            ctl.install();
        }
    }

    fn retry(&mut self) {
        if let Some(ctl) = &self.ctl {
            ctl.check();
        }
    }
}

/// Your own member info dialog in the first example room, shown as a room from the node (so
/// saving sends a command), after `edit` set the field and the delivery state.
fn nickname_scene(s: &mut RiverApp, edit: fn(&mut Chat, &mut chat::model::Room)) {
    connect(s);
    let mut rooms = chat::example::data().rooms;
    let uid = {
        let room = &rooms[0];
        room.member(room.me)
            .map(|m| m.uid.clone())
            .unwrap_or_default()
    };
    s.chat.source = chat::Source::Node(chat::LinkStatus::Live);
    s.chat
        .show_node_rooms(rooms.clone(), None, chat::LinkStatus::Live, 29_000_000);
    s.chat.open_member(&uid);
    edit(&mut s.chat, &mut rooms[0]);
    let (own, block) = (
        rooms[0].own_nickname.clone(),
        rooms[0].nickname_block.clone(),
    );
    s.chat.edit_rooms(|r| {
        r[0].own_nickname = own;
        r[0].nickname_block = block;
    });
}

/// The member dialog of `uid` in the example room `room`, shown as a room from the node (so
/// the deputy buttons send commands), after `room_edit` changed the room and `chat_edit` the chat.
fn deputy_scene(
    s: &mut RiverApp,
    room: &str,
    uid: &str,
    room_edit: fn(&mut chat::model::Room),
    chat_edit: fn(&mut Chat),
) {
    connect(s);
    let mut rooms = chat::example::data().rooms;
    s.chat.source = chat::Source::Node(chat::LinkStatus::Live);
    if let Some(r) = rooms.iter_mut().find(|r| r.key == room) {
        room_edit(r);
    }
    s.chat
        .show_node_rooms(rooms, None, chat::LinkStatus::Live, 29_000_000);
    s.chat.select(room);
    s.chat.open_member(uid);
    chat_edit(&mut s.chat);
}

/// Offer Ban for Priya (27) in the example room.
fn offer_ban(r: &mut chat::model::Room) {
    r.ban_offers
        .insert("27".into(), chat::model::BanOffer::Allowed);
    r.ban_preview.insert("27".into(), Default::default());
}

/// Offer Ban for Jonas (30) in the example room.
fn offer_ban_30(r: &mut chat::model::Room) {
    r.ban_offers
        .insert("30".into(), chat::model::BanOffer::Allowed);
    r.ban_preview.insert(
        "30".into(),
        chat::model::BanPreview {
            lifts: vec!["Priya Raman".into()],
            refused: false,
        },
    );
}

/// One of your own pending changes for the deputy scenes.
fn deputy_change(
    uid: &str,
    grant: bool,
    failed: Option<&str>,
    retry: bool,
) -> chat::model::DeputyChange {
    chat::model::DeputyChange {
        uid: uid.into(),
        grant,
        pending: chat::model::Pending {
            entry: 1,
            failed: failed.map(str::to_owned),
            retry,
        },
    }
}

/// An identity token for the scenes, made from fixed throwaway keys.
fn scene_token() -> String {
    use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
    let owner = ed25519_dalek::SigningKey::from_bytes(&[3; 32]);
    let member = ed25519_dalek::SigningKey::from_bytes(&[70; 32]);
    let owner_id = MemberId::from(&owner.verifying_key());
    river_core::room_state::identity::IdentityExport {
        room_owner: owner.verifying_key(),
        signing_key: member.clone(),
        authorized_member: AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member.verifying_key(),
            },
            &owner,
        ),
        invite_chain: vec![],
        member_info: None,
        room_name: Some("Town Square".into()),
        self_nickname: Some("Bea".into()),
        invitation_secrets: Default::default(),
    }
    .to_armored_string()
}

/// An invite link for the scenes, from fixed throwaway keys: the room owned by seed 3, invited
/// by its owner.
fn scene_invite_link() -> String {
    use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
    let owner = ed25519_dalek::SigningKey::from_bytes(&[3; 32]);
    let invitee = ed25519_dalek::SigningKey::from_bytes(&[71; 32]);
    let owner_id = MemberId::from(&owner.verifying_key());
    let invitation = river_core::invitation::Invitation {
        room: owner.verifying_key(),
        invitee: AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: invitee.verifying_key(),
            },
            &owner,
        ),
        invitee_signing_key: invitee,
        room_secrets: Vec::new(),
    };
    format!(
        "http://127.0.0.1:7509/v1/contract/web/river/?invitation={}",
        invitation.to_encoded_string().unwrap_or_default()
    )
}

/// A shell's answers for the "Background and startup" scenes: a tray, or none.
struct PreviewBackground {
    tray: bool,
}

impl chat::BackgroundHost for PreviewBackground {
    fn tray_available(&self) -> bool {
        self.tray
    }
    fn close_to_tray(&self) -> bool {
        true
    }
    fn set_close_to_tray(&self, _: bool) -> Result<(), String> {
        Ok(())
    }
    fn start_at_login(&self) -> bool {
        false
    }
    fn set_start_at_login(&self, _: bool) -> Result<(), String> {
        Ok(())
    }
}

/// The chat screen with "Node status" open, the link in `link`, after `edit` set what the node
/// answered and what went wrong.
fn node_status_scene(
    s: &mut RiverApp,
    link: chat::LinkStatus,
    edit: fn(&mut chat::node_status::NodeStatusUi, &str),
) {
    identity_scene(s, |_| {});
    s.chat.source = chat::Source::Node(link);
    s.access = Access::Ready;
    let at = s.chat.node.choice.address.authority();
    edit(&mut s.chat.status, &at);
    s.chat.status.open();
}

/// The node answered with `version`.
fn node_answered(ui: &mut chat::node_status::NodeStatusUi, at: &str, version: &str) {
    ui.apply(
        at,
        chat::node_status::StatusEvent::Reach {
            authority: at.to_owned(),
            reach: chat::node_status::Reach::Answered {
                version: version.into(),
            },
        },
        std::time::Instant::now(),
    );
}

/// The chat screen with "Background and startup" open, for a desktop with or without a tray.
fn background_scene(s: &mut RiverApp, tray: bool) {
    identity_scene(s, |_| {});
    s.chat.background = Some(chat::BackgroundUi::new(std::sync::Arc::new(
        PreviewBackground { tray },
    )));
    s.chat.open_background();
}

/// The chat screen with a scheduled do not disturb, at 23:00 (inside the 22:00 to 07:00
/// window), with its dialog open or, for `closed`, just the room list's indicator.
fn dnd_scene(s: &mut RiverApp, closed: bool) {
    identity_scene(s, |_| {});
    let d = &mut s.chat.dnd;
    d.pinned = true;
    d.now = 23 * 60;
    d.settings.schedule = Some(chat::DndSchedule {
        start: 22 * 60,
        end: 7 * 60,
    });
    d.settings.allow_mentions = !closed;
    if !closed {
        s.chat.open_dnd();
    }
}

/// The chat screen for the identity scenes: the example rooms as rooms from the node, so the
/// export and import buttons are live.
fn identity_scene(s: &mut RiverApp, edit: fn(&mut Chat)) {
    connect(s);
    let rooms = chat::example::data().rooms;
    s.chat.source = chat::Source::Node(chat::LinkStatus::Live);
    s.chat
        .show_node_rooms(rooms, None, chat::LinkStatus::Live, 29_000_000);
    edit(&mut s.chat);
}

/// A long description for the details scenes: paragraphs, links and a list.
const LONG_DESCRIPTION: &str = "Welcome to the **Book Club**. We read one book a month and talk about it on the last Sunday, see [the schedule](https://freenet.org/schedule).\n\nHow it works:\n\n- Pick up the book from [the library](https://freenet.org/library) or any shop\n- Post your thoughts here as you read, spoilers marked\n- Vote for the next book in the poll at the start of each month\n\nBe kind and read the [rules](https://freenet.org/rules) before you post.";

/// The room details dialog open on the example room `room`, shown as a room from the node (so
/// Save sends a command), after `edit` set the room and what is typed.
fn details_scene(s: &mut RiverApp, room: &str, edit: fn(&mut Chat, &mut chat::model::Room)) {
    connect(s);
    let mut rooms = chat::example::data().rooms;
    s.chat.source = chat::Source::Node(chat::LinkStatus::Live);
    let i = rooms.iter().position(|r| r.key == room).unwrap_or(0);
    let mut shown = rooms[i].clone();
    // Edited rooms are given to the chat before the dialog opens, so the form starts from them.
    shown.description = match room {
        "town-square" => "The **public** room for everyone using [Freenet](https://freenet.org).\n\n- Be kind\n- Read the [rules](https://freenet.org/rules)\n\nSee `#help` for questions.".into(),
        _ => shown.description,
    };
    shown.details.description_source = shown.description.clone();
    rooms[i] = shown;
    s.chat
        .show_node_rooms(rooms.clone(), None, chat::LinkStatus::Live, 29_000_000);
    s.chat.select(room);
    s.chat.open_details();
    let mut shown = rooms.swap_remove(i);
    edit(&mut s.chat, &mut shown);
    let pending = shown.details.pending.clone();
    let rotation = shown.details.rotation.clone();
    let editable = shown.details.editable.clone();
    s.chat.edit_rooms(|r| {
        if let Some(r) = r.iter_mut().find(|r| r.key == room) {
            r.details.rotation = rotation;
            r.details.pending = pending;
            r.details.editable = editable;
        }
    });
}

/// River pointed at the node at `address`, as if the person had chosen it.
fn use_node(s: &mut RiverApp, address: &str) {
    s.chat.node.choice =
        NodeChoice::saved(address::NodeAddress::parse(address).expect("an address"));
}

/// The first-run form for a node the person already has, with `typed` entered and Connect
/// pressed.
fn node_form_scene(s: &mut RiverApp, typed: &str) {
    s.setup = Setup::NotInstalled;
    s.chat.node.open();
    if let Some(form) = s.chat.node.form.as_mut() {
        form.input.replace_text(typed);
    }
    if !typed.is_empty() {
        s.chat.node.submit(false);
    }
}

/// The rooms from a node the person chose, with the form that changes it open and `typed`
/// entered and Connect pressed.
fn node_change_scene(s: &mut RiverApp, current: Option<&str>, typed: Option<&str>) {
    connect(s);
    s.chat.source = chat::Source::Node(chat::LinkStatus::Live);
    s.chat.show_node_rooms(
        chat::example::data().rooms,
        None,
        chat::LinkStatus::Live,
        29_000_000,
    );
    if let Some(a) = current {
        use_node(s, a);
    }
    s.chat.node.open();
    if let (Some(t), Some(form)) = (typed, s.chat.node.form.as_mut()) {
        form.input.replace_text(t);
        s.chat.node.submit(true);
    }
}

/// The overview scenes: the example rooms with unread messages, a mention in Rust Help and
/// a muted room that is left out.
fn overview_scene(s: &mut RiverApp, caught_up: bool) {
    use rooms::modes::NotificationMode;
    connect(s);
    s.chat.edit_rooms(|rooms| {
        for r in rooms {
            r.unread = match r.key.as_str() {
                "design-crew" => 3,
                "rust-help" => 2,
                "family" => 4,
                _ => 0,
            };
            if r.key == "rust-help"
                && let Some(m) = r.messages.iter_mut().rev().find(|m| m.author != r.me)
            {
                m.md = format!("{} [@Sam]({}:self)", m.text, chat::model::mention_scheme());
            }
        }
    });
    s.chat.set_mode("family", NotificationMode::Muted);
    if caught_up {
        s.chat.mark_all_read();
    }
    s.chat.toggle_overview();
}

/// The search panel open on the open room with `query` typed, after `extra` more messages that
/// all contain "release" have been added to it.
fn search_scene(s: &mut RiverApp, query: &str, extra: usize) {
    connect(s);
    if extra > 0 {
        s.chat.edit_rooms(|rooms| {
            let Some(r) = rooms.iter_mut().find(|r| r.key == "town-square") else {
                return;
            };
            let template = r.messages.last().expect("a message").clone();
            for i in 0..extra {
                let mut m = template.clone();
                m.key = format!("extra-{i}");
                m.id += i as u32 + 1;
                m.time += i as u32 + 1;
                m.text = format!("Release candidate {i} is ready for testing, please try it.");
                m.md = m.text.clone();
                m.reactions.clear();
                r.messages.push(m);
            }
        });
    }
    s.chat.search_chord();
    if let Some(search) = s.chat.search.as_mut() {
        search.query.replace_text(query);
    }
}

/// A named state for `--snap` and `--list-scenes`: a name and how it changes the initial state.
pub type Scene = (&'static str, fn(&mut RiverApp));

/// One scene per screen. CI renders every one of them.
pub const SCENES: &[Scene] = &[
    ("checking", |s| s.setup = Setup::Checking),
    ("not-installed", |s| s.setup = Setup::NotInstalled),
    ("finding-release", |s| {
        s.setup = Setup::Installing(InstallStep::FindingRelease)
    }),
    ("downloading", |s| {
        s.setup = Setup::Installing(InstallStep::Downloading {
            received: 9_400_000,
            total: Some(23_372_438),
        })
    }),
    ("verifying", |s| {
        s.setup = Setup::Installing(InstallStep::Verifying)
    }),
    ("installing", |s| {
        s.setup = Setup::Installing(InstallStep::Installing)
    }),
    ("waiting-for-installer", |s| {
        s.setup = Setup::Installing(InstallStep::WaitingForInstaller)
    }),
    ("starting", |s| s.setup = Setup::Starting),
    ("failed", |s| {
        s.setup = Setup::Failed("Couldn't install Freenet: the download was interrupted.".into())
    }),
    // Connected: the chat screen, on example data.
    ("chat", connect),
    ("chat-update-available", |s| {
        connect(s);
        update_scene(s, false, UpdateStatus::Available(example_update()));
    }),
    ("chat-update-downloading", |s| {
        connect(s);
        update_scene(
            s,
            true,
            UpdateStatus::Downloading {
                version: "0.2.0".into(),
                received: 6_200_000,
                total: 14_800_000,
            },
        );
    }),
    ("chat-update-ready", |s| {
        connect(s);
        update_scene(
            s,
            true,
            UpdateStatus::Ready {
                version: "0.2.0".into(),
            },
        );
    }),
    ("chat-update-restart-confirm", |s| {
        connect(s);
        update_scene(
            s,
            true,
            UpdateStatus::Ready {
                version: "0.2.0".into(),
            },
        );
        s.chat.update.confirm_restart = Some(
            "2 messages or changes are still being sent. You have text you have not sent.".into(),
        );
    }),
    ("chat-update-restart-failed", |s| {
        connect(s);
        update_scene(s, true, UpdateStatus::UpToDate);
        s.chat.update.staged = Some("0.2.0".into());
        s.chat.update.restart_error = Some(
            "River 0.2.0 did not start (the new River exited at once), so River went back to \
             0.1.0."
                .into(),
        );
    }),
    ("chat-update-rolled-back", |s| {
        connect(s);
        update_scene(s, true, UpdateStatus::Idle);
        s.chat.update.notice = Some(
            "River 0.2.0 did not start (the new River exited at once), so River went back to \
             0.1.0."
                .into(),
        );
    }),
    ("chat-node-status", |s| {
        node_status_scene(s, chat::LinkStatus::Live, |ui, at| {
            node_answered(ui, at, "0.2.158");
            ui.latest = chat::node_status::Latest::Known("0.2.160".into());
        })
    }),
    ("chat-node-status-reconnecting", |s| {
        node_status_scene(s, chat::LinkStatus::Reconnecting, |ui, at| {
            node_answered(ui, at, "0.2.160");
            ui.latest = chat::node_status::Latest::Known("0.2.160".into());
            ui.note(
                chat::node_status::ErrorKind::ConnectionLost,
                at,
                1_795_000_000,
                "",
            );
        })
    }),
    ("chat-node-status-old-node", |s| {
        node_status_scene(s, chat::LinkStatus::Live, |ui, at| {
            node_answered(ui, at, "0.2.155");
            ui.latest = chat::node_status::Latest::Unavailable;
        })
    }),
    ("chat-node-status-remote", |s| {
        use_node(s, "192.168.1.20:7509");
        node_status_scene(s, chat::LinkStatus::Live, |ui, at| {
            node_answered(ui, at, "0.2.159");
            ui.latest = chat::node_status::Latest::Fetching;
        })
    }),
    ("rooms-waiting-for-approval-node-status", |s| {
        connect(s);
        s.access = Access::WaitingForApproval {
            approve_url: "http://127.0.0.1:7509/permission/approve".into(),
        };
        s.chat.status.latest = chat::node_status::Latest::Known("0.2.160".into());
        s.chat.status.open();
    }),
    ("rooms-unreachable-node-status", |s| {
        connect(s);
        s.access = Access::Unreachable("Connection refused".into());
        let at = s.chat.node.choice.address.authority();
        s.chat.status.note_access(&s.access, &at, 1_795_000_000);
        s.chat.status.open();
    }),
    ("chat-settings", |s| {
        connect(s);
        update_scene(s, true, UpdateStatus::UpToDate);
        s.chat.update.settings_open = true;
    }),
    ("chat-settings-ready", |s| {
        connect(s);
        update_scene(
            s,
            true,
            UpdateStatus::Ready {
                version: "0.2.0".into(),
            },
        );
        s.chat.update.settings_open = true;
        s.chat.update.node_note =
            Some("Freenet updated itself from 0.2.158 to 0.2.160 while River was open.".into());
    }),
    ("chat-settings-package-managed", |s| {
        connect(s);
        update_scene(s, true, UpdateStatus::Available(example_update()));
        s.chat.update.support = UpdateSupport::External(
            "River was installed by your package manager, which updates it.".into(),
        );
        s.chat.update.settings_open = true;
    }),
    ("chat-settings-update-failed", |s| {
        connect(s);
        update_scene(
            s,
            true,
            UpdateStatus::Failed("the update manifest's signature is not valid".into()),
        );
        s.chat.update.settings_open = true;
    }),
    ("chat-members-hidden", |s| {
        connect(s);
        s.chat.members_open = false;
    }),
    ("chat-private-room", |s| {
        connect(s);
        s.chat.select(chat::example::PRIVATE_ROOM);
    }),
    ("chat-mention-popup", |s| {
        connect(s);
        s.chat.composer.replace_text("Thanks for the review @");
        s.chat.composer.focused = true;
    }),
    // `:shortcode:` autocomplete: the emoji whose name contains what follows the colon.
    ("chat-shortcode-popup", |s| {
        connect(s);
        s.chat.composer.replace_text("Great news :sm");
        s.chat.composer.focused = true;
    }),
    // Zoom: 200% and 80% of the window, and 200% in the narrowest window the app allows
    // (render that one with `--size 640x480`).
    ("chat-zoom-200", |s| {
        connect(s);
        s.chat.zoom.load(200);
    }),
    ("chat-zoom-80", |s| {
        connect(s);
        s.chat.zoom.load(80);
    }),
    // The keyboard shortcuts page.
    ("chat-shortcuts", |s| {
        connect(s);
        s.chat.open_shortcuts();
    }),
    ("chat-shortcuts-zoom-200", |s| {
        connect(s);
        s.chat.zoom.load(200);
        s.chat.open_shortcuts();
    }),
    ("chat-composing", |s| {
        connect(s);
        s.chat
            .composer
            .replace_text("Sounds good, I'll bring the slides on Thursday");
        s.chat.composer.focused = true;
    }),
    // The byte counter, from 80% of the room's limit (example rooms take 1000 bytes), and the
    // refusal over it.
    ("chat-near-limit", |s| {
        connect(s);
        s.chat
            .composer
            .replace_text("Sounds good, I'll bring the slides. ".repeat(23));
        s.chat.composer.focused = true;
    }),
    ("chat-over-limit", |s| {
        connect(s);
        s.chat
            .composer
            .replace_text("Sounds good, I'll bring the slides. ".repeat(29));
        s.chat.composer.focused = true;
    }),
    // Scrolled away from the newest message, so Latest shows. (The scene states it: the
    // history reports where it stands only once it is drawn.)
    ("chat-latest", |s| {
        connect(s);
        s.chat.at_bottom = false;
    }),
    ("chat-empty-room", |s| {
        connect(s);
        s.chat.select(chat::example::EMPTY_ROOM);
    }),
    // Scenes named `*narrow*` are rendered at a narrow size (`--size 820x720`, see
    // `.github/workflows/apps.yml`): the layout follows the window's width, which a scene
    // cannot set. At the default size they look like the wide layout.
    ("chat-narrow", connect),
    ("chat-narrow-members", |s| {
        connect(s);
        s.chat.members_overlay = true;
    }),
    // Rooms read from the node: none yet, and a lost connection.
    ("chat-no-rooms", |s| {
        connect(s);
        s.chat = Chat::new(
            chat::Source::Node(chat::LinkStatus::Live),
            Vec::new(),
            0,
            0,
            0,
        );
    }),
    // The connection dot and its words, for each state of the link.
    ("chat-connection-live", |s| {
        connect(s);
        s.chat.source = chat::Source::Node(chat::LinkStatus::Live);
    }),
    ("chat-connection-connecting", |s| {
        connect(s);
        s.chat.source = chat::Source::Node(chat::LinkStatus::Loading);
    }),
    ("chat-connection-lost", |s| {
        connect(s);
        s.chat.source = chat::Source::Node(chat::LinkStatus::Reconnecting);
    }),
    // A reply typed into a notification that could not be sent: said above the composer, and
    // the text is kept in the room's draft.
    ("chat-reply-not-sent", |s| {
        connect(s);
        s.chat.source = chat::Source::Node(chat::LinkStatus::Live);
        let _ = s.chat.send_reply("gone", "see you at six");
    }),
    // River has moved to a newer chat delegate or room contract than this build carries: the
    // rooms show, and nothing can be written.
    ("chat-update-required", |s| {
        connect(s);
        s.chat.source = chat::Source::Node(chat::LinkStatus::Live);
        s.chat.issues.update_required = true;
        let block = rooms::generation::UPDATE_REQUIRED_SEND;
        let rooms: Vec<_> = chat::example::data()
            .rooms
            .into_iter()
            .map(|mut r| {
                r.send_block = Some(block.into());
                r
            })
            .collect();
        s.chat
            .show_node_rooms(rooms, None, chat::LinkStatus::Live, 29_000_000);
    }),
    // A node the person already has: the form that picks it at first run (empty, with an address
    // the form refuses, and with one that is not on this computer, which asks first), a node that
    // did not answer, the approval step through a tunnel, and the rooms screen naming the node
    // with the form that changes it.
    ("not-installed-node-form", |s| node_form_scene(s, "")),
    ("not-installed-node-form-error", |s| {
        node_form_scene(s, "https://my-server.example:7509/")
    }),
    ("not-installed-node-warning", |s| {
        node_form_scene(s, "192.168.1.20:7509")
    }),
    ("checking-node", |s| {
        use_node(s, "localhost:40302");
        s.setup = Setup::Checking;
    }),
    ("failed-node-unreachable", |s| {
        use_node(s, "localhost:40302");
        s.setup = Setup::Failed(
            "Nothing answered at localhost:40302. Check that the node is running there and \
             that the address and port are right. If you reach it through an SSH tunnel or a \
             VPN, check that it is up."
                .into(),
        );
    }),
    ("rooms-waiting-for-approval-node", |s| {
        connect(s);
        use_node(s, "localhost:40302");
        s.access = Access::WaitingForApproval {
            approve_url: "http://localhost:40302/v1/app-grants/approve/example".into(),
        };
    }),
    ("rooms-unreachable-node", |s| {
        connect(s);
        use_node(s, "localhost:40302");
        s.access = Access::Unreachable("Connection refused".into());
    }),
    // A node that is not on this computer, said at every start however it was chosen; a bad
    // `--node` said on screen; a saved address that is not valid.
    ("chat-node-remote-banner", |s| {
        node_change_scene(s, Some("home.example.org:7509"), None);
        s.chat.node.close();
    }),
    ("checking-node-remote-banner", |s| {
        use_node(s, "home.example.org:7509");
        s.setup = Setup::Checking;
    }),
    ("failed-node-problem", |s| {
        s.setup = Setup::Failed(String::new());
        s.chat.node.problem = Some(
            "--node \"a@b\" isn't a node address: Enter just the host and port. A user name or \
             password isn't used."
                .into(),
        );
    }),
    ("chat-node-notice", |s| {
        node_change_scene(s, None, None);
        s.chat.node.close();
        s.chat.node.notice = Some(
            "Your saved node address http://x/ is not valid; using this computer's Freenet.".into(),
        );
    }),
    ("chat-node-entry", |s| {
        node_change_scene(s, Some("localhost:40302"), None);
        s.chat.node.close();
    }),
    ("chat-node-change", |s| {
        node_change_scene(s, Some("localhost:40302"), None)
    }),
    ("chat-node-change-error", |s| {
        node_change_scene(s, None, Some("my server:7509"))
    }),
    ("chat-node-change-confirm", |s| {
        node_change_scene(s, None, Some("localhost:40302"))
    }),
    ("chat-node-change-confirm-remote", |s| {
        node_change_scene(s, None, Some("home.example.org:7509"))
    }),
    // Getting at the rooms, once a node answered.
    ("rooms-connecting", |s| {
        connect(s);
        s.access = Access::Connecting;
    }),
    ("rooms-waiting-for-approval", |s| {
        connect(s);
        s.access = Access::WaitingForApproval {
            approve_url: "http://127.0.0.1:7509/v1/app-grants/approve/example".into(),
        };
    }),
    ("rooms-denied", |s| {
        connect(s);
        s.access = Access::NotApproved { expired: false };
    }),
    ("rooms-expired", |s| {
        connect(s);
        s.access = Access::NotApproved { expired: true };
    }),
    ("rooms-unsupported", |s| {
        connect(s);
        s.access = Access::Unsupported;
    }),
    ("rooms-refused", |s| {
        connect(s);
        s.access = Access::Refused(
            "Freenet won't let River use its permission over this connection. River has to \
             reach Freenet directly on this computer, at 127.0.0.1:7509."
                .into(),
        );
    }),
    ("rooms-unreachable", |s| {
        connect(s);
        s.access = Access::Unreachable("Connection refused".into());
    }),
    ("rooms-permission-removed", |s| {
        connect(s);
        s.access = Access::PermissionRemoved;
    }),
    ("chat-emoji", |s| {
        connect(s);
        s.chat.emoji_open = true;
    }),
    // A member's info dialog, for a name that imitates the owner's (the first letter after the
    // owner's is Cyrillic).
    ("chat-member-modal", |s| {
        connect(s);
        s.chat.edit_rooms(|rooms| {
            let room = &mut rooms[0];
            let owner = room
                .members
                .iter()
                .find(|m| m.role == chat::model::Role::Owner);
            let owner = owner.map_or(0, |m| m.id);
            let mut m = chat::model::Member::new(
                90,
                "Maya Chen".replacen('a', "\u{0430}", 1),
                chat::model::Role::Member,
            );
            m.invited_by = Some(owner);
            m.key = "7Zq2mWc9Lk4HfXtN8vRbYp3JdEaGu5sQoT1iKxVnCwMh".into();
            room.members.push(m);
        });
        s.chat.open_member("90");
    }),
    // A deputy's info dialog: the Deputy tag and who appointed them. In the example room Jonas
    // is the owner's deputy, so the author line and the members list show him too.
    ("chat-deputy", |s| {
        connect(s);
        s.chat.open_member("30");
    }),
    // Your own member info dialog, which edits your nickname: as it opens, with the new
    // nickname being typed, with one the room would refuse, on its way to the node, and after
    // the node refused it.
    ("chat-nickname-idle", |s| nickname_scene(s, |_, _| {})),
    ("chat-nickname-editing", |s| {
        nickname_scene(s, |chat, _| chat.nickname.replace_text("Sam"))
    }),
    ("chat-nickname-error", |s| {
        nickname_scene(s, |chat, _| {
            chat.nickname
                .replace_text("Sam Rivers-Hollingsworth the Third of Exeter, Esquire")
        })
    }),
    ("chat-nickname-pending", |s| {
        nickname_scene(s, |chat, room| {
            chat.nickname.replace_text("Sam");
            room.own_nickname = Some(chat::model::Pending {
                entry: 1,
                failed: None,
                retry: false,
            });
        })
    }),
    ("chat-nickname-failed", |s| {
        nickname_scene(s, |chat, room| {
            chat.nickname.replace_text("Sam");
            room.own_nickname = Some(chat::model::Pending {
                entry: 1,
                failed: Some("Not saved. Freenet refused it: the room is full.".into()),
                retry: true,
            });
        })
    }),
    // Your own dialog when the room would not take a change: said before Save, which is off.
    ("chat-nickname-blocked", |s| {
        nickname_scene(s, |chat, room| {
            chat.nickname.replace_text("Sam");
            room.nickname_block = Some(rooms::outbox::PRUNED_MESSAGE.into());
        })
    }),
    // Deputize and Revoke deputy in a member's dialog. Sam owns Design Crew, so Priya can be
    // made a deputy; in Town Square Elena is already Sam's deputy and can be revoked, though Sam
    // invited nobody there. Each step: the button, the confirmation, on its way, failed, and
    // when the room would not take a change.
    ("chat-deputize", |s| {
        deputy_scene(s, chat::example::PRIVATE_ROOM, "27", |_| {}, |_| {})
    }),
    ("chat-deputize-confirm", |s| {
        deputy_scene(
            s,
            chat::example::PRIVATE_ROOM,
            "27",
            |_| {},
            |c| c.ask_deputy(),
        )
    }),
    ("chat-deputize-sending", |s| {
        deputy_scene(
            s,
            chat::example::PRIVATE_ROOM,
            "27",
            |r| {
                if let Some(m) = r.members.iter_mut().find(|m| m.uid == "27") {
                    m.my_deputy = true;
                }
                r.deputy_changes
                    .push(deputy_change("27", true, None, false));
            },
            |_| {},
        )
    }),
    ("chat-deputize-failed", |s| {
        deputy_scene(
            s,
            chat::example::PRIVATE_ROOM,
            "27",
            |r| {
                r.deputy_changes.push(deputy_change(
                    "27",
                    true,
                    Some("Not saved. Freenet refused it: the room is full."),
                    true,
                ));
            },
            |_| {},
        )
    }),
    ("chat-deputize-blocked", |s| {
        deputy_scene(
            s,
            chat::example::PRIVATE_ROOM,
            "27",
            |r| r.deputy_block = Some(rooms::outbox::PRUNED_MESSAGE.into()),
            |_| {},
        )
    }),
    ("chat-revoke-deputy", |s| {
        deputy_scene(s, "town-square", "16", |_| {}, |_| {})
    }),
    ("chat-revoke-deputy-confirm", |s| {
        deputy_scene(s, "town-square", "16", |_| {}, |c| c.ask_deputy())
    }),
    // Ban in a member's dialog (Sam owns Design Crew, a private room, and Town Square is public
    // with Sam as a plain member): the button, the confirmation in a public and a private room,
    // what is withheld and why, and a ban the node refused, said above the composer.
    ("chat-ban", |s| {
        deputy_scene(s, chat::example::PRIVATE_ROOM, "27", offer_ban, |_| {})
    }),
    ("chat-ban-confirm", |s| {
        deputy_scene(s, "town-square", "30", offer_ban_30, |c| c.ask_ban())
    }),
    ("chat-ban-confirm-private", |s| {
        deputy_scene(s, chat::example::PRIVATE_ROOM, "27", offer_ban, |c| {
            c.ask_ban()
        })
    }),
    ("chat-ban-withheld", |s| {
        deputy_scene(
            s,
            chat::example::PRIVATE_ROOM,
            "27",
            |r| {
                r.ban_offers.insert(
                    "27".into(),
                    chat::model::BanOffer::Withheld(rooms::bans::PRIVATE_OWNER_ONLY.into()),
                );
            },
            |_| {},
        )
    }),
    ("chat-ban-failed", |s| {
        deputy_scene(
            s,
            chat::example::PRIVATE_ROOM,
            "27",
            |r| {
                r.notices.push(chat::model::Notice {
                    entry: 1,
                    text: "Your ban of Priya Raman couldn't be sent. Freenet refused it: the \
                           contract rejected the update."
                        .into(),
                    retry: true,
                });
            },
            |c| c.close_member(),
        )
    }),
    // The unread and mentions overview, opened from the room list: rooms with unread messages,
    // the one with a mention first, a muted room left out; and when everything is read.
    ("chat-overview", |s| overview_scene(s, false)),
    ("chat-overview-empty", |s| overview_scene(s, true)),
    // The quick switcher (Ctrl+K): a blank box lists rooms with unread first, then the open
    // room's members; typing narrows it; a query nothing matches says so.
    ("chat-switcher", |s| {
        connect(s);
        s.chat.toggle_switcher();
    }),
    ("chat-switcher-query", |s| {
        connect(s);
        s.chat.toggle_switcher();
        s.chat
            .switcher
            .as_mut()
            .expect("open")
            .query
            .replace_text("ri");
        s.chat
            .switcher_highlight(chat::switcher::Target::Member("27".into()));
    }),
    ("chat-switcher-empty", |s| {
        connect(s);
        s.chat.toggle_switcher();
        s.chat
            .switcher
            .as_mut()
            .expect("open")
            .query
            .replace_text("zzzz");
    }),
    // Message search (Ctrl+F): the messages of the open room that contain the query, the
    // matches drawn bold; a query nothing matches says so; a long list is cut and says so.
    ("chat-search", |s| search_scene(s, "the", 0)),
    ("chat-search-empty", |s| search_scene(s, "zzzz", 0)),
    ("chat-search-many", |s| search_scene(s, "release", 130)),
    // The bell in the conversation header opens a room's notification dialog: its three modes,
    // the current one checked. The name says what changes between the variants.
    ("chat-notification-modes", |s| {
        connect(s);
        s.chat.open_notify_dialog();
    }),
    ("chat-notification-modes-mentions", |s| {
        connect(s);
        let room = s.chat.room().expect("a room").key.clone();
        s.chat
            .set_mode(&room, rooms::modes::NotificationMode::MentionsAndReplies);
        s.chat.open_notify_dialog();
    }),
    // A muted room that is open: its bell is struck through.
    ("chat-notification-modes-muted", |s| {
        connect(s);
        let key = "rust-help";
        s.chat.set_mode(key, rooms::modes::NotificationMode::Muted);
        s.chat.select(key);
    }),
    // A muted room that is not open: its row shows no unread count, as the web UI's does not,
    // where Design Crew's does.
    ("chat-notification-modes-muted-list", |s| {
        connect(s);
        s.chat
            .set_mode("rust-help", rooms::modes::NotificationMode::Muted);
    }),
    // Export ID and Import ID: the token ready to copy, being made, refused; the import box
    // empty, with a token that does not check, and asking before it replaces an identity.
    ("chat-export-id", |s| {
        identity_scene(s, |c| {
            c.open_export();
            let room = c.room().map(|r| r.key.clone()).unwrap_or_default();
            c.identity_exported(room, Ok(scene_token()));
        })
    }),
    ("chat-export-id-copied", |s| {
        identity_scene(s, |c| {
            c.open_export();
            let room = c.room().map(|r| r.key.clone()).unwrap_or_default();
            c.identity_exported(room, Ok(scene_token()));
            c.export_copied();
        })
    }),
    ("chat-export-id-making", |s| {
        identity_scene(s, |c| c.open_export())
    }),
    ("chat-export-id-refused", |s| {
        identity_scene(s, |c| {
            c.open_export();
            let room = c.room().map(|r| r.key.clone()).unwrap_or_default();
            c.identity_exported(
                room,
                Err(
                    "Cannot export: membership data not available. Try sending a message first."
                        .into(),
                ),
            );
        })
    }),
    ("chat-background", |s| background_scene(s, true)),
    ("chat-dnd", |s| dnd_scene(s, false)),
    ("chat-dnd-on", |s| dnd_scene(s, true)),
    ("chat-background-no-tray", |s| background_scene(s, false)),
    ("chat-import-id", |s| identity_scene(s, |c| c.open_import())),
    ("chat-import-id-pasted", |s| {
        identity_scene(s, |c| {
            c.open_import();
            c.form.token.replace_text(scene_token());
        })
    }),
    ("chat-import-id-invalid", |s| {
        identity_scene(s, |c| {
            c.open_import();
            c.form.token.replace_text(
                "-----BEGIN RIVER IDENTITY-----\n3mJr7AoUXx2Wqd\n-----END RIVER IDENTITY-----",
            );
            c.submit_import();
        })
    }),
    ("chat-import-id-replace", |s| {
        identity_scene(s, |c| {
            // The token's room is in the list: the owner of the first example room is not a
            // key this scene can sign for, so the list's first room takes the token's key.
            let (owner, token) = (
                ed25519_dalek::SigningKey::from_bytes(&[3; 32]).verifying_key(),
                scene_token(),
            );
            let mut rooms = c.rooms().to_vec();
            if let Some(r) = rooms.first_mut() {
                r.key = bs58::encode(owner.to_bytes()).into_string();
            }
            c.set_rooms(rooms);
            c.open_import();
            c.form.token.replace_text(&token);
            c.submit_import();
        })
    }),
    // Joining from an invitation: a pasted invite link for a room not joined (the nickname is
    // filled in from the invitation), one for a room already joined, one that is no invite, and
    // the message offered after making an invitation.
    ("chat-join-invite", |s| {
        identity_scene(s, |c| {
            c.open_dialog(chat::DialogKind::Join);
            c.form.code.replace_text(scene_invite_link());
            c.join_code_edited();
        })
    }),
    ("chat-join-already-member", |s| {
        identity_scene(s, |c| {
            let owner = ed25519_dalek::SigningKey::from_bytes(&[3; 32]).verifying_key();
            let mut rooms = c.rooms().to_vec();
            if let Some(r) = rooms.first_mut() {
                r.key = bs58::encode(owner.to_bytes()).into_string();
            }
            c.set_rooms(rooms);
            c.open_dialog(chat::DialogKind::Join);
            c.form.code.replace_text(scene_invite_link());
            c.join_code_edited();
        })
    }),
    ("chat-join-invalid", |s| {
        identity_scene(s, |c| {
            c.open_dialog(chat::DialogKind::Join);
            c.form
                .code
                .replace_text("https://example.org/?invitation=3mJr7AoUXx2Wqd");
            c.join_code_edited();
        })
    }),
    ("chat-invite-made", |s| {
        identity_scene(s, |c| {
            if let Some(key) = c.room().map(|r| r.key.clone()) {
                c.invitation_made(key, Ok(scene_invite_link()));
            }
        })
    }),
    // The room details dialog: as a member sees it (a public and a private room), as the owner
    // edits it, with values the room would refuse, on its way to the node, after the node
    // refused it, and for an owner whose key this computer does not hold.
    ("chat-room-details-member", |s| {
        details_scene(s, "town-square", |_, _| {})
    }),
    ("chat-room-details-private", |s| {
        details_scene(s, "family", |_, _| {})
    }),
    ("chat-room-details-owner", |s| {
        details_scene(s, chat::example::EMPTY_ROOM, |_, _| {})
    }),
    // Leave Room: the question as a member and as the owner sees it.
    ("chat-room-leave-member", |s| {
        details_scene(s, "town-square", |chat, _| chat.ask_leave())
    }),
    ("chat-room-leave-owner", |s| {
        details_scene(s, chat::example::EMPTY_ROOM, |chat, _| chat.ask_leave())
    }),
    ("chat-room-details-editing", |s| {
        details_scene(s, chat::example::EMPTY_ROOM, |chat, _| {
            let form = chat.details.as_mut().expect("open");
            form.name.replace_text("Reading Circle");
            form.editing_description = true;
            form.description
                .replace_text("This month: **The Dispossessed**.\nSecond week: chapters 5-8.");
            form.limits[0].replace_text("40");
        })
    }),
    // A long, multi-paragraph description with links and a list: the dialog stays inside the
    // window and the description scrolls in its own box. For the owner and for a member.
    ("chat-room-details-long-description", |s| {
        details_scene(s, chat::example::EMPTY_ROOM, |chat, _| {
            let form = chat.details.as_mut().expect("open");
            form.description.replace_text(LONG_DESCRIPTION);
        })
    }),
    ("chat-room-details-long-description-member", |s| {
        details_scene(s, "town-square", |_, room| {
            room.description = LONG_DESCRIPTION.into();
        });
        s.chat.edit_rooms(|r| {
            if let Some(r) = r.iter_mut().find(|r| r.key == "town-square") {
                r.description = LONG_DESCRIPTION.into();
                r.details.description_source = LONG_DESCRIPTION.into();
            }
        });
        s.chat.open_details();
    }),
    ("chat-room-details-error", |s| {
        details_scene(s, chat::example::EMPTY_ROOM, |chat, _| {
            let form = chat.details.as_mut().expect("open");
            form.name.replace_text("Reading Circle");
            form.limits[2].replace_text("lots");
        })
    }),
    // The owner of a private room: Rotate beside the secret version, on its way, and refused.
    ("chat-room-details-rotating", |s| {
        details_scene(s, chat::example::PRIVATE_ROOM, |_, room| {
            room.details.rotation = Some(chat::model::Pending {
                entry: 1,
                failed: None,
                retry: false,
            });
        })
    }),
    ("chat-room-details-rotate-failed", |s| {
        details_scene(s, chat::example::PRIVATE_ROOM, |_, room| {
            room.details.rotation = Some(chat::model::Pending {
                entry: 1,
                failed: Some(
                    "Not saved. Freenet refused it: the contract rejected the update.".into(),
                ),
                retry: true,
            });
        })
    }),
    ("chat-room-details-pending", |s| {
        details_scene(s, chat::example::EMPTY_ROOM, |chat, room| {
            chat.details
                .as_mut()
                .expect("open")
                .name
                .replace_text("Reading Circle");
            room.details.pending = Some(chat::model::Pending {
                entry: 1,
                failed: None,
                retry: false,
            });
        })
    }),
    ("chat-room-details-failed", |s| {
        details_scene(s, chat::example::EMPTY_ROOM, |chat, room| {
            chat.details
                .as_mut()
                .expect("open")
                .name
                .replace_text("Reading Circle");
            room.details.pending = Some(chat::model::Pending {
                entry: 1,
                failed: Some("Not saved. Freenet refused it: the room is full.".into()),
                retry: true,
            });
        })
    }),
    // Lowering the member cap below the members the room has: asked to confirm, in plain words.
    ("chat-room-details-confirm", |s| {
        details_scene(s, chat::example::EMPTY_ROOM, |_, _| {});
        s.chat.edit_rooms(|r| {
            if let Some(r) = r.iter_mut().find(|r| r.key == chat::example::EMPTY_ROOM) {
                r.details.usage.members = 7;
                r.details.usage.messages = 61;
                r.details.usage.bans = 3;
            }
        });
        let form = s.chat.details.as_mut().expect("open");
        form.limits[0].replace_text("3");
        form.limits[3].replace_text("1");
        s.chat.submit_details();
    }),
    ("chat-room-details-no-key", |s| {
        details_scene(s, chat::example::EMPTY_ROOM, |_, room| {
            room.details.editable =
                chat::model::Editable::Blocked(rooms::room::NO_KEY_TO_EDIT.into());
        })
    }),
    // Markdown in a room's description and messages, with mention chips.
    ("chat-markdown", |s| {
        connect(s);
        s.chat.edit_rooms(|rooms| {
            let room = &mut rooms[0];
            room.description =
                "The **public** room for everyone using [Freenet](https://freenet.org). Be kind."
                    .into();
            let scheme = chat::model::mention_scheme();
            let last = room.messages.last().map_or(1, |m| m.id + 1);
            let samples = [
                format!(
                    "# Release notes\n\n**Bold**, *italic*, ~~struck~~ and `inline code`.\n\n- one\n- two\n\n> quoted\nnext line\n\nhello [@Maya Chen]({scheme}:member) and [@Sam Rivers]({scheme}:self), see https://freenet.org/ or [a link](https://freenet.org)"
                ),
                "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |".into(),
            ];
            for (i, md) in samples.into_iter().enumerate() {
                let mut m = room.messages.last().cloned().expect("example messages");
                m.id = last + i as u32;
                m.key = format!("md-sample-{i}");
                m.text = md.clone();
                m.source = md.clone();
                m.md = md;
                m.reactions.clear();
                m.reply_to = None;
                room.messages.push(m);
            }
        });
    }),
    // The node has no chat delegate under the key this build carries, though River registered
    // it.
    ("chat-no-store", |s| {
        connect(s);
        s.chat = Chat::new(
            chat::Source::Node(chat::LinkStatus::Live),
            Vec::new(),
            0,
            0,
            0,
        );
        s.chat.issues.store_missing = true;
    }),
];

fn example_update() -> UpdateInfo {
    UpdateInfo {
        version: "0.2.0".into(),
        notes: "Faster start.".into(),
        size: 14_800_000,
        required: false,
    }
}

/// An example of River's own update state, with the Freenet node at a version.
fn update_scene(s: &mut RiverApp, auto: bool, status: UpdateStatus) {
    let ui = &mut s.chat.update;
    ui.support = UpdateSupport::Automatic;
    ui.version = "0.1.0".into();
    ui.auto = auto;
    ui.status = status;
    ui.node_version = Some("0.2.160".into());
}

fn connect(s: &mut RiverApp) {
    s.setup = Setup::Connected {
        version: "0.2.155".into(),
    };
}

/// The window's opening size, in points: room for the room list, a conversation and the
/// members panel side by side.
pub const WINDOW_SIZE: (f32, f32) = (1280.0, 800.0);

/// The size the `*narrow*` scenes are rendered at. `.github/workflows/apps.yml` passes it as
/// `--size 820x720`; keep the two in step.
pub const NARROW_SCENE_SIZE: (f32, f32) = (820.0, 720.0);

/// The window size a scene is meant to be rendered at.
pub fn scene_size(name: &str) -> (f32, f32) {
    if name.contains("narrow") {
        NARROW_SCENE_SIZE
    } else {
        WINDOW_SIZE
    }
}

pub fn spec() -> AppSpec<RiverApp> {
    AppSpec {
        scenes: SCENES,
        min_size: Some((640.0, 480.0)),
        setup: Some(load_logo),
        poll: Some(|s, _, tx| {
            s.sync_unread(tx);
            false
        }),
        // Keys no focused control took, or any key while nothing has focus: Escape abandons a
        // reply or an edit even when the composer is not focused.
        on_key: Some(|s, ev, tx| {
            if chat::switcher::is_chord(ev) && s.chat.switcher_chord(std::time::Instant::now()) {
                return true;
            }
            if chat::search::is_chord(ev) && s.chat.search_chord() {
                return true;
            }
            // Ctrl+Q (Cmd+Q on a Mac) quits where the window's close button only hides it to
            // the tray, so a hidden River is never the only way out. It quits whatever dialog
            // is open: a dialog is no reason to keep a River running that someone is quitting.
            if ev.is_command('q') && s.chat.background.is_some() {
                tx.quit();
                return true;
            }
            if s.chat.app_key(ev, tx) {
                return true;
            }
            ev.key == rune::Key::Escape && s.chat.escape()
        }),
        ..AppSpec::new("River", WINDOW_SIZE, RiverApp::detached, view)
    }
}

/// Build the live app: `init` receives the app's own handle so background work can report back.
pub fn app(make: impl FnOnce(AppHandle<RiverApp>) -> RiverApp + 'static) -> rune::App<RiverApp> {
    let app = rune::App::new(spec()).on_focus(|s, focused, _| s.focus_changed(focused));
    let handle = app.handle();
    app.init(move || make(handle))
}

pub fn view(s: &RiverApp, cx: &mut Cx<RiverApp>) -> El {
    chat::zoom::apply(&s.chat, cx, |cx| screen(s, cx))
}

fn screen(s: &RiverApp, cx: &mut Cx<RiverApp>) -> El {
    // River is dark-only for now, like the chat screen; the setup screens match it.
    let palette = chat::style::palette();
    let k = Kit::neutral(true);
    let logo = s.logo.as_ref().map(|l| l.image.pixels());
    let t = (&palette, logo);
    let node = &s.chat.node.choice;
    let existing = node.is_existing_node();
    let at = node.address.authority();
    // The form that picks the node replaces the setup card until the rooms are showing, and is
    // a dialog over them after.
    let form = if s.rooms_shown() {
        None
    } else {
        chat::node_ui::screen(&s.chat, false, &k)
    };
    let body = if let Some(form) = form {
        panel_with(
            t,
            &form.title,
            &form.intro,
            form.content,
            None,
            form.actions,
        )
    } else if let Some(problem) = &s.chat.node.problem {
        // Said on screen, not only on a console that a release build on Windows has none of.
        panel(
            t,
            "Can't use that node address",
            problem,
            None,
            vec![
                use_node_button(&k, true),
                Button::new("Use Freenet on this computer")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.node_dismiss_problem())
                    .build(&k)
                    .key("problem-local"),
            ],
        )
    } else {
        match &s.setup {
            Setup::Checking if existing => panel(
                t,
                &format!("Checking the node at {at}…"),
                "River is asking it whether it is Freenet, and which version.",
                None,
                vec![],
            ),
            Setup::Checking => panel(
                t,
                "Looking for Freenet…",
                "River runs on Freenet, a peer-to-peer network on your own computer.",
                None,
                vec![],
            ),
            Setup::NotInstalled if s.can_install() => panel(
                t,
                "Freenet isn't installed",
                "River needs Freenet running on this computer. River can download it from the official release, check its signature, and set it up to run in the background.",
                None,
                vec![
                    Button::new("Install Freenet")
                        .variant(ButtonVariant::Contrast)
                        .on_press(|s: &mut RiverApp| s.install())
                        .build(&k)
                        .key("install"),
                    manual_install_button(&k),
                    // For someone who installed Freenet another way and came back.
                    Button::new("Check again")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut RiverApp| s.retry())
                        .build(&k)
                        .key("retry"),
                    use_node_button(&k, false),
                ],
            ),
            Setup::NotInstalled => panel(
                t,
                "Freenet isn't installed",
                "River needs Freenet running on this computer. Install it, then try again.",
                None,
                vec![
                    manual_install_button(&k),
                    retry_button(&k),
                    use_node_button(&k, false),
                ],
            ),
            Setup::Installing(step) => {
                let (title, fraction) = install_progress(step);
                let progress = fraction.map(|f| (f, download_detail(step)));
                panel(t, title, "This only happens once.", progress, vec![])
            }
            Setup::Starting => panel(
                t,
                "Starting Freenet…",
                "This can take a moment the first time.",
                None,
                vec![],
            ),
            // A node somebody else runs is not installed from here, so no install offer.
            Setup::Failed(message) if existing => panel(
                t,
                "Can't use that node",
                message,
                None,
                vec![retry_button(&k), use_node_button(&k, true)],
            ),
            Setup::Failed(message) => panel(
                t,
                "Something went wrong",
                message,
                None,
                vec![
                    retry_button(&k),
                    manual_install_button(&k),
                    use_node_button(&k, false),
                ],
            ),
            Setup::Connected { .. } => match &s.access {
                Access::Example | Access::Ready => return chat::view(&s.chat, logo, cx),
                access => rooms_panel(t, access, &k, node),
            },
        }
    };
    // A node that is not on this computer is said on every screen, at every start.
    let banners = chat::node_ui::banners(&s.chat.node, &palette);
    let mut root = col()
        .key("root")
        .w_full()
        .h_full()
        .center()
        .gap(16.0)
        .padding(24.0)
        .fill(palette.rail);
    if !banners.is_empty() {
        let mut notes = col().key("node-banners").w(PANEL_W + 64.0).gap(8.0);
        for b in banners {
            notes = notes.child(b);
        }
        root = root.child(notes);
    }
    // Before the rooms show, the node status says why River is not connected yet.
    let mut layer = Vec::new();
    chat::node_status::dialog(&s.chat, &k, &mut layer);
    rune_kit::root(
        root.child(body).child(
            Button::new("Node status")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut RiverApp| s.chat.status.open())
                .build(&k)
                .key("node-status-open"),
        ),
        layer,
    )
}

/// The screens between reaching the node and showing the rooms.
fn rooms_panel(t: Look, access: &Access, k: &Kit, node: &NodeChoice) -> El {
    let existing = node.is_existing_node();
    let at = node.address.authority();
    match access {
        Access::Example | Access::Ready | Access::Connecting => panel(
            t,
            "Opening your rooms…",
            "River is reading your rooms from Freenet.",
            None,
            vec![],
        ),
        Access::WaitingForApproval { approve_url } => {
            let url = approve_url.clone();
            let body = if existing {
                format!(
                    "Freenet keeps your rooms for River in your browser. Press the button to \
                     open a Freenet page in your browser that asks whether this app may use \
                     them too, then choose Allow there. The page is at {at}, so that address \
                     has to work in this computer's browser too: for a node reached through an \
                     SSH tunnel it is the tunnel's address on this computer, such as \
                     localhost:7509."
                )
            } else {
                "Freenet keeps your rooms for River in your browser. Press the button to open \
                 a Freenet page in your browser that asks whether this app may use them too, \
                 then choose Allow there."
                    .to_owned()
            };
            panel(
                t,
                "Allow River to open your rooms",
                &body,
                None,
                vec![
                    Button::new("Open Freenet's approval page")
                        .variant(ButtonVariant::Contrast)
                        .on_press_tx(move |_: &mut RiverApp, tx: &mut Tx| {
                            let _ = tx.open_url(&url);
                        })
                        .build(k)
                        .key("approve"),
                ],
            )
        }
        Access::NotApproved { expired } => panel(
            t,
            if *expired {
                "The request to open your rooms expired"
            } else {
                "River wasn't allowed to open your rooms"
            },
            if *expired {
                "Freenet's page in your browser wasn't answered in time. Ask again for a new one."
            } else {
                "Freenet's page in your browser was answered with Deny. To see your rooms here, \
                 ask again and choose Allow."
            },
            None,
            vec![ask_again_button(k)],
        ),
        Access::PermissionRemoved => panel(
            t,
            "River's permission was removed",
            "River no longer has Freenet's permission to open your rooms: it was revoked on \
             Freenet's Apps and permissions page, or Freenet lost it. Ask again to see your \
             rooms here.",
            None,
            vec![ask_again_button(k)],
        ),
        Access::Unsupported => panel(
            t,
            "Freenet needs an update",
            &format!(
                "To open your rooms, River needs Freenet {} or newer. Update it (run \
                 `freenet update`, or install the latest from freenet.org), then try again.",
                rooms::grant::GRANT_NODE_VERSION
            ),
            None,
            if existing {
                vec![rooms_retry_button(k), use_node_button(k, true)]
            } else {
                vec![rooms_retry_button(k), manual_install_button(k)]
            },
        ),
        Access::Refused(message) => panel(
            t,
            "River can't open your rooms",
            message,
            None,
            vec![rooms_retry_button(k), use_node_button(k, existing)],
        ),
        // The reason is the node's or the network library's own text; it goes to the log,
        // and the screen says what it means.
        Access::Unreachable(_) => panel(
            t,
            "Can't reach Freenet",
            &if existing {
                format!(
                    "Nothing is answering River at {at} yet. River will keep trying. If you \
                     reach the node through an SSH tunnel or a VPN, check that it is up."
                )
            } else {
                "Freenet on this computer isn't answering River yet. River will keep trying."
                    .to_owned()
            },
            None,
            vec![use_node_button(k, existing)],
        ),
    }
}

fn ask_again_button(k: &Kit) -> El {
    Button::new("Ask again")
        .variant(ButtonVariant::Contrast)
        .on_press(|s: &mut RiverApp| s.retry_rooms())
        .build(k)
        .key("ask-again")
}

fn rooms_retry_button(k: &Kit) -> El {
    Button::new("Try again")
        .variant(ButtonVariant::Contrast)
        .on_press(|s: &mut RiverApp| s.retry_rooms())
        .build(k)
        .key("rooms-retry")
}

fn install_progress(step: &InstallStep) -> (&'static str, Option<f32>) {
    match step {
        InstallStep::FindingRelease => ("Finding the latest Freenet release…", None),
        // No bar when the size is unknown: an empty bar that never moves looks stuck.
        InstallStep::Downloading { received, total } => (
            "Downloading Freenet…",
            total
                .filter(|t| *t > 0)
                .map(|t| (*received as f32 / t as f32).clamp(0.0, 1.0)),
        ),
        // The release's signature was checked before downloading; this matches the download
        // against the signed checksum.
        InstallStep::Verifying => ("Checking the download…", None),
        InstallStep::Installing => ("Installing Freenet…", None),
        InstallStep::WaitingForInstaller => {
            ("Finish setting up Freenet in its installer window", None)
        }
    }
}

/// "9.4 of 23.4 MB" under the download bar, when the download's size is known.
fn download_detail(step: &InstallStep) -> Option<String> {
    let InstallStep::Downloading {
        received,
        total: Some(total),
    } = step
    else {
        return None;
    };
    let mb = |b: u64| b as f64 / 1_000_000.0;
    Some(format!(
        "{:.1} of {:.1} MB",
        mb((*received).min(*total)),
        mb(*total)
    ))
}

/// The setup card's inner width.
const PANEL_W: f32 = 380.0;
/// The card's least height. With it every step's card is the same height, so the card, which
/// is centred, keeps its top (and the logo) still as setup moves from step to step.
const PANEL_MIN_H: f32 = 276.0;

type Look<'a> = (&'a chat::style::Palette, Option<&'a rune::Pixels>);

/// A progress bar's fraction, and the text under it.
type Progress = Option<(f32, Option<String>)>;

fn panel(t: Look, title: &str, body: &str, progress: Progress, actions: Vec<El>) -> El {
    panel_with(t, title, body, Vec::new(), progress, actions)
}

/// [`panel`] with `content` (a text field, a warning) between the words and the buttons.
fn panel_with(
    t: Look,
    title: &str,
    body: &str,
    content: Vec<El>,
    progress: Progress,
    actions: Vec<El>,
) -> El {
    let (p, logo) = t;
    let mark = match logo {
        Some(px) => el().size(LOGO_PT, LOGO_PT).pixels(px),
        None => el().size(LOGO_PT, LOGO_PT),
    };
    let mut card = col()
        .key("panel")
        .w(PANEL_W + 64.0)
        .min_h(PANEL_MIN_H)
        .padding(32.0)
        .gap(12.0)
        .radius(16.0)
        .fill(p.bg)
        .border(1.0, p.line)
        .shadow(18.0, 48.0, p.shadow)
        .child(
            row()
                .key("wordmark")
                .gap(10.0)
                .mb(10.0)
                .center_y()
                .child(mark)
                .child(text("River").font(chat::style::BRAND).color(p.text)),
        )
        .child(
            text(title.to_owned())
                .key("title")
                .font(chat::style::TITLE)
                .color(p.text)
                .wrap()
                .w_full(),
        );
    // A card with no words under the title (the confirmation) keeps no empty gap.
    if !body.is_empty() {
        card = card.child(
            text(body.to_owned())
                .key("body")
                .font(chat::style::BODY.lh(21.0))
                .color(p.text_secondary)
                .wrap()
                .w_full(),
        );
    }
    for c in content {
        card = card.child(c);
    }
    if let Some((fraction, detail)) = progress {
        card = card.child(progress_bar(p, fraction));
        if let Some(detail) = detail {
            card = card.child(
                text(detail)
                    .key("progress-detail")
                    .font(chat::style::SMALL)
                    .color(p.text_muted),
            );
        }
    }
    if !actions.is_empty() {
        let mut buttons = row().key("actions").gap(8.0).mt(10.0).flex_wrap();
        for action in actions {
            buttons = buttons.child(action);
        }
        card = card.child(buttons);
    }
    card
}

fn progress_bar(p: &chat::style::Palette, fraction: f32) -> El {
    el().key("track")
        .w_full()
        .h(4.0)
        .mt(6.0)
        .radius(2.0)
        .fill(p.surface_hover)
        .child(
            el().key("bar")
                .w(PANEL_W * fraction.clamp(0.0, 1.0))
                .h(4.0)
                .radius(2.0)
                .fill(p.accent)
                .motion(rune::Spring::new(0.3, 0.0)),
        )
}

/// The way to the form that picks a node the person already has: "Use a Freenet node I already
/// have" when none is chosen, "Use a different node" when one is.
fn use_node_button(k: &Kit, changing: bool) -> El {
    Button::new(if changing {
        "Use a different node"
    } else {
        "Use a Freenet node I already have"
    })
    .variant(ButtonVariant::Ghost)
    .on_press(|s: &mut RiverApp| s.node_open())
    .build(k)
    .key("use-node")
}

fn manual_install_button(k: &Kit) -> El {
    Button::new("Install it myself")
        .variant(ButtonVariant::Ghost)
        .on_press_tx(|_: &mut RiverApp, tx: &mut Tx| {
            let _ = tx.open_url(MANUAL_INSTALL_URL);
        })
        .build(k)
        .key("manual")
}

fn retry_button(k: &Kit) -> El {
    Button::new("Try again")
        .variant(ButtonVariant::Contrast)
        .on_press(|s: &mut RiverApp| s.retry())
        .build(k)
        .key("retry")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Runtime};

    fn render(setup: Setup) -> Runtime<RiverApp> {
        let mut state = RiverApp::detached();
        state.setup = setup;
        let mut rt = Runtime::new(
            state,
            view,
            (560.0, 440.0),
            Env {
                system_dark: true,
                scale: 1.0,
            },
        );
        rt.frame(0.0);
        rt
    }

    #[test]
    fn each_stage_offers_the_right_actions() {
        let cases = [
            (Setup::Checking, &[][..]),
            (Setup::NotInstalled, &["install", "manual", "retry"][..]),
            (Setup::Installing(InstallStep::Verifying), &[][..]),
            (Setup::Starting, &[][..]),
            (Setup::Failed("x".into()), &["retry", "manual"][..]),
            (
                Setup::Connected {
                    version: "1".into(),
                },
                &[][..],
            ),
        ];
        for (setup, expected) in cases {
            let rt = render(setup.clone());
            for key in ["install", "manual", "retry"] {
                assert_eq!(
                    rt.find(key).is_some(),
                    expected.contains(&key),
                    "{setup:?}: button {key}"
                );
            }
        }
    }

    fn downloading(received: u64, total: Option<u64>) -> InstallStep {
        InstallStep::Downloading { received, total }
    }

    #[test]
    fn only_a_download_of_known_size_shows_a_bar() {
        assert!(
            render(Setup::Installing(downloading(50, Some(100))))
                .find("bar")
                .is_some()
        );
        for step in [
            downloading(50, None),
            downloading(50, Some(0)),
            InstallStep::FindingRelease,
            InstallStep::Verifying,
            InstallStep::Installing,
            InstallStep::WaitingForInstaller,
        ] {
            assert!(
                render(Setup::Installing(step.clone()))
                    .find("bar")
                    .is_none(),
                "{step:?}"
            );
        }
        assert_eq!(install_progress(&downloading(50, Some(100))).1, Some(0.5));
        // A server that sends more than it announced doesn't overflow the bar.
        assert_eq!(install_progress(&downloading(150, Some(100))).1, Some(1.0));
    }

    /// A host that only counts probes, for pressing buttons in a live window.
    struct CountingHost {
        installable: bool,
        probes: std::sync::atomic::AtomicUsize,
    }

    impl NodeHost for CountingHost {
        fn probe_at(&self, _: &str) -> NodeProbe {
            self.probes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            NodeProbe::Unreachable
        }
        fn probe(&self) -> NodeProbe {
            self.probes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            NodeProbe::NotInstalled
        }
        fn can_install(&self) -> bool {
            self.installable
        }
        fn install(&self, _: &mut dyn FnMut(InstallStep)) -> Result<(), String> {
            Ok(())
        }
        fn start(&self) -> Result<(), String> {
            Ok(())
        }
    }

    fn render_with_host(
        setup: Setup,
        installable: bool,
    ) -> (Runtime<RiverApp>, std::sync::Arc<CountingHost>) {
        let host = std::sync::Arc::new(CountingHost {
            installable,
            probes: Default::default(),
        });
        let ctl = Controller::reporting_to(host.clone(), |_| {});
        let state = RiverApp {
            ctl: Some(ctl),
            setup,
            ..RiverApp::detached()
        };
        let env = Env {
            system_dark: true,
            scale: 1.0,
        };
        let mut rt = Runtime::new(state, view, (560.0, 440.0), env);
        rt.frame(0.0);
        (rt, host)
    }

    #[test]
    fn a_platform_that_cannot_install_offers_no_install_button() {
        let (rt, _) = render_with_host(Setup::NotInstalled, false);
        assert!(rt.find("install").is_none());
        assert!(rt.find("manual").is_some());
        assert!(rt.find("retry").is_some());
    }

    #[test]
    fn pressing_retry_checks_for_the_node_again() {
        let (mut rt, host) = render_with_host(Setup::Failed("x".into()), true);
        let (x, y) = rt.find("retry").expect("retry button");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 60.0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while host.probes.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "retry never probed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Fails to compile when a new screen is added, and fails when it has no scene.
    #[test]
    fn every_screen_has_a_scene() {
        fn screen(app: &RiverApp) -> &'static str {
            match &app.setup {
                Setup::Checking => "checking",
                Setup::NotInstalled => "not-installed",
                Setup::Installing(InstallStep::FindingRelease) => "finding-release",
                Setup::Installing(InstallStep::Downloading { .. }) => "downloading",
                Setup::Installing(InstallStep::Verifying) => "verifying",
                Setup::Installing(InstallStep::Installing) => "installing",
                Setup::Installing(InstallStep::WaitingForInstaller) => "waiting-for-installer",
                Setup::Starting => "starting",
                Setup::Failed(_) => "failed",
                Setup::Connected { .. } => match &app.access {
                    Access::Example | Access::Ready => "chat",
                    Access::Connecting => "rooms-connecting",
                    Access::WaitingForApproval { .. } => "rooms-waiting-for-approval",
                    Access::NotApproved { expired: false } => "rooms-denied",
                    Access::NotApproved { expired: true } => "rooms-expired",
                    Access::Unsupported => "rooms-unsupported",
                    Access::Refused(_) => "rooms-refused",
                    Access::Unreachable(_) => "rooms-unreachable",
                    Access::PermissionRemoved => "rooms-permission-removed",
                },
            }
        }
        let mut covered: Vec<&str> = SCENES
            .iter()
            .map(|(name, apply)| {
                let mut s = RiverApp::detached();
                apply(&mut s);
                let shown = screen(&s);
                // Variants of one screen share its name as a prefix ("chat-private-room").
                assert!(
                    *name == shown || name.starts_with(&format!("{shown}-")),
                    "scene {name} shows the {shown} screen"
                );
                shown
            })
            .collect();
        covered.sort();
        covered.dedup();
        assert_eq!(covered.len(), 18, "{covered:?}");
    }

    /// The logo is a gradient SVG shared with the web UI; this fails if an edit there breaks
    /// it for the native renderer. It checks the raster's size only: Rune's image handle does
    /// not expose decoded pixels, and checking coverage directly would mean depending on
    /// resvg here. The CI render step shows the result.
    #[test]
    fn the_logo_rasterizes() {
        let cache = rune::image::ImageCache::new(std::sync::Arc::new(|| {}));
        let image = cache.svg(LOGO_SVG, LOGO_PT, LOGO_SCALE);
        let [w, h] = image.wait().expect("the logo rasterizes");
        let px = (LOGO_PT * LOGO_SCALE) as u32;
        assert!(w > px / 2 && h > px / 2 && w <= px && h <= px, "{w}x{h}");
    }

    /// CI renders the `*narrow*` scenes with a `--size` it spells out itself; it must be the
    /// size [`scene_size`] and the tests use.
    #[test]
    fn ci_renders_narrow_scenes_at_the_narrow_size() {
        const WORKFLOW: &str = include_str!("../../../.github/workflows/apps.yml");
        let line = WORKFLOW
            .lines()
            .find(|l| l.contains("*narrow*)"))
            .expect("apps.yml picks a size for the narrow scenes");
        let size = line
            .split("--size ")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_alphanumeric()).next())
            .expect("a --size WxH");
        let (w, h) = NARROW_SCENE_SIZE;
        assert_eq!(size, format!("{w}x{h}"));
        // And the size it picks reaches the render.
        let snap = WORKFLOW
            .lines()
            .find(|l| l.contains("--snap"))
            .expect("apps.yml renders scenes with --snap");
        assert!(snap.contains("\"${size[@]}\""), "{snap}");
    }

    fn render_access(access: Access) -> Runtime<RiverApp> {
        let mut state = RiverApp::detached();
        state.setup = Setup::Connected {
            version: "0.2.156".into(),
        };
        state.access = access;
        let mut rt = Runtime::new(
            state,
            view,
            (560.0, 440.0),
            Env {
                system_dark: true,
                scale: 1.0,
            },
        );
        rt.frame(0.0);
        rt
    }

    #[test]
    fn each_rooms_screen_offers_the_right_actions() {
        let url = "http://127.0.0.1:7509/v1/app-grants/approve/x".to_owned();
        let cases = [
            (Access::Connecting, &[][..]),
            (
                Access::WaitingForApproval {
                    approve_url: url.clone(),
                },
                &["approve"][..],
            ),
            (Access::NotApproved { expired: false }, &["ask-again"][..]),
            (Access::NotApproved { expired: true }, &["ask-again"][..]),
            (Access::PermissionRemoved, &["ask-again"][..]),
            (Access::Unsupported, &["rooms-retry", "manual"][..]),
            (Access::Refused("no".into()), &["rooms-retry"][..]),
            (Access::Unreachable("down".into()), &[][..]),
        ];
        for (access, expected) in cases {
            let rt = render_access(access.clone());
            assert!(rt.find("chat").is_none(), "{access:?} shows no chat");
            for key in ["approve", "ask-again", "rooms-retry", "manual"] {
                assert_eq!(
                    rt.find(key).is_some(),
                    expected.contains(&key),
                    "{access:?}: button {key}"
                );
            }
        }
        assert!(render_access(Access::Ready).find("chat").is_some());
    }

    #[test]
    fn the_approval_page_opens_only_when_the_button_is_pressed() {
        let url = "http://127.0.0.1:7509/v1/app-grants/approve/x".to_owned();
        let mut rt = render_access(Access::Connecting);
        let waiting = Access::WaitingForApproval {
            approve_url: url.clone(),
        };
        let w = waiting.clone();
        rt.update(move |s, tx| s.apply_rooms(RoomsUpdate::Access(w), tx));
        rt.frame(1.0 / 60.0);
        rt.update(move |s, tx| s.apply_rooms(RoomsUpdate::Access(waiting), tx));
        rt.frame(2.0 / 60.0);
        // Reaching the waiting state opens nothing by itself.
        assert!(rt.take_url_requests().is_empty());
        let (x, y) = rt.find("approve").expect("the open button");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(3.0 / 60.0);
        assert_eq!(rt.take_url_requests(), [url]);
    }

    #[test]
    fn node_rooms_replace_the_screen_and_a_lost_connection_keeps_them() {
        let mut rt = render_access(Access::Connecting);
        let rooms = chat::example::data().rooms;
        let first = rooms[1].key.clone();
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Rooms {
                    rooms,
                    open: Some(first),
                    modes: Default::default(),
                    settled: Vec::new(),
                    status: chat::LinkStatus::Loading,
                    now: 29_000_000,
                    issues: Default::default(),
                    work: Vec::new(),
                },
                tx,
            );
            s.apply_rooms(RoomsUpdate::Access(Access::Ready), tx);
            s.apply_rooms(RoomsUpdate::Status(chat::LinkStatus::Reconnecting), tx);
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.find("chat").is_some());
        assert!(rt.find("source-status").is_some());
    }

    #[test]
    fn a_download_of_known_size_says_how_far_it_is() {
        assert_eq!(
            download_detail(&downloading(9_400_000, Some(23_372_438))).as_deref(),
            Some("9.4 of 23.4 MB")
        );
        assert_eq!(download_detail(&downloading(9_400_000, None)), None);
        assert_eq!(
            download_detail(&downloading(150, Some(100))).as_deref(),
            Some("0.0 of 0.0 MB")
        );
        let rt = render(Setup::Installing(downloading(50, Some(100))));
        assert!(rt.find("progress-detail").is_some());
    }

    /// The node's rooms, in a window wide enough for the members panel, on a test clipboard.
    fn node_rooms(board: &rune::clipboard::MemoryClipboard) -> Runtime<RiverApp> {
        node_rooms_sized(board, (1280.0, 800.0))
    }

    fn node_rooms_sized(
        board: &rune::clipboard::MemoryClipboard,
        size: (f32, f32),
    ) -> Runtime<RiverApp> {
        let mut state = RiverApp::detached();
        state.setup = Setup::Connected {
            version: "0.2.156".into(),
        };
        state.access = Access::Connecting;
        let mut rt = Runtime::new(
            state,
            view,
            size,
            Env {
                system_dark: true,
                scale: 1.0,
            },
        );
        rt.use_clipboard(board);
        rt.frame(0.0);
        let rooms = chat::example::data().rooms;
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Rooms {
                    rooms,
                    open: None,
                    modes: Default::default(),
                    settled: Vec::new(),
                    status: chat::LinkStatus::Live,
                    now: 29_000_000,
                    issues: Default::default(),
                    work: Vec::new(),
                },
                tx,
            );
            s.apply_rooms(RoomsUpdate::Access(Access::Ready), tx);
        });
        settle(&mut rt);
        rt
    }

    /// Run frames until motion has stopped, so what is found is where it stays.
    fn settle(rt: &mut Runtime<RiverApp>) {
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
    }

    fn press(rt: &mut Runtime<RiverApp>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no {key}"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        settle(rt);
    }

    /// A runtime that routes keys no control took to the app's own handler, as the window does.
    fn node_rooms_with_keys(board: &rune::clipboard::MemoryClipboard) -> Runtime<RiverApp> {
        let mut rt = node_rooms(board);
        rt.on_key = spec().on_key;
        rt
    }

    fn chord(c: char) -> rune::KeyEvent {
        rune::KeyEvent {
            key: rune::Key::Command(c),
            mods: rune::Mods {
                cmd: true,
                ctrl: !cfg!(target_os = "macos"),
                logo: cfg!(target_os = "macos"),
                ..Default::default()
            },
        }
    }

    fn bare(key: rune::Key) -> rune::KeyEvent {
        rune::KeyEvent {
            key,
            mods: rune::Mods::default(),
        }
    }

    fn open_room_name(rt: &Runtime<RiverApp>) -> String {
        rt.state.chat.room().expect("a room").name.clone()
    }

    #[test]
    fn the_chord_opens_the_switcher_even_from_the_composer_and_closes_it_again() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.update(|s, _| s.chat.composer.focused = true);
        settle(&mut rt);
        rt.key(chord('k'));
        settle(&mut rt);
        assert!(rt.state.chat.switcher.is_some(), "Ctrl+K opened it");
        assert!(rt.find("switcher-dialog").is_some());
        // A second chord inside the repeat guard is swallowed (a held key), one after it closes.
        // The guard's last time is set an hour ahead, so a real key now is always "too soon"
        // however fast or slow the machine runs, and the later one is given, not read.
        let t0 = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        rt.update(move |s, _| s.chat.pin_chord_time(Some(t0)));
        rt.key(chord('k'));
        settle(&mut rt);
        assert!(
            rt.state.chat.switcher.is_some(),
            "a chord this soon is a repeat"
        );
        let later = t0 + chat::switcher::CHORD_QUIET * 3;
        rt.update(move |s, _| assert!(s.chat.switcher_chord(later)));
        settle(&mut rt);
        assert!(rt.state.chat.switcher.is_none(), "the chord closes it");
        assert!(rt.find("switcher-dialog").is_none());
    }

    #[test]
    fn escape_closes_the_switcher_and_opens_nothing() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        let before = open_room_name(&rt);
        rt.key(chord('k'));
        settle(&mut rt);
        rt.key(bare(rune::Key::Escape));
        settle(&mut rt);
        assert!(rt.state.chat.switcher.is_none());
        assert_eq!(open_room_name(&rt), before);
    }

    #[test]
    fn arrows_and_enter_open_a_room_and_a_blank_box_lists_unread_rooms_first() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        assert_eq!(open_room_name(&rt), "Town Square");
        rt.key(chord('k'));
        settle(&mut rt);
        let hits = rt.state.chat.switcher_hits();
        let first_two: Vec<_> = hits
            .iter()
            .take(2)
            .map(|h| match &h.target {
                chat::switcher::Target::Room(k) => rt
                    .state
                    .chat
                    .rooms()
                    .iter()
                    .find(|r| &r.key == k)
                    .map(|r| r.name.clone())
                    .expect("a room"),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(first_two, ["Design Crew", "Rust Help"]);
        rt.key(bare(rune::Key::Down));
        settle(&mut rt);
        rt.key(bare(rune::Key::Enter));
        settle(&mut rt);
        assert!(rt.state.chat.switcher.is_none(), "choosing closes it");
        assert_eq!(open_room_name(&rt), "Rust Help");
    }

    #[test]
    fn up_from_the_top_wraps_to_the_last_result() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.key(chord('k'));
        settle(&mut rt);
        let n = rt.state.chat.switcher_hits().len();
        rt.key(bare(rune::Key::Up));
        settle(&mut rt);
        let hits = rt.state.chat.switcher_hits();
        assert_eq!(rt.state.chat.switcher_selected(&hits), n - 1);
    }

    #[test]
    fn typing_narrows_the_results_and_enter_on_a_member_opens_their_dialog() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.key(chord('k'));
        settle(&mut rt);
        rt.key(rune::KeyEvent {
            key: rune::Key::Text("p".into()),
            mods: rune::Mods::default(),
        });
        rt.key(rune::KeyEvent {
            key: rune::Key::Text("r".into()),
            mods: rune::Mods::default(),
        });
        settle(&mut rt);
        assert_eq!(
            rt.state.chat.switcher.as_ref().expect("open").query.text,
            "pr"
        );
        let hits = rt.state.chat.switcher_hits();
        assert!(
            matches!(&hits[0].target, chat::switcher::Target::Member(_)),
            "Priya is the best match: {hits:?}"
        );
        rt.key(bare(rune::Key::Enter));
        settle(&mut rt);
        assert!(rt.state.chat.switcher.is_none());
        let uid = rt
            .state
            .chat
            .member_modal
            .clone()
            .expect("the member dialog");
        let room = rt.state.chat.room().expect("room");
        assert_eq!(
            room.member_by_uid(&uid).expect("member").nickname,
            "Priya Raman"
        );
    }

    #[test]
    fn a_click_on_a_row_opens_that_room() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.key(chord('k'));
        settle(&mut rt);
        press(&mut rt, "switcher-row-1");
        assert!(rt.state.chat.switcher.is_none());
        assert_eq!(open_room_name(&rt), "Rust Help");
    }

    #[test]
    fn the_chord_does_nothing_over_another_dialog() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        press(&mut rt, "create-room");
        assert!(rt.state.chat.form.open.is_some());
        rt.key(chord('k'));
        settle(&mut rt);
        assert!(rt.state.chat.switcher.is_none());
        assert!(rt.state.chat.form.open.is_some(), "the dialog stays");
    }

    #[test]
    fn a_query_with_no_match_says_so_and_enter_does_nothing() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.key(chord('k'));
        settle(&mut rt);
        rt.update(|s, _| {
            s.chat
                .switcher
                .as_mut()
                .expect("open")
                .query
                .replace_text("qqqq")
        });
        settle(&mut rt);
        assert!(rt.find("switcher-empty").is_some());
        let before = open_room_name(&rt);
        rt.key(bare(rune::Key::Enter));
        settle(&mut rt);
        assert_eq!(open_room_name(&rt), before);
        assert!(rt.state.chat.member_modal.is_none());
    }

    fn type_text(rt: &mut Runtime<RiverApp>, text: &str) {
        for c in text.chars() {
            rt.key(rune::KeyEvent {
                key: rune::Key::Text(c.to_string()),
                mods: rune::Mods::default(),
            });
        }
        settle(rt);
    }

    /// Open it with the chord. The chord ignores a press soon after another, judged by the real
    /// clock, so forget the last one first: how fast the machine runs then cannot matter.
    fn open_switcher(rt: &mut Runtime<RiverApp>) {
        rt.update(|s, _| s.chat.pin_chord_time(None));
        rt.key(chord('k'));
        settle(rt);
        assert!(rt.state.chat.switcher.is_some(), "the chord opened it");
    }

    fn room_key(rt: &Runtime<RiverApp>, name: &str) -> String {
        rt.state
            .chat
            .rooms()
            .iter()
            .find(|r| r.name == name)
            .expect("a room")
            .key
            .clone()
    }

    /// The row the highlight is on, by what it opens.
    fn highlighted(rt: &Runtime<RiverApp>) -> chat::switcher::Target {
        let hits = rt.state.chat.switcher_hits();
        let i = rt.state.chat.switcher_selected(&hits);
        hits[i].target.clone()
    }

    #[test]
    fn the_highlight_follows_its_row_when_the_list_changes_under_it() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        open_switcher(&mut rt);
        rt.key(bare(rune::Key::Down));
        settle(&mut rt);
        let rust = room_key(&rt, "Rust Help");
        assert_eq!(highlighted(&rt), chat::switcher::Target::Room(rust.clone()));
        // Book Club gets nine unread and moves to the top, so Rust Help is on another row.
        rt.update(|s, _| {
            s.chat.edit_rooms(|rooms| {
                rooms
                    .iter_mut()
                    .find(|r| r.name == "Book Club")
                    .expect("room")
                    .unread = 9;
            })
        });
        settle(&mut rt);
        assert_eq!(
            highlighted(&rt),
            chat::switcher::Target::Room(rust),
            "still the same room"
        );
        rt.key(bare(rune::Key::Enter));
        settle(&mut rt);
        assert_eq!(open_room_name(&rt), "Rust Help");
    }

    #[test]
    fn a_click_opens_what_is_drawn_on_the_row_after_the_list_changed() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        open_switcher(&mut rt);
        rt.update(|s, _| {
            s.chat.edit_rooms(|rooms| {
                rooms
                    .iter_mut()
                    .find(|r| r.name == "Book Club")
                    .expect("room")
                    .unread = 9;
            })
        });
        settle(&mut rt);
        press(&mut rt, "switcher-row-0");
        assert_eq!(open_room_name(&rt), "Book Club");
    }

    #[test]
    fn a_highlighted_room_that_goes_away_leaves_the_top_row_highlighted() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        open_switcher(&mut rt);
        rt.key(bare(rune::Key::Down));
        settle(&mut rt);
        rt.update(|s, _| {
            s.chat
                .edit_rooms(|rooms| rooms.retain(|r| r.name != "Rust Help"))
        });
        settle(&mut rt);
        let hits = rt.state.chat.switcher_hits();
        assert_eq!(rt.state.chat.switcher_selected(&hits), 0);
        rt.key(bare(rune::Key::Enter));
        settle(&mut rt);
        assert_eq!(open_room_name(&rt), "Design Crew");
    }

    #[test]
    fn a_member_who_leaves_while_the_switcher_is_open_is_not_opened() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        open_switcher(&mut rt);
        type_text(&mut rt, "pri");
        let hits = rt.state.chat.switcher_hits();
        assert!(matches!(&hits[0].target, chat::switcher::Target::Member(_)));
        rt.update(|s, _| {
            s.chat.edit_rooms(|rooms| {
                rooms[0].members.retain(|m| m.nickname != "Priya Raman");
            })
        });
        settle(&mut rt);
        assert!(rt.state.chat.switcher_hits().is_empty());
        rt.key(bare(rune::Key::Enter));
        settle(&mut rt);
        assert!(rt.state.chat.member_modal.is_none());
    }

    #[test]
    fn editing_the_query_resets_the_highlight_even_when_the_old_text_comes_back() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        open_switcher(&mut rt);
        type_text(&mut rt, "a");
        for _ in 0..3 {
            rt.key(bare(rune::Key::Down));
        }
        settle(&mut rt);
        let hits = rt.state.chat.switcher_hits();
        assert_eq!(rt.state.chat.switcher_selected(&hits), 3);
        type_text(&mut rt, "b");
        rt.key(bare(rune::Key::Backspace));
        settle(&mut rt);
        assert_eq!(
            rt.state.chat.switcher.as_ref().expect("open").query.text,
            "a"
        );
        let hits = rt.state.chat.switcher_hits();
        assert_eq!(rt.state.chat.switcher_selected(&hits), 0);
    }

    #[test]
    fn typing_after_opening_from_the_composer_goes_to_the_switcher_only() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.update(|s, _| s.chat.composer.focused = true);
        settle(&mut rt);
        type_text(&mut rt, "x");
        assert_eq!(
            rt.state.chat.composer.text, "x",
            "the composer had the keyboard"
        );
        open_switcher(&mut rt);
        type_text(&mut rt, "pr");
        assert_eq!(
            rt.state.chat.switcher.as_ref().expect("open").query.text,
            "pr"
        );
        assert_eq!(
            rt.state.chat.composer.text, "x",
            "nothing reached the composer"
        );
    }

    #[test]
    fn the_composer_has_the_keyboard_back_after_escape_and_after_choosing() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.update(|s, _| s.chat.composer.focused = true);
        settle(&mut rt);
        open_switcher(&mut rt);
        rt.key(bare(rune::Key::Escape));
        settle(&mut rt);
        type_text(&mut rt, "a");
        assert_eq!(rt.state.chat.composer.text, "a", "after Escape");
        // Choosing a room: its composer takes the text.
        open_switcher(&mut rt);
        type_text(&mut rt, "rust");
        rt.key(bare(rune::Key::Enter));
        settle(&mut rt);
        assert_eq!(open_room_name(&rt), "Rust Help");
        type_text(&mut rt, "b");
        assert_eq!(rt.state.chat.composer.text, "b", "after choosing a room");
    }

    #[test]
    fn more_than_eight_results_show_a_window_that_follows_the_highlight() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.update(|s, _| {
            s.chat.edit_rooms(|rooms| {
                let base = rooms[0].clone();
                for i in 0..20 {
                    let mut r = base.clone();
                    r.key = format!("extra-{i}");
                    r.name = format!("Extra {i}");
                    rooms.push(r);
                }
            })
        });
        open_switcher(&mut rt);
        assert!(rt.state.chat.switcher_hits().len() > chat::switcher::VISIBLE_ROWS);
        let drawn = |rt: &Runtime<RiverApp>| {
            (0..40)
                .filter(|i| rt.find(&format!("switcher-row-{i}")).is_some())
                .collect::<Vec<_>>()
        };
        assert_eq!(drawn(&rt), (0..8).collect::<Vec<_>>());
        assert!(rt.find("switcher-more").is_some());
        for _ in 0..10 {
            rt.key(bare(rune::Key::Down));
        }
        settle(&mut rt);
        let rows = drawn(&rt);
        assert_eq!(rows.len(), 8);
        assert_eq!(
            rows.last(),
            Some(&10),
            "the highlighted row is the last shown"
        );
        press(&mut rt, "switcher-row-10");
        assert!(rt.state.chat.switcher.is_none());
    }

    #[test]
    fn muted_and_mentions_only_rooms_do_not_rank_as_unread() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        let design = room_key(&rt, "Design Crew");
        let rust = room_key(&rt, "Rust Help");
        rt.update(move |s, _| {
            s.chat
                .set_mode(&design, rooms::modes::NotificationMode::Muted);
            s.chat
                .set_mode(&rust, rooms::modes::NotificationMode::MentionsAndReplies);
        });
        open_switcher(&mut rt);
        let order: Vec<String> = rt
            .state
            .chat
            .switcher_hits()
            .iter()
            .take(2)
            .map(|h| match &h.target {
                chat::switcher::Target::Room(k) => k.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        // Muted counts as none. A mentions-only room ranks only for the messages that mention
        // you, which the example's two do not; so both fall back to the room list's order.
        let town = room_key(&rt, "Town Square");
        let design = room_key(&rt, "Design Crew");
        assert_eq!(order, [town, design]);
    }

    #[test]
    fn only_a_web_link_gets_a_menu_and_its_address_is_trimmed() {
        let press = rune::PointerPress { x: 1.0, y: 2.0 };
        let mut chat = chat::Chat::new(chat::Source::Example, Vec::new(), 0, 0, 0);
        for no in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            &format!("{}:member", chat::model::mention_scheme()),
        ] {
            chat.open_link_menu(no, press);
            assert!(chat.link_menu.is_none(), "{no}");
        }
        chat.open_link_menu(" https://freenet.org/ ", press);
        assert_eq!(chat.link_menu.as_ref().unwrap().0, "https://freenet.org/");
    }

    #[test]
    fn the_link_menu_opens_or_copies_the_link() {
        let url = "https://freenet.org/";
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        assert!(rt.find("link-menu-item0").is_none());
        rt.update(move |s, _| {
            s.chat.link_menu = Some((url.to_owned(), rune::PointerPress { x: 400.0, y: 300.0 }));
        });
        settle(&mut rt);
        press(&mut rt, "link-menu-item1");
        assert_eq!(board.text().as_deref().map(String::as_str), Some(url));
        assert!(rt.state.chat.link_menu.is_none(), "the menu closes");
        assert!(rt.take_url_requests().is_empty(), "copying opens nothing");
        rt.update(move |s, _| {
            s.chat.link_menu = Some((url.to_owned(), rune::PointerPress { x: 400.0, y: 300.0 }));
        });
        settle(&mut rt);
        press(&mut rt, "link-menu-item0");
        assert_eq!(rt.take_url_requests(), [url]);
        assert!(rt.state.chat.link_menu.is_none());
    }

    #[test]
    fn the_create_dialog_says_what_is_wrong_and_sends_what_is_right() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "create-room");
        assert!(rt.find("create-room-dialog").is_some());
        // No name: said in the dialog, nothing sent.
        press(&mut rt, "form-submit");
        assert!(rt.find("form-error").is_some());
        assert!(rt.state.chat.outgoing.is_empty());
        rt.update(|s, _| {
            s.chat.form.name.replace_text("Book club");
            s.chat.form.nickname.replace_text("Ada");
            s.chat.form.private = true;
        });
        press(&mut rt, "form-submit");
        assert!(rt.find("create-room-dialog").is_none(), "the dialog closes");
        assert_eq!(
            rt.state.chat.outgoing,
            [rooms::Command {
                room: String::new(),
                action: rooms::Action::CreateRoom {
                    name: "Book club".into(),
                    nickname: "Ada".into(),
                    private: true,
                },
            }]
        );
    }

    #[test]
    fn the_join_dialog_takes_a_pasted_link_and_refuses_what_is_not_an_invite() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "enter-invite");
        assert!(rt.find("join-room-dialog").is_some());
        rt.update(|s, _| {
            s.chat.form.code.replace_text("not an invite");
            s.chat.form.nickname.replace_text("Bea");
        });
        press(&mut rt, "form-submit");
        assert!(rt.find("form-error").is_some());
        assert!(rt.state.chat.outgoing.is_empty());
        let plan = rooms::create::new_room(
            ed25519_dalek::SigningKey::from_bytes(&[3; 32]),
            "Room",
            "Olive",
            false,
        )
        .unwrap();
        let room = rooms::room::LiveRoom::new(rooms::work::stored(&plan.room));
        let (_, link) = rooms::create::invitation(&room, "http://x/").unwrap();
        let code = link.split_once("?invitation=").unwrap().1.to_owned();
        let pasted = link.clone();
        rt.update(move |s, _| s.chat.form.code.replace_text(&pasted));
        press(&mut rt, "form-submit");
        assert_eq!(
            rt.state.chat.outgoing,
            [rooms::Command {
                room: String::new(),
                action: rooms::Action::Join {
                    code: rooms::InviteCode(code),
                    nickname: "Bea".into(),
                },
            }]
        );
    }

    /// An invite link into a room owned by seed `owner`, invited by the owner, and its code.
    fn invite_link_for(owner: u8) -> (String, String) {
        let plan = rooms::create::new_room(
            ed25519_dalek::SigningKey::from_bytes(&[owner; 32]),
            "Room",
            "Olive",
            false,
        )
        .unwrap();
        let room = rooms::room::LiveRoom::new(rooms::work::stored(&plan.room));
        let (_, link) = rooms::create::invitation(&room, "http://x/").unwrap();
        let code = link.split_once("?invitation=").unwrap().1.to_owned();
        (link, code)
    }

    /// Pasting an invitation offers the nickname its key gives, which a typed one is not
    /// replaced by. Joining sends what was in the box, and the box (a bearer credential) goes
    /// with the dialog.
    #[test]
    fn pasting_an_invitation_offers_a_generated_nickname_that_can_be_edited() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "enter-invite");
        let (link, code) = invite_link_for(9);
        type_text(&mut rt, &link);
        let invitee = rooms::create::decode_invitation(&link)
            .unwrap()
            .invitee
            .member
            .member_vk;
        assert_eq!(
            rt.state.chat.form.nickname.text,
            chat::visual_fold::generate_default_nickname(&invitee)
        );
        assert!(matches!(
            rt.state.chat.form.join,
            chat::JoinPreview::New { .. }
        ));
        assert!(rt.find("join-note").is_some());
        // Edited, then the code edited again: what was typed stays.
        rt.update(|s, _| s.chat.form.nickname.replace_text("Bea"));
        rt.update(|s, _| s.chat.join_code_edited());
        assert_eq!(rt.state.chat.form.nickname.text, "Bea");
        press(&mut rt, "form-submit");
        assert_eq!(
            rt.state.chat.outgoing,
            [rooms::Command {
                room: String::new(),
                action: rooms::Action::Join {
                    code: rooms::InviteCode(code),
                    nickname: "Bea".into(),
                },
            }]
        );
        assert!(rt.find("join-room-dialog").is_none());
        assert!(
            rt.state.chat.form.code.text.is_empty(),
            "the paste box is cleared"
        );
    }

    #[test]
    fn an_offered_nickname_follows_the_invitation_pasted() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "enter-invite");
        let (a, _) = invite_link_for(9);
        let (b, _) = invite_link_for(10);
        rt.update(move |s, _| {
            s.chat.form.code.replace_text(&a);
            s.chat.join_code_edited();
        });
        let first = rt.state.chat.form.nickname.text.clone();
        let b2 = b.clone();
        rt.update(move |s, _| {
            s.chat.form.code.replace_text(&b2);
            s.chat.join_code_edited();
        });
        let invitee = rooms::create::decode_invitation(&b)
            .unwrap()
            .invitee
            .member
            .member_vk;
        let second = rt.state.chat.form.nickname.text.clone();
        assert_eq!(
            second,
            chat::visual_fold::generate_default_nickname(&invitee)
        );
        assert!(!first.is_empty() && !second.is_empty());
    }

    #[test]
    fn an_invitation_to_a_room_already_joined_opens_that_room_and_joins_nothing() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        let (link, _) = invite_link_for(11);
        let owner = rooms::create::decode_invitation(&link).unwrap().room;
        // The room is in the list, not the one open.
        let key = bs58::encode(owner.to_bytes()).into_string();
        rt.update({
            let key = key.clone();
            move |s, _| {
                let mut rooms = s.chat.rooms().to_vec();
                rooms[2].key = key;
                rooms[2].name = "Garden".into();
                s.chat.set_rooms(rooms);
            }
        });
        assert_ne!(rt.state.chat.room().unwrap().key, key);
        press(&mut rt, "enter-invite");
        rt.update(move |s, _| {
            s.chat.form.code.replace_text(&link);
            s.chat.join_code_edited();
        });
        settle(&mut rt);
        assert!(matches!(
            rt.state.chat.form.join,
            chat::JoinPreview::Member { .. }
        ));
        assert!(
            rt.find("join-nickname").is_none(),
            "no nickname is asked for"
        );
        press(&mut rt, "form-submit");
        assert!(rt.find("join-room-dialog").is_none());
        assert_eq!(rt.state.chat.room().unwrap().key, key, "the room is opened");
        assert!(rt.state.chat.outgoing.is_empty(), "nothing is joined");
    }

    #[test]
    fn the_join_dialog_says_what_is_wrong_with_a_paste_as_it_is_typed() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "enter-invite");
        type_text(&mut rt, "https://example.org/?invitation=3mJr7AoUXx2Wqd");
        assert!(matches!(
            rt.state.chat.form.join,
            chat::JoinPreview::Refused(_)
        ));
        assert!(rt.find("join-note").is_some());
        press(&mut rt, "form-submit");
        assert!(rt.state.chat.outgoing.is_empty());
        // The reason is said once, in the note the preview already shows.
        assert!(rt.state.chat.form.error.is_some());
        assert!(rt.find("form-error").is_none());
        // A forged payload (names one room, grants another) is refused with its own message.
        let (link, _) = invite_link_for(12);
        let mut inv = rooms::create::decode_invitation(&link).unwrap();
        inv.room = ed25519_dalek::SigningKey::from_bytes(&[13; 32]).verifying_key();
        let forged = inv.to_encoded_string().unwrap();
        rt.update(move |s, _| {
            s.chat.form.code.replace_text(&forged);
            s.chat.join_code_edited();
        });
        let chat::JoinPreview::Refused(why) = rt.state.chat.form.join.clone() else {
            panic!("a forged invitation is refused");
        };
        assert!(why.contains("different room"), "{why}");
    }

    #[test]
    fn the_join_preview_holds_no_part_of_the_invitation() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "enter-invite");
        let (link, code) = invite_link_for(14);
        rt.update(move |s, _| {
            s.chat.form.code.replace_text(&link);
            s.chat.join_code_edited();
        });
        let shown = format!("{:?}", rt.state.chat.form.join);
        assert!(!shown.contains(&code[..24]), "{shown}");
    }

    /// The web UI's invite message, built from its own source rather than from native's copy of
    /// the text: the `format!` template in `invite_member_modal.rs`, with the room name and link
    /// put in.
    fn web_invite_message(name: &str, link: &str) -> String {
        let src = include_str!("../../../ui/src/components/members/invite_member_modal.rs")
            .replace("\r\n", "\n");
        let start = src
            .find("let default_msg = format!(")
            .expect("the web template");
        let end = src[start..]
            .find("room_name, invite_url")
            .expect("its arguments")
            + start;
        let mut template = String::new();
        for line in src[start..end].lines().skip(1) {
            let line = line.trim();
            template.push_str(line.strip_suffix('\\').unwrap_or(line));
        }
        let template = template
            .trim_start_matches('"')
            .trim_end_matches([',', ' '])
            .trim_end_matches('"')
            .replace("\\\"", "\"")
            .replace("\\n", "\n");
        template.replacen("{}", name, 1).replacen("{}", link, 1)
    }

    #[test]
    fn native_invite_message_is_the_web_uis() {
        let web = web_invite_message("Town Square", "http://x/?invitation=abc");
        assert!(web.starts_with("You've been invited to join the chat room \"Town Square\"!"));
        assert_eq!(
            rooms::create::invitation_message("Town Square", "http://x/?invitation=abc"),
            web
        );
    }

    #[test]
    fn copy_message_and_copy_link_copy_what_they_say_and_do_nothing_before_an_invitation() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_sized(&board, (1280.0, 1600.0));
        let open = rt.state.chat.room().unwrap().key.clone();
        let name = rt.state.chat.room().unwrap().name.clone();
        // Before any invitation there is no message and nothing to press.
        assert_eq!(rt.state.chat.invite_message(), None);
        assert!(rt.find("invite-copy-message").is_none());
        press(&mut rt, "invite");
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Invite {
                    room: open,
                    link: Ok("http://x/?invitation=abc".into()),
                },
                tx,
            )
        });
        settle(&mut rt);
        // Making the invitation copied the link.
        assert_eq!(
            board.text().as_deref().map(String::as_str),
            Some("http://x/?invitation=abc")
        );
        board.set_text("something else");
        press(&mut rt, "invite-copy-message");
        assert_eq!(
            board.text().as_deref().map(String::as_str),
            Some(web_invite_message(&name, "http://x/?invitation=abc").as_str())
        );
        press(&mut rt, "invite-copy");
        assert_eq!(
            board.text().as_deref().map(String::as_str),
            Some("http://x/?invitation=abc")
        );
        // A failed invitation leaves no message to copy.
        let open = rt.state.chat.room().unwrap().key.clone();
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Invite {
                    room: open,
                    link: Err("no key".into()),
                },
                tx,
            )
        });
        settle(&mut rt);
        assert_eq!(rt.state.chat.invite_message(), None);
        assert!(rt.find("invite-copy-message").is_none());
    }

    /// Paste into the focused box as the runtime does for the paste shortcut: the clipboard's
    /// text goes into the focused editor and its change hook runs.
    fn paste(rt: &mut Runtime<RiverApp>, board: &rune::clipboard::MemoryClipboard, text: &str) {
        board.set_text(text);
        rt.paste(text);
        settle(rt);
    }

    #[test]
    fn pasting_with_the_chord_decides_the_invitation_and_offers_a_nickname() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "enter-invite");
        let (link, code) = invite_link_for(15);
        paste(&mut rt, &board, &link);
        assert!(
            matches!(rt.state.chat.form.join, chat::JoinPreview::New { .. }),
            "{:?}",
            rt.state.chat.form.join
        );
        let invitee = rooms::create::decode_invitation(&link)
            .unwrap()
            .invitee
            .member
            .member_vk;
        assert_eq!(
            rt.state.chat.form.nickname.text,
            chat::visual_fold::generate_default_nickname(&invitee)
        );
        press(&mut rt, "form-submit");
        let [
            rooms::Command {
                action:
                    rooms::Action::Join {
                        code: sent,
                        nickname,
                    },
                ..
            },
        ] = rt.state.chat.outgoing.as_slice()
        else {
            panic!("one join");
        };
        assert_eq!(sent.0, code);
        assert_eq!(
            nickname,
            &chat::visual_fold::generate_default_nickname(&invitee)
        );
    }

    #[test]
    fn a_nickname_typed_before_a_paste_is_kept_through_pastes_of_other_invitations() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "enter-invite");
        // Type the nickname first: the code box is focused, so the nickname box is clicked.
        press(&mut rt, "join-nickname");
        type_text(&mut rt, "Bea");
        assert_eq!(rt.state.chat.form.nickname.text, "Bea");
        press(&mut rt, "invite-code");
        let (a, _) = invite_link_for(16);
        let (b, b_code) = invite_link_for(17);
        paste(&mut rt, &board, &a);
        assert!(matches!(
            rt.state.chat.form.join,
            chat::JoinPreview::New { .. }
        ));
        assert_eq!(rt.state.chat.form.nickname.text, "Bea");
        // Replace the code with another invitation: the typed name still stays.
        rt.update(|s, _| s.chat.form.code.replace_text(""));
        paste(&mut rt, &board, &b);
        assert_eq!(rt.state.chat.form.nickname.text, "Bea");
        press(&mut rt, "form-submit");
        assert_eq!(
            rt.state.chat.outgoing,
            [rooms::Command {
                room: String::new(),
                action: rooms::Action::Join {
                    code: rooms::InviteCode(b_code),
                    nickname: "Bea".into(),
                },
            }]
        );
    }

    #[test]
    fn a_room_in_the_list_that_you_are_not_a_member_of_is_not_called_not_needed() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        let (link, _) = invite_link_for(18);
        let owner = rooms::create::decode_invitation(&link).unwrap().room;
        let key = bs58::encode(owner.to_bytes()).into_string();
        for (blocked, expect_member) in [(None, true), (Some("Removed".to_owned()), false)] {
            let key = key.clone();
            let link = link.clone();
            rt.update(move |s, _| {
                let mut rooms = s.chat.rooms().to_vec();
                rooms[2].key = key;
                rooms[2].send_block = blocked;
                s.chat.set_rooms(rooms);
                s.chat.open_dialog(chat::DialogKind::Join);
                s.chat.form.code.replace_text(&link);
                s.chat.join_code_edited();
            });
            assert_eq!(
                matches!(rt.state.chat.form.join, chat::JoinPreview::Member { .. }),
                expect_member,
                "{:?}",
                rt.state.chat.form.join
            );
            assert!(matches!(
                rt.state.chat.form.join,
                chat::JoinPreview::Member { .. } | chat::JoinPreview::Have { .. }
            ));
            rt.update(|s, _| s.chat.close_dialog());
        }
    }

    #[test]
    fn an_unchanged_code_is_not_decoded_again() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "enter-invite");
        let (link, _) = invite_link_for(19);
        rt.update(move |s, _| {
            s.chat.form.code.replace_text(&link);
            s.chat.join_code_edited();
            // A change the edit hook sees that is not a change of the code (surrounding space).
            s.chat.form.join = chat::JoinPreview::Empty;
            s.chat
                .form
                .code
                .replace_text(format!("{} ", s.chat.form.code.text));
            s.chat.join_code_edited();
            assert_eq!(
                s.chat.form.join,
                chat::JoinPreview::Empty,
                "not decoded again"
            );
        });
    }

    /// An identity token for a member (seed `member`) of the room owned by seed `owner`.
    fn identity_token(owner: u8, member: u8) -> (String, ed25519_dalek::VerifyingKey) {
        use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
        let owner = ed25519_dalek::SigningKey::from_bytes(&[owner; 32]);
        let member = ed25519_dalek::SigningKey::from_bytes(&[member; 32]);
        let owner_id = MemberId::from(&owner.verifying_key());
        let export = river_core::room_state::identity::IdentityExport {
            room_owner: owner.verifying_key(),
            signing_key: member.clone(),
            authorized_member: AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: member.verifying_key(),
                },
                &owner,
            ),
            invite_chain: vec![],
            member_info: None,
            room_name: Some("Garden".into()),
            self_nickname: Some("Bea".into()),
            invitation_secrets: Default::default(),
        };
        (export.to_armored_string(), owner.verifying_key())
    }

    #[test]
    fn saving_one_preference_keeps_the_others_and_never_overwrites_an_unreadable_file() {
        let dir = std::env::temp_dir().join(format!("river-prefs-test-{}", std::process::id()));
        let path = dir.join("prefs.json");
        merge_pref(&path, "a", &1).unwrap();
        merge_pref(&path, "b", &true).unwrap();
        let both: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(both, serde_json::json!({"a": 1, "b": true}));
        std::fs::write(&path, "not json").unwrap();
        assert!(merge_pref(&path, "c", &2).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_do_not_disturb_entry_opens_its_dialog_and_the_switch_shows_in_the_footer() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        rt.update(|s, _| {
            s.chat.dnd.pinned = true;
            s.chat.dnd.now = 12 * 60;
        });
        settle(&mut rt);
        assert!(!rt.state.chat.dnd.active());
        press(&mut rt, "dnd");
        assert!(rt.find("dnd-dialog").is_some());
        press(&mut rt, "dnd-switch");
        assert!(rt.state.chat.dnd.active());
        press(&mut rt, "dnd-schedule");
        assert!(rt.find("dnd-start").is_some());
        press(&mut rt, "dnd-close");
        assert!(rt.find("dnd-dialog").is_none());
        assert!(
            rt.state.chat.dnd.settings.manual,
            "kept after the dialog closes"
        );
    }

    #[test]
    fn the_background_entry_opens_the_dialog_and_its_boxes_reach_the_shell() {
        struct Recorder(std::sync::Mutex<Vec<String>>);
        impl chat::BackgroundHost for Recorder {
            fn tray_available(&self) -> bool {
                true
            }
            fn close_to_tray(&self) -> bool {
                true
            }
            fn set_close_to_tray(&self, on: bool) -> Result<(), String> {
                self.0.lock().unwrap().push(format!("close {on}"));
                Ok(())
            }
            fn start_at_login(&self) -> bool {
                false
            }
            fn set_start_at_login(&self, on: bool) -> Result<(), String> {
                self.0.lock().unwrap().push(format!("login {on}"));
                Ok(())
            }
        }
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        assert!(
            rt.find("background").is_none(),
            "no entry without a shell that offers one"
        );
        let host = std::sync::Arc::new(Recorder(Default::default()));
        let shell = host.clone();
        rt.update(move |s, _| s.chat.background = Some(chat::BackgroundUi::new(shell)));
        settle(&mut rt);
        press(&mut rt, "background");
        assert!(rt.find("background-dialog").is_some());
        press(&mut rt, "background-start-at-login");
        press(&mut rt, "background-close-to-tray");
        assert_eq!(*host.0.lock().unwrap(), ["login true", "close false"]);
        assert!(
            rt.find("background-quit").is_some(),
            "a way out of a hidden River"
        );
        press(&mut rt, "background-close");
        assert!(rt.find("background-dialog").is_none());
    }

    #[test]
    fn node_status_opens_from_the_connection_dot_and_settings_and_copies_diagnostics() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        rt.update(|s, tx| s.apply_rooms(RoomsUpdate::Access(Access::Ready), tx));
        settle(&mut rt);
        assert!(rt.find("node-status-dialog").is_none());
        press(&mut rt, "connection-pill");
        assert!(rt.find("node-status-dialog").is_some());
        assert!(rt.find("ns-connection").is_some());
        assert!(rt.find("ns-peers").is_some());
        assert_eq!(board.text(), None, "nothing is copied until the user asks");
        assert!(
            rt.elements()
                .into_iter()
                .filter_map(|e| e.text)
                .any(|t| t.contains("includes node address")),
            "the button says the node address is copied"
        );
        press(&mut rt, "ns-copy");
        let copied: String = (*board.text().expect("diagnostics were copied")).clone();
        assert!(copied.starts_with("River node status\n"), "{copied}");
        assert!(
            copied.contains("Node host and port (identifying if remote): 127.0.0.1:7509"),
            "{copied}"
        );
        for room in rt.state.chat.rooms() {
            assert!(!copied.contains(&room.name), "{}", room.name);
        }
        press(&mut rt, "ns-close");
        assert!(rt.find("node-status-dialog").is_none());
        // From Settings.
        press(&mut rt, "settings");
        press(&mut rt, "settings-node-status");
        assert!(rt.find("node-status-dialog").is_some());
        assert!(rt.find("settings-dialog").is_none());
    }

    #[test]
    fn node_status_is_reachable_before_the_rooms_show() {
        let mut state = RiverApp::detached();
        state.setup = Setup::Connected {
            version: "0.2.158".into(),
        };
        state.access = Access::WaitingForApproval {
            approve_url: "http://127.0.0.1:7509/x".into(),
        };
        let mut rt = Runtime::new(
            state,
            view,
            (1280.0, 800.0),
            Env {
                system_dark: true,
                scale: 1.0,
            },
        );
        rt.frame(0.0);
        press(&mut rt, "node-status-open");
        assert!(rt.find("node-status-dialog").is_some());
        // The window's per-frame hook (`spec().poll`) is not run by the test runtime.
        rt.update(|s, tx| s.sync_node_status(tx));
        let at = rt.state.chat.status.connection;
        assert_eq!(at, chat::node_status::Connection::WaitingForApproval);
    }

    #[test]
    fn export_id_shows_the_warning_and_copies_only_when_asked() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        let open = rt.state.chat.room().unwrap().key.clone();
        press(&mut rt, "export-id");
        assert!(rt.find("export-identity-dialog").is_some());
        assert_eq!(
            rt.state.chat.outgoing,
            [rooms::Command {
                room: open.clone(),
                action: rooms::Action::ExportId,
            }]
        );
        assert!(
            rt.find("export-status").is_some(),
            "says it is preparing the token"
        );
        assert!(
            rt.find("export-warning").is_some(),
            "the private-key warning shows at once"
        );
        let (token, _) = identity_token(3, 70);
        let (room, shown) = (open.clone(), token.clone());
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Export {
                    room,
                    token: Ok(shown),
                },
                tx,
            )
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.find("export-token").is_some());
        assert!(rt.find("export-status").is_none());
        assert_eq!(board.text(), None, "nothing is copied until the user asks");
        press(&mut rt, "export-copy");
        assert_eq!(
            board.text().as_deref().map(String::as_str),
            Some(token.as_str())
        );
        assert!(rt.state.chat.export.as_ref().is_some_and(|d| d.copied));
        // The token is never in a printed state.
        let printed = format!("{:?}", rt.state.chat.export);
        assert!(
            !printed.contains("BEGIN") && !printed.contains(&token[40..80]),
            "{printed}"
        );
        press(&mut rt, "export-close");
        assert!(rt.find("export-identity-dialog").is_none());
        assert!(rt.state.chat.export.is_none(), "closing drops the token");
    }

    #[test]
    fn an_export_that_arrives_after_the_dialog_closed_is_dropped() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        let open = rt.state.chat.room().unwrap().key.clone();
        press(&mut rt, "export-id");
        press(&mut rt, "export-close");
        let (token, _) = identity_token(3, 70);
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Export {
                    room: open,
                    token: Ok(token),
                },
                tx,
            )
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.state.chat.export.is_none());
    }

    #[test]
    fn an_export_that_cannot_be_made_says_why_and_cannot_be_copied() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        let open = rt.state.chat.room().unwrap().key.clone();
        press(&mut rt, "export-id");
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Export {
                    room: open,
                    token: Err(
                        "Cannot export: the signing key for this room is not stored here.".into(),
                    ),
                },
                tx,
            )
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.find("export-error").is_some());
        assert!(rt.find("export-token").is_none());
        press(&mut rt, "export-copy");
        assert_eq!(board.text(), None, "a disabled Copy copies nothing");
    }

    #[test]
    fn import_id_checks_the_token_and_sends_it_for_a_room_not_in_the_list() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        press(&mut rt, "import-id");
        assert!(rt.find("import-identity-dialog").is_some());
        press(&mut rt, "form-submit");
        assert!(rt.find("form-error").is_some(), "an empty box is said");
        rt.update(|s, _| s.chat.form.token.replace_text("not a token"));
        press(&mut rt, "form-submit");
        assert!(
            rt.state
                .chat
                .form
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("Invalid token: ")),
            "{:?}",
            rt.state.chat.form.error
        );
        assert!(rt.state.chat.outgoing.is_empty());
        let (token, _) = identity_token(3, 70);
        let pasted = token.clone();
        rt.update(move |s, _| s.chat.form.token.replace_text(&pasted));
        press(&mut rt, "form-submit");
        assert!(
            rt.find("import-identity-dialog").is_none(),
            "the dialog closes"
        );
        assert_eq!(
            rt.state.chat.outgoing,
            [rooms::Command {
                room: String::new(),
                action: rooms::Action::ImportId {
                    token: rooms::IdentityToken(token),
                    replace: false,
                },
            }]
        );
        assert!(
            rt.state.chat.form.token.text.is_empty(),
            "the pasted key is dropped"
        );
    }

    #[test]
    fn import_id_for_a_room_in_the_list_asks_before_replacing_and_sends_what_it_asked_about() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        let (token_a, owner_a) = identity_token(3, 70);
        let (token_b, _) = identity_token(4, 71);
        // A room in the list that is the token's.
        rt.update(move |s, _| {
            let mut room = s.chat.rooms()[0].clone();
            room.key = bs58::encode(owner_a.to_bytes()).into_string();
            s.chat.set_rooms(vec![room]);
        });
        press(&mut rt, "import-id");
        let a = token_a.clone();
        rt.update(move |s, _| s.chat.form.token.replace_text(&a));
        press(&mut rt, "form-submit");
        assert!(rt.find("import-identity-replace-warning").is_some());
        assert!(
            rt.find("import-identity-replace-multitab-warning")
                .is_some()
        );
        assert!(
            rt.state.chat.outgoing.is_empty(),
            "nothing is sent before the user confirms"
        );
        // Cancel: back to the box, the paste kept.
        press(&mut rt, "import-identity-replace-cancel");
        assert!(rt.find("import-identity-replace-warning").is_none());
        assert_eq!(rt.state.chat.form.token.text, token_a);
        // Ask again, then edit the box to another room's token without going through the
        // box's change hook: Replace still imports the token the warning was about.
        press(&mut rt, "form-submit");
        let b = token_b.clone();
        rt.update(move |s, _| s.chat.form.token.replace_text(&b));
        press(&mut rt, "import-identity-replace-confirm");
        assert_eq!(
            rt.state.chat.outgoing,
            [rooms::Command {
                room: String::new(),
                action: rooms::Action::ImportId {
                    token: rooms::IdentityToken(token_a.clone()),
                    replace: true,
                },
            }]
        );
        assert!(rt.find("import-identity-dialog").is_none());
        // Editing the box drops the warning.
        press(&mut rt, "import-id");
        let a = token_a.clone();
        rt.update(move |s, _| s.chat.form.token.replace_text(&a));
        press(&mut rt, "form-submit");
        assert!(rt.find("import-identity-replace-warning").is_some());
        rt.update(|s, _| {
            s.chat.form.token.replace_text("x");
            s.chat.import_token_edited();
        });
        settle(&mut rt);
        assert!(rt.find("import-identity-replace-warning").is_none());
        assert!(rt.find("import-identity-replace-confirm").is_none());
    }

    /// Ctrl+Backspace edits the box by word without the field's change hook: the warning raised
    /// for the text before it must still go, and Replace must not import the old token.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_word_key_edit_while_the_replace_warning_is_up_drops_the_warning() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        let (token_a, owner_a) = identity_token(3, 70);
        rt.update(move |s, _| {
            let mut room = s.chat.rooms()[0].clone();
            room.key = bs58::encode(owner_a.to_bytes()).into_string();
            s.chat.set_rooms(vec![room]);
        });
        press(&mut rt, "import-id");
        let a = token_a.clone();
        rt.update(move |s, _| s.chat.form.token.replace_text(&a));
        press(&mut rt, "form-submit");
        assert!(rt.find("import-identity-replace-warning").is_some());
        rt.update(|s, _| s.chat.form.token.focused = true);
        settle(&mut rt);
        rt.key(rune::KeyEvent {
            key: rune::Key::Backspace,
            mods: rune::Mods {
                ctrl: true,
                ..Default::default()
            },
        });
        settle(&mut rt);
        assert_ne!(
            rt.state.chat.form.token.text, token_a,
            "the key edited the box"
        );
        assert!(
            rt.find("import-identity-replace-warning").is_none(),
            "the warning was for the text before the edit"
        );
        assert!(rt.find("import-identity-replace-confirm").is_none());
        assert!(rt.state.chat.outgoing.is_empty());
    }

    /// The replace dialog names the room you have, not what the token says it is called.
    #[test]
    fn the_replace_dialog_names_the_stored_room_not_the_tokens() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        let (token, owner) = identity_token(3, 70);
        rt.update(move |s, _| {
            let mut room = s.chat.rooms()[0].clone();
            room.key = bs58::encode(owner.to_bytes()).into_string();
            room.name = "Stored name".into();
            s.chat.set_rooms(vec![room]);
        });
        press(&mut rt, "import-id");
        rt.update(move |s, _| s.chat.form.token.replace_text(&token));
        press(&mut rt, "form-submit");
        let confirm = rt.state.chat.form.replace.as_ref().expect("asked");
        assert_eq!(confirm.room_name.as_deref(), Some("Stored name"));
        assert_ne!(
            confirm.room_name.as_deref(),
            Some("Garden"),
            "the token's name"
        );
    }

    #[test]
    fn export_and_import_are_not_offered_for_the_example_rooms() {
        let mut chat = chat::Chat::new(chat::Source::Example, Vec::new(), 0, 0, 0);
        chat.open_import();
        assert!(chat.form.open.is_none());
        chat.open_export();
        assert!(chat.export.is_none());
        assert!(chat.outgoing.is_empty());
    }

    #[test]
    fn inviting_copies_the_link_and_a_failure_is_said() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        let open = rt.state.chat.room().unwrap().key.clone();
        press(&mut rt, "invite");
        assert_eq!(
            rt.state.chat.outgoing,
            [rooms::Command {
                room: open.clone(),
                action: rooms::Action::Invite,
            }]
        );
        assert!(rt.find("invite-status").is_some(), "says it is making one");
        let room = open.clone();
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Invite {
                    room,
                    link: Ok("http://x/?invitation=abc".into()),
                },
                tx,
            )
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(
            board.text().as_deref().map(String::as_str),
            Some("http://x/?invitation=abc")
        );
        assert!(rt.find("invite-link").is_some());
        let room = open.clone();
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Invite {
                    room,
                    link: Err("This computer doesn't hold your key".into()),
                },
                tx,
            )
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.find("invite-link").is_none());
        assert!(rt.find("invite-status").is_some());
    }

    /// A finished create or join whose room has not appeared yet says so, and can be put away.
    #[test]
    fn a_finished_create_can_be_dismissed_before_its_room_shows() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        rt.update(|s, _| {
            s.chat.show_work(vec![rooms::work::WorkView {
                id: 5,
                what: "Creating “Book club”".into(),
                state: rooms::work::WorkState::Done {
                    room: "not-yet-shown".into(),
                },
            }])
        });
        settle(&mut rt);
        assert!(rt.find("work-status").is_some());
        press(&mut rt, "work-dismiss-5");
        assert_eq!(
            rt.state.chat.outgoing.last().map(|c| c.action.clone()),
            Some(rooms::Action::DismissWork { id: 5 })
        );
        assert!(rt.find("work-status").is_none());
        // And an invitation's link is never in a printed state.
        let shown = format!("{:?}", chat::InviteState::Copied("SECRETCODE".into()));
        assert!(!shown.contains("SECRETCODE"), "{shown}");
    }

    #[test]
    fn a_failed_create_shows_why_with_try_again_and_dismiss() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms(&board);
        rt.update(|s, _| {
            s.chat.show_work(vec![rooms::work::WorkView {
                id: 4,
                what: "Creating “Book club”".into(),
                state: rooms::work::WorkState::Failed {
                    why: "River's storage refused".into(),
                    retry: true,
                },
            }])
        });
        settle(&mut rt);
        assert!(rt.find("work-why").is_some());
        press(&mut rt, "work-retry-4");
        press(&mut rt, "work-dismiss-4");
        let actions: Vec<_> = rt
            .state
            .chat
            .outgoing
            .iter()
            .map(|c| c.action.clone())
            .collect();
        assert_eq!(
            actions,
            [
                rooms::Action::RetryWork { id: 4 },
                rooms::Action::DismissWork { id: 4 }
            ]
        );
        assert!(rt.find("work-why").is_none(), "dismissed at once");
    }

    /// The example rooms from the node with unread messages: a mention in Rust Help, Family muted.
    fn overview_rooms(board: &rune::clipboard::MemoryClipboard) -> Runtime<RiverApp> {
        let mut rt = node_rooms(board);
        rt.update(|s, _| {
            s.chat.edit_rooms(|rooms| {
                for r in rooms {
                    r.unread = match r.key.as_str() {
                        "design-crew" => 3,
                        "rust-help" => 2,
                        "family" => 4,
                        _ => 0,
                    };
                    if r.key == "rust-help"
                        && let Some(m) = r.messages.iter_mut().rev().find(|m| m.author != r.me)
                    {
                        m.md = format!("hi [@Sam]({}:self)", chat::model::mention_scheme());
                    }
                }
            });
            s.chat
                .set_mode("family", rooms::modes::NotificationMode::Muted);
        });
        settle(&mut rt);
        rt
    }

    #[test]
    fn the_room_lists_entry_opens_the_overview_with_the_unread_rooms_and_not_the_muted_one() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        assert!(rt.find("overview-dialog").is_none());
        press(&mut rt, "overview-open");
        assert!(rt.state.chat.overview);
        assert!(rt.find("overview-dialog").is_some());
        assert!(rt.find("overview-row-rust-help").is_some());
        assert!(rt.find("overview-row-design-crew").is_some());
        assert!(rt.find("overview-row-family").is_none(), "Family is muted");
        let rows = rt.state.chat.overview_rows();
        assert_eq!(rows[0].name, "Rust Help", "the mention comes first");
        assert_eq!(rt.state.chat.unread_summary().total, 5);
        assert_eq!(rt.state.chat.unread_summary().mentions, 1);
    }

    #[test]
    fn a_row_opens_its_room_closes_the_overview_and_stages_the_scroll() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        press(&mut rt, "overview-open");
        let want = rt.state.chat.overview_rows()[0].clone();
        press(&mut rt, "overview-row-rust-help");
        assert!(!rt.state.chat.overview);
        assert_eq!(rt.state.chat.room().expect("open").key, want.key);
        let pending = rt.state.chat.pending_reveal().expect("a scroll is staged");
        assert_eq!(Some(pending.message.as_str()), want.reveal.as_deref());
        // It waits for the history to be laid out, then goes once.
        let mut waits = 0;
        loop {
            let mut again = false;
            rt.update(|s, tx| again = s.chat.apply_reveal(tx));
            if !again {
                break;
            }
            waits += 1;
            assert!(waits < 5, "the reveal never fires");
        }
        assert!(rt.state.chat.pending_reveal().is_none());
        assert_eq!(
            rt.state.chat.unread_summary().total,
            3,
            "the room is read now"
        );
    }

    #[test]
    fn mark_all_read_clears_every_count_and_the_title() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        press(&mut rt, "overview-open");
        assert_eq!(rt.state.unread(), 5);
        press(&mut rt, "overview-mark-all");
        assert_eq!(rt.state.unread(), 0);
        assert_eq!(rt.state.chat.unread_summary(), Default::default());
        assert!(rt.state.chat.overview, "the overview stays, now empty");
        assert!(rt.find("overview-row-rust-help").is_none());
        assert!(rt.find("overview-row-design-crew").is_none());
    }

    #[test]
    fn the_overview_does_not_open_over_another_dialog_or_the_switcher() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        rt.on_key = spec().on_key;
        open_switcher(&mut rt);
        assert!(!rt.state.chat.toggle_overview());
        rt.key(bare(rune::Key::Escape));
        settle(&mut rt);
        press(&mut rt, "overview-open");
        rt.update(|s, _| {
            s.chat.pin_chord_time(None);
            assert!(!s.chat.switcher_chord(std::time::Instant::now()));
        });
        assert!(rt.state.chat.switcher.is_none());
    }

    #[test]
    fn mark_all_read_clears_only_the_rooms_the_overview_lists() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        press(&mut rt, "overview-open");
        press(&mut rt, "overview-mark-all");
        let unread = |rt: &Runtime<RiverApp>, k: &str| {
            rt.state
                .chat
                .rooms()
                .iter()
                .find(|r| r.key == k)
                .expect("room")
                .unread
        };
        assert_eq!(unread(&rt, "design-crew"), 0);
        assert_eq!(unread(&rt, "rust-help"), 0);
        assert_eq!(
            unread(&rt, "family"),
            4,
            "muted, so not listed and not cleared"
        );
    }

    #[test]
    fn mentions_only_rooms_keep_the_unread_the_overview_does_not_count() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        rt.update(|s, _| {
            s.chat.set_mode(
                "design-crew",
                rooms::modes::NotificationMode::MentionsAndReplies,
            );
            s.chat.mark_all_read();
        });
        let r = rt
            .state
            .chat
            .rooms()
            .iter()
            .find(|r| r.key == "design-crew");
        assert_eq!(r.expect("room").unread, 3, "none of it mentions you");
    }

    /// The same rooms as `overview_rooms`, as a later update from the node. `book_club` is the
    /// empty room's unread count, which the node reports as 0 for a room with no messages.
    fn node_update(rt: &mut Runtime<RiverApp>, history_in_book_club: bool) {
        let mut rooms = chat::example::data().rooms;
        for r in &mut rooms {
            r.unread = match r.key.as_str() {
                "design-crew" => 3,
                "rust-help" => 2,
                "family" => 4,
                _ => 0,
            };
            if r.key == chat::example::EMPTY_ROOM && history_in_book_club {
                let mut template = chat::example::data()
                    .rooms
                    .into_iter()
                    .find(|r| r.key == "rust-help")
                    .expect("room")
                    .messages;
                template.truncate(4);
                r.messages = template;
            }
        }
        rt.update(move |s, tx| {
            s.apply_rooms(
                RoomsUpdate::Rooms {
                    rooms,
                    open: None,
                    modes: [("family".to_owned(), rooms::modes::NotificationMode::Muted)]
                        .into_iter()
                        .collect(),
                    settled: Vec::new(),
                    status: chat::LinkStatus::Live,
                    now: 29_000_001,
                    issues: Default::default(),
                    work: Vec::new(),
                },
                tx,
            );
        });
        settle(rt);
    }

    #[test]
    fn mark_all_read_survives_the_next_update_from_the_node() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        node_update(&mut rt, false);
        assert_eq!(rt.state.chat.unread_total(), 5);
        press(&mut rt, "overview-open");
        press(&mut rt, "overview-mark-all");
        assert_eq!(rt.state.chat.unread_total(), 0);
        node_update(&mut rt, false);
        assert_eq!(rt.state.chat.unread_total(), 0, "nothing came back");
        assert!(rt.state.chat.overview_rows().is_empty());
    }

    /// An empty room carries no unread (the node reports 0 without a read position). Opening it
    /// must not make the history that arrives later count as unread.
    #[test]
    fn history_arriving_in_a_room_opened_while_empty_is_not_unread() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        node_update(&mut rt, false);
        rt.update(|s, _| s.chat.select(chat::example::EMPTY_ROOM));
        press(&mut rt, "overview-open");
        press(&mut rt, "overview-mark-all");
        assert_eq!(rt.state.chat.unread_total(), 0);
        node_update(&mut rt, true);
        let room = rt
            .state
            .chat
            .rooms()
            .iter()
            .find(|r| r.key == chat::example::EMPTY_ROOM)
            .expect("room");
        assert!(!room.messages.is_empty(), "the history arrived");
        assert_eq!(room.unread, 0);
        assert_eq!(rt.state.chat.unread_total(), 0);
    }

    /// The dialog takes Escape itself; no window-level handler is installed here, so this is
    /// the dialog's own path.
    #[test]
    fn escape_closes_the_overview_through_the_dialog() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        press(&mut rt, "overview-open");
        assert!(rt.find("overview-dialog").is_some());
        rt.key(bare(rune::Key::Escape));
        settle(&mut rt);
        assert!(!rt.state.chat.overview);
        assert!(rt.find("overview-dialog").is_none());
    }

    /// The window-level Escape leaves an open overview to the dialog.
    #[test]
    fn the_window_level_escape_does_not_act_while_the_overview_is_open() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        press(&mut rt, "overview-open");
        rt.update(|s, _| assert!(!s.chat.escape()));
        assert!(rt.state.chat.overview);
    }

    /// The per-frame hook that applies a staged scroll is `sync_unread`, which `spec().poll`
    /// runs; it must call `apply_reveal` even when the rooms are not from a node.
    #[test]
    fn the_per_frame_sync_applies_a_staged_scroll() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        press(&mut rt, "overview-open");
        press(&mut rt, "overview-row-rust-help");
        assert!(rt.state.chat.pending_reveal().is_some());
        let poll = spec().poll.expect("a poll hook");
        for _ in 0..10 {
            rt.update(|s, tx| {
                poll(s, 0.0, tx);
            });
            if rt.state.chat.pending_reveal().is_none() {
                break;
            }
        }
        assert!(rt.state.chat.pending_reveal().is_none());
    }

    /// A click on a row scrolls the long history to the first mention and leaves it there, even
    /// though the room opened stuck to its bottom. This drives `apply_reveal` directly; that the
    /// per-frame sync calls it is `the_per_frame_sync_applies_a_staged_scroll`.
    #[test]
    fn the_scroll_to_the_first_mention_lands_and_stays() {
        let board = rune::clipboard::memory();
        let mut rt = overview_rooms(&board);
        let mention = format!("hi [@Sam]({}:self)", chat::model::mention_scheme());
        let target = rt.state.chat.rooms().iter().find(|r| r.key == "rust-help");
        let template = target
            .expect("room")
            .messages
            .last()
            .expect("a message")
            .clone();
        let mut first_mention = String::new();
        rt.update(|s, _| {
            s.chat.edit_rooms(|rooms| {
                let r = rooms
                    .iter_mut()
                    .find(|r| r.key == "rust-help")
                    .expect("room");
                let base = r.messages.len();
                for i in 0..60usize {
                    let mut m = template.clone();
                    m.key = format!("long-{i}");
                    m.id = (base + i + 1) as u32;
                    m.time += i as u32 + 1;
                    m.text = format!("filler line number {i}");
                    m.md = m.text.clone();
                    // The 40th is the first unread mention; the last 20 are unread too.
                    if i == 40 {
                        m.md = mention.clone();
                    }
                    r.messages.push(m);
                }
                r.unread = 20;
                first_mention = "long-40".into();
            });
        });
        settle(&mut rt);
        press(&mut rt, "overview-open");
        press(&mut rt, "overview-row-rust-help");
        assert_eq!(
            rt.state.chat.pending_reveal().map(|r| r.message.as_str()),
            Some(first_mention.as_str())
        );
        let msg = format!("msg-{first_mention}");
        // The history viewport as laid out, not a guessed window position.
        let (view, _) = rt
            .presented("history-scroll-rust-help")
            .expect("the history is drawn");
        let inside = move |y: f32| y > view[1] && y < view[1] + view[3];
        // The room opened at its newest message, so the mention is far off screen.
        let before = rt.find(&msg).map(|(_, y)| y);
        assert!(
            before.is_none_or(|y| !inside(y)),
            "the mention starts off screen: {before:?}"
        );
        // What `sync_unread` does each frame, until the request goes out.
        for _ in 0..10 {
            let mut again = false;
            rt.update(|s, tx| again = s.chat.apply_reveal(tx));
            settle(&mut rt);
            if !again {
                break;
            }
        }
        let (_, y) = rt.find(&msg).expect("the mention is drawn");
        assert!(
            (80.0..680.0).contains(&y),
            "scrolled into the viewport: y = {y}"
        );
        // Still there after more frames: the bottom-sticking viewport does not pull it back.
        for _ in 0..5 {
            settle(&mut rt);
        }
        let (_, later) = rt.find(&msg).expect("still drawn");
        assert!((y - later).abs() < 1.0, "it moved from {y} to {later}");
    }

    // Message search (Ctrl+F).

    /// The open room with 60 more messages after its own, and one "needle" among the oldest.
    fn long_searchable_room(board: &rune::clipboard::MemoryClipboard) -> Runtime<RiverApp> {
        long_searchable_room_sized(board, (1280.0, 800.0))
    }

    fn long_searchable_room_sized(
        board: &rune::clipboard::MemoryClipboard,
        size: (f32, f32),
    ) -> Runtime<RiverApp> {
        let mut rt = node_rooms_sized(board, size);
        rt.on_key = spec().on_key;
        rt.update(|s, _| {
            s.chat.edit_rooms(|rooms| {
                let r = &mut rooms[0];
                let template = r.messages.last().expect("a message").clone();
                let base = r.messages.len();
                for i in 0..60usize {
                    let mut m = template.clone();
                    m.key = format!("long-{i}");
                    m.id = (base + i + 1) as u32;
                    m.time += i as u32 + 1;
                    m.text = format!("filler line number {i}");
                    m.md = m.text.clone();
                    m.reactions.clear();
                    if i == 3 {
                        m.text = "find the NEEDLE here".into();
                        m.md = "find the **needle** here".into();
                    }
                    r.messages.push(m);
                }
            });
        });
        settle(&mut rt);
        rt
    }

    fn type_query(rt: &mut Runtime<RiverApp>, q: &'static str) {
        rt.update(move |s, _| {
            s.chat
                .search
                .as_mut()
                .expect("the panel is open")
                .query
                .replace_text(q);
            s.chat.search_edited();
        });
        settle(rt);
    }

    #[test]
    fn the_search_chord_opens_the_panel_from_the_composer_and_a_repeat_never_closes_it() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.update(|s, _| s.chat.composer.focused = true);
        settle(&mut rt);
        rt.key(chord('f'));
        settle(&mut rt);
        assert!(rt.state.chat.search.is_some(), "Ctrl+F opened it");
        assert!(rt.find("search-panel").is_some());
        assert!(!rt.state.chat.composer.focused, "the box has the keyboard");
        // A held key repeats the chord: the panel stays, however many arrive.
        for _ in 0..4 {
            rt.key(chord('f'));
            settle(&mut rt);
        }
        assert!(
            rt.state.chat.search.is_some(),
            "a repeat does not toggle it"
        );
        rt.key(bare(rune::Key::Escape));
        settle(&mut rt);
        assert!(rt.state.chat.search.is_none(), "Escape closes it");
        assert!(
            rt.state.chat.composer.focused,
            "the composer has the keyboard again"
        );
        assert!(rt.find("search-panel").is_none());
    }

    #[test]
    fn the_search_does_not_open_over_a_dialog_or_the_switcher_and_they_close_it() {
        let board = rune::clipboard::memory();
        let mut rt = node_rooms_with_keys(&board);
        rt.update(|s, _| {
            s.chat.open_details();
            assert!(!s.chat.search_chord(), "not over the details dialog");
            s.chat.details = None;
            assert!(s.chat.search_chord());
            // Opening the switcher takes the panel's place.
            assert!(s.chat.toggle_switcher());
            assert!(s.chat.search.is_none());
            assert!(!s.chat.search_chord(), "not over the switcher");
            s.chat.close_switcher();
            assert!(s.chat.search_chord());
            // Another room's search is not this room's.
            s.chat.open_room("design-crew");
            assert!(s.chat.search.is_none());
        });
    }

    #[test]
    fn typing_lists_matches_and_the_arrows_and_enter_jump_to_the_highlighted_one() {
        let board = rune::clipboard::memory();
        let mut rt = long_searchable_room(&board);
        rt.key(chord('f'));
        settle(&mut rt);
        type_query(&mut rt, "line number 5");
        // "line number 5", "line number 50" to "59": newest first.
        let found = rt.state.chat.search_results();
        assert_eq!(found.total, 11);
        assert_eq!(found.hits[0].key, "long-59");
        assert!(rt.find("search-row-0").is_some());
        rt.key(bare(rune::Key::Down));
        settle(&mut rt);
        let hits = rt.state.chat.search_results().hits.clone();
        assert_eq!(rt.state.chat.search_selected(&hits), 1);
        rt.key(bare(rune::Key::Up));
        rt.key(bare(rune::Key::Up));
        settle(&mut rt);
        assert_eq!(rt.state.chat.search_selected(&hits), 10, "Up wraps round");
        rt.key(bare(rune::Key::Enter));
        settle(&mut rt);
        assert_eq!(
            rt.state.chat.pending_reveal().map(|r| r.message.as_str()),
            Some("long-5"),
            "Enter jumps to the highlighted (oldest) match"
        );
        assert!(
            rt.state.chat.search.is_some(),
            "the panel stays for the next match"
        );
    }

    #[test]
    fn a_match_that_is_edited_or_deleted_is_searched_as_it_is_now() {
        let board = rune::clipboard::memory();
        let mut rt = long_searchable_room(&board);
        rt.key(chord('f'));
        settle(&mut rt);
        type_query(&mut rt, "needle");
        assert_eq!(rt.state.chat.search_results().total, 1);
        // The markup is not searched, and what an edit replaced is gone.
        type_query(&mut rt, "**");
        assert_eq!(rt.state.chat.search_results().total, 0);
        rt.update(|s, _| {
            s.chat.edit_rooms(|rooms| {
                let m = rooms[0]
                    .messages
                    .iter_mut()
                    .find(|m| m.key == "long-3")
                    .expect("message");
                m.text = "now it says haystack".into();
                m.md = m.text.clone();
                m.edited = true;
            });
        });
        type_query(&mut rt, "needle");
        assert_eq!(
            rt.state.chat.search_results().total,
            0,
            "the old text is gone"
        );
        type_query(&mut rt, "haystack");
        assert_eq!(rt.state.chat.search_results().total, 1);
        rt.update(|s, _| {
            s.chat
                .edit_rooms(|rooms| rooms[0].messages.retain(|m| m.key != "long-3"))
        });
        type_query(&mut rt, "haystack");
        assert_eq!(
            rt.state.chat.search_results().total,
            0,
            "a deleted message is not found"
        );
    }

    /// A click on a result scrolls a message far from the bottom into view and leaves it
    /// there, and marks it until the frames count it out.
    #[test]
    fn a_jump_to_a_far_match_scrolls_it_into_view_and_marks_it_for_a_moment() {
        let board = rune::clipboard::memory();
        let mut rt = long_searchable_room(&board);
        rt.key(chord('f'));
        settle(&mut rt);
        type_query(&mut rt, "needle");
        let msg = "msg-long-3".to_owned();
        let (view, _) = rt
            .presented("history-scroll-town-square")
            .expect("the history is drawn");
        let inside = move |y: f32| y > view[1] && y < view[1] + view[3];
        let before = rt.find(&msg).map(|(_, y)| y);
        assert!(
            before.is_none_or(|y| !inside(y)),
            "the match starts off screen: {before:?}"
        );
        press(&mut rt, "search-row-0");
        assert_eq!(rt.state.chat.flashing(), Some("long-3"));
        for _ in 0..10 {
            let mut again = false;
            rt.update(|s, tx| again = s.chat.apply_reveal(tx));
            settle(&mut rt);
            if !again {
                break;
            }
        }
        let (_, y) = rt.find(&msg).expect("the match is drawn");
        assert!(
            (80.0..680.0).contains(&y),
            "scrolled into the viewport: y = {y}"
        );
        for _ in 0..5 {
            settle(&mut rt);
        }
        let (_, later) = rt.find(&msg).expect("still drawn");
        assert!((y - later).abs() < 1.0, "it moved from {y} to {later}");
        // The mark ends by the clock it is given: still there before its time, gone after.
        let t0 = std::time::Instant::now();
        rt.update(move |s, _| {
            s.chat.search_jump("long-3", t0);
            assert!(s.chat.tick_flash(t0 + chat::search::FLASH_FOR / 2));
            assert!(!s.chat.tick_flash(t0 + chat::search::FLASH_FOR));
        });
        assert_eq!(rt.state.chat.flashing(), None, "the mark fades");
    }

    /// At the smallest window the panel gives up rows so the history stays readable, and a
    /// jump still shows the match.
    #[test]
    fn at_the_minimum_window_size_the_history_keeps_room_and_the_jump_is_visible() {
        let board = rune::clipboard::memory();
        let mut rt = long_searchable_room_sized(&board, (640.0, 480.0));
        rt.key(chord('f'));
        settle(&mut rt);
        type_query(&mut rt, "line number");
        let (view, _) = rt
            .presented("history-scroll-town-square")
            .expect("the history is drawn");
        assert!(
            view[3] >= 100.0,
            "the history keeps room to read: {}",
            view[3]
        );
        press(&mut rt, "search-row-0");
        for _ in 0..10 {
            let mut again = false;
            rt.update(|s, tx| again = s.chat.apply_reveal(tx));
            settle(&mut rt);
            if !again {
                break;
            }
        }
        let (view, _) = rt
            .presented("history-scroll-town-square")
            .expect("the history is drawn");
        let (_, y) = rt.find("msg-long-59").expect("the match is drawn");
        assert!(
            y >= view[1] && y <= view[1] + view[3],
            "visible: {y} in {view:?}"
        );
    }

    /// With the composer grown to eight lines the history still keeps its minimum, wide or
    /// narrow, and a jump still shows the match.
    fn history_keeps_room_with_a_tall_draft(size: (f32, f32)) {
        let board = rune::clipboard::memory();
        let mut rt = long_searchable_room_sized(&board, size);
        rt.key(chord('f'));
        settle(&mut rt);
        type_query(&mut rt, "line number");
        rt.update(|s, _| s.chat.composer.replace_text(["draft line"; 8].join("\n")));
        for _ in 0..4 {
            settle(&mut rt);
        }
        let (view, _) = rt
            .presented("history-scroll-town-square")
            .expect("the history is drawn");
        assert!(
            view[3] >= chat::search::HISTORY_MIN_H - 1.0,
            "{size:?}: the history keeps room to read: {}",
            view[3]
        );
        rt.update(|s, _| s.chat.search_jump("msg-none", std::time::Instant::now()));
        rt.update(|s, _| s.chat.search_jump("long-59", std::time::Instant::now()));
        for _ in 0..10 {
            let mut again = false;
            rt.update(|s, tx| again = s.chat.apply_reveal(tx));
            settle(&mut rt);
            if !again {
                break;
            }
        }
        let (_, y) = rt.find("msg-long-59").expect("the match is drawn");
        assert!(
            y >= view[1] && y <= view[1] + view[3],
            "visible: {y} in {view:?}"
        );
    }

    #[test]
    fn a_tall_draft_at_the_minimum_window_leaves_the_history_its_room() {
        history_keeps_room_with_a_tall_draft((640.0, 480.0));
    }

    #[test]
    fn a_tall_draft_in_the_narrow_layout_leaves_the_history_its_room() {
        history_keeps_room_with_a_tall_draft(NARROW_SCENE_SIZE);
    }
}
