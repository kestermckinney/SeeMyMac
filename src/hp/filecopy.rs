//! Apple's file-copy extension (message 0x22), PC to Mac only: dropping files or
//! folders onto the remote screen copies them to the Mac. Reverse engineered from
//! `screensharingd` (which relays these messages) and the `SSFileCopyReceiver`
//! helper it starts as the logged-in user (which acts on them).
//!
//! Every message is `0x22, 0, u32 length`, then a body that starts with
//! `u16 version (1), u16 command, u32 session id`; everything is big endian.
//!
//! Client to Mac, one *session* per top-level file or folder (the receiver keeps a
//! single slot for the top-level item and exits when its session ends):
//!
//! * 2 `StartFileReceive`: `u32 flags (0), u32 reserved, u16 path length`, the
//!   destination folder (UTF-8, NUL terminated; `~` means the user's home).
//! * 100 `ItemInfo`: `u32 flags, u32 reserved`, then `u64` logical size, physical
//!   size, file count, fork count, folder count and `u32` allocation size.
//! * 101 `NewItem`, one per file or folder, parents before children: item kind
//!   (1 file, 2 folder), 33 bytes of Finder info (unused here), `u64` resource
//!   fork size, `u64` data fork size, five 8-byte timestamps (create, content
//!   modified, attribute modified, access, backup: `u16 high, u32 low` seconds
//!   since 1904 and `u16` fraction), `u16 node flags`, `u16` nesting level (0 for
//!   the top-level item), `u16` Unix mode (the receiver `chmod`s to it; 0 leaves
//!   the copy unusable), `u32`, `u16` name length, `u16` symlink length, and
//!   the NUL-terminated UTF-8 name.
//! * 102 `FileData`: `u32 length` and that many bytes of the data fork of the item
//!   just announced. Items with no data have none; a file's fork ends when its
//!   announced size has arrived. (103 carries the same data zlib-compressed.)
//! * 104 `EndSession`: `u32` error code (0).
//!
//! The other way (a file fetched from the Mac): the client sends 1 `StartFileSend`
//! (same layout as 2, flags 1 = start now, a path on the Mac) and the Mac's
//! `SSFileCopySender` answers with the same records 100 to 104, relayed verbatim;
//! 101 also carries the Unix mode, 103 is zlib-compressed 102 data (one stream for
//! the session, `u16` type 1, `u32` size, `u32` compressed size, then the data) and
//! a file's resource fork, if it has one, follows its data fork.
//!
//! Mac to client: 300 `Progress` (`u64` bytes written so far) and 200 `EndStatus`
//! (`u16` status, 0 for success; `u16` name length and the final name).

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use flate2::{Decompress, FlushDecompress};

pub const MSG: u8 = 0x22;
const VERSION: u16 = 1;
const CMD_START_SEND: u16 = 1;
const CMD_START_RECEIVE: u16 = 2;
const CMD_STOP: u16 = 5;
const CMD_ITEM_INFO: u16 = 100;
const CMD_NEW_ITEM: u16 = 101;
const CMD_FILE_DATA: u16 = 102;
const CMD_COMPRESSED: u16 = 103;
const CMD_END_SESSION: u16 = 104;
const CMD_END_STATUS: u16 = 200;
const CMD_PROGRESS: u16 = 300;

/// Where dropped items land on the Mac.
pub const DEFAULT_DEST: &str = "~/Desktop";
/// File bytes per message. A whole message must fit one control record (the Mac
/// takes up to 32 KiB), so a message is never interleaved with other traffic.
const CHUNK: usize = 32_000;
const KIND_FILE: u8 = 1;
const KIND_FOLDER: u8 = 2;
/// Seconds from 1904-01-01 (the Mac epoch) to 1970-01-01.
const MAC_EPOCH: u64 = 2_082_844_800;
/// The receiver indexes nesting levels below 0x400.
const MAX_LEVEL: u16 = 200;
/// Longest name the Mac accepts, in UTF-16 units.
const MAX_NAME: usize = 255;
/// How long to wait on the Mac for the end status once everything is sent.
const CONFIRM: Duration = Duration::from_secs(30);

// ----- messages --------------------------------------------------------------------

fn header(cmd: u16, session: u32, rest: usize) -> Vec<u8> {
    let mut m = Vec::with_capacity(14 + rest);
    m.extend_from_slice(&[MSG, 0]);
    m.extend_from_slice(&((8 + rest) as u32).to_be_bytes());
    m.extend_from_slice(&VERSION.to_be_bytes());
    m.extend_from_slice(&cmd.to_be_bytes());
    m.extend_from_slice(&session.to_be_bytes());
    m
}

pub fn start_receive(session: u32, dest: &str) -> Vec<u8> {
    start(CMD_START_RECEIVE, session, 0, dest)
}

/// Ask the Mac to send `path` (an absolute path on the Mac) right away.
pub fn start_send(session: u32, path: &str) -> Vec<u8> {
    start(CMD_START_SEND, session, 1, path)
}

fn stop(session: u32) -> Vec<u8> {
    header(CMD_STOP, session, 0)
}

