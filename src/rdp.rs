//! RDP client, built on the IronRDP crates.
//!
//! One background thread per session (like [`crate::rfb`] and [`crate::hp`]) runs
//! a blocking connection: TCP, TLS, CredSSP/NLA, then the active session. The
//! decoded desktop is copied into the shared [`Framebuffer`]; keyboard and mouse
//! arrive as [`Input`]s. Static and dynamic channels attached here: clipboard
//! (text), audio playback, drive and printer redirection, and display control
//! (the remote desktop follows the window size).

use crate::rfb::{Closer, Event, Framebuffer, Handle, Input, Screen};
use anyhow::{anyhow, bail, Context, Result};
use ironrdp_blocking::Framed;
use ironrdp_cliprdr::backend::{CliprdrBackend, ClipboardMessage};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_cliprdr::{Cliprdr, CliprdrClient};
use ironrdp_connector::credssp::KerberosConfig;
use ironrdp_connector::{self as connector, ClientConnector, ConnectionOutcome, Credentials, DesktopSize};
use ironrdp_core::impl_as_any;
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_pdu::rdp::server_redirection::{ServerRedirectionFlags, ServerRedirectionPdu};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_pdu::gcc::KeyboardType;
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_rdpdr::Rdpdr;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ironrdp_rdpdr::backend::RdpdrBackendFactory;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ironrdp_rdpdr_native::backend::NixRdpdrBackendFactory;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ironrdp_rdpdr_native::printer::PrintTarget;
use ironrdp_rdpsnd::client::{Rdpsnd, RdpsndClientHandler};
use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{ErrorKind, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::hp::audio::PcmPlayer;
use crate::hp::filecopy::Progress;

type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// How long a read waits before the loop looks at the input queue again.
const POLL: Duration = Duration::from_millis(6);
/// Frames are copied out to the UI at most this often while updates stream in.
const PUBLISH_EVERY: Duration = Duration::from_millis(8);

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// `user`, `DOMAIN\user` or `user@domain`.
    pub username: String,
    pub password: String,
    /// Initial desktop size.
    pub width: u32,
    pub height: u32,
    /// A local folder offered to the remote machine as a drive.
    pub share_folder: Option<PathBuf>,
    /// Offer the local default printer (CUPS) to the remote machine.
    pub printer: bool,
    pub audio: bool,
    pub clipboard: bool,
    /// 16 or 32.
    pub color_depth: u32,
    /// Ask for several monitors (the local ones, to span); empty or one entry = a single desktop.
    pub monitors: Vec<MonitorRect>,
    /// Where files the server copies to its clipboard are saved.
    pub download_dir: PathBuf,
    /// Offer the local smart-card readers (PC/SC; Linux).
    pub smartcard: bool,
    /// The Windows printer driver the redirected printer asks for; it must exist on the server.
    pub printer_driver: String,
    /// Reach the host through this RD Gateway (`host` or `host:port`); it signs in with the same user and password.
    pub gateway: Option<String>,
    /// RemoteApp: run only this program (a path, or `||alias`) instead of the desktop.
    pub remote_app: Option<String>,
}

/// One monitor's place in the layout asked of the server (x and y may be negative).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Config {
    pub fn new(host: impl Into<String>, port: u16, username: impl Into<String>, password: impl Into<String>) -> Self {
        Self { host: host.into(), port, username: username.into(), password: password.into(), width: 1920, height: 1080, share_folder: None, printer: false, audio: true, clipboard: true, color_depth: 32, monitors: Vec::new(), smartcard: false, printer_driver: "Microsoft PS Class Driver".to_string(), gateway: None, remote_app: None, download_dir: dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(std::env::temp_dir) }
    }
}

/// Start an RDP session on a background thread.
pub fn connect(cfg: Config) -> Handle {
    let (ev_tx, ev_rx) = channel();
    let (in_tx, in_rx) = channel();
    let fb = Arc::new(Mutex::new(Framebuffer::default()));
    let closer: Closer = Arc::new(Mutex::new(None));
    let handle = Handle::from_parts(ev_rx, fb.clone(), in_tx, closer.clone());
    std::thread::Builder::new()
        .name(format!("rdp-{}", cfg.host))
        .spawn(move || {
            let stopping = Arc::new(AtomicBool::new(false));
            {
                let st = stopping.clone();
                *closer.lock().unwrap() = Some(Box::new(move || st.store(true, Ordering::SeqCst)));
            }
            let result = run(&cfg, &fb, &stopping, in_rx, &ev_tx);
            let reason = match result {
                Err(e) if !stopping.load(Ordering::SeqCst) => {
                    log::warn!("{}: RDP session failed: {e:#}", cfg.host);
                    Some(format!("{e:#}"))
                }
                Ok(()) if !stopping.load(Ordering::SeqCst) => Some("The remote session ended.".to_string()),
                _ => None,
            };
            let _ = ev_tx.send(Event::Disconnected(reason));
        })
        .expect("spawn RDP thread");
    handle
}

fn stage(ev: &Sender<Event>, s: &str) {
    let _ = ev.send(Event::Stage(s.to_string()));
}

/// `DOMAIN\user` and `user@domain` become (user, Some(domain)).
fn split_user(name: &str) -> (String, Option<String>) {
    if let Some((d, u)) = name.split_once('\\') {
        return (u.to_string(), Some(d.to_string()));
    }
    (name.to_string(), None)
}

/// The desktop size and monitor list for a multi-monitor request: the first monitor is the
/// primary and sits at (0, 0); the desktop is the box around all of them.
fn monitor_layout(mons: &[MonitorRect]) -> Option<(DesktopSize, ironrdp_pdu::gcc::ClientMonitorData)> {
    use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
    if mons.len() < 2 {
        return None;
    }
    let mons = &mons[..mons.len().min(16)];
    let (ox, oy) = (mons[0].x, mons[0].y);
    let rel: Vec<(i32, i32, i32, i32)> = mons.iter().map(|m| (m.x - ox, m.y - oy, m.x - ox + m.w.max(200) as i32 - 1, m.y - oy + m.h.max(200) as i32 - 1)).collect();
    let (l, t) = (rel.iter().map(|m| m.0).min()?, rel.iter().map(|m| m.1).min()?);
    let (r, b) = (rel.iter().map(|m| m.2).max()?, rel.iter().map(|m| m.3).max()?);
    let (w, h) = ((r - l + 1) as u32, (b - t + 1) as u32);
    if w > 8192 * 4 || h > 8192 * 2 {
        return None;
    }
    let monitors = rel.iter().enumerate().map(|(i, m)| Monitor { left: m.0, top: m.1, right: m.2, bottom: m.3, flags: if i == 0 { MonitorFlags::PRIMARY } else { MonitorFlags::empty() } }).collect();
    Some((DesktopSize { width: (w as u16) & !1, height: h as u16 }, ironrdp_pdu::gcc::ClientMonitorData { monitors }))
}

/// The monitors asked for, as places within the desktop (the box around them starts at 0, 0), for the
/// view's monitor list when the server does not report its own layout.
fn requested_screens(mons: &[MonitorRect]) -> Vec<Screen> {
    if mons.len() < 2 {
        return Vec::new();
    }
    let mons = &mons[..mons.len().min(16)];
    let (ox, oy) = (mons[0].x, mons[0].y);
    let rel: Vec<(i32, i32, u32, u32)> = mons.iter().map(|m| (m.x - ox, m.y - oy, m.w.max(200), m.h.max(200))).collect();
    let (l, t) = (rel.iter().map(|m| m.0).min().unwrap_or(0), rel.iter().map(|m| m.1).min().unwrap_or(0));
    rel.iter().map(|&(x, y, w, h)| Screen { x: (x - l) as u32, y: (y - t) as u32, w, h }).collect()
}

fn connector_config(cfg: &Config) -> connector::Config {
    let (username, domain) = split_user(&cfg.username);
    let multi = monitor_layout(&cfg.monitors);
    connector::Config {
        desktop_size: multi.as_ref().map(|m| m.0).unwrap_or(DesktopSize { width: cfg.width.clamp(200, 8192) as u16 & !1, height: cfg.height.clamp(200, 8192) as u16 }),
        monitor_layout: multi.map(|m| m.1),
        desktop_scale_factor: 0,
        desktop_physical_size: None,
        desktop_orientation: None,
        device_scale_factor: None,
        monitor_extended_layout: None,
        relative_mouse_input: false,
        enable_tls: true,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        credentials: Credentials::UsernamePassword { username, password: cfg.password.clone() },
        domain,
        client_build: 0,
        client_name: "SeeMyMac".to_owned(),
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        connection_type: ironrdp_pdu::gcc::ConnectionType::Lan,
        ime_file_name: String::new(),
        bitmap: Some(connector::BitmapConfig { color_depth: if cfg.color_depth == 16 { 16 } else { 32 }, lossy_compression: true, codecs: ironrdp_pdu::rdp::capability_sets::client_codecs_capabilities(&[]).unwrap_or_default() }),
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        // RemoteApp sessions run Windows' RAIL shell, which then launches the program over the rail channel.
        alternate_shell: if cfg.remote_app.is_some() { "rdpinit.exe".to_owned() } else { String::new() },
        work_dir: String::new(),
        remote_application_mode: cfg.remote_app.is_some(),
        rail_support_level: if cfg.remote_app.is_some() { ironrdp_pdu::rdp::capability_sets::RailSupportLevel::SUPPORTED } else { ironrdp_pdu::rdp::capability_sets::RailSupportLevel::empty() },
        // GNOME Remote Desktop refuses clients without the graphics pipeline (RDPGFX).
        support_dyn_vc_gfx_protocol: true,
        platform: MajorPlatformType::UNIX,
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: cfg.audio,
        enable_audio_capture: false,
        performance_flags: PerformanceFlags::default(),
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: None,
        // The window shows the local cursor; the remote one is not drawn into the picture.
        enable_server_pointer: false,
        pointer_software_rendering: false,
        multitransport_flags: None,
    }
}

