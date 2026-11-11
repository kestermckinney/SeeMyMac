//! Smart-card redirection for RDP (Linux): answers the server's smart-card calls (MS-RDPESC)
//! from the local PC/SC service.
//!
//! IronRDP decodes the calls and encodes the replies; this module is the part in between,
//! which IronRDP leaves to the application. `libpcsclite` is loaded when the session
//! starts (like the audio output), so nothing is linked at build time and a computer
//! without it just offers no card readers.
//!
//! Calls that can wait a long time (waiting for a card to be inserted) run on their own
//! thread and complete through [`RdpdrBackend::poll_deferred_messages`], so they never
//! stall the session; `Cancel` interrupts them.

use ironrdp_pdu::utils::CharacterSet;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::backend::RdpdrBackend;
use ironrdp_rdpdr::pdu::efs::{DeviceControlRequest, DeviceControlResponse, NtStatus, ServerDeviceAnnounceResponse};
use ironrdp_rdpdr::pdu::esc::{
    CardProtocol, CardState, CardStateFlags, ConnectReturn, ControlReturn, EstablishContextReturn, GetAttribReturn, GetStatusChangeReturn, ListReadersReturn, LongReturn,
    ReaderStateCommonCall, ReconnectReturn, ReturnCode, SCardIORequest, ScardCall, ScardContext, ScardHandle, ScardIoCtlCode, StatusReturn, TransmitReturn,
};
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_svc::SvcMessage;
use libloading::Library;
use std::collections::HashSet;
use std::ffi::{c_char, c_long, c_ulong, c_void, CString};
use std::sync::{Arc, Mutex};

type Long = c_long;
type Dword = c_ulong;

/// `SCARD_READERSTATE` as libpcsclite lays it out on Linux.
#[repr(C)]
struct PcscReaderState {
    reader: *const c_char,
    user_data: *mut c_void,
    current: Dword,
    event: Dword,
    cb_atr: Dword,
    atr: [u8; 33],
}

#[repr(C)]
struct PcscIoRequest {
    protocol: Dword,
    pci_length: Dword,
}

/// The libpcsclite entry points used. The library is kept loaded for as long as they are.
struct Api {
    _lib: Library,
    establish: unsafe extern "C" fn(Dword, *const c_void, *const c_void, *mut Long) -> Long,
    release: unsafe extern "C" fn(Long) -> Long,
    is_valid: unsafe extern "C" fn(Long) -> Long,
    cancel: unsafe extern "C" fn(Long) -> Long,
    list_readers: unsafe extern "C" fn(Long, *const c_char, *mut c_char, *mut Dword) -> Long,
    status_change: unsafe extern "C" fn(Long, Dword, *mut PcscReaderState, Dword) -> Long,
    connect: unsafe extern "C" fn(Long, *const c_char, Dword, Dword, *mut Long, *mut Dword) -> Long,
    reconnect: unsafe extern "C" fn(Long, Dword, Dword, Dword, *mut Dword) -> Long,
    disconnect: unsafe extern "C" fn(Long, Dword) -> Long,
    begin: unsafe extern "C" fn(Long) -> Long,
    end: unsafe extern "C" fn(Long, Dword) -> Long,
    status: unsafe extern "C" fn(Long, *mut c_char, *mut Dword, *mut Dword, *mut Dword, *mut u8, *mut Dword) -> Long,
    transmit: unsafe extern "C" fn(Long, *const PcscIoRequest, *const u8, Dword, *mut PcscIoRequest, *mut u8, *mut Dword) -> Long,
    control: unsafe extern "C" fn(Long, Dword, *const c_void, Dword, *mut c_void, Dword, *mut Dword) -> Long,
    get_attrib: unsafe extern "C" fn(Long, Dword, *mut u8, *mut Dword) -> Long,
}

