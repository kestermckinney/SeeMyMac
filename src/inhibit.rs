//! Lets system shortcuts reach the Mac. GNOME and other Wayland desktops take
//! keys such as Super (Activities) and Super+Space (input source) before any
//! window sees them. The keyboard-shortcuts-inhibit protocol asks the desktop
//! to pass them to our window while it has focus; GNOME asks the user once
//! whether to allow this, and Super+Escape always gives the shortcuts back.
//!
//! gpui does not speak this protocol, so this binds it on gpui's own Wayland
//! connection and surface. X11 and other platforms get no inhibitor.

use gpui::Window;

pub struct Shortcuts {
    #[cfg(target_os = "linux")]
    imp: Option<wl::Inhibit>,
}

impl Shortcuts {
    /// Ready to inhibit shortcuts on `window`, when its platform supports it.
    pub fn for_window(window: &Window) -> Self {
        #[cfg(target_os = "linux")]
        {
            // Asking an X11 window for its Wayland handles panics in gpui.
            let imp = if gpui::guess_compositor() == "Wayland" { wl::Inhibit::new(window) } else { None };
            if imp.is_none() {
                log::info!("system shortcuts cannot be passed to the Mac on this desktop");
            }
            Self { imp }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = window;
            Self {}
        }
    }

    /// Pass system shortcuts to the window (`on`) or give them back to the desktop.
    pub fn set(&mut self, on: bool) {
        #[cfg(target_os = "linux")]
        if let Some(i) = &mut self.imp {
            i.set(on);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = on;
    }

    /// The user clicked the session: if Super+Escape gave the shortcuts back,
    /// ask for them again.
    pub fn rearm(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(i) = &mut self.imp {
            i.rearm();
        }
    }
}

#[cfg(target_os = "linux")]
mod wl {
    use gpui::Window;
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    use wayland_backend::client::{Backend, ObjectId};
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat, wl_surface::WlSurface};
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
        zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 as Manager,
        zwp_keyboard_shortcuts_inhibitor_v1::{Event as InhibitorEvent, ZwpKeyboardShortcutsInhibitorV1 as Inhibitor},
    };

    #[derive(Default)]
    pub struct State {
        /// The desktop took the shortcuts back (Super+Escape, or the user refused).
        inactive: bool,
    }

    pub struct Inhibit {
        conn: Connection,
        queue: EventQueue<State>,
        state: State,
        manager: Manager,
        seat: WlSeat,
        surface: WlSurface,
        active: Option<Inhibitor>,
    }

    impl Inhibit {
        pub fn new(window: &Window) -> Option<Self> {
            let RawDisplayHandle::Wayland(d) = window.display_handle().ok()?.as_raw() else { return None };
            let RawWindowHandle::Wayland(w) = HasWindowHandle::window_handle(window).ok()?.as_raw() else { return None };
            // SAFETY: both pointers come from gpui's live Wayland connection and
            // window, which outlive the app view that owns this.
            let backend = unsafe { Backend::from_foreign_display(d.display.as_ptr().cast()) };
            let conn = Connection::from_backend(backend);
            let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
            let qh = queue.handle();
            let manager: Manager = globals.bind(&qh, 1..=1, ()).map_err(|e| log::info!("no keyboard-shortcuts-inhibit support: {e}")).ok()?;
            let seat: WlSeat = globals.bind(&qh, 1..=1, ()).ok()?;
            let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), w.surface.as_ptr().cast()) }.ok()?;
            let surface = WlSurface::from_id(&conn, id).ok()?;
            let mut state = State::default();
            queue.roundtrip(&mut state).ok()?;
            Some(Self { conn, queue, state, manager, seat, surface, active: None })
        }

        pub fn set(&mut self, on: bool) {
            if on == self.active.is_some() {
                let _ = self.queue.dispatch_pending(&mut self.state);
                return;
            }
            if on {
                self.state.inactive = false;
                let qh = self.queue.handle();
                self.active = Some(self.manager.inhibit_shortcuts(&self.surface, &self.seat, &qh, ()));
                log::info!("asked the desktop to pass system shortcuts to the Mac");
            } else if let Some(i) = self.active.take() {
                i.destroy();
                log::info!("system shortcuts returned to the desktop");
            }
            let _ = self.conn.flush();
        }

        pub fn rearm(&mut self) {
            let _ = self.queue.dispatch_pending(&mut self.state);
            if self.active.is_some() && self.state.inactive {
                self.set(false);
                self.set(true);
            }
        }
    }

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }

    impl Dispatch<WlSeat, ()> for State {
        fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }

    impl Dispatch<Manager, ()> for State {
        fn event(_: &mut Self, _: &Manager, _: <Manager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }

    impl Dispatch<Inhibitor, ()> for State {
        fn event(state: &mut Self, _: &Inhibitor, e: InhibitorEvent, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            match e {
                InhibitorEvent::Active => {
                    state.inactive = false;
                    log::info!("system shortcuts now go to the Mac (Super+Escape gives them back)");
                }
                InhibitorEvent::Inactive => {
                    state.inactive = true;
                    log::info!("the desktop is handling system shortcuts again");
                }
                _ => {}
            }
        }
    }
}
