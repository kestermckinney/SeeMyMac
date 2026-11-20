//! Headless UI tests: the real `App` view rendered by gpui's test platform, driven by
//! simulated keystrokes against an in-process fake Mac (RFB) server.

use crate::app::{App, Status};
use crate::model::{Connection, CredentialMode, Proto, Store};
use crate::state::{Mon, View};
use gpui::{TestAppContext, VisualTestContext};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// Serves one client: no auth, `w`x`h` framebuffer filled red, optional two monitors.
/// Records what the client sends in `log`.
fn fake_mac(w: u16, h: u16, two_monitors: bool) -> (u16, std::sync::mpsc::Receiver<Vec<u8>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.write_all(b"RFB 003.889\n").unwrap();
        let mut b = [0u8; 12];
        s.read_exact(&mut b).unwrap();
        s.write_all(&[1, 1]).unwrap();
        let mut c = [0u8; 1];
        s.read_exact(&mut c).unwrap();
        s.write_all(&0u32.to_be_bytes()).unwrap();
        s.read_exact(&mut c).unwrap();
        let mut init = vec![];
        init.extend(w.to_be_bytes());
        init.extend(h.to_be_bytes());
        init.extend([0u8; 16]);
        init.extend(4u32.to_be_bytes());
        init.extend(b"Test");
        s.write_all(&init).unwrap();
        // SetPixelFormat(20) + SetEncodings(4 + 4*n) + request(10)
        let mut buf = vec![0u8; 20];
        s.read_exact(&mut buf).unwrap();
        let mut hdr = [0u8; 4];
        s.read_exact(&mut hdr).unwrap();
        let n = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        // Encodings, Apple's ViewerInfo (66) and AutoPasteboard (8), the update request (10).
        let mut e = vec![0u8; n * 4 + 66 + 8 + 10];
        s.read_exact(&mut e).unwrap();
        let mut m = vec![0, 0];
        m.extend((1 + two_monitors as u16).to_be_bytes());
        // red frame
        m.extend([0, 0, 0, 0]);
        m.extend(w.to_be_bytes());
        m.extend(h.to_be_bytes());
        m.extend(0u32.to_be_bytes());
        for _ in 0..(w as usize * h as usize) {
            m.extend([0, 0, 255, 0]);
        }
        if two_monitors {
            m.extend([0, 0, 0, 0]);
            m.extend(w.to_be_bytes());
            m.extend(h.to_be_bytes());
            m.extend((-308i32).to_be_bytes());
            m.extend([2, 0, 0, 0]);
            for (id, x) in [(1u32, 0u16), (2, w / 2)] {
                m.extend(id.to_be_bytes());
                m.extend(x.to_be_bytes());
                m.extend(0u16.to_be_bytes());
                m.extend((w / 2).to_be_bytes());
                m.extend(h.to_be_bytes());
                m.extend(0u32.to_be_bytes());
            }
        }
        s.write_all(&m).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200))).ok();
        let mut all = vec![];
        let end = Instant::now() + Duration::from_secs(8);
        while Instant::now() < end {
            let mut chunk = [0u8; 256];
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    all.extend(&chunk[..n]);
                    tx.send(all.clone()).ok();
                }
                Err(_) => {}
            }
        }
    });
    (port, rx)
}

fn store_with(host_port: Option<u16>) -> Store {
    let mut s = Store::load(None);
    let mut c = Connection::blank("c1".into());
    c.proto = Proto::Vnc; // the fake server speaks plain RFB
    c.name = "Studio".into();
    c.host = "127.0.0.1".into();
    c.port = host_port.unwrap_or(5900).to_string();
    c.fav = true;
    s.upsert(c);
    let mut c2 = Connection::blank("c2".into());
    c2.proto = Proto::Vnc;
    c2.name = "Laptop".into();
    c2.host = "127.0.0.1".into();
    c2.port = "1".into();
    c2.fav = false;
    c2.last_used = 5;
    s.upsert(c2);
    s
}

/// Open a connection and, if the login prompt appears, accept it (empty password: the
/// fake server needs none).
fn open_and_login(cx: &mut VisualTestContext, view: &gpui::Entity<App>, id: &'static str) {
    cx.update(|window, cx| view.update(cx, |a, cx| a.open_connection(id, window, cx)));
    if view.read_with(cx, |a, _| a.st.view == View::Password) {
        draw(cx);
        cx.simulate_keystrokes("enter");
    }
}

fn draw(cx: &mut VisualTestContext) {
    let _ = cx.update(|window, cx| window.draw(cx));
}

fn tick_until(cx: &mut VisualTestContext, view: &gpui::Entity<App>, what: &str, ok: impl Fn(&App) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        cx.update(|window, cx| view.update(cx, |app, cx| app.tick(window, cx)));
        if view.read_with(cx, |a, _| ok(a)) {
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[gpui::test]
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn close_button_quits_instead_of_dragging_the_window(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(None), None, window, cx));
    draw(cx);
    let b = cx.debug_bounds("win-close").expect("close button painted");
    cx.simulate_click(b.center(), gpui::Modifiers::default());
    view.read_with(cx, |a, _| {
        assert!(a.quit_requested, "the close button's click must complete");
        // (The test platform panics in start_window_move, so reaching the title
        // bar's drag handler would also fail this test.)
        assert_eq!(a.window_drags, 0, "pressing a window button must not start a window move");
    });
}

#[gpui::test]
fn empty_state_renders_and_menu_opens_with_shortcut(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(None), None, window, cx));
    draw(cx);
    assert!(view.read_with(cx, |a, _| a.st.no_sessions()));

    cx.update(|window, cx| view.update(cx, |a, cx| a.toggle_menu(window, cx)));
    draw(cx);
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Menu);
    cx.simulate_keystrokes("escape");
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Idle);

    cx.simulate_keystrokes("ctrl-m");
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Manage, "Ctrl+M opens the connection manager");
    cx.simulate_keystrokes("escape");
    cx.simulate_keystrokes("ctrl-,");
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Settings, "Ctrl+, opens settings");
    cx.simulate_keystrokes("escape");

    view.update_in(cx, |a, w, cx| a.open_about(w, cx));
    draw(cx);
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::About, "the About screen opens");
    cx.simulate_keystrokes("escape");
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Idle, "Escape closes it");

    // A found update is offered; "Later" (Escape) closes the prompt and is remembered.
    view.update_in(cx, |a, w, cx| {
        a.update = crate::app::UpdateStatus::Available(crate::update::Release { version: "9.9.9".into(), notes: "Notes".into(), asset_url: "https://example.com/x".into(), asset_name: "x".into() });
        a.st.set_view(View::Update);
        cx.notify();
        let _ = w;
    });
    draw(cx);
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Update);
    cx.simulate_keystrokes("escape");
    assert_eq!(view.read_with(cx, |a, _| (a.st.view, a.update.clone())), (View::Idle, crate::app::UpdateStatus::Idle));

    cx.simulate_keystrokes("ctrl-n");
    draw(cx);
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Dialog);
}

/// With "Check for updates automatically" on, a running SeeMyMac looks again once the interval
/// has passed; a version found during a remote session waits until the user is back on the start
/// screen. Turned off, it never looks by itself. (Linux builds never check.)
#[cfg(not(target_os = "linux"))]
#[gpui::test]
fn updates_are_checked_periodically_without_interrupting_a_session(cx: &mut TestAppContext) {
    let (port, _rx) = fake_mac(32, 18, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    let release = || crate::update::CheckResult::Available(crate::update::Release { version: "9.9.9".into(), notes: String::new(), asset_url: "https://example.com/x".into(), asset_name: "x".into() });
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

    // Turned off: an overdue check does not run.
    view.update(cx, |a, _| {
        a.update_checker = release;
        a.settings.check_updates = false;
        a.settings.last_update_check = 0;
    });
    for _ in 0..3 {
        cx.update(|window, cx| view.update(cx, |a, cx| a.tick(window, cx)));
    }
    assert_eq!(view.read_with(cx, |a, _| (a.update.clone(), a.settings.last_update_check)), (crate::app::UpdateStatus::Idle, 0));

    // On, checked recently: nothing until the interval has passed.
    view.update(cx, |a, _| {
        a.settings.check_updates = true;
        a.settings.last_update_check = now;
    });
    cx.update(|window, cx| view.update(cx, |a, cx| a.tick(window, cx)));
    assert_eq!(view.read_with(cx, |a, _| a.update.clone()), crate::app::UpdateStatus::Idle);

    // In a session when the interval runs out: the update is found but not shown yet.
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "connected", |a| a.sessions["c1"].status == Status::Connected);
    view.update(cx, |a, _| a.settings.last_update_check = now - crate::app::UPDATE_INTERVAL);
    tick_until(cx, &view, "update found", |a| matches!(a.update, crate::app::UpdateStatus::Available(_)));
    for _ in 0..3 {
        cx.update(|window, cx| view.update(cx, |a, cx| a.tick(window, cx)));
    }
    view.read_with(cx, |a, _| {
        assert!(a.settings.last_update_check >= now, "the check time is recorded");
        assert_ne!(a.st.view, View::Update, "a remote session is not interrupted");
    });

    // Back on the start screen, it is offered.
    cx.update(|window, cx| view.update(cx, |a, cx| a.close_session("c1", window, cx)));
    tick_until(cx, &view, "update offered", |a| a.st.view == View::Update);
}