impl Api {
    fn load() -> Option<Self> {
        // SAFETY: libpcsclite has no load-time side effects beyond defining symbols.
        let lib = ["libpcsclite.so.1", "libpcsclite.so"].iter().find_map(|n| unsafe { Library::new(n).ok() })?;
        macro_rules! sym {
            ($n:literal) => {
                // SAFETY: the declared signatures are the documented libpcsclite ones for this platform.
                *unsafe { lib.get($n).ok()? }
            };
        }
        Some(Self {
            establish: sym!(b"SCardEstablishContext\0"),
            release: sym!(b"SCardReleaseContext\0"),
            is_valid: sym!(b"SCardIsValidContext\0"),
            cancel: sym!(b"SCardCancel\0"),
            list_readers: sym!(b"SCardListReaders\0"),
            status_change: sym!(b"SCardGetStatusChange\0"),
            connect: sym!(b"SCardConnect\0"),
            reconnect: sym!(b"SCardReconnect\0"),
            disconnect: sym!(b"SCardDisconnect\0"),
            begin: sym!(b"SCardBeginTransaction\0"),
            end: sym!(b"SCardEndTransaction\0"),
            status: sym!(b"SCardStatus\0"),
            transmit: sym!(b"SCardTransmit\0"),
            control: sym!(b"SCardControl\0"),
            get_attrib: sym!(b"SCardGetAttrib\0"),
            _lib: lib,
        })
    }
}

/// A PC/SC result as the RDP return code (libpcsclite uses the Windows values).
fn rc(code: Long) -> ReturnCode {
    match code as u32 {
        0 => ReturnCode::Success,
        0x8010_0002 => ReturnCode::Cancelled,
        0x8010_0003 => ReturnCode::InvalidHandle,
        0x8010_0004 => ReturnCode::InvalidParameter,
        0x8010_0008 => ReturnCode::InsufficientBuffer,
        0x8010_0009 => ReturnCode::UnknownReader,
        0x8010_000A => ReturnCode::Timeout,
        0x8010_000B => ReturnCode::SharingViolation,
        0x8010_000C => ReturnCode::NoSmartcard,
        0x8010_000F => ReturnCode::ProtoMismatch,
        0x8010_0013 => ReturnCode::CommError,
        0x8010_0016 => ReturnCode::NotTransacted,
        0x8010_0017 => ReturnCode::ReaderUnavailable,
        0x8010_001D => ReturnCode::NoService,
        0x8010_001E => ReturnCode::ServiceStopped,
        0x8010_0022 => ReturnCode::UnsupportedFeature,
        0x8010_002E => ReturnCode::NoReadersAvailable,
        _ => ReturnCode::InternalError,
    }
}

/// Windows protocol bits to libpcsclite's (T=0, T=1, raw is 0x10000 there and 4 here).
fn protocols_to_pcsc(p: CardProtocol) -> Dword {
    let b = p.bits();
    let mut out = b & 3;
    if b & 0x0001_0000 != 0 {
        out |= 4;
    }
    if b & 0x8000_0000 != 0 || out == 0 {
        out |= 3;
    }
    out as Dword
}

fn protocol_from_pcsc(p: Dword) -> CardProtocol {
    let mut b = (p as u32) & 3;
    if p & 4 != 0 {
        b |= 0x0001_0000;
    }
    CardProtocol::from_bits_retain(b)
}

/// libpcsclite's card state is a bit mask; RDP wants the Windows enumeration.
fn state_from_pcsc(s: Dword) -> CardState {
    match s {
        s if s & 0x40 != 0 => CardState::SpecificMode,
        s if s & 0x20 != 0 => CardState::Negotiable,
        s if s & 0x10 != 0 => CardState::Powered,
        s if s & 0x08 != 0 => CardState::Swallowed,
        s if s & 0x04 != 0 => CardState::Present,
        s if s & 0x02 != 0 => CardState::Absent,
        _ => CardState::Unknown,
    }
}

/// "a\0b\0\0" to ["a", "b"].
fn split_multistring(buf: &[u8]) -> Vec<String> {
    buf.split(|&b| b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
}

fn reply<T: ironrdp_rdpdr::pdu::esc::rpce::Encode + 'static>(req: DeviceControlRequest<ScardIoCtlCode>, ret: T) -> SvcMessage {
    SvcMessage::from(RdpdrPdu::DeviceControlResponse(DeviceControlResponse::new(req, NtStatus::SUCCESS, Some(Box::new(ret)))))
}

fn long(req: DeviceControlRequest<ScardIoCtlCode>, code: ReturnCode) -> SvcMessage {
    reply(req, LongReturn::new(code))
}

struct Shared {
    api: Api,
    /// Every context established, so they can be cancelled and released when the session ends.
    contexts: Mutex<HashSet<usize>>,
    deferred: Mutex<Vec<SvcMessage>>,
}

