//! A running Apple High Performance session: handshake, startup burst, and the
//! threads that keep video, RTCP feedback and the control channel flowing.

use super::crypto::{aes128_ecb_encrypt, Enc1103Rx, Enc1103Tx, SrtcpRx, SrtcpTx, SrtpRx, SrtpTx, SRTP_BLOB_LEN};
use super::clipboard;
use super::filecopy;
use super::decode::{hevc_type, Codec, Picture, VideoDecoder};
use super::negotiate::{self, fbu_request, Negotiated, Params};
use super::net::{self, is_timeout};
use super::offers::{self, CodecOffer, Offers};
use super::rtp::{self, AuBuffer, Unit};
use super::Config;
use crate::rfb::{Closer, Event, Framebuffer, Input, RemoteDisplay, Screen};
use anyhow::{anyhow, bail, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, Sender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(500);
/// Payload-type 101 keepalive Apple's own viewer sends on the audio SSRC.
const HEARTBEAT: [u8; 4] = [0x00, 0x68, 0x34, 0x00];
/// Pictures must stop for this long before a new SSRC generation is adopted.
const ADOPT_AFTER: Duration = Duration::from_secs(2);
const ADOPT_MIN_PACKETS: u64 = 5;
const FIR_RETRY: Duration = Duration::from_secs(1);
const STARTUP_WINDOW: Duration = Duration::from_secs(4);
/// How long after a display switch a new stream is assumed to be that display.
const SWITCH_WINDOW: Duration = Duration::from_secs(4);

// ----- startup ------------------------------------------------------------------

/// What the initial burst gave us: decoder parameters and the first slices.
struct Startup {
    srtp: SrtpRx,
    ssrc: u32,
    codec: Codec,
    /// SPS and PPS (H.264), or VPS, SPS and PPS (HEVC).
    params: Vec<Vec<u8>>,
    /// The NAL units of each complete access unit, in order.
    units: Vec<Vec<Vec<u8>>>,
    /// Packets of access units still missing their tail; seeds the AU buffer.
    pending: Vec<(u32, u32, u16, bool, Vec<u8>)>,
}

struct Starved {
    packets: usize,
}

fn sort_seq(g: &mut [(u16, bool, Vec<u8>)]) {
    g.sort_by_key(|p| p.0);
    if g.len() >= 2 && g[g.len() - 1].0 - g[0].0 > 0x8000 {
        let n = g.len();
        let at = (0..n).max_by_key(|&i| g[(i + 1) % n].0.wrapping_sub(g[i].0)).unwrap();
        g.rotate_left((at + 1) % n);
    }
}

/// Parse a snapshot of encrypted startup packets with fresh SRTP state.
fn parse_snapshot(packets: &[Vec<u8>], key: &[u8; SRTP_BLOB_LEN]) -> Result<Startup, Starved> {
    let mut srtp = SrtpRx::new(key);
    let mut groups: HashMap<(u32, u32), Vec<(u16, bool, Vec<u8>)>> = HashMap::new();
    let mut order = vec![];
    let (mut avc, mut hevc) = (false, false);
    let mut any_cfg = None;
    let mut hevc_params: [Option<Vec<u8>>; 3] = Default::default();
    for p in packets {
        let Some(r) = srtp.decrypt(p) else { continue };
        if r.payload.first() == Some(&rtp::CONFIG_MARKER) {
            if let Some(c) = rtp::parse_avc_config(&r.payload) {
                avc = true;
                any_cfg.get_or_insert(c);
            }
        }
        if rtp::looks_like_hevc(&r.payload) {
            hevc = true;
            // Parameter sets arrive in an aggregation packet ahead of each IRAP picture.
            for n in rtp::reassemble_h265(&[&r.payload]).into_iter().filter(|n| (32..=34).contains(&hevc_type(n))) {
                let i = (hevc_type(&n) - 32) as usize;
                hevc_params[i] = Some(n);
            }
        }
        let k = (r.ssrc, r.ts);
        groups.entry(k).or_insert_with(|| {
            order.push(k);
            vec![]
        });
        let g = groups.get_mut(&k).unwrap();
        if !g.iter().any(|q| q.0 == r.seq) {
            g.push((r.seq, r.marker, r.payload));
        }
    }
    let starved = || Starved { packets: packets.len() };
    let Some(ssrc) = srtp.counts.iter().max_by_key(|(_, c)| **c).map(|(s, _)| *s) else {
        if !packets.is_empty() {
            let sizes: Vec<usize> = packets.iter().take(8).map(Vec::len).collect();
            let ssrcs: std::collections::BTreeSet<String> = packets.iter().filter(|p| p.len() >= 12).map(|p| format!("{:02x}{:02x}{:02x}{:02x}", p[8], p[9], p[10], p[11])).collect();
            log::debug!("HP startup: none of {} packets decrypt (sizes {sizes:?}, ssrcs {ssrcs:?})", packets.len());
        }
        return Err(starved());
    };
    let codec = if hevc && !avc { Codec::Hevc } else { Codec::Avc };
    let (mut params, mut units, mut pending) = (None, vec![], vec![]);
    for k in order.into_iter().filter(|k| k.0 == ssrc) {
        let mut g = groups.remove(&k).unwrap();
        if !g.iter().any(|p| p.1) {
            pending.extend(g.into_iter().map(|(seq, m, p)| (k.0, k.1, seq, m, p)));
            continue;
        }
        sort_seq(&mut g);
        for p in &g {
            if p.2.first() == Some(&rtp::CONFIG_MARKER) {
                if let Some(c) = rtp::parse_avc_config(&p.2) {
                    params = Some(c);
                }
            }
        }
        let payloads: Vec<&[u8]> = g.iter().map(|p| p.2.as_slice()).collect();
        let unit: Vec<Vec<u8>> = match codec {
            Codec::Avc => rtp::reassemble_h264(&payloads).into_iter().filter(|n| (1..=5).contains(&(n[0] & 0x1f))).collect(),
            Codec::Hevc => rtp::reassemble_h265(&payloads),
        };
        if !unit.is_empty() {
            units.push(unit);
        }
    }
    let params = match codec {
        Codec::Avc => params.or(any_cfg).map(|(sps, pps)| vec![sps, pps]),
        Codec::Hevc => hevc_params.into_iter().collect::<Option<Vec<_>>>(),
    };
    let Some(params) = params else {
        if log::log_enabled!(log::Level::Debug) {
            let mut srtp = SrtpRx::new(key);
            let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
            for p in packets {
                if let Some(r) = srtp.decrypt(p) {
                    let head: Vec<String> = r.payload.iter().take(2).map(|b| format!("{b:02x}")).collect();
                    *kinds.entry(format!("ssrc {:08x} pt {} head {}", r.ssrc, r.header[1] & 0x7f, head.join(" "))).or_default() += 1;
                }
            }
            log::debug!("HP startup without {codec:?} parameter sets: {kinds:?}");
        }
        return Err(starved());
    };
    if codec == Codec::Hevc {
        log::info!("HP: the Mac chose HEVC");
    }
    Ok(Startup { srtp, ssrc, codec, params, units, pending })
}

/// Sole reader of the video socket while TCP negotiation runs, so a previous
/// session's traffic can't fill the kernel queue and drop the new configuration.
struct Collector {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<VecDeque<Vec<u8>>>,
}

impl Collector {
    fn start(sock: UdpSocket) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let thread = thread::spawn(move || {
            let (mut q, mut bytes) = (VecDeque::new(), 0usize);
            let mut buf = vec![0u8; 65536];
            let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
            while !st.load(Ordering::SeqCst) {
                if let Ok(n) = sock.recv(&mut buf) {
                    bytes += n;
                    q.push_back(buf[..n].to_vec());
                    while q.len() > 32768 || bytes > 32 << 20 {
                        bytes -= q.pop_front().map_or(0, |p| p.len());
                    }
                }
            }
            q
        });
        Self { stop, thread }
    }

    fn finish(self) -> Vec<Vec<u8>> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().map(Vec::from).unwrap_or_default()
    }
}