#[gpui::test]
fn menu_exit_closes_connected_sessions_and_quits(cx: &mut TestAppContext) {
    let (port, _rx) = fake_mac(64, 36, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "connected session", |a| a.sessions["c1"].status == Status::Connected);
    cx.update(|window, cx| view.update(cx, |a, cx| a.toggle_menu(window, cx)));
    draw(cx);
    let bounds = cx.debug_bounds("act-exit").expect("Exit is visible in the menu");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    view.read_with(cx, |a, _| {
        assert!(a.quit_requested);
        assert!(a.sessions.values().all(|s| s.handle.is_none()), "Exit closes every connection");
    });
}

#[gpui::test]
fn dialog_typing_validation_and_save(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(Store::load(None), None, window, cx));
    cx.simulate_keystrokes("ctrl-n");
    draw(cx);
    // Save with no host: stays open with an error.
    cx.update(|window, cx| view.update(cx, |a, cx| a.save_form(false, window, cx)));
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Dialog);
    assert!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().error.is_some()));
    draw(cx);

    // Name is focused; type a name, tab to host, type host, tab to port.
    cx.simulate_input("My Mac");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("mac.local");
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("backspace backspace backspace backspace");
    cx.simulate_input("5901x");
    draw(cx);
    view.read_with(cx, |a, _| {
        let d = &a.form.as_ref().unwrap().draft;
        assert_eq!((d.name.as_str(), d.host.as_str(), d.port.as_str()), ("My Mac", "mac.local", "5901"));
    });
    cx.update(|window, cx| view.update(cx, |a, cx| a.save_form(false, window, cx)));
    view.read_with(cx, |a, _| {
        assert_eq!(a.st.view, View::Idle);
        assert_eq!(a.store.connections.len(), 1);
        assert_eq!(a.store.connections[0].name, "My Mac");
    });
}

/// Remote Desktop clients often send characters as Unicode input (VK_PACKET), which skips
/// the key-down event and arrives only through the text input handler.
#[gpui::test]
fn dialog_accepts_text_without_key_down(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(Store::load(None), None, window, cx));
    cx.simulate_keystrokes("ctrl-n");
    draw(cx);
    cx.update(|_, cx| view.update(cx, |a, cx| a.type_text("Jump é", cx)));
    cx.simulate_keystrokes("tab");
    cx.update(|_, cx| view.update(cx, |a, cx| a.type_text("10.0.0.5", cx)));
    view.read_with(cx, |a, _| {
        let d = &a.form.as_ref().unwrap().draft;
        assert_eq!((d.name.as_str(), d.host.as_str()), ("Jump é", "10.0.0.5"));
    });
}

#[gpui::test]
fn text_fields_edit_at_keyboard_and_mouse_cursor(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(Store::load(None), None, window, cx));
    cx.simulate_keystrokes("ctrl-n");
    cx.update(|_, cx| view.update(cx, |a, cx| {
        a.form.as_mut().unwrap().draft.name = "aéz".into();
        cx.notify();
    }));
    cx.simulate_keystrokes("left backspace");
    assert_eq!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().draft.name.clone()), "az");
    cx.simulate_keystrokes("x");
    assert_eq!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().draft.name.clone()), "axz");
    cx.simulate_keystrokes("home delete");
    assert_eq!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().draft.name.clone()), "xz");
    draw(cx);
    let bounds = cx.debug_bounds("f-name").unwrap();
    cx.simulate_click(gpui::point(bounds.left() + gpui::px(11.0), bounds.center().y), gpui::Modifiers::default());
    cx.simulate_keystrokes("b");
    assert_eq!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().draft.name.clone()), "bxz");
    cx.simulate_keystrokes("end c");
    assert_eq!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().draft.name.clone()), "bxzc");
}

#[gpui::test]
fn text_fields_select_replace_and_cut_unicode(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(Store::load(None), None, window, cx));
    cx.simulate_keystrokes("ctrl-n");
    view.update(cx, |a, _| a.form.as_mut().unwrap().draft.name = "aéz".into());
    cx.simulate_keystrokes("home shift-right shift-right");
    cx.simulate_keystrokes("ctrl-c");
    cx.update(|_, cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "aé"));
    cx.simulate_keystrokes("x");
    assert_eq!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().draft.name.clone()), "xz");
    cx.simulate_keystrokes("ctrl-a backspace");
    assert_eq!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().draft.name.clone()), "");
    cx.simulate_keystrokes("ctrl-v ctrl-a ctrl-x");
    assert_eq!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().draft.name.clone()), "");
    cx.update(|_, cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "aé"));
    cx.simulate_keystrokes("ctrl-v");
    draw(cx);
    let bounds = cx.debug_bounds("f-name").unwrap();
    let start = gpui::point(bounds.left() + gpui::px(11.0), bounds.center().y);
    cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_mouse_move(bounds.center(), gpui::MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_mouse_up(bounds.center(), gpui::MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_keystrokes("q");
    assert_eq!(view.read_with(cx, |a, _| a.form.as_ref().unwrap().draft.name.clone()), "q");
}

#[gpui::test]
fn find_computers_fills_in_the_host_name(cx: &mut TestAppContext) {
    use crate::discover::{Discovery, Found, Kind, Source};
    let (view, cx) = cx.add_window_view(|window, cx| App::new(Store::load(None), None, window, cx));
    view.update(cx, |a, _| {
        a.discover_start = || {
            Discovery::with_results(vec![
                Found { fullname: "Studio._rfb._tcp.local.".into(), name: "Studio".into(), host: "studio.local".into(), ip: None, port: 5900, kind: Kind::Vnc, source: Source::Bonjour },
                Found { fullname: "Den PC._rdp._tcp.local.".into(), name: "Den PC".into(), host: "den-pc.local".into(), ip: None, port: 3390, kind: Kind::Rdp, source: Source::Tailscale },
            ])
        }
    });
    cx.simulate_keystrokes("ctrl-n");
    draw(cx);
    let b = cx.debug_bounds("btn-find-hosts").expect("search button beside the host field");
    cx.simulate_click(b.center(), gpui::Modifiers::default());
    draw(cx);
    assert!(view.read_with(cx, |a, _| a.discover.is_some()));
    let b = cx.debug_bounds("found-0").expect("first found computer listed");
    cx.simulate_click(b.center(), gpui::Modifiers::default());
    view.read_with(cx, |a, _| {
        assert!(a.discover.is_none(), "picking closes the search");
        assert_eq!(a.st.view, View::Dialog, "the connection dialog stays open");
        let d = &a.form.as_ref().unwrap().draft;
        assert_eq!(d.host, "studio.local");
        assert_eq!(d.name, "Studio", "an empty display name takes the computer's name");
    });

    // Keyboard: down, Enter picks the RDP host, switching protocol and copying its odd port.
    let b = cx.debug_bounds("btn-find-hosts").unwrap();
    cx.simulate_click(b.center(), gpui::Modifiers::default());
    cx.simulate_keystrokes("down enter");
    view.read_with(cx, |a, _| {
        let d = &a.form.as_ref().unwrap().draft;
        assert_eq!((d.host.as_str(), d.proto, d.port.as_str(), d.name.as_str()), ("den-pc.local", Proto::Rdp, "3390", "Studio"));
    });

    // Escape closes only the search, not the dialog.
    let b = cx.debug_bounds("btn-find-hosts").unwrap();
    cx.simulate_click(b.center(), gpui::Modifiers::default());
    cx.simulate_keystrokes("escape");
    view.read_with(cx, |a, _| assert!(a.discover.is_none() && a.st.view == View::Dialog));
}

#[gpui::test]
fn protocol_change_updates_default_port(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(Store::load(None), None, window, cx));
    cx.simulate_keystrokes("ctrl-n");
    view.update(cx, |a, _| {
        a.set_protocol(crate::model::Proto::Rdp);
        assert_eq!(a.form.as_ref().unwrap().draft.port, "3389");
        a.form.as_mut().unwrap().draft.port = "4000".into();
        a.set_protocol(crate::model::Proto::Vnc);
        assert_eq!(a.form.as_ref().unwrap().draft.port, "4000", "custom ports are kept");
    });
    draw(cx);
}

