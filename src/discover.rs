//! Finding computers on the local network that can be remote-controlled, via Bonjour (mDNS/DNS-SD).
//!
//! Macs with Screen Sharing turned on, and most VNC servers, advertise `_rfb._tcp`; RDP hosts that
//! advertise themselves use `_rdp._tcp`. The browse runs on `mdns-sd`'s own thread, so it needs no
//! system Bonjour service and works the same on Windows, macOS and Linux.
//!
//! Bonjour does not cross a Tailscale tunnel, so the tailnet is searched separately: the Tailscale
//! CLI lists the online peers, and each is checked for an open VNC (5900) or RDP (3389) port.

use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const VNC_SERVICE: &str = "_rfb._tcp.local.";
const RDP_SERVICE: &str = "_rdp._tcp.local.";

/// How long the search window says "Searching…" before it reports what it has.
pub const SEARCH_TIME: Duration = Duration::from_secs(5);

/// How long to wait for a Tailscale peer's port to answer. A peer reached through a DERP relay can
/// take a few seconds to answer the first time.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a computer was found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Bonjour,
    Tailscale,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Screen Sharing or another VNC server (`_rfb._tcp`).
    Vnc,
    Rdp,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Vnc => "Screen Sharing / VNC",
            Kind::Rdp => "Remote Desktop",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Kind::Vnc => 5900,
            Kind::Rdp => 3389,
        }
    }
}

/// One advertised service on one computer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Found {
    /// The DNS-SD full name, unique per service: "Paul's Mac._rfb._tcp.local.".
    pub fullname: String,
    /// The name the computer gives itself: "Paul's Mac".
    pub name: String,
    /// Its Bonjour host name without the trailing dot: "pauls-mac.local".
    pub host: String,
    /// Its first IPv4 address (or IPv6 when it has none), shown beside the host name.
    pub ip: Option<IpAddr>,
    pub port: u16,
    pub kind: Kind,
    pub source: Source,
}

/// An online Tailscale peer that could be running a remote-desktop server.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Peer {
    pub name: String,
    /// MagicDNS name without the trailing dot ("pauls-imac.tail1234.ts.net"), or the IP if none.
    pub host: String,
    pub ip: IpAddr,
}

/// The online peers in `tailscale status --json` output, leaving out this computer and phones or
/// tablets (iOS and Android have no remote-desktop server to reach).
pub fn parse_tailscale_status(json: &str) -> Vec<Peer> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return vec![] };
    let Some(peers) = v.get("Peer").and_then(|p| p.as_object()) else { return vec![] };
    let mut out = vec![];
    for p in peers.values() {
        if p.get("Online").and_then(|o| o.as_bool()) != Some(true) {
            continue;
        }
        let os = p.get("OS").and_then(|o| o.as_str()).unwrap_or("").to_lowercase();
        if os == "ios" || os == "android" {
            continue;
        }
        let ips: Vec<IpAddr> = p.get("TailscaleIPs").and_then(|a| a.as_array()).map_or(vec![], |a| a.iter().filter_map(|i| i.as_str()?.parse().ok()).collect());
        let Some(ip) = ips.iter().find(|i| i.is_ipv4()).or(ips.first()).copied() else { continue };
        let dns = p.get("DNSName").and_then(|d| d.as_str()).unwrap_or("").trim_end_matches('.');
        let host = if dns.is_empty() { ip.to_string() } else { dns.to_string() };
        let name = p.get("HostName").and_then(|h| h.as_str()).filter(|h| !h.is_empty()).map_or_else(|| if dns.is_empty() { host.clone() } else { dns.split('.').next().unwrap_or(dns).to_string() }, str::to_string);
        out.push(Peer { name, host, ip });
    }
    out
}

