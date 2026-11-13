//! TCP handshake for Apple High Performance: from connect to the SRTP keys.
//!
//! 1. Version banner (RFB 003.889) and security types.
//! 2. SRP authentication, falling back to the older RSA1+AES variant.
//! 3. ClientInit/ServerInit, then the optional SessionSelect prompt (another user
//!    is logged in at the Mac; ask them to share their session).
//! 4. Plaintext ViewerInfo, display configuration and SetEncodings.
//! 5. The server's encoding-1103 rectangle → enc1103 keys; PostEncryptionToggle.
//! 6. Encrypted SetEncodings and the 0x1c media offer carrying our SRTP keys.
//! 7. The answer gives the encoded canvas size.

use super::auth::{self, AuthError};
use super::crypto::{Enc1103Keys, Enc1103Rx, Enc1103Tx, SRTP_BLOB_LEN};
use super::net::Tcp;
use super::offers::{self, Offers};
use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: &[u8] = b"RFB 003.889\n";

pub const HP_ENCODINGS: [i32; 13] = [1010, 1011, 1002, 6, 16, 1104, 1100, -223, 1101, 1105, 1107, 1109, 1110];

pub fn set_encodings() -> Vec<u8> {
    let mut m = vec![0x02, 0];
    m.extend_from_slice(&(HP_ENCODINGS.len() as u16).to_be_bytes());
    for e in HP_ENCODINGS {
        m.extend_from_slice(&e.to_be_bytes());
    }
    m
}

/// 0x21 ViewerInfo: protocol and OS versions, and the command mask whose bits turn
/// on the Mac's optional pushes (status heartbeats and pasteboard changes among them).
pub fn viewer_info_message() -> Vec<u8> {
    let mut m = vec![0x21, 0];
    m.extend_from_slice(&0x3eu16.to_be_bytes());
    m.extend_from_slice(&1u16.to_be_bytes());
    for v in [2u32, 6, 1, 0, 15, 3, 0] {
        m.extend_from_slice(&v.to_be_bytes());
    }
    let mut mask = [0u8; 32];
    mask[0] = 0xb0;
    mask[2] = 0x0c;
    mask[3] = 0x03;
    mask[4] = 0x90;
    mask[10] = 0x40;
    m.extend_from_slice(&mask);
    m
}

fn viewer_info() -> Vec<u8> {
    let mut m = viewer_info_message();
    // Apple's plaintext 0x12 follow-up; all-zero values fail the handshake.
    m.extend_from_slice(&[0x12, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 1]);
    m
}

/// 0x1d SetDisplayConfiguration: ask for `count` (1 or 2) virtual displays of
/// `w`x`h` points at 1x. With two, the Mac places them side by side and streams
/// the second over its own video stream (see `media_message`).
pub fn virtual_display(w: u32, h: u32, count: usize) -> Vec<u8> {
    const MODES: [(f64, f64); 5] = [(1920.0, 1080.0), (1440.0, 900.0), (1920.0, 1080.0), (1440.0, 810.0), (1312.0, 848.0)];
    let n = MODES.len();
    let size = 0x9c + 28 * n;
    let mut d = vec![0u8; size];
    let put16 = |d: &mut Vec<u8>, o: usize, v: u16| d[o..o + 2].copy_from_slice(&v.to_be_bytes());
    let put32 = |d: &mut Vec<u8>, o: usize, v: u32| d[o..o + 4].copy_from_slice(&v.to_be_bytes());
    put16(&mut d, 0, size as u16);
    let name = b"SeeMyMac Virtual Display";
    d[2..2 + name.len()].copy_from_slice(name);
    put32(&mut d, 0x7a, 1); // dynamic resolution
    put32(&mut d, 0x7e, 4); // virtual display
    d[0x82..0x86].copy_from_slice(&369.454_56_f32.to_be_bytes());
    d[0x86..0x8a].copy_from_slice(&207.818_17_f32.to_be_bytes());
    put32(&mut d, 0x8a, 3840);
    put32(&mut d, 0x8e, 2160);
    put32(&mut d, 0x96, 7);
    put16(&mut d, 0x9a, n as u16);
    let (sx, sy) = (w as f64 / 1920.0, h as f64 / 1080.0);
    for (i, (bw, bh)) in MODES.iter().enumerate() {
        let (mw, mh) = ((bw * sx + 0.5) as u32, (bh * sy + 0.5) as u32);
        let o = 0x9c + 28 * i;
        for (k, v) in [mw, mh, mw, mh].into_iter().enumerate() {
            put32(&mut d, o + 4 * k, v);
        }
        d[o + 0x10..o + 0x18].copy_from_slice(&60.0f64.to_be_bytes());
    }
    let count = count.clamp(1, 2);
    let mut m = vec![0x1d, 0];
    m.extend_from_slice(&((8 + size * count) as u16).to_be_bytes());
    m.extend_from_slice(&1u16.to_be_bytes());
    m.extend_from_slice(&(count as u16).to_be_bytes());
    m.extend_from_slice(&0u32.to_be_bytes());
    for i in 0..count {
        let mut di = d.clone();
        if i > 0 {
            let name = format!("SeeMyMac Virtual Display {}", i + 1);
            di[2..2 + name.len()].copy_from_slice(name.as_bytes());
        }
        m.extend_from_slice(&di);
    }
    m
}

