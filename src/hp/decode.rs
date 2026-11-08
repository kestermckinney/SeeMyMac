//! Video decoding, pure Rust with no system codecs needed: H.264 with
//! `rust_h264` ([`AvcDecoder`]) and HEVC with `rust_h265` ([`HevcDecoder`]).
//! The Mac picks the codec from our offer; it chooses HEVC (4:4:4, Range
//! Extensions profile) whenever it can, and it must for screens larger than
//! H.264 allows, such as a 5K iMac. [`VideoDecoder`] wraps whichever is in use.
//!
//! H.264 notes:
//!
//! Each Apple RTP timestamp group is one slice = one access unit, so every slice
//! is dispatched as a complete picture and released as soon as it is decoded
//! (no one-frame delay). Pictures decode on several cores at once (frame
//! threading): a 2880x1800 frame takes ~50 ms on one core, too slow for the
//! Mac's 30-60 fps, so a single-threaded decoder falls ever further behind. After a
//! restart or a broken reference chain, inter slices are dropped until the next
//! keyframe re-roots the decoder.
//!
//! Apple's encoder truncates slices. A P slice stops after its last coded
//! macroblock, leaving the trailing skipped ones (and sometimes the final bytes
//! of the arithmetic code) out; Apple's decoder treats the uncovered macroblocks
//! as P_Skip. Intra slices can also end a few bytes early, decoding correctly
//! when the decoder reads zeros past the end. Strict decoders reject both cases
//! (OpenH264, stock rust_h264) or conceal the bottom rows (FFmpeg), which shows
//! up as a frozen picture or a green strip along the bottom. We use rust_h264
//! with a P_Skip fill (vendor/rust_h264) and zero-pad intra slices.

use super::rtp::is_keyframe;
use anyhow::Result;
use rust_h264::decoder::{Decoder, Frame};
use rust_h264::threading::ThreadedDecoder;
use rust_h264::nal::{parse_annex_b, NalUnit};

/// Zero bytes appended to intra slices (see the module docs).
const CABAC_TAIL_PAD: usize = 64;

pub struct Picture {
    /// Size of the converted picture in `bgra`.
    pub width: u32,
    pub height: u32,
    /// The decoded picture is `scale` times larger in each direction (see [`HALVE_ABOVE`]).
    pub scale: u32,
}

/// Pictures wider than this (5K and 6K screens) are halved while converting to
/// BGRA. The window shows them scaled down anyway, and a 59 MB 5K frame would cost
/// the UI a large copy and texture upload 30 times a second; half size is the
/// Mac's own point resolution (2560x1440 for a 5K iMac).
pub const HALVE_ABOVE: u32 = 4096;

/// Pictures decoding at once (each on its own thread).
fn decode_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 8)
}

/// HEVC decoding threads. A 5K 4:4:4 picture takes ~200 ms on one core and
/// the Mac sends up to 30-60 a second, so HEVC uses every core.
fn hevc_threads() -> usize {
    if let Some(n) = std::env::var("SEEMYMAC_HEVC_THREADS").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    // More pictures in flight barely raise throughput but add latency (8 cores:
    // 213 ms with 8 threads, 285 ms with 12, at the same 5K frame rate).
    std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 8)
}

pub struct AvcDecoder {
    dec: Option<ThreadedDecoder>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// Drop inter slices until a keyframe arrives.
    await_key: bool,
    /// The reference chain broke: rebuild the decoder on the next keyframe.
    reset_pending: bool,
}

fn annexb(nal: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(nal.len() + 4);
    v.extend_from_slice(&[0, 0, 0, 1]);
    v.extend_from_slice(nal);
    v
}

/// Parse one raw NAL (no start code) into the decoder's form, padding slices.
fn nal_unit(nal: &[u8], pad: usize) -> Option<NalUnit<'static>> {
    let data = annexb(nal);
    let n = parse_annex_b(&data).into_iter().next()?;
    let mut rbsp = n.rbsp.into_owned();
    rbsp.resize(rbsp.len() + pad, 0);
    Some(NalUnit { nal_ref_idc: n.nal_ref_idc, nal_unit_type: n.nal_unit_type, rbsp: rbsp.into() })
}

