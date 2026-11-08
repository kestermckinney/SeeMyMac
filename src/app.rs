//! Application controller: owns the connection store, window state and live sessions.
//! Rendering lives in `view.rs`.

use crate::geometry;
use crate::hp;
use crate::model::{self, Connection, Proto, Scaling, Store};
use crate::rfb::{self, Event, Input};
use crate::settings::Settings;
use crate::state::{AppState, Mon, View};
use crate::vault;
use gpui::*;
use smallvec::SmallVec;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};


#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    Reconnecting,
    Failed(String),
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Connecting => "Connecting…",
            Status::Connected => "Connected",
            Status::Reconnecting => "Reconnecting…",
            Status::Failed(_) => "Disconnected",
        }
    }
}

pub struct Tile {
    pub image: Arc<RenderImage>,
    pub size: (u32, u32),
    /// Which remote monitor this is (0-based).
    pub monitor: usize,
    /// The monitor's real resolution, for its label. Apple HP scales a display
    /// into the canvas negotiated at connect, so `size` can differ.
    pub resolution: (u32, u32),
}

pub struct Session {
    pub conn_id: String,
    pub status: Status,
    pub handle: Option<rfb::Handle>,
    pub user: String,
    pub pass: String,
    pub scaling: Scaling,
    pub attempts: u32,
    pub retry_at: Option<Instant>,
    /// Latest progress text from the connection thread, and when this attempt began.
    pub stage: String,
    pub started: Instant,
    pub size: (u32, u32),
    pub monitors: u32,
    pub tiles: Vec<Tile>,
    seen_version: u64,
    seen_mon: Option<Mon>,
    last_build: Instant,
    ever_connected: bool,
    /// Apple HP: the connection wants a virtual display, but the session starts on
    /// the Mac's own screen (so a locked Mac's login window is visible) and moves to
    /// the virtual display of this size when the user asks.
    pub virtual_pending: Option<(u32, u32)>,
    /// Apple HP: size for a virtual display, and whether one has been offered this attempt.
    virtual_size: (u32, u32),
    virtual_offered: bool,
    /// Apple HP: the Mac's screen is 5K or larger (slow to decode; see the banner).
    pub big_screen: bool,
    /// Apple HP: the session moved to virtual displays.
    pub on_virtual: bool,
    /// Apple HP: the latest picture of each video stream (the second exists with
    /// two virtual displays), and the stream version last taken.
    streams: [Option<Tile>; 2],
    seen_stream2: u64,
    /// Apple HP: the picture pointer positions refer to (0 or 1).
    pointer_target: u8,
    /// Where the shown monitor sits in the remote framebuffer (for pointer events).
    offset: (u16, u16),
    /// The connection's multi-monitor preference was applied (once per session).
    multi_applied: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    None,
    Quick,
    Name,
    Host,
    Port,
    User,
    Gateway,
    Password,
    Manage,
    PwUser,
    PwPass,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsTab {
    General,
    Input,
    AppleHp,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DialogTab {
    General,
    Display,
    Local,
}

pub struct Form {
    pub draft: Connection,
    /// New password typed in the dialog; empty keeps whatever the vault already holds.
    pub password: String,
    pub has_saved_password: bool,
    pub is_new: bool,
    pub tab: DialogTab,
    pub error: Option<String>,
}

pub struct PwPrompt {
    pub conn_id: String,
    pub user: String,
    pub pass: String,
    pub error: Option<String>,
}

pub struct App {
    pub store: Store,
    pub settings: Settings,
    pub settings_tab: SettingsTab,
    pub manage_query: String,
    /// Connection whose Delete button was pressed once (a second press confirms).
    pub confirm_delete: Option<String>,
    pub confirm_forget: bool,
    /// Id of the open drop-down or toolbar picker, if any.
    pub pick: Option<&'static str>,
    /// Where closing the connection dialog returns to (the manager, if it opened it).
    pub dialog_return: View,
    hp_check: Option<Arc<Mutex<Option<String>>>>,
    /// Connection threads signal this when a new picture is ready.
    wake: async_channel::Sender<()>,
    pub hp_result: Option<String>,
    /// Window-chrome actions taken (also lets tests observe them; the test platform
    /// ignores quit and window moves).
    pub quit_requested: bool,
    pub window_drags: u32,
    pub corner_radius: f32,
    pub st: AppState,
    pub sessions: HashMap<String, Session>,
    pub focus: FocusHandle,
    pub field: Field,
    pub quick: String,
    pub form: Option<Form>,
    pub pw: Option<PwPrompt>,
    pub font: Option<SharedString>,
    /// Remote-screen area in window coordinates, updated on every render.
    pub area: Rc<Cell<Bounds<Pixels>>>,
    pub held_keys: HashMap<String, u32>,
    pub held_mods: Modifiers,
    pub buttons: u8,
    /// Last key press or click forwarded, to log display latency right after it.
    last_input: Option<Instant>,
    _poll: Option<Task<()>>,
}

/// Key combinations that are hard to type locally (the OS or window manager grabs them).
pub struct KeyCombo {
    pub label: &'static str,
    pub mods: &'static [u32],
    pub key: u32,
}

pub const KEY_COMBOS: &[KeyCombo] = &[
    KeyCombo { label: "Ctrl+Alt+Del", mods: &[rfb::KEYSYM_CONTROL, rfb::KEYSYM_ALT], key: rfb::KEYSYM_DELETE },
    KeyCombo { label: "Ctrl+Shift+Esc", mods: &[rfb::KEYSYM_CONTROL, rfb::KEYSYM_SHIFT], key: 0xff1b },
    KeyCombo { label: "Ctrl+Esc", mods: &[rfb::KEYSYM_CONTROL], key: 0xff1b },
    KeyCombo { label: "Alt+Tab", mods: &[rfb::KEYSYM_ALT], key: 0xff09 },
    KeyCombo { label: "Alt+F4", mods: &[rfb::KEYSYM_ALT], key: 0xffc1 },
    KeyCombo { label: "Alt+Esc", mods: &[rfb::KEYSYM_ALT], key: 0xff1b },
    KeyCombo { label: "Win / Super", mods: &[], key: rfb::KEYSYM_SUPER },
    KeyCombo { label: "Win+D (show desktop)", mods: &[rfb::KEYSYM_SUPER], key: 0x64 },
    KeyCombo { label: "Win+E (explorer)", mods: &[rfb::KEYSYM_SUPER], key: 0x65 },
    KeyCombo { label: "Win+L (lock)", mods: &[rfb::KEYSYM_SUPER], key: 0x6c },
    KeyCombo { label: "Win+R (run)", mods: &[rfb::KEYSYM_SUPER], key: 0x72 },
    KeyCombo { label: "Print Screen", mods: &[], key: 0xff61 },
];

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Build a gpui image from tightly packed BGRA pixels.
fn make_image(w: u32, h: u32, bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buf = image::RgbaImage::from_raw(w, h, bgra)?;
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(image::Frame::new(buf), 1))))
}

