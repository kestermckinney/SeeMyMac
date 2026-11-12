//! Window/session state machine from design handoff §5, §6 and §8 ("Runtime state").
//! Pure logic: no gpui types, so it is fully unit-tested.

use crate::model::StartIn;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Idle,
    Menu,
    Switcher,
    Dialog,
    /// "Manage all connections": every stored connection, with edit and delete.
    Manage,
    /// Application settings.
    Settings,
    /// The About screen: icon, version, build time and links.
    About,
    /// Asks for a password when a connection uses "Ask when connecting".
    Password,
    /// Asks for the path of a file or folder on the Mac to copy here.
    GetFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mon {
    One,
    Two,
    All,
}

#[derive(Debug, Clone)]
pub struct AppState {
    /// Ordered list of open session ids (the design's `open`).
    pub open: Vec<String>,
    pub active: Option<String>,
    pub mon: HashMap<String, Mon>,
    pub pinned: bool,
    pub full: bool,
    pub view: View,
    pinned_before_full: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self { open: vec![], active: None, mon: HashMap::new(), pinned: true, full: false, view: View::Idle, pinned_before_full: true }
    }

    pub fn is_open(&self, id: &str) -> bool {
        self.open.iter().any(|s| s == id)
    }

    /// Open a session, or switch to it when already open (§6: never a duplicate).
    /// Returns true when a new session was added.
    pub fn open_session(&mut self, id: &str, start: StartIn) -> bool {
        let added = if self.is_open(id) {
            false
        } else {
            self.open.push(id.to_string());
            self.mon.insert(id.to_string(), Mon::One);
            true
        };
        self.active = Some(id.to_string());
        self.view = View::Idle;
        if added {
            match start {
                StartIn::WindowUnpinned => self.pinned = false,
                StartIn::WindowPinned => self.pinned = true,
                StartIn::FullScreen => self.enter_full(),
            }
        }
        added
    }

    /// Close a session. The neighbour to its left (or right) becomes active; with
    /// none left the app shows the "No sessions" state.
    pub fn close_session(&mut self, id: &str) {
        let Some(idx) = self.open.iter().position(|s| s == id) else { return };
        self.open.remove(idx);
        self.mon.remove(id);
        if self.active.as_deref() == Some(id) {
            self.active = if self.open.is_empty() { None } else { Some(self.open[idx.saturating_sub(1).min(self.open.len() - 1)].clone()) };
        }
        if self.open.is_empty() {
            self.view = View::Idle;
        }
    }

    pub fn switch_to(&mut self, id: &str) {
        if self.is_open(id) {
            self.active = Some(id.to_string());
            self.view = View::Idle;
        }
    }

    /// Ctrl+Alt+1…9 (1-based).
    pub fn switch_to_index(&mut self, n: usize) {
        if n >= 1 {
            if let Some(id) = self.open.get(n - 1).cloned() {
                self.switch_to(&id);
            }
        }
    }

    pub fn switch_next(&mut self) {
        if self.open.len() < 2 {
            return;
        }
        let cur = self.active.as_ref().and_then(|a| self.open.iter().position(|s| s == a)).unwrap_or(0);
        let next = self.open[(cur + 1) % self.open.len()].clone();
        self.switch_to(&next);
    }

    pub fn set_view(&mut self, v: View) {
        self.view = v;
    }

    /// Toggle an overlay: opening replaces any other overlay, same call closes it.
    pub fn toggle_view(&mut self, v: View) {
        self.view = if self.view == v { View::Idle } else { v };
    }

    pub fn close_overlay(&mut self) {
        self.view = View::Idle;
    }

    pub fn toggle_pin(&mut self) {
        self.pinned = !self.pinned;
    }

    fn enter_full(&mut self) {
        if !self.full {
            self.pinned_before_full = self.pinned;
            self.full = true;
            self.pinned = false; // §5: toolbar unpins automatically on entry
        }
    }

    fn exit_full(&mut self) {
        if self.full {
            self.full = false;
            self.pinned = self.pinned_before_full;
        }
    }

    pub fn toggle_full(&mut self) {
        if self.full { self.exit_full() } else { self.enter_full() }
    }

    pub fn monitor(&self, id: &str) -> Mon {
        self.mon.get(id).copied().unwrap_or(Mon::One)
    }

    /// Monitor selection only applies when the session has more than one monitor.
    pub fn set_monitor(&mut self, id: &str, m: Mon, monitors: u32) {
        if monitors > 1 && self.is_open(id) {
            let m = if m == Mon::Two && monitors < 2 { Mon::One } else { m };
            self.mon.insert(id.to_string(), m);
        }
    }

    /// Clicking a monitor in the "Both" view zooms back into it (§5).
    pub fn zoom_into(&mut self, id: &str, which: Mon) {
        if self.monitor(id) == Mon::All && which != Mon::All {
            self.mon.insert(id.to_string(), which);
        }
    }

    pub fn no_sessions(&self) -> bool {
        self.open.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(ids: &[&str]) -> AppState {
        let mut s = AppState::new();
        for id in ids {
            s.open_session(id, StartIn::WindowPinned);
        }
        s
    }

    #[test]
    fn opening_same_connection_switches_instead_of_duplicating() {
        let mut s = with(&["a", "b"]);
        assert_eq!(s.active.as_deref(), Some("b"));
        let added = s.open_session("a", StartIn::WindowPinned);
        assert!(!added);
        assert_eq!(s.open, ["a", "b"]);
        assert_eq!(s.active.as_deref(), Some("a"));
    }

    #[test]
    fn start_in_applies_only_to_new_sessions() {
        let mut s = AppState::new();
        s.open_session("a", StartIn::WindowUnpinned);
        assert!(!s.pinned);
        s.toggle_pin();
        s.open_session("a", StartIn::WindowUnpinned); // re-select
        assert!(s.pinned, "switching must not reapply start-in");
        s.open_session("b", StartIn::FullScreen);
        assert!(s.full && !s.pinned);
    }

    #[test]
    fn closing_active_picks_neighbour_and_last_close_shows_empty_state() {
        let mut s = with(&["a", "b", "c"]);
        s.switch_to("b");
        s.close_session("b");
        assert_eq!(s.active.as_deref(), Some("a"));
        s.close_session("c"); // not active: active unchanged
        assert_eq!(s.active.as_deref(), Some("a"));
        s.close_session("a");
        assert!(s.no_sessions() && s.active.is_none());
        s.close_session("zzz"); // unknown id is harmless
    }

    #[test]
    fn closing_first_activates_next() {
        let mut s = with(&["a", "b"]);
        s.switch_to("a");
        s.close_session("a");
        assert_eq!(s.active.as_deref(), Some("b"));
    }

    #[test]
    fn switching_by_index_and_cycling() {
        let mut s = with(&["a", "b", "c"]);
        s.switch_to_index(1);
        assert_eq!(s.active.as_deref(), Some("a"));
        s.switch_to_index(9); // out of range: ignored
        s.switch_to_index(0);
        assert_eq!(s.active.as_deref(), Some("a"));
        s.switch_next();
        s.switch_next();
        s.switch_next();
        assert_eq!(s.active.as_deref(), Some("a"), "wraps around");
    }

    #[test]
    fn full_screen_unpins_and_restores() {
        let mut s = with(&["a"]);
        assert!(s.pinned);
        s.toggle_full();
        assert!(s.full && !s.pinned);
        s.toggle_full();
        assert!(!s.full && s.pinned);
    }

    #[test]
    fn overlays_toggle_and_replace() {
        let mut s = with(&["a"]);
        s.toggle_view(View::Menu);
        assert_eq!(s.view, View::Menu);
        s.toggle_view(View::Switcher);
        assert_eq!(s.view, View::Switcher);
        s.toggle_view(View::Switcher);
        assert_eq!(s.view, View::Idle);
        s.set_view(View::Dialog);
        s.switch_to("a");
        assert_eq!(s.view, View::Idle, "selecting a session dismisses overlays");
    }

    #[test]
    fn monitor_selection_needs_multiple_monitors() {
        let mut s = with(&["a"]);
        s.set_monitor("a", Mon::All, 1);
        assert_eq!(s.monitor("a"), Mon::One);
        s.set_monitor("a", Mon::All, 2);
        assert_eq!(s.monitor("a"), Mon::All);
        s.zoom_into("a", Mon::Two);
        assert_eq!(s.monitor("a"), Mon::Two);
        s.zoom_into("a", Mon::One); // only zooms from "Both"
        assert_eq!(s.monitor("a"), Mon::Two);
    }

    #[test]
    fn closing_clears_monitor_state() {
        let mut s = with(&["a"]);
        s.set_monitor("a", Mon::All, 2);
        s.close_session("a");
        assert_eq!(s.monitor("a"), Mon::One);
        assert!(s.mon.is_empty());
    }
}
