//! The macOS menu bar: every toolbar and in-app menu command, so the in-window toolbar
//! can be turned off (Settings > General) and the menu bar used instead. In full screen,
//! macOS shows the menu bar when the pointer reaches the top of the screen.
//!
//! No key equivalents are bound: every keystroke, Cmd+Q and Cmd+H included, goes to the Mac.
//! The menus are rebuilt only when what they show changes (see `refresh`).

use crate::app::{App, KEY_COMBOS};
use crate::model::Scaling;
use crate::state::{Mon, View};
use gpui::{actions, Action, Menu, MenuItem, SystemMenuType, WindowHandle};
use std::cell::RefCell;

actions!(
    seemymac,
    [
        About,
        CheckForUpdates,
        OpenSettings,
        HideApp,
        HideOthers,
        ShowAll,
        Quit,
        ShowConnections,
        NewConnection,
        ManageConnections,
        GetFileFromMac,
        CloseSession,
        NextSession,
        ShowSwitcher,
        UseVirtualDisplay,
        TogglePin,
        ToggleToolbar,
        ToggleFullScreen,
        Minimize,
        Zoom,
    ]
);

/// Open a saved connection (or switch to it when it is already open).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = seemymac, no_json)]
pub struct OpenConnection {
    pub id: String,
}

/// Send a `KEY_COMBOS` entry to the Mac.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = seemymac, no_json)]
pub struct SendKeys {
    pub index: usize,
}

/// Use a `Scaling::ALL` entry for the active session.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = seemymac, no_json)]
pub struct SetScaling {
    pub index: usize,
}

/// Show monitor 1, 2 or both (0, 1, 2).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = seemymac, no_json)]
pub struct SetMonitor {
    pub index: usize,
}

const MONITORS: [(Mon, &str); 3] = [(Mon::One, "Monitor 1"), (Mon::Two, "Monitor 2"), (Mon::All, "Both Monitors")];

/// Route each menu command to the main window's `App`. The handlers are global so the menus
/// work whichever SeeMyMac window is in front (spanned monitors open extra windows).
pub fn install(main: WindowHandle<App>, cx: &mut gpui::App) {
    fn on<A: Action + Clone>(main: WindowHandle<App>, cx: &mut gpui::App, f: impl Fn(&A, &mut App, &mut gpui::Window, &mut gpui::Context<App>) + 'static) {
        let f = std::rc::Rc::new(f);
        cx.on_action(move |a: &A, cx| {
            // The menu dispatches while the front window is being updated; run afterwards.
            let (a, f) = (a.clone(), f.clone());
            cx.defer(move |cx| {
                let _ = main.update(cx, |app, window, cx| {
                    f(&a, app, window, cx);
                    cx.notify();
                });
            });
        });
    }
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    on(main, cx, |_: &About, app, w, cx| app.open_about(w, cx));
    on(main, cx, |_: &CheckForUpdates, app, w, cx| {
        app.open_about(w, cx);
        app.start_update_check(true, cx);
    });
    on(main, cx, |_: &OpenSettings, app, w, cx| app.open_settings(w, cx));
    on(main, cx, |_: &Quit, app, _, cx| app.quit(cx));
    on(main, cx, |_: &ShowConnections, app, w, cx| {
        if app.st.view != View::Menu {
            app.toggle_menu(w, cx);
        }
    });
    on(main, cx, |_: &NewConnection, app, w, cx| app.new_connection_dialog(w, cx));
    on(main, cx, |_: &ManageConnections, app, w, cx| app.open_manage(w, cx));
    on(main, cx, |_: &GetFileFromMac, app, w, cx| app.open_get_file(w, cx));
    on(main, cx, |a: &OpenConnection, app, w, cx| app.open_connection(&a.id, w, cx));
    on(main, cx, |_: &CloseSession, app, w, cx| {
        if let Some(id) = app.st.active.clone() {
            app.close_session(&id, w, cx);
        }
    });
    on(main, cx, |_: &NextSession, app, w, _| {
        app.st.switch_next();
        w.focus(&app.focus);
    });
    on(main, cx, |_: &ShowSwitcher, app, w, _| {
        if app.st.view != View::Switcher {
            app.st.toggle_view(View::Switcher);
        }
        w.focus(&app.focus);
    });
    on(main, cx, |_: &UseVirtualDisplay, app, _, cx| app.use_virtual_display(cx));
    on(main, cx, |a: &SendKeys, app, _, _| app.send_key_combo(a.index));
    on(main, cx, |a: &SetScaling, app, _, _| {
        if let Some(s) = Scaling::ALL.get(a.index) {
            app.set_scaling(*s);
        }
    });
    on(main, cx, |a: &SetMonitor, app, _, _| {
        if let Some((m, _)) = MONITORS.get(a.index) {
            let id = app.st.active.clone();
            app.set_monitor(&id, *m);
        }
    });
    on(main, cx, |_: &TogglePin, app, _, _| app.st.toggle_pin());
    on(main, cx, |_: &ToggleToolbar, app, _, _| {
        app.settings.show_toolbar = !app.settings.show_toolbar;
        app.save_settings();
    });
    on(main, cx, |_: &ToggleFullScreen, app, w, cx| app.toggle_full(w, cx));
    on(main, cx, |_: &Minimize, _, w, _| w.minimize_window());
    on(main, cx, |_: &Zoom, _, w, _| w.zoom_window());
}

