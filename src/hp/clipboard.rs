//! Apple's clipboard extension, carried on the encrypted control channel (ported
//! from iShareScreen's reverse engineering of screensharingd):
//!
//! * 0x15 (client): turn on pasteboard monitoring, once per session.
//! * 0x0b (client): ask for the Mac's pasteboard contents.
//! * 0x14 (server) with command 2: the Mac's pasteboard changed; ask again.
//! * 0x1f (both ways): pasteboard contents. A 16-byte header (type, 0, promise
//!   flag, 0, reserved u32, uncompressed size u32, compressed size u32) and a zlib
//!   stream ending in a sync flush. A large message spans several records; only
//!   the first carries the header.
//!
//! The zlib payload is a list of items: u32 count, then per item a length-prefixed
//! type identifier (UTI), a reserved u32, u32 alias count with length-prefixed
//! (name, value) pairs, and the length-prefixed data. Text is
//! `public.utf8-plain-text`.
//!
//! The standard RFB ClientCutText (0x06) is not used over HP: the Mac only puts it
//! on the legacy scrap, which modern apps do not read.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

pub const MSG_SEND: u8 = 0x1f;
pub const MSG_STATUS: u8 = 0x14;
/// 0x14 command meaning "the Mac's pasteboard changed".
pub const STATUS_CLIPBOARD_CHANGED: u16 = 2;
const UTF8_TEXT: &[u8] = b"public.utf8-plain-text";
/// Largest record body we send (control records carry a u16 length).
const RECORD_MAX: usize = 32 * 1024;
/// Refuse to inflate pasteboards beyond this (images and such are not wanted).
const MAX_UNCOMPRESSED: usize = 16 << 20;

/// 0x15 AutoPasteboard: mode 1 starts monitoring the Mac's pasteboard.
pub fn enable() -> Vec<u8> {
    vec![0x15, 0, 0, 1, 0, 0, 0, 0]
}

/// 0x0b: fetch the full pasteboard (exactly 8 bytes; a 9th desyncs the Mac).
pub fn fetch() -> Vec<u8> {
    vec![0x0b, 0, 0, 0, 0, 0, 0, 0]
}

/// The command of a 0x14 status push.
pub fn status_command(m: &[u8]) -> Option<u16> {
    (m.first() == Some(&MSG_STATUS)).then(|| m.get(6..8).map(|b| u16::from_be_bytes([b[0], b[1]]))).flatten()
}

/// The 0x1f message putting `text` on the Mac's pasteboard, split into records.
pub fn send(text: &str) -> Vec<Vec<u8>> {
    let t = text.as_bytes();
    let mut inner = Vec::with_capacity(t.len() + 40);
    inner.extend_from_slice(&1u32.to_be_bytes());
    inner.extend_from_slice(&(UTF8_TEXT.len() as u32).to_be_bytes());
    inner.extend_from_slice(UTF8_TEXT);
    inner.extend_from_slice(&0u32.to_be_bytes()); // reserved
    inner.extend_from_slice(&0u32.to_be_bytes()); // no aliases
    inner.extend_from_slice(&(t.len() as u32).to_be_bytes());
    inner.extend_from_slice(t);

    let mut z = Compress::new(Compression::default(), true);
    let mut packed = Vec::with_capacity(inner.len() / 2 + 64);
    loop {
        packed.reserve(inner.len() / 2 + 64);
        let before = z.total_in() as usize;
        z.compress_vec(&inner[before..], &mut packed, FlushCompress::Sync).expect("in-memory deflate");
        if z.total_in() as usize == inner.len() && packed.len() < packed.capacity() {
            break;
        }
    }

    let mut msg = vec![MSG_SEND, 0, 0, 0, 0, 0, 0, 0];
    msg.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    msg.extend_from_slice(&(packed.len() as u32).to_be_bytes());
    msg.extend_from_slice(&packed);
    msg.chunks(RECORD_MAX).map(<[u8]>::to_vec).collect()
}

/// Joins a 0x1f message that spans several records.
#[derive(Default)]
pub struct Reassembler {
    buf: Vec<u8>,
    expected: usize,
}

