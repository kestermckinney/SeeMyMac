//! RTP depacketization for Apple's H.264 stream, RTCP feedback, and the
//! per-SSRC access-unit buffer that waits briefly for retransmissions.
//!
//! Apple's packetization (RFC 6184, DON-less): FU-A for large slices, single NAL
//! packets for small ones, and an Apple-specific config packet (first byte 0x92)
//! carrying an ISO-BMFF `avc1`/`avcC` box with the SPS/PPS. Keyframes are intra
//! slices in ordinary type-1 NALs; there are no type-5 IDRs.
//!
//! When the Mac chooses HEVC it follows RFC 7798 (no DONL): an aggregation
//! packet with the VPS/SPS/PPS before each IRAP picture, then fragmentation
//! units, one slice per picture.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CONFIG_MARKER: u8 = 0x92;

pub fn reassemble_h264(payloads: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut out = vec![];
    let mut fu: Option<Vec<u8>> = None;
    for p in payloads {
        let Some(&b0) = p.first() else { continue };
        if b0 == CONFIG_MARKER {
            continue;
        }
        match b0 & 0x1f {
            t @ (28 | 29) => {
                let off = if t == 29 { 4 } else { 2 };
                if p.len() < off {
                    continue;
                }
                let h = p[1];
                if h & 0x80 != 0 {
                    let mut v = vec![(b0 & 0xe0) | (h & 0x1f)];
                    v.extend_from_slice(&p[off..]);
                    fu = Some(v);
                } else if let Some(v) = fu.as_mut() {
                    v.extend_from_slice(&p[off..]);
                }
                if h & 0x40 != 0 {
                    if let Some(v) = fu.take() {
                        out.push(v);
                    }
                }
            }
            24 => {
                let mut pos = 3;
                while pos + 2 <= p.len() {
                    let n = u16::from_be_bytes([p[pos], p[pos + 1]]) as usize;
                    pos += 2;
                    if n == 0 || pos + n > p.len() {
                        break;
                    }
                    out.push(p[pos..pos + n].to_vec());
                    pos += n;
                }
            }
            1..=23 if p.len() >= 2 => out.push(p.to_vec()),
            _ => {}
        }
    }
    out
}

/// NAL units from one HEVC access unit's RTP payloads (RFC 7798).
pub fn reassemble_h265(payloads: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut out = vec![];
    let mut fu: Option<Vec<u8>> = None;
    for p in payloads {
        if p.len() < 3 || p[0] == CONFIG_MARKER {
            continue;
        }
        match (p[0] >> 1) & 0x3f {
            48 => {
                let mut pos = 2;
                while pos + 2 <= p.len() {
                    let n = u16::from_be_bytes([p[pos], p[pos + 1]]) as usize;
                    pos += 2;
                    if n < 2 || pos + n > p.len() {
                        break;
                    }
                    out.push(p[pos..pos + n].to_vec());
                    pos += n;
                }
            }
            49 => {
                let h = p[2];
                if h & 0x80 != 0 {
                    let mut v = vec![(p[0] & 0x81) | ((h & 0x3f) << 1), p[1]];
                    v.extend_from_slice(&p[3..]);
                    fu = Some(v);
                } else if let Some(v) = fu.as_mut() {
                    v.extend_from_slice(&p[3..]);
                }
                if h & 0x40 != 0 {
                    if let Some(v) = fu.take() {
                        out.push(v);
                    }
                }
            }
            0..=47 => out.push(p.to_vec()),
            _ => {}
        }
    }
    out
}

/// (SPS, PPS) from Apple's 0x92 config packet.
pub fn parse_avc_config(p: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let find = super::offers::find;
    let a1 = find(p, b"avc1").unwrap_or(0);
    let idx = a1 + find(&p[a1..], b"avcC")?;
    let mut q = idx + 4;
    let num_sps = (*p.get(q + 5)? & 0x1f) as usize;
    q += 6;
    let take = |q: &mut usize| -> Option<Vec<u8>> {
        let n = u16::from_be_bytes([*p.get(*q)?, *p.get(*q + 1)?]) as usize;
        *q += 2;
        let v = p.get(*q..*q + n)?.to_vec();
        *q += n;
        Some(v)
    };
    let mut sps = None;
    for _ in 0..num_sps {
        let v = take(&mut q)?;
        sps.get_or_insert(v);
    }
    let num_pps = *p.get(q)? as usize;
    q += 1;
    let mut pps = None;
    for _ in 0..num_pps {
        let v = take(&mut q)?;
        pps.get_or_insert(v);
    }
    let (sps, pps) = (sps?, pps?);
    if sps.first()? & 0x1f == 7 && pps.first()? & 0x1f == 8 {
        Some((sps, pps))
    } else {
        None
    }
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let b = *self.d.get(self.pos / 8)?;
        let v = (b >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v as u32)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut v = 0u32;
        for _ in 0..zeros {
            v = (v << 1) | self.bit()?;
        }
        Some((1u32 << zeros) - 1 + v)
    }
}