#[gpui::test]
fn pull_tab_slides_sideways_and_a_click_still_pins(cx: &mut TestAppContext) {
    use gpui::{point, px, Modifiers, MouseButton};
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(None), None, window, cx));
    view.update(cx, |a, _| a.st.pinned = false);
    draw(cx);
    let b = cx.debug_bounds("pulltab").expect("pull tab painted");

    // Press and slide right: the tab follows and the toolbar stays unpinned.
    let p = b.center();
    cx.simulate_mouse_down(p, MouseButton::Left, Modifiers::default());
    draw(cx);
    cx.simulate_mouse_move(point(p.x + px(60.0), p.y), MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(point(p.x + px(60.0), p.y), MouseButton::Left, Modifiers::default());
    draw(cx);
    view.read_with(cx, |a, _| {
        assert!(!a.st.pinned, "a slide must not pin the toolbar");
        assert!(a.pull_drag.is_none());
        assert_eq!(a.pull_offset, 60.0);
    });
    let moved = cx.debug_bounds("pulltab").unwrap();
    assert_eq!(moved.left(), b.left() + px(60.0));

    // Sliding far past the edge stops with the whole tab still on screen.
    let p = moved.center();
    cx.simulate_mouse_down(p, MouseButton::Left, Modifiers::default());
    draw(cx);
    cx.simulate_mouse_move(point(px(1.0), p.y), MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(point(px(1.0), p.y), MouseButton::Left, Modifiers::default());
    draw(cx);
    assert_eq!(cx.debug_bounds("pulltab").unwrap().left(), px(0.0));

    // A plain click still pins the toolbar.
    let p = cx.debug_bounds("pulltab").unwrap().center();
    cx.simulate_mouse_down(p, MouseButton::Left, Modifiers::default());
    draw(cx);
    cx.simulate_mouse_up(p, MouseButton::Left, Modifiers::default());
    view.read_with(cx, |a, _| assert!(a.st.pinned, "a click on the pull tab pins the toolbar"));
}

#[gpui::test]
fn connects_to_server_shows_frame_and_forwards_input(cx: &mut TestAppContext) {
    let (port, rx) = fake_mac(64, 36, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    open_and_login(cx, &view, "c1");
    view.read_with(cx, |a, _| {
        assert_eq!(a.st.open, ["c1"]);
        assert!(!a.st.pinned, "default Start-in is toolbar unpinned");
    });
    draw(cx);
    tick_until(cx, &view, "connected frame", |a| {
        a.sessions["c1"].status == Status::Connected && !a.sessions["c1"].tiles.is_empty()
    });
    assert_eq!(view.read_with(cx, |a, _| a.sessions["c1"].size), (64, 36));
    draw(cx);

    // Opening it again must not duplicate (design §6).
    cx.update(|window, cx| view.update(cx, |a, cx| a.open_connection("c1", window, cx)));
    assert_eq!(view.read_with(cx, |a, _| a.st.open.len()), 1);

    // Pull-tab click restores the toolbar; the unpin button hides it again.
    view.update(cx, |a, _| a.st.pinned = true);
    draw(cx);

    // Key press reaches the server as an RFB KeyEvent for 'a'.
    cx.simulate_keystrokes("a");
    view.update(cx, |a, _| a.send_ctrl_alt_del());
    let end = Instant::now() + Duration::from_secs(5);
    let mut seen = vec![];
    while Instant::now() < end {
        if let Ok(v) = rx.recv_timeout(Duration::from_millis(100)) {
            seen = v;
            // key down 'a' + ctrl,alt,del down/up
            let has = |m: [u8; 8]| seen.windows(8).any(|w| w == m);
            if has([4, 1, 0, 0, 0, 0, 0, 0x61]) && has([4, 1, 0, 0, 0, 0, 0xff, 0xff]) && has([4, 0, 0, 0, 0, 0, 0xff, 0xff]) {
                break;
            }
        }
    }
    assert!(seen.windows(8).any(|w| w == [4, 1, 0, 0, 0, 0, 0, 0x61]), "'a' key down not received: {seen:?}");
    assert!(seen.windows(8).any(|w| w == [4, 1, 0, 0, 0, 0, 0xff, 0xff]), "Delete key down not received");
    assert!(seen.windows(8).any(|w| w == [4, 0, 0, 0, 0, 0, 0xff, 0xff]), "Delete key up not received");

    // Closing the last session returns to the empty state.
    cx.update(|window, cx| view.update(cx, |a, cx| a.close_session("c1", window, cx)));
    assert!(view.read_with(cx, |a, _| a.st.no_sessions() && a.sessions.is_empty()));
    draw(cx);
}

#[gpui::test]
fn clicking_the_remote_picture_sends_mouse_buttons(cx: &mut TestAppContext) {
    let (port, rx) = fake_mac(64, 36, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "connected frame", |a| a.sessions["c1"].status == Status::Connected && !a.sessions["c1"].tiles.is_empty());
    draw(cx);
    let area = view.read_with(cx, |a, _| a.area.get());
    cx.simulate_click(area.center(), gpui::Modifiers::default());
    let end = Instant::now() + Duration::from_secs(5);
    let (mut down, mut up) = (false, false);
    while Instant::now() < end && !(down && up) {
        if let Ok(v) = rx.recv_timeout(Duration::from_millis(100)) {
            // RFB PointerEvent: [5, buttons, x(2), y(2)]
            down |= v.windows(2).any(|w| w == [5, 1]);
            up |= down && v.windows(2).any(|w| w == [5, 0]);
        }
    }
    assert!(down, "a click on the picture must send a button-down pointer event");
    assert!(up, "and the matching button-up");
}

#[gpui::test]
fn two_monitors_selector_shortcuts_and_both_view(cx: &mut TestAppContext) {
    let (port, _rx) = fake_mac(128, 36, true);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "two monitors", |a| a.sessions["c1"].monitors == 2 && !a.sessions["c1"].tiles.is_empty());
    view.update(cx, |a, _| a.st.pinned = true);
    draw(cx);

    cx.simulate_keystrokes("ctrl-alt-home");
    assert_eq!(view.read_with(cx, |a, _| a.st.monitor("c1")), Mon::All);
    tick_until(cx, &view, "both tiles", |a| a.sessions["c1"].tiles.len() == 2);
    draw(cx);

    cx.simulate_keystrokes("ctrl-alt-pagedown");
    assert_eq!(view.read_with(cx, |a, _| a.st.monitor("c1")), Mon::Two);
    tick_until(cx, &view, "monitor 2 tile", |a| a.sessions["c1"].tiles.len() == 1 && a.sessions["c1"].tiles[0].size == (64, 36));
    draw(cx);

    cx.simulate_keystrokes("ctrl-alt-pageup");
    assert_eq!(view.read_with(cx, |a, _| a.st.monitor("c1")), Mon::One);
}

/// macOS reports no monitors over plain VNC: the layout remembered from Apple HP
/// splits the desktop, but only when it spans exactly this desktop's size.
#[gpui::test]
fn vnc_uses_the_remembered_display_layout(cx: &mut TestAppContext) {
    let (port, _rx) = fake_mac(128, 36, false);
    let mut store = store_with(Some(port));
    store.get_mut("c1").unwrap().display_layout = vec![[0, 0, 64, 36], [64, 0, 64, 36]];
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store, None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "two monitors from the layout", |a| a.sessions["c1"].monitors == 2 && !a.sessions["c1"].tiles.is_empty());
    cx.simulate_keystrokes("ctrl-alt-home");
    tick_until(cx, &view, "both live tiles", |a| a.sessions["c1"].tiles.iter().map(|t| (t.monitor, t.size)).collect::<Vec<_>>() == vec![(0, (64, 36)), (1, (64, 36))]);
    cx.simulate_keystrokes("ctrl-alt-pagedown");
    tick_until(cx, &view, "monitor 2", |a| a.sessions["c1"].tiles.len() == 1 && a.sessions["c1"].tiles[0].monitor == 1);
}

#[gpui::test]
fn vnc_ignores_a_layout_for_a_different_desktop(cx: &mut TestAppContext) {
    let (port, _rx) = fake_mac(128, 36, false);
    let mut store = store_with(Some(port));
    store.get_mut("c1").unwrap().display_layout = vec![[0, 0, 64, 36], [64, 0, 80, 36]];
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store, None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "picture", |a| !a.sessions["c1"].tiles.is_empty());
    for _ in 0..5 {
        cx.update(|window, cx| view.update(cx, |app, cx| app.tick(window, cx)));
    }
    assert_eq!(view.read_with(cx, |a, _| a.sessions["c1"].monitors), 1);
}