// ----- TLS ------------------------------------------------------------------------

/// RDP servers present self-signed certificates by default, so the certificate is
/// not validated here (the same default as most RDP clients); NLA still
/// authenticates both sides through the TLS public key.
#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &rustls::pki_types::CertificateDer<'_>, d: &rustls::DigitallySignedStruct) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &rustls::pki_types::CertificateDer<'_>, d: &rustls::DigitallySignedStruct) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Upgrade the stream to TLS; also returns the server's public key (CredSSP binds to it).
fn tls_upgrade(stream: TcpStream, server_name: &str) -> Result<(Tls, Vec<u8>)> {
    use x509_cert::der::Decode as _;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
        .with_no_client_auth();
    // CredSSP cannot resume a TLS session.
    config.resumption = rustls::client::Resumption::disabled();
    let name = rustls::pki_types::ServerName::try_from(server_name.to_string()).map_err(|e| anyhow!("bad server name: {e}"))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)?;
    let mut tls = rustls::StreamOwned::new(conn, stream);
    // Flushing drives the handshake far enough to receive the certificate.
    tls.flush().context("TLS handshake")?;
    let cert = tls.conn.peer_certificates().and_then(|c| c.first()).context("the server sent no TLS certificate")?;
    let cert = x509_cert::Certificate::from_der(cert).map_err(|e| anyhow!("unreadable server certificate: {e}"))?;
    let key = cert.tbs_certificate().subject_public_key_info().subject_public_key.as_bytes().context("unaligned server public key")?.to_owned();
    Ok((tls, key))
}

/// Kerberos: CredSSP may need to reach the domain's KDC (TCP or UDP, port 88). KDC proxies over
/// HTTPS are not supported.
struct Kdc;

impl connector::sspi::network_client::NetworkClient for Kdc {
    fn send(&self, req: &connector::sspi::generator::NetworkRequest) -> connector::sspi::Result<Vec<u8>> {
        use connector::sspi::{network_client::NetworkProtocol, Error, ErrorKind};
        use std::io::Read;
        let fail = |m: String| Error::new(ErrorKind::NoAuthenticatingAuthority, m);
        let addr = format!("{}:{}", req.url.host_str().unwrap_or_default(), req.url.port().unwrap_or(88));
        let addrs: Vec<_> = addr.to_socket_addrs().map_err(|e| fail(format!("cannot resolve the KDC {addr}: {e}")))?.collect();
        match req.protocol {
            NetworkProtocol::Tcp => {
                let mut last = fail(format!("no address for the KDC {addr}"));
                for a in addrs {
                    match TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
                        Ok(mut s) => {
                            s.set_read_timeout(Some(Duration::from_secs(10))).ok();
                            s.write_all(&req.data).map_err(|e| fail(format!("KDC write: {e}")))?;
                            let mut len = [0u8; 4];
                            s.read_exact(&mut len).map_err(|e| fail(format!("KDC read: {e}")))?;
                            let n = u32::from_be_bytes(len) as usize;
                            if n > 1 << 20 {
                                return Err(fail("KDC reply too large".into()));
                            }
                            let mut buf = vec![0u8; n + 4];
                            buf[..4].copy_from_slice(&len);
                            s.read_exact(&mut buf[4..]).map_err(|e| fail(format!("KDC read: {e}")))?;
                            return Ok(buf);
                        }
                        Err(e) => last = fail(format!("cannot reach the KDC {a}: {e}")),
                    }
                }
                Err(last)
            }
            NetworkProtocol::Udp => {
                let sock = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| fail(format!("UDP: {e}")))?;
                sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let a = addrs.first().ok_or_else(|| fail(format!("no address for the KDC {addr}")))?;
                sock.send_to(&req.data, a).map_err(|e| fail(format!("KDC send: {e}")))?;
                let mut buf = vec![0u8; 0xbb80];
                let n = sock.recv(&mut buf).map_err(|e| fail(format!("KDC read: {e}")))?;
                let mut out = (n as u32).to_be_bytes().to_vec();
                out.extend_from_slice(&buf[..n]);
                Ok(out)
            }
            _ => Err(fail("Kerberos through an HTTPS KDC proxy is not supported".into())),
        }
    }
}

// ----- session ---------------------------------------------------------------------

/// One connection attempt: TCP, X.224 negotiation, TLS, CredSSP and the channel setup. `redirect`
/// carries what a Server Redirection PDU told the client to present at the new target.
fn dial(
    cfg: &Config,
    host: &str,
    port: u16,
    redirect: Option<&ironrdp_pdu::rdp::server_redirection::ServerRedirectionPdu>,
    ev: &Sender<Event>,
    clip_tx: &Sender<ClipboardMessage>,
    clip_state: &Arc<Mutex<ClipState>>,
) -> Result<(ConnectionOutcome, Framed<Tls>)> {
    stage(ev, &format!("Connecting to {host}…"));
    // Open the audio device alongside the connection: it can take seconds (WASAPI), and some
    // servers (GNOME Remote Desktop) hang up on a client that stays silent for about two.
    let audio = cfg.audio.then(open_audio);
    let tcp = match &cfg.gateway {
        // Through an RD Gateway: connect to the local end of its tunnel; the real host name is still
        // what TLS and sign-in use.
        Some(gw) => {
            stage(ev, &format!("Signing in to the gateway {gw}…"));
            let gw_user = cfg.username.clone();
            let addr = crate::rdp_gateway::start(gw, &gw_user, &cfg.password, host, "SeeMyMac")?;
            TcpStream::connect_timeout(&addr, Duration::from_secs(10)).with_context(|| format!("cannot connect to {addr}"))?
        }
        // Try every address the name resolves to (Tailscale, then IPv4, then IPv6).
        None => crate::dial::connect(host, port, |a| stage(ev, &format!("Connecting to {host} ({})…", a.ip())))?,
    };
    tcp.set_nodelay(true).ok();
    let client_addr = tcp.local_addr()?;

    let mut cc = connector_config(cfg);
    if let Some(p) = redirect {
        if let Some(u) = &p.username {
            cc.credentials = Credentials::UsernamePassword { username: u.clone(), password: cfg.password.clone() };
        }
        if p.domain.is_some() {
            cc.domain = p.domain.clone();
        }
    }
    let kerberos = cc.domain.as_ref().filter(|d| !d.is_empty()).map(|_| KerberosConfig { kdc_proxy_url: None, hostname: host.to_string() });
    let mut conn = ClientConnector::new(cc, client_addr)
        .with_static_channel(
            DrdynvcClient::new()
                .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())))
                // No H.264 decoder: the server then picks RemoteFX, ClearCodec or planar.
                .with_dynamic_channel(GraphicsPipelineClient::new(Box::new(Gfx), None)),
        );
    if let Some(p) = redirect {
        conn = conn.with_cluster_data(ironrdp_pdu::gcc::ClientClusterData {
            flags: ironrdp_pdu::gcc::RedirectionFlags::REDIRECTION_SUPPORTED | ironrdp_pdu::gcc::RedirectionFlags::REDIRECTED_SESSION_FIELD_VALID,
            redirection_version: ironrdp_pdu::gcc::RedirectionVersion::V6,
            redirected_session_id: p.session_id,
        });
        if let Some(lb) = routing_token(p) {
            conn = conn.with_load_balance_info(lb);
        }
        if let Some(pw) = &p.password {
            conn = conn.with_redirection_password_cookie(pw.clone());
        }
    }
    if let Some(app) = &cfg.remote_app {
        conn = conn.with_static_channel(crate::rdp_rail::Rail::new(app.clone(), ev.clone()));
    }
    // Windows only starts device redirection (drives, printers) when the audio channel is declared too,
    // so when redirection is wanted but audio is not (or cannot play), a silent channel stands in.
    let wants_devices = cfg.share_folder.is_some() || cfg.printer || cfg.smartcard;
    match audio {
        Some(opening) => conn = conn.with_static_channel(Rdpsnd::new(Box::new(Sound::new(opening)))),
        None if wants_devices => conn = conn.with_static_channel(Rdpsnd::new(Box::new(ironrdp_rdpsnd::client::NoopRdpsndBackend))),
        None => {}
    }
    if cfg.clipboard {
        let backend = Clip { tx: clip_tx.clone(), events: ev.clone(), state: clip_state.clone(), tmp: std::env::temp_dir().to_string_lossy().into_owned() };
        conn = conn.with_static_channel(Cliprdr::new(Box::new(backend)));
    }
    if let Some(r) = redirection(cfg)? {
        conn = conn.with_static_channel(r);
    }

    // A redirect carrying an encrypted one-time password (GNOME Remote Desktop hands a login
    // over to the user's session this way) is answered with RDSTLS instead of CredSSP.
    let rdstls = redirect.filter(|p| p.flags.contains(ServerRedirectionFlags::PASSWORD_IS_PK_ENCRYPTED) && p.password.is_some());
    let mut framed = Framed::new(tcp);
    stage(ev, "Negotiating…");
    let should_upgrade = match rdstls {
        Some(p) => {
            rdstls_negotiate(&mut framed, &mut conn, p)?;
            ironrdp_blocking::skip_connect_begin(&mut conn)
        }
        None => ironrdp_blocking::connect_begin(&mut framed, &mut conn).context("RDP negotiation")?,
    };
    let tcp = framed.into_inner_no_leftover();
    stage(ev, "Securing the connection…");
    let (mut tls, server_key) = tls_upgrade(tcp, host)?;
    if let Some(p) = rdstls {
        rdstls_authenticate(&mut tls, p)?;
    }
    let upgraded = ironrdp_blocking::mark_as_upgraded(should_upgrade, &mut conn);
    let mut framed = Framed::new(tls);
    stage(ev, "Signing in…");
    let outcome = ironrdp_blocking::connect_finalize(upgraded, conn, &mut framed, &mut Kdc, host.to_string().into(), server_key, kerberos).map_err(|e| anyhow!("sign-in failed: {e}"))?;
    Ok((outcome, framed))
}

