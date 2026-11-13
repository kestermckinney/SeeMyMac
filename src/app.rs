//! Application controller: owns the connection store, window state and live sessions.
//! Rendering lives in `view.rs`.

use crate::geometry;
use crate::hp;
use crate::model::{self, Connection, Proto, Scaling, Store};
use crate::rdp;
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

/// Where a software update stands (see `update.rs`).
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    UpToDate,
    Available(crate::update::Release),
    Downloading { received: u64, total: Option<u64> },
    Installing,
    /// Something to tell the user on the About screen (no update to offer, or an error).
    Info(String),
    Failed(String),
}

/// What the update threads report back to `tick`.
enum UpdateMsg {
    Checked(crate::update::CheckResult),
    Progress(u64, Option<u64>),
    Downloaded(std::path::PathBuf),
    Failed(String),
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
    /// Where each monitor's picture sits in the remote framebuffer (monitor, x, y), for
    /// "Span all local monitors", which shows every monitor at once.
    origins: Vec<(usize, u16, u16)>,
    /// How far a picture larger than the window is scrolled from centre, in screen pixels.
    pub pan: (f32, f32),
    /// The same for each monitor while spanning (each window scrolls on its own).
    pub span_pans: Vec<(f32, f32)>,
    /// The connection's multi-monitor preference was applied (once per session).
    multi_applied: bool,
    /// The clipboard text both sides last agreed on (sent there or received from
    /// there), so a change is sent once and the remote's own text is not echoed.
    clip_synced: Option<String>,
    /// Apple: what the latest file drop onto the screen is doing.
    pub file_note: Option<FileNote>,
    /// The files last copied in Finder on the Mac.
    pub mac_files: Vec<String>,
}