fn start(cmd: u16, session: u32, flags: u32, path: &str) -> Vec<u8> {
    let path = path.as_bytes();
    let mut m = header(cmd, session, 10 + path.len() + 1);
    m.extend_from_slice(&flags.to_be_bytes());
    m.extend_from_slice(&0u32.to_be_bytes()); // reserved
    m.extend_from_slice(&(path.len() as u16).to_be_bytes());
    m.extend_from_slice(path);
    m.push(0);
    m
}

pub fn item_info(session: u32, bytes: u64, files: u64, folders: u64) -> Vec<u8> {
    let mut m = header(CMD_ITEM_INFO, session, 52);
    m.extend_from_slice(&1u32.to_be_bytes());
    m.extend_from_slice(&0u32.to_be_bytes());
    for v in [bytes, bytes, files, files, folders] {
        m.extend_from_slice(&v.to_be_bytes());
    }
    m.extend_from_slice(&0u32.to_be_bytes()); // allocation size
    m
}

/// A time as the Mac's `UTCDateTime`: u16 high and u32 low seconds since 1904, u16 fraction.
fn mac_time(t: SystemTime) -> [u8; 8] {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) + MAC_EPOCH;
    let mut b = [0u8; 8];
    b[0..2].copy_from_slice(&((secs >> 32) as u16).to_be_bytes());
    b[2..6].copy_from_slice(&(secs as u32).to_be_bytes());
    b
}

fn new_item(session: u32, it: &Item) -> Vec<u8> {
    let name = it.name.as_bytes();
    let mut m = header(CMD_NEW_ITEM, session, 0x70 + name.len() + 1 - 8);
    m.push(it.kind);
    m.push(0);
    m.extend_from_slice(&[0u8; 32]); // Finder info
    m.extend_from_slice(&0u64.to_be_bytes()); // resource fork
    m.extend_from_slice(&it.size.to_be_bytes()); // data fork
    let modified = mac_time(it.modified);
    for t in [modified, modified, modified, mac_time(it.accessed), [0u8; 8]] {
        m.extend_from_slice(&t);
    }
    m.extend_from_slice(&0u16.to_be_bytes()); // node flags
    m.extend_from_slice(&it.level.to_be_bytes());
    m.extend_from_slice(&it.mode.to_be_bytes());
    m.extend_from_slice(&0u32.to_be_bytes());
    m.extend_from_slice(&(name.len() as u16).to_be_bytes());
    m.extend_from_slice(&0u16.to_be_bytes()); // symlink length
    m.extend_from_slice(name);
    m.push(0);
    m
}

fn file_data(session: u32, data: &[u8]) -> Vec<u8> {
    let mut m = header(CMD_FILE_DATA, session, 4 + data.len());
    m.extend_from_slice(&(data.len() as u32).to_be_bytes());
    m.extend_from_slice(data);
    m
}

fn end_session(session: u32, error: u32) -> Vec<u8> {
    let mut m = header(CMD_END_SESSION, session, 4);
    m.extend_from_slice(&error.to_be_bytes());
    m
}

// ----- what to send ------------------------------------------------------------------

#[derive(Debug)]
struct Item {
    path: PathBuf,
    name: String,
    level: u16,
    kind: u8,
    size: u64,
    modified: SystemTime,
    accessed: SystemTime,
    /// Unix permission bits (the receiver `chmod`s the finished item to these).
    mode: u16,
}

/// A top-level file or folder and everything inside it, parents first.
#[derive(Debug)]
pub struct Job {
    pub name: String,
    items: Vec<Item>,
    bytes: u64,
    files: u64,
    folders: u64,
}

fn visit(path: &Path, level: u16, job: &mut Job) -> io::Result<()> {
    // Links are followed: the Mac gets the content they point at.
    let meta = std::fs::metadata(path)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    if name.encode_utf16().count() > MAX_NAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("the name of {name:?} is too long for a Mac")));
    }
    let kind = if meta.is_dir() {
        KIND_FOLDER
    } else if meta.is_file() {
        KIND_FILE
    } else {
        return Ok(()); // sockets, devices and the like
    };
    let size = if kind == KIND_FILE { meta.len() } else { 0 };
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    // The owner must always be able to use the copy, whatever the source's mode.
    let mode = (meta.permissions().mode() & 0o7777) as u16 | if kind == KIND_FOLDER { 0o700 } else { 0o600 };
    job.items.push(Item { path: path.to_owned(), name, level, kind, size, modified, accessed: meta.accessed().unwrap_or(modified), mode });
    if kind == KIND_FILE {
        job.files += 1;
        job.bytes += size;
        return Ok(());
    }
    job.folders += 1;
    if level >= MAX_LEVEL {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "folders are nested too deeply"));
    }
    let mut kids: Vec<PathBuf> = std::fs::read_dir(path)?.map(|e| e.map(|e| e.path())).collect::<io::Result<_>>()?;
    kids.sort();
    for k in kids {
        visit(&k, level + 1, job)?;
    }
    Ok(())
}

pub fn plan(path: &Path) -> io::Result<Job> {
    let mut job = Job { name: String::new(), items: vec![], bytes: 0, files: 0, folders: 0 };
    visit(path, 0, &mut job)?;
    match job.items.first() {
        Some(first) => job.name = first.name.clone(),
        None => return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file or folder")),
    }
    Ok(job)
}

type Sink<'a> = dyn FnMut(&[u8]) -> io::Result<()> + 'a;