/// 0x0d SetDisplayMessage: stream one display (`Some(id)`) or all of them
/// combined into a single picture (`None`).
pub fn set_display(display: Option<u32>) -> Vec<u8> {
    let mut m = vec![0x0d, display.is_none() as u8, 0, 0];
    m.extend_from_slice(&display.unwrap_or(0).to_be_bytes());
    m
}

#[cfg(test)]
mod display_tests {
    #[test]
    fn set_display_message_layout() {
        assert_eq!(super::set_display(Some(3)), vec![0x0d, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(super::set_display(None), vec![0x0d, 1, 0, 0, 0, 0, 0, 0]);
    }
}

pub fn fbu_request(incremental: bool, w: u16, h: u16) -> Vec<u8> {
    let mut m = vec![0x03, incremental as u8, 0, 0, 0, 0];
    m.extend_from_slice(&w.to_be_bytes());
    m.extend_from_slice(&h.to_be_bytes());
    m
}

pub struct Keys {
    pub audio_v: [u8; SRTP_BLOB_LEN],
    pub audio_s: [u8; SRTP_BLOB_LEN],
    pub video_v: [u8; SRTP_BLOB_LEN],
    pub video_s: [u8; SRTP_BLOB_LEN],
}

impl Keys {
    fn random() -> Self {
        let mut r = rand::thread_rng();
        let mut k = || {
            let mut b = [0u8; SRTP_BLOB_LEN];
            r.fill_bytes(&mut b);
            b
        };
        Self { audio_v: k(), audio_s: k(), video_v: k(), video_s: k() }
    }
}

/// 0x1c MediaStreamConfiguration carrying both offers and the SRTP master keys.
pub fn media_message(o: &Offers, k: &Keys) -> Vec<u8> {
    let (a, v) = (o.audio.len(), o.video.len());
    let ms = a + v + 0xd8;
    let mut b = vec![0u8; ms + 4];
    b[0] = 0x1c;
    b[2..4].copy_from_slice(&(ms as u16).to_be_bytes());
    b[4..6].copy_from_slice(&3u16.to_be_bytes());
    // Bits: 60 fps stream 1, 60 fps stream 2, and "no cursor in the video" (the
    // local pointer is shown instead, so a baked-in cursor would appear twice).
    // SEEMYMAC_HP_CURSOR_IN_VIDEO=1 asks for the cursor in the video (diagnostics).
    let flags: u32 = if std::env::var_os("SEEMYMAC_HP_CURSOR_IN_VIDEO").is_some() { 3 } else { 7 };
    b[6..10].copy_from_slice(&flags.to_be_bytes());
    b[10..12].copy_from_slice(&(a as u16).to_be_bytes());
    b[12..14].copy_from_slice(&(v as u16).to_be_bytes());
    b[0x14..0x24].copy_from_slice(&offers::uuid_v4());
    b[0x24..0x52].copy_from_slice(&k.audio_v);
    b[0x52..0x80].copy_from_slice(&k.audio_s);
    b[0x80..0x80 + a].copy_from_slice(&o.audio);
    let vo = 0x80 + a;
    b[vo..vo + 0x2e].copy_from_slice(&k.video_v);
    b[vo + 0x2e..vo + 0x5c].copy_from_slice(&k.video_s);
    b[vo + 0x5c..vo + 0x5c + v].copy_from_slice(&o.video);
    if !o.video2.is_empty() {
        // A second video stream (for a second virtual display), arriving on the
        // port after the video port. It uses the same SRTP keys as the first.
        let v2 = o.video2.len();
        b[0x0e..0x10].copy_from_slice(&(v2 as u16).to_be_bytes());
        b.extend_from_slice(&k.video_v);
        b.extend_from_slice(&k.video_s);
        b.extend_from_slice(&o.video2);
        let ms = b.len() - 4;
        b[2..4].copy_from_slice(&(ms as u16).to_be_bytes());
    }
    b
}

pub struct Negotiated {
    pub tcp: Tcp,
    pub tx: Enc1103Tx,
    pub rx: Enc1103Rx,
    pub cbc_key: [u8; 16],
    pub keys: Keys,
    pub media_msg: Vec<u8>,
    pub canvas: (u32, u32),
    /// Control messages decrypted while waiting for the answer (dispatch them first).
    pub leftover: Vec<Vec<u8>>,
}

pub struct Params<'a> {
    pub addr: SocketAddrV4,
    pub username: &'a str,
    pub password: &'a str,
    pub width: u32,
    pub height: u32,
    pub virtual_display: bool,
    /// How many virtual displays to ask for (1 or 2).
    pub virtual_displays: usize,
    /// Called with every new control connection (so it can be shut down to cancel).
    pub on_socket: &'a dyn Fn(&std::net::TcpStream),
}

fn banner(t: &mut Tcp) -> Result<Vec<u8>> {
    let v = t.read_exact(12)?;
    if !v.starts_with(b"RFB ") {
        bail!("This is not a screen-sharing service (unexpected banner).");
    }
    t.send(PROTOCOL_VERSION)?;
    let n = t.read_exact(1)?[0] as usize;
    if n == 0 {
        let len = t.read_u32()? as usize;
        let why = t.read_exact(len.min(4096))?;
        bail!("The Mac refused the connection: {}", String::from_utf8_lossy(&why));
    }
    t.read_exact(n)
}

/// Apple's pre-session TCP probe: exchange banners, then hold briefly before the
/// real connection so screensharingd registers the session.
/// Connection failures are returned; a protocol hiccup is only logged.
pub fn warmup(addr: SocketAddrV4) -> Result<()> {
    let mut t = Tcp::connect(addr, Duration::from_secs(10))?;
    t.s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let r = banner(&mut t);
    t.abort();
    if let Err(e) = r {
        log::warn!("warm-up connection failed ({e:#}); continuing");
    }
    std::thread::sleep(Duration::from_millis(1400));
    Ok(())
}

fn open_and_auth(p: &Params) -> Result<(Tcp, [u8; 16])> {
    let mut last = None;
    for srp in [true, false] {
        let mut t = Tcp::connect(p.addr, Duration::from_secs(15))?;
        (p.on_socket)(&t.s);
        let types = banner(&mut t)?;
        if !types.contains(&0x21) {
            bail!("The Mac does not offer Apple authentication (security types {types:?}). Use the VNC protocol for this computer.");
        }
        let r = if srp { auth::srp(&mut t, p.username, p.password) } else { auth::nonsrp(&mut t, p.username, p.password) };
        match r {
            Ok(k) => return Ok((t, k)),
            Err(e) if e.downcast_ref::<AuthError>().is_some() => {
                log::warn!("{} authentication failed ({e}); trying the other method", if srp { "SRP" } else { "RSA1" });
                t.abort();
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    log::warn!("Mac authentication failed with both methods: {}", last.unwrap());
    Err(anyhow!("{}: the macOS user name or password is incorrect, or this user is not allowed to use Screen Sharing.", crate::rfb::LOGIN_REJECTED))
}

fn client_init(t: &mut Tcp) -> Result<(u32, u32)> {
    t.send(&[0xc1])?;
    let head = t.read_exact(24)?;
    let n = u32::from_be_bytes(head[20..24].try_into().unwrap()) as usize;
    if n > 0 {
        t.read_exact(n.min(1 << 16))?;
    }
    Ok((u16::from_be_bytes([head[0], head[1]]) as u32, u16::from_be_bytes([head[2], head[3]]) as u32))
}

const SS_MAGIC: [u8; 4] = [0x00, 0x4a, 0x00, 0x01];

/// Someone else is logged in at the Mac: ask them to share their session (cmd 0).
fn session_select(t: &mut Tcp, stage: &dyn Fn(String)) -> Result<()> {
    let prompt = t.read_exact(76)?;
    let flags = u32::from_be_bytes(prompt[4..8].try_into().unwrap());
    let user_end = prompt[0xc..].iter().position(|&b| b == 0).map_or(prompt.len(), |i| 0xc + i);
    let console = String::from_utf8_lossy(&prompt[0xc..user_end]).to_string();
    if flags & 1 == 0 {
        log::warn!("session select: share-console not offered (flags {flags:#x})");
    }
    let mut body = vec![0, 2, 0, 0, 0, 0, 0, 0];
    body.extend_from_slice(console.as_bytes());
    body.extend_from_slice(&[0, 0]);
    body.resize(72, 0);
    let mut m = (body.len() as u16).to_be_bytes().to_vec();
    m.extend_from_slice(&body);
    t.send(&m)?;
    let who = if console.is_empty() { "the user logged in on the Mac".to_string() } else { format!("{console} on the Mac") };
    stage(format!("Waiting for {who} to click Allow…"));
    match t.recv_some(Duration::from_secs(60))? {
        Some(_) => Ok(()),
        None => bail!("Nobody accepted the screen-sharing request on the Mac within 60 seconds."),
    }
}

fn read_until_1103(t: &mut Tcp, ecb_key: &[u8; 16]) -> Result<Enc1103Keys> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let what = "the encrypted control channel";
    loop {
        let ty = t.read_by(1, deadline, what)?[0];
        if ty == 0x14 {
            t.read_by(7, deadline, what)?;
            continue;
        }
        if ty != 0 {
            bail!("Unexpected message {ty:#x} before encryption was set up.");
        }
        let h = t.read_by(3, deadline, what)?;
        let count = u16::from_be_bytes([h[1], h[2]]);
        let mut keys = None;
        for _ in 0..count {
            let r = t.read_by(12, deadline, what)?;
            match i32::from_be_bytes(r[8..12].try_into().unwrap()) {
                1103 => {
                    let b: [u8; 36] = t.read_by(36, deadline, what)?.try_into().unwrap();
                    keys = Some(Enc1103Keys::unwrap(&b, ecb_key));
                }
                1010 | 1011 => {
                    let n = t.read_by(2, deadline, what)?;
                    t.read_by(u16::from_be_bytes([n[0], n[1]]) as usize, deadline, what)?;
                }
                e => bail!("Unexpected encoding {e} before encryption was set up."),
            }
        }
        if let Some(k) = keys {
            return Ok(k);
        }
    }
}

fn read_answer(t: &mut Tcp, rx: &mut Enc1103Rx, leftover: &mut Vec<Vec<u8>>) -> Result<(u32, u32)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Some(chunk) = t.recv_some(deadline.saturating_duration_since(Instant::now()))? else { continue };
        let mut found = None;
        for m in rx.feed(&chunk) {
            let (w, h, _) = offers::canvas_dims(&m);
            if w > 0 && h > 0 && found.is_none() {
                found = Some((w, h));
            } else {
                leftover.push(m);
            }
        }
        if let Some(c) = found {
            return Ok(c);
        }
    }
    Ok((0, 0))
}

pub fn negotiate(p: &Params, offers: &Offers, stage: &dyn Fn(String)) -> Result<Negotiated> {
    stage("Authenticating with the Mac…".into());
    // screensharingd sometimes stalls a login right after a previous session ended
    // or while the Mac wakes; a fresh connection then goes through.
    let mut attempt = 0;
    let (mut t, ecb_key) = loop {
        match open_and_auth(p) {
            Err(e) if e.downcast_ref::<super::net::Stalled>().is_some() && attempt < 2 => {
                attempt += 1;
                log::warn!("HP: login stalled; reconnecting ({attempt}/2)");
                stage("The Mac is slow to respond; trying again…".into());
                std::thread::sleep(Duration::from_secs(1));
            }
            r => break r?,
        }
    };
    let (sw, sh) = client_init(&mut t)?;
    log::info!("ServerInit {sw}x{sh}");
    if t.peek_prefix(&SS_MAGIC, Duration::from_secs(2))? {
        session_select(&mut t, stage)?;
    }

    stage("Configuring the remote display…".into());
    t.send(&viewer_info())?;
    std::thread::sleep(Duration::from_millis(100));
    if p.virtual_display {
        t.send(&virtual_display(p.width, p.height, p.virtual_displays))?;
    }
    t.send(&set_encodings())?;

    stage("Setting up encryption…".into());
    let k = read_until_1103(&mut t, &ecb_key)?;
    t.send(&[0x12, 0, 0, 2, 0, 1, 0, 0])?; // PostEncryptionToggle
    std::thread::sleep(Duration::from_millis(200));
    let (mut tx, mut rx) = (k.sender(), k.receiver());

    stage("Negotiating Apple High Performance video…".into());
    t.send(&tx.seal(&set_encodings()))?;
    let keys = Keys::random();
    let msg = media_message(offers, &keys);
    t.send(&tx.seal(&msg))?;
    // Wakes the control-channel pseudo-encoding sender (cursor, display layout).
    t.send(&tx.seal(&fbu_request(false, 0xffff, 0xffff)))?;
    let mut leftover = vec![];
    let mut canvas = read_answer(&mut t, &mut rx, &mut leftover)?;
    // The encoder may still be warming up (e.g. right after a login): re-ask.
    for attempt in 0..16 {
        if canvas.0 > 0 && canvas.1 > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        log::info!("media answer had no canvas; re-asking ({})", attempt + 1);
        stage(format!("Waiting for the Mac's video encoder (attempt {} of 16)…", attempt + 2));
        t.send(&tx.seal(&msg)).context("re-sending the media offer")?;
        canvas = read_answer(&mut t, &mut rx, &mut leftover)?;
    }
    log::info!("HP canvas {}x{}", canvas.0, canvas.1);
    Ok(Negotiated { tcp: t, tx, rx, cbc_key: k.cbc_key, keys, media_msg: msg, canvas, leftover })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_messages_have_apple_layouts() {
        let v = viewer_info();
        assert_eq!((v.len(), &v[..4]), (66 + 12, &[0x21, 0, 0, 0x3e][..]));
        let d = virtual_display(1920, 1080, 1);
        assert_eq!(d[0], 0x1d);
        // Two displays: the same descriptor twice, the count at +6.
        let two = virtual_display(1920, 1080, 2);
        assert_eq!(u16::from_be_bytes([two[6], two[7]]), 2);
        assert_eq!(two.len() - 12, 2 * (d.len() - 12));
        assert_eq!(u16::from_be_bytes([two[2], two[3]]) as usize, two.len() - 4);
        assert_eq!(u16::from_be_bytes([d[2], d[3]]) as usize, d.len() - 4);
        // First mode is 1920x1080 at 1x.
        let o = 12 + 0x9c;
        assert_eq!(u32::from_be_bytes(d[o..o + 4].try_into().unwrap()), 1920);
        assert_eq!(u32::from_be_bytes(d[o + 8..o + 12].try_into().unwrap()), 1920);
        let s = set_encodings();
        assert_eq!(s.len(), 4 + 4 * HP_ENCODINGS.len());
    }

    #[test]
    fn media_message_places_keys_and_offers() {
        let o = offers::create(offers::CodecOffer::AvcOnly, false);
        let k = Keys::random();
        let m = media_message(&o, &k);
        let a = o.audio.len();
        assert_eq!(&m[0x24..0x52], &k.audio_v);
        assert_eq!(&m[0x80 + a + 0x2e..0x80 + a + 0x5c], &k.video_s);
        assert_eq!(&m[0x80 + a + 0x5c..], &o.video[..]);
        assert_eq!(u16::from_be_bytes([m[2], m[3]]) as usize + 4, m.len());
        assert_eq!(&m[0x0e..0x10], &[0, 0], "no second video stream");
    }

    #[test]
    fn media_message_carries_a_second_video_stream() {
        let o = offers::create(offers::CodecOffer::Both, true);
        let k = Keys::random();
        let m = media_message(&o, &k);
        let (a, v, v2) = (o.audio.len(), o.video.len(), o.video2.len());
        assert!(v2 > 0 && o.video2_ssrc != o.video_ssrc);
        assert_eq!(u16::from_be_bytes([m[0x0e], m[0x0f]]) as usize, v2);
        // Second stream after the first: its two keys, then its offer.
        let o2 = 0x80 + a + 0x5c + v;
        assert_eq!(&m[o2 + 0x2e..o2 + 0x5c], &k.video_s);
        assert_eq!(&m[o2 + 0x5c..], &o.video2[..]);
        assert_eq!(u16::from_be_bytes([m[2], m[3]]) as usize + 4, m.len());
    }
}