fn run(cfg: &Config, fb: &Arc<Mutex<Framebuffer>>, stopping: &Arc<AtomicBool>, inputs: Receiver<Input>, ev: &Sender<Event>) -> Result<()> {
    let (clip_tx, clip_rx) = channel::<ClipboardMessage>();
    let clip_state = Arc::new(Mutex::new(ClipState { download_dir: cfg.download_dir.clone(), ..ClipState::default() }));

    // A load balancer may send the client on to another host, a few times at most.
    let mut target = cfg.host.clone();
    let mut port = cfg.port;
    let mut redirect: Option<Box<ironrdp_pdu::rdp::server_redirection::ServerRedirectionPdu>> = None;
    let mut redirects = 0;
    'session: loop {
        let (res, mut framed) = 'dial: {
            while redirects <= 4 {
                let (outcome, framed) = dial(cfg, &target, port, redirect.as_deref(), ev, &clip_tx, &clip_state)?;
                match outcome {
                    ConnectionOutcome::Connected(r) => break 'dial (r, framed),
                    ConnectionOutcome::Redirected(p) => {
                        redirects += 1;
                        follow_redirect(cfg, ev, &p, &mut target, &mut port);
                        redirect = Some(p);
                    }
                }
            }
            bail!("the server kept redirecting the session");
        };

        let (mut w, mut h) = (u32::from(res.desktop_size.width), u32::from(res.desktop_size.height));
        let mut image = DecodedImage::new(PixelFormat::BgrA32, res.desktop_size.width, res.desktop_size.height);
        {
            let mut f = fb.lock().unwrap();
            f.resize(w, h);
            f.bgra = vec![0; (w * h * 4) as usize];
            f.name = cfg.host.clone();
            f.screens = requested_screens(&cfg.monitors).into_iter().filter(|s| s.x + s.w <= w && s.y + s.h <= h).collect();
            f.version += 1;
        }
        let _ = ev.send(Event::Connected { width: w, height: h, name: cfg.host.clone() });
        let activation = res.activation_factory.clone();
        let mut stage_ = ActiveStageBuilder {
            static_channels: res.static_channels,
            user_channel_id: res.user_channel_id,
            io_channel_id: res.io_channel_id,
            message_channel_id: res.message_channel_id,
            share_id: res.share_id,
            compression_type: res.compression_type,
            enable_server_pointer: res.enable_server_pointer,
            pointer_software_rendering: res.pointer_software_rendering,
        }
        .build();

        // Short reads, so input and clipboard work is never starved by a quiet server.
        framed.get_inner_mut().0.sock.set_read_timeout(Some(POLL))?;

        let mut keys = Keys::default();
        let mut mouse = Mouse::default();
        let mut dirty: Option<Rect> = None;
        let mut last_publish = Instant::now();
        let mut want_size: Option<(u32, u32)> = None;
        let mut last_resize = Instant::now();
        let mut last_housekeeping = Instant::now();
        let mut channel_traffic: HashMap<u16, u32> = HashMap::new();

        loop {
            if stopping.load(Ordering::SeqCst) {
                log::debug!("{}: PDUs received per channel id: {:?}", cfg.host, channel_traffic);
                for o in stage_.graceful_shutdown().unwrap_or_default() {
                    if let ActiveStageOutput::ResponseFrame(f) = o {
                        let _ = framed.write_all(&f);
                    }
                }
                return Ok(());
            }

            // Input.
            let mut frames: Vec<Vec<u8>> = Vec::new();
            while let Ok(i) = inputs.try_recv() {
                match i {
                    Input::Pointer { x, y, buttons } => {
                        let evs = mouse.apply(x, y, buttons);
                        send_input(&mut stage_, &mut image, &mut frames, &evs)?;
                    }
                    Input::Key { keysym, down } => {
                        let evs = keys.key(keysym, down);
                        send_input(&mut stage_, &mut image, &mut frames, &evs)?;
                    }
                    Input::Clipboard(text) => {
                        clip_state.lock().unwrap().local = Some(text);
                        let _ = clip_tx.send(ClipboardMessage::SendInitiateCopy(vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]));
                    }
                    Input::SendFiles(paths) => {
                        let (descs, local) = describe_files(&paths);
                        if !descs.is_empty() {
                            let mut st = clip_state.lock().unwrap();
                            st.offered_total = descs.iter().map(|d| d.file_size.unwrap_or(0)).sum();
                            st.sent = 0;
                            st.offered = local;
                            drop(st);
                            let _ = clip_tx.send(ClipboardMessage::SendInitiateFileCopy(descs));
                        }
                    }
                    Input::UseVirtualDisplay { width, height } => want_size = Some((width, height)),
                    _ => {}
                }
            }

            // Ask the server for a new desktop size (debounced: a window drag sends many).
            if let Some((rw, rh)) = want_size {
                if last_resize.elapsed() > Duration::from_millis(250) {
                    want_size = None;
                    last_resize = Instant::now();
                    let (rw, rh) = MonitorLayoutEntry::adjust_display_size(rw, rh);
                    if (rw, rh) != (w, h) {
                        if let Some(f) = stage_.encode_resize(rw, rh, None, None) {
                            frames.push(f.map_err(|e| anyhow!("resize: {e}"))?);
                        }
                    }
                }
            }

            // Clipboard messages produced by the backend.
            while let Ok(m) = clip_rx.try_recv() {
                if let Some(f) = clipboard_frame(&mut stage_, m)? {
                    frames.push(f);
                }
            }
            if last_housekeeping.elapsed() > Duration::from_secs(5) {
                last_housekeeping = Instant::now();
                if let Some(c) = stage_.get_svc_processor_mut::<CliprdrClient>() {
                    if let Ok(msgs) = c.drive_timeouts() {
                        let f = stage_.process_svc_processor_messages(msgs).map_err(|e| anyhow!("clipboard: {e}"))?;
                        if !f.is_empty() {
                            frames.push(f);
                        }
                    }
                }
            }
            // Replies to calls that waited (a smart card inserted, say).
            if cfg.smartcard {
                if let Some(r) = stage_.get_svc_processor_mut::<RdpdrChannel>() {
                    if let Ok(msgs) = r.0.poll_deferred_messages() {
                        if !msgs.is_empty() {
                            let f = stage_.process_svc_processor_messages(ironrdp_svc::SvcProcessorMessages::<RdpdrChannel>::new(msgs)).map_err(|e| anyhow!("smart card: {e}"))?;
                            if !f.is_empty() {
                                frames.push(f);
                            }
                        }
                    }
                }
            }
            for f in frames {
                framed.write_all(&f).context("write to the server")?;
            }

            // Output from the server.
            let (action, payload) = match framed.read_pdu() {
                Ok(p) => p,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    publish(&image, fb, &mut dirty, &mut last_publish, true);
                    continue;
                }
                Err(e) => return Err(anyhow!(e).context("connection lost")),
            };
            if matches!(action, ironrdp_pdu::Action::X224) && payload.len() > 12 {
                // Which static channel this arrived on (MCS SendDataIndication: channel id at bytes 10..12).
                *channel_traffic.entry(u16::from_be_bytes([payload[10], payload[11]])).or_insert(0u32) += 1;
            }
            if let Some(p) = mid_session_redirect(action, &payload) {
                redirects += 1;
                if redirects > 4 {
                    bail!("the server kept redirecting the session");
                }
                follow_redirect(cfg, ev, &p, &mut target, &mut port);
                redirect = Some(Box::new(p));
                continue 'session;
            }
            let outs = match stage_.process(&mut image, action, &payload, framed.last_read_at()) {
                Ok(o) => o,
                // GNOME Remote Desktop sends on dynamic channels the client declined (telemetry,
                // audio); other clients ignore that, and so does this one.
                Err(e) if format!("{e:?}").contains("non existing DVC channel") => {
                    log::debug!("{}: ignored data on a declined dynamic channel", cfg.host);
                    continue;
                }
                Err(e) => return Err(anyhow!("session error: {e}")),
            };
            for out in outs {
                match out {
                    ActiveStageOutput::ResponseFrame(f) => framed.write_all(&f).context("write to the server")?,
                    ActiveStageOutput::GraphicsUpdate(r) => {
                        let r = Rect { l: u32::from(r.left), t: u32::from(r.top), r: u32::from(r.right), b: u32::from(r.bottom) };
                        dirty = Some(dirty.map_or(r, |d| d.union(r)));
                    }
                    ActiveStageOutput::MonitorLayout(mons) => {
                        let mut f = fb.lock().unwrap();
                        f.screens = mons.iter().filter(|m| m.right >= m.left && m.bottom >= m.top && m.left >= 0 && m.top >= 0).map(|m| Screen { x: m.left as u32, y: m.top as u32, w: (m.right - m.left + 1) as u32, h: (m.bottom - m.top + 1) as u32 }).collect();
                        f.version += 1;
                    }
                    ActiveStageOutput::DeactivateAll => {
                        // The server is changing the desktop size: run the reactivation sequence.
                        use connector::connection_activation::ConnectionActivationState;
                        use connector::Sequence as _;
                        let mut seq = activation.create();
                        let mut buf = ironrdp_core::WriteBuf::new();
                        // These reads must wait for the server, not give up after the short polling timeout.
                        framed.get_inner_mut().0.sock.set_read_timeout(Some(Duration::from_secs(15)))?;
                        loop {
                            buf.clear();
                            let written = if let Some(hint) = seq.next_pdu_hint() {
                                let pdu = framed.read_by_hint(hint).map_err(|e| anyhow!("reactivation: {e}"))?;
                                seq.step(&pdu, framed.last_read_at(), &mut buf).map_err(|e| anyhow!("reactivation: {e}"))?
                            } else {
                                seq.step_no_input(&mut buf).map_err(|e| anyhow!("reactivation: {e}"))?
                            };
                            if let Some(n) = written.size() {
                                framed.write_all(&buf[..n]).context("write to the server")?;
                            }
                            if let ConnectionActivationState::Finalized { desktop_size, share_id, enable_server_pointer, pointer_software_rendering, static_channel_chunk_size, .. } = seq.connection_activation_state() {
                                image = DecodedImage::new(PixelFormat::BgrA32, desktop_size.width, desktop_size.height);
                                stage_.reactivate(seq.io_channel_id(), seq.user_channel_id(), share_id, enable_server_pointer, pointer_software_rendering, static_channel_chunk_size);
                                (w, h) = (u32::from(desktop_size.width), u32::from(desktop_size.height));
                                let mut f = fb.lock().unwrap();
                                f.resize(w, h);
                                f.bgra = vec![0; (w * h * 4) as usize];
                                f.screens.clear();
                                f.version += 1;
                                dirty = Some(Rect { l: 0, t: 0, r: w - 1, b: h - 1 });
                                drop(f);
                                framed.get_inner_mut().0.sock.set_read_timeout(Some(POLL))?;
                                // A copy made around the resize may have been lost: offer it again.
                                if clip_state.lock().unwrap().local.is_some() {
                                    let _ = clip_tx.send(ClipboardMessage::SendInitiateCopy(vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]));
                                }
                                break;
                            }
                        }
                    }
                    ActiveStageOutput::Terminate(reason) => {
                        publish(&image, fb, &mut dirty, &mut last_publish, true);
                        log::info!("{}: RDP session terminated: {}", cfg.host, reason.description());
                        return Ok(());
                    }
                    _ => {}
                }
            }
            // The session composites the graphics pipeline into `image` and follows its
            // ResetGraphics by resizing it (a desktop resize, with RDPGFX): resize the framebuffer
            // too, or every later update is held back as "a resize in flight".
            let (iw, ih) = (u32::from(image.width()), u32::from(image.height()));
            if (iw, ih) != (w, h) && iw > 0 && ih > 0 {
                log::debug!("{}: graphics output resized to {iw}x{ih}", cfg.host);
                (w, h) = (iw, ih);
                let mut f = fb.lock().unwrap();
                f.resize(w, h);
                f.screens.retain(|s| s.x + s.w <= w && s.y + s.h <= h);
                drop(f);
                dirty = Some(Rect { l: 0, t: 0, r: w - 1, b: h - 1 });
                publish(&image, fb, &mut dirty, &mut last_publish, true);
            }
            publish(&image, fb, &mut dirty, &mut last_publish, false);
        }
    }
}