#[gpui::test]
fn session_shortcuts_switcher_and_fullscreen(cx: &mut TestAppContext) {
    let (p1, _r1) = fake_mac(32, 18, false);
    let (p2, _r2) = fake_mac(32, 18, false);
    let mut store = store_with(Some(p1));
    let mut c = Connection::blank("c3".into());
    c.proto = Proto::Vnc;
    c.name = "Other".into();
    c.host = "127.0.0.1".into();
    c.port = p2.to_string();
    c.start_in = crate::model::StartIn::WindowPinned;
    store.upsert(c);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store, None, window, cx));
    open_and_login(cx, &view, "c1");
    open_and_login(cx, &view, "c3");
    assert_eq!(view.read_with(cx, |a, _| a.st.active.clone()), Some("c3".into()));
    tick_until(cx, &view, "both connected", |a| a.sessions.values().all(|s| s.status == Status::Connected));
    draw(cx);

    cx.simulate_keystrokes("ctrl-alt-1");
    assert_eq!(view.read_with(cx, |a, _| a.st.active.clone()), Some("c1".into()));
    cx.simulate_keystrokes("ctrl-alt-tab");
    assert_eq!(view.read_with(cx, |a, _| a.st.active.clone()), Some("c3".into()));

    view.update(cx, |a, _| a.st.set_view(View::Switcher));
    draw(cx);
    cx.simulate_keystrokes("escape");
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Idle);

    cx.simulate_keystrokes("ctrl-alt-enter");
    view.read_with(cx, |a, _| assert!(a.st.full && !a.st.pinned));
    draw(cx);
    cx.simulate_keystrokes("ctrl-alt-enter");
    view.read_with(cx, |a, _| assert!(!a.st.full));
    draw(cx);

    // Menu lists both as active and shows the Open badge path without panicking.
    cx.update(|window, cx| view.update(cx, |a, cx| a.toggle_menu(window, cx)));
    draw(cx);
    cx.simulate_keystrokes("escape");
}

#[gpui::test]
fn failed_connection_shows_error_state_and_rdp_connects_like_the_others(cx: &mut TestAppContext) {
    // Port 1 on localhost: refused.
    let mut store = store_with(None);
    let mut rdp = Connection::blank("c9".into());
    rdp.name = "Win".into();
    rdp.host = "127.0.0.1".into();
    rdp.port = "1".into();
    rdp.proto = crate::model::Proto::Rdp;
    store.upsert(rdp);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store, None, window, cx));
    open_and_login(cx, &view, "c2");
    tick_until(cx, &view, "failure", |a| matches!(a.sessions["c2"].status, Status::Failed(_)));
    draw(cx);
    // RDP asks for credentials and really tries to connect (there is an RDP client now).
    open_and_login(cx, &view, "c9");
    tick_until(cx, &view, "RDP failure", |a| matches!(a.sessions["c9"].status, Status::Failed(_)));
    view.read_with(cx, |a, _| match &a.sessions["c9"].status {
        Status::Failed(m) => assert!(!m.contains("not supported"), "RDP must not be reported as unsupported: {m}"),
        s => panic!("{s:?}"),
    });
    draw(cx);
}

#[gpui::test]
fn password_prompt_flow(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(None), None, window, cx));
    // Opening an "Ask when connecting" connection asks before connecting.
    cx.update(|window, cx| view.update(cx, |a, cx| a.open_connection("c1", window, cx)));
    draw(cx);
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Password);
    assert!(view.read_with(cx, |a, _| a.sessions["c1"].handle.is_none()), "must not connect before the prompt is answered");
    // Password field is focused; type it, then go back to the user name with shift-tab.
    cx.simulate_input("secret");
    view.read_with(cx, |a, _| assert_eq!(a.pw.as_ref().unwrap().pass, "secret"));
    cx.simulate_keystrokes("shift-tab");
    cx.simulate_input("alice");
    view.read_with(cx, |a, _| assert_eq!(a.pw.as_ref().unwrap().user, "alice"));
    draw(cx);
    // Cancelling abandons the never-started session.
    cx.simulate_keystrokes("escape");
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Idle);
    assert!(view.read_with(cx, |a, _| a.pw.is_none() && a.st.no_sessions() && a.sessions.is_empty()));
}

#[gpui::test]
fn quick_connect_creates_recent_for_unknown_host(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(Store::load(None), None, window, cx));
    cx.update(|window, cx| view.update(cx, |a, cx| a.toggle_menu(window, cx)));
    cx.simulate_input("nohost.invalid:5999");
    draw(cx);
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |a, _| {
        assert_eq!(a.store.connections.len(), 1);
        let c = &a.store.connections[0];
        assert_eq!((c.host.as_str(), c.port.as_str(), c.fav), ("nohost.invalid", "5999", false));
        assert_eq!(a.st.open.len(), 1);
    });
}

#[gpui::test]
fn submitted_login_is_used_for_the_connection(cx: &mut TestAppContext) {
    let (port, _rx) = fake_mac(32, 18, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    cx.update(|window, cx| view.update(cx, |a, cx| a.open_connection("c1", window, cx)));
    cx.simulate_input("pw");
    cx.simulate_keystrokes("shift-tab");
    cx.simulate_input("bob");
    cx.simulate_keystrokes("enter");
    view.read_with(cx, |a, _| {
        let s = &a.sessions["c1"];
        assert_eq!((s.user.as_str(), s.pass.as_str()), ("bob", "pw"));
        assert!(s.handle.is_some(), "connecting after the prompt");
        assert_eq!(a.store.get("c1").unwrap().user, "bob", "user name is remembered");
    });
    tick_until(cx, &view, "connected", |a| a.sessions["c1"].status == Status::Connected);
}

#[gpui::test]
fn vault_password_is_saved_from_dialog_and_used_silently(cx: &mut TestAppContext) {
    let (port, _rx) = fake_mac(32, 18, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(Store::load(None), None, window, cx));
    cx.simulate_keystrokes("ctrl-n");
    draw(cx);
    cx.simulate_input("Vaulted");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("127.0.0.1");
    cx.simulate_keystrokes("tab backspace backspace backspace backspace");
    cx.simulate_input(&port.to_string());
    cx.simulate_keystrokes("tab");
    cx.simulate_input("carol");
    // Password field only exists once "Save in credential vault" is chosen.
    view.update(cx, |a, _| {
        let d = &mut a.form.as_mut().unwrap().draft;
        d.credentials = CredentialMode::Vault;
        d.proto = Proto::Vnc;
    });
    draw(cx);
    cx.simulate_keystrokes("tab");
    assert_eq!(view.read_with(cx, |a, _| a.field), crate::app::Field::Password);
    cx.simulate_input("hunter2");
    draw(cx);
    cx.update(|window, cx| view.update(cx, |a, cx| a.save_form(true, window, cx)));
    // No prompt: the stored password is used straight away.
    view.read_with(cx, |a, _| {
        assert_eq!(a.st.view, View::Idle);
        let id = a.st.open[0].clone();
        assert_eq!(a.sessions[&id].pass, "hunter2");
        assert_eq!(a.sessions[&id].user, "carol");
        assert!(a.sessions[&id].handle.is_some());
        let json = serde_json::to_string(&a.store).unwrap();
        assert!(!json.contains("hunter2"), "password must never be in connections.json");
    });
}

/// The whole Apple High Performance path through the real UI, using the built-in
/// synthetic source (SEEMYMAC_DEMO) so no Mac is required.
#[gpui::test]
fn apple_hp_session_streams_from_the_synthetic_source(cx: &mut TestAppContext) {
    std::env::set_var("SEEMYMAC_DEMO", "1");
    let mut store = Store::load(None);
    let mut c = Connection::blank("hp1".into());
    c.name = "Demo Mac".into();
    c.host = "demo.invalid".into();
    c.credentials = CredentialMode::Ask;
    c.resolution = crate::model::Resolution::Fhd;
    assert_eq!(c.proto, Proto::Apple, "Apple HP is the default protocol");
    store.upsert(c);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store, None, window, cx));
    open_and_login(cx, &view, "hp1");
    tick_until(cx, &view, "HP frame", |a| a.sessions["hp1"].status == Status::Connected && !a.sessions["hp1"].tiles.is_empty());
    std::env::remove_var("SEEMYMAC_DEMO");
    assert_eq!(view.read_with(cx, |a, _| a.sessions["hp1"].size), (1920, 1080));
    draw(cx);
    // Pointer and keys are accepted without error; closing stops the session.
    view.update(cx, |a, _| a.send_ctrl_alt_del());
    cx.update(|window, cx| view.update(cx, |a, cx| a.close_session("hp1", window, cx)));
    assert!(view.read_with(cx, |a, _| a.st.no_sessions()));
}