/// Stream one job's messages to `send`; `sent` is told how many file bytes have gone.
fn stream(job: &Job, session: u32, dest: &str, send: &mut Sink, sent: &mut dyn FnMut(u64), cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    send(&start_receive(session, dest))?;
    send(&item_info(session, job.bytes, job.files, job.folders))?;
    let mut total = 0u64;
    let mut buf = vec![0u8; CHUNK];
    for it in &job.items {
        send(&new_item(session, it))?;
        if it.kind != KIND_FILE || it.size == 0 {
            continue;
        }
        let mut f = std::fs::File::open(&it.path)?;
        let mut left = it.size;
        while left > 0 {
            if cancelled() {
                send(&end_session(session, 1))?;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let want = left.min(CHUNK as u64) as usize;
            let n = f.read(&mut buf[..want])?;
            if n == 0 {
                // The file shrank while it was sent; the Mac still expects its bytes.
                send(&end_session(session, 1))?;
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} changed while it was being sent", it.name)));
            }
            send(&file_data(session, &buf[..n]))?;
            left -= n as u64;
            total += n as u64;
            sent(total);
        }
    }
    send(&end_session(session, 0))
}

// ----- the Mac's replies ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Progress { session: u32, bytes: u64 },
    Done { session: u32, status: u16, name: String },
    /// A record of a file the Mac is sending (commands 100 to 104), whole.
    Record { session: u32, msg: Vec<u8> },
}

/// Joins a 0x22 message that may span several records.
#[derive(Default)]
pub struct Reader {
    buf: Vec<u8>,
    expected: usize,
}

impl Reader {
    pub fn in_progress(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Feed a record that is a 0x22 message or, while one is in progress, its
    /// continuation. Returns the whole message once complete.
    pub fn feed(&mut self, m: &[u8]) -> Option<Vec<u8>> {
        if self.buf.is_empty() {
            if m.len() < 6 || m[0] != MSG {
                return None;
            }
            self.expected = 6 + u32::from_be_bytes(m[2..6].try_into().unwrap()) as usize;
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

pub fn parse_reply(m: &[u8]) -> Option<Reply> {
    if m.len() < 14 || m[0] != MSG {
        return None;
    }
    let cmd = u16::from_be_bytes([m[8], m[9]]);
    let session = u32::from_be_bytes(m[10..14].try_into().ok()?);
    match cmd {
        CMD_PROGRESS => Some(Reply::Progress { session, bytes: u64::from_be_bytes(m.get(14..22)?.try_into().ok()?) }),
        CMD_END_STATUS => {
            let status = u16::from_be_bytes(m.get(14..16)?.try_into().ok()?);
            let n = u16::from_be_bytes(m.get(16..18)?.try_into().ok()?) as usize;
            let name = String::from_utf8_lossy(m.get(18..18 + n)?).into_owned();
            Some(Reply::Done { session, status, name })
        }
        CMD_ITEM_INFO..=CMD_END_SESSION => Some(Reply::Record { session, msg: m.to_vec() }),
        _ => None,
    }
}

/// Sessions in flight, so the connection's reader can hand each reply to the
/// transfer waiting for it.
#[derive(Default)]
pub struct Transfers {
    next: AtomicU32,
    waiting: Mutex<HashMap<u32, Sender<Reply>>>,
}

impl Transfers {
    fn begin(&self) -> (u32, Receiver<Reply>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = channel();
        self.waiting.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    fn end(&self, id: u32) {
        self.waiting.lock().unwrap().remove(&id);
    }

    pub fn dispatch(&self, r: Reply) {
        let (Reply::Progress { session, .. } | Reply::Done { session, .. } | Reply::Record { session, .. }) = &r;
        if let Some(tx) = self.waiting.lock().unwrap().get(session) {
            let _ = tx.send(r);
        }
    }
}

// ----- driving a drop ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Sending { name: String, sent: u64, total: u64 },
    Done { name: String },
    /// A file or folder is arriving from the Mac.
    Receiving { name: String, got: u64, total: u64 },
    /// It arrived, saved as `saved` (a path on this PC).
    Received { saved: PathBuf },
    Failed { name: String, reason: String },
}

fn refusal(status: u16) -> String {
    match status {
        3 => "The Mac could not find or open the destination folder.".into(),
        n => format!("The Mac could not save it (code {n})."),
    }
}

/// Copy each of `paths` to `dest` on the Mac, one after the other, reporting as it
/// goes. `send` writes one message to the connection.
pub fn send_paths(paths: &[PathBuf], dest: &str, transfers: &Transfers, send: &mut Sink, report: &dyn Fn(Progress), cancelled: &dyn Fn() -> bool) {
    for path in paths {
        if cancelled() {
            return;
        }
        let shown = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let fail = |reason: String| report(Progress::Failed { name: shown.clone(), reason });
        let job = match plan(path) {
            Ok(j) => j,
            Err(e) => {
                fail(format!("Could not read it: {e}"));
                continue;
            }
        };
        let (session, replies) = transfers.begin();
        let total = job.bytes;
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut sent = |n: u64| {
            if last.elapsed() >= Duration::from_millis(150) || n == total {
                last = Instant::now();
                report(Progress::Sending { name: job.name.clone(), sent: n, total });
            }
        };
        report(Progress::Sending { name: job.name.clone(), sent: 0, total });
        let streamed = stream(&job, session, dest, send, &mut sent, cancelled);
        let outcome = match streamed {
            Err(e) => Err(format!("Sending failed: {e}")),
            Ok(()) => {
                let mut result = Err("The Mac did not confirm that it saved the copy.".to_string());
                let mut deadline = Instant::now() + CONFIRM;
                while let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                    match replies.recv_timeout(wait.min(Duration::from_millis(500))) {
                        Ok(Reply::Done { status: 0, .. }) => {
                            result = Ok(());
                            break;
                        }
                        Ok(Reply::Done { status, .. }) => {
                            result = Err(refusal(status));
                            break;
                        }
                        Ok(Reply::Progress { .. } | Reply::Record { .. }) => deadline = Instant::now() + CONFIRM,
                        Err(RecvTimeoutError::Timeout) => {
                            if cancelled() {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                result
            }
        };
        transfers.end(session);
        match outcome {
            Ok(()) => report(Progress::Done { name: job.name.clone() }),
            Err(reason) => fail(reason),
        }
    }
}

// ----- fetching from the Mac ---------------------------------------------------------------

/// Largest zlib-compressed chunk we agree to inflate.
const MAX_CHUNK: usize = 16 << 20;

/// What a classic Mac OS error number from the sender means, in words.
fn mac_error(code: i32) -> String {
    match code {
        -43 | -120 => "The Mac could not find that file or folder. Check the path, including capital letters.".into(),
        -54 | -5000 | -5043 => "The Mac refused to read it. macOS may be blocking Screen Sharing from that folder (System Settings > Privacy & Security > Files and Folders or Full Disk Access).".into(),
        -61 => "The Mac says that item is not readable.".into(),
        n => format!("The Mac stopped sending (error {n})."),
    }
}

/// A name from the Mac that is safe to create inside a folder here.
fn safe_name(raw: &[u8]) -> io::Result<String> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("the Mac sent an unusable file name ({why})"));
    let n = std::str::from_utf8(raw).map_err(|_| bad("not UTF-8"))?;
    if n.is_empty() || n == "." || n == ".." || n.contains(['/', '\0']) {
        return Err(bad("path separator or empty"));
    }
    Ok(n.to_owned())
}

/// `dir/name`, or `dir/name 2`, `dir/name 3`… (before the extension) if that exists.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if std::fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..).map(|n| dir.join(format!("{stem} {n}{ext}"))).find(|p| std::fs::symlink_metadata(p).is_err()).unwrap()
}

struct Open {
    file: std::fs::File,
    data_left: u64,
    rsrc_left: u64,
    mode: u16,
    modified: SystemTime,
}

/// The files the Mac sends for one session, written below `dest`.
struct Incoming {
    dest: PathBuf,
    /// The folders of the nesting levels currently open.
    dirs: Vec<PathBuf>,
    cur: Option<Open>,
    /// What the top-level item was saved as.
    top: Option<PathBuf>,
    total: u64,
    got: u64,
    inflate: Option<Decompress>,
}

fn u16_be(m: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(m.get(o..o + 2)?.try_into().ok()?))
}
fn u32_be(m: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(m.get(o..o + 4)?.try_into().ok()?))
}
fn u64_be(m: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_be_bytes(m.get(o..o + 8)?.try_into().ok()?))
}

impl Incoming {
    fn new(dest: PathBuf) -> Self {
        Self { dest, dirs: vec![], cur: None, top: None, total: 0, got: 0, inflate: None }
    }