impl App {
    pub fn new(store: Store, font: Option<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_settings(store, Settings::default(), font, window, cx)
    }

    pub fn with_settings(store: Store, settings: Settings, font: Option<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let (wake, woken) = async_channel::bounded::<()>(1);
        let mut app = Self {
            wake,
            store,
            settings,
            settings_tab: SettingsTab::General,
            manage_query: String::new(),
            confirm_delete: None,
            confirm_forget: false,
            pick: None,
            dialog_return: View::Idle,
            hp_check: None,
            hp_result: None,
            quit_requested: false,
            window_drags: 0,
            corner_radius: 0.0,
            st: AppState::new(),
            sessions: HashMap::new(),
            focus,
            field: Field::None,
            quick: String::new(),
            form: None,
            pw: None,
            font,
            area: Rc::new(Cell::new(Bounds::default())),
            held_keys: HashMap::new(),
            held_mods: Modifiers::default(),
            buttons: 0,
            last_input: None,
            _poll: None,
        };
        // Wake for each new picture (so it is drawn at once, not up to a poll
        // interval later), and at least every 33 ms for connection events.
        app._poll = Some(cx.spawn_in(window, async move |this, cx| loop {
            let timer = cx.background_executor().timer(Duration::from_millis(33));
            futures_lite::future::or(timer, async {
                let _ = woken.recv().await;
            })
            .await;
            if this.update_in(cx, |this, window, cx| this.tick(window, cx)).is_err() {
                break;
            }
        }));
        app
    }

    // ----- sessions ---------------------------------------------------------

    pub fn session_for_conn(&self, conn_id: &str) -> bool {
        self.st.is_open(conn_id)
    }

