//! Apple High Performance Screen Sharing, implemented natively in Rust.
//!
//! macOS 14+ on Apple silicon streams H.264/HEVC video over SRTP/UDP after an
//! RFB-based TCP handshake with Apple-private messages. This is a port of the
//! protocol engine from iShareScreen (AGPL-3.0-or-later,
//! <https://github.com/renegadelink/iShareScreen>, as pinned in SeeMac), limited to
//! what SeeMyMac needs: one H.264 picture per frame, no audio, no clipboard.
//!
//! Layout:
//! * [`net`] / [`negotiate`] / [`auth`]: the TCP handshake and authentication.
//! * [`crypto`]: enc1103 control-channel records and SRTP/SRTCP.
//! * [`offers`]: the AVConference media offer and answer.
//! * [`rtp`] / [`decode`]: depacketization, loss repair and H.264 decoding (`rust_h264`).
//! * `session`: the running connection's threads.

pub mod auth;
pub mod crypto;
pub mod decode;
pub mod negotiate;
pub mod net;
pub mod offers;
pub mod rtp;
mod session;

use crate::rfb::{Closer, Event, Framebuffer, Handle, Input, Screen};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use session::{apple_buttons, to_logical};

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// Size (in points) advertised to the Mac for the virtual display.
    pub width: u32,
    pub height: u32,
    /// Ask the Mac for a virtual display. This blanks the Mac's own screens while
    /// connected; without it the Mac streams its physical display.
    pub virtual_display: bool,
    /// A built-in synthetic picture: no Mac, no network.
    pub demo: bool,
}

/// Start an Apple High Performance session on a background thread.
pub fn connect(cfg: Config) -> Handle {
    let (ev_tx, ev_rx) = channel();
    let (in_tx, in_rx) = channel();
    let fb = Arc::new(Mutex::new(Framebuffer::default()));
    let closer: Closer = Arc::new(Mutex::new(None));
    let handle = Handle::from_parts(ev_rx, fb.clone(), in_tx, closer.clone());
    std::thread::Builder::new()
        .name(format!("hp-{}", cfg.host))
        .spawn(move || {
            let stopping = Arc::new(AtomicBool::new(false));
            {
                let st = stopping.clone();
                *closer.lock().unwrap() = Some(Box::new(move || st.store(true, Ordering::SeqCst)));
            }
            let result = if cfg.demo { demo(&cfg, &fb, &stopping, in_rx, &ev_tx) } else { session::run(&cfg, &fb, &closer, &stopping, in_rx, &ev_tx) };
            let reason = match result {
                Err(e) if !stopping.load(Ordering::SeqCst) => {
                    log::warn!("{}: Apple High Performance session failed: {e:#}", cfg.host);
                    Some(format!("{e:#}"))
                }
                Ok(()) if !stopping.load(Ordering::SeqCst) => Some("The remote session ended.".to_string()),
                _ => None,
            };
            let _ = ev_tx.send(Event::Disconnected(reason));
        })
        .expect("spawn HP thread");
    handle
}

/// Check that the bundled H.264 decoder works here (used by Settings).
pub fn self_test() -> String {
    decode::self_test().unwrap_or_else(|e| format!("{e:#}"))
}

/// A moving test pattern at 60 fps, for UI tests and demos.
fn demo(cfg: &Config, fb: &Arc<Mutex<Framebuffer>>, stopping: &AtomicBool, input: Receiver<Input>, events: &Sender<Event>) -> anyhow::Result<()> {
    let (w, h) = (cfg.width.max(64), cfg.height.max(64));
    let two = std::env::var("SEEMYMAC_DEMO_DISPLAYS").as_deref() == Ok("2");
    let _ = events.send(Event::Stage("Starting the synthetic Apple High Performance source…".into()));
    let mut base = vec![0u8; (w * h * 4) as usize];
    for (i, px) in base.chunks_exact_mut(4).enumerate() {
        let x = (i as u32) % w;
        px.copy_from_slice(&[110, (((x / 64) % 2) * 170 + 30) as u8, (x % 256) as u8, 255]);
    }
    let mut n = 0u32;
    let mut next = Instant::now();
    while !stopping.load(Ordering::SeqCst) {
        match input.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if Instant::now() < next {
            continue;
        }
        next += Duration::from_millis(16);
        let mut img = base.clone();
        let left = (n * 13) % (w - 100).max(1);
        for y in h / 3..(h / 3 + 100.min(h / 3)) {
            for x in left..(left + 100).min(w) {
                let o = ((y * w + x) * 4) as usize;
                img[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        {
            let mut f = fb.lock().unwrap();
            f.width = w;
            f.height = h;
            f.bgra = img;
            f.logical = (0, 0);
            f.name = "Synthetic Mac".into();
            f.screens = if two { vec![Screen { x: 0, y: 0, w: w / 2, h }, Screen { x: w / 2, y: 0, w: w - w / 2, h }] } else { vec![] };
            f.version += 1;
        }
        if n == 0 {
            let _ = events.send(Event::Connected { width: w, height: h, name: "Synthetic Mac".into() });
        }
        n += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_session_paints_frames_and_stops() {
        let h = connect(Config { host: "demo".into(), port: 5900, username: String::new(), password: String::new(), width: 320, height: 200, virtual_display: true, demo: true });
        let end = Instant::now() + Duration::from_secs(5);
        let mut connected = false;
        while Instant::now() < end && !connected {
            if let Ok(Event::Connected { width, height, .. }) = h.events.recv_timeout(Duration::from_millis(100)) {
                assert_eq!((width, height), (320, 200));
                connected = true;
            }
        }
        assert!(connected);
        assert_eq!(h.fb.lock().unwrap().bgra.len(), 320 * 200 * 4);
        h.close();
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            match h.events.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Disconnected(r)) => {
                    assert_eq!(r, None, "a requested close is not an error");
                    break;
                }
                _ if Instant::now() > end => panic!("demo did not stop"),
                _ => {}
            }
        }
    }
}