    /// Handle one record; true once the session has ended.
    fn record(&mut self, m: &[u8]) -> io::Result<bool> {
        let short = || io::Error::new(io::ErrorKind::InvalidData, "a record from the Mac was cut short");
        match u16_be(m, 8).unwrap_or(0) {
            CMD_ITEM_INFO => self.total = u64_be(m, 22).ok_or_else(short)?,
            CMD_NEW_ITEM => self.new_item(m)?,
            CMD_FILE_DATA => {
                let n = u32_be(m, 14).ok_or_else(short)? as usize;
                self.data(m.get(18..18 + n).ok_or_else(short)?)?;
            }
            CMD_COMPRESSED => {
                let (size, packed) = (u32_be(m, 16).ok_or_else(short)? as usize, u32_be(m, 20).ok_or_else(short)? as usize);
                if size > MAX_CHUNK || packed > MAX_CHUNK {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "a compressed block from the Mac is too large"));
                }
                let out = self.inflate(m.get(24..24 + packed).ok_or_else(short)?, size)?;
                self.data(&out)?;
            }
            CMD_END_SESSION => {
                // The Mac's helper writes this one field in its own (little-endian) byte
                // order, as a classic Mac OS error number.
                let code = m.get(14..18).map_or(0, |b| i32::from_le_bytes(b.try_into().unwrap()));
                if code != 0 {
                    let kind = if matches!(code, -43 | -120) { io::ErrorKind::NotFound } else { io::ErrorKind::Other };
                    return Err(io::Error::new(kind, mac_error(code)));
                }
                if self.cur.is_some() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the Mac ended the transfer before the last file was complete"));
                }
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn inflate(&mut self, packed: &[u8], size: usize) -> io::Result<Vec<u8>> {
        let bad = |e: &dyn std::fmt::Display| io::Error::new(io::ErrorKind::InvalidData, format!("a compressed block from the Mac is damaged ({e})"));
        let z = self.inflate.get_or_insert_with(|| Decompress::new(true));
        let start = z.total_in();
        let mut out = Vec::with_capacity(size);
        loop {
            let used = (z.total_in() - start) as usize;
            let before = out.len();
            z.decompress_vec(&packed[used..], &mut out, FlushDecompress::Sync).map_err(|e| bad(&e))?;
            if out.len() >= size || ((z.total_in() - start) as usize == used && out.len() == before) {
                break;
            }
            out.reserve(size - out.len());
        }
        if out.len() != size {
            return Err(bad(&"wrong size"));
        }
        Ok(out)
    }

