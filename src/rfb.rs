//! RFB (VNC) client with Apple Remote Desktop authentication.
//!
//! This is what macOS "Screen Sharing" speaks in its standard mode. Apple's
//! "High Performance" transport lives in [`crate::hp`]. Supported security types: None (1), VNC password (2) and Apple
//! Diffie-Hellman (30). Supported encodings: ZRLE, Raw, CopyRect, DesktopSize and
//! ExtendedDesktopSize (used to discover multiple monitors).

use crate::hp::{clipboard, filecopy};
use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use anyhow::{anyhow, bail, Result};
use md5::{Digest, Md5};
use num_bigint::{BigUint, RandBigInt};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::{channel, Receiver, RecvError, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ENC_RAW: i32 = 0;
const ENC_COPYRECT: i32 = 1;
const ENC_ZRLE: i32 = 16;
const ENC_DESKTOP_SIZE: i32 = -223;
const ENC_EXT_DESKTOP_SIZE: i32 = -308;
/// Refuse absurd framebuffers from a misbehaving server (~64k x 64k pixels).
const MAX_PIXELS: u64 = 1 << 28;

/// Marks a disconnect reason meaning the server turned the user name or password down (or needs
/// one). Every transport (VNC, Apple High Performance, RDP) uses it so the app offers the sign-in
/// dialog again, where the rest of the reason is shown.
pub const LOGIN_REJECTED: &str = "Sign-in was rejected";

/// Whether a disconnect reason means the user should be asked for a new user name and password.
pub fn wants_login(reason: &str) -> bool {
    reason.contains(LOGIN_REJECTED)
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// BGRA framebuffer shared between the network thread and the UI.
#[derive(Debug, Default, Clone)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// Bumped on every change so the UI can skip redundant uploads.
    pub version: u64,
    pub screens: Vec<Screen>,
    pub name: String,
    /// Size of the coordinate space input is sent in, when it differs from the
    /// framebuffer (Apple High Performance scales the desktop). (0, 0) = same.
    pub logical: (u32, u32),
    /// When the current picture arrived (Apple High Performance), for latency logs.
    pub published: Option<Instant>,
    /// Apple High Performance: the Mac's screen is 5K or larger, so the picture was
    /// halved for display (and decoding it is expensive).
    pub halved: bool,
    /// Apple High Performance: every display the Mac has, in its layout order,
    /// whether or not the current picture shows it.
    pub displays: Vec<RemoteDisplay>,
    /// Apple High Performance: the display the picture shows (`None`: the whole
    /// desktop, or not known).
    pub showing: Option<u32>,
    /// Every update replaces the whole picture (Apple High Performance), so the UI
    /// may take `bgra` instead of copying it; it stays empty until the next update.
    pub whole_frames: bool,
    /// Signalled after each complete update, so the UI draws it at once instead
    /// of on its next periodic check.
    pub wake: Option<async_channel::Sender<()>>,
}

/// One of the remote computer's displays (Apple High Performance).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteDisplay {
    /// The Mac's display id (CGDirectDisplayID).
    pub id: u32,
    /// Position and size within the whole desktop, in backing pixels.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Framebuffer {
    pub fn resize(&mut self, w: u32, h: u32) {
        self.width = w;
        self.height = h;
        self.bgra = vec![0; (w as usize) * (h as usize) * 4];
        // Opaque black rather than transparent.
        for px in self.bgra.chunks_exact_mut(4) {
            px[3] = 255;
        }
        self.version += 1;
    }

    /// Tell the UI a complete update is ready.
    pub fn wake(&self) {
        if let Some(w) = &self.wake {
            let _ = w.try_send(());
        }
    }

    /// Number of remote monitors the server reported (at least 1).
    pub fn monitors(&self) -> u32 {
        self.screens.len().max(1) as u32
    }

    /// The area of one monitor (index 0-based), or the whole framebuffer.
    pub fn monitor_rect(&self, idx: usize) -> Screen {
        match self.screens.get(idx) {
            Some(s) if s.x + s.w <= self.width && s.y + s.h <= self.height && s.w > 0 && s.h > 0 => *s,
            _ => Screen { x: 0, y: 0, w: self.width, h: self.height },
        }
    }

    /// Copy a rectangle out as tightly packed BGRA.
    pub fn crop(&self, r: Screen) -> Vec<u8> {
        let mut out = Vec::with_capacity((r.w * r.h * 4) as usize);
        for row in r.y..r.y + r.h {
            let start = ((row * self.width + r.x) * 4) as usize;
            out.extend_from_slice(&self.bgra[start..start + (r.w * 4) as usize]);
        }
        out
    }

    fn put_raw(&mut self, x: u32, y: u32, w: u32, h: u32, data: &[u8]) -> Result<()> {
        if x.checked_add(w).map_or(true, |e| e > self.width) || y.checked_add(h).map_or(true, |e| e > self.height) {
            bail!("rectangle {x},{y} {w}x{h} outside {}x{} framebuffer", self.width, self.height);
        }
        for row in 0..h {
            let src = (row * w * 4) as usize;
            let dst = (((y + row) * self.width + x) * 4) as usize;
            self.bgra[dst..dst + (w * 4) as usize].copy_from_slice(&data[src..src + (w * 4) as usize]);
            for px in self.bgra[dst..dst + (w * 4) as usize].chunks_exact_mut(4) {
                px[3] = 255; // servers leave the pad byte undefined
            }
        }
        self.version += 1;
        Ok(())
    }

    fn copy_rect(&mut self, sx: u32, sy: u32, x: u32, y: u32, w: u32, h: u32) -> Result<()> {
        let fits = |a: u32, b: u32, max: u32| a.checked_add(b).map_or(false, |e| e <= max);
        if !fits(sx, w, self.width) || !fits(sy, h, self.height) || !fits(x, w, self.width) || !fits(y, h, self.height) {
            bail!("CopyRect outside framebuffer");
        }
        let src = self.crop(Screen { x: sx, y: sy, w, h });
        self.put_raw(x, y, w, h, &src)
    }
}

#[derive(Debug)]
pub enum Event {
    /// Human-readable progress ("Resolving…", "Authenticating…") for the connecting screen.
    Stage(String),
    Connected { width: u32, height: u32, name: String },
    /// Connection ended; `Some(reason)` when it failed or was dropped.
    Disconnected(Option<String>),
    /// The server ended the session. Reconnecting requires an explicit request.
    SessionEnded(String),
    ServerClipboard(String),
    /// Apple only: a file or folder dropped onto the screen is being copied to the Mac.
    FileCopy(filecopy::Progress),
    /// Apple: files or folders were copied in Finder on the Mac (their paths there).
    ServerFiles(Vec<String>),
}

#[derive(Debug)]
pub enum Input {
    Pointer { x: u16, y: u16, buttons: u8 },
    Key { keysym: u32, down: bool },
    /// Apple High Performance: move the running session onto a virtual display of
    /// this size (in points). Ignored by plain VNC.
    UseVirtualDisplay { width: u32, height: u32 },
    /// Apple High Performance: stream one of the Mac's displays (`Some(display id)`)
    /// or all of them combined into one picture (`None`). Ignored by plain VNC,
    /// which always sends the whole desktop.
    ShowDisplay(Option<u32>),
    /// Apple High Performance with two virtual displays: pointer positions that
    /// follow refer to picture 0 (the first display) or 1 (the second).
    PointerTarget(u8),
    /// Put this text on the remote clipboard.
    Clipboard(String),
    /// Apple: copy these files and folders to the Mac. Ignored by plain VNC.
    SendFiles(Vec<std::path::PathBuf>),
    /// Apple: fetch this path from the Mac into the local folder `dest`. Ignored by plain VNC.
    GetFile { paths: Vec<String>, dest: std::path::PathBuf },
}