fn unescape(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len());
    let mut zeros = 0;
    for &b in d {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// True for an IDR, or a type-1 slice whose slice_type is I (Apple's keyframes).
pub fn is_keyframe(nal: &[u8]) -> bool {
    match nal.first().map(|b| b & 0x1f) {
        Some(5) => true,
        Some(1) if nal.len() > 1 => {
            let rbsp = unescape(&nal[1..nal.len().min(32)]);
            let mut b = Bits { d: &rbsp, pos: 0 };
            b.ue().is_some() && matches!(b.ue(), Some(2 | 7))
        }
        _ => false,
    }
}

/// An HEVC VPS/SPS/PPS, aggregation or fragmentation packet (the Mac chose HEVC). Requires the
/// full two-byte HEVC header so H.264 headers such as 0x41/0x61 don't match.
pub fn looks_like_hevc(payload: &[u8]) -> bool {
    match payload {
        [b0, 1, ..] => b0 & 0x81 == 0 && matches!((b0 >> 1) & 0x3f, 32 | 33 | 34 | 48 | 49),
        _ => false,
    }
}

// ----- RTCP ---------------------------------------------------------------------

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

pub fn rr(sender: u32, sources: &[(u32, u32, u16)], sr: &HashMap<u32, (u32, Instant)>) -> Vec<u8> {
    if sources.is_empty() {
        let mut m = vec![0x80, 201, 0, 1];
        m.extend(be32(sender));
        return m;
    }
    let rc = sources.len().min(31);
    let mut m = vec![0x80 | rc as u8, 201];
    m.extend(((1 + rc * 6) as u16).to_be_bytes());
    m.extend(be32(sender));
    for &(ssrc, roc, max_seq) in &sources[..rc] {
        let (lsr, dlsr) = sr.get(&ssrc).map_or((0, 0), |(lsr, at)| (*lsr, (at.elapsed().as_secs_f64() * 65536.0) as u32));
        for v in [ssrc, 0, ((roc & 0xffff) << 16) | max_seq as u32, 0, lsr, dlsr] {
            m.extend(be32(v));
        }
    }
    m
}

pub fn empty_sr(sender: u32) -> Vec<u8> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let ntp_sec = (now.as_secs() + 2_208_988_800) as u32;
    let frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    let rtp_ts = (now.as_secs_f64() * 90000.0) as u64 as u32;
    let mut m = vec![0x80, 200, 0, 6];
    for v in [sender, ntp_sec, frac as u32, rtp_ts, 0, 0] {
        m.extend(be32(v));
    }
    m
}

/// AVPF FIR + PLI + the legacy RFC 2032 FIR the native viewer sends, after an RR.
pub fn keyframe_request(sender: u32, target: u32, seq: u8) -> Vec<u8> {
    let mut m = rr(sender, &[], &HashMap::new());
    m.extend([0x84, 206, 0, 4]);
    m.extend(be32(sender));
    m.extend(be32(0));
    m.extend(be32(target));
    m.extend([seq, 0, 0, 0]);
    m.extend([0x81, 206, 0, 2]);
    m.extend(be32(sender));
    m.extend(be32(target));
    m.extend([0x80, 192, 0, 1]);
    m.extend(be32(target));
    m
}

/// Generic NACK (RFC 4585) after an RR; consecutive losses share a bitmask.
pub fn nack(sender: u32, media: u32, lost: &HashSet<u16>) -> Option<Vec<u8>> {
    let mut seqs: Vec<u16> = lost.iter().copied().collect();
    if seqs.is_empty() {
        return None;
    }
    seqs.sort_unstable();
    let mut fci = vec![];
    let mut i = 0;
    while i < seqs.len() {
        let pid = seqs[i];
        let mut blp = 0u16;
        let mut j = i + 1;
        while j < seqs.len() {
            let d = seqs[j].wrapping_sub(pid);
            if (1..=16).contains(&d) {
                blp |= 1 << (d - 1);
                j += 1;
            } else {
                break;
            }
        }
        fci.extend(pid.to_be_bytes());
        fci.extend(blp.to_be_bytes());
        i = j;
    }
    let mut m = rr(sender, &[], &HashMap::new());
    m.extend([0x81, 205]);
    m.extend(((2 + fci.len() / 4) as u16).to_be_bytes());
    m.extend(be32(sender));
    m.extend(be32(media));
    m.extend(fci);
    Some(m)
}