/// Apple HP streams only the Mac's main physical display, so a Mac with two shows
/// one monitor (its layout is remembered for VNC). With two virtual displays each
/// is streamed separately: monitors 1, 2 and "Both" are all live. The synthetic
/// source plays the Mac (display 2's picture is tinted green).
#[gpui::test]
fn apple_hp_two_virtual_displays_are_live_monitors(cx: &mut TestAppContext) {
    std::env::set_var("SEEMYMAC_DEMO", "1");
    let mut store = Store::load(None);
    let mut c = Connection::blank("hp2".into());
    c.host = "two-displays.invalid".into();
    c.credentials = CredentialMode::Ask;
    c.resolution = crate::model::Resolution::Hd;
    c.virtual_display = true;
    c.virtual_displays = 2;
    store.upsert(c);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store, None, window, cx));
    open_and_login(cx, &view, "hp2");
    tick_until(cx, &view, "HP picture", |a| a.sessions["hp2"].status == Status::Connected && !a.sessions["hp2"].tiles.is_empty());
    std::env::remove_var("SEEMYMAC_DEMO");
    view.update(cx, |a, _| a.st.pinned = true);
    assert_eq!(view.read_with(cx, |a, _| a.sessions["hp2"].monitors), 1);
    cx.simulate_keystrokes("ctrl-alt-v");
    // After unlocking, switching creates two live monitors.
    tick_until(cx, &view, "two monitors", |a| a.sessions["hp2"].monitors == 2);
    let green = |a: &App| a.sessions["hp2"].tiles.first().and_then(|t| t.image.as_bytes(0).map(|b| b[1] == 255)).unwrap_or(false);
    cx.simulate_keystrokes("ctrl-alt-pagedown");
    tick_until(cx, &view, "monitor 2 (the second stream)", |a| a.sessions["hp2"].tiles.len() == 1 && a.sessions["hp2"].tiles[0].monitor == 1 && green(a));
    draw(cx);
    cx.simulate_keystrokes("ctrl-alt-home");
    tick_until(cx, &view, "both monitors", |a| a.sessions["hp2"].tiles.iter().map(|t| t.monitor).collect::<Vec<_>>() == vec![0, 1]);
    draw(cx);
    cx.simulate_keystrokes("ctrl-alt-pageup");
    tick_until(cx, &view, "monitor 1", |a| a.sessions["hp2"].tiles.len() == 1 && a.sessions["hp2"].tiles[0].monitor == 0 && !green(a));
    cx.update(|window, cx| view.update(cx, |a, cx| a.close_session("hp2", window, cx)));
}

/// With one virtual display there is no second stream: a single monitor.
#[gpui::test]
fn apple_hp_one_virtual_display_is_one_monitor(cx: &mut TestAppContext) {
    std::env::set_var("SEEMYMAC_DEMO", "1");
    let mut store = Store::load(None);
    let mut c = Connection::blank("hp3".into());
    c.host = "two-displays.invalid".into();
    c.credentials = CredentialMode::Ask;
    c.resolution = crate::model::Resolution::Hd;
    c.virtual_display = true;
    store.upsert(c);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store, None, window, cx));
    open_and_login(cx, &view, "hp3");
    tick_until(cx, &view, "HP picture", |a| !a.sessions["hp3"].tiles.is_empty());
    std::env::remove_var("SEEMYMAC_DEMO");
    assert!(view.read_with(cx, |a, _| a.sessions["hp3"].handle.as_ref().unwrap().fb2.is_none()));
    cx.simulate_keystrokes("ctrl-alt-v");
    for _ in 0..10 {
        cx.update(|window, cx| view.update(cx, |app, cx| app.tick(window, cx)));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(view.read_with(cx, |a, _| a.sessions["hp3"].monitors), 1);
    cx.update(|window, cx| view.update(cx, |a, cx| a.close_session("hp3", window, cx)));
}

/// Keep the login window visible until the user chooses the virtual display.
#[gpui::test]
fn apple_hp_virtual_display_waits_for_user_after_login(cx: &mut TestAppContext) {
    std::env::set_var("SEEMYMAC_DEMO", "1");
    let mut store = Store::load(None);
    let mut c = Connection::blank("hpv".into());
    c.host = "demo.invalid".into();
    c.credentials = CredentialMode::Ask;
    c.virtual_display = true;
    store.upsert(c);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store, None, window, cx));
    open_and_login(cx, &view, "hpv");
    tick_until(cx, &view, "HP frame", |a| a.sessions["hpv"].status == Status::Connected && !a.sessions["hpv"].tiles.is_empty());
    std::env::remove_var("SEEMYMAC_DEMO");
    assert!(view.read_with(cx, |a, _| a.sessions["hpv"].virtual_pending.is_some() && !a.sessions["hpv"].on_virtual));
    draw(cx);
    assert!(cx.debug_bounds("virtual-banner").is_some(), "offer switching after the Mac is unlocked");
    cx.simulate_keystrokes("ctrl-alt-v");
    assert!(view.read_with(cx, |a, _| a.sessions["hpv"].virtual_pending.is_none() && a.sessions["hpv"].on_virtual));
    // A VNC session never offers it.
    let (port, _rx) = fake_mac(32, 18, false);
    view.update(cx, |a, _| {
        let mut v = Connection::blank("vnc".into());
        v.proto = Proto::Vnc;
        v.host = "127.0.0.1".into();
        v.port = port.to_string();
        v.virtual_display = true;
        a.store.upsert(v);
    });
    open_and_login(cx, &view, "vnc");
    tick_until(cx, &view, "VNC frame", |a| a.sessions["vnc"].status == Status::Connected);
    assert!(view.read_with(cx, |a, _| a.sessions["vnc"].virtual_pending.is_none()));
}

// ----- regression guard: overlays must really be on screen ---------------------------

/// The bug that made the dialog, switcher and sign-in prompt invisible in the real
/// window: they were in the element tree (so state tests passed) but laid out with no
/// size. Check the painted bounds, not just the state.
fn assert_on_screen(cx: &mut VisualTestContext, selector: &'static str) {
    draw(cx);
    let b = cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} was not painted"));
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    assert!(w >= 300.0 && h >= 120.0, "{selector} has a degenerate size {w}x{h}");
    let win = cx.update(|window, _| window.viewport_size());
    assert!(f32::from(b.origin.x) >= 0.0 && f32::from(b.origin.y) >= 0.0, "{selector} starts off screen");
    assert!(f32::from(b.origin.x) + w <= f32::from(win.width) + 1.0 && f32::from(b.origin.y) + h <= f32::from(win.height) + 1.0, "{selector} runs off screen: {b:?} in {win:?}");
}

#[gpui::test]
fn every_overlay_is_painted_with_a_real_size(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(None), None, window, cx));
    cx.update(|window, cx| view.update(cx, |a, cx| a.toggle_menu(window, cx)));
    assert_on_screen(cx, "menu");
    cx.simulate_keystrokes("escape");

    cx.simulate_keystrokes("ctrl-n");
    assert_on_screen(cx, "dialog");
    // Every tab of the dialog.
    for tab in [crate::app::DialogTab::Display, crate::app::DialogTab::Local, crate::app::DialogTab::General] {
        view.update(cx, |a, _| a.form.as_mut().unwrap().tab = tab);
        assert_on_screen(cx, "dialog");
    }
    cx.simulate_keystrokes("escape");

    cx.simulate_keystrokes("ctrl-m");
    assert_on_screen(cx, "manage");
    cx.simulate_keystrokes("escape");

    cx.simulate_keystrokes("ctrl-,");
    for tab in [crate::app::SettingsTab::General, crate::app::SettingsTab::Input, crate::app::SettingsTab::AppleHp] {
        view.update(cx, |a, _| a.settings_tab = tab);
        assert_on_screen(cx, "settings");
    }
    cx.simulate_keystrokes("escape");

    // Sign-in prompt and session switcher need a session.
    cx.update(|window, cx| view.update(cx, |a, cx| a.open_connection("c2", window, cx)));
    assert_on_screen(cx, "pw");
    cx.simulate_keystrokes("escape");
    view.update(cx, |a, _| {
        let mut c = a.store.get("c1").unwrap().clone();
        c.credentials = CredentialMode::Vault;
        a.store.upsert(c);
    });
    cx.update(|window, cx| view.update(cx, |a, cx| a.open_connection("c1", window, cx)));
    view.update(cx, |a, _| a.st.set_view(View::Switcher));
    draw(cx);
    assert!(cx.debug_bounds("switcher").is_some(), "switcher was not painted");
}

