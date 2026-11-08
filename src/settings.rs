//! Application settings (the menu's "Settings"), saved to settings.json beside the
//! connection list. The design handoff does not define this screen; it reuses the
//! dialog styling and exposes only options that have an effect.

use crate::model::{Connection, Proto, Scaling, StartIn};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which local key is sent to the Mac as its Command key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandKey {
    /// The Windows/Super key is Command (Apple's layout position).
    SuperIsCommand,
    /// Ctrl is Command, so Ctrl+C / Ctrl+V behave like Cmd+C / Cmd+V on the Mac.
    CtrlIsCommand,
}

impl CommandKey {
    pub const ALL: [CommandKey; 2] = [Self::SuperIsCommand, Self::CtrlIsCommand];
    pub fn label(self) -> &'static str {
        match self {
            Self::SuperIsCommand => "Windows/Super key is Command",
            Self::CtrlIsCommand => "Ctrl is Command (swap Ctrl and Super)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub default_proto: Proto,
    pub default_start_in: StartIn,
    pub default_scaling: Scaling,
    pub default_virtual_display: bool,
    /// Automatic reconnect attempts after a dropped session (when the connection allows it).
    pub max_reconnects: u32,
    pub command_key: CommandKey,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_proto: Proto::Apple,
            default_start_in: StartIn::WindowUnpinned,
            default_scaling: Scaling::Fit,
            default_virtual_display: true,
            max_reconnects: 5,
            command_key: CommandKey::SuperIsCommand,
            path: None,
        }
    }
}

pub const RECONNECT_CHOICES: [u32; 5] = [0, 1, 3, 5, 10];

impl Settings {
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("SeeMyMac").join("settings.json"))
    }

    /// Load, falling back to defaults when the file is missing or unreadable.
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut s = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
            .unwrap_or_default();
        s.path = path;
        s
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

    /// Fill a brand-new connection with the user's defaults.
    pub fn apply_defaults(&self, c: &mut Connection) {
        c.proto = self.default_proto;
        c.port = self.default_proto.default_port().to_string();
        c.start_in = self.default_start_in;
        c.scaling = self.default_scaling;
        c.virtual_display = self.default_virtual_display;
    }

    /// Which modifier keys to send to the Mac, given the local state: (Control, Command).
    pub fn remote_modifiers(&self, local_ctrl: bool, local_super: bool) -> (bool, bool) {
        match self.command_key {
            CommandKey::SuperIsCommand => (local_ctrl, local_super),
            CommandKey::CtrlIsCommand => (local_super, local_ctrl),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let s = Settings::default();
        assert_eq!(s.default_proto, Proto::Apple);
        assert_eq!(s.max_reconnects, 5);
        assert!(RECONNECT_CHOICES.contains(&s.max_reconnects));
    }

    #[test]
    fn round_trip_and_partial_files() {
        let dir = std::env::temp_dir().join(format!("smm-set-{}", std::process::id()));
        let path = dir.join("settings.json");
        let mut s = Settings::load(Some(path.clone()));
        assert_eq!(s, Settings::default().tap_path(&path));
        s.max_reconnects = 10;
        s.command_key = CommandKey::CtrlIsCommand;
        s.save().unwrap();
        let back = Settings::load(Some(path.clone()));
        assert_eq!((back.max_reconnects, back.command_key), (10, CommandKey::CtrlIsCommand));
        // Files from versions that used the Python bridge still load.
        std::fs::write(&path, r#"{"max_reconnects": 4, "bridge_script": "/opt/bridge.py", "bridge_python": ""}"#).unwrap();
        assert_eq!(Settings::load(Some(path.clone())).max_reconnects, 4);
        // A file written by an older version (missing fields) still loads.
        std::fs::write(&path, r#"{"max_reconnects": 3}"#).unwrap();
        let old = Settings::load(Some(path.clone()));
        assert_eq!(old.max_reconnects, 3);
        assert_eq!(old.default_proto, Proto::Apple);
        // Corrupt file: defaults, not a crash.
        std::fs::write(&path, "{nope").unwrap();
        assert_eq!(Settings::load(Some(path)).max_reconnects, 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    impl Settings {
        fn tap_path(mut self, p: &std::path::Path) -> Self {
            self.path = Some(p.to_path_buf());
            self
        }
    }

    #[test]
    fn new_connections_get_the_defaults() {
        let s = Settings { default_proto: Proto::Vnc, default_start_in: StartIn::FullScreen, default_scaling: Scaling::Fill, default_virtual_display: false, ..Settings::default() };
        let mut c = Connection::blank("c1".into());
        s.apply_defaults(&mut c);
        assert_eq!((c.proto, c.port.as_str(), c.start_in, c.scaling, c.virtual_display), (Proto::Vnc, "5900", StartIn::FullScreen, Scaling::Fill, false));
        let rdp = Settings { default_proto: Proto::Rdp, ..Settings::default() };
        rdp.apply_defaults(&mut c);
        assert_eq!(c.port, "3389");
    }

    #[test]
    fn command_key_mapping_swaps_ctrl_and_super() {
        let normal = Settings::default();
        assert_eq!(normal.remote_modifiers(true, false), (true, false));
        assert_eq!(normal.remote_modifiers(false, true), (false, true));
        let swap = Settings { command_key: CommandKey::CtrlIsCommand, ..Settings::default() };
        assert_eq!(swap.remote_modifiers(true, false), (false, true), "local Ctrl becomes Command");
        assert_eq!(swap.remote_modifiers(false, true), (true, false), "local Super becomes Ctrl");
    }
}