/// Graphics pipeline events need no handling here: the session composites the surfaces into
/// its image itself.
struct Gfx;

impl GraphicsPipelineHandler for Gfx {}

// ----- server redirection -----------------------------------------------------------

/// Points the next connection at a redirect's target; with none named, the same host again.
fn follow_redirect(cfg: &Config, ev: &Sender<Event>, p: &ServerRedirectionPdu, target: &mut String, port: &mut u16) {
    let next = p.target_net_address.clone().or_else(|| p.target_fqdn.clone()).or_else(|| p.target_net_addresses.as_ref().and_then(|a| a.first().cloned()));
    log::info!("{}: the server redirected the session to {next:?}", cfg.host);
    if let Some(n) = next {
        *target = n;
        *port = cfg.port;
    }
    stage(ev, "Redirected to another server…");
}

/// The load-balancing info of a redirect, as the X.224 routing token (without its CRLF).
fn routing_token(p: &ServerRedirectionPdu) -> Option<String> {
    let lb = p.load_balance_info.as_deref()?;
    Some(String::from_utf8_lossy(lb).trim_end_matches(['\r', '\n', '\0']).to_owned()).filter(|t| !t.is_empty())
}

/// A Server Redirection PDU arriving after the session started (GNOME Remote Desktop hands
/// a login over to the user's own session this way).
///
/// GNOME (FreeRDP) writes PDU version 1 in the Share Control header and pads the packet
/// differently from [MS-RDPBCGR] 2.2.13.3.1, which ironrdp's decoder rejects, so the
/// redirection packet is found by its `SEC_REDIRECTION_PKT` flags instead.
fn mid_session_redirect(action: ironrdp_pdu::Action, payload: &[u8]) -> Option<ServerRedirectionPdu> {
    const PDUTYPE_SERVER_REDIR_PKT: u16 = 0x000A;
    const SEC_REDIRECTION_PKT: [u8; 2] = [0x00, 0x04];
    if action != ironrdp_pdu::Action::X224 {
        return None;
    }
    let data = ironrdp_pdu::mcs::decode_send_data_indication(payload).ok()?.user_data;
    let header = data.get(..6)?;
    if u16::from_le_bytes([header[2], header[3]]) & 0x000F != PDUTYPE_SERVER_REDIR_PKT {
        return None;
    }
    // The packet follows the 6-byte header and a few bytes of padding.
    (6..data.len().min(16)).filter(|&i| data[i..].starts_with(&SEC_REDIRECTION_PKT)).find_map(|i| match ironrdp_core::decode::<ServerRedirectionPdu>(&data[i..]) {
        Ok(p) => Some(p),
        Err(e) => {
            log::debug!("not a redirection packet at offset {i}: {e}");
            None
        }
    })
}

/// X.224 negotiation for an RDSTLS reconnection: asks for RDSTLS only and sends the
/// redirect's routing token, which ironrdp's connector cannot do, then leaves the
/// connector ready for the TLS upgrade.
fn rdstls_negotiate(framed: &mut Framed<TcpStream>, conn: &mut ClientConnector, p: &ServerRedirectionPdu) -> Result<()> {
    use ironrdp_pdu::nego::{ConnectionConfirm, ConnectionRequest, ConnectionRequestWithOpaqueRoutingToken, OpaqueRoutingToken, RequestFlags, SecurityProtocol};
    use ironrdp_pdu::x224::X224;
    let token = routing_token(p).context("the server's redirect has no routing token")?;
    let request = ConnectionRequestWithOpaqueRoutingToken {
        request: ConnectionRequest { nego_data: None, flags: RequestFlags::empty(), protocol: SecurityProtocol::RDSTLS, correlation_info: None },
        routing_token: OpaqueRoutingToken(token),
    };
    framed.write_all(&ironrdp_core::encode_vec(&X224(request)).map_err(|e| anyhow!("RDP negotiation: {e}"))?).context("write to the server")?;
    let (_, pdu) = framed.read_pdu().context("RDP negotiation")?;
    match ironrdp_core::decode::<X224<ConnectionConfirm>>(&pdu).map_err(|e| anyhow!("RDP negotiation: {e}"))?.0 {
        ConnectionConfirm::Response { protocol, .. } if protocol.contains(SecurityProtocol::RDSTLS) => {}
        other => bail!("the server would not resume the redirected session (RDSTLS): {other:?}"),
    }
    // The connector reads the confirmation itself, so it also keeps the server's flags
    // (whether to ask for the message channel, among others).
    use connector::Sequence as _;
    conn.state = connector::ClientConnectorState::ConnectionInitiationWaitConfirm { requested_protocol: SecurityProtocol::RDSTLS };
    conn.step(&pdu, framed.last_read_at(), &mut ironrdp_core::WriteBuf::new()).map_err(|e| anyhow!("RDP negotiation: {e}"))?;
    Ok(())
}