impl AvcDecoder {
    pub fn new(sps: Vec<u8>, pps: Vec<u8>) -> Self {
        Self { dec: None, sps, pps, await_key: true, reset_pending: false }
    }

    pub fn sps(&self) -> &[u8] {
        &self.sps
    }

    pub fn set_params(&mut self, sps: Vec<u8>, pps: Vec<u8>) {
        self.sps = sps;
        self.pps = pps;
        self.restart();
    }

    pub fn restart(&mut self) {
        self.dec = None;
        self.await_key = true;
        self.reset_pending = false;
    }

    pub fn mark_broken(&mut self) {
        self.reset_pending = true;
        self.await_key = true;
    }

    pub fn waiting_for_keyframe(&self) -> bool {
        self.await_key
    }

    /// Pictures dispatched but not yet returned by [`poll`](Self::poll).
    pub fn busy(&self) -> bool {
        self.dec.as_ref().map_or(false, |d| d.in_flight() > 0)
    }

    fn ensure<'a>(dec: &'a mut Option<ThreadedDecoder>, sps: &[u8], pps: &[u8]) -> Result<&'a mut ThreadedDecoder> {
        if dec.is_none() {
            let mut d = ThreadedDecoder::new(decode_threads());
            d.set_fill_truncated_p_slices(true);
            d.set_low_delay(true);
            for p in [sps, pps] {
                let n = nal_unit(p, 0).ok_or_else(|| anyhow::anyhow!("bad parameter set"))?;
                d.decode_nal(&n).map_err(|e| anyhow::anyhow!("H.264 parameter set rejected: {e:?}"))?;
            }
            *dec = Some(d);
        }
        Ok(dec.as_mut().unwrap())
    }

    /// Dispatch one NAL for decoding, then return the newest picture that has
    /// finished (converted into `bgra`), if any. Pictures still decoding come
    /// out of later `feed` or [`poll`](Self::poll) calls.
    /// `Err` means a picture was corrupt; the caller should request a keyframe.
    pub fn feed(&mut self, nal: &[u8], bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        let t = nal.first().map_or(0, |b| b & 0x1f);
        if !(1..=5).contains(&t) {
            return Ok(None); // parameter sets travel out of band in the avcC config
        }
        let key = is_keyframe(nal);
        if self.await_key && !key {
            return Ok(None);
        }
        if key && self.reset_pending {
            self.dec = None;
            self.reset_pending = false;
        }
        dump(&annexb(nal), &[&self.sps, &self.pps]);
        let Some(unit) = nal_unit(nal, if key { CABAC_TAIL_PAD } else { 0 }) else { return Ok(None) };
        let dec = match Self::ensure(&mut self.dec, &self.sps, &self.pps) {
            Ok(d) => d,
            Err(e) => {
                self.mark_broken();
                return Err(e);
            }
        };
        let r = dec.decode_nal(&unit);
        // Each access unit is a single slice: start decoding it now.
        dec.end_picture();
        let ready = r.and_then(|mut v| dec.poll().map(|w| {
            v.extend(w);
            v
        }));
        if ready.is_ok() {
            self.await_key = false;
        }
        self.output(ready, bgra)
    }

    /// Feed one access unit's NALs; the newest finished picture, if any.
    pub fn feed_au(&mut self, nals: &[Vec<u8>], bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        let mut newest = None;
        for nal in nals {
            if let Some(p) = self.feed(nal, bgra)? {
                newest = Some(p);
            }
        }
        Ok(newest)
    }

    /// Return the newest picture that finished since the last call, if any.
    pub fn poll(&mut self, bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        let Some(dec) = self.dec.as_mut() else { return Ok(None) };
        let ready = dec.poll();
        self.output(ready, bgra)
    }

    /// Wait for every picture in flight and return the newest (tests, replay).
    pub fn finish(&mut self, bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        let Some(dec) = self.dec.as_mut() else { return Ok(None) };
        let ready = Ok(dec.flush());
        self.output(ready, bgra)
    }

    fn output(&mut self, ready: Result<Vec<Frame>, rust_h264::error::DecodeError>, bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        match ready {
            // Only the newest picture is shown; older ones are already stale.
            Ok(frames) => Ok(frames.last().map(|f| to_bgra(f, bgra))),
            Err(e) => {
                self.mark_broken();
                Err(anyhow::anyhow!("H.264 decode error: {e:?}"))
            }
        }
    }
}