    /// Open a connection: switch to it if already open, otherwise start a session.
    /// Sessions are keyed by connection id, so a favourite can never be opened twice.
    pub fn open_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.store.get(id).cloned() else { return };
        let start = conn.start_in;
        let added = self.st.open_session(id, start);
        self.field = Field::None;
        if added {
            self.store.touch(id, now_secs());
            self.persist();
            let session = Session {
                conn_id: id.to_string(),
                status: Status::Connecting,
                handle: None,
                user: conn.user.clone(),
                pass: String::new(),
                scaling: conn.scaling,
                attempts: 0,
                retry_at: None,
                stage: String::new(),
                started: Instant::now(),
                size: (0, 0),
                monitors: 1,
                tiles: vec![],
                seen_version: 0,
                seen_mon: None,
                last_build: Instant::now(),
                ever_connected: false,
                virtual_pending: None,
                virtual_size: (1920, 1080),
                virtual_offered: false,
                big_screen: false,
                on_virtual: false,
                streams: [None, None],
                seen_stream2: 0,
                pointer_target: 0,
                offset: (0, 0),
                multi_applied: false,
            };
            self.sessions.insert(id.to_string(), session);
            self.begin_login(id, window, cx);
            if self.st.full != window.is_fullscreen() {
                window.toggle_fullscreen();
            }
        }
        window.focus(&self.focus);
        cx.notify();
    }

    /// Decide how to authenticate a new session: a password saved in the vault is used
    /// silently; otherwise ask for the user name and password before connecting.
    fn begin_login(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.store.get(id).cloned() else { return };
        if !conn.proto.is_implemented() {
            self.start_connect(id);
            return;
        }
        let saved = if conn.credentials == model::CredentialMode::Vault {
            match vault::get(id) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("vault unavailable: {e}");
                    None
                }
            }
        } else {
            None
        };
        match saved {
            Some(pw) => {
                if let Some(s) = self.sessions.get_mut(id) {
                    s.pass = pw;
                }
                self.start_connect(id);
            }
            None => self.show_password(id, None, window, cx),
        }
    }

    fn start_connect(&mut self, id: &str) {
        let conn = match self.store.get(id) {
            Some(c) => c.clone(),
            None => return,
         };
        let s = match self.sessions.get_mut(id) {
            Some(s) => s,
            None => return,
        };
        s.handle = None;
        s.retry_at = None;
        if !conn.proto.is_implemented() {
            s.status = Status::Failed(format!("{} connections are not supported in this build. Use Apple or VNC.", conn.proto.label()));
            return;
        }
        s.status = if s.attempts > 0 { Status::Reconnecting } else { Status::Connecting };
        s.stage = "Starting…".into();
        s.started = Instant::now();
        s.virtual_pending = None;
        s.virtual_offered = false;
        s.big_screen = false;
        s.on_virtual = false;
        s.pointer_target = 0;

        let handle = match conn.proto {
            Proto::Apple => {
                // Size advertised for the virtual display.
                let (w, h) = match conn.resolution {
                    model::Resolution::Fhd => (1920, 1080),
                    model::Resolution::Hd => (1280, 720),
                    _ => {
                        let a = self.area.get();
                        let (aw, ah) = (f32::from(a.size.width) as u32, f32::from(a.size.height) as u32);
                        if aw >= 640 && ah >= 360 { (aw & !1, ah & !1) } else { (1920, 1080) }
                    }
                };
                // Start on the Mac's own screen: if it is locked, the login window is
                // only drawn there. "Use virtual display" then moves this session.
                s.virtual_pending = conn.virtual_display.then_some((w, h));
                s.virtual_size = (w, h);
                s.virtual_offered = conn.virtual_display;
                hp::connect(hp::Config { host: conn.host.clone(), port: conn.port_number(), username: s.user.clone(), password: s.pass.clone(), width: w, height: h, virtual_display: false, virtual_displays: conn.virtual_displays.clamp(1, 2) as u32, demo: std::env::var_os("SEEMYMAC_DEMO").is_some() })
            }
            _ => rfb::connect(rfb::Config { host: conn.host.clone(), port: conn.port_number(), username: s.user.clone(), password: s.pass.clone() }),
        };
        
        for fb in std::iter::once(&handle.fb).chain(handle.fb2.as_ref()) {
            fb.lock().unwrap().wake = Some(self.wake.clone());
        }
        s.handle = Some(handle);
    }

    pub fn close_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.release_session(id, window, cx);
        self.st.close_session(id);
        if self.st.no_sessions() && self.st.full == false && window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        window.focus(&self.focus);
        cx.notify();
    }

    fn release_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mut s) = self.sessions.remove(id) {
            for t in s.tiles.drain(..).chain(s.streams.iter_mut().filter_map(Option::take)) {
                cx.drop_image(t.image, Some(window));
            }
            // Dropping the handle shuts the socket.
        }
    }

    pub fn retry_session(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.attempts = 0;
        }
        self.start_connect(id);
        cx.notify();
    }

    pub fn active_session(&self) -> Option<&Session> {
        self.st.active.as_ref().and_then(|a| self.sessions.get(a))
    }

    pub fn active_conn(&self) -> Option<&Connection> {
        self.st.active.as_ref().and_then(|a| self.store.get(a))
    }

    /// Per-frame housekeeping: network events, reconnects and image refresh.
    pub(crate) fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        let mut prompt: Option<(String, String)> = None;
        let mut layout_learnt = false;
        for id in ids {
            let active = self.st.active.as_deref() == Some(id.as_str());
            let conn = self.store.get(&id).cloned();
            let Some(s) = self.sessions.get_mut(&id) else { continue };

            // Network events.
            let mut events = vec![];
            if let Some(h) = &s.handle {
                while let Ok(e) = h.events.try_recv() {
                    events.push(e);
                }
            }
            for e in events {
                changed = true;
                match e {
                    Event::Stage(t) => s.stage = t,
                    Event::Connected { width, height, .. } => {
                        s.status = Status::Connected;
                        s.attempts = 0;
                        s.ever_connected = true;
                        s.size = (width, height);
                        // A 5K/6K screen decodes slowly in software (input feels laggy):
                        // offer the virtual display once, for after logging in.
                        let halved = s.handle.as_ref().map_or(false, |h| h.fb.lock().unwrap().halved);
                        if halved && !s.virtual_offered {
                            s.virtual_offered = true;
                            s.big_screen = true;
                            s.virtual_pending = Some(s.virtual_size);
                        }
                    }
                    Event::ServerClipboard(text) => {
                        if conn.as_ref().map_or(true, |c| c.local.clipboard) {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    }
                    Event::Disconnected(reason) => {
                        s.handle = None;
                        let reason = reason.unwrap_or_else(|| "The remote computer closed the connection.".into());
                        let wants_login = reason.contains("requires a password") || reason.contains("authentication failed");
                        if wants_login {
                            s.status = Status::Failed(reason.clone());
                            prompt = Some((id.clone(), reason));
                        } else if s.ever_connected && conn.as_ref().map_or(false, |c| c.reconnect) && s.attempts < self.settings.max_reconnects {
                            s.attempts += 1;
                            s.status = Status::Reconnecting;
                            s.retry_at = Some(Instant::now() + Duration::from_secs(2 * s.attempts as u64));
                        } else {
                            s.status = Status::Failed(reason);
                        }
                    }
                }
            }

            if matches!(s.status, Status::Connecting | Status::Reconnecting) || (s.status == Status::Connected && s.tiles.is_empty()) {
                if s.started.elapsed().as_millis() % 1000 < 40 {
                    changed = true; // keeps the elapsed-seconds counter moving
                }
            }

            // Scheduled reconnect.
            if s.retry_at.map_or(false, |t| Instant::now() >= t) {
                self.start_connect(&id);
                changed = true;
                continue;
            }

            // Refresh images from the framebuffer.
            let Some(s) = self.sessions.get_mut(&id) else { continue };
            let Some(h) = &s.handle else { continue };
            if s.status != Status::Connected {
                continue;
            }
            let due = active || s.last_build.elapsed() > Duration::from_secs(1);
            let layout = conn.as_ref().map_or(vec![], |c| c.display_layout.clone());
            let (version, monitors, displays) = {
                let mut f = h.fb.lock().unwrap();
                // Plain VNC to a Mac: no monitor list, so use the layout learnt over HP
                // when it tiles exactly this desktop.
                if f.displays.is_empty() && f.screens.is_empty() {
                    if let Some(screens) = layout_screens(&layout, f.width, f.height) {
                        log::info!("{} monitors from the remembered display layout", screens.len());
                        f.screens = screens;
                        f.version += 1;
                    }
                }
                // Apple HP lists the Mac's displays (one is streamed at a time); VNC
                // reports the monitors within its whole-desktop framebuffer.
                // Apple HP streams one display per video stream: two monitors only with
                // two virtual displays (the second stream has a picture).
                let n = if f.displays.is_empty() && h.fb2.is_none() {
                    f.monitors()
                } else {
                    1 + (s.on_virtual && h.fb2.as_ref().map_or(false, |f2| f2.lock().unwrap().version > 0)) as u32
                };
                (f.version, n, f.displays.clone())
            };
            // Remember the Mac's physical display layout for plain VNC sessions.
            if !s.on_virtual && displays.len() > 1 {
                let learnt: Vec<[u32; 4]> = displays.iter().map(|d| [d.x, d.y, d.width, d.height]).collect();
                if let Some(c) = self.store.get_mut(&id).filter(|c| c.display_layout != learnt) {
                    c.display_layout = learnt;
                    layout_learnt = true;
                }
            }
            if monitors != s.monitors {
                // "Side by side" connections open on the "Both" view.
                if monitors > 1 && !s.multi_applied {
                    s.multi_applied = true;
                    if conn.as_ref().map_or(false, |c| c.multi_monitor == model::MultiMonitor::SideBySide) {
                        self.st.set_monitor(&id, Mon::All, monitors);
                    }
                }
                s.monitors = monitors;
                if let Some(c) = self.store.get_mut(&id) {
                    c.monitors = monitors;
                }
                changed = true;
            }
            let mon = self.st.monitor(&id);
            let eff_mon = if active { mon } else { Mon::One };
            let mut retired = vec![];
            if !displays.is_empty() || h.fb2.is_some() || version > 0 && h.fb.lock().unwrap().whole_frames {
                // The handle is lent out so the refresh can update the session too.
                if let Some(h) = s.handle.take() {
                    changed |= hp_refresh(s, &h, due, eff_mon, self.last_input, &mut retired);
                    s.handle = Some(h);
                }
            } else if due && (version != s.seen_version || s.seen_mon != Some(eff_mon) || s.tiles.is_empty()) {
                let t0 = Instant::now();
                let arrived;
                let tiles = {
                    let f = h.fb.lock().unwrap();
                    arrived = f.published;
                    s.size = (f.width, f.height);
                    let n = f.screens.len();
                    let rects: Vec<(usize, geometry_screen::Screen)> = match (eff_mon, n) {
                        (Mon::All, n) if n > 1 => (0..n).map(|i| (i, f.monitor_rect(i))).collect(),
                        (Mon::Two, n) if n > 1 => vec![(1, f.monitor_rect(1))],
                        (_, n) if n > 1 => vec![(0, f.monitor_rect(0))],
                        _ => vec![(0, f.monitor_rect(usize::MAX))],
                    };
                    s.offset = rects.first().map_or((0, 0), |(_, r)| (r.x as u16, r.y as u16));
                    if f.bgra.len() < (f.width * f.height * 4) as usize {
                        vec![] // nothing received yet
                    } else {
                        rects
                            .into_iter()
                            .filter_map(|(monitor, r)| make_image(r.w, r.h, f.crop(r)).map(|image| Tile { image, size: (r.w, r.h), monitor, resolution: (r.w, r.h) }))
                            .collect::<Vec<_>>()
                    }
                };
                retired.extend(std::mem::replace(&mut s.tiles, tiles).into_iter().map(|t| t.image));
                if let (Some(a), Some(i)) = (arrived, self.last_input) {
                    if i.elapsed() < Duration::from_secs(1) {
                        log::debug!("UI: picture ready to draw {} ms after it arrived (copy took {} ms)", a.elapsed().as_millis(), t0.elapsed().as_millis());
                    }
                }
                s.seen_version = version;
                s.seen_mon = Some(eff_mon);
                s.last_build = Instant::now();
                changed = true;
            }
            for image in retired {
                cx.drop_image(image, Some(window));
            }
        }
        if layout_learnt {
            self.persist();
        }
        if let Some(slot) = &self.hp_check {
            let done = slot.lock().unwrap().take();
            if let Some(text) = done {
                self.hp_result = Some(text);
                self.hp_check = None;
                changed = true;
            }
        }
        if let Some((id, reason)) = prompt {
            self.show_password(&id, Some(reason), window, cx);
        }
        if changed {
            cx.notify();
        }
    }

    // ----- login prompt -----------------------------------------------------

    pub fn show_password(&mut self, id: &str, error: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let user = self.sessions.get(id).map(|s| s.user.clone()).unwrap_or_default();
        self.pw = Some(PwPrompt { conn_id: id.to_string(), user, pass: String::new(), error });
        self.st.set_view(View::Password);
        self.field = Field::PwPass;
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn submit_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.pw.take() else { return };
        self.st.close_overlay();
        self.field = Field::None;
        if let Some(c) = self.store.get_mut(&p.conn_id) {
            if c.user != p.user {
                c.user = p.user.clone();
                self.persist();
            }
        }
        if self.store.get(&p.conn_id).map_or(false, |c| c.credentials == model::CredentialMode::Vault) && !p.pass.is_empty() {
            if let Err(e) = vault::set(&p.conn_id, &p.pass) {
                log::warn!("could not save password to the vault: {e}");
            }
        }
        if let Some(s) = self.sessions.get_mut(&p.conn_id) {
            s.user = p.user;
            s.pass = p.pass;
            s.attempts = 0;
        }
        self.start_connect(&p.conn_id);
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn cancel_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let abandoned = self.pw.take().map(|p| p.conn_id).filter(|id| self.sessions.get(id).map_or(false, |s| s.handle.is_none() && !s.ever_connected));
        self.st.close_overlay();
        self.field = Field::None;
        if let Some(id) = abandoned {
            self.close_session(&id, window, cx);
        }
        window.focus(&self.focus);
        cx.notify();
    }

    // ----- menu / quick connect --------------------------------------------

    pub fn toggle_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.st.toggle_view(View::Menu);
        self.quick.clear();
        self.field = if self.st.view == View::Menu { Field::Quick } else { Field::None };
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pick.take().is_some() {
            cx.notify();
            return;
        }
        match self.st.view {
            View::Password => return self.cancel_password(window, cx),
            View::Dialog => {
                self.leave_dialog();
                window.focus(&self.focus);
                cx.notify();
                return;
            }
            View::Settings => self.save_settings(),
            _ => {}
        }
        self.confirm_delete = None;
        self.confirm_forget = false;
        self.st.close_overlay();
        self.field = Field::None;
        window.focus(&self.focus);
        cx.notify();
    }

    /// Enter in the Quick connect field: first matching saved connection, else treat
    /// the text as a host name (optionally host:port) and connect with defaults.
    pub fn quick_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.quick.trim().to_string();
        if text.is_empty() {
            return;
        }
        let hit = self.store.search(&text).first().map(|c| c.id.clone());
        let id = match hit {
            Some(id) => id,
            None => {
                let Some((host, port)) = model::parse_quick_host(&text) else { return };
                let id = self.store.new_id();
                let mut c = Connection::blank(id.clone());
                c.name = host.clone();
                c.host = host;
                c.fav = false;
                if let Some(p) = port {
                    c.port = p.to_string();
                }
                self.store.upsert(c);
                self.persist();
                id
            }
        };
        self.quick.clear();
        self.open_connection(&id, window, cx);
    }

    // ----- dialog -----------------------------------------------------------

    pub fn new_connection_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.store.new_id();
        let mut draft = Connection::blank(id);
        self.settings.apply_defaults(&mut draft);
        self.dialog_return = View::Idle;
        self.form = Some(Form { draft, password: String::new(), has_saved_password: false, is_new: true, tab: DialogTab::General, error: None });
        self.pick = None;
        self.st.set_view(View::Dialog);
        self.field = Field::Name;
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn edit_connection_dialog(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.store.get(id).cloned() else { return };
        let has_saved_password = vault::get(id).ok().flatten().is_some();
        self.dialog_return = if self.st.view == View::Manage { View::Manage } else { View::Idle };
        self.form = Some(Form { draft: c, password: String::new(), has_saved_password, is_new: false, tab: DialogTab::General, error: None });
        self.pick = None;
        self.st.set_view(View::Dialog);
        self.field = Field::Name;
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn set_protocol(&mut self, p: Proto) {
        if let Some(f) = &mut self.form {
            if f.draft.port.trim().is_empty() || f.draft.port == f.draft.proto.default_port().to_string() {
                f.draft.port = p.default_port().to_string();
            }
            f.draft.proto = p;
        }
    }

    /// Returns true if saved. `connect` also opens the session.
    pub fn save_form(&mut self, connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = &mut self.form else { return };
        let mut c = f.draft.clone();
        if let Err(e) = c.validate() {
            f.error = Some(e.to_string());
            cx.notify();
            return;
        }
        if c.name.trim().is_empty() {
            c.name = c.host.trim().to_string();
        }
        c.host = c.host.trim().to_string();
        let id = c.id.clone();
        if c.credentials == model::CredentialMode::Vault {
            if !f.password.is_empty() {
                if let Err(e) = vault::set(&id, &f.password) {
                    f.error = Some(format!("Could not save the password to the system keyring: {e}"));
                    cx.notify();
                    return;
                }
            }
        } else {
            vault::delete(&id);
        }
        self.store.upsert(c);
        self.persist();
        if connect {
            self.form = None;
            self.dialog_return = View::Idle;
            self.st.close_overlay();
            self.field = Field::None;
        } else {
            self.leave_dialog();
        }
        if connect {
            self.open_connection(&id, window, cx);
        }
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn delete_from_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.form.take() else { return };
        let id = f.draft.id;
        self.release_session(&id, window, cx);
        self.st.close_session(&id);
        vault::delete(&id);
        self.store.delete(&id);
        self.persist();
        self.leave_dialog();
        window.focus(&self.focus);
        cx.notify();
    }

    /// Close the connection dialog, returning to the manager if that is where it came from.
    fn leave_dialog(&mut self) {
        self.form = None;
        let back = std::mem::replace(&mut self.dialog_return, View::Idle);
        self.st.set_view(back);
        self.field = if back == View::Manage { Field::Manage } else { Field::None };
    }

    // ----- manager ------------------------------------------------------------

    pub fn open_manage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.manage_query.clear();
        self.confirm_delete = None;
        self.st.set_view(View::Manage);
        self.field = Field::Manage;
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn duplicate_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(mut c) = self.store.get(id).cloned() else { return };
        c.id = self.store.new_id();
        c.name = format!("{} copy", c.display_name());
        c.last_used = 0;
        self.store.upsert(c);
        self.persist();
        cx.notify();
    }

    /// First press arms the confirmation; the second deletes (§: no accidental data loss).
    pub fn delete_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.as_deref() != Some(id) {
            self.confirm_delete = Some(id.to_string());
            cx.notify();
            return;
        }
        self.confirm_delete = None;
        self.release_session(id, window, cx);
        self.st.close_session(id);
        vault::delete(id);
        self.store.delete(id);
        self.persist();
        self.st.set_view(View::Manage);
        cx.notify();
    }

    // ----- settings ---------------------------------------------------------

    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_tab = SettingsTab::General;
        self.confirm_forget = false;
        self.st.set_view(View::Settings);
        self.field = Field::None;
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn save_settings(&self) {
        if let Err(e) = self.settings.save() {
            log::error!("could not save settings: {e}");
        }
    }

    /// Move the active Apple HP session onto its virtual display (after logging in).
    pub fn use_virtual_display(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.st.active.clone() else { return };
        let Some(s) = self.sessions.get_mut(&id) else { return };
        if s.status != Status::Connected {
            return;
        }
        if let (Some((width, height)), Some(h)) = (s.virtual_pending.take(), &s.handle) {
            h.send(Input::UseVirtualDisplay { width, height });
            s.on_virtual = true;
            cx.notify();
        }
    }

    /// The title bar's close button: end every session, then quit.
    pub fn quit(&mut self, cx: &mut Context<Self>) {
        self.quit_requested = true;
        for s in self.sessions.values_mut() {
            if let Some(h) = s.handle.take() {
                h.close();
            }
        }
        cx.quit();
    }

    /// Check the built-in H.264 decoder on a worker thread; `tick` picks up the answer.
    pub fn check_hp(&mut self, cx: &mut Context<Self>) {
        let slot = Arc::new(Mutex::new(None));
        self.hp_check = Some(slot.clone());
        self.hp_result = Some("Checking…".into());
        std::thread::spawn(move || {
            *slot.lock().unwrap() = Some(hp::self_test());
        });
        cx.notify();
    }

    /// Remove every password saved in the vault. Connections themselves are kept.
    pub fn forget_all_passwords(&mut self, cx: &mut Context<Self>) {
        if !self.confirm_forget {
            self.confirm_forget = true;
            cx.notify();
            return;
        }
        self.confirm_forget = false;
        let ids: Vec<String> = self.store.connections.iter().map(|c| c.id.clone()).collect();
        for id in ids {
            vault::delete(&id);
        }
        cx.notify();
    }

    fn persist(&self) {
        if let Err(e) = self.store.save() {
            log::error!("could not save connections: {e}");
        }
    }

    // ----- text entry -------------------------------------------------------

    fn field_buf(&mut self) -> Option<&mut String> {
        match self.field {
            Field::Quick => Some(&mut self.quick),
            Field::PwUser => self.pw.as_mut().map(|p| &mut p.user),
            Field::PwPass => self.pw.as_mut().map(|p| &mut p.pass),
            Field::Name => self.form.as_mut().map(|f| &mut f.draft.name),
            Field::Host => self.form.as_mut().map(|f| &mut f.draft.host),
            Field::Port => self.form.as_mut().map(|f| &mut f.draft.port),
            Field::User => self.form.as_mut().map(|f| &mut f.draft.user),
            Field::Gateway => self.form.as_mut().map(|f| &mut f.draft.gateway),
            Field::Password => self.form.as_mut().map(|f| &mut f.password),
            Field::Manage => Some(&mut self.manage_query),
            Field::None => None,
        }
    }

    pub fn field_order(&self) -> Vec<Field> {
        match self.st.view {
            View::Dialog => {
                let mut v = vec![Field::Name, Field::Host, Field::Port, Field::User];
                if self.form.as_ref().map_or(false, |f| f.draft.credentials == model::CredentialMode::Vault) {
                    v.push(Field::Password);
                }
                v.push(Field::Gateway);
                v
            }
            View::Password => vec![Field::PwUser, Field::PwPass],
            _ => vec![],
        }
    }

    fn handle_overlay_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        match k.key.as_str() {
            "escape" => return self.close_overlay(window, cx),
            "tab" => {
                let order = self.field_order();
                if !order.is_empty() {
                    let i = order.iter().position(|f| *f == self.field).map_or(0, |i| i + 1);
                    let next = if m.shift { order[(i + order.len() - 2) % order.len()] } else { order[i % order.len()] };
                    self.field = next;
                    cx.notify();
                }
                return;
            }
            "enter" => {
                match self.st.view {
                    View::Menu => self.quick_connect(window, cx),
                    View::Password => self.submit_password(window, cx),
                    View::Dialog => self.save_form(true, window, cx),
                    View::Manage | View::Settings => {}
                    _ => {}
                }
                return;
            }
            _ => {}
        }
        if m.control && k.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                let line: String = text.chars().filter(|c| !c.is_control()).collect();
                if let Some(b) = self.field_buf() {
                    b.push_str(&line);
                }
            }
        } else if k.key == "backspace" {
            if let Some(b) = self.field_buf() {
                b.pop();
            }
        } else if !m.control && !m.platform && !m.alt {
            if let Some(ch) = k.key_char.as_deref().filter(|c| c.chars().all(|c| !c.is_control())) {
                let ch = ch.to_string();
                if let Some(b) = self.field_buf() {
                    b.push_str(&ch);
                }
                if self.field == Field::Port {
                    if let Some(f) = &mut self.form {
                        f.draft.port.retain(|c| c.is_ascii_digit());
                    }
                }
            }
        }
        if self.field == Field::Manage {
            self.confirm_delete = None;
        }
        if self.form.as_ref().map_or(false, |f| f.error.is_some()) {
            if let Some(f) = &mut self.form {
                f.error = None;
            }
        }
        cx.notify();
    }

    // ----- keyboard / mouse -------------------------------------------------

    pub fn on_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;

        // Global shortcuts (design §4.1, §6).
        if m.control && m.alt {
            let id = self.st.active.clone();
            match k.key.as_str() {
                "tab" => self.st.switch_next(),
                "enter" => self.toggle_full(window, cx),
                "pageup" => self.set_monitor(&id, Mon::One),
                "pagedown" => self.set_monitor(&id, Mon::Two),
                "home" => self.set_monitor(&id, Mon::All),
                "v" => return self.use_virtual_display(cx),
                d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() && d != "0" => {
                    self.st.switch_to_index(d.parse().unwrap_or(0));
                }
                _ => return self.forward_key(ev, true),
            }
            window.focus(&self.focus);
            cx.notify();
            return;
        }
        if m.control && !m.alt && !m.shift {
            match k.key.as_str() {
                "n" => return self.new_connection_dialog(window, cx),
                "m" => return self.open_manage(window, cx),
                "," => return self.open_settings(window, cx),
                _ => {}
            }
        }

        match self.st.view {
            View::Idle => self.forward_key(ev, true),
            View::Switcher => {
                if k.key == "escape" {
                    self.close_overlay(window, cx);
                }
            }
            _ => self.handle_overlay_key(ev, window, cx),
        }
    }

    pub fn on_key_up(&mut self, ev: &KeyUpEvent, _w: &mut Window, _cx: &mut Context<Self>) {
        log::debug!("key up: {} ({} keys held)", if ev.keystroke.key.chars().count() == 1 { "character key" } else { ev.keystroke.key.as_str() }, self.held_keys.len());
        if let Some(sym) = self.held_keys.remove(&ev.keystroke.key) {
            self.send(Input::Key { keysym: sym, down: false });
        }
    }

    fn forward_key(&mut self, ev: &KeyDownEvent, _from_shortcut: bool) {
        if self.st.view != View::Idle {
            log::debug!("key not forwarded: {:?} panel is open", self.st.view);
            return;
        }
        let k = &ev.keystroke;
        let sym = rfb::keysym(&k.key, k.key_char.as_deref());
        // Never log the character itself: this may be a password.
        log::debug!("key down: {} (key_char {}, repeat {})", if k.key.chars().count() == 1 { "character key" } else { k.key.as_str() }, if k.key_char.is_some() { "present" } else { "none" }, ev.is_held);
        if let Some(sym) = sym {
            self.last_input = Some(Instant::now());
            self.held_keys.insert(k.key.clone(), sym);
            self.send(Input::Key { keysym: sym, down: true });
        }
    }

    /// Modifier keys arrive as their own event in gpui; mirror changes to the remote.
    pub fn on_modifiers(&mut self, ev: &ModifiersChangedEvent, _w: &mut Window, _cx: &mut Context<Self>) {
        let (old, new) = (self.held_mods, ev.modifiers);
        self.held_mods = new;
        if self.st.view != View::Idle {
            return;
        }
        let (old_ctrl, old_cmd) = self.settings.remote_modifiers(old.control, old.platform);
        let (new_ctrl, new_cmd) = self.settings.remote_modifiers(new.control, new.platform);
        for (was, now, sym) in [
            (old.shift, new.shift, rfb::KEYSYM_SHIFT),
            (old_ctrl, new_ctrl, rfb::KEYSYM_CONTROL),
            (old.alt, new.alt, rfb::KEYSYM_ALT),
            (old_cmd, new_cmd, rfb::KEYSYM_SUPER),
        ] {
            if was != now {
                self.send(Input::Key { keysym: sym, down: now });
            }
        }
    }

    fn send(&self, i: Input) {
        let Some(s) = self.active_session() else {
            log::debug!("input dropped: no active session");
            return;
        };
        if s.status != Status::Connected {
            if matches!(i, Input::Key { .. }) {
                log::debug!("key dropped: session status is {:?}", s.status);
            }
            return;
        }
        match &s.handle {
            Some(h) => h.send(i),
            None => log::debug!("input dropped: session has no connection"),
        }
    }

    pub fn send_ctrl_alt_del(&self) {
        self.send_key_combo(0);
    }

    /// Press a `KEY_COMBOS` entry on the remote: modifiers down, key down and up, modifiers up.
    pub fn send_key_combo(&self, i: usize) {
        let Some(c) = KEY_COMBOS.get(i) else { return };
        for m in c.mods {
            self.send(Input::Key { keysym: *m, down: true });
        }
        self.send(Input::Key { keysym: c.key, down: true });
        self.send(Input::Key { keysym: c.key, down: false });
        for m in c.mods.iter().rev() {
            self.send(Input::Key { keysym: *m, down: false });
        }
    }

    pub fn pointer(&mut self, pos: Point<Pixels>, buttons: Option<u8>, down: Option<bool>) {
        // Explain dropped clicks (not moves, which are too frequent to log).
        let why = |reason: &str| {
            if buttons.is_some() {
                log::debug!("click not forwarded: {reason}");
            }
        };
        if self.st.view != View::Idle {
            return why(&format!("{:?} panel is open", self.st.view));
        }
        let Some(s) = self.active_session() else { return why("no active session") };
        let Some(tile) = s.tiles.first() else { return why("no picture yet") };
        if self.st.active.as_deref().map_or(false, |id| self.st.monitor(id) == Mon::All) && s.tiles.len() > 1 {
            return why("\"Both monitors\" view"); // clicking zooms in, handled by the view
        }
        let area = self.area.get();
        let rel_x = f32::from(pos.x - area.origin.x);
        let rel_y = f32::from(pos.y - area.origin.y);
        let Some((x, y)) = geometry::to_remote(f32::from(area.size.width), f32::from(area.size.height), tile.size.0, tile.size.1, s.scaling, rel_x, rel_y) else { return why("outside the picture") };
        if let (Some(b), Some(d)) = (buttons, down) {
            log::debug!("click: button {b} {} at remote {x},{y}", if d { "down" } else { "up" });
        }
        // Offset into the shown monitor within the whole framebuffer (worked out
        // when the picture was built, so mouse moves never wait on its lock).
        let (ox, oy) = s.offset;
        if let (Some(mask), Some(d)) = (buttons, down) {
            self.last_input = Some(Instant::now());
            if d {
                self.buttons |= mask;
            } else {
                self.buttons &= !mask;
            }
        }
        let b = self.buttons;
        self.send(Input::Pointer { x: x + ox, y: y + oy, buttons: b });
    }

    pub fn scroll(&mut self, pos: Point<Pixels>, dy: f32) {
        if dy == 0.0 {
            return;
        }
        self.pointer(pos, None, None);
        let mask = if dy > 0.0 { 8 } else { 16 }; // button 4 = up, 5 = down
        let b = self.buttons;
        if let Some(s) = self.active_session() {
            if let (Some(h), Some(t)) = (&s.handle, s.tiles.first()) {
                let area = self.area.get();
                let (x, y) = geometry::to_remote(f32::from(area.size.width), f32::from(area.size.height), t.size.0, t.size.1, s.scaling, f32::from(pos.x - area.origin.x), f32::from(pos.y - area.origin.y)).unwrap_or((0, 0));
                let (x, y) = (x + s.offset.0, y + s.offset.1);
                h.send(Input::Pointer { x, y, buttons: b | mask });
                h.send(Input::Pointer { x, y, buttons: b });
            }
        }
    }

    // ----- window state -----------------------------------------------------

    pub fn toggle_full(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.st.toggle_full();
        if self.st.full != window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        cx.notify();
    }

    pub fn set_monitor(&mut self, id: &Option<String>, m: Mon) {
        if let Some(id) = id {
            let n = self.sessions.get(id).map_or(1, |s| s.monitors);
            self.st.set_monitor(id, m, n);
        }
    }

    pub fn cycle_scaling(&mut self) {
        let next = self.active_session().map(|s| s.scaling.next());
        if let Some(v) = next {
            self.set_scaling(v);
        }
    }

    pub fn set_scaling(&mut self, v: Scaling) {
        if let Some(id) = self.st.active.clone() {
            if let Some(s) = self.sessions.get_mut(&id) {
                s.scaling = v;
            }
        }
    }
}