/// RDSTLS sign-in ([MS-RDPBCGR] 2.2.17): after the server's capabilities, send the redirect's
/// GUID, user name and encrypted password back, and wait for the result.
fn rdstls_authenticate(s: &mut (impl std::io::Read + Write), p: &ServerRedirectionPdu) -> Result<()> {
    const VERSION_1: u16 = 1;
    const TYPE_CAPABILITIES: u16 = 1;
    const TYPE_AUTH_REQUEST: u16 = 2;
    const TYPE_AUTH_RESPONSE: u16 = 4;
    const DATA_PASSWORD_CREDS: u16 = 1;
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);

    let mut caps = [0u8; 8];
    s.read_exact(&mut caps).context("RDSTLS: no capabilities from the server")?;
    if u16_at(&caps, 2) != TYPE_CAPABILITIES || u16_at(&caps, 6) & VERSION_1 == 0 {
        bail!("RDSTLS: unexpected capabilities {caps:02x?}");
    }

    let mut req = Vec::new();
    for v in [VERSION_1, TYPE_AUTH_REQUEST, DATA_PASSWORD_CREDS] {
        req.extend(v.to_le_bytes());
    }
    let mut data = |bytes: &[u8]| -> Result<()> {
        req.extend(u16::try_from(bytes.len()).context("RDSTLS field too long")?.to_le_bytes());
        req.extend(bytes);
        Ok(())
    };
    // Strings are UTF-16LE with their terminating NUL.
    let text = |s: Option<&str>| -> Vec<u8> { s.unwrap_or("").encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect() };
    data(p.redirection_guid.as_deref().unwrap_or(&[]))?;
    data(&text(p.username.as_deref()))?;
    data(&text(p.domain.as_deref()))?;
    data(p.password.as_deref().unwrap_or(&[]))?;
    s.write_all(&req).context("RDSTLS: write to the server")?;
    s.flush().ok();

    let mut resp = [0u8; 10];
    s.read_exact(&mut resp).context("RDSTLS: the server closed the connection")?;
    let code = u32::from_le_bytes([resp[6], resp[7], resp[8], resp[9]]);
    if u16_at(&resp, 2) != TYPE_AUTH_RESPONSE || code != 0 {
        bail!("the server refused the redirected sign-in (RDSTLS result {code:#x})");
    }
    Ok(())
}

fn send_input(stage_: &mut ActiveStage, image: &mut DecodedImage, frames: &mut Vec<Vec<u8>>, evs: &[FastPathInputEvent]) -> Result<()> {
    if evs.is_empty() {
        return Ok(());
    }
    for o in stage_.process_fastpath_input(image, evs).map_err(|e| anyhow!("input: {e}"))? {
        if let ActiveStageOutput::ResponseFrame(f) = o {
            frames.push(f);
        }
    }
    Ok(())
}

fn clipboard_frame(stage_: &mut ActiveStage, m: ClipboardMessage) -> Result<Option<Vec<u8>>> {
    let Some(c) = stage_.get_svc_processor_mut::<CliprdrClient>() else { return Ok(None) };
    let err = |e| anyhow!("clipboard: {e}");
    let msgs = match m {
        ClipboardMessage::SendInitiateCopy(f) => c.initiate_copy(&f).map_err(err)?,
        ClipboardMessage::SendFormatData(r) => c.submit_format_data(r).map_err(err)?,
        ClipboardMessage::SendInitiatePaste(f) => c.initiate_paste(f).map_err(err)?,
        ClipboardMessage::SendInitiateFileCopy(f) => c.initiate_file_copy(f).map_err(err)?,
        ClipboardMessage::SendFileContentsRequest(r) => c.request_file_contents(r).map_err(err)?,
        ClipboardMessage::SendFileContentsResponse(r) => c.submit_file_contents(r).map_err(err)?,
        ClipboardMessage::Error(e) => {
            log::warn!("RDP clipboard: {e}");
            return Ok(None);
        }
    };
    let f = stage_.process_svc_processor_messages(msgs).map_err(|e| anyhow!("clipboard: {e}"))?;
    Ok((!f.is_empty()).then_some(f))
}

// ----- picture ----------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Rect {
    l: u32,
    t: u32,
    r: u32,
    b: u32,
}

impl Rect {
    fn union(self, o: Rect) -> Rect {
        Rect { l: self.l.min(o.l), t: self.t.min(o.t), r: self.r.max(o.r), b: self.b.max(o.b) }
    }
}

