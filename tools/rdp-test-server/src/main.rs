//! A minimal RDP server for testing the client end to end.
//!
//!   cargo run --manifest-path tools/rdp-test-server/Cargo.toml -- [PORT] [USER] [PASSWORD]
//!
//! It requires NLA (CredSSP) like Windows does, with a throwaway self-signed
//! certificate. The desktop is 640x480 with a moving square; every keyboard and
//! mouse event it receives is printed, one per line, for the probe to check.

use ironrdp_cliprdr::backend::{CliprdrBackend, CliprdrBackendFactory, ClipboardMessage};
use ironrdp_cliprdr::pdu::{ClipboardFormatName, FileContentsFlags, FileDescriptor, ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId};
use ironrdp_server::tokio::time::{sleep, Duration};
use ironrdp_server::{
    BitmapUpdate, CliprdrServerFactory, Credentials, DesktopSize, DisplayUpdate, KeyboardEvent, MouseEvent, PixelFormat, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerEvent, ServerEventSender, TlsIdentityCtx,
};
use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler, RdpsndServerMessage};
use ironrdp_server::SoundServerFactory;
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex};

const W: u16 = 640;
const H: u16 = 480;

/// The desktop size, changed by the client's display-control layout requests.
type Size = Arc<Mutex<(u16, u16)>>;

#[derive(Clone)]
struct Handler {
    size: Size,
}

impl RdpServerInputHandler for Handler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        println!("KEY {event:?}");
    }

    fn mouse(&mut self, event: MouseEvent) {
        println!("MOUSE {event:?}");
    }
}

struct Updates {
    tick: u32,
    size: Size,
    current: (u16, u16),
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        sleep(Duration::from_millis(100)).await;
        self.tick += 1;
        let (w, h) = *self.size.lock().unwrap();
        if (w, h) != self.current {
            self.current = (w, h);
            println!("RESIZE {w}x{h}");
            return Ok(Some(DisplayUpdate::Resize(DesktopSize { width: w, height: h })));
        }
        let (ww, hh) = (w as usize, h as usize);
        // Whole-screen update: a fixed background and a square that moves.
        let sq = ((self.tick * 8) % (ww as u32).saturating_sub(40).max(1)) as usize;
        let mut data = Vec::with_capacity(ww * hh * 4);
        for y in 0..hh {
            for x in 0..ww {
                let in_sq = x >= sq && x < sq + 40 && (100..140).contains(&y);
                if in_sq {
                    data.extend_from_slice(&[0x00, 0xff, 0x00, 0xff]); // BGRA: green
                } else {
                    data.extend_from_slice(&[0x30, 0x20, 0x10, 0xff]); // BGRA: b=0x30 g=0x20 r=0x10
                }
            }
        }
        Ok(Some(DisplayUpdate::Bitmap(BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(w).unwrap(),
            height: NonZeroU16::new(h).unwrap(),
            format: PixelFormat::BgrA32,
            data: data.into(),
            stride: NonZeroUsize::new(ww * 4).unwrap(),
        })))
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for Handler {
    async fn size(&mut self) -> DesktopSize {
        let (width, height) = *self.size.lock().unwrap();
        DesktopSize { width, height }
    }

    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        let current = *self.size.lock().unwrap();
        Ok(Box::new(Updates { tick: 0, size: self.size.clone(), current }))
    }

    fn request_layout(&mut self, layout: ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout) {
        if let Some(m) = layout.monitors().iter().find(|m| m.is_primary()).or_else(|| layout.monitors().first()) {
            let (w, h) = m.dimensions();
            println!("LAYOUT {} monitor(s), primary {w}x{h}", layout.monitors().len());
            *self.size.lock().unwrap() = (w as u16, h as u16);
        }
    }
}

type Tx = Arc<Mutex<Option<ironrdp_server::tokio::sync::mpsc::UnboundedSender<ServerEvent>>>>;

const SERVER_TEXT: &str = "hello from the server";

/// Offers SERVER_TEXT as its clipboard, and prints any text the client copies.
struct Clip {
    tx: Tx,
}

impl CliprdrBackendFactory for Clip {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(ClipBackend { tx: self.tx.clone(), fetch: Vec::new(), cur: 0, data_id: None, stream: 1 })
    }
}