/// The status line for a file drop: it stays while sending, then fades.
pub struct FileNote {
    pub text: String,
    pub error: bool,
    pub until: Option<Instant>,
    /// A saved item the "Show in folder" button opens the folder of.
    pub reveal: Option<std::path::PathBuf>,
    /// Paths on the Mac offered by a "Get" button (files copied in Finder there).
    pub fetch: Vec<String>,
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
    RemoteApp,
    Password,
    Manage,
    PwUser,
    PwPass,
    /// The path asked for by "Get file from Mac".
    GetPath,
    /// Settings: the folder fetched files are saved in.
    Download,
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
    /// "Save password in the credential vault" is ticked: the password is stored when it is
    /// submitted (and the connection switches to the vault); unticked forgets a saved one.
    pub save: bool,
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
    pub update: UpdateStatus,
    update_rx: Option<std::sync::mpsc::Receiver<UpdateMsg>>,
    update_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// The check was asked for (About screen), so its result is always shown.
    update_manual: bool,
    /// A version the user said "Later" to, not offered again until the next start.
    update_dismissed: Option<String>,
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
    pub(crate) field_cursor: Option<(Field, usize)>,
    pub(crate) field_anchor: Option<(Field, usize)>,
    pub quick: String,
    pub form: Option<Form>,
    pub pw: Option<PwPrompt>,
    /// "Get file from Mac": the path typed so far and any complaint.
    pub get_file: Option<(String, Option<String>)>,
    pub font: Option<SharedString>,
    /// Remote-screen area in window coordinates, updated on every render.
    pub area: Rc<Cell<Bounds<Pixels>>>,
    /// This computer's monitors (x, y, width, height), for asking an RDP server for the same layout.
    local_displays: Vec<rdp::MonitorRect>,
    /// The same displays' ids, in the same order (primary first, then left to right).
    local_display_ids: Vec<DisplayId>,
    pub held_keys: HashMap<String, u32>,
    pub held_mods: Modifiers,
    pub buttons: u8,
    /// Last key press or click forwarded, to log display latency right after it.
    last_input: Option<Instant>,
    /// When this PC's clipboard was last checked for text to send.
    clip_checked: Instant,
    /// System-shortcut inhibitor for the real window (None in tests).
    pub shortcuts: Option<crate::inhibit::Shortcuts>,
    /// "Span all local monitors" is showing: the main window shows remote monitor `span_main`
    /// and `span_windows` cover the other local displays.
    pub span: bool,
    /// Direction the pointer is pushing against a window edge while zoomed in (-1, 0 or 1 each).
    edge: (f32, f32),
    /// Where the pointer last was: the monitor (when spanning) and the area of its window.
    last_target: Option<(Option<usize>, Bounds<Pixels>)>,
    pub span_main: usize,
    /// The extra windows, when each opened and how many times it was asked to go full screen.
    span_windows: Vec<(AnyWindowHandle, Instant, u8)>,
    span_opening: bool,
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
    // Super is the Mac's Command key; this Mac's own Spotlight takes Cmd+Space when typed here.
    KeyCombo { label: "Cmd+Space (Spotlight)", mods: &[rfb::KEYSYM_SUPER], key: 0x20 },
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
            update: UpdateStatus::Idle,
            update_rx: None,
            update_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            update_manual: false,
            update_dismissed: None,
            hp_result: None,
            quit_requested: false,
            window_drags: 0,
            corner_radius: 0.0,
            st: AppState::new(),
            sessions: HashMap::new(),
            focus,
            field: Field::None,
            field_cursor: None,
            field_anchor: None,
            quick: String::new(),
            form: None,
            pw: None,
            get_file: None,
            font,
            area: Rc::new(Cell::new(Bounds::default())),
            local_displays: Vec::new(),
            held_keys: HashMap::new(),
            held_mods: Modifiers::default(),
            buttons: 0,
            last_input: None,
            clip_checked: Instant::now(),
            shortcuts: None,
            local_display_ids: vec![],
            span: false,
            edge: (0.0, 0.0),
            last_target: None,
            span_main: 0,
            span_windows: vec![],
            span_opening: false,
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
        // Closing the main window while spanning ends that connection, which closes the
        // full-screen windows on the other monitors too.
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = this.update(cx, |a, cx| a.end_span(window, cx));
            true
        });
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
        self.field_cursor = None;
        self.field_anchor = None;
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
                origins: vec![],
                pan: (0.0, 0.0),
                span_pans: vec![],
                multi_applied: false,
                clip_synced: None,
                file_note: None,
                mac_files: vec![],
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

    /// Size asked of the remote machine: the connection's chosen resolution, or the window's.
    fn requested_size(&self, conn: &model::Connection) -> (u32, u32) {
        match conn.resolution {
            model::Resolution::Fhd => (1920, 1080),
            model::Resolution::Hd => (1280, 720),
            _ => {
                let a = self.area.get();
                let (aw, ah) = (f32::from(a.size.width) as u32, f32::from(a.size.height) as u32);
                if aw >= 640 && ah >= 360 { (aw & !1, ah & !1) } else { (1920, 1080) }
            }
        }
    }

    fn start_connect(&mut self, id: &str) {
        let conn = match self.store.get(id) {
            Some(c) => c.clone(),
            None => return,
         };
        let size = self.requested_size(&conn);
        let displays = self.local_displays.clone();
        let s = match self.sessions.get_mut(id) {
            Some(s) => s,
            None => return,
        };
        s.handle = None;
        s.retry_at = None;
        if !conn.proto.is_implemented() {
            s.status = Status::Failed(format!("{} connections are not supported in this build.", conn.proto.label()));
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
                let (w, h) = size;
                // A locked Mac's login window stays on its physical screen.
                // Let the user unlock it before creating the virtual display.
                s.virtual_pending = conn.virtual_display.then_some((w, h));
                s.virtual_size = (w, h);
                s.virtual_offered = conn.virtual_display;
                s.on_virtual = false;
                hp::connect(hp::Config { host: conn.host.clone(), port: conn.port_number(), username: s.user.clone(), password: s.pass.clone(), width: w, height: h, virtual_display: false, virtual_displays: conn.virtual_displays.clamp(1, 2) as u32, demo: std::env::var_os("SEEMYMAC_DEMO").is_some() })
            }
            Proto::Rdp => {
                let (w, h) = size;
                s.virtual_size = (w, h); // the size asked of the server; later window changes are compared with it
                let mut c = rdp::Config::new(conn.host.clone(), conn.port_number(), s.user.clone(), s.pass.clone());
                (c.width, c.height) = (w, h);
                c.clipboard = conn.local.clipboard;
                c.audio = conn.local.audio;
                c.printer = conn.local.printers;
                c.color_depth = if conn.depth == model::ColorDepth::Bits16 { 16 } else { 32 };
                // "Local drives" shares the Downloads folder (the home folder when there is none).
                c.share_folder = conn.local.drives.then(|| dirs::download_dir().or_else(dirs::home_dir)).flatten();
                if conn.span_monitors {
                    c.monitors = displays;
                }
                c.download_dir = self.settings.download_path();
                c.smartcard = conn.local.smart_cards;
                c.gateway = Some(conn.gateway.trim().to_string()).filter(|g| !g.is_empty());
                c.remote_app = Some(conn.remote_app.trim().to_string()).filter(|g| !g.is_empty());
                rdp::connect(c)
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
    /// Send this PC's clipboard text to the active session when it changes. There is
    /// no change notification, so it is polled, only while this window has focus
    /// (Wayland only lets a focused window read the clipboard).
    fn sync_clipboard(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.clip_checked.elapsed() < CLIPBOARD_POLL || !window.is_window_active() {
            return;
        }
        self.clip_checked = Instant::now();
        let Some(id) = self.st.active.clone() else { return };
        if !self.store.get(&id).map_or(false, |c| c.local.clipboard) {
            return;
        }
        let Some(s) = self.sessions.get_mut(&id).filter(|s| s.status == Status::Connected) else { return };
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()).filter(|t| !t.is_empty()) else { return };
        if s.clip_synced.as_deref().map_or(false, |c| same_clip(c, &text)) {
            return;
        }
        if let Some(h) = &s.handle {
            // Macs and VNC servers expect Unix line endings.
            h.send(Input::Clipboard(text.replace("\r\n", "\n")));
            s.clip_synced = Some(text);
        }
    }

    pub(crate) fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_clipboard(window, cx);
        if self.poll_update(window, cx) {
            cx.notify();
        }
        // Zoomed in with the pointer against a window edge: keep scrolling that way.
        if self.edge != (0.0, 0.0) && self.st.view == View::Idle {
            const SPEED: f32 = 18.0; // pixels per tick (about 33 ms)
            if let Some((mon, area)) = self.last_target {
                if self.pan_by_at(mon, area, self.edge.0 * SPEED, self.edge.1 * SPEED) {
                    cx.notify();
                }
            }
        }
        (self.local_display_ids, self.local_displays) = local_displays(cx).into_iter().unzip();
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
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            s.clip_synced = Some(text);
                        }
                    }
                    Event::ServerFiles(paths) => {
                        // Not while a copy is running: its notice is more useful.
                        if !s.file_note.as_ref().map_or(false, |n| n.until.is_none()) {
                            let first = paths[0].rsplit('/').next().unwrap_or(&paths[0]).to_string();
                            let text = if paths.len() == 1 { format!("Copied on the Mac: {first}") } else { format!("Copied on the Mac: {first} and {} more", paths.len() - 1) };
                            s.file_note = Some(FileNote { text, error: false, until: Some(Instant::now() + Duration::from_secs(20)), reveal: None, fetch: paths.clone() });
                        }
                        s.mac_files = paths;
                    }
                    Event::FileCopy(p) => {
                        use crate::hp::filecopy::Progress;
                        let fade = |secs| Some(Instant::now() + Duration::from_secs(secs));
                        s.file_note = Some(match p {
                            Progress::Receiving { name, got, total } if total > 0 => FileNote { text: format!("Receiving {name}… {}%", got.min(total) * 100 / total), error: false, until: None, reveal: None, fetch: vec![] },
                            Progress::Receiving { name, .. } => FileNote { text: format!("Receiving {name}…"), error: false, until: None, reveal: None, fetch: vec![] },
                            Progress::Received { saved } => FileNote { text: format!("Saved to {}", saved.display()), error: false, until: fade(15), reveal: Some(saved), fetch: vec![] },
                            Progress::Sending { name, sent, total } if total > 0 => FileNote { text: format!("Sending {name}… {}%", (sent.min(total) * 100 / total)), error: false, until: None, reveal: None, fetch: vec![] },
                            Progress::Sending { name, .. } => FileNote { text: format!("Sending {name}…"), error: false, until: None, reveal: None, fetch: vec![] },
                            Progress::Done { name } => FileNote { text: format!("Copied {name} to the Mac’s Desktop"), error: false, until: fade(4), reveal: None, fetch: vec![] },
                            Progress::Failed { name, reason } => FileNote { text: format!("Could not copy {name}. {reason}"), error: true, until: fade(10), reveal: None, fetch: vec![] },
                        });
                    }
                    Event::Disconnected(reason) => {
                        s.handle = None;
                        let reason = reason.unwrap_or_else(|| "The remote computer closed the connection.".into());
                        if rfb::wants_login(&reason) {
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

            if s.file_note.as_ref().map_or(false, |n| n.until.map_or(false, |t| Instant::now() >= t)) {
                s.file_note = None;
                changed = true;
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
            // RDP "match window": the remote desktop follows the window's size.
            if active && conn.as_ref().map_or(false, |c| c.proto == Proto::Rdp && c.resolution == model::Resolution::MatchWindow && !c.span_monitors) {
                let a = self.area.get();
                let (aw, ah) = (f32::from(a.size.width) as u32 & !1, f32::from(a.size.height) as u32 & !1);
                if aw >= 640 && ah >= 360 && (aw, ah) != s.virtual_size {
                    s.virtual_size = (aw, ah);
                    h.send(Input::UseVirtualDisplay { width: aw, height: ah });
                }
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
            // Spanning shows every monitor at once (one per local display).
            let span = active
                && window.is_fullscreen()
                && self.local_displays.len() > 1
                && s.monitors > 1
                && conn.as_ref().map_or(false, |c| c.proto == Proto::Rdp && c.span_monitors);
            if active && self.span != span {
                self.span = span;
                changed = true;
            }
            let eff_mon = if span { Mon::All } else if active { mon } else { Mon::One };
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
                    s.origins = rects.iter().map(|(m, r)| (*m, r.x as u16, r.y as u16)).collect();
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
        self.sync_span_windows(window, cx);
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
        let save = self.store.get(id).map_or(false, |c| c.credentials == model::CredentialMode::Vault);
        self.pw = Some(PwPrompt { conn_id: id.to_string(), user, pass: String::new(), error, save });
        self.st.set_view(View::Password);
        self.field_cursor = None;
        self.field_anchor = None;
        self.field = Field::PwPass;
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn submit_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.pw.take() else { return };
        self.st.close_overlay();
        self.field_cursor = None;
        self.field_anchor = None;
        self.field = Field::None;
        if let Some(c) = self.store.get_mut(&p.conn_id) {
            let mut dirty = false;
            if c.user != p.user {
                c.user = p.user.clone();
                dirty = true;
            }
            // Ticking "Save password" moves an "Ask" connection to the vault; unticking it moves a
            // vault connection back to asking and forgets what was saved.
            let mode = match (p.save, c.credentials) {
                (true, model::CredentialMode::Ask) => model::CredentialMode::Vault,
                (false, model::CredentialMode::Vault) => model::CredentialMode::Ask,
                (_, m) => m,
            };
            if mode != c.credentials {
                c.credentials = mode;
                dirty = true;
            }
            if dirty {
                self.persist();
            }
        }
        if p.save && !p.pass.is_empty() {
            if let Err(e) = vault::set(&p.conn_id, &p.pass) {
                log::warn!("could not save password to the vault: {e}");
            }
        } else if !p.save {
            vault::delete(&p.conn_id);
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
        self.field_cursor = None;
        self.field_anchor = None;
        self.field = Field::None;
        if let Some(id) = abandoned {
            self.close_session(&id, window, cx);
        }
        window.focus(&self.focus);
        cx.notify();
    }

    // ----- get a file from the Mac ---------------------------------------------

    /// Fetch files copied in Finder on the Mac (the "Get" button on the notice).
    pub fn fetch_mac_files(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        let dest = self.settings.download_path();
        let user = self.mac_user();
        for entry in paths {
            self.send(Input::GetFile { paths: mac_candidates(&entry, &user), dest: dest.clone() });
        }
        if let Some(s) = self.st.active.clone().and_then(|id| self.sessions.get_mut(&id)) {
            s.file_note = None;
        }
        cx.notify();
    }

    /// The Mac account the session signed in with (its home is /Users/<name>).
    fn mac_user(&self) -> String {
        let s = self.active_session().map(|s| s.user.clone()).filter(|u| !u.is_empty());
        s.or_else(|| self.active_conn().map(|c| c.user.clone())).unwrap_or_default()
    }

    pub fn open_get_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Whatever was last copied in Finder on the Mac is offered as the path.
        let copied = self.active_session().and_then(|s| s.mac_files.first().cloned()).unwrap_or_default();
        self.get_file = Some((copied, None));
        self.st.set_view(View::GetFile);
        self.field_cursor = None;
        self.field_anchor = None;
        self.field = Field::GetPath;
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn submit_get_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, _)) = self.get_file.clone() else { return };
        let path = path.trim().to_string();
        let problem = if path.is_empty() {
            Some("Enter a file name, or the full path on the Mac (for example /Users/me/Documents/report.pdf).")
        } else if !self.active_session().map_or(false, |s| s.status == Status::Connected) || !self.active_conn().map_or(false, |c| c.proto == Proto::Apple) {
            Some("Connect to a Mac with Apple HP first.")
        } else {
            None
        };
        if let Some(p) = problem {
            self.get_file = Some((path, Some(p.into())));
            cx.notify();
            return;
        }
        let paths = mac_candidates(&path, &self.mac_user());
        self.send(Input::GetFile { paths, dest: self.settings.download_path() });
        self.get_file = None;
        self.st.close_overlay();
        self.field_cursor = None;
        self.field_anchor = None;
        self.field = Field::None;
        window.focus(&self.focus);
        cx.notify();
    }

    // ----- menu / quick connect --------------------------------------------

    pub fn toggle_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.st.toggle_view(View::Menu);
        self.quick.clear();
        self.field_cursor = None;
        self.field_anchor = None;
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
            View::Update => return self.dismiss_update(window, cx),
            View::GetFile => self.get_file = None,
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
        self.field_cursor = None;
        self.field_anchor = None;
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
        self.field_cursor = None;
        self.field_anchor = None;
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
        self.field_cursor = None;
        self.field_anchor = None;
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
            self.field_cursor = None;
            self.field_anchor = None;
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
        self.field_cursor = None;
        self.field_anchor = None;
        self.field = if back == View::Manage { Field::Manage } else { Field::None };
    }

    // ----- manager ------------------------------------------------------------

    pub fn open_manage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.manage_query.clear();
        self.confirm_delete = None;
        self.st.set_view(View::Manage);
        self.field_cursor = None;
        self.field_anchor = None;
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
        self.field_cursor = None;
        self.field_anchor = None;
        self.field = Field::None;
        window.focus(&self.focus);
        cx.notify();
    }

    // ----- software updates ---------------------------------------------------

    /// At start-up: look for an update quietly, at most once a day, when the setting allows.
    /// (Linux's Flatpak is updated by the software manager, so it is not checked there.)
    pub fn auto_update_check(&mut self, cx: &mut Context<Self>) {
        if cfg!(target_os = "linux") || !self.settings.check_updates {
            return;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if now.saturating_sub(self.settings.last_update_check) < 24 * 60 * 60 {
            return;
        }
        self.settings.last_update_check = now;
        self.save_settings();
        self.start_update_check(false, cx);
    }

    /// Ask GitLab for the latest release on a background thread. `manual`: the user asked, so
    /// the answer is always shown; otherwise only a newer version is mentioned.
    pub fn start_update_check(&mut self, manual: bool, cx: &mut Context<Self>) {
        if matches!(self.update, UpdateStatus::Checking | UpdateStatus::Downloading { .. } | UpdateStatus::Installing) {
            return;
        }
        self.update = UpdateStatus::Checking;
        self.update_manual = manual;
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let _ = tx.send(UpdateMsg::Checked(crate::update::check()));
            let _ = wake.try_send(());
        });
        cx.notify();
    }

    /// "Install and restart": download the installer, then start it and quit.
    pub fn start_update_download(&mut self, cx: &mut Context<Self>) {
        let UpdateStatus::Available(r) = self.update.clone() else { return };
        self.update = UpdateStatus::Downloading { received: 0, total: None };
        self.update_cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        let (wake, cancel) = (self.wake.clone(), self.update_cancel.clone());
        std::thread::spawn(move || {
            let mut last = std::time::Instant::now();
            let progress_tx = tx.clone();
            let result = crate::update::download(&r.asset_url, &r.asset_name, &cancel, |got, total| {
                if last.elapsed() > Duration::from_millis(100) {
                    last = std::time::Instant::now();
                    let _ = progress_tx.send(UpdateMsg::Progress(got, total));
                    let _ = wake.try_send(());
                }
            });
            let _ = tx.send(match result {
                Ok(path) => UpdateMsg::Downloaded(path),
                Err(e) => UpdateMsg::Failed(e),
            });
            let _ = wake.try_send(());
        });
        cx.notify();
    }

    /// "Later" or Cancel on the update prompt.
    pub fn dismiss_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.update.clone() {
            UpdateStatus::Downloading { .. } => {
                self.update_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                self.update_rx = None;
                self.update = UpdateStatus::Idle;
            }
            UpdateStatus::Available(r) => {
                self.update_dismissed = Some(r.version);
                self.update = UpdateStatus::Idle;
            }
            UpdateStatus::Installing => return,
            _ => self.update = UpdateStatus::Idle,
        }
        self.st.close_overlay();
        window.focus(&self.focus);
        cx.notify();
    }

    /// Handle what the update threads reported. Returns whether anything changed.
    fn poll_update(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(rx) = &self.update_rx else { return false };
        let msgs: Vec<UpdateMsg> = rx.try_iter().collect();
        let mut changed = !msgs.is_empty();
        for m in msgs {
            match m {
                UpdateMsg::Checked(result) => {
                    use crate::update::CheckResult;
                    let manual = self.update_manual;
                    self.update = match result {
                        CheckResult::UpToDate => if manual { UpdateStatus::UpToDate } else { UpdateStatus::Idle },
                        CheckResult::Available(r) if !manual && self.update_dismissed.as_deref() == Some(r.version.as_str()) => {
                            let _ = r;
                            UpdateStatus::Idle
                        }
                        CheckResult::Available(r) => UpdateStatus::Available(r),
                        CheckResult::Manual(msg) => if manual { UpdateStatus::Info(msg) } else { UpdateStatus::Idle },
                        CheckResult::Failed(e) => {
                            log::warn!("update check failed: {e}");
                            if manual { UpdateStatus::Failed(e) } else { UpdateStatus::Idle }
                        }
                    };
                }
                UpdateMsg::Progress(received, total) => {
                    if matches!(self.update, UpdateStatus::Downloading { .. }) {
                        self.update = UpdateStatus::Downloading { received, total };
                    }
                }
                UpdateMsg::Downloaded(path) => {
                    self.update = UpdateStatus::Installing;
                    self.update_rx = None;
                    match crate::update::install(&path) {
                        Ok(()) => {
                            log::info!("update installer started; quitting");
                            self.quit(cx);
                        }
                        Err(e) => {
                            log::error!("update install failed: {e}");
                            self.update = UpdateStatus::Failed(e);
                        }
                    }
                    return true;
                }
                UpdateMsg::Failed(e) => {
                    self.update_rx = None;
                    self.update = if e == "Cancelled" { UpdateStatus::Idle } else { UpdateStatus::Failed(e) };
                }
            }
        }
        // A newer version is ready to offer: show the prompt when nothing else is in the way
        // (or from the About screen, where the user asked).
        if matches!(self.update, UpdateStatus::Available(_)) && self.st.view != View::Update && (self.st.view == View::Idle || self.st.view == View::About) {
            self.st.set_view(View::Update);
            window.focus(&self.focus);
            changed = true;
        }
        changed
    }

    pub fn open_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.st.set_view(View::About);
        self.field_cursor = None;
        self.field_anchor = None;
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

    /// End every session, then quit (title bar close and menu Exit).
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
            Field::RemoteApp => self.form.as_mut().map(|f| &mut f.draft.remote_app),
            Field::Password => self.form.as_mut().map(|f| &mut f.password),
            Field::Manage => Some(&mut self.manage_query),
            Field::GetPath => self.get_file.as_mut().map(|g| &mut g.0),
            Field::Download => Some(&mut self.settings.download_dir),
            Field::None => None,
        }
    }

    pub(crate) fn cursor_for(&self, field: Field, value: &str) -> usize {
        let mut i = self.field_cursor.filter(|(f, _)| *f == field).map_or(value.len(), |(_, i)| i.min(value.len()));
        while !value.is_char_boundary(i) { i -= 1; }
        i
    }

    fn edit_field(&mut self, key: &str, text: Option<&str>, extend: bool) {
        let field = self.field;
        let saved = self.field_cursor;
        let anchor = self.field_anchor.filter(|(f, _)| *f == field).map(|(_, i)| i);
        let Some(buf) = self.field_buf() else { return };
        let mut cursor = saved.filter(|(f, _)| *f == field).map_or(buf.len(), |(_, i)| i.min(buf.len()));
        while !buf.is_char_boundary(cursor) { cursor -= 1; }
        let mut anchor = anchor.unwrap_or(cursor).min(buf.len());
        while !buf.is_char_boundary(anchor) { anchor -= 1; }
        let range = anchor.min(cursor)..anchor.max(cursor);
        let moving = matches!(key, "left" | "right" | "home" | "end");
        if !moving && !range.is_empty() {
            buf.replace_range(range.clone(), "");
            cursor = range.start;
        }
        let prev = buf[..cursor].char_indices().last().map_or(0, |(i, _)| i);
        let next = buf[cursor..].chars().next().map_or(cursor, |c| cursor + c.len_utf8());
        match key {
            "left" => cursor = if !extend && !range.is_empty() { range.start } else { prev },
            "right" => cursor = if !extend && !range.is_empty() { range.end } else { next },
            "home" => cursor = 0,
            "end" => cursor = buf.len(),
            "backspace" => if range.is_empty() { buf.replace_range(prev..cursor, ""); cursor = prev; },
            "delete" => if range.is_empty() { buf.replace_range(cursor..next, ""); },
            _ => if let Some(text) = text {
                let text: String = text.chars().filter(|c| !c.is_control() && (field != Field::Port || c.is_ascii_digit())).collect();
                buf.insert_str(cursor, &text);
                cursor += text.len();
            }
        }
        self.field_cursor = Some((field, cursor));
        self.field_anchor = if moving && extend { Some((field, anchor)) } else { None };
    }

    pub fn field_order(&self) -> Vec<Field> {
        match self.st.view {
            View::Dialog => {
                let mut v = vec![Field::Name, Field::Host, Field::Port, Field::User];
                if self.form.as_ref().map_or(false, |f| f.draft.proto == Proto::Rdp) {
                    v.push(Field::Gateway);
                    v.push(Field::RemoteApp);
                }
                if self.form.as_ref().map_or(false, |f| f.draft.credentials == model::CredentialMode::Vault) {
                    v.push(Field::Password);
                }
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
                    self.field_cursor = None;
                    self.field_anchor = None;
                    self.field = next;
                    cx.notify();
                }
                return;
            }
            "enter" => {
                match self.st.view {
                    View::Menu => self.quick_connect(window, cx),
                    View::Password => self.submit_password(window, cx),
                    View::GetFile => self.submit_get_file(window, cx),
                    View::Dialog => self.save_form(true, window, cx),
                    View::Manage | View::Settings => {}
                    _ => {}
                }
                return;
            }
            _ => {}
        }
        if (m.control || m.platform) && k.key == "a" {
            let field = self.field;
            if let Some(buf) = self.field_buf() {
                let len = buf.len();
                self.field_anchor = Some((field, 0));
                self.field_cursor = Some((field, len));
            }
        } else if (m.control || m.platform) && matches!(k.key.as_str(), "c" | "x") {
            let field = self.field;
            if let (Some((_, a)), Some((_, b))) = (self.field_anchor.filter(|(f, _)| *f == field), self.field_cursor.filter(|(f, _)| *f == field)) {
                if !matches!(field, Field::Password | Field::PwPass) {
                    if let Some(buf) = self.field_buf() {
                        if let Some(text) = buf.get(a.min(b)..a.max(b)).filter(|t| !t.is_empty()) {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                        }
                    }
                    if k.key == "x" { self.edit_field("insert", Some(""), false); }
                }
            }
        } else if (m.control || m.platform) && k.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                let line: String = text.chars().filter(|c| !c.is_control()).collect();
                self.edit_field("insert", Some(&line), false);
            }
        } else if matches!(k.key.as_str(), "left" | "right" | "home" | "end" | "backspace" | "delete") {
            self.edit_field(&k.key, None, m.shift);
        } else if !m.control && !m.platform && !m.alt {
            if let Some(ch) = k.key_char.as_deref().filter(|c| c.chars().all(|c| !c.is_control())) {
                let ch = ch.to_string();
                self.edit_field("insert", Some(&ch), false);
                consume_typed_key(cx);
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
                k @ ("left" | "right" | "up" | "down") if self.pan_room() != (0.0, 0.0) => {
                    // (While spanning, this scrolls the monitor the pointer was last on.)
                    const STEP: f32 = 120.0;
                    let (dx, dy) = match k { "left" => (STEP, 0.0), "right" => (-STEP, 0.0), "up" => (0.0, STEP), _ => (0.0, -STEP) };
                    self.pan_by(dx, dy);
                }
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
            View::Idle => {
                self.forward_key(ev, true);
                if k.key_char.as_deref().map_or(false, |c| c.chars().any(|c| !c.is_control())) {
                    consume_typed_key(cx);
                }
            }
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

    /// Text that arrives without a key-down event: on Windows, characters sent as Unicode
    /// input (VK_PACKET), as Remote Desktop clients often do, only reach the app as WM_CHAR,
    /// which gpui hands to the text input handler. Type it into the open field, or send it
    /// to the Mac as key presses.
    pub fn type_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() {
            return;
        }
        match self.st.view {
            View::Idle => {
                for c in text.chars() {
                    let s = c.to_string();
                    if let Some(sym) = rfb::keysym(&s, Some(&s)) {
                        self.last_input = Some(Instant::now());
                        self.send(Input::Key { keysym: sym, down: true });
                        self.send(Input::Key { keysym: sym, down: false });
                    }
                }
            }
            View::Switcher => {}
            _ => {
                self.edit_field("insert", Some(&text), false);
                if self.field == Field::Manage {
                    self.confirm_delete = None;
                }
                if let Some(f) = &mut self.form {
                    f.error = None;
                }
                cx.notify();
            }
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

    /// Open the folder holding a fetched item in the desktop's file manager.
    pub fn show_in_folder(&mut self, path: &std::path::Path) {
        let dir = path.parent().unwrap_or(path);
        let opener = if cfg!(target_os = "windows") { "explorer.exe" } else if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        if let Err(e) = std::process::Command::new(opener).arg(dir).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            log::warn!("could not open {}: {e}", dir.display());
        }
    }

    /// Files dropped onto the remote screen: copy them to the Mac (Apple sessions only).
    pub fn drop_files(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let Some(s) = self.active_session() else { return };
        if s.status != Status::Connected || paths.is_empty() {
            return;
        }
        if !self.active_conn().map_or(false, |c| matches!(c.proto, Proto::Apple | Proto::Rdp)) {
            return;
        }
        self.send(Input::SendFiles(paths));
        cx.notify();
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
        let mon = self.span.then_some(self.span_main);
        let area = self.area.get();
        self.pointer_at(mon, area, pos, buttons, down);
    }

    /// The remote position under `pos` (relative to `area`, the window's remote-screen area) when
    /// showing remote monitor `mon` (None: the one shown), and the monitor's offset in the desktop.
    fn remote_xy(&self, mon: Option<usize>, area: Bounds<Pixels>, pos: Point<Pixels>) -> Option<((u16, u16), (u16, u16))> {
        let s = self.active_session()?;
        let tile = match mon {
            Some(m) => s.tiles.iter().find(|t| t.monitor == m)?,
            None => s.tiles.first()?,
        };
        let rel_x = f32::from(pos.x - area.origin.x);
        let rel_y = f32::from(pos.y - area.origin.y);
        let pan = match mon {
            Some(m) => s.span_pans.get(m).copied().unwrap_or_default(),
            None => s.pan,
        };
        let xy = geometry::to_remote_pan(f32::from(area.size.width), f32::from(area.size.height), tile.size.0, tile.size.1, s.scaling, pan, rel_x, rel_y)?;
        let offset = match mon {
            Some(m) => s.origins.iter().find(|o| o.0 == m).map_or(s.offset, |o| (o.1, o.2)),
            None => s.offset,
        };
        Some((xy, offset))
    }

    pub fn pointer_at(&mut self, mon: Option<usize>, area: Bounds<Pixels>, pos: Point<Pixels>, buttons: Option<u8>, down: Option<bool>) {
        self.last_target = Some((mon, area));
        // Zoomed in: remember when the pointer is pushed against an edge, so the picture scrolls.
        let (lx, ly) = self.pan_room_at(mon, area);
        self.edge = (0.0, 0.0);
        if self.settings.edge_scroll && self.buttons == 0 && (lx > 0.0 || ly > 0.0) {
            const BAND: f32 = 6.0;
            let (rx, ry) = (f32::from(pos.x - area.origin.x), f32::from(pos.y - area.origin.y));
            let (w, h) = (f32::from(area.size.width), f32::from(area.size.height));
            if rx >= 0.0 && ry >= 0.0 && rx <= w && ry <= h {
                let dir = |v: f32, len: f32, room: f32| if room <= 0.0 { 0.0 } else if v < BAND { 1.0 } else if v > len - BAND { -1.0 } else { 0.0 };
                self.edge = (dir(rx, w, lx), dir(ry, h, ly));
            }
        }
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
        if s.tiles.is_empty() {
            return why("no picture yet");
        }
        if mon.is_none() && self.st.active.as_deref().map_or(false, |id| self.st.monitor(id) == Mon::All) && s.tiles.len() > 1 {
            return why("\"Both monitors\" view"); // clicking zooms in, handled by the view
        }
        let Some(((x, y), (ox, oy))) = self.remote_xy(mon, area, pos) else { return why("outside the picture") };
        if let (Some(b), Some(d)) = (buttons, down) {
            log::debug!("click: button {b} {} at remote {x},{y}", if d { "down" } else { "up" });
        }
        // The offset into the shown monitor within the whole framebuffer is worked out
        // when the picture was built, so mouse moves never wait on its lock.
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

    /// Wheel or trackpad over the picture. A zoomed-in picture scrolls (Shift turns a plain
    /// wheel sideways); otherwise the scroll goes to the remote.
    pub fn scroll_wheel(&mut self, pos: Point<Pixels>, dx: f32, dy: f32, shift: bool) -> bool {
        let mon = self.span.then_some(self.span_main);
        let area = self.area.get();
        self.scroll_wheel_at(mon, area, pos, dx, dy, shift)
    }

    pub fn scroll_wheel_at(&mut self, mon: Option<usize>, area: Bounds<Pixels>, pos: Point<Pixels>, dx: f32, dy: f32, shift: bool) -> bool {
        if self.st.view == View::Idle && self.pan_room_at(mon, area) != (0.0, 0.0) {
            let (px, py) = if shift && dx == 0.0 { (dy, 0.0) } else { (dx, dy) };
            if px != 0.0 || py != 0.0 {
                self.pan_by_at(mon, area, px, py);
                return true;
            }
        }
        self.scroll_at(mon, area, pos, dy);
        false
    }

    pub fn scroll(&mut self, pos: Point<Pixels>, dy: f32) {
        let mon = self.span.then_some(self.span_main);
        let area = self.area.get();
        self.scroll_at(mon, area, pos, dy);
    }

    pub fn scroll_at(&mut self, mon: Option<usize>, area: Bounds<Pixels>, pos: Point<Pixels>, dy: f32) {
        if dy == 0.0 {
            return;
        }
        self.pointer_at(mon, area, pos, None, None);
        let mask = if dy > 0.0 { 8 } else { 16 }; // button 4 = up, 5 = down
        let b = self.buttons;
        let ((x, y), (ox, oy)) = self.remote_xy(mon, area, pos).unwrap_or(((0, 0), self.active_session().map_or((0, 0), |s| s.offset)));
        let (x, y) = (x + ox, y + oy);
        self.send(Input::Pointer { x, y, buttons: b | mask });
        self.send(Input::Pointer { x, y, buttons: b });
    }

    /// The picture of remote monitor `m` for a spanning window: image, size and scaling.
    pub fn span_tile(&self, m: usize) -> Option<(Arc<RenderImage>, (u32, u32), Scaling, (f32, f32))> {
        if !self.span {
            return None;
        }
        let s = self.active_session()?;
        let t = s.tiles.iter().find(|t| t.monitor == m)?;
        Some((t.image.clone(), t.size, s.scaling, s.span_pans.get(m).copied().unwrap_or_default()))
    }

    /// Closing any window of "Span all local monitors" (the main one or one on another
    /// monitor) ends the connection it shows, and with it the windows on every monitor.
    /// `window` is the main window.
    pub fn end_span(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.span && self.span_windows.is_empty() {
            return;
        }
        if let Some(id) = self.st.active.clone() {
            self.close_session(&id, window, cx);
        }
        self.span = false;
        self.sync_span_windows(window, cx);
    }

    /// Open or close the extra full-screen windows that "Span all local monitors" needs.
    fn sync_span_windows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Forget windows the user closed.
        self.span_windows.retain(|(h, _, _)| h.update(cx, |_, _, _| ()).is_ok());
        // A window asked to open full screen is often not yet in that state (it was not on screen
        // when asked), so ask again once it has settled, a few times at most.
        for (h, opened, tries) in &mut self.span_windows {
            if *tries < 4 && opened.elapsed() > Duration::from_millis(400 * (*tries as u64 + 1)) {
                *tries += 1;
                let _ = h.update(cx, |_, w, _| {
                    if !w.is_fullscreen() {
                        w.toggle_fullscreen();
                    }
                });
            }
        }
        let displays = self.local_display_ids.clone();
        if let Some(d) = window.display(cx) {
            if let Some(i) = displays.iter().position(|x| *x == d.id()) {
                self.span_main = i;
            }
        }
        let wanted: Vec<usize> = if self.span {
            let have = self.active_session().map_or(0, |s| s.tiles.len());
            (0..displays.len().min(have)).filter(|i| *i != self.span_main).collect()
        } else {
            vec![]
        };
        if wanted.is_empty() {
            for (h, _, _) in self.span_windows.drain(..) {
                let _ = h.update(cx, |_, w, _| w.remove_window());
            }
            return;
        }
        if self.span_windows.len() == wanted.len() || self.span_opening {
            return;
        }
        for (h, _, _) in self.span_windows.drain(..) {
            let _ = h.update(cx, |_, w, _| w.remove_window());
        }
        // A new window draws as soon as it opens, and its picture is read from this app, so
        // it must open after the current update has finished (not inside it).
        self.span_opening = true;
        let app = cx.entity();
        let main = window.window_handle();
        let sizes = self.local_displays.clone();
        let displays: Vec<_> = wanted.iter().map(|i| (*i, displays[*i], sizes[*i])).collect();
        cx.defer(move |cx| {
            let mut handles = vec![];
            for (i, id, r) in displays {
                let app = app.clone();
                let options = gpui::WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Fullscreen(Bounds::new(point(px(0.0), px(0.0)), size(px(r.w as f32), px(r.h as f32))))),
                    // A title bar (hidden) is what lets macOS take the window full screen.
                    titlebar: Some(TitlebarOptions { title: Some("SeeMyMac".into()), appears_transparent: true, ..Default::default() }),
                    is_resizable: true,
                    focus: false,
                    display_id: Some(id),
                    ..Default::default()
                };
                match cx.open_window(options, move |w, cx| cx.new(|cx| crate::span::SpanScreen::new(app, main, i, w, cx))) {
                    Ok(h) => handles.push((AnyWindowHandle::from(h), Instant::now(), 0)),
                    Err(e) => log::warn!("could not open a window for monitor {}: {e:#}", i + 1),
                }
            }
            let _ = main.update(cx, |_, w, _| w.activate_window());
            app.update(cx, |a, _| {
                a.span_windows = handles;
                a.span_opening = false;
            });
        });
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
                s.pan = (0.0, 0.0);
                s.span_pans.clear();
            }
        }
    }

    /// How far the picture shown in `area` can be scrolled (0, 0 when it fits or several monitors
    /// are shown). `mon` is the remote monitor when spanning, else None.
    fn pan_room_at(&self, mon: Option<usize>, area: Bounds<Pixels>) -> (f32, f32) {
        let Some(s) = self.active_session() else { return (0.0, 0.0) };
        let tile = match mon {
            Some(m) => s.tiles.iter().find(|t| t.monitor == m),
            None if s.tiles.len() > 1 && self.st.active.as_deref().map_or(false, |id| self.st.monitor(id) == Mon::All) => None,
            None => s.tiles.first(),
        };
        let Some(t) = tile else { return (0.0, 0.0) };
        geometry::pan_limits(f32::from(area.size.width), f32::from(area.size.height), t.size.0 as f32, t.size.1 as f32, s.scaling)
    }

    /// The same for the main window's own picture.
    fn pan_room(&self) -> (f32, f32) {
        self.pan_room_at(self.span.then_some(self.span_main), self.area.get())
    }

    /// Move a zoomed-in picture by (dx, dy) screen pixels; true if it moved.
    pub fn pan_by_at(&mut self, mon: Option<usize>, area: Bounds<Pixels>, dx: f32, dy: f32) -> bool {
        let (lx, ly) = self.pan_room_at(mon, area);
        let Some(id) = self.st.active.clone() else { return false };
        let Some(s) = self.sessions.get_mut(&id) else { return false };
        let slot = match mon {
            Some(m) => {
                if s.span_pans.len() <= m {
                    s.span_pans.resize(m + 1, (0.0, 0.0));
                }
                &mut s.span_pans[m]
            }
            None => &mut s.pan,
        };
        let new = ((slot.0 + dx).clamp(-lx, lx), (slot.1 + dy).clamp(-ly, ly));
        let moved = new != *slot;
        *slot = new;
        moved
    }

    /// Scroll by the keyboard: the window the pointer was last in (the main one if unknown).
    pub fn pan_by(&mut self, dx: f32, dy: f32) -> bool {
        let (mon, area) = match self.last_target {
            Some(t) if self.span => t,
            _ => (self.span.then_some(self.span_main), self.area.get()),
        };
        self.pan_by_at(mon, area, dx, dy)
    }
}