/// Copy the changed part of the decoded desktop into the shared framebuffer and wake the UI.
fn publish(image: &DecodedImage, fb: &Arc<Mutex<Framebuffer>>, dirty: &mut Option<Rect>, last: &mut Instant, force: bool) {
    let Some(d) = *dirty else { return };
    if !force && last.elapsed() < PUBLISH_EVERY {
        return;
    }
    *dirty = None;
    *last = Instant::now();
    let (w, h) = (u32::from(image.width()), u32::from(image.height()));
    let mut f = fb.lock().unwrap();
    if f.width != w || f.height != h || f.bgra.len() != (w * h * 4) as usize {
        return; // a resize is in flight; the next update repaints everything
    }
    let (l, r, t, b) = (d.l.min(w - 1), d.r.min(w - 1), d.t.min(h - 1), d.b.min(h - 1));
    let data = image.data();
    for y in t..=b {
        let a = ((y * w + l) * 4) as usize;
        let z = ((y * w + r + 1) * 4) as usize;
        f.bgra[a..z].copy_from_slice(&data[a..z]);
        for px in f.bgra[a..z].chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    f.version += 1;
    f.wake();
}

// ----- mouse and keyboard -------------------------------------------------------------

/// Turns the app's pointer state (x, y, button mask) into RDP mouse events.
#[derive(Default)]
struct Mouse {
    db: Database,
    buttons: u8,
}

impl Mouse {
    fn apply(&mut self, x: u16, y: u16, buttons: u8) -> Vec<FastPathInputEvent> {
        let mut ops = vec![Operation::MouseMove(MousePosition { x, y })];
        for (bit, b) in [(1u8, MouseButton::Left), (2, MouseButton::Middle), (4, MouseButton::Right)] {
            match (self.buttons & bit != 0, buttons & bit != 0) {
                (false, true) => ops.push(Operation::MouseButtonPressed(b)),
                (true, false) => ops.push(Operation::MouseButtonReleased(b)),
                _ => {}
            }
        }
        // Wheel: a press of button 4 or 5 (bits 8, 16) is one notch up or down; 32, 64 are sideways.
        let pressed = |bit: u8| buttons & bit != 0 && self.buttons & bit == 0;
        for (bit, vertical, units) in [(8u8, true, 120i16), (16, true, -120), (32, false, -120), (64, false, 120)] {
            if pressed(bit) {
                ops.push(Operation::WheelRotations(WheelRotations { is_vertical: vertical, rotation_units: units }));
            }
        }
        self.buttons = buttons;
        self.db.apply(ops).into_iter().collect()
    }
}

/// How one pressed key was sent, so its release matches whatever the modifiers were then.
#[derive(Clone, Copy)]
enum Sent {
    Scan(Scancode),
    Char(char),
}

#[derive(Default)]
struct Keys {
    db: Database,
    down: HashMap<u32, Sent>,
}

impl Keys {
    fn key(&mut self, keysym: u32, down: bool) -> Vec<FastPathInputEvent> {
        let op = if down {
            let sent = self.down.get(&keysym).copied().or_else(|| self.choose(keysym));
            let Some(s) = sent else { return Vec::new() };
            self.down.insert(keysym, s);
            match s {
                Sent::Scan(c) => Operation::KeyPressed(c),
                Sent::Char(c) => Operation::UnicodeKeyPressed(c),
            }
        } else {
            match self.down.remove(&keysym) {
                Some(Sent::Scan(c)) => Operation::KeyReleased(c),
                Some(Sent::Char(c)) => Operation::UnicodeKeyReleased(c),
                None => return Vec::new(),
            }
        };
        self.db.apply([op]).into_iter().collect()
    }

    /// Shortcuts (Ctrl or Alt held) go by key position, as the remote layout is not known;
    /// plain typing is sent as the character itself, so it comes out right on any layout.
    fn choose(&self, keysym: u32) -> Option<Sent> {
        if let Some(sc) = special_scancode(keysym) {
            return Some(Sent::Scan(sc));
        }
        let ch = keysym_char(keysym)?;
        let shortcut = [(false, 0x1d), (true, 0x1d), (false, 0x38), (true, 0x38)].iter().any(|&(e, c)| self.db.is_key_pressed(Scancode::from_u8(e, c)));
        if shortcut {
            if let Some(sc) = ascii_scancode(ch) {
                return Some(Sent::Scan(sc));
            }
        }
        Some(Sent::Char(ch))
    }
}

fn keysym_char(k: u32) -> Option<char> {
    match k {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(k),
        0x0100_0000..=0x0110_ffff => char::from_u32(k - 0x0100_0000),
        _ => None,
    }
}

fn special_scancode(k: u32) -> Option<Scancode> {
    let (ext, code) = match k {
        0xff1b => (false, 0x01),
        0xff08 => (false, 0x0e),
        0xff09 => (false, 0x0f),
        0xff0d => (false, 0x1c),
        0xff8d => (true, 0x1c),
        0xffe5 => (false, 0x3a),
        0xffe1 => (false, 0x2a),
        0xffe2 => (false, 0x36),
        0xffe3 => (false, 0x1d),
        0xffe4 => (true, 0x1d),
        0xffe9 | 0xffe7 => (false, 0x38),
        0xffea | 0xffe8 | 0xfe03 => (true, 0x38),
        0xffeb | 0xffed => (true, 0x5b),
        0xffec | 0xffee => (true, 0x5c),
        0xff67 => (true, 0x5d),
        0xff63 => (true, 0x52),
        0xffff => (true, 0x53),
        0xff50 => (true, 0x47),
        0xff57 => (true, 0x4f),
        0xff55 => (true, 0x49),
        0xff56 => (true, 0x51),
        0xff51 => (true, 0x4b),
        0xff52 => (true, 0x48),
        0xff53 => (true, 0x4d),
        0xff54 => (true, 0x50),
        0xff7f => (false, 0x45),
        0xff14 => (false, 0x46),
        0xffbe..=0xffc7 => (false, 0x3b + (k - 0xffbe) as u8), // F1-F10
        0xffc8 => (false, 0x57),
        0xffc9 => (false, 0x58),
        _ => return None,
    };
    Some(Scancode::from_u8(ext, code))
}

/// Position of a character on a US keyboard (shifted symbols map to their base key).
fn ascii_scancode(c: char) -> Option<Scancode> {
    let code: u8 = match c.to_ascii_lowercase() {
        'q' => 0x10, 'w' => 0x11, 'e' => 0x12, 'r' => 0x13, 't' => 0x14, 'y' => 0x15, 'u' => 0x16, 'i' => 0x17, 'o' => 0x18, 'p' => 0x19,
        'a' => 0x1e, 's' => 0x1f, 'd' => 0x20, 'f' => 0x21, 'g' => 0x22, 'h' => 0x23, 'j' => 0x24, 'k' => 0x25, 'l' => 0x26,
        'z' => 0x2c, 'x' => 0x2d, 'c' => 0x2e, 'v' => 0x2f, 'b' => 0x30, 'n' => 0x31, 'm' => 0x32,
        '1' | '!' => 0x02, '2' | '@' => 0x03, '3' | '#' => 0x04, '4' | '$' => 0x05, '5' | '%' => 0x06,
        '6' | '^' => 0x07, '7' | '&' => 0x08, '8' | '*' => 0x09, '9' | '(' => 0x0a, '0' | ')' => 0x0b,
        '-' | '_' => 0x0c, '=' | '+' => 0x0d, '[' | '{' => 0x1a, ']' | '}' => 0x1b, ';' | ':' => 0x27,
        '\'' | '"' => 0x28, '`' | '~' => 0x29, '\\' | '|' => 0x2b, ',' | '<' => 0x33, '.' | '>' => 0x34, '/' | '?' => 0x35,
        ' ' => 0x39,
        _ => return None,
    };
    Some(Scancode::from_u8(false, code))
}

// ----- clipboard ---------------------------------------------------------------------

#[derive(Default)]
struct ClipState {
    /// Text the user copied locally, offered to the server on request.
    local: Option<String>,
    /// The format to ask for when the server's clipboard is pasted here.
    remote_format: Option<ClipboardFormatId>,
    /// Files offered to the server (index = the server's file index), and how much it has read.
    offered: Vec<PathBuf>,
    sent: u64,
    offered_total: u64,
    /// Where files copied on the server are saved, and the transfer in progress.
    download_dir: PathBuf,
    incoming: Option<Incoming>,
    next_stream: u32,
}

/// A transfer from the server: one file at a time, read in ranges.
struct Incoming {
    files: Vec<FileDescriptor>,
    data_id: Option<u32>,
    idx: usize,
    pos: u64,
    out: Option<(std::fs::File, PathBuf)>,
    total: u64,
    got: u64,
    first_saved: Option<PathBuf>,
}

/// Bytes asked of the server at a time.
const FILE_CHUNK: u32 = 256 * 1024;
/// A transfer larger than this is refused (the sizes come from the server).
const FILE_MAX: u64 = 64 << 30;

/// A server-supplied name made safe to create under the download folder.
fn safe_component(name: &str) -> Option<String> {
    let n: String = name.chars().map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':') { '_' } else { c }).collect();
    let n = n.trim().trim_matches('.').trim().to_string();
    (!n.is_empty() && n.len() <= 200).then_some(n)
}

/// A path for a received file under `root` that does not exist yet.
fn received_path(root: &std::path::Path, f: &FileDescriptor) -> Option<PathBuf> {
    let mut dir = root.to_path_buf();
    if let Some(rel) = &f.relative_path {
        for part in rel.split(['\\', '/']).filter(|p| !p.is_empty()) {
            dir.push(safe_component(part)?);
        }
    }
    let name = safe_component(&f.name.rsplit(['\\', '/']).next().unwrap_or(&f.name))?;
    let mut path = dir.join(&name);
    let mut n = 1;
    while path.exists() {
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.clone(), String::new()),
        };
        path = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
        if n > 1000 {
            return None;
        }
    }
    Some(path)
}

/// Files (and the files inside folders) as clipboard descriptors; their local paths, in the same order.
fn describe_files(paths: &[PathBuf]) -> (Vec<FileDescriptor>, Vec<PathBuf>) {
    fn walk(path: &std::path::Path, rel: Option<String>, out: &mut Vec<(FileDescriptor, PathBuf)>) {
        if out.len() >= 5000 {
            return;
        }
        let Ok(meta) = std::fs::metadata(path) else { return };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if meta.is_dir() {
            let sub = match &rel {
                Some(r) => format!("{r}\\{name}"),
                None => name,
            };
            if let Ok(rd) = std::fs::read_dir(path) {
                let mut kids: Vec<_> = rd.flatten().map(|e| e.path()).collect();
                kids.sort();
                for k in kids {
                    walk(&k, Some(sub.clone()), out);
                }
            }
        } else if meta.is_file() {
            let mut d = FileDescriptor::new(name);
            d.file_size = Some(meta.len());
            d.relative_path = rel;
            out.push((d, path.to_path_buf()));
        }
    }
    let mut all = Vec::new();
    for p in paths {
        walk(p, None, &mut all);
    }
    all.into_iter().unzip()
}

#[derive(Debug)]
struct Clip {
    tx: Sender<ClipboardMessage>,
    events: Sender<Event>,
    state: Arc<Mutex<ClipState>>,
    tmp: String,
}

impl std::fmt::Debug for ClipState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipState").finish_non_exhaustive()
    }
}

impl_as_any!(Clip);

