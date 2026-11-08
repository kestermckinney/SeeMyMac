//! Remote audio. The Mac sends AAC-ELD-SBR (48 kHz stereo), one raw access unit
//! per SRTP packet of payload type 101 on the control port; a 4-byte unit is
//! silence; each decodes to 480 frames (10 ms). Units are decoded with `fdk-aac-rs`, a pure-Rust port of fdk-aac
//! compiled into SeeMyMac (distributions' `fdk-aac-free` builds lack AAC-ELD, so
//! the system library cannot be relied on). The PCM goes to the sound server
//! through libpulse-simple, loaded at run time (PulseAudio or PipeWire's pulse
//! server, including inside a Flatpak with `--socket=pulseaudio`), or failing
//! that through `paplay`, `pw-play` or `aplay`.
//!
//! Audio is best-effort: without a sound server the session carries on silently.

use fdk_aac_rs::decoder::{DecodeFlags, Decoder};
use libloading::Library;
use std::ffi::c_void;
use std::io::Write;
use std::os::raw::{c_char, c_int};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

pub const PAYLOAD_TYPE: u8 = 101;
const RATE: u32 = 48_000;
/// MPEG-4 AudioSpecificConfig: AAC-ELD, SBR, 48 kHz, stereo, 480-sample frames.
const ASC: [u8; 6] = [0xf8, 0xe6, 0x51, 0x32, 0xe0, 0x00];
const MAX_SAMPLES: usize = 8192;
/// Units queued ahead of the decoder (10 ms each); beyond this new audio is dropped.
const QUEUE: usize = 8;
/// Target playback buffer.
const LATENCY_MS: u32 = 60;

struct Aac {
    dec: Decoder,
    out: Vec<i16>,
}

impl Aac {
    fn open() -> Result<Self, String> {
        let dec = Decoder::with_config(&ASC).map_err(|e| format!("AAC-ELD decoder: {e:?}"))?;
        Ok(Self { dec, out: vec![0; MAX_SAMPLES] })
    }

    /// Decode one access unit to interleaved stereo 48 kHz s16, if it yields any.
    fn decode(&mut self, au: &[u8], pcm: &mut Vec<i16>) -> bool {
        if self.dec.fill(au).is_err() {
            return false;
        }
        let Ok(frame) = self.dec.decode_frame(&mut self.out, DecodeFlags::default()) else { return false };
        let s = frame.samples();
        let info = self.dec.stream_info();
        let (rate, ch) = (info.sample_rate(), info.num_channels().max(1) as usize);
        if s.is_empty() {
            return false;
        }
        // Repeat samples up to 48 kHz should the core (non-SBR) rate be reported.
        let rep = if rate > 0 && (rate as u32) < RATE { (RATE / rate as u32).max(1) as usize } else { 1 };
        for f in s.chunks(ch) {
            let (l, r) = (f[0], if ch > 1 { f[1] } else { f[0] });
            for _ in 0..rep {
                pcm.push(l);
                pcm.push(r);
            }
        }
        true
    }
}

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

/// A libpulse-simple playback stream.
struct Pulse {
    lib: Library,
    s: *mut c_void,
}

// The stream is used by the audio thread only.
unsafe impl Send for Pulse {}

impl Pulse {
    fn open() -> Result<Self, String> {
        let lib = unsafe { Library::new("libpulse-simple.so.0") }.map_err(|e| e.to_string())?;
        unsafe {
            type New = unsafe extern "C" fn(*const c_char, *const c_char, c_int, *const c_char, *const c_char, *const SampleSpec, *const c_void, *const BufferAttr, *mut c_int) -> *mut c_void;
            let new: libloading::Symbol<New> = lib.get(b"pa_simple_new\0").map_err(|e| e.to_string())?;
            let ss = SampleSpec { format: 3, rate: RATE, channels: 2 }; // PA_SAMPLE_S16LE
            let tl = RATE * 4 * LATENCY_MS / 1000;
            let attr = BufferAttr { maxlength: u32::MAX, tlength: tl, prebuf: u32::MAX, minreq: u32::MAX, fragsize: u32::MAX };
            let mut err = 0;
            let s = new(std::ptr::null(), c"SeeMyMac".as_ptr(), 1, std::ptr::null(), c"Remote audio".as_ptr(), &ss, std::ptr::null(), &attr, &mut err);
            if s.is_null() {
                return Err(format!("no PulseAudio/PipeWire server (error {err})"));
            }
            Ok(Self { lib, s })
        }
    }

