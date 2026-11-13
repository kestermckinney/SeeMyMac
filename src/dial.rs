//! Opening a TCP connection to a host name the way a user expects on a mix of LAN and Tailscale.
//!
//! A name such as `pauls-legion` can resolve through several search domains at once: the Tailscale
//! MagicDNS name (`100.x.y.z`) and the home router's name (`pauls-legion.attlocal.net`), which often
//! hands back global or temporary IPv6 addresses that are only reachable on that LAN. Taking the first
//! address the resolver returns (often IPv6) then fails or hangs whenever the Mac is away from home,
//! so every address is tried in turn: Tailscale first, then IPv4, then IPv6.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

/// How long to keep trying before giving up on the host.
const DEADLINE: Duration = Duration::from_secs(45);
/// How long an attempt runs alone before the next address is tried alongside it.
const STAGGER: Duration = Duration::from_secs(2);
/// How often to start a fresh attempt to a Tailscale address that has not answered yet.
const TS_REPEAT: Duration = Duration::from_secs(3);

/// True for Tailscale's CGNAT IPv4 range (100.64.0.0/10) and its IPv6 ULA prefix (fd7a:115c:a1e0::/48).
pub fn is_tailscale(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Link-local IPv6 (fe80::/10) needs a scope id the resolver rarely supplies; it never routes.
fn is_link_local_v6(ip: &IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
}

/// Order addresses: Tailscale, then other IPv4, then IPv6; drop duplicates and unusable link-local ones.
pub fn order(addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let mut v: Vec<SocketAddr> = Vec::new();
    for a in addrs {
        if !is_link_local_v6(&a.ip()) && !v.contains(&a) {
            v.push(a);
        }
    }
    v.sort_by_key(|a| (!is_tailscale(&a.ip()), a.is_ipv6()));
    v
}

/// Ask the local Tailscale daemon for a peer's address. Used when the system resolver knows nothing
/// of the name (MagicDNS off, or the single-label name was not expanded by the search domain).
pub fn tailscale_lookup(host: &str, v4: bool) -> Option<IpAddr> {
    if host.parse::<IpAddr>().is_ok() || host.is_empty() || host.contains(char::is_whitespace) {
        return None;
    }
    let flag = if v4 { "-4" } else { "-6" };
    for exe in tailscale_cli() {
        let Ok(out) = Command::new(exe).args(["ip", flag, host]).output() else { continue };
        if !out.status.success() {
            continue;
        }
        if let Some(ip) = String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.trim().parse::<IpAddr>().ok()) {
            return Some(ip);
        }
    }
    None
}

fn tailscale_cli() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["tailscale", "/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale", "/Applications/Tailscale.app/Contents/MacOS/Tailscale"]
    } else if cfg!(windows) {
        &["tailscale", r"C:\Program Files\Tailscale\tailscale.exe"]
    } else {
        &["tailscale"]
    }
}

/// Every address `host:port` could mean, best first. Falls back to Tailscale when DNS fails.
pub fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let found = match (host, port).to_socket_addrs() {
        Ok(it) => order(it),
        Err(e) => match tailscale_lookup(host, true) {
            Some(ip) => vec![SocketAddr::new(ip, port)],
            None => return Err(anyhow!("cannot resolve {host}: {e}")),
        },
    };
    if found.is_empty() {
        return Err(anyhow!("no address for {host}"));
    }
    Ok(found)
}

/// Connect to `host:port`, racing the resolved addresses until one answers.
///
/// Attempts start one address at a time, `STAGGER` apart, best first, and none is cancelled while
/// waiting: an earlier one that is merely slow can still win. A Tailscale peer reached through a DERP
/// relay drops the first packets while its tunnel wakes up (5–25 s when the path has been idle), and
/// TCP's own SYN retries back off to 8 and 16 s gaps, so a fresh attempt to each Tailscale address is
/// also started every `TS_REPEAT` to catch the tunnel soon after it comes up.
pub fn connect(host: &str, port: u16, on_try: impl FnMut(&SocketAddr)) -> Result<TcpStream> {
    connect_within(host, port, DEADLINE, on_try)
}