impl CliprdrBackend for Clip {
    fn temporary_directory(&self) -> &str {
        &self.tmp
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        let formats = if self.state.lock().unwrap().local.is_some() { vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)] } else { Vec::new() };
        let _ = self.tx.send(ClipboardMessage::SendInitiateCopy(formats));
    }

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available: &[ClipboardFormat]) {
        // Files win over text: copying files in Explorer also puts their names on the clipboard.
        if let Some(f) = available.iter().find(|a| a.name.as_ref() == Some(&ClipboardFormatName::FILE_LIST)) {
            self.state.lock().unwrap().remote_format = Some(f.id());
            let _ = self.tx.send(ClipboardMessage::SendInitiatePaste(f.id()));
            return;
        }
        let pick = [ClipboardFormatId::CF_UNICODETEXT, ClipboardFormatId::CF_TEXT].into_iter().find(|f| available.iter().any(|a| a.id() == *f));
        if let Some(f) = pick {
            self.state.lock().unwrap().remote_format = Some(f);
            let _ = self.tx.send(ClipboardMessage::SendInitiatePaste(f));
        }
    }

    fn on_format_data_request(&mut self, req: FormatDataRequest) {
        let text = self.state.lock().unwrap().local.clone();
        log::debug!("RDP clipboard: the server asks for format {} (have text: {})", req.format.value(), text.is_some());
        let resp = match (text, req.format) {
            (Some(t), ClipboardFormatId::CF_UNICODETEXT) => {
                let mut bytes: Vec<u8> = t.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().flat_map(u16::to_le_bytes).collect();
                bytes.extend_from_slice(&[0, 0]);
                FormatDataResponse::new_data(bytes)
            }
            _ => FormatDataResponse::new_error(),
        };
        let _ = self.tx.send(ClipboardMessage::SendFormatData(ironrdp_core::IntoOwned::into_owned(resp)));
    }

    fn on_format_data_response(&mut self, resp: FormatDataResponse<'_>) {
        if resp.is_error() {
            return;
        }
        let fmt = self.state.lock().unwrap().remote_format;
        let data = resp.data();
        let text = if fmt == Some(ClipboardFormatId::CF_UNICODETEXT) {
            let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
            String::from_utf16_lossy(&units)
        } else {
            data.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect()
        };
        let _ = self.events.send(Event::ServerClipboard(text.replace("\r\n", "\n")));
    }

    /// The server pastes files we offered: answer its size and range requests from the local files.
    fn on_file_contents_request(&mut self, req: FileContentsRequest) {
        use std::io::{Read, Seek, SeekFrom};
        let path = usize::try_from(req.index).ok().and_then(|i| self.state.lock().unwrap().offered.get(i).cloned());
        let resp = match path {
            None => FileContentsResponse::new_error(req.stream_id),
            Some(p) if req.flags.contains(FileContentsFlags::SIZE) => match std::fs::metadata(&p) {
                Ok(m) => FileContentsResponse::new_size_response(req.stream_id, m.len()),
                Err(_) => FileContentsResponse::new_error(req.stream_id),
            },
            Some(p) => {
                let want = req.requested_size.min(FILE_CHUNK * 4) as usize;
                let read = std::fs::File::open(&p).and_then(|mut f| {
                    f.seek(SeekFrom::Start(req.position))?;
                    let mut buf = Vec::with_capacity(want);
                    f.take(want as u64).read_to_end(&mut buf)?;
                    Ok(buf)
                });
                match read {
                    Ok(buf) => {
                        let mut st = self.state.lock().unwrap();
                        st.sent += buf.len() as u64;
                        let (sent, total) = (st.sent, st.offered_total);
                        drop(st);
                        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let _ = self.events.send(Event::FileCopy(if sent >= total { Progress::Done { name } } else { Progress::Sending { name, sent, total } }));
                        FileContentsResponse::new_data_response(req.stream_id, buf)
                    }
                    Err(_) => FileContentsResponse::new_error(req.stream_id),
                }
            }
        };
        let _ = self.tx.send(ClipboardMessage::SendFileContentsResponse(ironrdp_core::IntoOwned::into_owned(resp)));
    }

    fn on_file_contents_response(&mut self, resp: FileContentsResponse<'_>) {
        let fail = |this: &mut Self, why: &str| {
            let mut st = this.state.lock().unwrap();
            let name = st.incoming.as_ref().and_then(|i| i.files.get(i.idx)).map(|f| f.name.clone()).unwrap_or_default();
            if let Some(Incoming { out: Some((_, p)), .. }) = st.incoming.take() {
                let _ = std::fs::remove_file(p);
            }
            drop(st);
            let _ = this.events.send(Event::FileCopy(Progress::Failed { name, reason: why.to_string() }));
        };
        if resp.is_error() {
            return fail(self, "the server could not provide the file");
        }
        let mut st = self.state.lock().unwrap();
        let Some(inc) = st.incoming.as_mut() else { return };
        let Some(f) = inc.files.get(inc.idx).cloned() else { return };
        let data = resp.data();
        let size = f.file_size.unwrap_or(0);
        if inc.out.is_none() {
            let Some(path) = received_path(&st_dir(&st.download_dir), &f) else {
                drop(st);
                return fail(self, "the file name is not usable");
            };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::File::create(&path) {
                Ok(file) => {
                    let inc = st.incoming.as_mut().unwrap();
                    inc.first_saved.get_or_insert_with(|| path.clone());
                    inc.out = Some((file, path));
                }
                Err(e) => {
                    drop(st);
                    return fail(self, &format!("cannot create the file: {e}"));
                }
            }
        }
        let inc = st.incoming.as_mut().unwrap();
        if let Some((file, _)) = inc.out.as_mut() {
            if file.write_all(data).is_err() {
                drop(st);
                return fail(self, "cannot write the file");
            }
        }
        inc.pos += data.len() as u64;
        inc.got += data.len() as u64;
        let (got, total, name) = (inc.got, inc.total, f.name.clone());
        let file_done = inc.pos >= size || data.is_empty();
        if file_done {
            inc.out = None;
            inc.idx += 1;
            inc.pos = 0;
        }
        let next = inc.files.get(inc.idx).map(|n| (inc.idx, inc.pos, n.file_size.unwrap_or(0), inc.data_id));
        let finished = if next.is_none() { st.incoming.take().and_then(|i| i.first_saved) } else { None };
        let stream = st.next_stream;
        st.next_stream += 1;
        drop(st);
        if let Some((idx, pos, size, data_id)) = next {
            let _ = self.events.send(Event::FileCopy(Progress::Receiving { name, got, total }));
            let remaining = size.saturating_sub(pos);
            let _ = self.tx.send(ClipboardMessage::SendFileContentsRequest(FileContentsRequest {
                stream_id: stream,
                index: idx as i32,
                flags: FileContentsFlags::RANGE,
                position: pos,
                requested_size: remaining.min(u64::from(FILE_CHUNK)) as u32,
                data_id,
            }));
        } else if let Some(saved) = finished {
            let _ = self.events.send(Event::FileCopy(Progress::Received { saved }));
        }
    }

    /// The server copied files: start fetching them, one after another.
    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        let files: Vec<FileDescriptor> = files.iter().filter(|f| f.attributes.map_or(true, |a| !a.contains(ironrdp_cliprdr::pdu::ClipboardFileAttributes::DIRECTORY))).cloned().collect();
        let total: u64 = files.iter().map(|f| f.file_size.unwrap_or(0)).sum();
        if files.is_empty() {
            return;
        }
        if total > FILE_MAX {
            let _ = self.events.send(Event::FileCopy(Progress::Failed { name: files[0].name.clone(), reason: "the files are too large".into() }));
            return;
        }
        let mut st = self.state.lock().unwrap();
        let stream = st.next_stream;
        st.next_stream += 1;
        let (first_size, name) = (files[0].file_size.unwrap_or(0), files[0].name.clone());
        st.incoming = Some(Incoming { files, data_id: clip_data_id, idx: 0, pos: 0, out: None, total, got: 0, first_saved: None });
        drop(st);
        let _ = self.events.send(Event::FileCopy(Progress::Receiving { name, got: 0, total }));
        let _ = self.tx.send(ClipboardMessage::SendFileContentsRequest(FileContentsRequest {
            stream_id: stream,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: 0,
            requested_size: first_size.min(u64::from(FILE_CHUNK)) as u32,
            data_id: clip_data_id,
        }));
    }

    fn on_lock(&mut self, _: LockDataId) {}
    fn on_unlock(&mut self, _: LockDataId) {}
}

fn st_dir(p: &std::path::Path) -> PathBuf {
    p.to_path_buf()
}

// ----- audio -------------------------------------------------------------------------

/// Opening the output can take seconds (WASAPI), so it happens on its own thread while the
/// connection is made; sound that arrives before it is ready is dropped.
struct Sound {
    player: Option<PcmPlayer>,
    opening: Option<Receiver<Option<PcmPlayer>>>,
    formats: Vec<AudioFormat>,
    waves: u64,
}

fn open_audio() -> Receiver<Option<PcmPlayer>> {
    let (tx, rx) = channel();
    let _ = std::thread::Builder::new().name("rdp-audio-open".into()).spawn(move || {
        let _ = tx.send(PcmPlayer::start());
    });
    rx
}

impl std::fmt::Debug for Sound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sound").finish_non_exhaustive()
    }
}

impl Sound {
    fn new(opening: Receiver<Option<PcmPlayer>>) -> Self {
        let pcm = |rate: u32| AudioFormat { format: WaveFormat::PCM, n_channels: 2, n_samples_per_sec: rate, n_avg_bytes_per_sec: rate * 4, n_block_align: 4, bits_per_sample: 16, data: None };
        // Only the rate the output plays at, so no resampling is needed.
        Self { player: None, opening: Some(opening), formats: vec![pcm(PcmPlayer::RATE)], waves: 0 }
    }
}

impl RdpsndClientHandler for Sound {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, _: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        self.waves += 1;
        if self.waves == 1 || self.waves % 500 == 0 {
            log::debug!("RDP audio: {} wave blocks received ({} bytes in the latest)", self.waves, data.len());
        }
        if let Some(rx) = &self.opening {
            match rx.try_recv() {
                Ok(player) => {
                    if player.is_none() {
                        log::warn!("RDP audio unavailable; the session will be silent");
                    }
                    self.player = player;
                    self.opening = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.opening = None,
            }
        }
        if let Some(p) = &self.player {
            p.push(data.into_owned());
        }
    }

    fn set_volume(&mut self, _: ironrdp_rdpsnd::pdu::VolumePdu) {}

    fn set_pitch(&mut self, _: ironrdp_rdpsnd::pdu::PitchPdu) {}

    fn close(&mut self) {}
}

/// The device-redirection channel, declared the way real clients declare it: with the INITIALIZED,
/// ENCRYPT_RDP and COMPRESS_RDP options. (IronRDP leaves INITIALIZED out; Windows ignores a channel
/// declared without it for device redirection, though it tolerates that for the clipboard.)
#[derive(Debug)]
struct RdpdrChannel(Rdpdr);

ironrdp_core::impl_as_any!(RdpdrChannel);