fn to_bgra(f: &Frame, bgra: &mut Vec<u8>) -> Picture {
    planes_to_bgra(&f.y, &f.u, &f.v, f.width as usize, f.height as usize, false, bgra);
    Picture { width: f.width, height: f.height, scale: 1 }
}

/// Convert planar YUV (4:2:0, or 4:4:4 when `full_chroma`) into `bgra`, in
/// horizontal bands (an even number of rows each) on several cores.
fn planes_to_bgra(y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize, full_chroma: bool, bgra: &mut Vec<u8>) {
    let cw = if full_chroma { w } else { w.div_ceil(2) };
    bgra.resize(w * h * 4, 0);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8);
    let band = ((h.div_ceil(threads) + 1) & !1).max(2);
    std::thread::scope(|s| {
        for (i, out) in bgra.chunks_mut(band * w * 4).enumerate() {
            let y0 = i * band;
            let rows = out.len() / (w * 4);
            let yb = &y[y0 * w..];
            if full_chroma {
                let (ub, vb) = (&u[y0 * cw..], &v[y0 * cw..]);
                s.spawn(move || yuv444_to_bgra(yb, ub, vb, w, w, rows, out));
            } else {
                let (ub, vb) = (&u[(y0 / 2) * cw..], &v[(y0 / 2) * cw..]);
                s.spawn(move || yuv_to_bgra(yb, ub, vb, w, cw, cw, w, rows, out));
            }
        }
    });
}

/// `SEEMYMAC_HP_DUMP=file` appends every decoded NAL (Annex B) for offline
/// analysis, after the parameter sets `params` (written once per process).
fn dump(pkt: &[u8], params: &[&[u8]]) {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    let Some(path) = std::env::var_os("SEEMYMAC_HP_DUMP") else { return };
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    if !STARTED.swap(true, Ordering::SeqCst) {
        for p in params {
            let _ = f.write_all(&annexb(p));
        }
    }
    let _ = f.write_all(pkt);
}

/// BT.709 full-range YUV 4:2:0 → BGRA (what Apple's screen stream uses).
#[allow(clippy::too_many_arguments)]
pub fn yuv_to_bgra(y: &[u8], u: &[u8], v: &[u8], ys: usize, us: usize, vs: usize, w: usize, h: usize, out: &mut [u8]) {
    // 16.16 fixed point coefficients.
    const RV: i32 = 103206; // 1.5748
    const GU: i32 = 12276; // 0.1873
    const GV: i32 = 30679; // 0.4681
    const BU: i32 = 121609; // 1.8556
    let clamp = |x: i32| (x >> 16).clamp(0, 255) as u8;
    for row in 0..h {
        let yr = &y[row * ys..row * ys + w];
        let ur = &u[(row / 2) * us..];
        let vr = &v[(row / 2) * vs..];
        let o = &mut out[row * w * 4..(row + 1) * w * 4];
        for col in 0..w {
            let yy = (yr[col] as i32) << 16;
            let cu = ur[col / 2] as i32 - 128;
            let cv = vr[col / 2] as i32 - 128;
            let px = &mut o[col * 4..col * 4 + 4];
            px[0] = clamp(yy + BU * cu + 32768);
            px[1] = clamp(yy - GU * cu - GV * cv + 32768);
            px[2] = clamp(yy + RV * cv + 32768);
            px[3] = 255;
        }
    }
}