/// Ask Tailscale for its peers and report each open VNC or RDP port on `tx` as it answers.
/// Every probe runs on its own thread; the channel closes when the last one finishes.
fn search_tailscale(tx: mpsc::Sender<Found>) {
    let mut json = None;
    for exe in crate::dial::tailscale_cli() {
        match crate::dial::tailscale_command(exe).args(["status", "--json"]).output() {
            Ok(out) if out.status.success() => {
                json = Some(String::from_utf8_lossy(&out.stdout).into_owned());
                break;
            }
            _ => continue,
        }
    }
    let Some(json) = json else {
        log::info!("Tailscale is not installed or not running; searching Bonjour only");
        return;
    };
    for peer in parse_tailscale_status(&json) {
        for kind in [Kind::Vnc, Kind::Rdp] {
            let (tx, peer) = (tx.clone(), peer.clone());
            std::thread::spawn(move || {
                let port = kind.default_port();
                if TcpStream::connect_timeout(&SocketAddr::new(peer.ip, port), PROBE_TIMEOUT).is_ok() {
                    log::info!("Tailscale peer {} answers on {port}", peer.name);
                    let fullname = format!("tailscale:{}:{port}", peer.host);
                    let _ = tx.send(Found { fullname, name: peer.name, host: peer.host, ip: Some(peer.ip), port, kind, source: Source::Tailscale });
                }
            });
        }
    }
}

