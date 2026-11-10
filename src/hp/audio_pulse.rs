//! PulseAudio output and command-line player fallback for Unix desktops.
use super::{LATENCY_MS, RATE};
use libloading::Library;
use std::ffi::c_void;
use std::io::Write;
use std::os::raw::{c_char, c_int};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::AtomicBool;

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
pub(super) struct Pulse {
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

pub(super) enum Output {
    Pulse(Pulse),
    Player(Child, ChildStdin),
}

impl Output {
    pub(super) fn open() -> Option<Self> {
        match Pulse::open() {
            Ok(p) => {
                log::info!("HP audio: playing through libpulse-simple");
                return Some(Output::Pulse(p));
            }
            Err(e) => log::info!("HP audio: libpulse-simple unavailable ({e}); trying a player command"),
        }
        spawn_player().map(|(c, w)| Output::Player(c, w))
    }

    pub(super) fn write(&mut self, pcm: &[u8], _stop: &AtomicBool) -> bool {
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