impl ServerEventSender for Clip {
    fn set_sender(&mut self, sender: ironrdp_server::tokio::sync::mpsc::UnboundedSender<ServerEvent>) {
        *self.tx.lock().unwrap() = Some(sender);
    }
}

impl CliprdrServerFactory for Clip {}

#[derive(Debug)]
struct ClipBackend {
    tx: Tx,
    /// Files the client offered: (descriptor, bytes so far, checksum), the one being read, the clip data id.
    fetch: Vec<(FileDescriptor, u64, u64)>,
    cur: usize,
    data_id: Option<u32>,
    stream: u32,
}

const OFFER_SIZE: u64 = 600_000;

impl ClipBackend {
    fn request_next(&mut self) {
        let Some((f, got, _)) = self.fetch.get(self.cur) else { return };
        let remaining = f.file_size.unwrap_or(0).saturating_sub(*got);
        self.stream += 1;
        let req = FileContentsRequest { stream_id: self.stream, index: self.cur as i32, flags: FileContentsFlags::RANGE, position: *got, requested_size: remaining.min(200_000) as u32, data_id: self.data_id };
        self.send(ClipboardMessage::SendFileContentsRequest(req));
    }

    fn send(&self, m: ClipboardMessage) {
        if let Some(t) = self.tx.lock().unwrap().as_ref() {
            let _ = t.send(ServerEvent::Clipboard(m));
        }
    }
}

ironrdp_core::impl_as_any!(ClipBackend);

impl CliprdrBackend for ClipBackend {
    fn temporary_directory(&self) -> &str {
        ".clipboard"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
    }

    fn on_ready(&mut self) {
        if std::env::var_os("RDP_TEST_OFFER_FILE").is_some() {
            println!("CLIP channel ready; offering a server file");
            let mut d = FileDescriptor::new("from-server.bin");
            d.file_size = Some(OFFER_SIZE);
            self.send(ClipboardMessage::SendInitiateFileCopy(vec![d]));
            return;
        }
        println!("CLIP channel ready; offering server text");
        self.send(ClipboardMessage::SendInitiateCopy(vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]));
    }

    fn on_request_format_list(&mut self) {}

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        println!("CLIP client offers {} format(s)", formats.len());
        if let Some(f) = formats.iter().find(|f| f.name.as_ref() == Some(&ClipboardFormatName::FILE_LIST)) {
            self.send(ClipboardMessage::SendInitiatePaste(f.id()));
            return;
        }
        if formats.iter().any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT) {
            self.send(ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT));
        }
    }

    fn on_format_data_request(&mut self, req: FormatDataRequest) {
        let resp = if req.format == ClipboardFormatId::CF_UNICODETEXT {
            let mut b: Vec<u8> = SERVER_TEXT.encode_utf16().flat_map(u16::to_le_bytes).collect();
            b.extend_from_slice(&[0, 0]);
            FormatDataResponse::new_data(b)
        } else {
            FormatDataResponse::new_error()
        };
        self.send(ClipboardMessage::SendFormatData(ironrdp_core::IntoOwned::into_owned(resp)));
    }

    fn on_format_data_response(&mut self, resp: FormatDataResponse<'_>) {
        if resp.is_error() {
            println!("CLIP paste failed");
            return;
        }
        let units: Vec<u16> = resp.data().chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        println!("CLIP client text: {:?}", String::from_utf16_lossy(&units));
    }

    fn on_file_contents_request(&mut self, req: FileContentsRequest) {
        let resp = if req.flags.contains(FileContentsFlags::SIZE) {
            FileContentsResponse::new_size_response(req.stream_id, OFFER_SIZE)
        } else {
            let end = (req.position + u64::from(req.requested_size)).min(OFFER_SIZE);
            let data: Vec<u8> = (req.position..end).map(|i| (i % 251) as u8).collect();
            FileContentsResponse::new_data_response(req.stream_id, data)
        };
        self.send(ClipboardMessage::SendFileContentsResponse(ironrdp_core::IntoOwned::into_owned(resp)));
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        println!("CLIP client offers {} file(s)", files.len());
        self.fetch = files.iter().cloned().map(|f| (f, 0, 0)).collect();
        self.cur = 0;
        self.data_id = clip_data_id;
        self.request_next();
    }

    fn on_file_contents_response(&mut self, resp: FileContentsResponse<'_>) {
        if resp.is_error() {
            println!("FILE error");
            return;
        }
        let Some((f, got, sum)) = self.fetch.get_mut(self.cur) else { return };
        for b in resp.data() {
            *sum = sum.wrapping_mul(31).wrapping_add(u64::from(*b));
        }
        *got += resp.data().len() as u64;
        if *got >= f.file_size.unwrap_or(0) || resp.data().is_empty() {
            println!("FILE {} {} bytes checksum {}", f.name, got, sum);
            self.cur += 1;
        }
        self.request_next();
    }
    fn on_lock(&mut self, _: LockDataId) {}
    fn on_unlock(&mut self, _: LockDataId) {}
}