/// The instance part of a full service name: "Paul's Mac._rfb._tcp.local." → "Paul's Mac".
/// DNS-SD escapes dots and backslashes in instance names (`\.`, `\\`, `\032`); undo that.
pub fn instance_name(fullname: &str, ty_domain: &str) -> String {
    let raw = fullname.strip_suffix(ty_domain).unwrap_or(fullname).trim_end_matches('.');
    let mut out = Vec::with_capacity(raw.len());
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
            let n: u32 = std::str::from_utf8(&b[i + 1..i + 4]).unwrap().parse().unwrap();
            out.push(n.min(255) as u8);
            i += 4;
        } else if b[i] == b'\\' && i + 1 < b.len() {
            out.push(b[i + 1]);
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Add or refresh one service, keeping the list sorted by name then kind.
pub fn upsert(list: &mut Vec<Found>, f: Found) {
    match list.iter_mut().find(|e| e.fullname == f.fullname) {
        Some(e) => *e = f,
        None => list.push(f),
    }
    list.sort_by(|a, b| {
        a.name.to_lowercase().cmp(&b.name.to_lowercase()).then((a.source == Source::Tailscale).cmp(&(b.source == Source::Tailscale))).then((a.kind == Kind::Rdp).cmp(&(b.kind == Kind::Rdp)))
    });
}

/// A running search. Dropping it stops the search and its network thread.
pub struct Discovery {
    daemon: Option<ServiceDaemon>,
    browses: Vec<(Kind, Receiver<ServiceEvent>)>,
    /// Tailscale peers whose ports answered; `None` once every probe has finished.
    tailscale: Option<mpsc::Receiver<Found>>,
    pub found: Vec<Found>,
    /// Why the search could not start (no usable network interface, port 5353 refused, …).
    pub error: Option<String>,
    pub started: Instant,
    /// Highlighted row (arrow keys and Enter).
    pub sel: usize,
    /// "Searching…" was showing the last time the window was drawn.
    pub(crate) searching_shown: bool,
}

impl Discovery {
    /// Start browsing for VNC and RDP hosts, on the local network and on the tailnet.
    pub fn start() -> Self {
        let mut d = Self::with_results(vec![]);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || search_tailscale(tx));
        d.tailscale = Some(rx);
        match ServiceDaemon::new() {
            Ok(daemon) => {
                for (kind, ty) in [(Kind::Vnc, VNC_SERVICE), (Kind::Rdp, RDP_SERVICE)] {
                    match daemon.browse(ty) {
                        Ok(rx) => d.browses.push((kind, rx)),
                        Err(e) => log::warn!("Bonjour browse for {ty} failed: {e}"),
                    }
                }
                if d.browses.is_empty() {
                    d.error = Some("Could not search the local network.".into());
                }
                d.daemon = Some(daemon);
            }
            Err(e) => {
                log::warn!("could not start Bonjour discovery: {e}");
                d.error = Some(format!("Could not search the local network: {e}"));
            }
        }
        d
    }

    /// A search that never touches the network, already holding `found` (for tests).
    pub fn with_results(found: Vec<Found>) -> Self {
        Self { daemon: None, browses: vec![], tailscale: None, found, error: None, started: Instant::now(), sel: 0, searching_shown: true }
    }

    /// Still in the first few seconds, when more computers are likely to answer.
    pub fn searching(&self) -> bool {
        (self.daemon.is_some() && self.started.elapsed() < SEARCH_TIME) || self.tailscale.is_some()
    }

    /// Take what the network thread has seen. Returns whether anything shown changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let mut events = vec![];
        for (kind, rx) in &self.browses {
            while let Ok(e) = rx.try_recv() {
                events.push((*kind, e));
            }
        }
        for (kind, e) in events {
            match e {
                ServiceEvent::ServiceResolved(s) => {
                    let ip = s.addresses.iter().map(|a| a.to_ip_addr()).filter(|a| !crate::dial::is_link_local_v6(a)).min_by_key(|a| a.is_ipv6());
                    let host = s.host.trim_end_matches('.').to_string();
                    let f = Found { name: instance_name(&s.fullname, &s.ty_domain), fullname: s.fullname.clone(), host, ip, port: s.port, kind, source: Source::Bonjour };
                    log::info!("found {} at {}:{} ({:?})", f.name, f.host, f.port, kind);
                    upsert(&mut self.found, f);
                    changed = true;
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let n = self.found.len();
                    self.found.retain(|f| f.fullname != fullname);
                    changed |= self.found.len() != n;
                }
                _ => {}
            }
        }
        if let Some(rx) = &self.tailscale {
            loop {
                match rx.try_recv() {
                    Ok(f) => {
                        upsert(&mut self.found, f);
                        changed = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.tailscale = None;
                        break;
                    }
                }
            }
        }
        self.sel = self.sel.min(self.found.len().saturating_sub(1));
        if self.searching_shown != self.searching() {
            self.searching_shown = self.searching();
            changed = true;
        }
        changed
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(d) = self.daemon.take() {
            let _ = d.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(full: &str, name: &str, kind: Kind) -> Found {
        Found { fullname: full.into(), name: name.into(), host: "h.local".into(), ip: None, port: kind.default_port(), kind, source: Source::Bonjour }
    }

    #[test]
    fn tailscale_status_lists_online_computers() {
        let json = r#"{"Self": {"HostName": "this-pc", "Online": true, "TailscaleIPs": ["100.1.1.1"]},
            "Peer": {
              "a": {"HostName": "Paul's iMac", "DNSName": "pauls-imac.tail1.ts.net.", "OS": "macOS", "Online": true, "TailscaleIPs": ["fd7a:115c:a1e0::1", "100.108.34.100"]},
              "b": {"HostName": "pauls-hp", "DNSName": "pauls-hp.tail1.ts.net.", "OS": "linux", "Online": false, "TailscaleIPs": ["100.98.146.65"]},
              "c": {"HostName": "pauls-iphone", "DNSName": "pauls-iphone.tail1.ts.net.", "OS": "iOS", "Online": true, "TailscaleIPs": ["100.114.112.19"]},
              "d": {"HostName": "", "DNSName": "", "OS": "windows", "Online": true, "TailscaleIPs": ["100.98.42.62"]}
            }}"#;
        let mut peers = parse_tailscale_status(json);
        peers.sort_by(|a, b| a.host.cmp(&b.host));
        assert_eq!(peers, vec![
            Peer { name: "100.98.42.62".into(), host: "100.98.42.62".into(), ip: "100.98.42.62".parse().unwrap() },
            Peer { name: "Paul's iMac".into(), host: "pauls-imac.tail1.ts.net".into(), ip: "100.108.34.100".parse().unwrap() },
        ]);
        assert!(parse_tailscale_status("not json").is_empty());
    }

    #[test]
    fn instance_names_are_unescaped() {
        assert_eq!(instance_name("Paul's Mac._rfb._tcp.local.", VNC_SERVICE), "Paul's Mac");
        assert_eq!(instance_name("Office\\.Mini._rfb._tcp.local.", VNC_SERVICE), "Office.Mini");
        assert_eq!(instance_name("A\\032B._rdp._tcp.local.", RDP_SERVICE), "A B");
        assert_eq!(instance_name("odd", VNC_SERVICE), "odd");
    }

    #[test]
    fn upsert_replaces_and_sorts() {
        let mut v = vec![];
        upsert(&mut v, found("b._rfb", "beta", Kind::Vnc));
        upsert(&mut v, found("a._rdp", "Alpha", Kind::Rdp));
        upsert(&mut v, found("a._rfb", "alpha", Kind::Vnc));
        assert_eq!(v.iter().map(|f| f.fullname.as_str()).collect::<Vec<_>>(), ["a._rfb", "a._rdp", "b._rfb"]);
        let mut again = found("b._rfb", "beta", Kind::Vnc);
        again.port = 5901;
        upsert(&mut v, again);
        assert_eq!(v.len(), 3);
        assert_eq!(v[2].port, 5901);
    }
}