/// The input channel as the network side reads it: a run of queued pointer
/// moves collapses into the latest one, so a backlog of motion events can't
/// delay the clicks and keystrokes queued behind it. Presses and releases
/// (a change of buttons) are never merged and keep their own position.
pub(crate) struct InputQueue {
    rx: Receiver<Input>,
    held: Option<Input>,
    buttons: u8,
}

impl InputQueue {
    pub(crate) fn new(rx: Receiver<Input>) -> Self {
        Self { rx, held: None, buttons: 0 }
    }

    pub(crate) fn recv(&mut self) -> Result<Input, RecvError> {
        let first = match self.held.take() {
            Some(i) => i,
            None => self.rx.recv()?,
        };
        Ok(self.coalesce(first))
    }

    pub(crate) fn recv_timeout(&mut self, t: Duration) -> Result<Input, RecvTimeoutError> {
        let first = match self.held.take() {
            Some(i) => i,
            None => self.rx.recv_timeout(t)?,
        };
        Ok(self.coalesce(first))
    }

    fn coalesce(&mut self, first: Input) -> Input {
        let Input::Pointer { buttons, .. } = first else { return first };
        if buttons != self.buttons {
            self.buttons = buttons;
            return first;
        }
        let mut latest = first;
        while let Ok(next) = self.rx.try_recv() {
            match next {
                Input::Pointer { buttons: b, .. } if b == buttons => latest = next,
                other => {
                    self.held = Some(other);
                    break;
                }
            }
        }
        latest
    }
}

/// Runs once to tear a connection down (shut a socket, stop a child process).
pub type Closer = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// A running connection, whatever the protocol. Dropping it closes it.
pub struct Handle {
    pub events: Receiver<Event>,
    pub fb: Arc<Mutex<Framebuffer>>,
    /// Apple High Performance with two virtual displays: the second one's pictures.
    pub fb2: Option<Arc<Mutex<Framebuffer>>>,
    input: Sender<Input>,
    closer: Closer,
}

impl Handle {
    pub fn from_parts(events: Receiver<Event>, fb: Arc<Mutex<Framebuffer>>, input: Sender<Input>, closer: Closer) -> Self {
        Self { events, fb, fb2: None, input, closer }
    }

    pub fn send(&self, i: Input) {
        let _ = self.input.send(i);
    }

    pub fn close(&self) {
        if let Some(f) = self.closer.lock().unwrap().take() {
            f();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.close();
    }
}

/// Start connecting on a background thread; progress arrives through `Handle::events`.
pub fn connect(cfg: Config) -> Handle {
    let (ev_tx, ev_rx) = channel();
    let (in_tx, in_rx) = channel();
    let fb = Arc::new(Mutex::new(Framebuffer::default()));
    let socket: Closer = Arc::new(Mutex::new(None));
    let handle = Handle::from_parts(ev_rx, fb.clone(), in_tx, socket.clone());

    std::thread::Builder::new()
        .name(format!("rfb-{}", cfg.host))
        .spawn(move || {
            let result = run(&cfg, &fb, &socket, in_rx, &ev_tx);
            let _ = ev_tx.send(Event::Disconnected(result.err().map(|e| format!("{e:#}"))));
        })
        .expect("spawn rfb thread");
    handle
}

fn run(
    cfg: &Config,
    fb: &Arc<Mutex<Framebuffer>>,
    socket: &Closer,
    input: Receiver<Input>,
    events: &Sender<Event>,
) -> Result<()> {
    let stage = |t: String| {
        log::info!("{}: {t}", cfg.host);
        events.send(Event::Stage(t)).ok();
    };
    stage(format!("Looking up {}…", cfg.host));
    let mut stream = crate::dial::connect(&cfg.host, cfg.port, |a| stage(format!("Connecting to {a}…")))?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(45))).ok(); // handshake only
    let closer_stream = stream.try_clone()?;
    *socket.lock().unwrap() = Some(Box::new(move || {
        let _ = closer_stream.shutdown(Shutdown::Both);
    }));

    let init = handshake(&mut stream, &cfg.username, &cfg.password, &mut |t| stage(t))?;
    stream.set_read_timeout(None).ok();
    {
        let mut f = fb.lock().unwrap();
        f.resize(init.width, init.height);
        f.name = init.name.clone();
    }
    stage(format!("Connected to “{}”. Waiting for the first screen image ({}×{})…", init.name, init.width, init.height));
    send_setup(&mut stream)?;
    let apple = init.apple;
    if apple {
        // Apple's viewer information turns on its status pushes, which announce
        // pasteboard changes; then pasteboard monitoring itself. Without them the
        // Mac never sends its clipboard to a VNC viewer.
        stream.write_all(&crate::hp::negotiate::viewer_info_message())?;
        stream.write_all(&clipboard::enable())?;
    }
    request_update(&mut stream, false, init.width, init.height)?;
    events.send(Event::Connected { width: init.width, height: init.height, name: init.name }).ok();

    // Writes come from two threads (update requests and user input).
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let w2 = writer.clone();
    let transfers = Arc::new(filecopy::Transfers::default());
    let tr2 = transfers.clone();
    let ev2 = events.clone();
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stopped.clone();
    std::thread::spawn(move || {
        let mut input = InputQueue::new(input);
        while let Ok(i) = input.recv() {
            let msg = match i {
                Input::Pointer { x, y, buttons } => {
                    let mut m = vec![5, buttons];
                    m.extend_from_slice(&x.to_be_bytes());
                    m.extend_from_slice(&y.to_be_bytes());
                    m
                }
                Input::Key { keysym, down } => {
                    let mut m = vec![4, down as u8, 0, 0];
                    m.extend_from_slice(&keysym.to_be_bytes());
                    m
                }
                // Apple's own message reaches the pasteboard modern Mac apps read
                // (and carries any character); ClientCutText is Latin-1 only.
                Input::Clipboard(text) if apple => clipboard::send(&text).concat(),
                Input::Clipboard(text) => client_cut_text(&text),
                Input::SendFiles(paths) if apple => {
                    let (w, tr, ev, stop) = (w2.clone(), tr2.clone(), ev2.clone(), stop2.clone());
                    std::thread::spawn(move || {
                        let mut send = |m: &[u8]| w.lock().unwrap().write_all(m);
                        filecopy::send_paths(&paths, filecopy::DEFAULT_DEST, &tr, &mut send, &|p| drop(ev.send(Event::FileCopy(p))), &|| stop.load(std::sync::atomic::Ordering::Relaxed));
                    });
                    continue;
                }
                Input::GetFile { paths, dest } if apple => {
                    let (w, tr, ev, stop) = (w2.clone(), tr2.clone(), ev2.clone(), stop2.clone());
                    std::thread::spawn(move || {
                        let mut send = |m: &[u8]| w.lock().unwrap().write_all(m);
                        filecopy::get_path(&paths, &dest, &tr, &mut send, &|p| drop(ev.send(Event::FileCopy(p))), &|| stop.load(std::sync::atomic::Ordering::Relaxed));
                    });
                    continue;
                }
                Input::SendFiles(_) | Input::GetFile { .. } | Input::UseVirtualDisplay { .. } | Input::ShowDisplay(_) | Input::PointerTarget(_) => continue,
            };
            if w2.lock().unwrap().write_all(&msg).is_err() {
                break;
            }
        }
    });

    let result = read_loop(&mut stream, &writer, fb, events, &transfers);
    stopped.store(true, std::sync::atomic::Ordering::Relaxed);
    result
}