struct Snd {
    sender: Arc<Mutex<Option<ironrdp_server::tokio::sync::mpsc::UnboundedSender<ServerEvent>>>>,
}

impl ServerEventSender for Snd {
    fn set_sender(&mut self, sender: ironrdp_server::tokio::sync::mpsc::UnboundedSender<ServerEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }
}

impl SoundServerFactory for Snd {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(SndHandler { sender: self.sender.clone(), task: None })
    }
}

#[derive(Debug)]
struct SndHandler {
    sender: Arc<Mutex<Option<ironrdp_server::tokio::sync::mpsc::UnboundedSender<ServerEvent>>>>,
    task: Option<ironrdp_server::tokio::task::JoinHandle<()>>,
}

impl RdpsndServerHandler for SndHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &[AudioFormat { format: WaveFormat::PCM, n_channels: 2, n_samples_per_sec: 48000, n_avg_bytes_per_sec: 192000, n_block_align: 4, bits_per_sample: 16, data: None }]
    }

    fn choose_format<'a>(&mut self, common: &'a [NegotiatedFormat]) -> Option<&'a NegotiatedFormat> {
        println!("AUDIO client accepts {} common format(s)", common.len());
        common.first()
    }

    fn start(&mut self, _: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let sender = self.sender.clone();
        self.task = Some(ironrdp_server::tokio::spawn(async move {
            let mut tick = ironrdp_server::tokio::time::interval(Duration::from_millis(20));
            let (mut ts, mut phase) = (0u32, 0f32);
            println!("AUDIO streaming a 440 Hz tone");
            loop {
                tick.tick().await;
                let mut data = Vec::with_capacity(960 * 4);
                for _ in 0..960 {
                    let v = ((phase).sin() * 6000.0) as i16;
                    phase = (phase + 2.0 * std::f32::consts::PI * 440.0 / 48000.0) % (2.0 * std::f32::consts::PI);
                    data.extend_from_slice(&v.to_le_bytes());
                    data.extend_from_slice(&v.to_le_bytes());
                }
                if let Some(s) = sender.lock().unwrap().as_ref() {
                    let _ = s.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, ts)));
                }
                ts = ts.wrapping_add(20);
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

#[ironrdp_server::tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let port: u16 = a.next().and_then(|p| p.parse().ok()).unwrap_or(3390);
    let user = a.next().unwrap_or_else(|| "tester".into());
    let pass = a.next().unwrap_or_else(|| "secret".into());

    let dir = std::env::temp_dir().join(format!("rdp-test-server-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])?;
    let (cert_path, key_path) = (dir.join("cert.pem"), dir.join("key.pem"));
    std::fs::write(&cert_path, ck.cert.pem())?;
    std::fs::write(&key_path, ck.signing_key.serialize_pem())?;
    let identity = TlsIdentityCtx::init_from_paths(&cert_path, &key_path)?;
    let acceptor = identity.make_acceptor()?;

    let handler = Handler { size: Arc::new(Mutex::new((W, H))) };
    let mut server = RdpServer::builder()
        .with_addr(([127, 0, 0, 1], port))
        .with_hybrid(acceptor, identity.pub_key)
        .with_input_handler(handler.clone())
        .with_display_handler(handler)
        .with_cliprdr_factory(Some(Box::new(Clip { tx: Arc::new(Mutex::new(None)) })))
        .with_sound_factory(Some(Box::new(Snd { sender: Arc::new(Mutex::new(None)) })))
        .build();
    server.set_credentials(Some(Credentials { username: user.clone(), password: pass, domain: None }));
    println!("LISTENING 127.0.0.1:{port} user={user}");
    let r = server.run().await;
    let _ = std::fs::remove_dir_all(&dir);
    r
}