/// The smart-card side of an RDPDR backend; everything else goes to `inner`.
pub struct ScardBackend {
    inner: Box<dyn RdpdrBackend>,
    shared: Option<Arc<Shared>>,
}

impl std::fmt::Debug for ScardBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScardBackend").field("pcsc", &self.shared.is_some()).finish_non_exhaustive()
    }
}

ironrdp_core::impl_as_any!(ScardBackend);

impl ScardBackend {
    /// Wrap `inner`. Without libpcsclite the card calls fail with "no service", which is what a
    /// Windows machine without a smart-card service answers.
    pub fn new(inner: Box<dyn RdpdrBackend>) -> Self {
        let shared = Api::load().map(|api| Arc::new(Shared { api, contexts: Mutex::new(HashSet::new()), deferred: Mutex::new(Vec::new()) }));
        if shared.is_none() {
            log::warn!("smart cards: libpcsclite is not installed; no readers will be offered");
        }
        Self { inner, shared }
    }

    /// Wrap `inner` only to report what the server says about the devices offered (no PC/SC).
    pub fn reporting(inner: Box<dyn RdpdrBackend>) -> Self {
        Self { inner, shared: None }
    }

    pub fn available() -> bool {
        Api::load().is_some()
    }
}

impl Drop for ScardBackend {
    fn drop(&mut self) {
        if let Some(sh) = &self.shared {
            for c in sh.contexts.lock().unwrap().drain() {
                // SAFETY: valid contexts this module established.
                unsafe {
                    (sh.api.cancel)(c as Long);
                    (sh.api.release)(c as Long);
                }
            }
        }
    }
}

fn handle_of(h: &ScardHandle) -> Long {
    h.native() as Long
}

fn ctx_of(c: &ScardContext) -> Long {
    c.native() as Long
}

impl Shared {
    fn list_readers(&self, ctx: Long) -> (Long, Vec<String>) {
        let mut len: Dword = 0;
        // SAFETY: a NULL buffer asks for the length.
        let r = unsafe { (self.api.list_readers)(ctx, std::ptr::null(), std::ptr::null_mut(), &mut len) };
        if r != 0 || len == 0 {
            return (r, Vec::new());
        }
        let mut buf = vec![0u8; len as usize];
        // SAFETY: `buf` is `len` bytes.
        let r = unsafe { (self.api.list_readers)(ctx, std::ptr::null(), buf.as_mut_ptr().cast(), &mut len) };
        (r, if r == 0 { split_multistring(&buf[..(len as usize).min(buf.len())]) } else { Vec::new() })
    }

    /// `SCardGetStatusChange` for the server's reader list; the reply to send.
    fn status_change(&self, ctx: Long, timeout: u32, states: &[(String, ReaderStateCommonCall)]) -> (Long, Vec<ReaderStateCommonCall>) {
        let names: Vec<CString> = states.iter().map(|(n, _)| CString::new(n.replace('\0', "")).unwrap_or_default()).collect();
        let mut raw: Vec<PcscReaderState> = states
            .iter()
            .zip(&names)
            .map(|((_, s), n)| {
                let mut atr = [0u8; 33];
                let l = (s.atr_length as usize).min(33);
                atr[..l].copy_from_slice(&s.atr[..l]);
                PcscReaderState { reader: n.as_ptr(), user_data: std::ptr::null_mut(), current: Dword::from(s.current_state.bits()), event: 0, cb_atr: l as Dword, atr }
            })
            .collect();
        // SAFETY: `raw` and the names it points into outlive the call.
        let r = unsafe { (self.api.status_change)(ctx, Dword::from(timeout), raw.as_mut_ptr(), raw.len() as Dword) };
        let out = raw
            .iter()
            .zip(states)
            .map(|(p, (_, orig))| {
                let mut atr = [0u8; 36];
                let l = (p.cb_atr as usize).min(33);
                atr[..l].copy_from_slice(&p.atr[..l]);
                ReaderStateCommonCall { current_state: orig.current_state, event_state: CardStateFlags::from_bits_retain(p.event as u32), atr_length: l as u32, atr }
            })
            .collect();
        (r, out)
    }
}

impl RdpdrBackend for ScardBackend {
    fn supports_drive_security(&self) -> bool {
        self.inner.supports_drive_security()
    }

