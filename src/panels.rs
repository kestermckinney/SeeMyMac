//! Connection manager and application settings: two modal panels in the same visual
//! language as the connection dialog (ui_design/README.md §9).

use crate::app::{App, Field, SettingsTab, UpdateStatus};
use crate::model::*;
use crate::settings::{CommandKey, Settings, RECONNECT_CHOICES};
use crate::view::*;
use gpui::prelude::*;
use gpui::*;
use std::rc::Rc;

fn small_btn(id: impl Into<ElementId>, label: impl Into<SharedString>, color: u32) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .flex_none()
        .rounded(px(6.0))
        .cursor_pointer()
        .text_size(px(12.0))
        .text_color(rgb(color))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|s| s.bg(rgb(SELECTED)))
        .child(label.into())
}

fn form_row(label: &str, control: AnyElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(12.0))
        .min_h(px(40.0))
        .child(div().w(px(190.0)).flex_none().child(label_text(label)))
        .child(div().flex_1().min_w_0().child(control))
}

impl App {
    /// The dark scrim plus a centred panel. Clicking the scrim closes it.
    fn modal(
        &self,
        scrim_id: &'static str,
        selector: &'static str,
        width: f32,
        title: &str,
        tabs: Option<AnyElement>,
        body: AnyElement,
        footer: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(scrim_id)
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .bg(rgba(0x010409B3))
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx)))
            .child(
                div()
                    .id(selector)
                    .debug_selector(move || selector.into())
                    .w(px(width))
                    .max_w_full()
                    .rounded(px(12.0))
                    .bg(rgb(SURFACE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(div().px(px(20.0)).pt(px(16.0)).text_size(px(16.0)).font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
                    .child(div().flex().px(px(12.0)).mt(px(8.0)).border_b_1().border_color(rgb(DIVIDER)).children(tabs))
                    .child(div().px(px(20.0)).py(px(12.0)).w_full().child(body))
                    .child(div().flex().items_center().gap(px(8.0)).px(px(20.0)).py(px(14.0)).border_t_1().border_color(rgb(DIVIDER)).child(footer)),
            )
            .into_any_element()
    }

    // ----- connection manager ------------------------------------------------

    pub(crate) fn manage_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let all: Vec<Connection> = self.store.search(&self.manage_query).into_iter().cloned().collect();
        let total = self.store.connections.len();

        let mut list = div().id("manage-list").flex().flex_col().max_h(px(340.0)).overflow_y_scroll();
        for (i, c) in all.iter().enumerate() {
            let open = self.st.is_open(&c.id);
            let armed = self.confirm_delete.as_deref() == Some(c.id.as_str());
            let (cid, eid, did, xid) = (c.id.clone(), c.id.clone(), c.id.clone(), c.id.clone());
            list = list.child(
                div()
                    .id(("mrow", i))
                    .min_h(px(52.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .border_b_1()
                    .border_color(rgb(DIVIDER))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().truncate().child(c.display_name().to_string()))
                            .child(div().truncate().text_size(px(12.0)).text_color(rgb(MUTED)).child(format!("{} · {}:{}{}", c.proto.label(), c.host, c.port_number(), if c.user.is_empty() { String::new() } else { format!(" · {}", c.user) }))),
                    )
                    .when(open, |d| d.child(div().px(px(6.0)).py(px(1.0)).rounded(px(4.0)).bg(rgb(SELECTED)).border_1().border_color(rgb(BORDER)).text_size(px(11.0)).child("Open")))
                    .child(small_btn(("mconn", i), "Connect", TEXT).on_click(cx.listener(move |this, _, window, cx| this.open_connection(&cid, window, cx))))
                    .child(small_btn(("medit", i), "Edit", TEXT).on_click(cx.listener(move |this, _, window, cx| this.edit_connection_dialog(&eid, window, cx))))
                    .child(small_btn(("mdup", i), "Copy", TEXT).on_click(cx.listener(move |this, _, _, cx| this.duplicate_connection(&did, cx))))
                    .child(
                        small_btn(("mdel", i), if armed { "Sure?" } else { "Delete" }, DANGER)
                            .when(armed, |d| d.border_color(rgb(DANGER)))
                            .on_click(cx.listener(move |this, _, window, cx| this.delete_connection(&xid, window, cx))),
                    ),
            );
        }
        if all.is_empty() {
            list = list.child(div().p(px(16.0)).text_color(rgb(MUTED)).child(if total == 0 { "No saved connections yet. Choose New connection… to add one." } else { "No connections match your search." }));
        }

        let body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(self.text_field("manage-search", Field::Manage, &self.manage_query, &format!("Search {total} saved connection{}", if total == 1 { "" } else { "s" }), false, cx))
            .child(list)
            .child(div().text_size(px(12.0)).text_color(rgb(MUTED)).child("Press Delete twice to remove a connection and its saved password."))
            .into_any_element();

        let footer = div()
            .flex()
            .items_center()
            .w_full()
            .gap(px(8.0))
            .child(secondary_btn("manage-new", "New connection…").on_click(cx.listener(|this, _, window, cx| {
                this.new_connection_dialog(window, cx);
                this.dialog_return = crate::state::View::Manage;
            })))
            .child(div().flex_1())
            .child(primary_btn("manage-close", "Close").on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx))))
            .into_any_element();

        self.modal("manage-scrim", "manage", 720.0, "All connections", None, body, footer, cx)
    }

    // ----- settings -----------------------------------------------------------

    fn settings_cycle<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        all: &'static [T],
        current: T,
        label: impl Fn(T) -> String,
        apply: impl Fn(&mut Settings, T) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let labels = all.iter().map(|v| SharedString::from(label(*v))).collect();
        let cur = all.iter().position(|x| *x == current).unwrap_or(0);
        self.dropdown(id, labels, cur, Rc::new(move |this, i| {
            apply(&mut this.settings, all[i]);
            this.save_settings();
        }), cx)
    }

    fn settings_check(&self, id: &'static str, text: &'static str, on: bool, apply: impl Fn(&mut Settings, bool) + 'static, cx: &mut Context<Self>) -> AnyElement {
        let dd = self.dropdown(id, vec!["Yes".into(), "No".into()], if on { 0 } else { 1 }, Rc::new(move |this, i| {
            apply(&mut this.settings, i == 0);
            this.save_settings();
        }), cx);
        div().flex().items_center().gap(px(12.0)).min_h(px(40.0)).child(div().w(px(190.0)).flex_none().child(label_text(text))).child(div().flex_1().min_w_0().child(dd)).into_any_element()
    }

    pub(crate) fn settings_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let st = &self.settings;
        let tab = |id: &'static str, text: &'static str, t: SettingsTab, cx: &mut Context<Self>| {
            let sel = self.settings_tab == t;
            div()
                .id(id)
                .px(px(14.0))
                .h(px(36.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .border_b_2()
                .border_color(rgb(if sel { ACCENT_DARK } else { SURFACE }))
                .text_color(rgb(if sel { TEXT } else { MUTED }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings_tab = t;
                    this.field = Field::None;
                    cx.notify();
                }))
                .child(text)
        };
        let tabs = div().flex().child(tab("st-general", "General", SettingsTab::General, cx)).child(tab("st-input", "Keyboard", SettingsTab::Input, cx)).child(tab("st-hp", "Apple HP", SettingsTab::AppleHp, cx)).into_any_element();

        let body: AnyElement = match self.settings_tab {
            SettingsTab::General => {
                let data_path = Store::default_path().map(|p| p.display().to_string()).unwrap_or_else(|| "(not available)".into());
                div()
                    .flex()
                    .flex_col()
                    .child(div().pb(px(4.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child("Defaults for new connections"))
                    .child(form_row("Protocol", self.settings_cycle("s-proto", &Proto::ALL, st.default_proto, |p| p.label().to_string(), |s, v| s.default_proto = v, cx)))
                    .child(form_row("Start in", self.settings_cycle("s-start", &StartIn::ALL, st.default_start_in, |p| p.label().to_string(), |s, v| s.default_start_in = v, cx)))
                    .child(form_row("Scaling", self.settings_cycle("s-scale", &Scaling::ALL, st.default_scaling, |p| p.label().to_string(), |s, v| s.default_scaling = v, cx)))
                    .child(self.settings_check("s-virtual", "Use a virtual display for Apple HP", st.default_virtual_display, |s, v| s.default_virtual_display = v, cx))
                    .child(div().pl(px(202.0)).pb(px(4.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child("Sessions start on the Mac's own screen. Keep that screen to view existing windows. Use virtual display (Ctrl+Alt+V) switches to a separate display, which may start with an empty desktop."))
                    .child(div().pt(px(10.0)).pb(px(4.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child("Files"))
                    .child(form_row("Save files from the Mac in", self.text_field("s-download", Field::Download, &st.download_dir, "~/Downloads", false, cx).into_any_element()))
                    .child(div().pt(px(10.0)).pb(px(4.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child("Updates"))
                    .child(self.settings_check("s-updates", "Check for a new version when SeeMyMac starts", st.check_updates, |s, v| s.check_updates = v, cx))
                    .child(div().pt(px(10.0)).pb(px(4.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child("Connections"))
                    .child(form_row("Reconnect attempts", self.settings_cycle("s-reconnect", &RECONNECT_CHOICES, st.max_reconnects, |n| if n == 0 { "Never reconnect".into() } else { format!("Up to {n}") }, |s, v| s.max_reconnects = v, cx)))
                    .child(form_row(
                        "Saved passwords",
                        div()
                            .flex()
                            .child(
                                small_btn("s-forget", if self.confirm_forget { "Click again to forget every saved password" } else { "Forget all saved passwords" }, DANGER)
                                    .when(self.confirm_forget, |d| d.border_color(rgb(DANGER)))
                                    .on_click(cx.listener(|this, _, _, cx| this.forget_all_passwords(cx))),
                            )
                            .into_any_element(),
                    ))
                    .child(div().pt(px(6.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child(format!("Connections are stored in {data_path}. Passwords are kept in the system keyring, never in that file.")))
                    .into_any_element()
            }
            SettingsTab::Input => div()
                .flex()
                .flex_col()
                .child(form_row("Mac Command key", self.settings_cycle("s-cmd", &CommandKey::ALL, st.command_key, |c| c.label().to_string(), |s, v| s.command_key = v, cx)))
                .child(form_row(
                    "System shortcuts",
                    self.settings_cycle("s-syskeys", &[true, false], st.capture_system_keys, |on| if on { "Send to the Mac (Super, Super+Space, Alt+Tab…)".into() } else { "Keep for this PC".into() }, |s, v| s.capture_system_keys = v, cx),
                ))
                .child(form_row(
                    "Zoomed in",
                    self.settings_cycle("s-edge", &[true, false], st.edge_scroll, |on| if on { "Scroll when the pointer touches a window edge".into() } else { "Scroll with the wheel or Ctrl+Alt+arrows only".into() }, |s, v| s.edge_scroll = v, cx),
                ))
                .child(div().pb(px(6.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child("GNOME asks once whether to allow this. Super+Escape gives the shortcuts back to this PC until you click the session again."))
                .child(div().pt(px(10.0)).pb(px(4.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child("Keyboard shortcuts"))
                .children(
                    [
                        ("Ctrl+Alt+Tab", "Switch session"),
                        ("Ctrl+Alt+1 … 9", "Go to session"),
                        ("Ctrl+Alt+PgUp / PgDn / Home", "Monitor 1 / 2 / Both"),
                        ("Ctrl+Alt+Enter", "Full screen"),
                        ("Ctrl+Alt+Arrows", "Scroll a zoomed-in picture"),
                        ("Ctrl+Alt+V", "Apple HP: use the virtual display"),
                        ("Ctrl+N", "New connection"),
                        ("Ctrl+M", "Manage all connections"),
                        ("Ctrl+,", "Settings"),
                        ("Esc", "Close a panel"),
                    ]
                    .map(|(k, v)| div().flex().h(px(24.0)).child(div().w(px(230.0)).text_color(rgb(MUTED)).child(k)).child(div().child(v))),
                )
                .into_any_element(),
            SettingsTab::AppleHp => div()
                .w_full()
                .flex()
                .flex_col()
                .child(div().pb(px(6.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child("Apple High Performance is built in. SeeMyMac negotiates the session with the Mac directly and decodes its H.264 video with a bundled decoder; no other software is needed."))
                .child(div().pb(px(6.0)).text_size(px(12.0)).text_color(rgb(MUTED)).child("Requirements: a Mac with Apple silicon running macOS 14 or later, Screen Sharing turned on, an IPv4 address, and UDP ports 5900 and 5901 (the connection port and the next one) open between this computer and the Mac."))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .py(px(8.0))
                        .child(secondary_btn("s-check", "Check decoder").on_click(cx.listener(|this, _, _, cx| this.check_hp(cx))))
                        .children(self.hp_result.clone().map(|r| div().flex_1().min_w_0().text_size(px(12.0)).text_color(rgb(if r.starts_with("Ready") { OK } else { DANGER })).child(r))),
                )
                .into_any_element(),
        };

        let footer = div()
            .flex()
            .w_full()
            .child(div().flex_1())
            .child(primary_btn("settings-close", "Done").on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx))))
            .into_any_element();
        self.modal("settings-scrim", "settings", 640.0, "Settings", Some(tabs), div().w_full().min_h(px(400.0)).child(body).into_any_element(), footer, cx)
    }

    // ----- about ----------------------------------------------------------------

    pub(crate) fn about_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let link = |id: &'static str, text: &'static str, url: &'static str, cx: &mut Context<Self>| {
            div()
                .id(id)
                .cursor_pointer()
                .text_color(rgb(ACCENT))
                .hover(|s| s.underline())
                .child(text)
                .on_click(cx.listener(move |_, _, _, cx| cx.open_url(url)))
        };
        let body = div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.0))
            .py(px(8.0))
            .children(app_icon().map(|i| img(i).w(px(96.0)).h(px(96.0)).object_fit(ObjectFit::Contain)))
            .child(div().mt(px(6.0)).text_size(px(20.0)).font_weight(FontWeight::SEMIBOLD).child("SeeMyMac"))
            .child(div().text_color(rgb(MUTED)).child(format!("Version {}", env!("CARGO_PKG_VERSION"))))
            .child(div().text_size(px(12.0)).text_color(rgb(MUTED)).child(format!("Built {}", env!("SEEMYMAC_BUILD_TIME"))))
            .child(self.update_status_line(cx))
            .child(div().mt(px(8.0)).child(link("about-gitlab", "gitlab.com/kestermckinney/SeeMyMac", "https://gitlab.com/kestermckinney/SeeMyMac", cx)))
            .child(div().w_full().h(px(1.0)).bg(rgb(DIVIDER)).my(px(10.0)))
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Project managers: meet Project Notes"))
            .child(
                div()
                    .max_w(px(380.0))
                    .text_align(TextAlign::Center)
                    .text_color(rgb(MUTED))
                    .child("Keep project notes, action items and status reports in one place so nothing slips. Try Project Notes, our project notes and action items software, free."),
            )
            .child(link("about-pnp", "www.projectnotespro.com", "https://www.projectnotespro.com", cx))
            .into_any_element();
        let busy = matches!(self.update, UpdateStatus::Checking | UpdateStatus::Downloading { .. } | UpdateStatus::Installing);
        let footer = div()
            .flex()
            .w_full()
            .child(secondary_btn("about-update", if busy { "Checking…" } else { "Check for updates" }).on_click(cx.listener(|this, _, _, cx| this.start_update_check(true, cx))))
            .child(div().flex_1())
            .child(primary_btn("about-close", "Close").on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx))))
            .into_any_element();
        self.modal("about-scrim", "about", 460.0, "About SeeMyMac", None, body, footer, cx)
    }

    /// The line under the version on the About screen that says how an update check went.
    fn update_status_line(&self, _cx: &mut Context<Self>) -> AnyElement {
        let (text, color) = match &self.update {
            UpdateStatus::Checking => ("Checking for updates…".to_string(), MUTED),
            UpdateStatus::UpToDate => ("You have the latest version.".to_string(), MUTED),
            UpdateStatus::Info(m) => (m.clone(), MUTED),
            UpdateStatus::Failed(m) => (m.clone(), DANGER),
            _ => return div().into_any_element(),
        };
        div().mt(px(6.0)).max_w(px(380.0)).text_align(TextAlign::Center).text_size(px(12.0)).text_color(rgb(color)).child(text).into_any_element()
    }

    // ----- software update prompt --------------------------------------------------

    pub(crate) fn update_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let mb = |n: u64| format!("{:.1} MB", n as f64 / 1_048_576.0);
        let (title, body, footer): (String, AnyElement, AnyElement) = match &self.update {
            UpdateStatus::Available(r) => {
                let notes: String = r.notes.trim().chars().take(1500).collect();
                (
                    format!("SeeMyMac {} is available", r.version),
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(div().child(format!("You have version {}. Download and install version {} now? SeeMyMac will restart.", crate::update::current_version(), r.version)))
                        .children((!notes.is_empty()).then(|| div().id("update-notes").max_h(px(220.0)).overflow_y_scroll().p(px(10.0)).rounded(px(6.0)).border_1().border_color(rgb(BORDER)).text_size(px(12.0)).text_color(rgb(MUTED)).child(notes)))
                        .into_any_element(),
                    div()
                        .flex()
                        .w_full()
                        .gap(px(8.0))
                        .child(div().flex_1())
                        .child(secondary_btn("update-later", "Later").on_click(cx.listener(|this, _, window, cx| this.dismiss_update(window, cx))))
                        .child(primary_btn("update-install", "Install and restart").on_click(cx.listener(|this, _, _, cx| this.start_update_download(cx))))
                        .into_any_element(),
                )
            }
            UpdateStatus::Downloading { received, total } => {
                let frac = total.filter(|t| *t > 0).map(|t| (*received as f32 / t as f32).clamp(0.0, 1.0));
                let label = match total {
                    Some(t) => format!("Downloading… {} of {}", mb(*received), mb(*t)),
                    None => format!("Downloading… {}", mb(*received)),
                };
                (
                    "Downloading the update".to_string(),
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(div().child(label))
                        .child(div().w_full().h(px(6.0)).rounded(px(3.0)).bg(rgb(BORDER)).child(div().h_full().rounded(px(3.0)).bg(rgb(ACCENT)).w(relative(frac.unwrap_or(0.05)))))
                        .child(div().text_size(px(12.0)).text_color(rgb(MUTED)).child("SeeMyMac restarts by itself when the update is installed."))
                        .into_any_element(),
                    div()
                        .flex()
                        .w_full()
                        .child(div().flex_1())
                        .child(secondary_btn("update-cancel", "Cancel").on_click(cx.listener(|this, _, window, cx| this.dismiss_update(window, cx))))
                        .into_any_element(),
                )
            }
            UpdateStatus::Installing => (
                "Installing the update".to_string(),
                div().child("Restarting SeeMyMac…").into_any_element(),
                div().into_any_element(),
            ),
            UpdateStatus::Failed(m) => (
                "The update did not install".to_string(),
                div().max_w(px(440.0)).text_color(rgb(DANGER)).child(m.clone()).into_any_element(),
                div().flex().w_full().child(div().flex_1()).child(primary_btn("update-close", "Close").on_click(cx.listener(|this, _, window, cx| this.dismiss_update(window, cx)))).into_any_element(),
            ),
            _ => return div().into_any_element(),
        };
        self.modal("update-scrim", "update", 480.0, &title, None, body, footer, cx)
    }
}