fn connect_within(host: &str, port: u16, limit: Duration, mut on_try: impl FnMut(&SocketAddr)) -> Result<TcpStream> {
    let mut addrs = resolve(host, port)?;
    let start = Instant::now();
    let deadline = start + limit;
    let (tx, rx) = mpsc::channel::<(SocketAddr, io::Result<TcpStream>)>();
    let launch = |a: SocketAddr| {
        let tx = tx.clone();
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(100));
        thread::spawn(move || {
            // A late winner after another attempt already connected is simply dropped (closed).
            let _ = tx.send((a, TcpStream::connect_timeout(&a, left)));
        });
    };

    let mut launched = 0; // addrs[..launched] have had a first attempt
    let mut next_at = start;
    let mut asked_tailscale = false;
    let mut repeat_at: Option<Instant> = None;
    let mut pending = 0usize;
    let mut failed: Vec<(SocketAddr, String)> = Vec::new(); // addresses that answered with an error
    let is_dead = |failed: &Vec<(SocketAddr, String)>, a: &SocketAddr| failed.iter().any(|(f, _)| f == a);

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_at {
            if launched < addrs.len() {
                let a = addrs[launched];
                launched += 1;
                on_try(&a);
                launch(a);
                pending += 1;
                next_at = now + STAGGER;
                if is_tailscale(&a.ip()) && repeat_at.is_none() {
                    repeat_at = Some(now + TS_REPEAT);
                }
                continue;
            } else if !asked_tailscale {
                // DNS gave only LAN addresses and none has answered: the daemon may still know the peer.
                asked_tailscale = true;
                if !addrs.iter().any(|a| is_tailscale(&a.ip())) {
                    if let Some(ip) = tailscale_lookup(host, true) {
                        addrs.push(SocketAddr::new(ip, port));
                        continue;
                    }
                }
            }
            next_at = deadline;
        }
        let live_ts: Vec<SocketAddr> = addrs[..launched].iter().copied().filter(|a| is_tailscale(&a.ip()) && !is_dead(&failed, a)).collect();
        if let Some(t) = repeat_at {
            if now >= t {
                for a in &live_ts {
                    log::info!("{host}: still waiting on {a}, trying it again");
                    launch(*a);
                    pending += 1;
                }
                repeat_at = Some(now + TS_REPEAT);
            }
        }
        // Every address has answered with an error and nothing is left to try.
        if pending == 0 && launched == addrs.len() && asked_tailscale && live_ts.is_empty() {
            break;
        }
        let wake = [Some(next_at), repeat_at, Some(deadline)].into_iter().flatten().min().unwrap_or(deadline);
        match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok((a, Ok(s))) => {
                log::info!("{host}: connected via {a} after {} ms", start.elapsed().as_millis());
                return Ok(s);
            }
            Ok((a, Err(e))) => {
                pending -= 1;
                log::info!("{host}: {a} failed: {e}");
                if !is_dead(&failed, &a) {
                    failed.push((a, short(&e)));
                }
                // Refused or unreachable: no point waiting on the stagger for the next address.
                if launched < addrs.len() {
                    next_at = Instant::now();
                }
            }
            Err(_) => {}
        }
    }
    let why: Vec<String> = addrs[..launched]
        .iter()
        .map(|a| match failed.iter().find(|(f, _)| f == a) {
            Some((_, e)) => format!("{a}: {e}"),
            None => format!("{a}: timed out"),
        })
        .collect();
    Err(anyhow!("cannot reach {host}:{port} ({})", why.join("; ")))
}

fn short(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => "timed out".into(),
        io::ErrorKind::ConnectionRefused => "refused".into(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn tailscale_ranges() {
        assert!(is_tailscale(&"100.81.185.73".parse().unwrap()));
        assert!(is_tailscale(&"100.64.0.1".parse().unwrap()));
        assert!(is_tailscale(&"100.127.255.255".parse().unwrap()));
        assert!(!is_tailscale(&"100.128.0.1".parse().unwrap()));
        assert!(!is_tailscale(&"100.63.0.1".parse().unwrap()));
        assert!(is_tailscale(&"fd7a:115c:a1e0::53".parse().unwrap()));
        assert!(!is_tailscale(&"2600:1700:4e20:8b90::1b".parse().unwrap()));
    }

    #[test]
    fn connects_to_a_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let s = connect_within("127.0.0.1", port, Duration::from_secs(5), |_| {}).unwrap();
        assert_eq!(s.peer_addr().unwrap().port(), port);
    }

    #[test]
    fn a_refused_port_fails_fast() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let t = Instant::now();
        let e = connect_within("127.0.0.1", port, Duration::from_secs(20), |_| {}).unwrap_err().to_string();
        assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
        assert!(e.contains("cannot reach") && e.contains("refused"), "{e}");
    }

    #[test]
    fn orders_tailscale_then_v4_then_v6() {
        let got = order(vec![
            sa("[2600:1700:4e20:8b90::1b]:3389"),
            sa("[fe80::1]:3389"),
            sa("192.168.1.174:3389"),
            sa("[2600:1700:4e20:8b90::1b]:3389"),
            sa("100.81.185.73:3389"),
        ]);
        assert_eq!(got, vec![sa("100.81.185.73:3389"), sa("192.168.1.174:3389"), sa("[2600:1700:4e20:8b90::1b]:3389")]);
    }
}