#[derive(Debug)]
pub struct ServerInit {
    pub width: u32,
    pub height: u32,
    pub name: String,
    /// macOS Screen Sharing (RFB 003.889): its own clipboard messages are used.
    pub apple: bool,
}

fn read_exact_n<R: Read>(r: &mut R, n: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    r.read_exact(&mut v)?;
    Ok(v)
}
fn rd_u8<R: Read>(r: &mut R) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}
fn rd_u16<R: Read>(r: &mut R) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}
fn rd_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_reason<R: Read>(r: &mut R) -> String {
    rd_u32(r)
        .ok()
        .filter(|n| *n < 4096)
        .and_then(|n| read_exact_n(r, n as usize).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Version, security and initialisation exchange.
pub fn handshake<S: Read + Write>(s: &mut S, user: &str, password: &str, stage: &mut dyn FnMut(String)) -> Result<ServerInit> {
    let ver = read_exact_n(s, 12)?;
    let ver = String::from_utf8_lossy(&ver);
    if !ver.starts_with("RFB ") {
        bail!("not an RFB/VNC server (greeting {:?})", ver.trim());
    }
    // Apple reports 003.889; anything >= 3.8 is spoken as 3.8.
    let minor: u32 = ver[8..11].parse().unwrap_or(0);
    let apple = minor == 889;
    if minor < 7 {
        bail!("server speaks RFB 3.{minor}; 3.7 or newer is required");
    }
    let minor = minor.min(8);
    stage(format!("Server speaks {}. Negotiating…", ver.trim()));
    s.write_all(format!("RFB 003.00{minor}\n").as_bytes())?;

    let count = rd_u8(s)?;
    if count == 0 {
        bail!("server refused the connection: {}", read_reason(s));
    }
    let types = read_exact_n(s, count as usize)?;
    // ARD only helps when we have credentials; plain VNC auth needs a password.
    let chosen = if types.contains(&30) && !password.is_empty() {
        30
    } else if types.contains(&2) && !password.is_empty() {
        2
    } else if types.contains(&1) {
        1
    } else if types.contains(&30) || types.contains(&2) {
        bail!("{LOGIN_REJECTED}: this computer needs a password.");
    } else {
        bail!("no supported login method (the server offered security types {types:?}; this client supports 1, 2 and 30)");
    };
    stage(format!(
        "Server offers security types {types:?}; using {} ({}). {}",
        chosen,
        match chosen { 30 => "Apple Remote Desktop login", 2 => "VNC password", _ => "no login" },
        if chosen == 1 { "" } else { "Signing in… If the Mac asks to approve the connection, accept it there." }
    ));
    s.write_all(&[chosen])?;

    match chosen {
        2 => {
            let challenge = read_exact_n(s, 16)?;
            s.write_all(&vnc_auth_response(&challenge, password))?;
        }
        30 => ard_auth(s, user, password)?,
        _ => {}
    }
    if chosen != 1 || minor >= 8 {
        let status = rd_u32(s)?;
        if status != 0 {
            let reason = if minor >= 8 { read_reason(s) } else { String::new() };
            log::warn!("login refused (security type {chosen}, status {status}): {reason:?}");
            let why = if chosen == 30 { "the user name or password is incorrect" } else { "the password is incorrect" };
            match reason.trim() {
                "" => bail!("{LOGIN_REJECTED}: {why}."),
                r => bail!("{LOGIN_REJECTED}: {why} (the computer said “{r}”)."),
            }
        }
    }

    stage("Signed in. Requesting the desktop…".into());
    s.write_all(&[1])?; // ClientInit: shared session
    let width = rd_u16(s)? as u32;
    let height = rd_u16(s)? as u32;
    let _pixel_format = read_exact_n(s, 16)?;
    let name_len = rd_u32(s)?;
    if name_len > 1 << 16 || (width as u64) * (height as u64) > MAX_PIXELS || width == 0 || height == 0 {
        bail!("server sent an invalid ServerInit ({width}x{height})");
    }
    let name = String::from_utf8_lossy(&read_exact_n(s, name_len as usize)?).into_owned();
    Ok(ServerInit { width, height, name, apple })
}

/// DES challenge response; VNC reverses the bits of each key byte.
pub fn vnc_auth_response(challenge: &[u8], password: &str) -> Vec<u8> {
    use des::cipher::BlockEncrypt as _;
    use des::cipher::KeyInit as _;
    let mut key = [0u8; 8];
    for (i, b) in password.bytes().take(8).enumerate() {
        key[i] = b.reverse_bits();
    }
    let des = des::Des::new(GenericArray::from_slice(&key));
    let mut out = challenge.to_vec();
    for block in out.chunks_exact_mut(8) {
        des.encrypt_block(GenericArray::from_mut_slice(block));
    }
    out
}

/// Apple Remote Desktop auth: Diffie-Hellman, then username and password
/// (64 bytes each, zero-terminated) encrypted with AES-128-ECB under MD5(secret).
fn ard_auth<S: Read + Write>(s: &mut S, user: &str, password: &str) -> Result<()> {
    let generator = rd_u16(s)?;
    let key_len = rd_u16(s)? as usize;
    if key_len == 0 || key_len > 1024 {
        bail!("invalid Diffie-Hellman key length {key_len}");
    }
    let prime = BigUint::from_bytes_be(&read_exact_n(s, key_len)?);
    let server_pub = BigUint::from_bytes_be(&read_exact_n(s, key_len)?);
    let (reply, ) = ard_client_reply(generator, &prime, &server_pub, key_len, user, password);
    s.write_all(&reply)?;
    Ok(())
}

/// Builds the client's ARD message (encrypted credentials + public key).
pub fn ard_client_reply(
    generator: u16,
    prime: &BigUint,
    server_pub: &BigUint,
    key_len: usize,
    user: &str,
    password: &str,
) -> (Vec<u8>,) {
    let mut rng = rand::thread_rng();
    let private = rng.gen_biguint((key_len * 8) as u64);
    let public = BigUint::from(generator).modpow(&private, prime);
    let shared = server_pub.modpow(&private, prime);
    let mut shared_bytes = shared.to_bytes_be();
    while shared_bytes.len() < key_len {
        shared_bytes.insert(0, 0);
    }
    let key = Md5::digest(&shared_bytes);

    let mut creds = [0u8; 128];
    rand::RngCore::fill_bytes(&mut rng, &mut creds); // random padding, as Apple's client does
    for (off, text) in [(0usize, user), (64, password)] {
        let bytes = text.as_bytes();
        let n = bytes.len().min(63);
        creds[off..off + n].copy_from_slice(&bytes[..n]);
        creds[off + n] = 0;
    }
    let cipher = aes::Aes128::new(GenericArray::from_slice(&key));
    for block in creds.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }

    let mut pub_bytes = public.to_bytes_be();
    while pub_bytes.len() < key_len {
        pub_bytes.insert(0, 0);
    }
    let mut out = creds.to_vec();
    out.extend_from_slice(&pub_bytes);
    (out,)
}

fn send_setup<W: Write>(w: &mut W) -> Result<()> {
    // 32 bpp, depth 24, little endian, true colour: R<<16 | G<<8 | B => BGRA in memory.
    let mut m = vec![0, 0, 0, 0, 32, 24, 0, 1];
    m.extend_from_slice(&255u16.to_be_bytes());
    m.extend_from_slice(&255u16.to_be_bytes());
    m.extend_from_slice(&255u16.to_be_bytes());
    m.extend_from_slice(&[16, 8, 0, 0, 0, 0]);
    // SetEncodings
    let encs = [ENC_ZRLE, ENC_COPYRECT, ENC_RAW, ENC_EXT_DESKTOP_SIZE, ENC_DESKTOP_SIZE];
    m.extend_from_slice(&[2, 0]);
    m.extend_from_slice(&(encs.len() as u16).to_be_bytes());
    for e in encs {
        m.extend_from_slice(&e.to_be_bytes());
    }
    w.write_all(&m)?;
    Ok(())
}

fn request_update<W: Write>(w: &mut W, incremental: bool, width: u32, height: u32) -> Result<()> {
    let mut m = vec![3, incremental as u8, 0, 0, 0, 0];
    m.extend_from_slice(&(width as u16).to_be_bytes());
    m.extend_from_slice(&(height as u16).to_be_bytes());
    w.write_all(&m)?;
    Ok(())
}

/// ClientCutText. RFB clipboard text is Latin-1; other characters become '?'.
fn client_cut_text(text: &str) -> Vec<u8> {
    let latin1: Vec<u8> = text.chars().filter(|&c| c != '\r').map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?')).collect();
    let mut m = vec![6, 0, 0, 0];
    m.extend_from_slice(&(latin1.len() as u32).to_be_bytes());
    m.extend_from_slice(&latin1);
    m
}

/// ServerCutText is Latin-1 by the spec, but some servers send UTF-8.
fn cut_text_string(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| e.into_bytes().iter().map(|&c| c as char).collect())
}