/// Apple HP picture refresh. Each video stream's picture is one whole display:
/// take it from its framebuffer (no copy) and keep it as that stream's latest
/// picture. With two virtual displays there are two streams, shown as monitors 1
/// and 2, or side by side for "Both". Returns whether anything visible changed;
/// images no longer shown are pushed to `retired`.
fn hp_refresh(s: &mut Session, h: &rfb::Handle, due: bool, mon: Mon, last_input: Option<Instant>, retired: &mut Vec<Arc<RenderImage>>) -> bool {
    let mut fresh = false;
    if due {
        for (stream, fb) in std::iter::once(&h.fb).chain(h.fb2.as_ref()).enumerate() {
            let t0 = Instant::now();
            let (frame, arrived) = {
                let mut f = fb.lock().unwrap();
                let seen = if stream == 0 { &mut s.seen_version } else { &mut s.seen_stream2 };
                if f.version == *seen {
                    continue;
                }
                *seen = f.version;
                let (w, ht) = (f.width, f.height);
                let bgra = if f.whole_frames { std::mem::take(&mut f.bgra) } else { f.crop(f.monitor_rect(usize::MAX)) };
                let resolution = f.displays.iter().find(|d| Some(d.id) == f.showing).map_or((w, ht), |d| (d.width, d.height));
                let ok = w > 0 && bgra.len() == (w * ht * 4) as usize;
                (ok.then(|| make_image(w, ht, bgra).map(|image| Tile { image, size: (w, ht), monitor: stream, resolution })).flatten(), f.published)
            };
            let Some(tile) = frame else { continue };
            if stream == 0 {
                s.size = tile.size;
            }
            if let Some(old) = s.streams[stream].replace(tile) {
                retired.push(old.image);
            }
            fresh = true;
            if let (Some(a), Some(i)) = (arrived, last_input) {
                if i.elapsed() < Duration::from_secs(1) {
                    log::debug!("UI: picture ready to draw {} ms after it arrived (hand-off took {} ms)", a.elapsed().as_millis(), t0.elapsed().as_millis());
                }
            }
        }
    }
    if !(fresh || s.seen_mon != Some(mon) || (s.tiles.is_empty() && s.streams[0].is_some())) {
        return false;
    }
    let dual = s.on_virtual && s.streams[1].is_some();
    let tile = |i: usize| s.streams[i].as_ref().map(|t| Tile { image: t.image.clone(), size: t.size, monitor: i, resolution: t.resolution });
    let shown: &[usize] = match (mon, dual) {
        (Mon::All, true) => &[0, 1],
        (Mon::Two, true) => &[1],
        _ => &[0],
    };
    // The images belong to `streams`; only replaced pictures are retired.
    s.tiles = shown.iter().filter_map(|&i| tile(i)).collect();
    let target = if dual && mon == Mon::Two { 1 } else { 0 };
    if target != s.pointer_target {
        s.pointer_target = target;
        h.send(Input::PointerTarget(target));
    }
    s.offset = (0, 0); // the HP session maps pointer positions onto the shown display
    s.seen_mon = Some(mon);
    s.last_build = Instant::now();
    true
}

/// Monitor rectangles (left to right) from a remembered display layout, if it
/// has several displays that exactly span a `w`x`h` desktop.
fn layout_screens(layout: &[[u32; 4]], w: u32, h: u32) -> Option<Vec<geometry_screen::Screen>> {
    if layout.len() < 2 || w == 0 || h == 0 {
        return None;
    }
    let mut screens: Vec<geometry_screen::Screen> = layout.iter().map(|&[x, y, w, h]| geometry_screen::Screen { x, y, w, h }).collect();
    let fits = screens.iter().all(|s| s.w > 0 && s.h > 0 && s.x + s.w <= w && s.y + s.h <= h);
    let span = (screens.iter().map(|s| s.x + s.w).max()?, screens.iter().map(|s| s.y + s.h).max()?);
    if !fits || span != (w, h) {
        return None;
    }
    screens.sort_by_key(|s| (s.x, s.y));
    Some(screens)
}

/// Local alias so `app.rs` need not import the type with a long path.
mod geometry_screen {
    pub use crate::rfb::Screen;
}