thread_local! {
    /// What the menus last showed, so they are rebuilt only when that changes.
    static SHOWN: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Rebuild the menu bar when the connections, sessions or view state it shows have changed.
/// Cheap enough to call on every render.
pub fn refresh(app: &App, cx: &gpui::App) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let sig = signature(app);
    let changed = SHOWN.with(|s| {
        let mut s = s.borrow_mut();
        if *s == sig {
            false
        } else {
            *s = sig;
            true
        }
    });
    if changed {
        cx.set_menus(build(app));
    }
}

fn signature(app: &App) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for c in &app.store.connections {
        let _ = write!(s, "c{}\u{1}{}\u{1}", c.id, c.display_name());
    }
    for id in &app.st.open {
        let _ = write!(s, "o{id}\u{1}");
    }
    let session = app.active_session();
    let _ = write!(
        s,
        "a{:?}|{}|{}|{}|{:?}|{}|{:?}|{}",
        app.st.active,
        app.st.pinned,
        app.st.full,
        app.settings.show_toolbar,
        session.map(|s| s.scaling),
        session.map_or(1, |s| s.monitors),
        app.st.active.as_deref().map(|a| app.st.monitor(a)),
        session.map_or(false, |s| s.virtual_pending.is_some()),
    );
    s
}

/// A menu item label marked as chosen (or padded to line up with the marked ones).
fn mark(on: bool, label: &str) -> String {
    format!("{}{label}", if on { "✓ " } else { "\u{2003} " })
}

fn build(app: &App) -> Vec<Menu> {
    let session = app.active_session();
    let has_session = session.is_some();

    let mut conn_items = vec![
        MenuItem::action("Connections…", ShowConnections),
        MenuItem::action("New Connection…", NewConnection),
        MenuItem::action("Manage All Connections…", ManageConnections),
    ];
    if !app.store.connections.is_empty() {
        conn_items.push(MenuItem::separator());
        conn_items.push(MenuItem::submenu(Menu {
            name: "Open".into(),
            items: app.store.connections.iter().map(|c| MenuItem::action(mark(app.st.is_open(&c.id), c.display_name()), OpenConnection { id: c.id.clone() })).collect(),
        }));
    }
    conn_items.push(MenuItem::separator());
    conn_items.push(MenuItem::action("Get File from Mac…", GetFileFromMac));
    if has_session {
        conn_items.push(MenuItem::separator());
        conn_items.push(MenuItem::action("Close Session", CloseSession));
    }

    let mut session_items = Vec::new();
    if !app.st.open.is_empty() {
        for id in &app.st.open {
            let name = app.store.get(id).map_or(id.as_str(), |c| c.display_name());
            session_items.push(MenuItem::action(mark(app.st.active.as_deref() == Some(id.as_str()), name), OpenConnection { id: id.clone() }));
        }
        session_items.push(MenuItem::separator());
        session_items.push(MenuItem::action("Next Session", NextSession));
        session_items.push(MenuItem::action("Session Switcher", ShowSwitcher));
        session_items.push(MenuItem::separator());
    }
    if let Some(s) = session {
        if s.monitors > 1 {
            let cur = app.st.active.as_deref().map(|a| app.st.monitor(a));
            for (i, (m, label)) in MONITORS.iter().enumerate() {
                session_items.push(MenuItem::action(mark(cur == Some(*m), label), SetMonitor { index: i }));
            }
            session_items.push(MenuItem::separator());
        }
        session_items.push(MenuItem::submenu(Menu {
            name: "Scaling".into(),
            items: Scaling::ALL.iter().enumerate().map(|(i, v)| MenuItem::action(mark(s.scaling == *v, v.label()), SetScaling { index: i })).collect(),
        }));
        if s.virtual_pending.is_some() {
            session_items.push(MenuItem::action("Use Virtual Display", UseVirtualDisplay));
        }
        session_items.push(MenuItem::separator());
    }
    session_items.push(MenuItem::action(mark(app.settings.show_toolbar, "Show Toolbar in Window"), ToggleToolbar));
    if app.settings.show_toolbar {
        session_items.push(MenuItem::action(mark(app.st.pinned, "Pin Toolbar"), TogglePin));
    }
    session_items.push(MenuItem::action(if app.st.full { "Exit Full Screen" } else { "Full Screen" }, ToggleFullScreen));

    let keys = KEY_COMBOS.iter().enumerate().map(|(i, c)| MenuItem::action(c.label, SendKeys { index: i })).collect();

    vec![
        // macOS names the first menu after the app.
        Menu {
            name: "SeeMyMac".into(),
            items: vec![
                MenuItem::action("About SeeMyMac", About),
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide SeeMyMac", HideApp),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit SeeMyMac", Quit),
            ],
        },
        Menu { name: "Connection".into(), items: conn_items },
        // Not "View": AppKit adds its own full-screen item to a menu with that name.
        Menu { name: "Session".into(), items: session_items },
        Menu { name: "Keys".into(), items: keys },
        Menu { name: "Window".into(), items: vec![MenuItem::action("Minimize", Minimize), MenuItem::action("Zoom", Zoom)] },
    ]
}