/// Like [`planes_to_bgra`], but averaging each 2x2 block into one pixel: the
/// output is `w / 2` by `h / 2`.
fn planes_to_bgra_half(y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize, full_chroma: bool, bgra: &mut Vec<u8>) {
    let (ow, oh) = (w / 2, h / 2);
    let cw = if full_chroma { w } else { w.div_ceil(2) };
    bgra.resize(ow * oh * 4, 0);
    if ow == 0 || oh == 0 {
        return;
    }
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8);
    let band = oh.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for (i, out) in bgra.chunks_mut(band * ow * 4).enumerate() {
            s.spawn(move || {
                let mut row = vec![0u8; ow * 3];
                for (r, o) in out.chunks_exact_mut(ow * 4).enumerate() {
                    let oy = i * band + r;
                    let (y0, y1) = (2 * oy * w, (2 * oy + 1) * w);
                    let avg4 = |p: &[u8], a: usize, b: usize, x: usize| ((p[a + x] as u32 + p[a + x + 1] as u32 + p[b + x] as u32 + p[b + x + 1] as u32 + 2) / 4) as u8;
                    for ox in 0..ow {
                        let x = 2 * ox;
                        row[ox] = avg4(y, y0, y1, x);
                        if full_chroma {
                            let (c0, c1) = (2 * oy * cw, (2 * oy + 1) * cw);
                            row[ow + ox] = avg4(u, c0, c1, x);
                            row[2 * ow + ox] = avg4(v, c0, c1, x);
                        } else {
                            row[ow + ox] = u[oy * cw + ox];
                            row[2 * ow + ox] = v[oy * cw + ox];
                        }
                    }
                    let (yr, rest) = row.split_at(ow);
                    let (ur, vr) = rest.split_at(ow);
                    yuv444_to_bgra(yr, ur, vr, ow, ow, 1, o);
                }
            });
        }
    });
}

/// BT.709 full-range YUV 4:4:4 → BGRA (Apple's HEVC screen stream).
pub fn yuv444_to_bgra(y: &[u8], u: &[u8], v: &[u8], stride: usize, w: usize, h: usize, out: &mut [u8]) {
    const RV: i32 = 103206;
    const GU: i32 = 12276;
    const GV: i32 = 30679;
    const BU: i32 = 121609;
    let clamp = |x: i32| (x >> 16).clamp(0, 255) as u8;
    for row in 0..h {
        let r = row * stride..row * stride + w;
        let (yr, ur, vr) = (&y[r.clone()], &u[r.clone()], &v[r]);
        let o = &mut out[row * w * 4..(row + 1) * w * 4];
        for (((px, &yy), &cu), &cv) in o.chunks_exact_mut(4).zip(yr).zip(ur).zip(vr) {
            let yy = (yy as i32) << 16;
            let (cu, cv) = (cu as i32 - 128, cv as i32 - 128);
            px[0] = clamp(yy + BU * cu + 32768);
            px[1] = clamp(yy - GU * cu - GV * cv + 32768);
            px[2] = clamp(yy + RV * cv + 32768);
            px[3] = 255;
        }
    }
}

// ----- HEVC ----------------------------------------------------------------------

/// HEVC NAL unit type from the two-byte header.
pub fn hevc_type(nal: &[u8]) -> u8 {
    nal.first().map_or(63, |b| (b >> 1) & 0x3f)
}

/// IRAP pictures (IDR, CRA, BLA) start a decodable stream.
pub fn hevc_is_keyframe(nal: &[u8]) -> bool {
    (16..=21).contains(&hevc_type(nal))
}

/// Access unit delimiter: ends the picture before it, so a picture is shown as
/// soon as it decodes instead of when the next one starts (a still screen
/// sends nothing more).
const HEVC_AUD: [u8; 3] = [35 << 1, 1, 0x50];

pub struct HevcDecoder {
    dec: Option<rust_h265::Decoder>,
    /// The latest VPS, SPS and PPS (Apple sends them in-band before each IRAP).
    params: [Option<Vec<u8>>; 3],
    await_key: bool,
    reset_pending: bool,
    /// Pictures fed but not yet returned (an estimate: damaged pictures may never come out).
    in_flight: usize,
    /// When each picture in flight was fed, to measure decoding latency.
    fed_at: std::collections::VecDeque<std::time::Instant>,
    /// (pictures, total and worst feed-to-output latency) since the last `take_latency`.
    latency: (u32, std::time::Duration, std::time::Duration),
}

impl HevcDecoder {
    pub fn new(params: &[Vec<u8>]) -> Self {
        let mut d = Self { dec: None, params: [None, None, None], await_key: true, reset_pending: false, in_flight: 0, fed_at: Default::default(), latency: Default::default() };
        for p in params {
            d.set_param(p);
        }
        d
    }