    fn write(&mut self, pcm: &[u8]) -> bool {
        unsafe {
            let Ok(w) = self.lib.get::<unsafe extern "C" fn(*mut c_void, *const c_void, usize, *mut c_int) -> c_int>(b"pa_simple_write\0") else { return false };
            let mut err = 0;
            w(self.s, pcm.as_ptr().cast(), pcm.len(), &mut err) == 0
        }
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        unsafe {
            if let Ok(free) = self.lib.get::<unsafe extern "C" fn(*mut c_void)>(b"pa_simple_free\0") {
                free(self.s);
            }
        }
    }
}

enum Output {
    Pulse(Pulse),
    Player(Child, ChildStdin),
}

impl Output {
    fn open() -> Option<Self> {
        match Pulse::open() {
            Ok(p) => {
                log::info!("HP audio: playing through libpulse-simple");
                return Some(Output::Pulse(p));
            }
            Err(e) => log::info!("HP audio: libpulse-simple unavailable ({e}); trying a player command"),
        }
        spawn_player().map(|(c, w)| Output::Player(c, w))
    }

    fn write(&mut self, pcm: &[u8]) -> bool {
        match self {
            Output::Pulse(p) => p.write(pcm),
            Output::Player(_, w) => w.write_all(pcm).is_ok(),
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Output::Player(c, _) = self {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn spawn_player() -> Option<(Child, ChildStdin)> {
    let rate = RATE.to_string();
    let cmds: [(&str, Vec<&str>); 3] = [
        ("paplay", vec!["--raw", "--format=s16le", "--rate", &rate, "--channels=2", "--latency-msec=60", "--client-name=SeeMyMac", "--stream-name=Remote audio"]),
        ("pw-play", vec!["--format=s16", "--rate", &rate, "--channels=2", "--latency=60ms", "-"]),
        ("aplay", vec!["-q", "-t", "raw", "-f", "S16_LE", "-r", &rate, "-c", "2", "--buffer-time=100000"]),
    ];
    for (prog, args) in cmds {
        if let Ok(mut c) = Command::new(prog).args(&args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            if let Some(w) = c.stdin.take() {
                log::info!("HP audio: playing through {prog}");
                return Some((c, w));
            }
        }
    }
    None
}

/// Feeds decrypted audio units to the decoder thread; dropped on session end.
pub struct Audio {
    tx: SyncSender<Vec<u8>>,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Audio {
    /// None when audio is switched off or unavailable (the reason is logged).
    pub fn start() -> Option<Self> {
        if std::env::var_os("SEEMYMAC_NO_AUDIO").is_some() {
            return None;
        }
        let mut fdk = match Aac::open() {
            Ok(f) => f,
            Err(e) => {
                log::warn!("HP audio disabled: {e}");
                return None;
            }
        };
        let Some(mut sink) = Output::open() else {
            log::warn!("HP audio disabled: no sound server, and none of paplay, pw-play or aplay could be started");
            return None;
        };
        let (tx, rx): (_, Receiver<Vec<u8>>) = sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("hp-audio".into())
            .spawn(move || {
                let mut pcm = Vec::with_capacity(2048);
                let (mut units, mut peak) = (0u32, 0i16);
                while !s2.load(Ordering::Relaxed) {
                    let au = match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(a) => a,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    pcm.clear();
                    if !fdk.decode(&au, &mut pcm) || pcm.is_empty() {
                        continue;
                    }
                    peak = peak.max(pcm.iter().map(|s| s.saturating_abs()).max().unwrap_or(0));
                    units += 1;
                    if units == 100 {
                        let i = fdk.dec.stream_info();
                        log::debug!("HP audio: peak level {peak} over the last second; {} Hz, {} frames x {} ch", i.sample_rate(), i.frame_size(), i.num_channels());
                        (units, peak) = (0, 0);
                    }
                    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                    if !sink.write(&bytes) {
                        log::warn!("HP audio: playback stopped");
                        break;
                    }
                }
                drop(sink);
            })
            .ok()?;
        Some(Self { tx, thread: Some(thread), stop })
    }

    /// Queue one decrypted access unit; a backlog drops the new unit rather than add latency.
    pub fn push(&self, au: Vec<u8>) {
        if let Err(TrySendError::Disconnected(_)) = self.tx.try_send(au) {
            // The decoder thread is gone; nothing more to do.
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_units_decode_to_silence() {
        let mut fdk = Aac::open().expect("the bundled decoder accepts Apple's AAC-ELD configuration");
        let (mut pcm, mut decoded) = (vec![], 0);
        for _ in 0..4 {
            pcm.clear();
            if fdk.decode(&[0, 0, 0, 0], &mut pcm) {
                decoded += 1;
                assert_eq!(pcm.len(), 480 * 2, "10 ms of 48 kHz stereo per unit");
            }
            assert!(pcm.iter().all(|s| s.abs() < 8), "silence stays silent");
        }
        assert!(decoded > 0);
    }
}