fn gather(sock: &UdpSocket, packets: &mut Vec<Vec<u8>>, key: &[u8; SRTP_BLOB_LEN], stopping: &AtomicBool) -> Result<Startup, Starved> {
    let deadline = Instant::now() + STARTUP_WINDOW;
    let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
    let mut buf = vec![0u8; 65536];
    let (mut dirty, mut next_parse) = (true, Instant::now());
    let mut failure = Starved { packets: 0 };
    let mut bytes: usize = packets.iter().map(Vec::len).sum();
    loop {
        let now = Instant::now();
        if dirty && (now >= next_parse || now >= deadline) {
            match parse_snapshot(packets, key) {
                Ok(s) => return Ok(s),
                Err(f) => failure = f,
            }
            dirty = false;
            next_parse = Instant::now() + Duration::from_millis(50);
        }
        if Instant::now() >= deadline || stopping.load(Ordering::SeqCst) || packets.len() > 65536 || bytes > 64 << 20 {
            return Err(failure);
        }
        if let Ok(n) = sock.recv(&mut buf) {
            bytes += n;
            packets.push(buf[..n].to_vec());
            dirty = true;
        }
    }
}

// ----- shared state -----------------------------------------------------------

#[derive(Clone)]
struct Layout {
    backing: (u32, u32),
    displays: Vec<Display>,
}

/// One Mac display within the combined desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Display {
    /// The Mac's display id (for SetDisplayMessage).
    id: u32,
    /// In backing pixels.
    pixel: Screen,
    /// In points (the coordinate space pointer events use).
    point: Screen,
}

struct Shared {
    /// The SSRC we decode (Apple starts a new one for each encoder generation).
    ssrc: u32,
    blacklist: HashSet<u32>,
    seq: HashMap<u32, (u32, u16)>,
    nack: HashSet<u16>,
    keyframe_wanted: bool,
    last_fir: Option<Instant>,
    fir_seq: u8,
    post_layout_fir: bool,
    last_publish: Instant,
    last_packet: Option<Instant>,
    last_restart: Instant,
    restart_decoder: bool,
    /// The display geometry changed; wait for the new avcC configuration.
    reconfig: Option<Instant>,
    server_sr: HashMap<u32, (u32, Instant)>,
    canvas: (u32, u32),
    /// Region of the Mac's pointer coordinate space shown by the current picture.
    pointer_area: Screen,
    /// Size of the last published picture, so input never waits on the framebuffer lock.
    pic_size: (u32, u32),
    /// When the last click or key went out, to log how soon the Mac answers with a picture.
    input_at: Option<Instant>,
    /// The display the user picked (SetDisplayMessage); `None` = the Mac's choice.
    selected: Option<u32>,
    /// A display switch was requested at this time; its stream is adopted at once.
    switching: Option<(u32, Instant)>,
    dead: Option<String>,
}

/// UDP feedback path to the Mac (RTCP and keepalives go to its control port).
struct Media {
    ctrl: UdpSocket,
    host: SocketAddrV4,
    srtcp: Mutex<SrtcpTx>,
    our_ssrc: u32,
}

impl Media {
    fn rtcp(&self, pkt: &[u8]) {
        let p = self.srtcp.lock().unwrap().protect(pkt);
        let _ = self.ctrl.send_to(&p, self.host);
    }

    fn keyframe(&self, sh: &mut Shared, force: bool) {
        sh.keyframe_wanted = true;
        let now = Instant::now();
        if !force && sh.last_fir.map_or(false, |t| now - t < Duration::from_millis(250)) {
            return;
        }
        sh.last_fir = Some(now);
        sh.fir_seq = sh.fir_seq.wrapping_add(1);
        log::debug!("HP: keyframe request #{}", sh.fir_seq);
        self.rtcp(&rtp::keyframe_request(self.our_ssrc, sh.ssrc, sh.fir_seq));
    }
}

/// Encrypted control channel writer; sealing and writing stay paired.
struct Control {
    w: Mutex<(Enc1103Tx, TcpStream)>,
    cbc_key: [u8; 16],
    /// Files being copied to the Mac, waiting for its replies.
    transfers: filecopy::Transfers,
}

impl Control {
    fn send(&self, msg: &[u8]) -> std::io::Result<()> {
        let mut g = self.w.lock().unwrap();
        let rec = g.0.seal(msg);
        g.1.write_all(&rec)
    }

    /// Apple's viewer wraps pointer events in msg 0x10 (AES-ECB under the CBC key).
    fn pointer(&self, buttons: u8, x: u16, y: u16) -> std::io::Result<()> {
        let mut b = [0u8; 16];
        b[10] = 0xff;
        b[11] = buttons;
        b[12..14].copy_from_slice(&x.to_be_bytes());
        b[14..16].copy_from_slice(&y.to_be_bytes());
        aes128_ecb_encrypt(&self.cbc_key, &mut b);
        let mut m = vec![0x10, 0];
        m.extend_from_slice(&b);
        self.send(&m)
    }
}

/// RFB button bits (1 left, 2 middle, 4 right) to Apple's (2 right, 4 middle).
pub fn apple_buttons(b: u8) -> u8 {
    (b & !0b110) | ((b & 2) << 1) | ((b & 4) >> 1)
}

/// Map a framebuffer pixel into `area`, the region of the Mac's pointer coordinate
/// space the video shows (an empty area means "the picture, 1:1"). See
/// `picture_geometry` for how that region is chosen.
pub fn to_logical(fb: (u32, u32), area: Screen, x: u16, y: u16) -> Option<(u16, u16)> {
    let (fw, fh) = fb;
    if fw == 0 || fh == 0 {
        return None;
    }
    let (lw, lh) = if area.w > 0 && area.h > 0 { (area.w, area.h) } else { fb };
    let sx = area.x as u64 + (x as u64 * lw as u64 / fw as u64).min(lw as u64 - 1);
    let sy = area.y as u64 + (y as u64 * lh as u64 / fh as u64).min(lh as u64 - 1);
    Some((sx.min(0xffff) as u16, sy.min(0xffff) as u16))
}