impl ironrdp_svc::SvcProcessor for RdpdrChannel {
    fn channel_name(&self) -> ironrdp_pdu::gcc::ChannelName {
        self.0.channel_name()
    }

    fn compression_condition(&self) -> ironrdp_svc::CompressionCondition {
        self.0.compression_condition()
    }

    fn channel_options(&self) -> ironrdp_pdu::gcc::ChannelOptions {
        use ironrdp_pdu::gcc::ChannelOptions as O;
        O::INITIALIZED | O::ENCRYPT_RDP | O::COMPRESS_RDP
    }

    fn start(&mut self) -> ironrdp_pdu::PduResult<Vec<ironrdp_svc::SvcMessage>> {
        self.0.start()
    }

    fn process(&mut self, payload: &[u8]) -> ironrdp_pdu::PduResult<Vec<ironrdp_svc::SvcMessage>> {
        self.0.process(payload)
    }
}

impl ironrdp_svc::SvcClientProcessor for RdpdrChannel {}

// ----- drives and printer ---------------------------------------------------------------

/// Drives and the printer (Linux and macOS; Windows has a different native backend that is not wired up).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn redirection(cfg: &Config) -> Result<Option<RdpdrChannel>> {
    if cfg.share_folder.is_some() || cfg.printer {
        log::warn!("drive and printer redirection are not available on this platform yet");
    }
    Ok(None)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn redirection(cfg: &Config) -> Result<Option<RdpdrChannel>> {
    if cfg.share_folder.is_none() && !cfg.printer && !cfg.smartcard {
        return Ok(None);
    }
    let base = cfg.share_folder.as_ref().map_or_else(|| "/".to_string(), |p| p.to_string_lossy().into_owned());
    let mut factory = NixRdpdrBackendFactory::new(base);
    if let Some(p) = &cfg.share_folder {
        let name = p.file_name().map_or_else(|| "Shared".to_string(), |n| n.to_string_lossy().into_owned());
        factory = factory.with_drive(1, name);
    }
    if cfg.printer {
        // Jobs go to the default CUPS printer; with none set up they are saved as files instead.
        let has_cups_default = std::process::Command::new("lpstat").arg("-d").output().map_or(false, |o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("destination:"));
        let target = if has_cups_default { PrintTarget::DefaultPrinter } else { PrintTarget::Folder(cfg.download_dir.clone()) };
        log::info!("RDP printer: jobs go to {target:?}");
        factory = factory.with_printer("SeeMyMac Printer".to_string(), target);
    }
    let product = factory.build_rdpdr_backend().map_err(|e| anyhow!("device redirection: {e}"))?;
    // The announced driver must be one the server has; "Microsoft PS Class Driver" ships with Windows 10 and 11.
    let printers: Vec<_> = product.printers().iter().map(|p| ironrdp_rdpdr::RdpdrPrinter::new(p.device_id(), p.name().to_string(), cfg.printer_driver.clone()).with_default(true)).collect();
    let (backend, drives) = product.into_parts();
    #[cfg(target_os = "linux")]
    let backend: Box<dyn ironrdp_rdpdr::backend::RdpdrBackend> = if cfg.smartcard { Box::new(crate::rdp_scard::ScardBackend::new(backend)) } else { Box::new(crate::rdp_scard::ScardBackend::reporting(backend)) };
    let mut rdpdr = Rdpdr::new(backend, "SeeMyMac".to_owned()).with_drives(Some(drives.into_iter().map(|d| d.into_parts()).collect()));
    for p in printers {
        rdpdr = rdpdr.with_redirected_printer(p);
    }
    #[cfg(target_os = "linux")]
    if cfg.smartcard {
        rdpdr = rdpdr.with_smartcard(0);
    }
    Ok(Some(RdpdrChannel(rdpdr)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_names_split_into_user_and_domain() {
        assert_eq!(split_user("CORP\\alice"), ("alice".into(), Some("CORP".into())));
        assert_eq!(split_user("alice"), ("alice".into(), None));
    }

    #[test]
    fn typing_uses_characters_and_shortcuts_use_key_positions() {
        let mut k = Keys::default();
        // 'a' on its own is the character.
        let ev = k.key('a' as u32, true);
        assert!(matches!(ev[0], FastPathInputEvent::UnicodeKeyboardEvent(..)));
        k.key('a' as u32, false);
        // Ctrl+C is Ctrl and the C key.
        k.key(0xffe3, true);
        let ev = k.key('c' as u32, true);
        assert!(matches!(ev[0], FastPathInputEvent::KeyboardEvent(_, 0x2e)));
    }

    #[test]
    fn release_matches_the_way_the_key_was_pressed() {
        let mut k = Keys::default();
        k.key('a' as u32, true);
        k.key(0xffe3, true); // Ctrl goes down while 'a' is held
        let ev = k.key('a' as u32, false);
        assert!(matches!(ev[0], FastPathInputEvent::UnicodeKeyboardEvent(..)));
    }

    #[test]
    fn wheel_notches_become_wheel_events() {
        let mut m = Mouse::default();
        let ev = m.apply(10, 10, 8);
        assert!(ev.iter().any(|e| matches!(e, FastPathInputEvent::MouseEvent(p) if p.number_of_wheel_rotation_units == 120)));
        let ev = m.apply(10, 10, 0);
        assert!(!ev.iter().any(|e| matches!(e, FastPathInputEvent::MouseEvent(p) if p.number_of_wheel_rotation_units != 0)));
    }

    #[test]
    fn received_file_names_cannot_escape_the_download_folder() {
        let root = std::env::temp_dir().join("seemymac-rdp-names-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut f = FileDescriptor::new("..\\..\\etc\\passwd");
        let p = received_path(&root, &f).unwrap();
        assert_eq!(p, root.join("passwd"), "only the last component of a name is used");
        f.relative_path = Some("..\\..\\up".into());
        assert!(received_path(&root, &f).is_none(), "a path that climbs out is refused");
        f.relative_path = Some("docs\\sub".into());
        assert_eq!(received_path(&root, &f).unwrap(), root.join("docs").join("sub").join("passwd"));
        assert!(safe_component("..").is_none() && safe_component("").is_none() && safe_component("a:b").unwrap() == "a_b");
        // An existing file is never overwritten.
        std::fs::write(root.join("passwd"), b"x").unwrap();
        f.relative_path = None;
        assert_eq!(received_path(&root, &f).unwrap(), root.join("passwd (1)"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dropped_folders_are_described_by_their_files() {
        let root = std::env::temp_dir().join("seemymac-rdp-offer-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d/e")).unwrap();
        std::fs::write(root.join("d/a.txt"), b"abc").unwrap();
        std::fs::write(root.join("d/e/b.txt"), b"hello").unwrap();
        let (descs, paths) = describe_files(&[root.join("d")]);
        assert_eq!(paths.len(), 2);
        let rels: Vec<_> = descs.iter().map(|d| (d.name.clone(), d.relative_path.clone(), d.file_size)).collect();
        assert!(rels.contains(&("a.txt".into(), Some("d".into()), Some(3))), "{rels:?}");
        assert!(rels.contains(&("b.txt".into(), Some("d\\e".into()), Some(5))), "{rels:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unconfigured_redirection_adds_no_channel() {
        assert!(redirection(&Config::new("h", 3389, "u", "p")).unwrap().is_none());
    }

    #[test]
    fn several_monitors_become_one_desktop_with_a_primary_at_the_origin() {
        let m = |x, y, w, h| MonitorRect { x, y, w, h };
        assert!(monitor_layout(&[m(0, 0, 1920, 1080)]).is_none(), "one monitor needs no layout");
        // The second monitor sits to the left of the primary (negative x on the desktop).
        let (size, data) = monitor_layout(&[m(1920, 0, 1920, 1080), m(0, 0, 1920, 1080)]).unwrap();
        assert_eq!((size.width, size.height), (3840, 1080));
        assert_eq!((data.monitors[0].left, data.monitors[0].right), (0, 1919));
        assert!(data.monitors[0].flags.contains(ironrdp_pdu::gcc::MonitorFlags::PRIMARY));
        assert_eq!((data.monitors[1].left, data.monitors[1].right), (-1920, -1));
    }

    #[test]
    fn requested_monitors_become_the_views_monitor_list() {
        let m = |x, y, w, h| MonitorRect { x, y, w, h };
        assert!(requested_screens(&[m(0, 0, 1920, 1080)]).is_empty());
        // The first monitor is the primary; the second sits to its left.
        let s = requested_screens(&[m(1920, 0, 1920, 1080), m(0, 0, 1280, 1024)]);
        assert_eq!((s[0].x, s[0].w), (1920, 1920));
        assert_eq!((s[1].x, s[1].w), (0, 1280));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_shared_folder_and_a_printer_make_a_redirection_channel() {
        let dir = std::env::temp_dir();
        let mut c = Config::new("h", 3389, "u", "p");
        c.share_folder = Some(dir);
        assert!(redirection(&c).unwrap().is_some(), "a drive is offered");
        c.share_folder = None;
        c.printer = true;
        assert!(redirection(&c).unwrap().is_some(), "a printer alone is offered");
    }
}