    /// Store a VPS/SPS/PPS. A different SPS (new geometry) restarts the decoder.
    fn set_param(&mut self, nal: &[u8]) -> bool {
        let i = (hevc_type(nal) - 32) as usize;
        if self.params[i].as_deref() == Some(nal) {
            return false;
        }
        if i == 1 && self.params[1].is_some() {
            self.restart();
        }
        self.params[i] = Some(nal.to_vec());
        true
    }

    pub fn restart(&mut self) {
        self.dec = None;
        self.await_key = true;
        self.reset_pending = false;
        self.in_flight = 0;
        self.fed_at.clear();
    }

    /// Feed-to-output latency of the pictures returned since the last call: (count, mean, worst).
    pub fn take_latency(&mut self) -> (u32, std::time::Duration, std::time::Duration) {
        let (n, total, worst) = std::mem::take(&mut self.latency);
        (n, total.checked_div(n).unwrap_or_default(), worst)
    }

    pub fn mark_broken(&mut self) {
        self.reset_pending = true;
        self.await_key = true;
    }

    fn ensure(&mut self) -> Result<Option<&mut rust_h265::Decoder>> {
        if self.dec.is_none() {
            let [Some(vps), Some(sps), Some(pps)] = &self.params else { return Ok(None) };
            let mut d = rust_h265::Decoder::new().with_display_order().with_threads(hevc_threads());
            for p in [vps, sps, pps] {
                let n = rust_h265::parse_nal(p).ok_or_else(|| anyhow::anyhow!("bad HEVC parameter set"))?;
                d.decode_nal(&n).map_err(|e| anyhow::anyhow!("HEVC parameter set rejected: {e}"))?;
            }
            self.dec = Some(d);
            self.in_flight = 0;
        }
        Ok(self.dec.as_mut())
    }

    /// Feed one access unit's NALs (parameter sets, then its slices); the
    /// newest finished picture, if any. `Err` means a picture was corrupt.
    pub fn feed_au(&mut self, nals: &[Vec<u8>], bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        let mut sliced = false;
        for nal in nals {
            let t = hevc_type(nal);
            if (32..=34).contains(&t) {
                if self.set_param(nal) {
                    if let Some(d) = self.dec.as_mut() {
                        let n = rust_h265::parse_nal(nal).ok_or_else(|| anyhow::anyhow!("bad HEVC parameter set"))?;
                        d.decode_nal(&n).map_err(|e| anyhow::anyhow!("HEVC parameter set rejected: {e}"))?;
                    }
                }
                continue;
            }
            if t > 31 {
                continue; // SEI and other non-picture units
            }
            let key = hevc_is_keyframe(nal);
            if self.await_key && !key {
                continue;
            }
            if key && self.reset_pending {
                self.dec = None;
                self.reset_pending = false;
                self.in_flight = 0;
                self.fed_at.clear();
            }
            let p = &self.params;
            dump(&annexb(nal), &[p[0].as_deref().unwrap_or(&[]), p[1].as_deref().unwrap_or(&[]), p[2].as_deref().unwrap_or(&[])]);
            let Some(n) = rust_h265::parse_nal(nal) else { continue };
            let r = match self.ensure() {
                Ok(Some(d)) => d.decode_nal(&n).map_err(|e| anyhow::anyhow!("HEVC decode error: {e}")),
                Ok(None) => continue, // no parameter sets yet
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                self.mark_broken();
                return Err(e);
            }
            sliced = true;
            if key {
                self.await_key = false;
            }
        }
        if sliced {
            if let Some(d) = self.dec.as_mut() {
                let aud = rust_h265::parse_nal(&HEVC_AUD).expect("AUD");
                if let Err(e) = d.decode_nal(&aud) {
                    self.mark_broken();
                    return Err(anyhow::anyhow!("HEVC decode error: {e}"));
                }
                self.in_flight += 1;
                self.fed_at.push_back(std::time::Instant::now());
            }
        }
        self.poll(bgra)
    }