/// (ssrc, middle 32 bits of the NTP time) for each Sender Report in a compound packet.
pub fn sender_reports(d: &[u8]) -> Vec<(u32, u32)> {
    let mut out = vec![];
    let mut pos = 0;
    while pos + 4 <= d.len() {
        let len = (u16::from_be_bytes([d[pos + 2], d[pos + 3]]) as usize + 1) * 4;
        if pos + len > d.len() {
            break;
        }
        if d[pos + 1] == 200 && len >= 28 {
            let r = |o: usize| u32::from_be_bytes(d[pos + o..pos + o + 4].try_into().unwrap());
            out.push((r(4), ((r(8) & 0xffff) << 16) | (r(12) >> 16)));
        }
        pos += len;
    }
    out
}

// ----- access-unit buffer -----------------------------------------------------

/// How long an access unit with a sequence hole waits for a NACK retransmission.
const REPAIR_WAIT: Duration = Duration::from_millis(75);
/// A group that never sees its marker packet is dropped after this long.
const GROUP_TTL: Duration = Duration::from_millis(200);
const DEDUP_TTL: Duration = Duration::from_secs(2);

type Key = (u32, u32);
type Pkt = (u16, bool, Vec<u8>);

struct Group {
    pkts: Vec<Pkt>,
    arrival: Instant,
    deadline: Option<Instant>,
}

pub enum Unit {
    /// A complete access unit's payloads in sequence order.
    Complete { ssrc: u32, payloads: Vec<Vec<u8>> },
    /// An access unit was lost for good; the reference chain is broken.
    Lost { ssrc: u32 },
}

/// Collects RTP packets into timestamp groups and releases them in decode order.
#[derive(Default)]
pub struct AuBuffer {
    groups: HashMap<Key, Group>,
    flushed: HashMap<Key, Instant>,
    last_marker: HashMap<u32, u16>,
    last_evict: Option<Instant>,
}

fn sorted(pkts: &[Pkt]) -> Vec<&Pkt> {
    let mut v: Vec<&Pkt> = pkts.iter().collect();
    v.sort_by_key(|p| p.0);
    if v.len() < 2 || v[v.len() - 1].0 - v[0].0 <= 0x8000 {
        return v;
    }
    // Wrapped: start right after the largest circular gap.
    let n = v.len();
    let (mut best, mut at) = (0u16, 0);
    for i in 0..n {
        let g = v[(i + 1) % n].0.wrapping_sub(v[i].0);
        if g >= best {
            best = g;
            at = i;
        }
    }
    let start = (at + 1) % n;
    v.rotate_left(start);
    v
}

impl AuBuffer {
    fn complete(&self, key: Key, g: &Group) -> bool {
        let p = sorted(&g.pkts);
        if p.is_empty() || !p[p.len() - 1].1 {
            return false;
        }
        if p.windows(2).any(|w| w[1].0.wrapping_sub(w[0].0) > 1) {
            return false;
        }
        self.last_marker.get(&key.0).map_or(true, |&m| p[0].0 == m.wrapping_add(1))
    }

    fn ordered_keys(&self, ssrc: u32) -> Vec<Key> {
        let mut keys: Vec<Key> = self.groups.keys().filter(|k| k.0 == ssrc).copied().collect();
        match self.last_marker.get(&ssrc) {
            None => keys.sort_by_key(|k| self.groups[k].arrival),
            Some(&m) => {
                let expected = m.wrapping_add(1);
                keys.sort_by_key(|k| {
                    let g = &self.groups[k];
                    (sorted(&g.pkts).first().map_or(u16::MAX, |p| p.0.wrapping_sub(expected)), g.arrival)
                });
            }
        }
        keys
    }

    fn flush(&mut self, key: Key, out: &mut Vec<Unit>) {
        let Some(g) = self.groups.remove(&key) else { return };
        self.flushed.insert(key, Instant::now());
        let p = sorted(&g.pkts);
        if let Some(m) = p.iter().rev().find(|p| p.1) {
            self.last_marker.insert(key.0, m.0);
        }
        let order: Vec<u16> = p.iter().map(|p| p.0).collect();
        let mut by_seq: HashMap<u16, Vec<u8>> = g.pkts.into_iter().map(|p| (p.0, p.2)).collect();
        let payloads = order.iter().filter_map(|s| by_seq.remove(s)).collect();
        out.push(Unit::Complete { ssrc: key.0, payloads });
    }