fn read_loop(
    stream: &mut TcpStream,
    writer: &Arc<Mutex<TcpStream>>,
    fb: &Arc<Mutex<Framebuffer>>,
    events: &Sender<Event>,
    transfers: &filecopy::Transfers,
) -> Result<()> {
    let mut zrle = Zrle::new();
    loop {
        match rd_u8(stream)? {
            0 => {
                rd_u8(stream)?;
                let rects = rd_u16(stream)?;
                for _ in 0..rects {
                    read_rect(stream, fb, &mut zrle)?;
                }
                let (w, h) = {
                    let f = fb.lock().unwrap();
                    f.wake();
                    (f.width, f.height)
                };
                request_update(&mut *writer.lock().unwrap(), true, w, h)?;
            }
            1 => {
                // SetColourMapEntries: we always use true colour, so skip it.
                rd_u8(stream)?;
                rd_u16(stream)?;
                let n = rd_u16(stream)?;
                read_exact_n(stream, n as usize * 6)?;
            }
            2 => {} // Bell
            3 => {
                read_exact_n(stream, 3)?;
                let n = rd_u32(stream)? as usize;
                if n > 1 << 24 {
                    bail!("clipboard text too large");
                }
                let text = cut_text_string(read_exact_n(stream, n)?);
                events.send(Event::ServerClipboard(text)).ok();
            }
            // Apple status push: u16 length, then its body (bytes 2..4: the command).
            clipboard::MSG_STATUS => {
                let h = read_exact_n(stream, 3)?;
                let body = read_exact_n(stream, u16::from_be_bytes([h[1], h[2]]) as usize)?;
                let mut m = vec![clipboard::MSG_STATUS];
                m.extend_from_slice(&h);
                m.extend_from_slice(&body);
                if clipboard::status_command(&m) == Some(clipboard::STATUS_CLIPBOARD_CHANGED) {
                    writer.lock().unwrap().write_all(&clipboard::fetch())?;
                }
            }
            // Apple pasteboard contents (the reply to a fetch).
            clipboard::MSG_SEND => {
                let h = read_exact_n(stream, 15)?;
                let n = u32::from_be_bytes(h[11..15].try_into().unwrap()) as usize;
                if n > 1 << 24 {
                    bail!("clipboard data too large");
                }
                let mut m = vec![clipboard::MSG_SEND];
                m.extend_from_slice(&h);
                m.extend_from_slice(&read_exact_n(stream, n)?);
                if let Some(text) = clipboard::text(&m) {
                    events.send(Event::ServerClipboard(text)).ok();
                }
                let (files, types) = clipboard::files(&m);
                log::debug!("Mac pasteboard types: {types:?}");
                if !files.is_empty() {
                    events.send(Event::ServerFiles(files)).ok();
                }
            }
            // Apple file copy: progress and the final status of a drop sent to the Mac.
            filecopy::MSG => {
                let h = read_exact_n(stream, 5)?;
                let n = u32::from_be_bytes(h[1..5].try_into().unwrap()) as usize;
                if n > 1 << 20 {
                    bail!("file copy message too large");
                }
                let mut m = vec![filecopy::MSG];
                m.extend_from_slice(&h);
                m.extend_from_slice(&read_exact_n(stream, n)?);
                if let Some(r) = filecopy::parse_reply(&m) {
                    transfers.dispatch(r);
                }
            }
            t => bail!("unsupported server message type {t}"),
        }
    }
}

fn read_rect<R: Read>(r: &mut R, fb: &Arc<Mutex<Framebuffer>>, zrle: &mut Zrle) -> Result<()> {
    let x = rd_u16(r)? as u32;
    let y = rd_u16(r)? as u32;
    let w = rd_u16(r)? as u32;
    let h = rd_u16(r)? as u32;
    let enc = rd_u32(r)? as i32;
    match enc {
        ENC_RAW => {
            if (w as u64) * (h as u64) > MAX_PIXELS {
                bail!("raw rectangle too large");
            }
            let data = read_exact_n(r, (w * h * 4) as usize)?;
            fb.lock().unwrap().put_raw(x, y, w, h, &data)?;
        }
        ENC_ZRLE => {
            let n = rd_u32(r)? as usize;
            if n > 1 << 26 {
                bail!("ZRLE rectangle too large");
            }
            let compressed = read_exact_n(r, n)?;
            let pixels = zrle.decode(&compressed, w, h)?;
            fb.lock().unwrap().put_raw(x, y, w, h, &pixels)?;
        }
        ENC_COPYRECT => {
            let sx = rd_u16(r)? as u32;
            let sy = rd_u16(r)? as u32;
            fb.lock().unwrap().copy_rect(sx, sy, x, y, w, h)?;
        }
        ENC_DESKTOP_SIZE => {
            check_size(w, h)?;
            let mut f = fb.lock().unwrap();
            f.resize(w, h);
            f.screens.clear();
        }
        ENC_EXT_DESKTOP_SIZE => {
            // x = reason, y = status (non-zero: a failed client request; ignore those).
            let n = rd_u8(r)? as usize;
            read_exact_n(r, 3)?;
            let mut screens = Vec::new();
            for _ in 0..n {
                let _id = rd_u32(r)?;
                let sx = rd_u16(r)? as u32;
                let sy = rd_u16(r)? as u32;
                let sw = rd_u16(r)? as u32;
                let sh = rd_u16(r)? as u32;
                let _flags = rd_u32(r)?;
                screens.push(Screen { x: sx, y: sy, w: sw, h: sh });
            }
            if y == 0 || x == 0 {
                // x == 0: initial notification; y == 0: no error.
                if y == 0 {
                    check_size(w, h)?;
                    let mut f = fb.lock().unwrap();
                    if f.width != w || f.height != h {
                        f.resize(w, h);
                    }
                    // Left-to-right order so "monitor 1/2" are stable.
                    screens.sort_by_key(|s| (s.x, s.y));
                    f.screens = screens;
                    f.version += 1;
                }
            }
        }
        other => bail!("server used unsupported encoding {other}"),
    }
    Ok(())
}

/// ZRLE decoder (RFC 6143 §7.7.6). The zlib stream persists for the whole connection.
pub struct Zrle {
    inflate: flate2::Decompress,
}

impl Zrle {
    pub fn new() -> Self {
        Self { inflate: flate2::Decompress::new(true) }
    }