impl Reassembler {
    pub fn in_progress(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Feed a record that is a 0x1f message or, while one is in progress, its
    /// continuation. Returns the whole message once complete.
    pub fn feed(&mut self, m: &[u8]) -> Option<Vec<u8>> {
        if self.buf.is_empty() {
            if m.len() < 16 || m[0] != MSG_SEND {
                return None;
            }
            self.expected = 16 + u32::from_be_bytes(m[12..16].try_into().unwrap()) as usize;
        }
        self.buf.extend_from_slice(m);
        if self.buf.len() < self.expected {
            return None;
        }
        let mut full = std::mem::take(&mut self.buf);
        full.truncate(self.expected);
        Some(full)
    }
}

/// The text on a pasteboard sent in a complete 0x1f message, if it has any.
pub fn text(msg: &[u8]) -> Option<String> {
    if msg.len() < 16 || msg[0] != MSG_SEND {
        return None;
    }
    let size = u32::from_be_bytes(msg[8..12].try_into().unwrap()) as usize;
    if size == 0 || size > MAX_UNCOMPRESSED {
        return None;
    }
    let mut out = Vec::with_capacity(size);
    let mut z = Decompress::new(true);
    let input = &msg[16..];
    while out.len() < size {
        let at = z.total_in() as usize;
        let st = z.decompress_vec(&input[at..], &mut out, FlushDecompress::Sync).ok()?;
        if st == Status::StreamEnd || (z.total_in() as usize == input.len() && out.len() < out.capacity()) {
            break;
        }
        out.reserve(size - out.len());
    }
    let items = items(&out)?;
    let pick = |pred: &dyn Fn(&str) -> bool| items.iter().find(|(uti, _)| pred(uti));
    if let Some((_, d)) = pick(&|u| u == "public.utf8-plain-text") {
        return Some(String::from_utf8_lossy(d).into_owned());
    }
    if let Some((u, d)) = pick(&|u| u.starts_with("public.") && u.contains("text")) {
        if u.contains("utf16") {
            let be = u.contains("external");
            let w: Vec<u16> = d.chunks_exact(2).map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
            return Some(String::from_utf16_lossy(&w));
        }
        return Some(String::from_utf8_lossy(d).into_owned());
    }
    pick(&|u| u == "com.apple.traditional-mac-plain-text").map(|(_, d)| d.iter().map(|&b| b as char).collect())
}

fn items(d: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let mut p = 0usize;
    let u32_at = |p: &mut usize| -> Option<u32> {
        let v = u32::from_be_bytes(d.get(*p..*p + 4)?.try_into().ok()?);
        *p += 4;
        Some(v)
    };
    let bytes_at = |p: &mut usize| -> Option<&[u8]> {
        let n = u32_at(p)? as usize;
        let b = d.get(*p..p.checked_add(n)?)?;
        *p += n;
        Some(b)
    };
    let count = u32_at(&mut p)?;
    let mut v = vec![];
    for _ in 0..count.min(1024) {
        let uti = String::from_utf8_lossy(bytes_at(&mut p)?).into_owned();
        u32_at(&mut p)?;
        for _ in 0..u32_at(&mut p)? {
            bytes_at(&mut p)?;
            bytes_at(&mut p)?;
        }
        v.push((uti, bytes_at(&mut p)?.to_vec()));
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sent_text_reads_back() {
        for t in ["hello", "naïve café — ✓ 日本語\nline two", &"x".repeat(200_000)] {
            let recs = send(t);
            assert!(recs.iter().all(|r| r.len() <= RECORD_MAX));
            let mut r = Reassembler::default();
            let mut full = None;
            for rec in &recs {
                assert!(full.is_none());
                full = r.feed(rec);
            }
            assert!(!r.in_progress());
            assert_eq!(text(&full.unwrap()).as_deref(), Some(t));
        }
    }

    #[test]
    fn large_text_spans_records() {
        let mut x = 0x2545_f491u32;
        let noisy: String = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                char::from(b'a' + (x % 26) as u8)
            })
            .collect();
        let recs = send(&noisy);
        assert!(recs.len() > 1);
        let mut r = Reassembler::default();
        let full = recs.iter().find_map(|rec| r.feed(rec)).unwrap();
        assert_eq!(text(&full).as_deref(), Some(noisy.as_str()));
    }

    #[test]
    fn fixed_messages() {
        assert_eq!(fetch().len(), 8);
        assert_eq!(enable(), [0x15, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(status_command(&[0x14, 0, 0, 4, 0, 1, 0, 2]), Some(STATUS_CLIPBOARD_CHANGED));
        assert_eq!(status_command(&[0x00, 0, 0, 4, 0, 1, 0, 2]), None);
    }

    #[test]
    fn empty_or_non_text_pasteboards_have_no_text() {
        let mut m = send("x").concat();
        m[8..12].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(text(&m), None);
        assert_eq!(text(&[0x1f, 0, 0]), None);
    }
}