/// Apple HP picture refresh. Each video stream's picture is one whole display:
/// take it from its framebuffer (no copy) and keep it as that stream's latest
/// picture. With two virtual displays there are two streams, shown as monitors 1
/// and 2, or side by side for "Both". Returns whether anything visible changed;
/// images no longer shown are pushed to `retired`.
/// How often this PC's clipboard is checked for new text to send.
const CLIPBOARD_POLL: Duration = Duration::from_millis(750);

/// Clipboard texts that differ only in line endings or trailing space are the same
/// (otherwise a round trip through the Mac would bounce back and forth).
/// Where on the Mac to look for what the user named: a full path as it is, `~/…` in
/// the signed-in account's home, and a bare file name in the places files usually are.
fn mac_candidates(entry: &str, user: &str) -> Vec<String> {
    let entry = entry.trim();
    let home = if user.is_empty() { "/Users/Shared".to_string() } else { format!("/Users/{user}") };
    if entry.starts_with('/') {
        return vec![entry.to_string()];
    }
    if entry == "~" {
        return vec![home];
    }
    if let Some(rest) = entry.strip_prefix("~/") {
        return vec![format!("{home}/{rest}")];
    }
    ["Desktop", "Documents", "Downloads", ""].iter().map(|d| if d.is_empty() { format!("{home}/{entry}") } else { format!("{home}/{d}/{entry}") }).collect()
}