    fn flush_ready(&mut self, ssrc: u32, out: &mut Vec<Unit>) {
        loop {
            let keys = self.ordered_keys(ssrc);
            let Some(&key) = keys.first() else { return };
            if !self.complete(key, &self.groups[&key]) {
                let g = self.groups.get_mut(&key).unwrap();
                g.deadline.get_or_insert(Instant::now() + REPAIR_WAIT);
                return;
            }
            self.flush(key, out);
        }
    }

    pub fn push(&mut self, ssrc: u32, ts: u32, seq: u16, marker: bool, payload: Vec<u8>, out: &mut Vec<Unit>) {
        let key = (ssrc, ts);
        if self.flushed.contains_key(&key) {
            return;
        }
        let g = self.groups.entry(key).or_insert_with(|| Group { pkts: vec![], arrival: Instant::now(), deadline: None });
        if !g.pkts.iter().any(|p| p.0 == seq) {
            g.pkts.push((seq, marker, payload));
        }
        if !g.pkts.iter().any(|p| p.1) {
            return;
        }
        let now = Instant::now();
        let keys = self.ordered_keys(ssrc);
        let pos = keys.iter().position(|k| *k == key).unwrap_or(0);
        // An older group still missing its marker may own the skipped packets.
        for k in &keys[..pos] {
            let g = self.groups.get_mut(k).unwrap();
            if !g.pkts.iter().any(|p| p.1) {
                g.deadline.get_or_insert(now + REPAIR_WAIT);
            }
        }
        if keys[..pos].iter().any(|k| self.groups[k].deadline.is_some()) {
            return;
        }
        if !self.complete(key, &self.groups[&key]) {
            self.groups.get_mut(&key).unwrap().deadline.get_or_insert(now + REPAIR_WAIT);
            return;
        }
        self.flush(key, out);
        self.flush_ready(ssrc, out);
    }

    fn drop_group(&mut self, key: Key, now: Instant, out: &mut Vec<Unit>) {
        let Some(g) = self.groups.remove(&key) else { return };
        self.flushed.insert(key, now);
        match sorted(&g.pkts).iter().rev().find(|p| p.1) {
            Some(m) => {
                self.last_marker.insert(key.0, m.0);
            }
            None => {
                self.last_marker.remove(&key.0);
            }
        }
        out.push(Unit::Lost { ssrc: key.0 });
    }

    /// Expire groups whose repair window ran out; call often (every ~20 ms).
    pub fn evict(&mut self, out: &mut Vec<Unit>) {
        let now = Instant::now();
        if self.last_evict.map_or(false, |t| now - t < Duration::from_millis(20)) {
            return;
        }
        self.last_evict = Some(now);
        let mut unblocked = HashSet::new();
        let expired: Vec<Key> = self
            .groups
            .iter()
            .filter(|(_, g)| match g.deadline {
                Some(d) => now >= d,
                None => now - g.arrival > GROUP_TTL && !g.pkts.iter().any(|p| p.1),
            })
            .map(|(k, _)| *k)
            .collect();
        for k in expired {
            self.drop_group(k, now, out);
            unblocked.insert(k.0);
        }
        for s in unblocked {
            self.flush_ready(s, out);
        }
        self.flushed.retain(|_, t| now - *t <= DEDUP_TTL);
    }

