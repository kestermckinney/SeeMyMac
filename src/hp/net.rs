//! Small blocking-socket helpers for the HP handshake.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// The Mac stopped answering mid-exchange (busy or still waking); worth retrying.
#[derive(Debug)]
pub struct Stalled;

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The Mac stopped responding during setup.")
    }
}

impl std::error::Error for Stalled {}

pub struct Tcp {
    pub s: TcpStream,
}

pub fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

impl Tcp {
    pub fn connect(addr: SocketAddrV4, timeout: Duration) -> Result<Self> {
        let s = TcpStream::connect_timeout(&SocketAddr::V4(addr), timeout).map_err(|e| match e.kind() {
            ErrorKind::ConnectionRefused => anyhow!("The Mac refused the screen-sharing connection. Turn on Screen Sharing in System Settings and check the address."),
            _ if is_timeout(&e) => anyhow!("The Mac did not respond. Check that it is awake and reachable."),
            _ => anyhow!("Could not reach the Mac: {e}"),
        })?;
        s.set_nodelay(true).ok();
        s.set_read_timeout(Some(Duration::from_secs(15)))?;
        s.set_write_timeout(Some(Duration::from_secs(15)))?;
        Ok(Self { s })
    }

    pub fn send(&mut self, b: &[u8]) -> Result<()> {
        self.s.write_all(b).context("sending to the Mac")
    }

    pub fn read_exact(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut b = vec![0u8; n];
        self.s.read_exact(&mut b).map_err(|e| {
            if is_timeout(&e) {
                anyhow::Error::new(Stalled)
            } else if e.kind() == ErrorKind::UnexpectedEof {
                anyhow!("The Mac closed the connection during setup.")
            } else {
                anyhow!("Reading from the Mac failed: {e}")
            }
        })?;
        Ok(b)
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.read_exact(4)?.try_into().unwrap()))
    }

    /// Read exactly `n` bytes before `deadline`.
    pub fn read_by(&mut self, n: usize, deadline: Instant, what: &str) -> Result<Vec<u8>> {
        let mut b = vec![0u8; n];
        let mut got = 0;
        while got < n {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("Timed out waiting for {what}.");
            }
            self.s.set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
            match self.s.read(&mut b[got..]) {
                Ok(0) => bail!("The Mac closed the connection while waiting for {what}."),
                Ok(k) => got += k,
                Err(e) if is_timeout(&e) => continue,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        self.s.set_read_timeout(Some(Duration::from_secs(15)))?;
        Ok(b)
    }

    /// One `recv` with a timeout; `Ok(None)` on timeout, error on close.
    pub fn recv_some(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        self.s.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        let mut b = vec![0u8; 65536];
        let r = match self.s.read(&mut b) {
            Ok(0) => Err(anyhow!("The Mac closed the connection.")),
            Ok(n) => {
                b.truncate(n);
                Ok(Some(b))
            }
            Err(e) if is_timeout(&e) || e.kind() == ErrorKind::Interrupted => Ok(None),
            Err(e) => Err(e.into()),
        };
        self.s.set_read_timeout(Some(Duration::from_secs(15)))?;
        r
    }

    /// Wait up to `wait` for `want` to appear at the head of the stream, without consuming it.
    /// Returns false as soon as the buffered bytes can't be that prefix.
    pub fn peek_prefix(&mut self, want: &[u8], wait: Duration) -> Result<bool> {
        let deadline = Instant::now() + wait;
        let mut b = vec![0u8; want.len()];
        let r = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Ok(false);
            }
            if let Err(e) = self.s.set_read_timeout(Some(left)) {
                break Err(e.into());
            }
            match self.s.peek(&mut b) {
                Ok(0) => break Ok(false),
                Ok(n) if !want.starts_with(&b[..n]) => break Ok(false),
                Ok(n) if n == want.len() => break Ok(true),
                Ok(_) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) if is_timeout(&e) => break Ok(false),
                Err(e) => break Err(e.into()),
            }
        };
        let _ = self.s.set_read_timeout(Some(Duration::from_secs(15)));
        r
    }

    /// Close with an RST so screensharingd releases the session immediately.
    pub fn abort(self) {
        let _ = socket2::SockRef::from(&self.s).set_linger(Some(Duration::ZERO));
        let _ = self.s.shutdown(Shutdown::Both);
    }
}

/// The HP media transport is IPv4-only, so the control connection must be too.
pub fn resolve_ipv4(host: &str, port: u16) -> Result<SocketAddrV4> {
    let addrs = (host, port).to_socket_addrs().map_err(|e| anyhow!("Could not resolve {host}: {e}"))?;
    for a in addrs {
        if let SocketAddr::V4(v4) = a {
            return Ok(v4);
        }
    }
    bail!("{host} has no IPv4 address. Apple High Performance needs IPv4; enter the Mac's IPv4 address instead.")
}

/// Bind a media socket on the same local port the Mac sends from (it streams symmetrically).
pub fn bind_udp(port: u16) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    let _ = s.set_recv_buffer_size(8 * 1024 * 1024);
    s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into()).map_err(|e| {
        if e.kind() == ErrorKind::AddrInUse {
            anyhow!("Local UDP port {port} is already in use. Close other screen-sharing clients and try again.")
        } else {
            anyhow!("Could not open local UDP port {port}: {e}")
        }
    })?;
    let u: UdpSocket = s.into();
    u.set_read_timeout(Some(Duration::from_millis(200)))?;
    Ok(u)
}