    /// Return the newest picture that finished since the last call, if any.
    pub fn poll(&mut self, bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        let Some(d) = self.dec.as_mut() else { return Ok(None) };
        if let Some(e) = d.next_error() {
            self.mark_broken();
            return Err(anyhow::anyhow!("HEVC decode error: {e}"));
        }
        let mut newest = None;
        while let Some(f) = d.next_output() {
            self.in_flight = self.in_flight.saturating_sub(1);
            if let Some(t) = self.fed_at.pop_front() {
                let l = t.elapsed();
                self.latency.0 += 1;
                self.latency.1 += l;
                self.latency.2 = self.latency.2.max(l);
            }
            newest = Some(f);
        }
        Ok(newest.map(|f| hevc_to_bgra(&f, bgra)))
    }

    /// Wait for every picture in flight and return the newest (tests, replay).
    pub fn finish(&mut self, bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        let Some(d) = self.dec.as_mut() else { return Ok(None) };
        let mut newest = None;
        while let Some(f) = d.flush() {
            newest = Some(f);
        }
        self.in_flight = 0;
        if let Some(e) = d.next_error() {
            self.mark_broken();
            return Err(anyhow::anyhow!("HEVC decode error: {e}"));
        }
        Ok(newest.map(|f| hevc_to_bgra(&f, bgra)))
    }
}

fn hevc_to_bgra(f: &rust_h265::Frame, bgra: &mut Vec<u8>) -> Picture {
    use rust_h265::PixelData;
    let full = f.chroma_format_idc == 3;
    let (w, h) = (f.width as usize, f.height as usize);
    let half = f.width > HALVE_ABOVE;
    let convert = |y: &[u8], u: &[u8], v: &[u8], bgra: &mut Vec<u8>| {
        if half {
            planes_to_bgra_half(y, u, v, w, h, full, bgra)
        } else {
            planes_to_bgra(y, u, v, w, h, full, bgra)
        }
    };
    match (&f.y, &f.u, &f.v) {
        (PixelData::U8(y), PixelData::U8(u), PixelData::U8(v)) => convert(y, u, v, bgra),
        _ => {
            // 10/12-bit: drop the extra precision.
            let shift = f.bit_depth.saturating_sub(8) as u32;
            let narrow = |p: &PixelData| match p {
                PixelData::U8(v) => v.clone(),
                PixelData::U16(v) => v.iter().map(|&s| (s >> shift).min(255) as u8).collect(),
            };
            convert(&narrow(&f.y), &narrow(&f.u), &narrow(&f.v), bgra);
        }
    }
    if half {
        Picture { width: f.width / 2, height: f.height / 2, scale: 2 }
    } else {
        Picture { width: f.width, height: f.height, scale: 1 }
    }
}

// ----- either codec -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Avc,
    Hevc,
}

pub enum VideoDecoder {
    Avc(AvcDecoder),
    Hevc(HevcDecoder),
}

impl VideoDecoder {
    /// `params`: SPS and PPS for H.264; VPS, SPS and PPS for HEVC.
    pub fn new(codec: Codec, params: &[Vec<u8>]) -> Self {
        match codec {
            Codec::Avc => Self::Avc(AvcDecoder::new(params[0].clone(), params[1].clone())),
            Codec::Hevc => Self::Hevc(HevcDecoder::new(params)),
        }
    }

    pub fn codec(&self) -> Codec {
        match self {
            Self::Avc(_) => Codec::Avc,
            Self::Hevc(_) => Codec::Hevc,
        }
    }

    pub fn feed_au(&mut self, nals: &[Vec<u8>], bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        match self {
            Self::Avc(d) => d.feed_au(nals, bgra),
            Self::Hevc(d) => d.feed_au(nals, bgra),
        }
    }

    pub fn poll(&mut self, bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        match self {
            Self::Avc(d) => d.poll(bgra),
            Self::Hevc(d) => d.poll(bgra),
        }
    }

    pub fn finish(&mut self, bgra: &mut Vec<u8>) -> Result<Option<Picture>> {
        match self {
            Self::Avc(d) => d.finish(bgra),
            Self::Hevc(d) => d.finish(bgra),
        }
    }

    pub fn restart(&mut self) {
        match self {
            Self::Avc(d) => d.restart(),
            Self::Hevc(d) => d.restart(),
        }
    }

    pub fn mark_broken(&mut self) {
        match self {
            Self::Avc(d) => d.mark_broken(),
            Self::Hevc(d) => d.mark_broken(),
        }
    }