fn same_clip(a: &str, b: &str) -> bool {
    let n = |s: &str| s.replace("\r\n", "\n").trim_end().to_string();
    n(a) == n(b)
}

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

#[cfg(test)]
mod candidate_tests {
    use super::mac_candidates;

    #[test]
    fn names_and_paths_on_the_mac() {
        assert_eq!(mac_candidates("/Volumes/X/a.txt", "me"), ["/Volumes/X/a.txt"]);
        assert_eq!(mac_candidates("~/Documents/a.txt", "me"), ["/Users/me/Documents/a.txt"]);
        assert_eq!(
            mac_candidates("a.txt", "me"),
            ["/Users/me/Desktop/a.txt", "/Users/me/Documents/a.txt", "/Users/me/Downloads/a.txt", "/Users/me/a.txt"]
        );
    }
}

/// This computer's displays with their real places on the desktop, primary first, then left to
/// right. (gpui reports every macOS display at 0, 0, which made the remote monitors overlap.)
fn local_displays(cx: &gpui::App) -> Vec<(DisplayId, rdp::MonitorRect)> {
    let mut v: Vec<(DisplayId, rdp::MonitorRect)> = cx
        .displays()
        .iter()
        .map(|d| {
            let b = d.bounds();
            let mut r = rdp::MonitorRect { x: f32::from(b.origin.x) as i32, y: f32::from(b.origin.y) as i32, w: f32::from(b.size.width) as u32, h: f32::from(b.size.height) as u32 };
            #[cfg(target_os = "macos")]
            {
                let g = core_graphics::display::CGDisplay::new(u32::from(d.id())).bounds();
                (r.x, r.y) = (g.origin.x as i32, g.origin.y as i32);
            }
            (d.id(), r)
        })
        .collect();
    v.sort_by_key(|(_, r)| ((r.x, r.y) != (0, 0), r.x, r.y));
    v
}

/// Marks a typed character as handled. On Windows an unhandled key down is translated into
/// WM_CHAR, which gpui passes to the text input handler (`App::type_text`), so the character
/// would arrive twice.
fn consume_typed_key(cx: &mut Context<App>) {
    if cfg!(windows) {
        cx.stop_propagation();
    }
}