/// A server that turns the password down brings the sign-in dialog back (not a raw error), and a
/// new password typed there with "Save password" ticked lands in the vault.
#[gpui::test]
fn rejected_login_asks_again_and_saves_the_new_password(cx: &mut TestAppContext) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.write_all(b"RFB 003.008\n").unwrap();
        s.read_exact(&mut [0u8; 12]).unwrap();
        s.write_all(&[1, 2]).unwrap();
        s.read_exact(&mut [0u8; 1]).unwrap();
        s.write_all(&[0; 16]).unwrap();
        s.read_exact(&mut [0u8; 16]).unwrap();
        s.write_all(&1u32.to_be_bytes()).unwrap();
        s.write_all(&12u32.to_be_bytes()).unwrap();
        s.write_all(b"Bad password").unwrap();
    });
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    crate::vault::delete("c1");
    cx.update(|window, cx| view.update(cx, |a, cx| a.open_connection("c1", window, cx)));
    draw(cx);
    cx.simulate_input("wrong");
    cx.simulate_keystrokes("enter");
    tick_until(cx, &view, "the sign-in dialog again", |a| a.st.view == View::Password && a.pw.as_ref().map_or(false, |p| p.error.is_some()));
    view.read_with(cx, |a, _| {
        let p = a.pw.as_ref().unwrap();
        assert!(p.error.as_deref().unwrap().contains("Bad password"), "{:?}", p.error);
        assert!(!p.save, "an \"Ask\" connection starts unticked");
    });
    draw(cx);
    let save = cx.debug_bounds("pw-save").expect("save checkbox painted").center();
    cx.simulate_click(save, gpui::Modifiers::none());
    assert!(view.read_with(cx, |a, _| a.pw.as_ref().unwrap().save));
    cx.simulate_input("right");
    cx.simulate_keystrokes("enter");
    assert_eq!(crate::vault::get("c1").unwrap().as_deref(), Some("right"));
    assert_eq!(view.read_with(cx, |a, _| a.store.get("c1").unwrap().credentials), CredentialMode::Vault);
    crate::vault::delete("c1");
}

// ----- connection manager -----------------------------------------------------------

#[gpui::test]
fn manager_edits_duplicates_favorites_and_deletes_with_confirmation(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(None), None, window, cx));
    cx.simulate_keystrokes("ctrl-m");
    draw(cx);
    // c2 is not a favourite and was never used by name, yet the manager lists it.
    assert_eq!(view.read_with(cx, |a, _| a.store.search(&a.manage_query).len()), 2);

    // Search narrows the list.
    cx.simulate_input("stud");
    assert_eq!(view.read_with(cx, |a, _| a.store.search(&a.manage_query).len()), 1);
    draw(cx);
    view.update(cx, |a, _| a.manage_query.clear());

    // Duplicate.
    cx.update(|_, cx| view.update(cx, |a, cx| a.duplicate_connection("c1", cx)));
    view.read_with(cx, |a, _| {
        assert_eq!(a.store.connections.len(), 3);
        let copy = a.store.connections.last().unwrap();
        assert_eq!(copy.name, "Studio copy");
        assert_ne!(copy.id, "c1");
    });

    // Edit from the manager, change the name, save: back in the manager, not on the canvas.
    cx.update(|window, cx| view.update(cx, |a, cx| a.edit_connection_dialog("c1", window, cx)));
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Dialog);
    view.update(cx, |a, _| a.form.as_mut().unwrap().draft.name = "Renamed".into());
    cx.update(|window, cx| view.update(cx, |a, cx| a.save_form(false, window, cx)));
    view.read_with(cx, |a, _| {
        assert_eq!(a.st.view, View::Manage, "returns to the manager");
        assert_eq!(a.store.get("c1").unwrap().name, "Renamed");
    });
    draw(cx);

    // Cancelling an edit also returns to the manager.
    cx.update(|window, cx| view.update(cx, |a, cx| a.edit_connection_dialog("c2", window, cx)));
    cx.simulate_keystrokes("escape");
    assert_eq!(view.read_with(cx, |a, _| a.st.view), View::Manage);

    // Delete needs two presses.
    cx.update(|window, cx| view.update(cx, |a, cx| a.delete_connection("c2", window, cx)));
    assert!(view.read_with(cx, |a, _| a.store.get("c2").is_some()), "first press only arms");
    draw(cx);
    cx.update(|window, cx| view.update(cx, |a, cx| a.delete_connection("c2", window, cx)));
    assert!(view.read_with(cx, |a, _| a.store.get("c2").is_none()));
    // Pressing Delete on one row then another must not delete the second.
    cx.update(|window, cx| view.update(cx, |a, cx| a.delete_connection("c1", window, cx)));
    cx.update(|window, cx| view.update(cx, |a, cx| a.delete_connection("c3", window, cx)));
    assert!(view.read_with(cx, |a, _| a.store.get("c1").is_some()), "confirmation is per connection");
    draw(cx);
}

#[gpui::test]
fn deleting_an_open_connection_closes_its_session_and_forgets_its_password(cx: &mut TestAppContext) {
    let (port, _rx) = fake_mac(32, 18, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "connected", |a| a.sessions["c1"].status == Status::Connected);
    crate::vault::set("c1", "pw").unwrap();
    cx.simulate_keystrokes("ctrl-m");
    cx.update(|window, cx| view.update(cx, |a, cx| a.delete_connection("c1", window, cx)));
    cx.update(|window, cx| view.update(cx, |a, cx| a.delete_connection("c1", window, cx)));
    view.read_with(cx, |a, _| {
        assert!(a.st.no_sessions() && a.sessions.is_empty() && a.store.get("c1").is_none());
    });
    assert_eq!(crate::vault::get("c1").unwrap(), None);
}

// ----- settings ---------------------------------------------------------------------------

#[gpui::test]
fn settings_drive_new_connection_defaults_and_persist(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("smm-ui-set-{}", std::process::id()));
    let path = dir.join("settings.json");
    let settings = crate::settings::Settings::load(Some(path.clone()));
    let (view, cx) = cx.add_window_view(|window, cx| App::with_settings(Store::load(None), settings, None, window, cx));
    cx.simulate_keystrokes("ctrl-,");
    view.update(cx, |a, _| {
        a.settings.default_proto = Proto::Vnc;
        a.settings.default_start_in = crate::model::StartIn::WindowPinned;
        a.settings.default_virtual_display = false;
        a.save_settings();
    });
    cx.simulate_keystrokes("escape");
    cx.simulate_keystrokes("ctrl-n");
    view.read_with(cx, |a, _| {
        let d = &a.form.as_ref().unwrap().draft;
        assert_eq!((d.proto, d.port.as_str(), d.start_in, d.virtual_display), (Proto::Vnc, "5900", crate::model::StartIn::WindowPinned, false));
    });
    // Survives a restart.
    let again = crate::settings::Settings::load(Some(path));
    assert_eq!(again.default_proto, Proto::Vnc);
    let _ = std::fs::remove_dir_all(dir);
}

#[gpui::test]
fn apple_hp_settings_check_the_built_in_decoder(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(Store::load(None), None, window, cx));
    cx.simulate_keystrokes("ctrl-,");
    view.update(cx, |a, _| a.settings_tab = crate::app::SettingsTab::AppleHp);
    draw(cx);
    cx.update(|_, cx| view.update(cx, |a, cx| a.check_hp(cx)));
    tick_until(cx, &view, "decoder check", |a| a.hp_result.as_deref().map_or(false, |r| r != "Checking…"));
    let r = view.read_with(cx, |a, _| a.hp_result.clone().unwrap());
    assert!(r.starts_with("Ready."), "{r}");
    draw(cx);
}