    fn new_item(&mut self, m: &[u8]) -> io::Result<()> {
        let short = || io::Error::new(io::ErrorKind::InvalidData, "a record from the Mac was cut short");
        if self.cur.is_some() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the Mac started a new file before finishing the last"));
        }
        let kind = *m.get(14).ok_or_else(short)?;
        let rsrc = u64_be(m, 0x30).ok_or_else(short)?;
        let data = u64_be(m, 0x38).ok_or_else(short)?;
        let level = u16_be(m, 0x6a).ok_or_else(short)? as usize;
        let mode = u16_be(m, 0x6c).ok_or_else(short)?;
        let nlen = u16_be(m, 0x72).ok_or_else(short)? as usize;
        let name = safe_name(m.get(0x76..0x76 + nlen).ok_or_else(short)?)?;
        let modified = match (u16_be(m, 0x48), u32_be(m, 0x4a)) {
            (Some(hi), Some(lo)) => {
                let secs = (u64::from(hi) << 32 | u64::from(lo)).saturating_sub(MAC_EPOCH);
                UNIX_EPOCH + Duration::from_secs(secs)
            }
            _ => SystemTime::now(),
        };
        if level > self.dirs.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "the Mac sent an item outside any folder"));
        }
        self.dirs.truncate(level);
        let parent = if level == 0 { self.dest.clone() } else { self.dirs[level - 1].clone() };
        let path = if level == 0 { unique(&parent, &name) } else { parent.join(&name) };
        match kind {
            KIND_FOLDER => {
                std::fs::create_dir(&path)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(u32::from(mode & 0o777 | 0o700)))?;
                self.dirs.push(path.clone());
            }
            KIND_FILE => {
                let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
                let open = Open { file, data_left: data, rsrc_left: rsrc, mode, modified };
                if data == 0 && rsrc == 0 {
                    Self::finish(open);
                } else {
                    self.cur = Some(open);
                }
            }
            _ => return Ok(()), // links and anything else are not copied
        }
        if level == 0 {
            self.top = Some(path);
        }
        Ok(())
    }

    fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        self.got += bytes.len() as u64;
        let Some(o) = &mut self.cur else { return Ok(()) };
        let n = (bytes.len() as u64).min(o.data_left) as usize;
        o.file.write_all(&bytes[..n])?;
        o.data_left -= n as u64;
        // A resource fork (Mac-only metadata) follows the data fork; it is dropped.
        o.rsrc_left = o.rsrc_left.saturating_sub((bytes.len() - n) as u64);
        if o.data_left == 0 && o.rsrc_left == 0 {
            Self::finish(self.cur.take().unwrap());
        }
        Ok(())
    }

    fn finish(o: Open) {
        let _ = o.file.set_modified(o.modified);
        let mode = if o.mode == 0 { 0o644 } else { u32::from(o.mode & 0o777) | 0o600 };
        let _ = o.file.set_permissions(std::fs::Permissions::from_mode(mode));
    }
}

/// One try at fetching `path`; `Err((reason, not_found))`.
fn fetch_one(path: &str, dest: &Path, transfers: &Transfers, send: &mut Sink, report: &dyn Fn(Progress), cancelled: &dyn Fn() -> bool, shown: &str) -> Result<PathBuf, (String, bool)> {
    let (session, replies) = transfers.begin();
    let mut incoming = Incoming::new(dest.to_owned());
    let mut last = Instant::now();
    let outcome: Result<(), (String, bool)> = (|| {
        send(&start_send(session, path)).map_err(|e| (format!("Sending the request failed: {e}"), false))?;
        let mut deadline = Instant::now() + CONFIRM;
        loop {
            if cancelled() {
                return Err(("Cancelled.".into(), false));
            }
            let Some(wait) = deadline.checked_duration_since(Instant::now()) else {
                return Err(("The Mac did not send it. Check that the path exists and that you can read it.".into(), false));
            };
            match replies.recv_timeout(wait.min(Duration::from_millis(500))) {
                Ok(Reply::Record { msg, .. }) => {
                    deadline = Instant::now() + CONFIRM;
                    match incoming.record(&msg) {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(e) => return Err((e.to_string(), e.kind() == io::ErrorKind::NotFound)),
                    }
                    if last.elapsed() >= Duration::from_millis(150) {
                        last = Instant::now();
                        report(Progress::Receiving { name: shown.to_owned(), got: incoming.got, total: incoming.total });
                    }
                }
                Ok(Reply::Done { status, .. }) if status != 0 => return Err((refusal(status), false)),
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(("The connection closed.".into(), false)),
            }
        }
    })();
    transfers.end(session);
    match outcome {
        Ok(()) => incoming.top.take().ok_or_else(|| ("The Mac sent nothing (links are not copied).".to_string(), false)),
        Err(e) => {
            let _ = send(&stop(session));
            drop(incoming.cur.take());
            if let Some(top) = incoming.top.take() {
                // Do not leave half a copy behind.
                let _ = if top.is_dir() { std::fs::remove_dir_all(&top) } else { std::fs::remove_file(&top) };
            }
            Err(e)
        }
    }
}