/// AppleDisplayLayout (encoding 0x451) → (scaled, backing, displays). Each 56-byte
/// display entry holds its id at +16, then (y0, x0, y1, x1) rectangles in points
/// at +20 and in backing pixels at +28.
fn parse_layout(p: &[u8]) -> Option<((u32, u32), (u32, u32), Vec<Display>)> {
    if p.len() < 20 {
        return None;
    }
    let u = |o: usize| u16::from_be_bytes([p[o], p[o + 1]]) as u32;
    let (scaled, backing) = ((u(2), u(4)), (u(6), u(8)));
    let count = u(18) as usize;
    let mut displays = vec![];
    for i in 0..count {
        let o = 20 + 56 * i;
        if o + 56 > p.len() {
            break;
        }
        let rect = |o: usize| {
            let (y0, x0, y1, x1) = (u(o), u(o + 2), u(o + 4), u(o + 6));
            (x1 > x0 && y1 > y0).then(|| Screen { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
        };
        if let (Some(point), Some(pixel)) = (rect(o + 20), rect(o + 28)) {
            let id = u32::from_be_bytes([p[o + 16], p[o + 17], p[o + 18], p[o + 19]]);
            displays.push(Display { id, pixel, point });
        }
    }
    Some((scaled, backing, displays))
}

/// Work out what the decoded picture shows: returns the area of the Mac's pointer
/// coordinate space (backing pixels of the whole desktop) that pointer events map
/// into, the per-monitor rectangles within the picture, and the id of the one
/// display the picture shows (`None` when it shows the whole desktop).
///
/// With a virtual display the Mac encodes only that display, while the layout
/// still lists every display (e.g. the MacBook's own panel beside it). A display
/// whose pixel size equals the picture is that display; otherwise the picture is
/// the whole desktop, scaled.
///
/// A display picked with SetDisplayMessage (`selected`) is always that display:
/// the Mac scales it into the canvas negotiated at connect time, so its size
/// need not match (a 6K display arrives in a 5K iMac's 5120x2880 canvas).
///
/// With two virtual displays (`slot` = this stream's index) each stream shows
/// one of them: the last two displays in the layout, left to right.
fn picture_geometry(pic: (u32, u32), layout: Option<&Layout>, canvas: (u32, u32), selected: Option<u32>, slot: Option<usize>) -> (Screen, Vec<Screen>, Option<u32>) {
    let (w, h) = pic;
    let Some(l) = layout.filter(|l| l.backing.0 > 0 && l.backing.1 > 0) else {
        let (cw, ch) = if canvas.0 > 0 && canvas.1 > 0 { canvas } else { pic };
        return (Screen { x: 0, y: 0, w: cw, h: ch }, vec![], None);
    };
    if let Some(d) = selected.and_then(|id| l.displays.iter().find(|d| d.id == id)) {
        return (d.pixel, vec![], Some(d.id));
    }
    if let (Some(i), n) = (slot, l.displays.len()) {
        if n >= 2 {
            let mut pair = [l.displays[n - 2], l.displays[n - 1]];
            pair.sort_by_key(|d| (d.pixel.x, d.pixel.y));
            let d = pair[i.min(1)];
            return (d.pixel, vec![], Some(d.id));
        }
    }
    if l.displays.len() > 1 {
        if let Some(d) = l.displays.iter().rev().find(|d| (d.pixel.w, d.pixel.h) == (w, h)) {
            // Pointer positions go to the display's place in the desktop measured in
            // backing pixels. Verified live on a MacBook (2880x1800 panel) with a 1x
            // virtual display at pixel x=2880: pixel offsets hit the right menu item;
            // point offsets (x=1440) and display-relative positions missed.
            return (d.pixel, vec![], Some(d.id));
        }
    }
    let (dw, dh) = l.backing;
    let scale = |v: u32, num: u32, den: u32| ((v as u64 * num as u64 + den as u64 / 2) / den as u64).min(num as u64) as u32;
    let screens = l
        .displays
        .iter()
        .filter_map(|d| {
            let p = d.pixel;
            let (x0, y0, x1, y1) = (scale(p.x, w, dw), scale(p.y, h, dh), scale(p.x + p.w, w, dw), scale(p.y + p.h, h, dh));
            (x1 > x0 && y1 > y0).then(|| Screen { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
        })
        .collect::<Vec<_>>();
    // The picture is the whole desktop: pointer positions are its backing pixels
    // (like RFB framebuffer coordinates; ServerInit reports the pixel size too).
    let only = (l.displays.len() == 1).then(|| l.displays[0].id);
    (Screen { x: 0, y: 0, w: dw, h: dh }, if screens.len() > 1 { screens } else { vec![] }, only)
}

// ----- threads ----------------------------------------------------------------

/// One video stream and what it shares with the session. The first stream shows
/// the Mac's display; with two virtual displays a second `Ctx` (stream 1) shows
/// the other one, sharing the control channel, layout and halt flag.
struct Ctx {
    /// 0: the first video stream; 1: the second virtual display's.
    stream: usize,
    shared: Mutex<Shared>,
    media: Media,
    control: Arc<Control>,
    layout: Arc<Mutex<Option<Layout>>>,
    /// Two virtual displays are in use, one per stream.
    dual: Arc<AtomicBool>,
    fb: Arc<Mutex<Framebuffer>>,
    events: Sender<Event>,
    halt: Arc<AtomicBool>,
    name: String,
    offers: Arc<Offers>,
    keys: Arc<negotiate::Keys>,
}

fn new_shared(ssrc: u32, seq: HashMap<u32, (u32, u16)>, canvas: (u32, u32), started: bool) -> Shared {
    let now = Instant::now();
    Shared {
        ssrc,
        blacklist: HashSet::new(),
        seq,
        nack: HashSet::new(),
        keyframe_wanted: started,
        last_fir: None,
        fir_seq: 0,
        post_layout_fir: false,
        last_publish: now,
        last_packet: started.then_some(now),
        last_restart: now,
        restart_decoder: false,
        reconfig: None,
        server_sr: HashMap::new(),
        canvas,
        pointer_area: Screen { x: 0, y: 0, w: 0, h: 0 },
        pic_size: (0, 0),
        input_at: None,
        selected: None,
        switching: None,
        dead: None,
    }
}

impl Ctx {
    fn halted(&self) -> bool {
        self.halt.load(Ordering::SeqCst)
    }

    fn publish(&self, pic: &Picture, bgra: &mut Vec<u8>, last_size: &mut (u32, u32)) {
        let (w, h) = (pic.width, pic.height);
        let layout = self.layout.lock().unwrap().clone();
        let slot = self.dual.load(Ordering::SeqCst).then_some(self.stream);
        let (area, screens, showing, displays) = {
            let mut sh = self.shared.lock().unwrap();
            sh.last_publish = Instant::now();
            sh.keyframe_wanted = false;
            // Geometry follows the decoded size; monitor rectangles shrink with a halved picture.
            let k = pic.scale.max(1);
            let mut r = picture_geometry((w * k, h * k), layout.as_ref(), sh.canvas, sh.selected, slot);
            for m in &mut r.1 {
                *m = Screen { x: m.x / k, y: m.y / k, w: m.w / k, h: m.h / k };
            }
            sh.pointer_area = r.0;
            sh.pic_size = (w, h);
            if let Some(t) = sh.input_at.take() {
                log::debug!("HP: next picture {} ms after the last click/key was sent", t.elapsed().as_millis());
            }
            let displays = layout.as_ref().map_or(vec![], |l| l.displays.iter().map(|d| RemoteDisplay { id: d.id, x: d.pixel.x, y: d.pixel.y, width: d.pixel.w, height: d.pixel.h }).collect());
            (r.0, r.1, r.2, displays)
        };
        {
            let mut f = self.fb.lock().unwrap();
            std::mem::swap(&mut f.bgra, bgra);
            f.width = w;
            f.height = h;
            f.logical = if (area.w, area.h) == (w, h) || area.w == 0 { (0, 0) } else { (area.w, area.h) };
            f.screens = screens;
            f.showing = showing;
            f.displays = displays;
            f.name = self.name.clone();
            f.version += 1;
            f.published = Some(Instant::now());
            f.halved = pic.scale > 1;
            f.whole_frames = true;
            f.wake();
        }
        if *last_size != (w, h) {
            *last_size = (w, h);
            if self.stream == 0 {
                let _ = self.events.send(Event::Connected { width: w, height: h, name: self.name.clone() });
            }
        }
    }
}

/// Packets older than this when the decoder reaches them mean it has fallen
/// behind the Mac; the backlog is dropped and a fresh keyframe requested.
const MAX_PACKET_AGE: Duration = Duration::from_millis(1000);

fn video_loop(ctx: Arc<Ctx>, packets: Receiver<(Instant, Vec<u8>)>, start: Startup) {
    let Startup { mut srtp, codec, params, units: first_units, pending, .. } = start;
    let mut dec = VideoDecoder::new(codec, &params);
    let mut au = AuBuffer::default();
    let mut bgra = vec![];
    let mut size = (0, 0);
    // When the picture froze (a lost or undecodable frame), for recovery logging.
    let mut frozen: Option<Instant> = None;
    let mut units = vec![];
    // Throughput stats, logged every few seconds at debug level.
    let mut stats_at = Instant::now();
    let (mut n_pkts, mut n_pics, mut max_age, mut decoding) = (0u32, 0u32, Duration::ZERO, Duration::ZERO);

    for u in &first_units {
        if let Ok(Some(p)) = dec.feed_au(u, &mut bgra) {
            ctx.publish(&p, &mut bgra, &mut size);
        }
    }
    if dec.waiting_for_keyframe() {
        let mut sh = ctx.shared.lock().unwrap();
        ctx.media.keyframe(&mut sh, true);
    }
    for (ssrc, ts, seq, m, p) in pending {
        au.push(ssrc, ts, seq, m, p, &mut units);
    }

    // SEEMYMAC_HP_RTPDUMP=file records decrypted video RTP for offline analysis.
    let mut rtp_dump = std::env::var_os("SEEMYMAC_HP_RTPDUMP").and_then(|p| std::fs::File::create(p).ok());
    while !ctx.halted() {
        if stats_at.elapsed() >= Duration::from_secs(5) {
            log::debug!(
                "HP video: {n_pkts} packets, {n_pics} pictures, video thread inside the decoder {:.0}% of the time, oldest packet waited {} ms",
                decoding.as_secs_f32() * 100.0 / stats_at.elapsed().as_secs_f32(),
                max_age.as_millis()
            );
            if let VideoDecoder::Hevc(d) = &mut dec {
                let (n, mean, worst) = d.take_latency();
                log::debug!("HP HEVC: {n} pictures, decoding latency mean {} ms, worst {} ms", mean.as_millis(), worst.as_millis());
            }
            stats_at = Instant::now();
            (n_pkts, n_pics, max_age, decoding) = (0, 0, Duration::ZERO, Duration::ZERO);
        }
        // Pictures decode on worker threads; collect finished ones promptly.
        match dec.poll(&mut bgra) {
            Ok(Some(p)) => {
                n_pics += 1;
                if let Some(t) = frozen.take() {
                    log::info!("HP: picture resumed after {} ms", t.elapsed().as_millis());
                }
                ctx.publish(&p, &mut bgra, &mut size);
            }
            Ok(None) => {}
            Err(e) => {
                log::debug!("HP: {e}; requesting a keyframe");
                frozen.get_or_insert_with(Instant::now);
                let mut sh = ctx.shared.lock().unwrap();
                ctx.media.keyframe(&mut sh, false);
            }
        }
        let wait = if dec.busy() { Duration::from_millis(2) } else { Duration::from_millis(20) };
        match packets.recv_timeout(wait) {
            Ok((at, pkt)) => {
                let age = at.elapsed();
                n_pkts += 1;
                max_age = max_age.max(age);
                if age > MAX_PACKET_AGE {
                    let mut dropped = 1;
                    while packets.try_recv().is_ok() {
                        dropped += 1;
                    }
                    log::warn!("HP: video is {} ms behind the Mac; dropped {dropped} queued packets and asked for a fresh keyframe", age.as_millis());
                    dec.mark_broken();
                    frozen.get_or_insert_with(Instant::now);
                    let mut sh = ctx.shared.lock().unwrap();
                    ctx.media.keyframe(&mut sh, true);
                    continue;
                }
                if let Some(r) = srtp.decrypt(&pkt) {
                    if let Some(f) = rtp_dump.as_mut() {
                        // [u16 header len][u16 payload len][header][payload]
                        let _ = f.write_all(&(r.header.len() as u16).to_be_bytes());
                        let _ = f.write_all(&(r.payload.len() as u16).to_be_bytes());
                        let _ = f.write_all(&r.header);
                        let _ = f.write_all(&r.payload);
                    }
                    if track(&ctx, &srtp, r.ssrc, r.seq) {
                        dec.restart();
                        au.retain_ssrc(r.ssrc);
                    }
                    au.push(r.ssrc, r.ts, r.seq, r.marker, r.payload, &mut units);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        au.evict(&mut units);
        {
            let mut sh = ctx.shared.lock().unwrap();
            if std::mem::take(&mut sh.restart_decoder) {
                dec.restart();
                ctx.media.keyframe(&mut sh, true);
            }
        }
        for u in units.drain(..) {
            let current = ctx.shared.lock().unwrap().ssrc;
            match u {
                Unit::Lost { ssrc } if ssrc == current => {
                    log::debug!("HP: access unit lost; waiting for a keyframe");
                    dec.mark_broken();
                    frozen.get_or_insert_with(Instant::now);
                    let mut sh = ctx.shared.lock().unwrap();
                    ctx.media.keyframe(&mut sh, false);
                }
                Unit::Complete { ssrc, payloads } if ssrc == current => {
                    // Apple sends fresh parameter sets when the geometry changes:
                    // an avcC config packet (H.264) or an in-band VPS/SPS/PPS (HEVC).
                    let mut reconfig = ctx.shared.lock().unwrap().reconfig.is_some();
                    let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
                    let nals = match &mut dec {
                        VideoDecoder::Avc(d) => {
                            for p in payloads.iter().filter(|p| p.first() == Some(&rtp::CONFIG_MARKER)) {
                                if let Some((sps, pps)) = rtp::parse_avc_config(p) {
                                    if reconfig || sps != d.sps() {
                                        log::info!("HP: new H.264 configuration ({} byte SPS)", sps.len());
                                        d.set_params(sps, pps);
                                        reconfig = false;
                                        let mut sh = ctx.shared.lock().unwrap();
                                        sh.reconfig = None;
                                        ctx.media.keyframe(&mut sh, true);
                                    }
                                }
                            }
                            rtp::reassemble_h264(&refs)
                        }
                        VideoDecoder::Hevc(_) => {
                            let nals = rtp::reassemble_h265(&refs);
                            if reconfig && nals.iter().any(|n| hevc_type(n) == 33) {
                                log::info!("HP: new HEVC configuration");
                                reconfig = false;
                                ctx.shared.lock().unwrap().reconfig = None;
                            }
                            nals
                        }
                    };
                    if reconfig {
                        continue; // don't decode new-geometry slices with the old SPS
                    }
                    {
                        let t = Instant::now();
                        let fed = dec.feed_au(&nals, &mut bgra);
                        decoding += t.elapsed();
                        match fed {
                            Ok(Some(p)) => {
                                n_pics += 1;
                                if let Some(t) = frozen.take() {
                                    log::info!("HP: picture resumed after {} ms", t.elapsed().as_millis());
                                }
                                ctx.publish(&p, &mut bgra, &mut size)
                            }
                            // e.g. new HEVC parameter sets without an IRAP picture yet
                            Ok(None) if dec.waiting_for_keyframe() => ctx.shared.lock().unwrap().keyframe_wanted = true,
                            Ok(None) => {}
                            Err(e) => {
                                log::debug!("HP: {e}; requesting a keyframe");
                                frozen.get_or_insert_with(Instant::now);
                                let mut sh = ctx.shared.lock().unwrap();
                                ctx.media.keyframe(&mut sh, false);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Sequence/loss accounting and SSRC-generation adoption. Returns true when a
/// new SSRC was adopted (the caller resets its decoder).
fn track(ctx: &Ctx, srtp: &SrtpRx, ssrc: u32, seq: u16) -> bool {
    let mut sh = ctx.shared.lock().unwrap();
    let now = Instant::now();
    sh.last_packet = Some(now);
    if ssrc != sh.ssrc {
        // After a requested display switch the new stream is the one we asked for:
        // adopt it at once instead of waiting for the old pictures to stop.
        let switching = sh.switching.map_or(false, |(_, t)| now - t < SWITCH_WINDOW);
        if !sh.blacklist.contains(&ssrc) && (switching || now - sh.last_publish >= ADOPT_AFTER) && srtp.counts.get(&ssrc).copied().unwrap_or(0) >= ADOPT_MIN_PACKETS {
            log::info!("HP: adopting new video stream {ssrc:#010x}");
            if let Some((id, t)) = sh.switching.take() {
                log::info!("HP: now showing display {id:#x} ({} ms after asking)", t.elapsed().as_millis());
                sh.selected = Some(id);
            }
            let old = sh.ssrc;
            sh.blacklist.insert(old);
            sh.ssrc = ssrc;
            sh.nack.clear();
            sh.last_publish = now; // grace period before another adoption
            sh.last_restart = now;
            sh.seq.insert(ssrc, (0, seq));
            ctx.media.keyframe(&mut sh, true);
            return true;
        }
        let e = sh.seq.entry(ssrc).or_insert((0, seq));
        e.1 = seq;
        return false;
    }
    let Some(&(roc, prev)) = sh.seq.get(&ssrc) else {
        sh.seq.insert(ssrc, (0, seq));
        return false;
    };
    let diff = seq.wrapping_sub(prev);
    if diff == 0 {
        return false;
    }
    if diff > 0x8000 {
        sh.nack.remove(&seq); // a late retransmission
        return false;
    }
    for k in 1..diff.min(32) {
        sh.nack.insert(prev.wrapping_add(k));
    }
    sh.seq.insert(ssrc, (if seq < prev { roc + 1 } else { roc }, seq));
    if diff > 1 {
        let lost = std::mem::take(&mut sh.nack);
        if let Some(n) = rtp::nack(ctx.media.our_ssrc, ssrc, &lost) {
            ctx.media.rtcp(&n);
        }
    }
    false
}

fn udp_reader(ctx: Arc<Ctx>, sock: UdpSocket, tx: std::sync::mpsc::SyncSender<(Instant, Vec<u8>)>) {
    let mut buf = vec![0u8; 65536];
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    while !ctx.halted() {
        match sock.recv(&mut buf) {
            Ok(n) => match tx.try_send((Instant::now(), buf[..n].to_vec())) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return,
            },
            Err(e) if is_timeout(&e) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// The control port carries audio RTP and RTCP; keep SR times for RRs and play the audio.
fn rtcp_reader(ctx: Arc<Ctx>, sock: UdpSocket, rx: SrtcpRx) {
    let mut buf = vec![0u8; 65536];
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let audio = super::audio::Audio::start();
    let mut audio_rx = audio.as_ref().map(|_| SrtpRx::new(&ctx.keys.audio_s));
    while !ctx.halted() {
        let Ok(n) = sock.recv(&mut buf) else { continue };
        let p = &buf[..n];
        if let (Some(a), Some(srtp)) = (&audio, audio_rx.as_mut()) {
            if n > 12 && p[0] >> 6 == 2 && p[1] & 0x7f == super::audio::PAYLOAD_TYPE {
                if let Some(r) = srtp.decrypt(p) {
                    a.push(r.payload);
                }
                continue;
            }
        }
        if n < 2 || p[0] & 0xc0 != 0x80 || !(200..=207).contains(&(p[1] & 0x7f)) {
            continue;
        }
        if let Some(d) = rx.unprotect(p) {
            let mut sh = ctx.shared.lock().unwrap();
            for (ssrc, mid) in rtp::sender_reports(&d) {
                sh.server_sr.insert(ssrc, (mid, Instant::now()));
            }
        }
    }
}

fn handle_control(ctx: &Ctx, m: &[u8]) {
    log::debug!("HP control message type {:#04x}, {} bytes: {:02x?}", m.first().copied().unwrap_or(0), m.len(), &m[..m.len().min(24)]);
    if m.first() != Some(&0) || m.len() < 4 {
        return; // 0x14 status pushes and anything else need no reply
    }
    let n = u16::from_be_bytes([m[2], m[3]]);
    let mut o = 4;
    for _ in 0..n {
        let Some(h) = m.get(o..o + 12) else { return };
        let enc = i32::from_be_bytes(h[8..12].try_into().unwrap());
        o += 12;
        let len16 = |o: usize| m.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize);
        match enc {
            1104 => {
                // Cursor shape: cache id, compressed size, zlib pixmap. The local pointer is used instead.
                let Some(b) = m.get(o..o + 8) else { return };
                o += 8 + u32::from_be_bytes(b[4..8].try_into().unwrap()) as usize;
            }
            0x451 => {
                let Some(len) = len16(o) else { return };
                let Some(p) = m.get(o + 2..o + 2 + len) else { return };
                o += 2 + len;
                let Some((scaled, backing, displays)) = parse_layout(p) else { continue };
                log::info!("HP display layout: scaled {}x{}, backing {}x{}, displays {:?}", scaled.0, scaled.1, backing.0, backing.1, displays);
                let changed = {
                    let mut lay = ctx.layout.lock().unwrap();
                    let mut sh = ctx.shared.lock().unwrap();
                    let changed = lay.as_ref().map_or(false, |l| l.backing != backing && backing.0 > 0);
                    let prev = lay.take();
                    *lay = Some(Layout {
                        backing: if backing.0 > 0 && backing.1 > 0 { backing } else { prev.as_ref().map_or(sh.canvas, |l| l.backing) },
                        displays,
                    });
                    if changed {
                        sh.reconfig = Some(Instant::now());
                        sh.post_layout_fir = true;
                    }
                    changed
                };
                let _ = ctx.control.send(&fbu_request(false, 0xffff, 0xffff));
                if changed {
                    // The encoder waits for a fresh media configuration after a resize.
                    let _ = ctx.control.send(&negotiate::media_message(&ctx.offers, &ctx.keys));
                }
            }
            // Config blobs (1010/1011 media, 1107 keysyms, 1109 keyboard, 1110 model) and 0x3ea.
            1010 | 1011 | 1107 | 1109 | 1110 | 0x3ea => {
                // Usually a u16 length; the Mac also sends 1011 with a u32 length
                // that spans exactly the rest of the message.
                let len32 = m.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize);
                if len32 == Some(m.len() - o - 4) {
                    o += 4 + len32.unwrap();
                    continue;
                }
                let Some(len) = len16(o) else { return };
                o += 2 + len;
            }
            _ => return, // unknown payload size: stop walking this update
        }
    }
}

/// File-copy replies (0x22: progress and end status of a drop); true when `m` was one.
fn handle_filecopy(ctx: &Ctx, fc: &mut filecopy::Reader, m: &[u8]) -> bool {
    if !fc.in_progress() && m.first() != Some(&filecopy::MSG) {
        return false;
    }
    if let Some(full) = fc.feed(m) {
        if let Some(r) = filecopy::parse_reply(&full) {
            ctx.control.transfers.dispatch(r);
        }
    }
    true
}

/// Clipboard records (0x14 change notices, 0x1f contents); true when `m` was one.
fn handle_clipboard(ctx: &Ctx, clip: &mut clipboard::Reassembler, m: &[u8]) -> bool {
    if clip.in_progress() || m.first() == Some(&clipboard::MSG_SEND) {
        if let Some(full) = clip.feed(m) {
            if let Some(t) = clipboard::text(&full) {
                // Never log the text: it may be a password.
                log::info!("HP clipboard: {} characters from the Mac", t.chars().count());
                let _ = ctx.events.send(Event::ServerClipboard(t));
            }
            let (files, types) = clipboard::files(&full);
            log::debug!("Mac pasteboard types: {types:?}");
            if !files.is_empty() {
                log::info!("HP clipboard: {} file(s) copied on the Mac", files.len());
                let _ = ctx.events.send(Event::ServerFiles(files));
            }
        }
        return true;
    }
    if clipboard::status_command(m) == Some(clipboard::STATUS_CLIPBOARD_CHANGED) {
        log::debug!("Mac pasteboard changed; fetching it");
        let _ = ctx.control.send(&clipboard::fetch());

        return true;
    }
    false
}

fn tcp_reader(ctx: Arc<Ctx>, mut s: TcpStream, mut rx: Enc1103Rx, leftover: Vec<Vec<u8>>) {
    let mut clip = clipboard::Reassembler::default();
    let mut fc = filecopy::Reader::default();
    for m in &leftover {
        if !handle_filecopy(&ctx, &mut fc, m) && !handle_clipboard(&ctx, &mut clip, m) {
            handle_control(&ctx, m);
        }
    }
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = vec![0u8; 65536];
    while !ctx.halted() {
        match s.read(&mut buf) {
            Ok(0) => {
                ctx.shared.lock().unwrap().dead = Some("The Mac ended the screen-sharing session.".into());
                return;
            }
            Ok(n) => {
                for m in rx.feed(&buf[..n]) {
                    if !handle_filecopy(&ctx, &mut fc, &m) && !handle_clipboard(&ctx, &mut clip, &m) {
                        handle_control(&ctx, &m);
                    }
                }
            }
            Err(e) if is_timeout(&e) || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                if !ctx.halted() {
                    ctx.shared.lock().unwrap().dead = Some(format!("The connection to the Mac was lost: {e}"));
                }
                return;
            }
        }
    }
}

fn tick(ctx: &Ctx, hb: &mut SrtpTx, n: u64) {
    let _ = ctx.media.ctrl.send_to(&hb.encrypt(&HEARTBEAT, 101), ctx.media.host);
    tick_stream(ctx, n);
    // Keep the whole desktop subscribed to changes, including after a resize.
    // A 1x1 request only subscribes to changes at the top-left pixel.
    let _ = ctx.control.send(&fbu_request(true, u16::MAX, u16::MAX));
}

/// Per-stream upkeep: receiver reports, keyframe retries and stall recovery.
fn tick_stream(ctx: &Ctx, n: u64) {
    let mut sh = ctx.shared.lock().unwrap();
    if sh.ssrc == 0 {
        return; // a second stream that has not started
    }
    let ssrc = sh.ssrc;
    let (roc, max) = sh.seq.get(&ssrc).copied().unwrap_or((0, 0));
    let mut rr = rtp::rr(ctx.media.our_ssrc, &[(ssrc, roc, max)], &sh.server_sr);
    if n % 10 == 0 {
        let mut sr = rtp::empty_sr(ctx.media.our_ssrc);
        sr.extend(rr);
        rr = sr;
    }
    ctx.media.rtcp(&rr);

    let now = Instant::now();
    if let Some((id, t)) = sh.switching {
        if now - t >= SWITCH_WINDOW {
            // No new stream came; the Mac switched the existing one (or ignored us).
            log::info!("HP: no new stream after switching to display {id:#x}; assuming it is shown");
            sh.switching = None;
            sh.selected = Some(id);
        }
    }
    let flowing = sh.last_packet.map_or(false, |t| now - t < Duration::from_millis(1500));
    if std::mem::take(&mut sh.post_layout_fir) {
        ctx.media.keyframe(&mut sh, true);
    }
    if sh.reconfig.map_or(false, |t| now - t > Duration::from_secs(3)) {
        log::warn!("HP: no new H.264 configuration after a layout change; continuing with the old one");
        sh.reconfig = None;
        sh.restart_decoder = true;
    }
    // Keep asking until a keyframe actually decodes (the reply can be lost too).
    if sh.keyframe_wanted && flowing && sh.last_fir.map_or(true, |t| now - t >= FIR_RETRY) {
        ctx.media.keyframe(&mut sh, true);
    }
    // Stall recovery: nothing decoded although packets keep arriving.
    let gap = now - sh.last_publish;
    if flowing && gap > Duration::from_secs(15) && now - sh.last_restart > Duration::from_secs(8) {
        log::warn!("HP: no picture for {:.0}s; restarting the decoder", gap.as_secs_f32());
        sh.last_restart = now;
        sh.restart_decoder = true;
    } else if flowing && gap > Duration::from_secs(3) && sh.last_fir.map_or(true, |t| now - t >= Duration::from_millis(1500)) {
        ctx.media.keyframe(&mut sh, true);
    }
}

/// The second virtual display's video (port after the video port): wait for it
/// to start (it only exists once two virtual displays do), then decode it like
/// the first stream.
fn second_stream(ctx: Arc<Ctx>, sock: UdpSocket, mut packets: Vec<Vec<u8>>) {
    let key = ctx.keys.video_s;
    let start = loop {
        if ctx.halted() {
            return;
        }
        match gather(&sock, &mut packets, &key, &ctx.halt) {
            Ok(s) => break s,
            Err(_) => {
                // Video without usable parameter sets: they travel with keyframes, so ask
                // the stream that is arriving for one.
                let mut counts: HashMap<u32, usize> = HashMap::new();
                for p in packets.iter().filter(|p| p.len() >= 12 && p[1] & 0x7f == 100) {
                    *counts.entry(u32::from_be_bytes([p[8], p[9], p[10], p[11]])).or_default() += 1;
                }
                if let Some((&ssrc, _)) = counts.iter().max_by_key(|(_, n)| **n) {
                    let mut sh = ctx.shared.lock().unwrap();
                    sh.ssrc = ssrc;
                    ctx.media.keyframe(&mut sh, true);
                }
                packets.clear();
            }
        }
    };
    log::info!("HP: the second display's video started ({:?})", start.codec);
    {
        let st = start.srtp.state(start.ssrc);
        let mut sh = ctx.shared.lock().unwrap();
        let canvas = sh.canvas;
        *sh = new_shared(start.ssrc, HashMap::from([(start.ssrc, (st.roc, st.max_seq))]), canvas, true);
    }
    let (tx, rx) = sync_channel(16384);
    let c = ctx.clone();
    let reader = thread::Builder::new().name("hp-udp2".into()).spawn(move || udp_reader(c, sock, tx));
    video_loop(ctx, rx, start);
    if let Ok(r) = reader {
        let _ = r.join();
    }
}

// ----- entry point --------------------------------------------------------------

/// `fb2` receives the second virtual display's pictures (two virtual displays).
#[allow(clippy::too_many_arguments)]
pub fn run(cfg: &Config, fb: &Arc<Mutex<Framebuffer>>, fb2: Option<&Arc<Mutex<Framebuffer>>>, closer: &Closer, stopping: &Arc<AtomicBool>, input: Receiver<Input>, events: &Sender<Event>) -> Result<()> {
    let stage = |t: String| {
        log::info!("{}: {t}", cfg.host);
        let _ = events.send(Event::Stage(t));
    };
    let cancelled = || stopping.load(Ordering::SeqCst);

    // Let the closer abort a blocking handshake read.
    let live_tcp: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
    {
        let (st, lt) = (stopping.clone(), live_tcp.clone());
        *closer.lock().unwrap() = Some(Box::new(move || {
            st.store(true, Ordering::SeqCst);
            if let Some(s) = lt.lock().unwrap().as_ref() {
                let _ = s.shutdown(Shutdown::Both);
            }
        }));
    }

    stage("Resolving the Mac's address…".into());
    let addr = net::resolve_ipv4(&cfg.host, cfg.port)?;
    let video_port = cfg.port.checked_add(1).ok_or_else(|| anyhow!("invalid port"))?;
    let host_ctrl = SocketAddrV4::new(*addr.ip(), cfg.port);
    let host_video = SocketAddrV4::new(*addr.ip(), video_port);
    let ctrl = net::bind_udp(cfg.port)?;
    let video = net::bind_udp(video_port)?;
    // Two virtual displays: the second one's video arrives on the next port.
    let dual_wanted = cfg.virtual_displays >= 2 && fb2.is_some();
    let host_video2 = SocketAddrV4::new(*addr.ip(), video_port.wrapping_add(1));
    let video2 = if dual_wanted { Some(net::bind_udp(video_port + 1)?) } else { None };

    stage("Opening the Mac's screen-sharing service…".into());
    negotiate::warmup(addr)?;
    if cancelled() {
        return Ok(());
    }

    // Outbound datagrams open stateful firewalls for the Mac's symmetric media.
    let punch_stop = Arc::new(AtomicBool::new(false));
    let punch = {
        let (c, v, st) = (ctrl.try_clone()?, video.try_clone()?, punch_stop.clone());
        let v2 = video2.as_ref().map(UdpSocket::try_clone).transpose()?;
        thread::spawn(move || {
            while !st.load(Ordering::SeqCst) {
                let _ = c.send_to(&[0], host_ctrl);
                let _ = v.send_to(&[0], host_video);
                if let Some(v2) = &v2 {
                    let _ = v2.send_to(&[0], host_video2);
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let on_socket = |s: &TcpStream| *live_tcp.lock().unwrap() = s.try_clone().ok();
    let params = Params { addr, username: &cfg.username, password: &cfg.password, width: cfg.width, height: cfg.height, virtual_display: cfg.virtual_display, virtual_displays: if dual_wanted { 2 } else { 1 }, on_socket: &on_socket };
    let result = (|| -> Result<(Negotiated, Offers, Startup, Vec<Vec<u8>>)> {
        let mut tries = 0;
        let mut last = Starved { packets: 0 };
        while tries < 3 {
            if cancelled() {
                bail!("cancelled");
            }
            // The Mac picks H.264 or HEVC; both are decoded. (A 5K screen can only be HEVC.)
            // A second video stream is offered for a second virtual display.
            let offers = offers::create(CodecOffer::Both, dual_wanted);
            let collector = Collector::start(video.try_clone()?);
            let collector2 = video2.as_ref().map(|v| v.try_clone().map(Collector::start)).transpose()?;
            let neg = negotiate::negotiate(&params, &offers, &stage);
            let mut packets = collector.finish();
            let packets2 = collector2.map_or(vec![], Collector::finish);
            let neg = neg?;
            if neg.canvas.0 == 0 {
                log::warn!("HP: the Mac never reported a video canvas");
            }
            stage("Waiting for the first video packets…".into());
            match gather(&video, &mut packets, &neg.keys.video_s, stopping) {
                Ok(s) => return Ok((neg, offers, s, packets2)),
                Err(s) => {
                    neg.tcp.abort();
                    log::warn!("HP: startup video incomplete ({} packets); retrying", s.packets);
                    last = s;
                    tries += 1;
                    thread::sleep(Duration::from_millis(800));
                }
            }
        }
        if last.packets == 0 {
            bail!("The Mac accepted the session but sent no video. Check that UDP ports {} and {video_port} are not blocked by a firewall.", cfg.port);
        }
        bail!("Apple High Performance video could not start ({} packets arrived without a usable video configuration).", last.packets)
    })();
    punch_stop.store(true, Ordering::SeqCst);
    let _ = punch.join();
    let (neg, offers, start, packets2) = match result {
        Err(_) if cancelled() => return Ok(()),
        r => r?,
    };

    let Negotiated { tcp, tx, rx, cbc_key, keys, canvas, leftover, .. } = neg;
    let stream = tcp.s;
    *live_tcp.lock().unwrap() = stream.try_clone().ok();
    let srtcp_rx = SrtcpRx::new(&keys.video_s);
    let mut hb = SrtpTx::new(&keys.audio_v, offers.audio_ssrc);
    let st0 = start.srtp.state(start.ssrc);
    let (offers, keys) = (Arc::new(offers), Arc::new(keys));
    let control = Arc::new(Control { w: Mutex::new((tx, stream.try_clone()?)), cbc_key, transfers: filecopy::Transfers::default() });
    let (layout, halt) = (Arc::new(Mutex::new(None)), Arc::new(AtomicBool::new(false)));
    // Connecting straight onto two virtual displays (otherwise "Use virtual display" sets it).
    let dual = Arc::new(AtomicBool::new(dual_wanted && cfg.virtual_display));
    let ctx = Arc::new(Ctx {
        stream: 0,
        shared: Mutex::new(new_shared(start.ssrc, HashMap::from([(start.ssrc, (st0.roc, st0.max_seq))]), canvas, true)),
        media: Media { ctrl: ctrl.try_clone()?, host: host_ctrl, srtcp: Mutex::new(SrtcpTx::new(&keys.video_v)), our_ssrc: offers.video_ssrc },
        control: control.clone(),
        layout: layout.clone(),
        dual: dual.clone(),
        fb: fb.clone(),
        events: events.clone(),
        halt: halt.clone(),
        name: cfg.host.clone(),
        offers: offers.clone(),
        keys: keys.clone(),
    });
    let ctx2 = match (video2, fb2) {
        (Some(sock), Some(fb2)) => Some((
            Arc::new(Ctx {
                stream: 1,
                shared: Mutex::new(new_shared(0, HashMap::new(), canvas, false)),
                media: Media { ctrl: ctrl.try_clone()?, host: host_ctrl, srtcp: Mutex::new(SrtcpTx::new(&keys.video_v)), our_ssrc: offers.video2_ssrc },
                control: control.clone(),
                layout: layout.clone(),
                dual: dual.clone(),
                fb: fb2.clone(),
                events: events.clone(),
                halt: halt.clone(),
                name: cfg.host.clone(),
                offers: offers.clone(),
                keys: keys.clone(),
            }),
            sock,
        )),
        _ => None,
    };
    stage("Apple High Performance connected; waiting for video…".into());

    let (ptx, prx) = sync_channel(16384);
    let mut threads = vec![
        { let c = ctx.clone(); thread::Builder::new().name("hp-udp".into()).spawn(move || udp_reader(c, video, ptx))? },
        { let c = ctx.clone(); thread::Builder::new().name("hp-video".into()).spawn(move || video_loop(c, prx, start))? },
        { let c = ctx.clone(); thread::Builder::new().name("hp-rtcp".into()).spawn(move || rtcp_reader(c, ctrl, srtcp_rx))? },
        { let c = ctx.clone(); let s = stream.try_clone()?; thread::Builder::new().name("hp-tcp".into()).spawn(move || tcp_reader(c, s, rx, leftover))? },
    ];
    let ctx2 = match ctx2 {
        Some((c2, sock)) => {
            let c = c2.clone();
            threads.push(thread::Builder::new().name("hp-video2".into()).spawn(move || second_stream(c, sock, packets2))?);
            Some(c2)
        }
        None => None,
    };

    // The Mac then announces each pasteboard change (0x14) for us to fetch. The
    // current contents are not fetched: on connect this PC's clipboard is sent over.
    ctx.control.send(&clipboard::enable())?;

    let result = (|| -> Result<()> {
        let mut next = Instant::now();
        let mut n = 0u64;
        let mut last_buttons = 0u8;
        // Which picture pointer positions refer to (1: the second virtual display's).
        let mut pointer_on = &ctx;
        let mut input = crate::rfb::InputQueue::new(input);
        loop {
            if cancelled() {
                return Ok(());
            }
            if let Some(why) = ctx.shared.lock().unwrap().dead.take() {
                bail!(why);
            }
            match input.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(Input::Pointer { x, y, buttons }) => {
                    let click = buttons != last_buttons;
                    if click {
                        log::debug!("HP input: pointer buttons {buttons:#x} at {x},{y}");
                        last_buttons = buttons;
                    }
                    let (size, area) = {
                        let sh = pointer_on.shared.lock().unwrap();
                        (sh.pic_size, sh.pointer_area)
                    };
                    if let Some((lx, ly)) = to_logical(size, area, x, y) {
                        ctx.control.pointer(apple_buttons(buttons), lx, ly)?;
                        if click {
                            ctx.shared.lock().unwrap().input_at = Some(Instant::now());
                        }
                    }
                }
                Ok(Input::Key { keysym, down }) => {
                    // Never log the character itself: this may be a password.
                    let kind = match keysym {
                        0xff0d => "Return",
                        0xff08 => "BackSpace",
                        0xff1b => "Escape",
                        0xff09 => "Tab",
                        0xffe1 | 0xffe2 => "Shift",
                        0xffe3 | 0xffe4 => "Control",
                        0xffe9 | 0xffea => "Alt",
                        0xffeb | 0xffec => "Command",
                        0xff00..=0xffff => "other named key",
                        _ => "character",
                    };
                    log::debug!("HP input: {kind} {}", if down { "down" } else { "up" });
                    let mut m = vec![0x04, down as u8, 0, 0];
                    m.extend_from_slice(&keysym.to_be_bytes());
                    ctx.control.send(&m)?;
                    if down {
                        ctx.shared.lock().unwrap().input_at = Some(Instant::now());
                    }
                }
                Ok(Input::UseVirtualDisplay { width, height }) => {
                    // The same 0x1d message as at connect; mid-session the Mac adds the
                    // display, re-sends its layout and restarts the encoder on it.
                    log::info!("HP: switching to a {width}x{height} virtual display");
                    {
                        let mut sh = ctx.shared.lock().unwrap();
                        sh.selected = None;
                        sh.switching = None;
                    }
                    let count = if ctx2.is_some() { 2 } else { 1 };
                    ctx.dual.store(count == 2, Ordering::SeqCst);
                    ctx.control.send(&negotiate::virtual_display(width, height, count))?;
                }
                Ok(Input::PointerTarget(t)) => {
                    pointer_on = match (t, &ctx2) {
                        (1, Some(c2)) => c2,
                        _ => &ctx,
                    };
                }
                Ok(Input::ShowDisplay(display)) => {
                    // Over HP the Mac streams one display at a time; "all displays"
                    // (combine flag) falls back to the main display.
                    log::info!("HP: asking the Mac to show {}", display.map_or("all displays".to_string(), |d| format!("display {d:#x}")));
                    {
                        let mut sh = ctx.shared.lock().unwrap();
                        let current = sh.switching.map(|s| s.0).or(sh.selected);
                        if display.is_some() && display == current {
                            continue;
                        }
                        sh.switching = display.map(|d| (d, Instant::now()));
                        if display.is_none() {
                            sh.selected = None;
                        }
                    }
                    ctx.control.send(&negotiate::set_display(display))?;
                    // Like a layout change: re-arm the sender and refresh the picture.
                    ctx.control.send(&fbu_request(false, 0xffff, 0xffff))?;
                    let mut sh = ctx.shared.lock().unwrap();
                    ctx.media.keyframe(&mut sh, true);
                }
                Ok(Input::SendFiles(paths)) => {
                    log::info!("HP file copy: {} item(s) to the Mac", paths.len());
                    let c = ctx.clone();
                    thread::Builder::new().name("hp-filecopy".into()).spawn(move || {
                        let mut send = |m: &[u8]| c.control.send(m);
                        filecopy::send_paths(&paths, filecopy::DEFAULT_DEST, &c.control.transfers, &mut send, &|p| drop(c.events.send(Event::FileCopy(p))), &|| c.halted())
                    })?;
                }
                Ok(Input::GetFile { paths, dest }) => {
                    log::info!("HP file copy: fetching a file from the Mac");
                    let c = ctx.clone();
                    thread::Builder::new().name("hp-filecopy".into()).spawn(move || {
                        let mut send = |m: &[u8]| c.control.send(m);
                        filecopy::get_path(&paths, &dest, &c.control.transfers, &mut send, &|p| drop(c.events.send(Event::FileCopy(p))), &|| c.halted())
                    })?;
                }
                Ok(Input::Clipboard(text)) => {
                    log::info!("HP clipboard: {} characters to the Mac", text.chars().count());
                    for rec in clipboard::send(&text) {
                        ctx.control.send(&rec)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if Instant::now() >= next {
                tick(&ctx, &mut hb, n);
                if let Some(c2) = &ctx2 {
                    tick_stream(c2, n);
                }
                n += 1;
                next += TICK;
                if next < Instant::now() {
                    next = Instant::now() + TICK;
                }
            }
        }
    })();
    ctx.halt.store(true, Ordering::SeqCst);
    let _ = socket2::SockRef::from(&stream).set_linger(Some(Duration::ZERO));
    let _ = stream.shutdown(Shutdown::Both);
    for t in threads {
        let _ = t.join();
    }
    match result {
        Err(_) if cancelled() => Ok(()),
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_swap_right_and_middle() {
        assert_eq!(apple_buttons(1), 1);
        assert_eq!(apple_buttons(2), 4); // RFB middle -> Apple middle bit
        assert_eq!(apple_buttons(4), 2); // RFB right -> Apple right bit
        assert_eq!(apple_buttons(8 | 4), 8 | 2);
    }

    #[test]
    fn pointer_maps_to_logical_points() {
        let s = |x, y, w, h| Screen { x, y, w, h };
        assert_eq!(to_logical((3840, 2160), s(0, 0, 1920, 1080), 3839, 2159), Some((1919, 1079)));
        assert_eq!(to_logical((1920, 1080), s(0, 0, 0, 0), 100, 50), Some((100, 50)));
        assert_eq!(to_logical((1920, 1080), s(1440, 0, 1920, 1080), 0, 0), Some((1440, 0)));
        assert_eq!(to_logical((0, 0), s(0, 0, 0, 0), 1, 1), None);
    }

    #[test]
    fn display_layout_is_parsed() {
        // The layout this MacBook Pro reports with a virtual display: its own
        // 2880x1800 (1440x900 pt) panel, then the 1920x1080 virtual display.
        let mut p = vec![0u8; 20 + 56 * 2];
        let put = |p: &mut Vec<u8>, o: usize, v: u16| p[o..o + 2].copy_from_slice(&v.to_be_bytes());
        for (o, v) in [(2, 3360), (4, 1080), (6, 4800), (8, 1800), (18, 2)] {
            put(&mut p, o, v);
        }
        // (y0, x0, y1, x1) in points at +20 and in pixels at +28.
        let entries: [([u16; 4], [u16; 4]); 2] = [([0, 0, 900, 1440], [0, 0, 1800, 2880]), ([0, 1440, 1080, 3360], [0, 2880, 1080, 4800])];
        for (i, (pt, px)) in entries.into_iter().enumerate() {
            for k in 0..4 {
                put(&mut p, 20 + 56 * i + 20 + 2 * k, pt[k]);
                put(&mut p, 20 + 56 * i + 28 + 2 * k, px[k]);
            }
        }
        let (s, b, d) = parse_layout(&p).unwrap();
        assert_eq!((s, b), ((3360, 1080), (4800, 1800)));
        let sc = |x, y, w, h| Screen { x, y, w, h };
        assert_eq!(d, vec![Display { id: 0, pixel: sc(0, 0, 2880, 1800), point: sc(0, 0, 1440, 900) }, Display { id: 0, pixel: sc(2880, 0, 1920, 1080), point: sc(1440, 0, 1920, 1080) }]);

        // A 1920x1080 picture is the virtual display: pointer at its pixel origin, one monitor.
        let layout = Layout { backing: b, displays: d.clone() };
        assert_eq!(picture_geometry((1920, 1080), Some(&layout), (1920, 1080), None, None), (sc(2880, 0, 1920, 1080), vec![], Some(0)));
        // A picture of the whole desktop maps to all backing pixels and reports both monitors.
        let (area, screens, _) = picture_geometry((2400, 900), Some(&layout), (2400, 900), None, None);
        assert_eq!(area, sc(0, 0, 4800, 1800));
        assert_eq!(screens, vec![sc(0, 0, 1440, 900), sc(1440, 0, 960, 540)]);
        // The MacBook's own Retina screen alone (no virtual display): pointer positions
        // are its backing pixels, so the picture maps 1:1 (not halved to points).
        let panel = Layout { backing: (2880, 1800), displays: vec![d[0]] };
        let (area, screens, _) = picture_geometry((2880, 1800), Some(&panel), (2880, 1800), None, None);
        assert_eq!((area, screens), (sc(0, 0, 2880, 1800), vec![]));
        assert_eq!(to_logical((2880, 1800), area, 1440, 1624), Some((1440, 1624)));
        // Before any layout: the negotiated canvas.
        assert_eq!(picture_geometry((1920, 1080), None, (1920, 1080), None, None).0, sc(0, 0, 1920, 1080));
    }

    #[test]
    fn selected_display_maps_the_whole_picture_onto_it() {
        // A 5K iMac (id 1) with a 6K display (id 3) to its right. The Mac encodes
        // into the 5120x2880 canvas it negotiated, whichever display is picked.
        let sc = |x, y, w, h| Screen { x, y, w, h };
        let layout = Layout {
            backing: (11136, 3384),
            displays: vec![
                Display { id: 1, pixel: sc(0, 0, 5120, 2880), point: sc(0, 0, 2560, 1440) },
                Display { id: 3, pixel: sc(5120, 0, 6016, 3384), point: sc(2560, 0, 3008, 1692) },
            ],
        };
        // Nothing picked: the picture's size identifies the main display.
        assert_eq!(picture_geometry((5120, 2880), Some(&layout), (5120, 2880), None, None), (sc(0, 0, 5120, 2880), vec![], Some(1)));
        // The 6K display scaled into the 5K canvas: pointer positions land on it.
        let (area, screens, showing) = picture_geometry((5120, 2880), Some(&layout), (5120, 2880), Some(3), None);
        assert_eq!((area, screens, showing), (sc(5120, 0, 6016, 3384), vec![], Some(3)));
        // The halved picture's centre is the 6K display's centre.
        assert_eq!(to_logical((2560, 1440), area, 1280, 720), Some((5120 + 3008, 1692)));
        // An unknown id falls back to the size match.
        assert_eq!(picture_geometry((5120, 2880), Some(&layout), (5120, 2880), Some(9), None).2, Some(1));
    }

    #[test]
    fn two_virtual_displays_map_one_per_stream() {
        // Two 1920x1080 virtual displays side by side (listed after nothing else when
        // the Mac's own screens are curtained): stream 0 is the left one, stream 1 the right.
        let sc = |x, y, w, h| Screen { x, y, w, h };
        let d = |id, x| Display { id, pixel: sc(x, 0, 1920, 1080), point: sc(x, 0, 1920, 1080) };
        let layout = Layout { backing: (3840, 1080), displays: vec![d(23, 0), d(24, 1920)] };
        assert_eq!(picture_geometry((1920, 1080), Some(&layout), (1920, 1080), None, Some(0)), (sc(0, 0, 1920, 1080), vec![], Some(23)));
        assert_eq!(picture_geometry((1920, 1080), Some(&layout), (1920, 1080), None, Some(1)), (sc(1920, 0, 1920, 1080), vec![], Some(24)));
        // Pointer positions in the second picture land on the second display.
        assert_eq!(to_logical((1920, 1080), sc(1920, 0, 1920, 1080), 10, 20), Some((1930, 20)));
    }

    #[test]
    fn layout_records_carry_display_ids() {
        let mut p = vec![0u8; 20 + 56];
        p[18..20].copy_from_slice(&1u16.to_be_bytes());
        p[20 + 16..20 + 20].copy_from_slice(&0x0423_1d5au32.to_be_bytes());
        for (k, v) in [0u16, 0, 1080, 1920].into_iter().enumerate() {
            p[20 + 20 + 2 * k..20 + 22 + 2 * k].copy_from_slice(&v.to_be_bytes());
            p[20 + 28 + 2 * k..20 + 30 + 2 * k].copy_from_slice(&v.to_be_bytes());
        }
        let (_, _, d) = parse_layout(&p).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id, 0x0423_1d5a);
    }

    #[test]
    fn startup_snapshot_finds_the_avc_configuration() {
        let key = [4u8; SRTP_BLOB_LEN];
        let mut tx = SrtpTx::new(&key, 0xabc);
        let mut cfg = vec![rtp::CONFIG_MARKER];
        cfg.extend_from_slice(b"avc1....avcC");
        cfg.extend_from_slice(&[1, 100, 0, 40, 0xff, 0xe1, 0, 2, 0x67, 1, 1, 0, 2, 0x68, 2]);
        let pkts = vec![tx.encrypt(&cfg, 100)];
        let s = parse_snapshot(&pkts, &key);
        assert!(s.is_ok(), "config alone is enough to start");
        let s = s.ok().unwrap();
        assert_eq!((s.ssrc, s.codec, s.params), (0xabc, Codec::Avc, vec![vec![0x67, 1], vec![0x68, 2]]));
        assert!(matches!(parse_snapshot(&[], &key), Err(Starved { packets: 0 })));
    }

    #[test]
    fn startup_snapshot_finds_hevc_parameter_sets() {
        let key = [4u8; SRTP_BLOB_LEN];
        let mut tx = SrtpTx::new(&key, 0xabc);
        let mut ap = vec![0x60, 0x01];
        for nal in [&[0x40u8, 0x01, 0xaa][..], &[0x42, 0x01, 0xbb], &[0x44, 0x01, 0xcc]] {
            ap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            ap.extend_from_slice(nal);
        }
        let s = parse_snapshot(&[tx.encrypt(&ap, 100)], &key).ok().expect("parameter sets alone are enough to start");
        assert_eq!(s.codec, Codec::Hevc);
        assert_eq!(s.params, vec![vec![0x40, 0x01, 0xaa], vec![0x42, 0x01, 0xbb], vec![0x44, 0x01, 0xcc]]);
        // Fragments without the parameter sets: HEVC, but not startable.
        let frag = tx.encrypt(&[0x62, 0x01, 0x80 | 1, 7], 100);
        assert!(matches!(parse_snapshot(&[frag], &key), Err(Starved { packets: 1 })));
    }
}
