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

/// Files and folders on the Mac that a complete 0x1f message stands for (paths,
/// or bare names for a Finder Copy), and the item types it held.
pub fn files(msg: &[u8]) -> (Vec<String>, Vec<String>) {
    let Some(items) = unpack(msg) else { return (vec![], vec![]) };
    // Type and size of every item, and the message's promise flag (byte 2), for the log.
    let types = items.iter().map(|(u, d)| format!("{u} ({} bytes, promise flag {})", d.len(), msg[2])).collect();
    let mut paths: Vec<String> = vec![];
    // Finder's Copy on a file puts its name (as text) and its icon on the pasteboard
    // but no path; the names are returned for the caller to look for.
    let icon = items.iter().any(|(u, _)| u == "com.apple.icns");
    for (uti, d) in &items {
        if uti == "public.file-url" || uti.ends_with("promised-file-url") {
            let url = String::from_utf8_lossy(d);
            if let Some(p) = file_url_path(url.trim_matches(|c: char| c == '\0' || c.is_whitespace())) {
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
    }
    if paths.is_empty() && icon {
        if let Some((_, d)) = items.iter().find(|(u, _)| u == "public.utf8-plain-text") {
            for line in String::from_utf8_lossy(d).lines() {
                let name = line.trim_matches(|c: char| c == '\0' || c.is_whitespace());
                if !name.is_empty() && name.chars().count() < 256 && !paths.iter().any(|p| p == name) {
                    paths.push(name.to_owned());
                }
            }
        }
    }
    (paths, types)
}

/// `file:///Users/me/a%20b.txt` to `/Users/me/a b.txt`.
fn file_url_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let b = path.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&path[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    let p = String::from_utf8(out).ok()?;
    Some(if p.len() > 1 { p.trim_end_matches('/').to_owned() } else { p })
}

/// The items of a complete 0x1f message.
fn unpack(msg: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
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
    items(&out)
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
    fn finder_copies_give_file_paths() {
        assert_eq!(file_url_path("file:///Users/me/a%20b%C3%A9.txt").as_deref(), Some("/Users/me/a bé.txt"));
        assert_eq!(file_url_path("file://localhost/Users/me/Folder/").as_deref(), Some("/Users/me/Folder"));
        assert_eq!(file_url_path("https://x/y"), None);
        // %41 at the very end of the string is still decoded.
        assert_eq!(file_url_path("file:///a%41").as_deref(), Some("/aA"));
    }

    #[test]
    fn empty_or_non_text_pasteboards_have_no_text() {
        let mut m = send("x").concat();
        m[8..12].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(text(&m), None);
        assert_eq!(text(&[0x1f, 0, 0]), None);
    }
}