    /// Forget everything about SSRCs other than `keep` (after adopting a new stream).
    pub fn retain_ssrc(&mut self, keep: u32) {
        self.groups.retain(|k, _| k.0 == keep);
        self.last_marker.retain(|s, _| *s == keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fu_a_and_single_nals_reassemble() {
        let fu1 = [0x7c, 0x81, 1, 2];
        let fu2 = [0x7c, 0x01, 3];
        let fu3 = [0x7c, 0x41, 4];
        let single = [0x61, 9, 9];
        let cfg = [CONFIG_MARKER, 1, 2];
        let nals = reassemble_h264(&[&cfg, &fu1, &fu2, &fu3, &single]);
        assert_eq!(nals, vec![vec![0x61, 1, 2, 3, 4], vec![0x61, 9, 9]]);
    }

    #[test]
    fn hevc_detection_ignores_h264_headers() {
        assert!(looks_like_hevc(&[0x40, 0x01, 0x0c]) && looks_like_hevc(&[0x60, 0x01]));
        assert!(!looks_like_hevc(&[0x61, 0x01]) && !looks_like_hevc(&[0x41, 0x01]) && !looks_like_hevc(&[0x67, 0x64]));
    }

    #[test]
    fn hevc_units_are_reassembled() {
        // AP with VPS/SPS/PPS, then an IDR_N_LP slice in three fragments.
        let mut ap = vec![0x60, 0x01];
        for nal in [&[0x40u8, 0x01, 0xaa][..], &[0x42, 0x01, 0xbb, 0xcc], &[0x44, 0x01, 0xdd]] {
            ap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            ap.extend_from_slice(nal);
        }
        let f1 = [0x62, 0x01, 0x80 | 20, 1, 2];
        let f2 = [0x62, 0x01, 20, 3];
        let f3 = [0x62, 0x01, 0x40 | 20, 4];
        let single = [0x02, 0x01, 9, 9];
        let nals = reassemble_h265(&[&ap, &f1, &f2, &f3, &single, &[CONFIG_MARKER, 0, 0]]);
        assert_eq!(nals, vec![vec![0x40, 0x01, 0xaa], vec![0x42, 0x01, 0xbb, 0xcc], vec![0x44, 0x01, 0xdd], vec![40, 0x01, 1, 2, 3, 4], vec![0x02, 0x01, 9, 9]]);
    }

    #[test]
    fn keyframes_are_intra_slices() {
        // first_mb=0 (1), slice_type=7 (0001000) -> bits 1 0001000 -> 0x88
        assert!(is_keyframe(&[0x61, 0x88]));
        // slice_type=0 (P): bits 1 1 -> 0xc0
        assert!(!is_keyframe(&[0x61, 0xc0]));
        assert!(is_keyframe(&[0x65, 0x88]));
    }

    #[test]
    fn avc_config_is_found_in_the_avcc_box() {
        let mut p = vec![CONFIG_MARKER, 0, 0, 0];
        p.extend_from_slice(b"....avc1........avcC");
        p.extend_from_slice(&[1, 100, 0, 40, 0xff, 0xe1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 5]);
        assert_eq!(parse_avc_config(&p), Some((vec![0x67, 1, 2], vec![0x68, 5])));
        assert_eq!(parse_avc_config(&p[..p.len() - 3]), None);
    }

    #[test]
    fn nack_coalesces_losses() {
        let lost: HashSet<u16> = [10, 11, 13, 40].into_iter().collect();
        let m = nack(1, 2, &lost).unwrap();
        let f = &m[8 + 12..];
        assert_eq!(f, &[0, 10, 0, 0b101, 0, 40, 0, 0]);
    }

    fn units(b: &mut AuBuffer, pkts: &[(u32, u16, bool)]) -> Vec<String> {
        let mut out = vec![];
        for &(ts, seq, m) in pkts {
            b.push(7, ts, seq, m, vec![seq as u8], &mut out);
        }
        out.iter()
            .map(|u| match u {
                Unit::Complete { payloads, .. } => format!("{:?}", payloads.iter().map(|p| p[0]).collect::<Vec<_>>()),
                Unit::Lost { .. } => "lost".into(),
            })
            .collect()
    }

    #[test]
    fn au_buffer_orders_repairs_and_drops() {
        let mut b = AuBuffer::default();
        // In-order frames flush at their marker.
        assert_eq!(units(&mut b, &[(1, 1, false), (1, 2, true)]), vec!["[1, 2]"]);
        // A hole holds the frame and everything after it until the retransmit lands.
        assert!(units(&mut b, &[(2, 3, false), (2, 5, true), (3, 6, true)]).is_empty());
        assert_eq!(units(&mut b, &[(2, 4, false)]), vec!["[3, 4, 5]", "[6]"]);
        // Wrap-around ordering.
        let mut w = AuBuffer::default();
        assert_eq!(units(&mut w, &[(9, 65535, false), (9, 0, false), (9, 1, true)]), vec!["[255, 0, 1]"]);
        // An unrepaired hole is dropped after the repair window, unblocking successors.
        let mut d = AuBuffer::default();
        assert_eq!(units(&mut d, &[(1, 1, true)]), vec!["[1]"]);
        assert!(units(&mut d, &[(2, 3, true), (3, 4, true)]).is_empty());
        std::thread::sleep(REPAIR_WAIT + Duration::from_millis(30));
        let mut out = vec![];
        d.evict(&mut out);
        let got: Vec<&str> = out.iter().map(|u| if matches!(u, Unit::Lost { .. }) { "lost" } else { "ok" }).collect();
        assert_eq!(got, vec!["lost", "ok"]);
    }
}