#[gpui::test]
fn command_key_setting_swaps_modifiers_sent_to_the_mac(cx: &mut TestAppContext) {
    let (port, rx) = fake_mac(32, 18, false);
    let mut settings = crate::settings::Settings::default();
    settings.command_key = crate::settings::CommandKey::CtrlIsCommand;
    let (view, cx) = cx.add_window_view(|window, cx| App::with_settings(store_with(Some(port)), settings, None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "connected", |a| a.sessions["c1"].status == Status::Connected);
    // Local Ctrl down => remote Super (Command, 0xffeb).
    cx.update(|window, cx| {
        view.update(cx, |a, cx| {
            a.on_modifiers(&gpui::ModifiersChangedEvent { modifiers: gpui::Modifiers { control: true, ..Default::default() }, capslock: gpui::Capslock::default() }, window, cx)
        })
    });
    let end = Instant::now() + Duration::from_secs(5);
    let mut seen = vec![];
    while Instant::now() < end {
        if let Ok(v) = rx.recv_timeout(Duration::from_millis(100)) {
            seen = v;
            if seen.windows(8).any(|w| w == [4, 1, 0, 0, 0, 0, 0xff, 0xeb]) {
                break;
            }
        }
    }
    assert!(seen.windows(8).any(|w| w == [4, 1, 0, 0, 0, 0, 0xff, 0xeb]), "Ctrl should arrive as Command: {seen:?}");
    assert!(!seen.windows(8).any(|w| w == [4, 1, 0, 0, 0, 0, 0xff, 0xe3]), "and not as Control");
}

#[gpui::test]
fn held_modifiers_from_key_events_reach_the_server(cx: &mut TestAppContext) {
    use gpui::{KeyDownEvent, KeyUpEvent, Keystroke, Modifiers};

    let (port, rx) = fake_mac(32, 18, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::with_settings(store_with(Some(port)), crate::settings::Settings::default(), None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "connected", |a| a.sessions["c1"].status == Status::Connected);
    draw(cx);

    // A modifier can already be held when the remote view first receives input.
    let modifiers = Modifiers { shift: true, control: true, ..Default::default() };
    let key = Keystroke { modifiers, key: "left".into(), key_char: None };
    cx.simulate_event(KeyDownEvent { keystroke: key, is_held: false });
    cx.simulate_event(KeyUpEvent {
        keystroke: Keystroke { modifiers: Modifiers::default(), key: "left".into(), key_char: None },
    });

    let expected = [(1, 0xe1), (1, 0xe3), (1, 0x51), (0, 0x51), (0, 0xe1), (0, 0xe3)];
    let end = Instant::now() + Duration::from_secs(5);
    while let Ok(bytes) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
        let mut rest = bytes.as_slice();
        if expected.iter().all(|&(down, key)| {
            let Some(pos) = rest.windows(8).position(|w| w == [4, down, 0, 0, 0, 0, 0xff, key]) else {
                return false;
            };
            rest = &rest[pos + 8..];
            true
        }) {
            return;
        }
    }
    panic!("expected Shift and Control to surround the Left Arrow key events");
}

#[gpui::test]
fn cmd_space_combo_reaches_the_mac_as_command_space(cx: &mut TestAppContext) {
    let (port, rx) = fake_mac(32, 18, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "connected", |a| a.sessions["c1"].status == Status::Connected);
    let i = crate::app::KEY_COMBOS.iter().position(|c| c.label.starts_with("Cmd+Space")).expect("Cmd+Space is offered");
    view.update(cx, |a, _| a.send_key_combo(i));
    // KeyEvent (4): Command down, Space down, Space up, Command up.
    let want: [[u8; 8]; 4] = [[4, 1, 0, 0, 0, 0, 0xff, 0xeb], [4, 1, 0, 0, 0, 0, 0, 0x20], [4, 0, 0, 0, 0, 0, 0, 0x20], [4, 0, 0, 0, 0, 0, 0xff, 0xeb]];
    let end = Instant::now() + Duration::from_secs(5);
    let mut seen = vec![];
    while Instant::now() < end && !want.iter().all(|w| seen.windows(8).any(|s| s == w)) {
        if let Ok(v) = rx.recv_timeout(Duration::from_millis(100)) {
            seen = v;
        }
    }
    let pos = |w: &[u8; 8]| seen.windows(8).position(|s| s == w);
    let order: Vec<_> = want.iter().map(pos).collect();
    assert!(order.iter().all(Option::is_some), "all four key events sent: {seen:?}");
    assert!(order.windows(2).all(|p| p[0] < p[1]), "in order: {order:?}");
}

#[gpui::test]
fn turning_the_toolbar_off_gives_its_room_to_the_remote_screen(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(None), None, window, cx));
    draw(cx);
    let with_bar = view.read_with(cx, |a, _| a.area.get().origin.y);
    view.update(cx, |a, _| a.settings.show_toolbar = false);
    draw(cx);
    let without = view.read_with(cx, |a, _| a.area.get().origin.y);
    if cfg!(target_os = "macos") {
        assert_eq!(f32::from(with_bar - without), crate::view::TOOLBAR_H, "the toolbar's height goes to the remote screen");
    } else {
        assert_eq!(with_bar, without, "only macOS, with its menu bar, can hide the toolbar");
    }
}

