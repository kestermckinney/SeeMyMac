//! Connection records and their persistence (design handoff §7 and §8).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Proto {
    /// Apple High Performance Screen Sharing (video stream), implemented natively in `hp`.
    Apple,
    /// Standard RFB/VNC, including Apple's standard-mode Screen Sharing login.
    Vnc,
    Rdp,
}

impl Proto {
    pub const ALL: [Proto; 3] = [Proto::Apple, Proto::Vnc, Proto::Rdp];

    pub fn label(self) -> &'static str {
        match self {
            Proto::Apple => "Apple HP",
            Proto::Vnc => "VNC",
            Proto::Rdp => "RDP",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Proto::Apple | Proto::Vnc => 5900,
            Proto::Rdp => 3389,
        }
    }

    /// Every protocol has a client (RDP is built on IronRDP, see `crate::rdp`).
    pub fn is_implemented(self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialMode {
    Ask,
    Vault,
    SystemSignIn,
}

impl CredentialMode {
    pub const ALL: [CredentialMode; 3] = [Self::Ask, Self::Vault, Self::SystemSignIn];
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask when connecting",
            Self::Vault => "Save in credential vault",
            Self::SystemSignIn => "Use my Windows sign-in",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    MatchWindow,
    MatchLocal,
    Fhd,
    Hd,
}
impl Resolution {
    pub const ALL: [Resolution; 4] = [Self::MatchWindow, Self::MatchLocal, Self::Fhd, Self::Hd];
    pub fn label(self) -> &'static str {
        match self {
            Self::MatchWindow => "Match client window",
            Self::MatchLocal => "Match local display",
            Self::Fhd => "1920 × 1080",
            Self::Hd => "1280 × 720",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scaling {
    Fit,
    Actual,
    Fill,
    P150,
    P125,
    P75,
    P50,
}
impl Scaling {
    pub const ALL: [Scaling; 7] = [Self::Fit, Self::Fill, Self::P150, Self::P125, Self::Actual, Self::P75, Self::P50];
    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit to window",
            Self::Fill => "Fill",
            Self::Actual => "100%",
            Self::P150 => "150%",
            Self::P125 => "125%",
            Self::P75 => "75%",
            Self::P50 => "50%",
        }
    }
    pub fn short(self) -> &'static str {
        match self {
            Self::Fit => "Fit",
            other => other.label(),
        }
    }
    /// Fixed zoom factor, or None when it depends on the window.
    pub fn factor(self) -> Option<f32> {
        match self {
            Self::Fit | Self::Fill => None,
            Self::Actual => Some(1.0),
            Self::P150 => Some(1.5),
            Self::P125 => Some(1.25),
            Self::P75 => Some(0.75),
            Self::P50 => Some(0.5),
        }
    }
    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorDepth {
    Bits32,
    Bits16,
}
impl ColorDepth {
    pub const ALL: [ColorDepth; 2] = [Self::Bits32, Self::Bits16];
    pub fn label(self) -> &'static str {
        match self {
            Self::Bits32 => "32-bit",
            Self::Bits16 => "16-bit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartIn {
    WindowUnpinned,
    WindowPinned,
    FullScreen,
}
impl StartIn {
    pub const ALL: [StartIn; 3] = [Self::WindowUnpinned, Self::WindowPinned, Self::FullScreen];
    pub fn label(self) -> &'static str {
        match self {
            Self::WindowUnpinned => "Window, toolbar unpinned",
            Self::WindowPinned => "Window, toolbar pinned",
            Self::FullScreen => "Full screen",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MultiMonitor {
    /// "PrimaryOnly" (a removed choice) in older files also means this.
    #[serde(alias = "PrimaryOnly")]
    OneAtATime,
    SideBySide,
}
impl MultiMonitor {
    pub const ALL: [MultiMonitor; 2] = [Self::OneAtATime, Self::SideBySide];
    pub fn label(self) -> &'static str {
        match self {
            Self::OneAtATime => "One at a time",
            Self::SideBySide => "All side by side",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalResources {
    pub clipboard: bool,
    pub drives: bool,
    pub printers: bool,
    pub audio: bool,
    pub key_combinations: bool,
    pub smart_cards: bool,
}
impl Default for LocalResources {
    fn default() -> Self {
        Self { clipboard: true, drives: false, printers: false, audio: true, key_combinations: true, smart_cards: false }
    }
}

fn yes() -> bool {
    true
}

fn one() -> u8 {
    1
}

pub const GROUPS: [&str; 4] = ["Servers", "Plant floor", "Lab", "No group"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub host: String,
    pub proto: Proto,
    /// Kept as text like the design's numeric text field; parsed on use.
    pub port: String,
    pub user: String,
    pub group: String,
    pub fav: bool,
    pub monitors: u32,
    pub credentials: CredentialMode,
    pub reconnect: bool,
    pub resolution: Resolution,
    pub scaling: Scaling,
    pub depth: ColorDepth,
    pub start_in: StartIn,
    pub multi_monitor: MultiMonitor,
    pub span_monitors: bool,
    /// Apple HP only: ask the Mac for a virtual display (blanks the Mac's own screens).
    #[serde(default = "yes")]
    pub virtual_display: bool,
    /// Apple HP only: how many virtual displays (1 or 2) to use; with 2 each is
    /// streamed separately and shown as its own monitor.
    #[serde(default = "one")]
    pub virtual_displays: u8,
    pub local: LocalResources,
    /// Unix seconds of the last connect; 0 = never. Drives the Recent list.
    pub last_used: u64,
    /// The Mac's physical displays as (x, y, width, height) in backing pixels,
    /// learnt from Apple HP sessions. macOS does not report its monitors over
    /// plain VNC, so a VNC desktop of the same overall size is split with these.
    #[serde(default)]
    pub display_layout: Vec<[u32; 4]>,
    /// RDP only: an RD Gateway (`host` or `host:port`) to reach the host through; empty = direct.
    #[serde(default)]
    pub gateway: String,
    /// RDP only: run just this program (a path, or `||alias` for a published RemoteApp) instead of the desktop.
    #[serde(default)]
    pub remote_app: String,
}

impl Connection {
    pub fn blank(id: String) -> Self {
        Self {
            id,
            name: String::new(),
            host: String::new(),
            proto: Proto::Apple,
            port: Proto::Apple.default_port().to_string(),
            user: String::new(),
            group: GROUPS[0].into(),
            fav: true,
            monitors: 1,
            credentials: CredentialMode::Ask,
            reconnect: true,
            resolution: Resolution::MatchWindow,
            scaling: Scaling::Fit,
            depth: ColorDepth::Bits32,
            start_in: StartIn::WindowUnpinned,
            multi_monitor: MultiMonitor::OneAtATime,
            span_monitors: false,
            virtual_display: true,
            virtual_displays: 1,
            local: LocalResources::default(),
            last_used: 0,
            display_layout: vec![],
            gateway: String::new(),
            remote_app: String::new(),
        }
    }

    pub fn port_number(&self) -> u16 {
        self.port.trim().parse().unwrap_or_else(|_| self.proto.default_port())
    }

    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() { &self.host } else { &self.name }
    }

    /// "host · protocol" subtitle used by menu rows.
    pub fn subtitle(&self) -> String {
        format!("{} · {}", self.host, self.proto.label())
    }

    /// Validation for the dialog's Save buttons.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.host.trim().is_empty() {
            return Err("Enter a host name or IP address.");
        }
        if self.port.trim().parse::<u16>().map(|p| p == 0).unwrap_or(true) {
            return Err("Port must be a number from 1 to 65535.");
        }
        Ok(())
    }
}

/// The saved connection list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    pub connections: Vec<Connection>,
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(skip)]
    next: u64,
}

impl Store {
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("SeeMyMac").join("connections.json"))
    }

    /// Load from `path`, or start empty when the file is missing or unreadable.
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut store = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Store>(&s).ok())
            .unwrap_or_default();
        store.path = path;
        store.next = store
            .connections
            .iter()
            .filter_map(|c| c.id.strip_prefix('c').and_then(|n| n.parse::<u64>().ok()))
            .max()
            .map_or(1, |n| n + 1);
        store
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// A fresh id, never one already in the list (even if records were added by hand).
    pub fn new_id(&mut self) -> String {
        loop {
            let id = format!("c{}", self.next);
            self.next += 1;
            if self.get(&id).is_none() {
                return id;
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Connection> {
        self.connections.iter_mut().find(|c| c.id == id)
    }

    /// Insert a new record or replace the one with the same id.
    pub fn upsert(&mut self, c: Connection) {
        match self.connections.iter_mut().find(|x| x.id == c.id) {
            Some(slot) => *slot = c,
            None => self.connections.push(c),
        }
    }

    pub fn delete(&mut self, id: &str) {
        self.connections.retain(|c| c.id != id);
    }

    pub fn favorites(&self) -> Vec<&Connection> {
        self.connections.iter().filter(|c| c.fav).collect()
    }

    /// Non-favorites that have been used, most recent first.
    pub fn recent(&self) -> Vec<&Connection> {
        let mut r: Vec<_> = self.connections.iter().filter(|c| !c.fav && c.last_used > 0).collect();
        r.sort_by(|a, b| b.last_used.cmp(&a.last_used));
        r
    }

    pub fn touch(&mut self, id: &str, now: u64) {
        if let Some(c) = self.get_mut(id) {
            c.last_used = now;
        }
    }

    /// Case-insensitive match on name or host, in list order.
    pub fn search(&self, query: &str) -> Vec<&Connection> {
        let q = query.trim().to_lowercase();
        self.connections
            .iter()
            .filter(|c| q.is_empty() || c.name.to_lowercase().contains(&q) || c.host.to_lowercase().contains(&q))
            .collect()
    }
}

/// Split "host", "host:port" (not IPv6 literals) typed into Quick connect.
pub fn parse_quick_host(text: &str) -> Option<(String, Option<u16>)> {
    let t = text.trim();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return None;
    }
    if let Some((h, p)) = t.rsplit_once(':') {
        if !h.contains(':') && !h.is_empty() {
            return p.parse::<u16>().ok().filter(|p| *p > 0).map(|p| (h.to_string(), Some(p)));
        }
    }
    Some((t.to_string(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str, name: &str, host: &str, fav: bool, used: u64) -> Connection {
        let mut c = Connection::blank(id.into());
        c.name = name.into();
        c.host = host.into();
        c.fav = fav;
        c.last_used = used;
        c
    }

    #[test]
    fn defaults_follow_design_dialog() {
        let c = Connection::blank("c1".into());
        assert_eq!(c.credentials, CredentialMode::Ask);
        assert_eq!(c.group, "Servers");
        assert!(c.fav && c.reconnect);
        assert_eq!(c.resolution, Resolution::MatchWindow);
        assert_eq!(c.scaling, Scaling::Fit);
        assert_eq!(c.depth, ColorDepth::Bits32);
        assert_eq!(c.start_in, StartIn::WindowUnpinned);
        assert_eq!(c.multi_monitor, MultiMonitor::OneAtATime);
        assert!(!c.span_monitors);
        assert!(c.local.clipboard && c.local.audio && c.local.key_combinations);
        assert!(!c.local.drives && !c.local.printers && !c.local.smart_cards);
    }

    #[test]
    fn ports_follow_protocol() {
        assert_eq!(Proto::Rdp.default_port(), 3389);
        assert_eq!(Proto::Vnc.default_port(), 5900);
        assert_eq!(Proto::Apple.default_port(), 5900);
        let mut c = Connection::blank("c1".into());
        c.port = "abc".into();
        assert_eq!(c.port_number(), 5900);
    }

    #[test]
    fn validation() {
        let mut c = Connection::blank("c1".into());
        assert!(c.validate().is_err());
        c.host = "mac.local".into();
        assert!(c.validate().is_ok());
        c.port = "70000".into();
        assert!(c.validate().is_err());
        c.port = "0".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn favorites_recent_and_search() {
        let mut s = Store::default();
        s.upsert(conn("c1", "Studio", "studio.local", true, 5));
        s.upsert(conn("c2", "Laptop", "10.0.0.7", false, 9));
        s.upsert(conn("c3", "Old", "old.local", false, 2));
        s.upsert(conn("c4", "Never", "never.local", false, 0));
        assert_eq!(s.favorites().len(), 1);
        let r: Vec<_> = s.recent().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(r, ["c2", "c3"]);
        assert_eq!(s.search("LOCAL").len(), 3);
        assert_eq!(s.search("10.0").len(), 1);
        assert_eq!(s.search("").len(), 4);
        s.delete("c2");
        assert!(s.get("c2").is_none());
    }

    #[test]
    fn persistence_round_trip_and_ids() {
        let dir = std::env::temp_dir().join(format!("smm-test-{}", std::process::id()));
        let path = dir.join("connections.json");
        let mut s = Store::load(Some(path.clone()));
        assert!(s.connections.is_empty());
        let id = s.new_id();
        s.upsert(conn(&id, "A", "a.local", true, 0));
        s.save().unwrap();
        let mut s2 = Store::load(Some(path));
        assert_eq!(s2.connections.len(), 1);
        assert_ne!(s2.new_id(), id, "ids must not collide after reload");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn new_ids_never_collide_with_existing_records() {
        let mut s = Store::default();
        s.upsert(conn("c1", "A", "a", true, 0));
        s.upsert(conn("c2", "B", "b", true, 0));
        let id = s.new_id();
        assert!(s.get(&id).is_none(), "{id} already exists");
        s.upsert(conn(&id, "C", "c", true, 0));
        assert_eq!(s.connections.len(), 3);
    }

    #[test]
    fn corrupt_file_starts_empty() {
        let dir = std::env::temp_dir().join(format!("smm-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connections.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(Store::load(Some(path)).connections.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quick_host_parsing() {
        assert_eq!(parse_quick_host("mac.local"), Some(("mac.local".into(), None)));
        assert_eq!(parse_quick_host("mac.local:5901"), Some(("mac.local".into(), Some(5901))));
        assert_eq!(parse_quick_host("a b"), None);
        assert_eq!(parse_quick_host(""), None);
        assert_eq!(parse_quick_host("host:99999"), None);
    }
}