    pub fn waiting_for_keyframe(&self) -> bool {
        match self {
            Self::Avc(d) => d.waiting_for_keyframe(),
            Self::Hevc(d) => d.await_key,
        }
    }

    pub fn busy(&self) -> bool {
        match self {
            Self::Avc(d) => d.busy(),
            Self::Hevc(d) => d.in_flight > 0,
        }
    }
}

/// Decode tiny embedded clips to prove the built-in decoders work here.
pub fn self_test() -> Result<String> {
    let clip = include_bytes!("testdata/selftest.h264");
    let mut d = Decoder::new();
    let mut pictures = 0;
    for n in parse_annex_b(clip) {
        d.decode_nal(&n).map_err(|e| anyhow::anyhow!("H.264 decoder self-test failed: {e:?}"))?;
        if let Some(f) = d.flush() {
            anyhow::ensure!((f.width, f.height) == (64, 48), "H.264 decoder self-test produced {}x{}", f.width, f.height);
            pictures += 1;
        }
    }
    anyhow::ensure!(pictures == 3, "H.264 decoder self-test produced {pictures} of 3 pictures");

    // 160x96 HEVC Range Extensions 4:4:4, as Apple sends.
    let clip = include_bytes!("testdata/selftest444.h265");
    let mut d = rust_h265::Decoder::new();
    let mut pictures = 0;
    let mut check = |f: rust_h265::Frame| -> Result<()> {
        anyhow::ensure!((f.width, f.height, f.chroma_format_idc) == (160, 96, 3), "HEVC decoder self-test produced {}x{} (chroma format {})", f.width, f.height, f.chroma_format_idc);
        pictures += 1;
        Ok(())
    };
    for n in rust_h265::parse_annex_b(clip) {
        if let Some(f) = d.decode_nal(&n).map_err(|e| anyhow::anyhow!("HEVC decoder self-test failed: {e}"))? {
            check(f)?;
        }
    }
    while let Some(f) = d.flush() {
        check(f)?;
    }
    anyhow::ensure!(pictures == 3, "HEVC decoder self-test produced {pictures} of 3 pictures");
    Ok("Ready. Built-in H.264 and HEVC decoders OK.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grey_and_primaries_convert() {
        let mut out = vec![0u8; 2 * 2 * 4];
        yuv_to_bgra(&[128; 4], &[128], &[128], 2, 1, 1, 2, 2, &mut out);
        assert!(out.chunks(4).all(|p| p == [128, 128, 128, 255]));
        // Pure red in BT.709 full range: Y=54, U=99, V=255.
        let mut px = vec![0u8; 4];
        yuv_to_bgra(&[54], &[99], &[255], 1, 1, 1, 1, 1, &mut px);
        assert!(px[2] > 245 && px[1] < 10 && px[0] < 10, "{px:?}");
    }

    #[test]
    fn banded_conversion_matches_single_threaded() {
        for (w, h) in [(64usize, 48usize), (30, 22), (1920, 1080), (8, 2)] {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let px = |i: usize| (i * 7919 % 251) as u8;
            let f = Frame {
                width: w as u32,
                height: h as u32,
                y: (0..w * h).map(px).collect(),
                u: (0..cw * ch).map(|i| px(i + 3)).collect(),
                v: (0..cw * ch).map(|i| px(i + 5)).collect(),
                pic_order_cnt: 0,
                chroma_format: rust_h264::decoder::ChromaFormat::Yuv420,
                bit_depth: 8,
            };
            let mut banded = vec![];
            to_bgra(&f, &mut banded);
            let mut single = vec![0u8; w * h * 4];
            yuv_to_bgra(&f.y, &f.u, &f.v, w, cw, cw, w, h, &mut single);
            assert!(banded == single, "{w}x{h}");
        }
    }

    #[test]
    fn full_chroma_conversion_matches_per_pixel_math() {
        // 4:4:4: each pixel has its own chroma, so neighbours can differ in colour.
        let (y, u, v) = ([54u8, 128, 54, 128], [99u8, 128, 99, 128], [255u8, 128, 255, 128]);
        let mut banded = vec![];
        planes_to_bgra(&y, &u, &v, 2, 2, true, &mut banded);
        for i in 0..4 {
            let mut px = [0u8; 4];
            yuv_to_bgra(&y[i..], &u[i..], &v[i..], 1, 1, 1, 1, 1, &mut px);
            assert_eq!(banded[i * 4..i * 4 + 4], px, "pixel {i}");
        }
        assert!(banded[2] > 245 && banded[6] == 128, "{banded:?}");
    }

    #[test]
    fn halving_averages_each_block() {
        // 4x2 luma (4:4:4): two 2x2 blocks, grey 100 and grey 200 on average.
        let y = [90u8, 110, 190, 210, 100, 100, 200, 200];
        let c = [128u8; 8];
        let mut out = vec![];
        planes_to_bgra_half(&y, &c, &c, 4, 2, true, &mut out);
        assert_eq!(out, vec![100, 100, 100, 255, 200, 200, 200, 255]);
        // 4:2:0: chroma is already at half resolution.
        let mut out420 = vec![];
        planes_to_bgra_half(&y, &[128, 128], &[128, 128], 4, 2, false, &mut out420);
        assert_eq!(out420, out);
    }

    #[test]
    fn hevc_access_units_decode_through_the_wrapper() {
        let clip = include_bytes!("testdata/selftest444.h265");
        let nals = split_annex_b(clip);
        assert!(nals.iter().any(|n| hevc_is_keyframe(n)));
        let mut d = VideoDecoder::new(Codec::Hevc, &[]);
        let (mut bgra, mut au, mut pics) = (vec![], vec![], 0);
        for n in nals {
            let vcl = hevc_type(&n) <= 31;
            au.push(n);
            if vcl {
                pics += d.feed_au(&std::mem::take(&mut au), &mut bgra).unwrap().is_some() as usize;
            }
        }
        pics += d.finish(&mut bgra).unwrap().is_some() as usize;
        assert!(pics >= 1, "no picture decoded");
        assert_eq!(bgra.len(), 160 * 96 * 4);
        // testsrc2's top-left quadrant is not grey: the chroma made it through.
        assert!(bgra.chunks(4).any(|p| p[0].abs_diff(p[2]) > 60), "picture has no colour");
    }

    /// Raw NAL units (without start codes) of an Annex B stream.
    fn split_annex_b(d: &[u8]) -> Vec<Vec<u8>> {
        let mut starts: Vec<usize> = d.windows(3).enumerate().filter(|(_, w)| *w == [0, 0, 1]).map(|(i, _)| i + 3).collect();
        starts.push(d.len() + 3);
        starts
            .windows(2)
            .map(|w| {
                let mut n = d[w[0]..w[1] - 3].to_vec();
                while n.last() == Some(&0) {
                    n.pop(); // the next start code's leading zero
                }
                n
            })
            .collect()
    }

    #[test]
    fn bundled_decoder_self_test_passes() {
        let r = self_test().unwrap();
        assert!(r.starts_with("Ready."), "{r}");
    }

    #[test]
    fn padded_slices_decode_through_the_wrapper() {
        // The clip's own SPS/PPS, then every slice through AvcDecoder (which pads).
        let clip = include_bytes!("testdata/selftest.h264");
        let mut starts: Vec<usize> = clip.windows(4).enumerate().filter(|(_, w)| *w == [0, 0, 0, 1]).map(|(i, _)| i + 4).collect();
        starts.push(clip.len() + 4);
        let nals: Vec<Vec<u8>> = starts.windows(2).map(|w| clip[w[0]..w[1] - 4].to_vec()).collect();
        let sps = nals.iter().find(|n| n[0] & 0x1f == 7).unwrap().clone();
        let pps = nals.iter().find(|n| n[0] & 0x1f == 8).unwrap().clone();
        let mut d = AvcDecoder::new(sps, pps);
        let mut bgra = vec![];
        let pics = nals.iter().filter(|n| matches!(n[0] & 0x1f, 1 | 5)).filter_map(|n| d.feed(n, &mut bgra).unwrap()).count();
        let last = d.finish(&mut bgra).unwrap();
        assert!(pics >= 1 || last.is_some(), "no picture decoded");
        assert!(!d.busy());
        assert_eq!(bgra.len(), 64 * 48 * 4);
    }
}