    fn reset(&mut self) -> PduResult<()> {
        self.inner.reset()
    }

    fn restore_drive(&mut self, device_id: u32) -> PduResult<()> {
        self.inner.restore_drive(device_id)
    }

    fn add_drive(&mut self, device_id: u32) -> PduResult<()> {
        self.inner.add_drive(device_id)
    }

    fn remove_drive(&mut self, device_id: u32) -> PduResult<Vec<SvcMessage>> {
        self.inner.remove_drive(device_id)
    }

    fn handle_server_device_announce_response(&mut self, pdu: ServerDeviceAnnounceResponse) -> PduResult<()> {
        // What the server said about each device offered (drive, printer, smart card).
        log::info!("RDP device {} announce result: {:?}", pdu.device_id, pdu.result_code);
        self.inner.handle_server_device_announce_response(pdu)
    }

    fn handle_drive_io_request(&mut self, req: ironrdp_rdpdr::pdu::efs::ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_drive_io_request(req)
    }

    fn handle_drive_device_control(&mut self, req: ironrdp_rdpdr::pdu::efs::DecodedDeviceControlRequest<ironrdp_rdpdr::pdu::efs::AnyIoCtlCode>) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_drive_device_control(req)
    }

    fn handle_user_logged_on(&mut self, rdpdr: &mut ironrdp_rdpdr::Rdpdr) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_user_logged_on(rdpdr)
    }

    fn handle_printer_io_request(&mut self, req: ironrdp_rdpdr::pdu::efs::PrinterIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_printer_io_request(req)
    }

    fn handle_port_io_request(&mut self, req: ironrdp_rdpdr::pdu::esp::PortIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_port_io_request(req)
    }

    fn reject_printer_write(&mut self, req: ironrdp_rdpdr::pdu::efs::DeviceIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.inner.reject_printer_write(req)
    }

    /// Completions of calls that waited (and answers the inner backend deferred).
    fn poll_deferred_messages(&mut self) -> PduResult<Vec<SvcMessage>> {
        let mut out = self.inner.poll_deferred_messages()?;
        if let Some(sh) = &self.shared {
            out.append(&mut sh.deferred.lock().unwrap());
        }
        Ok(out)
    }

    fn handle_scard_call(&mut self, req: DeviceControlRequest<ScardIoCtlCode>, call: ScardCall) -> PduResult<Vec<SvcMessage>> {
        log::debug!("smart card call from the server: {:?}", req.io_control_code);
        let Some(sh) = self.shared.clone() else { return Ok(vec![long(req, ReturnCode::NoService)]) };
        let api = &sh.api;
        let code = req.io_control_code;
        let msg = match call {
            ScardCall::AccessStartedEventCall(_) => long(req, ReturnCode::Success),
            ScardCall::EstablishContextCall(c) => {
                let mut ctx: Long = 0;
                // SAFETY: `ctx` is a valid out-pointer.
                let r = unsafe { (api.establish)(c.scope as Dword, std::ptr::null(), std::ptr::null(), &mut ctx) };
                log::debug!("smart card: SCardEstablishContext -> {:#x}", r as u32);
                if r == 0 {
                    sh.contexts.lock().unwrap().insert(ctx as usize);
                }
                reply(req, EstablishContextReturn::new(rc(r), ScardContext::from_native(ctx as usize)))
            }
            ScardCall::ContextCall(c) => {
                let ctx = ctx_of(&c.context);
                // SAFETY: plain calls on a context handle; an invalid one is reported by the library.
                let r = unsafe {
                    match code {
                        ScardIoCtlCode::ReleaseContext => {
                            sh.contexts.lock().unwrap().remove(&(ctx as usize));
                            (api.release)(ctx)
                        }
                        ScardIoCtlCode::IsValidContext => (api.is_valid)(ctx),
                        _ => (api.cancel)(ctx),
                    }
                };
                long(req, rc(r))
            }
            ScardCall::ListReaderGroupsCall(_) => {
                let enc = if matches!(code, ScardIoCtlCode::ListReaderGroupsA) { CharacterSet::Ansi } else { CharacterSet::Unicode };
                reply(req, ListReadersReturn::new(ReturnCode::Success, vec!["SCard$DefaultReaders".to_string()], enc))
            }
            ScardCall::ListReadersCall(c) => {
                let enc = if matches!(code, ScardIoCtlCode::ListReadersA) { CharacterSet::Ansi } else { CharacterSet::Unicode };
                let (r, readers) = sh.list_readers(ctx_of(&c.context));
                log::debug!("smart card: SCardListReaders -> {:#x}, readers {:?}", r as u32, readers);
                let code = if r == 0 && readers.is_empty() { ReturnCode::NoReadersAvailable } else { rc(r) };
                reply(req, ListReadersReturn::new(code, readers, enc))
            }
            ScardCall::GetStatusChangeCall(c) => {
                let states: Vec<(String, ReaderStateCommonCall)> = c.states.iter().map(|s| (s.reader.clone(), s.common.clone())).collect();
                let ctx = ctx_of(&c.context);
                if c.timeout == 0 {
                    let (r, out) = sh.status_change(ctx, 0, &states);
                    reply(req, GetStatusChangeReturn::new(rc(r), out))
                } else {
                    // May wait for a card to come or go: finish it off the session thread.
                    let sh2 = sh.clone();
                    let timeout = c.timeout;
                    std::thread::Builder::new()
                        .name("rdp-scard-wait".into())
                        .spawn(move || {
                            let (r, out) = sh2.status_change(ctx, timeout, &states);
                            sh2.deferred.lock().unwrap().push(reply(req, GetStatusChangeReturn::new(rc(r), out)));
                        })
                        .ok();
                    return Ok(Vec::new());
                }
            }
            ScardCall::ConnectCall(c) => {
                let reader = CString::new(c.reader.replace('\0', "")).unwrap_or_default();
                let (mut h, mut active): (Long, Dword) = (0, 0);
                // SAFETY: valid C string and out-pointers.
                let r = unsafe { (api.connect)(ctx_of(&c.common.context), reader.as_ptr(), Dword::from(c.common.share_mode), protocols_to_pcsc(c.common.preferred_protocols), &mut h, &mut active) };
                reply(req, ConnectReturn::new(rc(r), ScardHandle::from_native(c.common.context, h as usize), protocol_from_pcsc(active)))
            }
            ScardCall::ReconnectCall(c) => {
                let mut active: Dword = 0;
                // SAFETY: valid handle value and out-pointer.
                let r = unsafe { (api.reconnect)(handle_of(&c.handle), Dword::from(c.share_mode), protocols_to_pcsc(c.preferred_protocols), Dword::from(c.initialization), &mut active) };
                reply(req, ReconnectReturn::new(rc(r), protocol_from_pcsc(active)))
            }
            ScardCall::HCardAndDispositionCall(c) => {
                let h = handle_of(&c.handle);
                // SAFETY: plain calls on a card handle.
                let r = unsafe {
                    match code {
                        ScardIoCtlCode::BeginTransaction => (api.begin)(h),
                        ScardIoCtlCode::EndTransaction => (api.end)(h, Dword::from(c.disposition)),
                        _ => (api.disconnect)(h, Dword::from(c.disposition)),
                    }
                };
                long(req, rc(r))
            }
            ScardCall::TransmitCall(c) => {
                let send = PcscIoRequest { protocol: protocols_to_pcsc(c.send_pci.protocol), pci_length: std::mem::size_of::<PcscIoRequest>() as Dword };
                let mut recv_pci = PcscIoRequest { protocol: send.protocol, pci_length: send.pci_length };
                let cap = if c.recv_length == 0 { 264 } else { (c.recv_length as usize).min(66_000) };
                let mut buf = vec![0u8; cap];
                let mut n = cap as Dword;
                // SAFETY: buffers are sized as declared.
                let r = unsafe { (api.transmit)(handle_of(&c.handle), &send, c.send_buffer.as_ptr(), c.send_buffer.len() as Dword, &mut recv_pci, buf.as_mut_ptr(), &mut n) };
                buf.truncate(if r == 0 { n as usize } else { 0 });
                let pci = SCardIORequest { protocol: protocol_from_pcsc(recv_pci.protocol), extra_bytes_length: 0, extra_bytes: Vec::new() };
                reply(req, TransmitReturn::new(rc(r), Some(pci), buf))
            }
            ScardCall::StatusCall(c) => {
                let mut names = vec![0u8; 256];
                let (mut nlen, mut state, mut proto, mut atr_len): (Dword, Dword, Dword, Dword) = (256, 0, 0, 33);
                let mut atr = [0u8; 33];
                // SAFETY: buffers match the lengths passed.
                let r = unsafe { (api.status)(handle_of(&c.handle), names.as_mut_ptr().cast(), &mut nlen, &mut state, &mut proto, atr.as_mut_ptr(), &mut atr_len) };
                let readers = if r == 0 { split_multistring(&names[..(nlen as usize).min(names.len())]) } else { Vec::new() };
                let mut a32 = [0u8; 32];
                let l = (atr_len as usize).min(32);
                a32[..l].copy_from_slice(&atr[..l]);
                reply(req, StatusReturn::new(rc(r), readers, state_from_pcsc(state), protocol_from_pcsc(proto), a32, l as u32, if matches!(code, ScardIoCtlCode::StatusA) { CharacterSet::Ansi } else { CharacterSet::Unicode }))
            }
            ScardCall::ControlCall(c) => {
                let cap = if c.out_buffer_is_null { 0 } else { (c.out_buffer_size as usize).min(66_000) };
                let mut out = vec![0u8; cap];
                let mut n: Dword = 0;
                // SAFETY: buffers match the lengths passed.
                let r = unsafe { (api.control)(handle_of(&c.handle), Dword::from(c.control_code), c.in_buffer.as_ptr().cast(), c.in_buffer.len() as Dword, out.as_mut_ptr().cast(), cap as Dword, &mut n) };
                out.truncate(if r == 0 { n as usize } else { 0 });
                reply(req, ControlReturn::new(rc(r), out))
            }
            ScardCall::GetAttribCall(c) => {
                let cap = if c.attr_is_null { 0 } else { (c.attr_length as usize).min(66_000) };
                let mut out = vec![0u8; cap.max(1)];
                let mut n = out.len() as Dword;
                // SAFETY: buffers match the lengths passed.
                let r = unsafe { (api.get_attrib)(handle_of(&c.handle), Dword::from(c.attr_id), out.as_mut_ptr(), &mut n) };
                out.truncate(if r == 0 { (n as usize).min(out.len()) } else { 0 });
                reply(req, GetAttribReturn::new(rc(r), out))
            }
            // Reader management, caches, locating cards by name and so on: not offered.
            _ => long(req, ReturnCode::UnsupportedFeature),
        };
        Ok(vec![msg])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multistrings_split_into_names() {
        assert_eq!(split_multistring(b"Reader A\0Reader B\0\0"), vec!["Reader A", "Reader B"]);
        assert!(split_multistring(b"\0\0").is_empty());
    }

    #[test]
    fn protocols_and_states_translate_between_windows_and_pcsc_values() {
        assert_eq!(protocols_to_pcsc(CardProtocol::SCARD_PROTOCOL_T1), 2);
        assert_eq!(protocols_to_pcsc(CardProtocol::SCARD_PROTOCOL_RAW), 4);
        assert_eq!(protocols_to_pcsc(CardProtocol::SCARD_PROTOCOL_DEFAULT), 3);
        assert_eq!(protocol_from_pcsc(4), CardProtocol::SCARD_PROTOCOL_RAW);
        assert!(matches!(state_from_pcsc(0x04), CardState::Present));
        assert!(matches!(state_from_pcsc(0x02), CardState::Absent));
        assert!(matches!(state_from_pcsc(0x10 | 0x04), CardState::Powered));
    }

    /// Against the real PC/SC service when there is one (readers may be none).
    #[test]
    fn the_pcsc_service_answers_context_and_reader_calls() {
        let Some(api) = Api::load() else { return };
        let sh = Shared { api, contexts: Mutex::new(HashSet::new()), deferred: Mutex::new(Vec::new()) };
        let mut ctx: Long = 0;
        // SAFETY: valid out-pointer.
        let r = unsafe { (sh.api.establish)(2, std::ptr::null(), std::ptr::null(), &mut ctx) };
        if r != 0 {
            return; // no pcscd running here
        }
        let (r, readers) = sh.list_readers(ctx);
        // Success with readers, or "no readers available".
        assert!(r == 0 || r as u32 == 0x8010_002E, "{r:#x}");
        eprintln!("PC/SC readers here: {readers:?}");
        // SAFETY: the context made above.
        unsafe { (sh.api.release)(ctx) };
    }
}