/// Live check against a real Mac, through the real UI code (not run by default):
///
///   SEEMYMAC_LIVE=c4 SEEMYMAC_LIVE_STEPS="snap:a;click:1325,1455;wait:2;snap:b" \
///       cargo test --lib live_ -- --ignored --nocapture
///
/// Connects with the saved connection SEEMYMAC_LIVE (password from the system keyring;
/// SEEMYMAC_LIVE_VIRTUAL=1 for a virtual display) and runs the steps through the app's
/// own event handlers: `snap:NAME` saves the framebuffer to SEEMYMAC_LIVE_SHOTS,
/// `click:X,Y` / `rclick:X,Y` / `move:X,Y` left/right-click or point at remote pixel X,Y, `type:TEXT`, `password` (the saved one, never
/// printed), `key:NAME` (e.g. enter, backspace, escape), `shortcut:ctrl-alt-v`, `banner`
/// (reports whether the virtual-display banner is painted), `require_locked` (abort
/// unless the lock screen shows), `env:KEY=VALUE`, `wait:SECONDS`.
#[gpui::test]
#[ignore]
fn live_apple_hp_typing_reaches_the_mac(cx: &mut TestAppContext) {
    use gpui::{KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent};
    let _ = env_logger::builder().is_test(true).filter_module("see_my_mac", log::LevelFilter::Debug).try_init();
    let id: &'static str = Box::leak(std::env::var("SEEMYMAC_LIVE").expect("SEEMYMAC_LIVE=connection id").into_boxed_str());
    // A copy of the saved connection in an in-memory store: the overrides below
    // must never be written back to the user's connections.json.
    let mut c = Store::load(Store::default_path()).get(id).expect("saved connection").clone();
    let mut store = Store::load(None);
    c.virtual_display = std::env::var_os("SEEMYMAC_LIVE_VIRTUAL").is_some();
    if let Some(n) = std::env::var("SEEMYMAC_LIVE_DISPLAYS").ok().and_then(|v| v.parse().ok()) {
        c.virtual_displays = n;
    }
    if std::env::var("SEEMYMAC_LIVE_PROTO").as_deref() == Ok("vnc") {
        c.proto = Proto::Vnc;
    }
    c.credentials = CredentialMode::Vault;
    store.upsert(c);
    let pw = keyring::Entry::new("SeeMyMac", id).unwrap().get_password().expect("saved password");
    crate::vault::set(id, &pw).unwrap();
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store, None, window, cx));
    cx.update(|window, cx| view.update(cx, |a, cx| a.open_connection(id, window, cx)));
    let pump = |cx: &mut VisualTestContext, secs: f32| {
        let end = Instant::now() + Duration::from_secs_f32(secs);
        while Instant::now() < end {
            cx.update(|window, cx| view.update(cx, |app, cx| app.tick(window, cx)));
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let end = Instant::now() + Duration::from_secs(40);
    while !view.read_with(cx, |a, _| a.sessions[id].status == Status::Connected && !a.sessions[id].tiles.is_empty()) {
        assert!(Instant::now() < end, "never connected: {:?}", view.read_with(cx, |a, _| a.sessions[id].status.clone()));
        pump(cx, 0.05);
    }
    draw(cx);
    pump(cx, 1.0);
    let shots = std::env::var("SEEMYMAC_LIVE_SHOTS").unwrap_or_else(|_| "/tmp".into());
    // Type one character the way a real keyboard does: Shift as a modifier change
    // for shifted symbols, then key down/up carrying the produced character.
    let type_char = |cx: &mut VisualTestContext, ch: char| {
        let shifted = ch.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(ch);
        let base = if ch.is_ascii_uppercase() { ch.to_ascii_lowercase().to_string() } else { ch.to_string() };
        let mods = Modifiers { shift: shifted, ..Default::default() };
        if shifted {
            cx.simulate_event(ModifiersChangedEvent { modifiers: mods, capslock: Default::default() });
        }
        let ks = Keystroke { modifiers: mods, key: base, key_char: Some(ch.to_string()) };
        cx.simulate_event(KeyDownEvent { keystroke: ks.clone(), is_held: false });
        cx.simulate_event(KeyUpEvent { keystroke: ks });
        if shifted {
            cx.simulate_event(ModifiersChangedEvent { modifiers: Modifiers::default(), capslock: Default::default() });
        }
    };
    let steps = std::env::var("SEEMYMAC_LIVE_STEPS").unwrap_or_else(|_| "snap:start".into());
    for step in steps.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (op, arg) = step.split_once(':').unwrap_or((step, ""));
        match op {
            "snap" => view.read_with(cx, |a, _| {
                // What is drawn: every tile (the UI takes HP frames out of the framebuffer).
                for (i, t) in a.sessions[id].tiles.iter().enumerate() {
                    let mut ppm = format!("P6\n{} {}\n255\n", t.size.0, t.size.1).into_bytes();
                    for px in t.image.as_bytes(0).unwrap_or_default().chunks_exact(4) {
                        ppm.extend_from_slice(&[px[2], px[1], px[0]]);
                    }
                    std::fs::write(format!("{shots}/live_{arg}_{i}.ppm"), ppm).unwrap();
                }
            }),
            "state" => view.read_with(cx, |a, _| {
                let s = &a.sessions[id];
                let f = s.handle.as_ref().unwrap().fb.lock().unwrap();
                println!(
                    "LIVE: monitor choice {:?}, monitors {}, on virtual {}, tiles {:?}, second stream version {:?}, fb showing {:?}, displays {:?}",
                    a.st.monitor(id),
                    s.monitors,
                    s.on_virtual,
                    s.tiles.iter().map(|t| (t.monitor, t.size, t.resolution)).collect::<Vec<_>>(),
                    s.handle.as_ref().unwrap().fb2.as_ref().map(|f2| f2.lock().unwrap().version),
                    f.showing,
                    f.displays
                );
            }),
            "click" | "rclick" | "move" => {
                // Remote pixel -> window position, through the same geometry the app uses.
                let (x, y) = arg.split_once(',').map(|(x, y)| (x.parse::<f32>().unwrap(), y.parse::<f32>().unwrap())).unwrap();
                let (area, size, scaling) = view.read_with(cx, |a, _| (a.area.get(), a.sessions[id].tiles[0].size, a.sessions[id].scaling));
                let r = crate::geometry::image_rect(f32::from(area.size.width), f32::from(area.size.height), size.0 as f32, size.1 as f32, scaling);
                let pos = gpui::point(area.origin.x + gpui::px(r.x + (x + 0.5) * r.w / size.0 as f32), area.origin.y + gpui::px(r.y + (y + 0.5) * r.h / size.1 as f32));
                cx.simulate_mouse_move(pos, None, Modifiers::default());
                pump(cx, 0.2);
                if op == "move" {
                    // Wiggle so the Mac redraws its cursor at the final spot.
                    for d in [8.0, 4.0, 0.0] {
                        cx.simulate_mouse_move(gpui::point(pos.x - gpui::px(d), pos.y - gpui::px(d)), None, Modifiers::default());
                        pump(cx, 0.05);
                    }
                } else if op == "click" {
                    cx.simulate_click(pos, Modifiers::default());
                } else {
                    cx.simulate_mouse_down(pos, gpui::MouseButton::Right, Modifiers::default());
                    cx.simulate_mouse_up(pos, gpui::MouseButton::Right, Modifiers::default());
                }
            }
            "type" => arg.chars().for_each(|ch| type_char(cx, ch)),
            "password" => pw.chars().for_each(|ch| {
                type_char(cx, ch);
                std::thread::sleep(Duration::from_millis(30));
            }),
            "key" => {
                let ks = Keystroke { modifiers: Modifiers::default(), key: arg.to_string(), key_char: None };
                cx.simulate_event(KeyDownEvent { keystroke: ks.clone(), is_held: false });
                cx.simulate_event(KeyUpEvent { keystroke: ks });
            }
            "wait" => pump(cx, arg.parse().unwrap()),
            "env" => {
                let (k, v) = arg.split_once('=').unwrap();
                if v.is_empty() { std::env::remove_var(k) } else { std::env::set_var(k, v) }
            }
            "require_locked" => {
                // Refuse to continue (and so to type a password) unless the lock screen
                // shows: its large clock (white, or lavender on the dynamic wallpaper)
                // in the top centre, over mostly deep-blue wallpaper.
                let (clock, blue, total) = view.read_with(cx, |a, _| {
                    // The drawn picture (the UI takes HP frames out of the framebuffer).
                    let t = &a.sessions[id].tiles[0];
                    let bgra = t.image.as_bytes(0).unwrap_or_default();
                    let (w, h) = (t.size.0 as usize, t.size.1 as usize);
                    let (mut clock, mut blue, mut total) = (0, 0, 0);
                    for y in h * 6 / 100..h * 22 / 100 {
                        for x in w * 38 / 100..w * 62 / 100 {
                            let p = &bgra[(y * w + x) * 4..(y * w + x) * 4 + 3];
                            let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
                            let white = b > 200 && g > 200 && r > 200;
                            let lavender = b > 230 && r > 130 && g > 100 && b - g > 60;
                            clock += (white || lavender) as usize;
                            blue += (b > r + 80 && b > g + 80) as usize;
                            total += 1;
                        }
                    }
                    (clock, blue, total)
                });
                println!("LIVE: lock-screen clock pixels: {clock}, wallpaper blue {}%", blue * 100 / total.max(1));
                assert!(clock > 1500 && blue * 100 >= total * 60, "the Mac does not show its lock screen; refusing to type the password");
            }
            "banner" => {
                draw(cx);
                let shown = cx.debug_bounds("virtual-banner").is_some();
                let pending = view.read_with(cx, |a, _| a.sessions[id].virtual_pending.is_some());
                println!("LIVE: virtual-display banner {} (switch pending: {pending})", if shown { "painted" } else { "not painted" });
            }
            "shortcut" => cx.simulate_keystrokes(arg),
            // "display:all" / "display:ID": send SetDisplayMessage directly.
            "display" => view.read_with(cx, |a, _| {
                let h = a.sessions[id].handle.as_ref().unwrap();
                let d = match arg {
                    "all" => None,
                    // "other": the last listed display that is not the one shown.
                    "other" => {
                        let f = h.fb.lock().unwrap();
                        f.displays.iter().rev().map(|d| d.id).find(|d| Some(*d) != f.showing)
                    }
                    _ => Some(u32::from_str_radix(arg.trim_start_matches("0x"), 16).unwrap()),
                };
                println!("LIVE: asking for display {d:?}");
                h.send(crate::rfb::Input::ShowDisplay(d));
            }),
            other => panic!("unknown step {other}"),
        }
        pump(cx, 0.1);
    }
    cx.update(|window, cx| view.update(cx, |a, cx| a.close_session(id, window, cx)));
}

/// This PC's clipboard text reaches a Mac VNC session (as Apple's 0x1f, so every
/// character survives) once, not again on every poll.
#[gpui::test]
fn local_clipboard_text_is_sent_to_the_session_once(cx: &mut TestAppContext) {
    use crate::hp::clipboard::{text, MSG_SEND};
    let (port, rx) = fake_mac(64, 36, false);
    let (view, cx) = cx.add_window_view(|window, cx| App::new(store_with(Some(port)), None, window, cx));
    open_and_login(cx, &view, "c1");
    tick_until(cx, &view, "connected", |a| a.sessions["c1"].status == Status::Connected);
    cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("copied ✓\r\nhere".into())));
    let texts = |all: &[u8]| -> Vec<String> {
        (0..all.len()).filter(|&i| all[i] == MSG_SEND && i + 16 <= all.len()).filter_map(|i| {
            let n = u32::from_be_bytes(all[i + 12..i + 16].try_into().unwrap()) as usize;
            all.get(i..i + 16 + n).and_then(text)
        }).collect()
    };
    // The clipboard is only read while the window has focus.
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    assert!(cx.update(|window, _| window.is_window_active()));
    let end = Instant::now() + Duration::from_secs(5);
    let mut all = vec![];
    while texts(&all).is_empty() {
        assert!(Instant::now() < end, "clipboard text never reached the server");
        cx.update(|window, cx| view.update(cx, |app, cx| app.tick(window, cx)));
        while let Ok(v) = rx.try_recv() {
            all = v;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(texts(&all), ["copied ✓\nhere"], "sent with Unix line endings");
    // Unchanged clipboard: nothing more is sent over the next few polls.
    let end = Instant::now() + Duration::from_millis(1800);
    while Instant::now() < end {
        cx.update(|window, cx| view.update(cx, |app, cx| app.tick(window, cx)));
        while let Ok(v) = rx.try_recv() {
            all = v;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(texts(&all).len(), 1, "sent once");
    cx.update(|window, cx| view.update(cx, |a, cx| a.close_session("c1", window, cx)));
}