/// Fetch one file or folder from the Mac into the folder `dest` on this PC,
/// trying each of `candidates` (paths on the Mac) until the Mac finds one.
pub fn get_path(candidates: &[String], dest: &Path, transfers: &Transfers, send: &mut Sink, report: &dyn Fn(Progress), cancelled: &dyn Fn() -> bool) {
    let Some(first) = candidates.first() else { return };
    let shown = first.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(first).to_owned();
    let fail = |reason: String| report(Progress::Failed { name: shown.clone(), reason });
    if let Err(e) = std::fs::create_dir_all(dest) {
        return fail(format!("Could not use the folder {}: {e}", dest.display()));
    }
    report(Progress::Receiving { name: shown.clone(), got: 0, total: 0 });
    let mut last_error = String::new();
    for (i, path) in candidates.iter().enumerate() {
        match fetch_one(path, dest, transfers, send, report, cancelled, &shown) {
            Ok(saved) => return report(Progress::Received { saved }),
            Err((reason, true)) if i + 1 < candidates.len() => last_error = reason,
            Err((reason, _)) => {
                return fail(if candidates.len() > 1 { format!("Not found in {}. {reason}", candidates.iter().map(|c| c.rsplit_once('/').map_or("", |(d, _)| d)).collect::<Vec<_>>().join(", ")) } else { reason });
            }
        }
    }
    fail(last_error);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(m: &[u8], o: usize) -> u16 {
        u16::from_be_bytes([m[o], m[o + 1]])
    }
    fn u32_at(m: &[u8], o: usize) -> u32 {
        u32::from_be_bytes(m[o..o + 4].try_into().unwrap())
    }
    fn u64_at(m: &[u8], o: usize) -> u64 {
        u64::from_be_bytes(m[o..o + 8].try_into().unwrap())
    }

    /// The length field counts everything after the 6-byte header.
    fn framed(m: &[u8]) {
        assert_eq!(m[0], MSG);
        assert_eq!(m[1], 0);
        assert_eq!(u32_at(m, 2) as usize, m.len() - 6);
    }

    #[test]
    fn start_message_matches_the_daemon_layout() {
        let m = start_receive(7, "~/Desktop");
        framed(&m);
        assert_eq!((u16_at(&m, 6), u16_at(&m, 8), u32_at(&m, 10)), (1, 2, 7));
        assert_eq!(u32_at(&m, 14), 0, "flags must be clear");
        assert_eq!(u16_at(&m, 22), 9);
        assert_eq!(&m[24..33], b"~/Desktop");
        assert_eq!(m[33], 0, "the daemon looks for a terminator at 24 + path length");
        assert_eq!(u32_at(&m, 2) as usize, 18 + 9 + 1);
    }

    #[test]
    fn item_info_is_sixty_bytes_of_body() {
        let m = item_info(1, 1000, 3, 2);
        framed(&m);
        assert_eq!(u32_at(&m, 2), 0x3c);
        assert_eq!((u64_at(&m, 22), u64_at(&m, 30), u64_at(&m, 38), u64_at(&m, 46), u64_at(&m, 54)), (1000, 1000, 3, 3, 2));
    }

    #[test]
    fn new_item_fields_sit_where_the_receiver_reads_them() {
        let it = Item { path: "x".into(), name: "héllo.txt".into(), level: 3, kind: KIND_FILE, size: 12345, mode: 0o644, modified: UNIX_EPOCH + Duration::from_secs(1_700_000_000), accessed: UNIX_EPOCH };
        let m = new_item(9, &it);
        framed(&m);
        let name = "héllo.txt".as_bytes();
        assert_eq!(u16_at(&m, 8), 101);
        assert_eq!(m[14], KIND_FILE);
        assert_eq!(u64_at(&m, 0x30), 0, "resource fork");
        assert_eq!(u64_at(&m, 0x38), 12345, "data fork");
        let secs = 1_700_000_000u64 + MAC_EPOCH;
        assert_eq!(u16_at(&m, 0x40), (secs >> 32) as u16);
        assert_eq!(u32_at(&m, 0x42), secs as u32);
        assert_eq!(u16_at(&m, 0x6a), 3, "level");
        assert_eq!(u16_at(&m, 0x6c), 0o644, "mode");
        assert_eq!(u16_at(&m, 0x72) as usize, name.len());
        assert_eq!(&m[0x76..0x76 + name.len()], name);
        assert_eq!(m[0x76 + name.len()], 0);
        // The receiver wants at least 0x70 + name length bytes after the 6-byte header,
        // and treats anything past the name and its terminator as extended attributes.
        assert_eq!(m.len() - 6, 0x70 + name.len() + 1);
    }

    #[test]
    fn data_and_end_messages() {
        let m = file_data(2, b"abc");
        framed(&m);
        assert_eq!((u16_at(&m, 8), u32_at(&m, 14), &m[18..]), (102, 3, &b"abc"[..]));
        assert!(u32_at(&m, 2) >= 12);
        let e = end_session(2, 0);
        framed(&e);
        assert_eq!((u16_at(&e, 8), e.len() - 6), (104, 12));
    }

    #[test]
    fn mac_replies_parse() {
        let mut p = vec![MSG, 0, 0, 0, 0, 16, 0, 1, 0x01, 0x2c, 0, 0, 0, 5];
        p.extend_from_slice(&777u64.to_be_bytes());
        assert_eq!(parse_reply(&p), Some(Reply::Progress { session: 5, bytes: 777 }));

        let name = b"a.txt";
        let mut d = vec![MSG, 0];
        d.extend_from_slice(&(13 + name.len() as u32).to_be_bytes());
        d.extend_from_slice(&[0, 1, 0, 200, 0, 0, 0, 5, 0, 0]);
        d.extend_from_slice(&(name.len() as u16).to_be_bytes());
        d.extend_from_slice(name);
        d.push(0);
        assert_eq!(parse_reply(&d), Some(Reply::Done { session: 5, status: 0, name: "a.txt".into() }));
        assert_eq!(parse_reply(&[MSG, 0, 0]), None);
    }

    #[test]
    fn reader_joins_split_messages() {
        let mut p = vec![MSG, 0, 0, 0, 0, 16, 0, 1, 0x01, 0x2c, 0, 0, 0, 5];
        p.extend_from_slice(&1u64.to_be_bytes());
        let mut r = Reader::default();
        assert!(r.feed(&p[..10]).is_none());
        assert!(r.in_progress());
        assert_eq!(r.feed(&p[10..]).as_deref(), Some(&p[..]));
        assert!(!r.in_progress());
        assert!(r.feed(&[0x14, 0, 0]).is_none());
    }

    #[test]
    fn a_folder_is_sent_parents_first_and_files_whole() {
        let dir = std::env::temp_dir().join(format!("seemymac-fc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("sub")).unwrap();
        std::fs::write(top.join("a.bin"), vec![7u8; 70_000]).unwrap();
        std::fs::write(top.join("sub").join("empty"), b"").unwrap();
        std::fs::write(top.join("sub").join("b.txt"), b"hello").unwrap();

        let transfers = Transfers::default();
        let log = Mutex::new(Vec::<Vec<u8>>::new());
        let events = Mutex::new(Vec::<Progress>::new());
        let (id, _) = (1u32, ());
        // The Mac confirms as soon as the end-of-session message arrives.
        let mut send = |m: &[u8]| {
            log.lock().unwrap().push(m.to_vec());
            if u16_at(m, 8) == 104 {
                transfers.dispatch(Reply::Done { session: id, status: 0, name: String::new() });
            }
            Ok(())
        };
        send_paths(&[top.clone()], "~", &transfers, &mut send, &|p| events.lock().unwrap().push(p), &|| false);

        let log = log.into_inner().unwrap();
        let cmds: Vec<u16> = log.iter().map(|m| u16_at(m, 8)).collect();
        // start, info, top, a.bin + 3 data chunks, sub, b.txt + data, empty, end
        assert_eq!(cmds, [2, 100, 101, 101, 102, 102, 102, 101, 101, 102, 101, 104]);
        let items: Vec<(u8, u16)> = log.iter().filter(|m| u16_at(m, 8) == 101).map(|m| (m[14], u16_at(m, 0x6a))).collect();
        assert_eq!(items, [(2, 0), (1, 1), (2, 1), (1, 2), (1, 2)]);
        let sent: usize = log.iter().filter(|m| u16_at(m, 8) == 102).map(|m| u32_at(m, 14) as usize).sum();
        assert_eq!(sent, 70_005);
        assert!(log.iter().all(|m| m.len() <= 32_768));
        assert_eq!(events.into_inner().unwrap().last(), Some(&Progress::Done { name: "top".into() }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refusal_is_reported() {
        let dir = std::env::temp_dir().join(format!("seemymac-fc-refuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("one.txt");
        std::fs::write(&f, b"x").unwrap();
        let transfers = Transfers::default();
        let events = Mutex::new(Vec::<Progress>::new());
        let mut send = |m: &[u8]| {
            if u16_at(m, 8) == 104 {
                transfers.dispatch(Reply::Done { session: 1, status: 3, name: String::new() });
            }
            Ok(())
        };
        send_paths(&[f], "/nope", &transfers, &mut send, &|p| events.lock().unwrap().push(p), &|| false);
        assert!(matches!(events.into_inner().unwrap().last(), Some(Progress::Failed { reason, .. }) if reason.contains("destination")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_paths_are_reported_not_fatal() {
        let transfers = Transfers::default();
        let events = Mutex::new(Vec::<Progress>::new());
        let mut send = |_: &[u8]| Ok(());
        send_paths(&["/definitely/not/here".into()], "~", &transfers, &mut send, &|p| events.lock().unwrap().push(p), &|| false);
        assert!(matches!(events.into_inner().unwrap().as_slice(), [Progress::Failed { .. }]));
    }
    #[test]
    fn the_senders_error_code_is_little_endian() {
        let mut m = end_session(1, 0);
        m[14..18].copy_from_slice(&(-43i32).to_le_bytes());
        let mut inc = Incoming::new(std::env::temp_dir());
        let e = inc.record(&m).unwrap_err().to_string();
        assert!(e.contains("could not find"), "{e}");
    }

    #[test]
    fn start_send_asks_to_start_at_once() {
        let m = start_send(4, "/Users/me/a.txt");
        framed(&m);
        assert_eq!((u16_at(&m, 8), u32_at(&m, 14)), (1, 1));
        assert_eq!(u16_at(&m, 22), 15);
        assert_eq!(&m[24..39], b"/Users/me/a.txt");
    }

    #[test]
    fn names_from_the_mac_cannot_escape() {
        for bad in ["", ".", "..", "a/b", "../x", "a\0b"] {
            assert!(safe_name(bad.as_bytes()).is_err(), "{bad:?}");
        }
        assert_eq!(safe_name("ok ✓.txt".as_bytes()).unwrap(), "ok ✓.txt");
    }

    /// What the Mac's sender produces: a folder with a plain and a compressed file.
    #[test]
    fn a_folder_from_the_mac_is_written_out() {
        use flate2::{Compress, Compression, FlushCompress};
        let dir = std::env::temp_dir().join(format!("seemymac-get-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir(dir.join("Pics")).unwrap(); // forces "Pics 2"
        let item = |name: &str, level: u16, kind: u8, size: u64, mode: u16| {
            new_item(3, &Item { path: "".into(), name: name.into(), level, kind, size, modified: UNIX_EPOCH + Duration::from_secs(1_600_000_000), accessed: UNIX_EPOCH, mode })
        };
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut z = Compress::new(Compression::default(), true);
        let mut packed = Vec::with_capacity(big.len() + 1024);
        z.compress_vec(&big, &mut packed, FlushCompress::Sync).unwrap();
        let mut comp = header(CMD_COMPRESSED, 3, 4 + 4 + 2 + packed.len());
        comp.extend_from_slice(&1u16.to_be_bytes());
        comp.extend_from_slice(&(big.len() as u32).to_be_bytes());
        comp.extend_from_slice(&(packed.len() as u32).to_be_bytes());
        comp.extend_from_slice(&packed);

        let mut inc = Incoming::new(dir.clone());
        let records = [
            item_info(3, 100_005, 2, 1),
            item("Pics", 0, KIND_FOLDER, 0, 0o755),
            item("small.txt", 1, KIND_FILE, 5, 0o640),
            file_data(3, b"hello"),
            item("big.bin", 1, KIND_FILE, big.len() as u64, 0),
            comp,
            item("empty", 1, KIND_FILE, 0, 0o600),
        ];
        for r in &records {
            assert!(!inc.record(r).unwrap());
        }
        assert!(inc.record(&end_session(3, 0)).unwrap());
        let top = inc.top.clone().unwrap();
        assert_eq!(top, dir.join("Pics 2"));
        assert_eq!(std::fs::read(top.join("small.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(top.join("big.bin")).unwrap(), big);
        assert_eq!(std::fs::metadata(top.join("empty")).unwrap().len(), 0);
        let mode = |n: &str| std::fs::metadata(top.join(n)).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode("small.txt"), mode("big.bin")), (0o640, 0o644));
        let m = std::fs::metadata(top.join("small.txt")).unwrap().modified().unwrap();
        assert_eq!(m, UNIX_EPOCH + Duration::from_secs(1_600_000_000));
        assert_eq!(inc.total, 100_005);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_path_reports_the_saved_location() {
        let dir = std::env::temp_dir().join(format!("seemymac-get2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let transfers = Transfers::default();
        let events = Mutex::new(Vec::<Progress>::new());
        let mut send = |m: &[u8]| {
            if u16_at(m, 8) == 1 && m[24..].starts_with(b"/Users/me/missing") {
                let sid = u32_at(m, 10);
                let mut e = end_session(sid, 0);
                e[14..18].copy_from_slice(&(-43i32).to_le_bytes());
                transfers.dispatch(parse_reply(&e).unwrap());
            } else if u16_at(m, 8) == 1 {
                let it = Item { path: "".into(), name: "r.txt".into(), level: 0, kind: KIND_FILE, size: 2, modified: UNIX_EPOCH, accessed: UNIX_EPOCH, mode: 0o644 };
                let sid = u32_at(m, 10);
                for rec in [item_info(sid, 2, 1, 0), new_item(sid, &it), file_data(sid, b"hi"), end_session(sid, 0)] {
                    transfers.dispatch(parse_reply(&rec).unwrap());
                }
            }
            Ok(())
        };
        get_path(&["/Users/me/missing/r.txt".to_string(), "/Users/me/r.txt".to_string()], &dir.join("dl"), &transfers, &mut send, &|p| events.lock().unwrap().push(p), &|| false);
        assert_eq!(events.into_inner().unwrap().last(), Some(&Progress::Received { saved: dir.join("dl").join("r.txt") }));
        assert_eq!(std::fs::read(dir.join("dl").join("r.txt")).unwrap(), b"hi");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