    /// Decode one rectangle to tightly packed BGRA (alpha 255).
    pub fn decode(&mut self, compressed: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
        if (w as u64) * (h as u64) > MAX_PIXELS {
            bail!("ZRLE rectangle too large");
        }
        let mut data = Vec::with_capacity(compressed.len() * 4 + 64);
        let start_in = self.inflate.total_in();
        loop {
            let consumed = (self.inflate.total_in() - start_in) as usize;
            if consumed >= compressed.len() {
                break;
            }
            data.reserve(1 << 16);
            let before_out = self.inflate.total_out();
            self.inflate
                .decompress_vec(&compressed[consumed..], &mut data, flate2::FlushDecompress::Sync)
                .map_err(|e| anyhow!("ZRLE data is corrupt: {e}"))?;
            if self.inflate.total_out() == before_out && (self.inflate.total_in() - start_in) as usize == consumed {
                break; // no progress
            }
            if data.len() > (w as usize * h as usize * 4 + 1024) * 2 {
                bail!("ZRLE data larger than the rectangle");
            }
        }
        let mut out = vec![0u8; (w * h * 4) as usize];
        let mut c = Cursor { d: &data, i: 0 };
        let mut ty = 0;
        while ty < h {
            let th = (h - ty).min(64);
            let mut tx = 0;
            while tx < w {
                let tw = (w - tx).min(64);
                decode_tile(&mut c, &mut out, w, tx, ty, tw, th)?;
                tx += 64;
            }
            ty += 64;
        }
        Ok(out)
    }
}

struct Cursor<'a> {
    d: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn u8(&mut self) -> Result<u8> {
        let b = *self.d.get(self.i).ok_or_else(|| anyhow!("ZRLE data ended early"))?;
        self.i += 1;
        Ok(b)
    }
    fn cpixel(&mut self) -> Result<[u8; 4]> {
        let s = self.d.get(self.i..self.i + 3).ok_or_else(|| anyhow!("ZRLE data ended early"))?;
        self.i += 3;
        Ok([s[0], s[1], s[2], 255]) // CPIXEL is B,G,R for our pixel format
    }
    fn run_len(&mut self) -> Result<usize> {
        let mut n = 1usize;
        loop {
            let b = self.u8()?;
            n += b as usize;
            if b != 255 {
                return Ok(n);
            }
        }
    }
}

fn decode_tile(c: &mut Cursor, out: &mut [u8], stride: u32, tx: u32, ty: u32, tw: u32, th: u32) -> Result<()> {
    let sub = c.u8()?;
    let n = (tw * th) as usize;
    let mut put = |idx: usize, px: [u8; 4]| {
        let (x, y) = (tx + (idx as u32 % tw), ty + (idx as u32 / tw));
        let o = ((y * stride + x) * 4) as usize;
        out[o..o + 4].copy_from_slice(&px);
    };
    let rle = sub & 0x80 != 0;
    let psize = (sub & 0x7f) as usize;
    match (rle, psize) {
        (false, 0) => {
            for i in 0..n {
                let px = c.cpixel()?;
                put(i, px);
            }
        }
        (false, 1) => {
            let px = c.cpixel()?;
            for i in 0..n {
                put(i, px);
            }
        }
        (false, 2..=16) => {
            let pal: Vec<[u8; 4]> = (0..psize).map(|_| c.cpixel()).collect::<Result<_>>()?;
            let bits = match psize { 2 => 1, 3..=4 => 2, _ => 4 };
            for row in 0..th as usize {
                let mut byte = 0u8;
                for col in 0..tw as usize {
                    let used = col * bits;
                    if used % 8 == 0 {
                        byte = c.u8()?;
                    }
                    let shift = 8 - bits - (used % 8);
                    let idx = ((byte >> shift) & ((1 << bits) - 1)) as usize;
                    put(row * tw as usize + col, *pal.get(idx).ok_or_else(|| anyhow!("ZRLE palette index out of range"))?);
                }
            }
        }
        (true, 0) => {
            let mut i = 0;
            while i < n {
                let px = c.cpixel()?;
                let len = c.run_len()?;
                if i + len > n {
                    bail!("ZRLE run past end of tile");
                }
                for k in 0..len {
                    put(i + k, px);
                }
                i += len;
            }
        }
        (true, 2..=127) => {
            let pal: Vec<[u8; 4]> = (0..psize).map(|_| c.cpixel()).collect::<Result<_>>()?;
            let mut i = 0;
            while i < n {
                let b = c.u8()?;
                let px = *pal.get((b & 0x7f) as usize).ok_or_else(|| anyhow!("ZRLE palette index out of range"))?;
                let len = if b & 0x80 != 0 { c.run_len()? } else { 1 };
                if i + len > n {
                    bail!("ZRLE run past end of tile");
                }
                for k in 0..len {
                    put(i + k, px);
                }
                i += len;
            }
        }
        _ => bail!("unsupported ZRLE tile type {sub}"),
    }
    Ok(())
}

fn check_size(w: u32, h: u32) -> Result<()> {
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_PIXELS {
        bail!("invalid desktop size {w}x{h}");
    }
    Ok(())
}

/// X11 keysym for a gpui keystroke, or None when it has no remote meaning.
pub fn keysym(key: &str, key_char: Option<&str>) -> Option<u32> {
    if let Some(c) = key_char.and_then(|s| {
        let mut it = s.chars();
        match (it.next(), it.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    }) {
        if c == '\n' || c == '\r' {
            return Some(0xff0d);
        }
        if (c as u32) >= 0x20 && c != '\u{7f}' {
            return Some(if (c as u32) < 0x100 { c as u32 } else { 0x0100_0000 | c as u32 });
        }
    }
    let named = match key {
        "enter" => 0xff0d,
        "escape" => 0xff1b,
        "backspace" => 0xff08,
        "tab" => 0xff09,
        "space" => 0x20,
        "delete" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "capslock" => 0xffe5,
        k if k.starts_with('f') && k[1..].parse::<u32>().map_or(false, |n| (1..=12).contains(&n)) => {
            0xffbe + k[1..].parse::<u32>().unwrap() - 1
        }
        k if k.chars().count() == 1 => {
            let c = k.chars().next().unwrap();
            if (c as u32) < 0x100 { c as u32 } else { 0x0100_0000 | c as u32 }
        }
        _ => return None,
    };
    Some(named)
}

pub const KEYSYM_SHIFT: u32 = 0xffe1;
pub const KEYSYM_CONTROL: u32 = 0xffe3;
pub const KEYSYM_ALT: u32 = 0xffe9;
pub const KEYSYM_SUPER: u32 = 0xffeb;
pub const KEYSYM_DELETE: u32 = 0xffff;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn queued_pointer_moves_collapse_but_clicks_and_keys_survive() {
        let (tx, rx) = channel();
        let p = |x, buttons| Input::Pointer { x, y: 0, buttons };
        for x in 1..=500 {
            tx.send(p(x, 0)).unwrap();
        }
        tx.send(p(500, 1)).unwrap(); // press
        tx.send(p(501, 1)).unwrap(); // drag
        tx.send(p(502, 1)).unwrap();
        tx.send(p(502, 0)).unwrap(); // release
        tx.send(Input::Key { keysym: 0x61, down: true }).unwrap();
        tx.send(p(600, 0)).unwrap();
        drop(tx);

        let mut q = InputQueue::new(rx);
        let mut out = vec![];
        while let Ok(i) = q.recv() {
            out.push(format!("{:?}", i));
        }
        let want = [
            format!("{:?}", p(500, 0)),
            format!("{:?}", p(500, 1)), // the press keeps its own position
            format!("{:?}", p(502, 1)),
            format!("{:?}", p(502, 0)),
            format!("{:?}", Input::Key { keysym: 0x61, down: true }),
            format!("{:?}", p(600, 0)),
        ];
        assert_eq!(out, want);
    }
    use std::net::TcpListener;

    #[test]
    fn keysym_mapping() {
        assert_eq!(keysym("a", Some("a")), Some(0x61));
        assert_eq!(keysym("a", Some("A")), Some(0x41));
        assert_eq!(keysym("enter", Some("\n")), Some(0xff0d));
        assert_eq!(keysym("enter", None), Some(0xff0d));
        assert_eq!(keysym("f1", None), Some(0xffbe));
        assert_eq!(keysym("f12", None), Some(0xffc9));
        assert_eq!(keysym("left", None), Some(0xff51));
        assert_eq!(keysym("c", None), Some(0x63), "Ctrl+C reports no key_char");
        assert_eq!(keysym("é", Some("é")), Some(0xe9));
        assert_eq!(keysym("€", Some("€")), Some(0x0100_20ac));
        assert_eq!(keysym("nonsense", None), None);
    }

    #[test]
    fn vnc_auth_matches_known_vector() {
        // Well-known VNC test: password "password", all-zero challenge.
        let r = vnc_auth_response(&[0u8; 16], "password");
        assert_eq!(r.len(), 16);
        // Same input twice is deterministic; different password differs.
        assert_eq!(r, vnc_auth_response(&[0u8; 16], "password"));
        assert_ne!(r, vnc_auth_response(&[0u8; 16], "passwore"));
        // First block is DES(key=bitrev("password"), 0) -> independent check:
        use des::cipher::{BlockEncrypt, KeyInit};
        let key: Vec<u8> = b"password".iter().map(|b| b.reverse_bits()).collect();
        let d = des::Des::new(GenericArray::from_slice(&key));
        let mut blk = [0u8; 8];
        d.encrypt_block(GenericArray::from_mut_slice(&mut blk));
        assert_eq!(&r[..8], &blk);
    }

    /// Decrypt the ARD reply the way a server would to prove the client's maths.
    #[test]
    fn ard_reply_decrypts_on_the_server_side() {
        use aes::cipher::BlockDecrypt;
        let key_len = 64usize;
        // A fixed 512-bit prime-ish modulus is enough: both sides use modpow identically.
        let prime = BigUint::from_bytes_be(&[0xff; 64]) - 188u32; // 2^512 - 189 is prime
        let g = 2u16;
        let server_priv = BigUint::from(0x1234_5678_9abc_def0u64);
        let server_pub = BigUint::from(g).modpow(&server_priv, &prime);

        let (reply,) = ard_client_reply(g, &prime, &server_pub, key_len, "alice", "s3cret");
        assert_eq!(reply.len(), 128 + key_len);
        let client_pub = BigUint::from_bytes_be(&reply[128..]);
        let shared = client_pub.modpow(&server_priv, &prime);
        let mut sb = shared.to_bytes_be();
        while sb.len() < key_len {
            sb.insert(0, 0);
        }
        let key = Md5::digest(&sb);
        let cipher = aes::Aes128::new(GenericArray::from_slice(&key));
        let mut creds = reply[..128].to_vec();
        for b in creds.chunks_exact_mut(16) {
            cipher.decrypt_block(GenericArray::from_mut_slice(b));
        }
        let cstr = |s: &[u8]| String::from_utf8(s.iter().copied().take_while(|b| *b != 0).collect()).unwrap();
        assert_eq!(cstr(&creds[..64]), "alice");
        assert_eq!(cstr(&creds[64..]), "s3cret");
    }

    /// Scripted RFB server: returns (port, thread) that serves one client.
    fn fake_server(
        sec_types: Vec<u8>,
        auth: impl FnOnce(&mut TcpStream) -> bool + Send + 'static,
        script: impl FnOnce(&mut TcpStream) + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(b"RFB 003.889\n").unwrap();
            let _ = read_exact_n(&mut s, 12).unwrap();
            let mut offer = vec![sec_types.len() as u8];
            offer.extend(&sec_types);
            s.write_all(&offer).unwrap();
            let _chosen = rd_u8(&mut s).unwrap();
            if !auth(&mut s) {
                return;
            }
            s.write_all(&0u32.to_be_bytes()).unwrap(); // SecurityResult OK
            let _shared = rd_u8(&mut s).unwrap();
            let mut init = vec![];
            init.extend(8u16.to_be_bytes());
            init.extend(4u16.to_be_bytes());
            init.extend([0u8; 16]);
            init.extend(4u32.to_be_bytes());
            init.extend(b"Test");
            s.write_all(&init).unwrap();
            script(&mut s);
        });
        (port, t)
    }

    fn rect_header(x: u16, y: u16, w: u16, h: u16, enc: i32) -> Vec<u8> {
        let mut v = vec![];
        for n in [x, y, w, h] {
            v.extend(n.to_be_bytes());
        }
        v.extend(enc.to_be_bytes());
        v
    }

    /// Reads the client's SetPixelFormat + SetEncodings + first update request.
    #[test]
    fn plain_vnc_clipboard_text_is_latin1() {
        let m = client_cut_text("héllo→x\r\nz");
        assert_eq!(&m[..4], [6, 0, 0, 0]);
        assert_eq!(u32::from_be_bytes(m[4..8].try_into().unwrap()) as usize, m.len() - 8);
        assert_eq!(&m[8..], b"h\xe9llo?x\nz", "Latin-1, Unix line endings, '?' for the rest");
        assert_eq!(cut_text_string(b"ca\xe9".to_vec()), "caé");
        assert_eq!(cut_text_string("café".as_bytes().to_vec()), "café", "UTF-8 is kept");
    }

    fn drain_client_setup(s: &mut TcpStream) {
        read_exact_n(s, 20).unwrap(); // SetPixelFormat
        let hdr = read_exact_n(s, 4).unwrap();
        let n = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        read_exact_n(s, n * 4).unwrap();
        // The fake server is a Mac (003.889): ViewerInfo, then AutoPasteboard.
        let apple = read_exact_n(s, 66 + 8).unwrap();
        assert_eq!((apple[0], apple[66]), (0x21, 0x15));
        read_exact_n(s, 10).unwrap(); // FramebufferUpdateRequest
    }

    fn wait_event(h: &Handle, pred: impl Fn(&Event) -> bool) -> Event {
        loop {
            let e = h.events.recv_timeout(Duration::from_secs(5)).expect("timed out waiting for event");
            if pred(&e) {
                return e;
            }
        }
    }

    #[test]
    fn end_to_end_none_auth_raw_copyrect_and_input() {
        let (port, srv) = fake_server(vec![1], |_| true, |s| {
            drain_client_setup(s);
            // One update: fill 2x1 at (0,0) with red then CopyRect it to (4,2).
            let mut m = vec![0, 0, 0, 2];
            m.extend(rect_header(0, 0, 2, 1, ENC_RAW));
            m.extend([0, 0, 255, 0, 0, 0, 255, 0]); // BGRX red, red
            m.extend(rect_header(4, 2, 2, 1, ENC_COPYRECT));
            m.extend(0u16.to_be_bytes());
            m.extend(0u16.to_be_bytes());
            s.write_all(&m).unwrap();
            // Client must ask for the next (incremental) update, then send our input.
            let req = read_exact_n(s, 10).unwrap();
            assert_eq!(req[0], 3);
            assert_eq!(req[1], 1, "follow-up request must be incremental");
            let ptr = read_exact_n(s, 6).unwrap();
            assert_eq!(ptr, [5, 1, 0, 3, 0, 2]);
            let key = read_exact_n(s, 8).unwrap();
            assert_eq!(key, [4, 1, 0, 0, 0, 0, 0, 0x61]);
            // A Mac gets clipboard text as Apple's 0x1f, which keeps every character.
            let hdr = read_exact_n(s, 16).unwrap();
            assert_eq!(hdr[0], clipboard::MSG_SEND);
            let n = u32::from_be_bytes(hdr[12..16].try_into().unwrap()) as usize;
            let mut m = hdr.clone();
            m.extend(read_exact_n(s, n).unwrap());
            assert_eq!(clipboard::text(&m).as_deref(), Some("héllo→x\nz"));
            // "Pasteboard changed" must be answered with a fetch...
            s.write_all(&[0x14, 0, 0, 4, 0, 1, 0, 2]).unwrap();
            assert_eq!(read_exact_n(s, 8).unwrap(), clipboard::fetch());
            // ...whose reply is the Mac's text.
            s.write_all(&clipboard::send("hi ✓").concat()).unwrap();
            s.write_all(&[3, 0, 0, 0, 0, 0, 0, 3, b'c', b'a', 0xe9]).unwrap(); // Latin-1 ServerCutText
            // then drop the connection
        });
        let h = connect(Config { host: "127.0.0.1".into(), port, username: String::new(), password: String::new() });
        let Event::Connected { width, height, name } = wait_event(&h, |e| matches!(e, Event::Connected { .. })) else { unreachable!() };
        assert_eq!((width, height, name.as_str()), (8, 4, "Test"));
        // Wait for the pixels to land.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let f = h.fb.lock().unwrap();
                let px = |x: usize, y: usize| f.bgra[(y * 8 + x) * 4..(y * 8 + x) * 4 + 4].to_vec();
                if px(0, 0) == [0, 0, 255, 255] {
                    assert_eq!(px(1, 0), [0, 0, 255, 255]);
                    assert_eq!(px(4, 2), [0, 0, 255, 255], "CopyRect");
                    assert_eq!(px(5, 2), [0, 0, 255, 255]);
                    assert_eq!(px(7, 3), [0, 0, 0, 255], "untouched stays opaque black");
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "framebuffer never updated");
            std::thread::sleep(Duration::from_millis(10));
        }
        h.send(Input::Pointer { x: 3, y: 2, buttons: 1 });
        h.send(Input::Key { keysym: 0x61, down: true });
        h.send(Input::Clipboard("héllo→x\nz".into()));
        let Event::ServerClipboard(t) = wait_event(&h, |e| matches!(e, Event::ServerClipboard(_))) else { unreachable!() };
        assert_eq!(t, "hi ✓");
        let Event::ServerClipboard(t) = wait_event(&h, |e| matches!(e, Event::ServerClipboard(_))) else { unreachable!() };
        assert_eq!(t, "caé");
        let Event::Disconnected(reason) = wait_event(&h, |e| matches!(e, Event::Disconnected(_))) else { unreachable!() };
        assert!(reason.is_some(), "a dropped connection must be reported");
        srv.join().unwrap();
    }

    #[test]
    fn vnc_password_auth_is_verified_by_server() {
        let challenge = [7u8; 16];
        let (port, srv) = fake_server(
            vec![2],
            move |s| {
                s.write_all(&challenge).unwrap();
                let resp = read_exact_n(s, 16).unwrap();
                assert_eq!(resp, vnc_auth_response(&challenge, "hunter2"));
                true
            },
            |s| {
                drain_client_setup(s);
                std::thread::sleep(Duration::from_millis(200));
            },
        );
        let h = connect(Config { host: "127.0.0.1".into(), port, username: String::new(), password: "hunter2".into() });
        wait_event(&h, |e| matches!(e, Event::Connected { .. }));
        drop(h);
        srv.join().unwrap();
    }

    #[test]
    fn rejected_password_reports_reason() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(b"RFB 003.008\n").unwrap();
            read_exact_n(&mut s, 12).unwrap();
            s.write_all(&[1, 2]).unwrap();
            rd_u8(&mut s).unwrap();
            s.write_all(&[0; 16]).unwrap();
            read_exact_n(&mut s, 16).unwrap();
            s.write_all(&1u32.to_be_bytes()).unwrap();
            s.write_all(&12u32.to_be_bytes()).unwrap();
            s.write_all(b"Bad password").unwrap();
        });
        let h = connect(Config { host: "127.0.0.1".into(), port, username: String::new(), password: "x".into() });
        let Event::Disconnected(Some(reason)) = wait_event(&h, |e| matches!(e, Event::Disconnected(_))) else { panic!() };
        assert!(reason.starts_with(LOGIN_REJECTED) && reason.contains("password is incorrect") && reason.contains("Bad password"), "{reason}");
        assert!(wants_login(&reason));
        srv.join().unwrap();
    }

    #[test]
    fn password_required_is_reported_before_auth() {
        let mut data = Vec::new();
        data.extend(b"RFB 003.008\n");
        data.extend([1u8, 2]);
        let mut c = Cursor::new(data);
        struct Rw<'a>(&'a mut Cursor<Vec<u8>>, Vec<u8>);
        impl Read for Rw<'_> {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(b)
            }
        }
        impl Write for Rw<'_> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.1.extend(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let err = handshake(&mut Rw(&mut c, vec![]), "", "", &mut |_| {}).unwrap_err().to_string();
        assert!(wants_login(&err) && err.contains("needs a password"), "{err}");
    }

    #[test]
    fn server_refusal_message_is_surfaced() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(b"RFB 003.008\n").unwrap();
            read_exact_n(&mut s, 12).unwrap();
            s.write_all(&[0]).unwrap();
            s.write_all(&4u32.to_be_bytes()).unwrap();
            s.write_all(b"busy").unwrap();
        });
        let h = connect(Config { host: "127.0.0.1".into(), port, username: String::new(), password: String::new() });
        let Event::Disconnected(Some(r)) = wait_event(&h, |e| matches!(e, Event::Disconnected(_))) else { panic!() };
        assert!(r.contains("busy"), "{r}");
        srv.join().unwrap();
    }

    #[test]
    fn unreachable_host_fails_cleanly() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port(); // closed again
        let h = connect(Config { host: "127.0.0.1".into(), port, username: String::new(), password: String::new() });
        let Event::Disconnected(Some(r)) = wait_event(&h, |e| matches!(e, Event::Disconnected(_))) else { panic!() };
        assert!(r.contains("cannot reach"), "{r}");
    }

    #[test]
    fn extended_desktop_size_reports_monitors_and_resizes() {
        let (port, srv) = fake_server(vec![1], |_| true, |s| {
            drain_client_setup(s);
            let mut m = vec![0, 0, 0, 1];
            m.extend(rect_header(0, 0, 16, 4, ENC_EXT_DESKTOP_SIZE));
            m.extend([2, 0, 0, 0]);
            // Screen 2 first on purpose: the client must sort left-to-right.
            for (id, x) in [(2u32, 8u16), (1u32, 0u16)] {
                m.extend(id.to_be_bytes());
                m.extend(x.to_be_bytes());
                m.extend(0u16.to_be_bytes());
                m.extend(8u16.to_be_bytes());
                m.extend(4u16.to_be_bytes());
                m.extend(0u32.to_be_bytes());
            }
            s.write_all(&m).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let h = connect(Config { host: "127.0.0.1".into(), port, username: String::new(), password: String::new() });
        wait_event(&h, |e| matches!(e, Event::Connected { .. }));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let f = h.fb.lock().unwrap();
                if f.screens.len() == 2 {
                    assert_eq!((f.width, f.height), (16, 4));
                    assert_eq!(f.monitors(), 2);
                    assert_eq!(f.monitor_rect(0), Screen { x: 0, y: 0, w: 8, h: 4 });
                    assert_eq!(f.monitor_rect(1), Screen { x: 8, y: 0, w: 8, h: 4 });
                    assert_eq!(f.crop(f.monitor_rect(1)).len(), 8 * 4 * 4);
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "screens never reported");
            std::thread::sleep(Duration::from_millis(10));
        }
        srv.join().unwrap();
    }

    #[test]
    fn out_of_bounds_rectangle_is_rejected() {
        let mut f = Framebuffer::default();
        f.resize(4, 4);
        assert!(f.put_raw(3, 3, 2, 2, &[0; 16]).is_err());
        assert!(f.copy_rect(0, 0, 3, 3, 2, 2).is_err());
        assert!(f.put_raw(0, 0, 4, 4, &vec![0; 64]).is_ok());
    }

    #[test]
    fn non_vnc_greeting_is_rejected() {
        let mut c = Cursor::new(b"SSH-2.0-Open".to_vec());
        struct Rw(Cursor<Vec<u8>>);
        impl Read for Rw {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(b)
            }
        }
        impl Write for Rw {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let _ = &mut c;
        let err = handshake(&mut Rw(c), "", "", &mut |_| {}).unwrap_err().to_string();
        assert!(err.contains("not an RFB"), "{err}");
    }

    fn zlib_chunk(c: &mut flate2::Compress, raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(raw.len() + 64);
        c.compress_vec(raw, &mut out, flate2::FlushCompress::Sync).unwrap();
        out
    }

    fn bgr(b: u8, g: u8, r: u8) -> [u8; 3] {
        [b, g, r]
    }

    #[test]
    fn zrle_all_tile_types_across_two_rects_with_persistent_stream() {
        let mut comp = flate2::Compress::new(flate2::Compression::default(), true);
        let mut dec = Zrle::new();

        // Rect 1: 70x2 -> two tiles (64 wide + 6 wide).
        let mut raw = vec![];
        raw.push(1); // tile 1: solid
        raw.extend(bgr(1, 2, 3));
        raw.push(0); // tile 2: raw 6x2
        for i in 0..12u8 {
            raw.extend(bgr(i, i, i));
        }
        let px = dec.decode(&zlib_chunk(&mut comp, &raw), 70, 2).unwrap();
        assert_eq!(&px[0..4], &[1, 2, 3, 255]);
        assert_eq!(&px[63 * 4..64 * 4], &[1, 2, 3, 255]);
        assert_eq!(&px[64 * 4..64 * 4 + 4], &[0, 0, 0, 255]);
        assert_eq!(&px[(70 + 64 + 1) * 4..(70 + 64 + 2) * 4], &[7, 7, 7, 255], "row 1, col 1 of the raw tile");

        // Rect 2 (same zlib stream): 4x2 packed palette, 1 bit per pixel.
        let mut raw = vec![2];
        raw.extend(bgr(10, 0, 0));
        raw.extend(bgr(0, 20, 0));
        raw.push(0b1010_0000); // row 0: 1,0,1,0
        raw.push(0b0101_0000); // row 1: 0,1,0,1
        let px = dec.decode(&zlib_chunk(&mut comp, &raw), 4, 2).unwrap();
        let at = |x: usize, y: usize| px[(y * 4 + x) * 4..(y * 4 + x) * 4 + 3].to_vec();
        assert_eq!(at(0, 0), [0, 20, 0]);
        assert_eq!(at(1, 0), [10, 0, 0]);
        assert_eq!(at(0, 1), [10, 0, 0]);
        assert_eq!(at(3, 1), [0, 20, 0]);

        // Rect 3: plain RLE 3x2: 4 red pixels, then 2 blue (run lengths 4 and 2).
        let mut raw = vec![128];
        raw.extend(bgr(0, 0, 255));
        raw.push(3); // run of 4
        raw.extend(bgr(255, 0, 0));
        raw.push(1); // run of 2
        let px = dec.decode(&zlib_chunk(&mut comp, &raw), 3, 2).unwrap();
        assert_eq!(&px[3 * 4..3 * 4 + 3], &[0, 0, 255]);
        assert_eq!(&px[4 * 4..4 * 4 + 3], &[255, 0, 0]);

        // Rect 4: palette RLE 2x2 with a long run (256 = 255 + 1 -> bytes 255, 0 means 256): use 4 px.
        let mut raw = vec![128 | 2];
        raw.extend(bgr(9, 9, 9));
        raw.extend(bgr(8, 8, 8));
        raw.push(0x80); // index 0 run...
        raw.push(2); // ...of 3
        raw.push(1); // single index 1
        let px = dec.decode(&zlib_chunk(&mut comp, &raw), 2, 2).unwrap();
        assert_eq!(&px[0..3], &[9, 9, 9]);
        assert_eq!(&px[2 * 4..2 * 4 + 3], &[9, 9, 9]);
        assert_eq!(&px[3 * 4..3 * 4 + 3], &[8, 8, 8]);
    }

    #[test]
    fn zrle_rejects_truncated_and_overrunning_data() {
        let mut comp = flate2::Compress::new(flate2::Compression::default(), true);
        let mut dec = Zrle::new();
        assert!(dec.decode(&zlib_chunk(&mut comp, &[0, 1, 2]), 4, 4).is_err(), "raw tile cut short");
        let mut comp = flate2::Compress::new(flate2::Compression::default(), true);
        let mut dec = Zrle::new();
        let mut raw = vec![128];
        raw.extend([1, 2, 3, 200]); // run of 201 in a 2x2 tile
        assert!(dec.decode(&zlib_chunk(&mut comp, &raw), 2, 2).is_err());
        assert!(Zrle::new().decode(&[1, 2, 3, 4, 5], 2, 2).is_err(), "garbage is not zlib");
    }

    #[test]
    fn end_to_end_zrle_update_and_stage_events() {
        let (port, srv) = fake_server(vec![1], |_| true, |s| {
            let hdr = {
                read_exact_n(s, 20).unwrap();
                let h = read_exact_n(s, 4).unwrap();
                let n = u16::from_be_bytes([h[2], h[3]]) as usize;
                let encs = read_exact_n(s, n * 4).unwrap();
                read_exact_n(s, 10).unwrap();
                encs
            };
            assert_eq!(&hdr[0..4], &16i32.to_be_bytes(), "ZRLE must be the preferred encoding");
            let mut comp = flate2::Compress::new(flate2::Compression::default(), true);
            let mut raw = vec![1u8];
            raw.extend([0, 255, 0]); // solid green over the 8x4 screen
            let z = zlib_chunk(&mut comp, &raw);
            let mut m = vec![0, 0, 0, 1];
            m.extend(rect_header(0, 0, 8, 4, ENC_ZRLE));
            m.extend((z.len() as u32).to_be_bytes());
            m.extend(z);
            s.write_all(&m).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let h = connect(Config { host: "127.0.0.1".into(), port, username: String::new(), password: String::new() });
        let mut stages = vec![];
        loop {
            match h.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Stage(t) => stages.push(t),
                Event::Connected { .. } => break,
                other => panic!("{other:?}"),
            }
        }
        assert!(stages.iter().any(|t| t.contains("Looking up")), "{stages:?}");
        assert!(stages.iter().any(|t| t.contains("Negotiating")), "{stages:?}");
        assert!(stages.iter().any(|t| t.contains("security types")), "{stages:?}");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if h.fb.lock().unwrap().bgra[..4] == [0, 255, 0, 255] {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "ZRLE frame never decoded");
            std::thread::sleep(Duration::from_millis(10));
        }
        srv.join().unwrap();
    }
}
