//! Rendering, following the layout and tokens in ui_design/README.md.

use crate::app::{App, DialogTab, Field, Status, KEY_COMBOS};
use crate::geometry;
use crate::model::*;
use crate::state::{Mon, View};
use gpui::prelude::*;
use gpui::*;
use std::rc::Rc;

// Visual tokens (design §9).
pub(crate) const BG_CHROME: u32 = 0x0D1117;
pub(crate) const BG_TITLE: u32 = 0x010409;
pub(crate) const SURFACE: u32 = 0x161B22;
pub(crate) const SELECTED: u32 = 0x1F2630;
pub(crate) const BORDER: u32 = 0x30363D;
pub(crate) const DIVIDER: u32 = 0x262C36;
pub(crate) const TEXT: u32 = 0xE6EDF3;
pub(crate) const MUTED: u32 = 0x9198A1;
pub(crate) const ACCENT: u32 = 0x2F6FEB;
pub(crate) const ACCENT_DARK: u32 = 0x6CA4FF;
pub(crate) const OK: u32 = 0x3FB950;
pub(crate) const WARN: u32 = 0xD29922;
pub(crate) const DANGER: u32 = 0xF47067;

/// macOS and Windows use the OS window frame; Linux draws its own GNOME-style header bar.
pub const NATIVE_FRAME: bool = cfg!(any(target_os = "macos", target_os = "windows"));
pub(crate) const TITLE_H: f32 = 32.0;
pub(crate) const TOOLBAR_H: f32 = 34.0;
pub(crate) const PULL_H: f32 = 24.0;

fn status_color(s: &Status) -> u32 {
    match s {
        Status::Connected => OK,
        Status::Connecting | Status::Reconnecting => WARN,
        Status::Failed(_) => DANGER,
    }
}

pub(crate) fn dot(color: u32) -> Div {
    div().size(px(8.0)).rounded_full().bg(rgb(color)).flex_none()
}

pub(crate) fn tool_btn(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .flex_none()
        .rounded(px(6.0))
        .cursor_pointer()
        .text_color(rgb(TEXT))
        .hover(|s| s.bg(rgb(SELECTED)))
        .child(label.into())
}

pub(crate) fn primary_btn(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.0))
        .px(px(14.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .bg(rgb(ACCENT))
        .text_color(rgb(0xFFFFFF))
        .font_weight(FontWeight::SEMIBOLD)
        .hover(|s| s.opacity(0.9))
        .child(label.into())
}

pub(crate) fn secondary_btn(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.0))
        .px(px(14.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .border_1()
        .border_color(rgb(BORDER))
        .text_color(rgb(TEXT))
        .hover(|s| s.bg(rgb(SELECTED)))
        .child(label.into())
}

pub(crate) fn label_text(t: &str) -> Div {
    div().text_color(rgb(MUTED)).text_size(px(12.0)).child(t.to_string())
}

impl App {
    pub(crate) fn text_field(&self, id: &'static str, field: Field, value: &str, placeholder: &str, masked: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let focused = self.field == field;
        let shown: String = if masked { "•".repeat(value.chars().count()) } else { value.to_string() };
        let cursor = self.cursor_for(field, value);
        let display_cursor = if masked { value[..cursor].chars().count() * "•".len() } else { cursor };
        let anchor = self.field_anchor.filter(|(f, _)| *f == field).map(|(_, i)| i.min(value.len())).unwrap_or(cursor);
        let display_anchor = if masked { value[..anchor].chars().count() * "•".len() } else { anchor };
        let placeholder = placeholder.to_string();
        let actual = value.to_string();
        let hit = Rc::new(std::cell::RefCell::new(None::<(Bounds<Pixels>, ShapedLine, Pixels)>));
        let paint_hit = hit.clone();
        let drag_hit = hit.clone();
        let drag_actual = actual.clone();
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .h(px(32.0))
            .w_full()
            .px(px(10.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .bg(rgb(BG_CHROME))
            .border_1()
            .border_color(rgb(if focused { ACCENT_DARK } else { BORDER }))
            .cursor_text()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                let old_cursor = this.cursor_for(field, &actual);
                this.field = field;
                this.field_anchor = if ev.modifiers.shift { Some((field, this.field_anchor.filter(|(f, _)| *f == field).map_or(old_cursor, |(_, i)| i))) } else { None };
                if let Some((bounds, line, offset)) = hit.borrow().as_ref() {
                    let index = line.closest_index_for_x(ev.position.x - bounds.left() + *offset);
                    let index = if masked {
                        actual.char_indices().nth(index / "•".len()).map_or(actual.len(), |(i, _)| i)
                    } else { index.min(actual.len()) };
                    if !ev.modifiers.shift { this.field_anchor = Some((field, index)); }
                    this.field_cursor = Some((field, index));
                }
                window.focus(&this.focus);
                cx.notify();
            }))
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                if ev.pressed_button != Some(MouseButton::Left) || this.field != field || this.field_anchor.is_none() { return; }
                if let Some((bounds, line, offset)) = drag_hit.borrow().as_ref() {
                    let index = line.closest_index_for_x(ev.position.x - bounds.left() + *offset);
                    let index = if masked { drag_actual.char_indices().nth(index / "•".len()).map_or(drag_actual.len(), |(i, _)| i) } else { index.min(drag_actual.len()) };
                    this.field_cursor = Some((field, index));
                    cx.notify();
                }
            }))
            .child(canvas(
                move |bounds, window, _| {
                    let style = window.text_style();
                    let empty = shown.is_empty() && !focused;
                    let text: SharedString = if empty { placeholder.clone().into() } else { shown.clone().into() };
                    let run = TextRun { len: text.len(), font: style.font(), color: rgb(if empty { MUTED } else { TEXT }).into(), background_color: None, underline: None, strikethrough: None };
                    let line = window.text_system().shape_line(text, style.font_size.to_pixels(window.rem_size()), &[run], None);
                    let caret = line.x_for_index(display_cursor);
                    let offset = if focused { (caret - bounds.size.width + px(2.0)).max(px(0.0)) } else { px(0.0) };
                    *paint_hit.borrow_mut() = Some((bounds, line.clone(), offset));
                    let anchor = line.x_for_index(display_anchor);
                    (line, caret, offset, anchor)
                },
                move |bounds, (line, caret, offset, anchor), window, cx| {
                    if focused && caret != anchor {
                        window.paint_quad(fill(Bounds::new(point(bounds.left() + caret.min(anchor) - offset, bounds.top()), size((caret - anchor).abs(), px(20.0))), rgb(ACCENT)));
                    }
                    line.paint(point(bounds.left() - offset, bounds.top()), px(20.0), window, cx).ok();
                    if focused {
                        window.paint_quad(fill(Bounds::new(point(bounds.left() + caret - offset, bounds.top() + px(2.0)), size(px(1.0), px(16.0))), rgb(ACCENT_DARK)));
                    }
                },
            ).w_full().h(px(20.0)))
    }

    /// The styled list shared by every dropdown and toolbar picker.
    pub(crate) fn popup_list(id: &'static str, labels: &[SharedString], current: Option<usize>, on_pick: Rc<dyn Fn(&mut App, usize)>, cx: &mut Context<Self>) -> Stateful<Div> {
        let mut list = div()
            .id(SharedString::from(format!("{id}-list")))
            .occlude()
            .p(px(4.0))
            .flex()
            .flex_col()
            .rounded(px(6.0))
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(BORDER))
            .shadow_lg();
        for (i, l) in labels.iter().enumerate() {
            let f = on_pick.clone();
            list = list.child(
                div()
                    .id((id, i))
                    .h(px(28.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .flex_none()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .bg(rgb(if current == Some(i) { SELECTED } else { SURFACE }))
                    .hover(|s| s.bg(rgb(SELECTED)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick = None;
                        f(this, i);
                        cx.notify();
                    }))
                    .child(l.clone()),
            );
        }
        list
    }

    /// A drop-down picker: a button showing the current choice that opens a list.
    pub(crate) fn dropdown(&self, id: &'static str, labels: Vec<SharedString>, current: usize, on_pick: Rc<dyn Fn(&mut App, usize)>, cx: &mut Context<Self>) -> AnyElement {
        let open = self.pick == Some(id);
        let button = div()
            .id(id)
            .h(px(32.0))
            .w_full()
            .px(px(10.0))
            .flex()
            .items_center()
            .justify_between()
            .rounded(px(6.0))
            .bg(rgb(BG_CHROME))
            .border_1()
            .border_color(rgb(if open { ACCENT_DARK } else { BORDER }))
            .cursor_pointer()
            .hover(|s| s.border_color(rgb(ACCENT_DARK)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.pick = if this.pick == Some(id) { None } else { Some(id) };
                cx.notify();
            }))
            .child(labels.get(current).cloned().unwrap_or_default())
            .child(div().text_color(rgb(MUTED)).child("▾"));
        let list = Self::popup_list(id, &labels, Some(current), on_pick, cx).absolute().top(px(34.0)).left_0().w_full();
        div().relative().w_full().child(button).when(open, |d| d.child(deferred(list).with_priority(1))).into_any_element()
    }

    pub(crate) fn cycle<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        all: &'static [T],
        current: T,
        label: impl Fn(T) -> &'static str,
        apply: impl Fn(&mut Connection, T) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let labels = all.iter().map(|v| SharedString::from(label(*v))).collect();
        let cur = all.iter().position(|x| *x == current).unwrap_or(0);
        self.dropdown(id, labels, cur, Rc::new(move |this, i| {
            if let Some(f) = &mut this.form {
                apply(&mut f.draft, all[i]);
            }
        }), cx)
    }

    /// An on/off setting, shown as a Yes/No drop-down.
    pub(crate) fn check(&self, id: &'static str, text: &'static str, on: bool, apply: impl Fn(&mut Connection, bool) + 'static, cx: &mut Context<Self>) -> AnyElement {
        let dd = self.dropdown(id, vec!["Yes".into(), "No".into()], if on { 0 } else { 1 }, Rc::new(move |this, i| {
            if let Some(f) = &mut this.form {
                apply(&mut f.draft, i == 0);
            }
        }), cx);
        div().flex().items_center().gap(px(12.0)).min_h(px(40.0)).child(div().w(px(190.0)).flex_none().child(label_text(text))).child(div().flex_1().min_w_0().child(dd)).into_any_element()
    }

    // ----- chrome -----------------------------------------------------------

    fn title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match (self.active_conn(), self.st.active.is_some()) {
            (Some(c), true) => format!("{} — {} — Remote Desktop", c.display_name(), c.host),
            _ => "SeeMyMac — Remote Desktop".to_string(),
        };
        let win_btn = |id: &'static str, glyph: &'static str, danger: bool| {
            div()
                .id(id)
                .size(px(24.0))
                .mr(px(6.0))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .text_color(rgb(MUTED))
                .hover(move |s| if danger { s.bg(rgb(0xC42B1C)).text_color(rgb(0xFFFFFF)) } else { s.bg(rgb(SELECTED)).text_color(rgb(TEXT)) })
                // Keep the press from reaching the title bar, whose mouse-down starts a
                // window move: the compositor would take the pointer and the click
                // would never complete.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .debug_selector(move || id.to_string())
                .child(glyph)
        };
        div()
            .id("titlebar")
            .debug_selector(|| "titlebar".into())
            .rounded_t(px(self.corner_radius))
            .h(px(TITLE_H))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .bg(rgb(BG_TITLE))
            .border_b_1()
            .border_color(rgb(DIVIDER))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, ev: &MouseDownEvent, window, _| {
                if ev.click_count >= 2 {
                    window.zoom_window();
                } else {
                    this.window_drags += 1;
                    window.start_window_move();
                }
            }))
            .child(div().w(px(78.0)).flex_none().flex().items_center().pl(px(10.0)).child(match app_icon() {
                Some(i) => img(i).size(px(20.0)).object_fit(ObjectFit::Contain).into_any_element(),
                None => div().into_any_element(),
            }))
            .child(div().flex_1().truncate().text_center().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).text_size(px(13.0)).child(title))
            .child(win_btn("win-min", "–", false).on_click(cx.listener(|_, _, window, _| window.minimize_window())))
            .child(win_btn("win-max", "▢", false).on_click(cx.listener(|_, _, window, _| window.zoom_window())))
            .child(win_btn("win-close", "✕", true).on_click(cx.listener(|this, _, _, cx| this.quit(cx))))
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.st.active.clone();
        let mut tabs = div().flex().items_center().h_full().min_w_0().overflow_hidden();
        for (i, id) in self.st.open.iter().enumerate() {
            let (name, color) = match (self.store.get(id), self.sessions.get(id)) {
                (Some(c), Some(s)) => (c.display_name().to_string(), status_color(&s.status)),
                _ => (id.clone(), MUTED),
            };
            let is_active = active.as_deref() == Some(id.as_str());
            let sid = id.clone();
            let cid = id.clone();
            tabs = tabs.child(
                div()
                    .id(("tab", i))
                    .h_full()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .border_b_2()
                    .border_color(rgb(if is_active { ACCENT_DARK } else { BG_CHROME }))
                    .bg(rgb(if is_active { SELECTED } else { BG_CHROME }))
                    .hover(|s| s.bg(rgb(SELECTED)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.st.switch_to(&sid);
                        window.focus(&this.focus);
                        cx.notify();
                    }))
                    .child(dot(color))
                    .child(div().max_w(px(160.0)).truncate().child(name))
                    .child(
                        div()
                            .id(("tabx", i))
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .text_color(rgb(MUTED))
                            .hover(|s| s.bg(rgb(BORDER)).text_color(rgb(TEXT)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_session(&cid, window, cx);
                            }))
                            .child("×"),
                    ),
            );
        }

        let monitors = self.active_session().map_or(1, |s| s.monitors);
        let cur_mon = active.as_deref().map_or(Mon::One, |a| self.st.monitor(a));
        let mon_sel = (monitors > 1).then(|| {
            let seg = |id: &'static str, text: &'static str, m: Mon, cx: &mut Context<Self>| {
                let sel = cur_mon == m;
                div()
                    .id(id)
                    .h(px(24.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .rounded(px(4.0))
                    .bg(rgb(if sel { ACCENT } else { BG_CHROME }))
                    .text_color(rgb(if sel { 0xFFFFFF } else { TEXT }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let a = this.st.active.clone();
                        this.set_monitor(&a, m);
                        cx.notify();
                    }))
                    .child(text)
            };
            div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .p(px(2.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(rgb(BORDER))
                .child(seg("mon1", "1", Mon::One, cx))
                .child(seg("mon2", "2", Mon::Two, cx))
                .child(seg("monall", "Both", Mon::All, cx))
        });

        let res = self.active_session().filter(|s| s.size.0 > 0).map_or(String::new(), |s| format!("{} × {} · {}", s.size.0, s.size.1, s.scaling.short()));

        div()
            .id("toolbar")
            .h(px(TOOLBAR_H))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(4.0))
            .bg(rgb(BG_CHROME))
            .border_b_1()
            .border_color(rgb(DIVIDER))
            .child(tool_btn("btn-connections", "").child(svg().path("ui/menu.svg").size(px(15.3)).text_color(rgb(TEXT))).on_click(cx.listener(|this, _, window, cx| this.toggle_menu(window, cx))))
            .child(tabs)
            .child(tool_btn("btn-new", "+").on_click(cx.listener(|this, _, window, cx| this.new_connection_dialog(window, cx))))
            .child(div().flex_1())
            .children(mon_sel)
            .child(div().px(px(8.0)).text_color(rgb(MUTED)).flex_none().child(res))
            .child(tool_btn("btn-switch", "").child(svg().path("ui/switch.svg").size(px(16.0)).text_color(rgb(TEXT))).on_click(cx.listener(|this, _, window, cx| {
                this.st.toggle_view(View::Switcher);
                window.focus(&this.focus);
                cx.notify();
            })))
            .when(self.active_session().map_or(false, |s| s.status == Status::Connected && s.virtual_pending.is_some()), |d| {
                d.child(tool_btn("btn-virtual", "Use virtual display").on_click(cx.listener(|this, _, _, cx| this.use_virtual_display(cx))))
            })
            .child(tool_btn("btn-cad", "").child(svg().path("ui/keys.svg").size(px(16.0)).text_color(rgb(TEXT))).child(div().ml(px(4.0)).text_color(rgb(MUTED)).child("▾")).on_click(cx.listener(|this, _, _, cx| {
                this.pick = if this.pick == Some("tb-keys") { None } else { Some("tb-keys") };
                cx.notify();
            })))
            .child(tool_btn("btn-scale", "").child(svg().path("ui/scale.svg").size(px(16.0)).text_color(rgb(TEXT))).child(div().ml(px(6.0)).child(self.active_session().map_or(Scaling::Fit, |s| s.scaling).short())).child(div().ml(px(4.0)).text_color(rgb(MUTED)).child("▾")).on_click(cx.listener(|this, _, _, cx| {
                this.pick = if this.pick == Some("tb-scale") { None } else { Some("tb-scale") };
                cx.notify();
            })))
            .child(
                tool_btn("btn-pin", "")
                    .child(svg().path(if self.st.pinned { "ui/pin.svg" } else { "ui/pin-off.svg" }).size(px(16.0)).text_color(rgb(if self.st.pinned { ACCENT_DARK } else { MUTED })))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.st.toggle_pin();
                        cx.notify();
                    })),
            )
            .child(
                tool_btn("btn-full", "")
                    .child(svg().path(if self.st.full { "ui/fullscreen-exit.svg" } else { "ui/fullscreen.svg" }).size(px(16.0)).text_color(rgb(TEXT)))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_full(window, cx))),
            )
    }

    /// Picker lists opened from the toolbar (scaling and special key combinations).
    fn toolbar_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (id, labels, current, on_pick): (&'static str, Vec<SharedString>, Option<usize>, Rc<dyn Fn(&mut App, usize)>) = if self.pick == Some("tb-keys") {
            ("tb-keys", KEY_COMBOS.iter().map(|c| SharedString::from(c.label)).collect(), None, Rc::new(|this, i| this.send_key_combo(i)))
        } else {
            let cur = self.active_session().map_or(Scaling::Fit, |s| s.scaling);
            ("tb-scale", Scaling::ALL.iter().map(|s| SharedString::from(s.label())).collect(), Scaling::ALL.iter().position(|s| *s == cur), Rc::new(|this, i| this.set_scaling(Scaling::ALL[i])))
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .child(div().id("picker-backdrop").absolute().top_0().left_0().right_0().bottom_0().on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.pick = None;
                cx.notify();
            })))
            .child(Self::popup_list(id, &labels, current, on_pick, cx).absolute().top(px(2.0)).right(px(8.0)).w(px(220.0)))
    }

    /// The progress or result of a file drop, at the bottom of the screen.
    fn file_note(n: &crate::app::FileNote, cx: &mut Context<Self>) -> impl IntoElement {
        let reveal = n.reveal.clone();
        let fetch = n.fetch.clone();
        div().absolute().bottom(px(16.0)).left_0().right_0().flex().justify_center().child(
            div()
                .id("file-note")
                .debug_selector(|| "file-note".into())
                .occlude()
                .flex()
                .items_center()
                .gap(px(12.0))
                .px(px(14.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .bg(rgba(0x0D1117E6))
                .border_1()
                .border_color(if n.error { rgb(0xE5534B) } else { rgb(BORDER) })
                .text_size(px(12.0))
                .child(n.text.clone())
                .children(reveal.map(|p| secondary_btn("file-note-show", "Show in folder").on_click(cx.listener(move |this, _, _, _| this.show_in_folder(&p)))))
                .children((!fetch.is_empty()).then(|| primary_btn("file-note-get", "Get").on_click(cx.listener(move |this, _, _, cx| this.fetch_mac_files(fetch.clone(), cx))))),
        )
    }

    /// Shown while an Apple HP session that wants a virtual display is still on the
    /// Mac's own screen (where a locked Mac shows its login window).
    fn virtual_banner(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().absolute().bottom(px(16.0)).left_0().right_0().flex().justify_center().child(
            div()
                .id("virtual-banner")
                .debug_selector(|| "virtual-banner".into())
                .occlude()
                .flex()
                .items_center()
                .gap(px(12.0))
                .px(px(14.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .bg(rgba(0x0D1117E6))
                .border_1()
                .border_color(rgb(BORDER))
                .child(div().text_size(px(12.0)).child(if self.active_session().map_or(false, |s| s.big_screen) {
                    "This Mac's 5K screen is slow to show here, so input lags. Log in if it is locked, then switch."
                } else {
                    "Showing the Mac's own screen. Virtual displays may start with an empty desktop."
                }))
                .child(primary_btn("banner-virtual", "Use virtual display").on_click(cx.listener(|this, _, _, cx| this.use_virtual_display(cx))))
                .child(secondary_btn("banner-stay", "Stay on this screen").on_click(cx.listener(|this, _, _, cx| {
                    let id = this.st.active.clone();
                    if let Some(s) = id.and_then(|id| this.sessions.get_mut(&id)) {
                        s.virtual_pending = None;
                    }
                    cx.notify();
                }))),
        )
    }

    fn pull_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (name, color, status) = match (self.active_conn(), self.active_session()) {
            (Some(c), Some(s)) => (c.display_name().to_string(), status_color(&s.status), s.status.label()),
            _ => ("No sessions".to_string(), MUTED, ""),
        };
        let more = self.st.open.len().saturating_sub(1);
        let mon = self.st.active.as_deref().map(|a| self.st.monitor(a));
        let monitors = self.active_session().map_or(1, |s| s.monitors);
        div().absolute().top_0().left_0().right_0().flex().justify_center().child(
            div()
                .id("pulltab")
                .h(px(PULL_H))
                .px(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded_b(px(6.0))
                .bg(rgb(BG_CHROME))
                .border_1()
                .border_t_0()
                .border_color(rgb(BORDER))
                .text_size(px(12.0))
                .cursor_pointer()
                .occlude()
                .hover(|s| s.bg(rgb(SELECTED)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.st.pinned = true;
                    cx.notify();
                }))
                .child(dot(color))
                .child(name)
                .child(div().text_color(rgb(MUTED)).child(status))
                .when(monitors > 1, |d| {
                    d.child(div().text_color(rgb(MUTED)).child(match mon {
                        Some(Mon::Two) => "Monitor 2",
                        Some(Mon::All) => "Both monitors",
                        _ => "Monitor 1",
                    }))
                })
                .when(more > 0, |d| d.child(div().text_color(rgb(MUTED)).child(format!("+{more} more")))),
        )
    }

    // ----- remote area ------------------------------------------------------

    fn remote_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let area = self.area.get();
        let (aw, ah) = (f32::from(area.size.width), f32::from(area.size.height));
        let Some(id) = self.st.active.clone() else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(14.0))
                .child(div().text_size(px(16.0)).font_weight(FontWeight::MEDIUM).child("No active sessions"))
                .child(div().text_color(rgb(MUTED)).child("Open a saved connection or create a new one."))
                .child(primary_btn("open-connections", "Open connections").on_click(cx.listener(|this, _, window, cx| this.toggle_menu(window, cx))))
                .into_any_element();
        };
        let mon = self.st.monitor(&id);
        let Some(s) = self.sessions.get(&id) else { return div().into_any_element() };
        let host = self.store.get(&id).map(|c| c.host.clone()).unwrap_or_default();

        match &s.status {
            Status::Connected if !s.tiles.is_empty() => {
                if mon == Mon::All && s.tiles.len() > 1 {
                    // Monitors side by side at 16:9 on a dark letterbox, each labelled (§5).
                    let n = s.tiles.len() as f32;
                    let tile_w = ((aw - 16.0 - 8.0 * (n - 1.0)) / n).max(1.0);
                    let mut row = div().size_full().flex().items_center().justify_center().gap(px(8.0)).p(px(8.0)).bg(rgb(BG_TITLE));
                    for (i, t) in s.tiles.iter().enumerate() {
                        let zoom = if t.monitor == 0 { Mon::One } else { Mon::Two };
                        let zid = id.clone();
                        row = row.child(
                            div()
                                .id(("tile", i))
                                .flex_1()
                                .min_w_0()
                                .relative()
                                .cursor_pointer()
                                .child(div().w_full().h(px(tile_w * 9.0 / 16.0)).bg(rgb(0x000000)).child(img(t.image.clone()).size_full().object_fit(ObjectFit::Contain)))
                                .child(
                                    div()
                                        .absolute()
                                        .left(px(8.0))
                                        .bottom(px(8.0))
                                        .px(px(8.0))
                                        .py(px(2.0))
                                        .rounded(px(4.0))
                                        .bg(rgba(0x000000B3))
                                        .text_size(px(12.0))
                                        .child(format!("Monitor {} · {} × {}", t.monitor + 1, t.resolution.0, t.resolution.1)),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.st.zoom_into(&zid, zoom);
                                    cx.notify();
                                })),
                        );
                    }
                    row.into_any_element()
                } else {
                    let t = &s.tiles[0];
                    let r = geometry::image_rect(aw, ah, t.size.0 as f32, t.size.1 as f32, s.scaling);
                    div()
                        .size_full()
                        .relative()
                        .overflow_hidden()
                        .bg(rgb(0x000000))
                        .child(img(t.image.clone()).absolute().left(px(r.x)).top(px(r.y)).w(px(r.w)).h(px(r.h)).object_fit(ObjectFit::Fill))
                        .into_any_element()
                }
            }
            Status::Failed(msg) => {
                let rid = id.clone();
                let cid = id.clone();
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.0))
                    .p(px(24.0))
                    .child(div().flex().items_center().gap(px(8.0)).child(dot(DANGER)).child(div().font_weight(FontWeight::MEDIUM).child(format!("Could not connect to {host}"))))
                    .child(div().max_w(px(520.0)).text_color(rgb(MUTED)).child(msg.clone()))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(primary_btn("retry", "Try again").on_click(cx.listener(move |this, _, window, cx| {
                                // Re-prompt for a login when the connection asks for one.
                                let needs_login = this.sessions.get(&rid).map_or(false, |s| matches!(&s.status, Status::Failed(m) if m.contains("password") || m.contains("authentication")));
                                if needs_login {
                                    this.show_password(&rid, None, window, cx);
                                } else {
                                    this.retry_session(&rid, cx);
                                }
                            })))
                            .child(secondary_btn("close-failed", "Close").on_click(cx.listener(move |this, _, window, cx| this.close_session(&cid, window, cx)))),
                    )
                    .into_any_element()
            }
            other => {
                let reconnecting = *other == Status::Reconnecting;
                let title = if matches!(other, Status::Connected) {
                    format!("Waiting for the first screen image from {host}…")
                } else if reconnecting {
                    format!("Reconnecting to {host}…")
                } else {
                    format!("Connecting to {host}…")
                };
                let secs = s.started.elapsed().as_secs();
                let cid = id.clone();
                let hint = if secs >= 8 { "Taking a while? Check the address, that Screen Sharing is on, and whether the Mac is asking you to approve the connection." } else { "" };
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.0))
                    .p(px(24.0))
                    .child(div().flex().items_center().gap(px(8.0)).child(dot(WARN)).child(div().font_weight(FontWeight::MEDIUM).child(title)).child(div().text_color(rgb(MUTED)).child(format!("{secs}s"))))
                    .child(div().max_w(px(560.0)).text_color(rgb(MUTED)).child(s.stage.clone()))
                    .child(div().max_w(px(560.0)).text_color(rgb(MUTED)).child(hint))
                    .child(secondary_btn("cancel-connect", "Cancel").on_click(cx.listener(move |this, _, window, cx| this.close_session(&cid, window, cx))))
                    .into_any_element()
            }
        }
    }

    // ----- overlays ---------------------------------------------------------

    fn menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let q = self.quick.clone();
        let matches = |c: &&Connection| {
            let ql = q.trim().to_lowercase();
            ql.is_empty() || c.name.to_lowercase().contains(&ql) || c.host.to_lowercase().contains(&ql)
        };
        let all: Vec<Connection> = self.store.connections.iter().filter(matches).cloned().collect();

        let section = |t: String| div().px(px(14.0)).pt(px(10.0)).pb(px(4.0)).text_size(px(11.0)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(MUTED)).child(t);
        let row_base = |id: ElementId| div().id(id).min_h(px(40.0)).px(px(14.0)).flex().items_center().gap(px(10.0)).cursor_pointer().hover(|s| s.bg(rgb(SELECTED)));

        let mut list = div().flex().flex_col();

        // Active (N)
        if !self.st.open.is_empty() {
            list = list.child(section(format!("ACTIVE ({})", self.st.open.len())));
            for (i, id) in self.st.open.iter().enumerate() {
                let (Some(c), Some(s)) = (self.store.get(id), self.sessions.get(id)) else { continue };
                let current = self.st.active.as_deref() == Some(id.as_str());
                let sid = id.clone();
                list = list.child(
                    row_base(ElementId::from(("act", i)))
                        .bg(rgb(if current { SELECTED } else { SURFACE }))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.st.switch_to(&sid);
                            window.focus(&this.focus);
                            cx.notify();
                        }))
                        .child(dot(status_color(&s.status)))
                        .child(div().flex_1().truncate().child(c.display_name().to_string()))
                        .child(div().text_size(px(12.0)).text_color(rgb(MUTED)).child(s.status.label()))
                        .when(i < 9, |d| d.child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child(format!("Ctrl+Alt+{}", i + 1)))),
                );
            }
        }

        let conn_row = |c: &Connection, idx: usize, prefix: &'static str, cx: &mut Context<Self>| {
            let open = self.st.is_open(&c.id);
            let (oid, eid) = (c.id.clone(), c.id.clone());
            row_base(ElementId::from((prefix, idx)))
                .on_click(cx.listener(move |this, _, window, cx| this.open_connection(&oid, window, cx)))
                .child(svg().path("ui/connections.svg").size(px(18.0)).text_color(rgb(TEXT)))
                .child(div().flex_1().min_w_0().truncate().child(c.display_name().to_string()))
                .when(open, |d| d.child(div().px(px(6.0)).py(px(1.0)).rounded(px(4.0)).bg(rgb(SELECTED)).border_1().border_color(rgb(BORDER)).text_size(px(11.0)).child("Open")))
                .child(
                    div()
                        .id(ElementId::from(("con-edit", idx)))
                        .size(px(28.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .text_color(rgb(MUTED))
                        .hover(|s| s.bg(rgb(BORDER)).text_color(rgb(TEXT)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.edit_connection_dialog(&eid, window, cx);
                        }))
                        .child("✎"),
                )
        };

        if !all.is_empty() {
            list = list.child(section("CONNECTIONS".into()));
            for (i, c) in all.iter().enumerate() {
                list = list.child(conn_row(c, i, "con", cx));
            }
        }
        if all.is_empty() && self.st.open.is_empty() {
            list = list.child(div().px(px(14.0)).py(px(12.0)).text_color(rgb(MUTED)).child(if q.trim().is_empty() { "No saved connections yet.".to_string() } else { format!("No match. Press Enter to connect to “{}”.", q.trim()) }));
        }

        let action = |id: &'static str, text: &'static str, hint: &'static str| {
            row_base(ElementId::from(id)).child(svg().path(match id { "act-new" => "ui/new.svg", "act-manage" => "ui/connections.svg", "act-getfile" => "ui/download.svg", _ => "ui/settings.svg" }).size(px(18.0)).text_color(rgb(TEXT))).child(div().flex_1().child(text)).child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child(hint))
        };

        div()
            .id("menu")
            .debug_selector(|| "menu".into())
            .absolute()
            .top(px(0.0))
            .left(px(8.0))
            .w(px(380.0))
            .max_h(px((f32::from(self.area.get().size.height) - 16.0).max(200.0)))
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(BORDER))
            .shadow_lg()
            .occlude()
            .child(div().p(px(10.0)).child(self.text_field("quick", Field::Quick, &self.quick, "Quick connect: search or enter a host name", false, cx)))
            .child(div().id("menu-list").flex_1().min_h_0().overflow_y_scroll().child(list))
            .child(div().h(px(1.0)).bg(rgb(DIVIDER)).mt(px(6.0)))
            .child(action("act-new", "New connection…", "Ctrl+N").on_click(cx.listener(|this, _, window, cx| this.new_connection_dialog(window, cx))))
            .child(action("act-manage", "Manage all connections…", "Ctrl+M").on_click(cx.listener(|this, _, window, cx| this.open_manage(window, cx))))
            .child(action("act-getfile", "Get file from Mac…", "").on_click(cx.listener(|this, _, window, cx| this.open_get_file(window, cx))))
            .child(action("act-settings", "Settings", "Ctrl+,").on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))))
            .child(div().h(px(1.0)).bg(rgb(DIVIDER)))
            .child(row_base(ElementId::from("act-exit"))
                .debug_selector(|| "act-exit".into())
                .child(svg().path("ui/exit.svg").size(px(18.0)).text_color(rgb(TEXT)))
                .child("Exit")
                .on_click(cx.listener(|this, _, _, cx| this.quit(cx))))
    }

    fn switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut cards = div().flex().flex_wrap().justify_center().gap(px(16.0)).p(px(24.0));
        for (i, id) in self.st.open.iter().enumerate() {
            let (Some(c), Some(s)) = (self.store.get(id), self.sessions.get(id)) else { continue };
            let sid = id.clone();
            let current = self.st.active.as_deref() == Some(id.as_str());
            cards = cards.child(
                div()
                    .id(("card", i))
                    .w(px(244.0))
                    .rounded(px(10.0))
                    .bg(rgb(SURFACE))
                    .border_2()
                    .border_color(rgb(if current { ACCENT_DARK } else { BORDER }))
                    .cursor_pointer()
                    .overflow_hidden()
                    .hover(|st| st.border_color(rgb(ACCENT_DARK)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.st.switch_to(&sid);
                        window.focus(&this.focus);
                        cx.notify();
                    }))
                    .child(div().w_full().h(px(137.0)).bg(rgb(0x000000)).flex().items_center().justify_center().child(match s.tiles.first() {
                        Some(t) => img(t.image.clone()).size_full().object_fit(ObjectFit::Contain).into_any_element(),
                        None => div().text_color(rgb(MUTED)).child(s.status.label()).into_any_element(),
                    }))
                    .child(
                        div()
                            .p(px(10.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(dot(status_color(&s.status)))
                            .child(div().flex_1().truncate().child(c.display_name().to_string()))
                            .child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child(if i < 9 { format!("Ctrl+Alt+{}", i + 1) } else { String::new() })),
                    ),
            );
        }
        cards = cards.child(
            div()
                .id("card-new")
                .w(px(244.0))
                .h(px(137.0 + 40.0))
                .rounded(px(10.0))
                .border_2()
                .border_color(rgb(BORDER))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .text_color(rgb(MUTED))
                .hover(|s| s.border_color(rgb(ACCENT_DARK)).text_color(rgb(TEXT)))
                .on_click(cx.listener(|this, _, window, cx| this.new_connection_dialog(window, cx)))
                .child("+  New connection"),
        );
        div()
            .id("switcher")
            .debug_selector(|| "switcher".into())
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .bg(rgba(0x010409E6))
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx)))
            .child(div().id("switcher-inner").on_click(|_, _, cx| cx.stop_propagation()).child(cards))
    }

    fn dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(form) = &self.form else { return div().into_any_element() };
        let d = &form.draft;
        let tab_btn = |id: &'static str, text: &'static str, t: DialogTab, cx: &mut Context<Self>| {
            let sel = form.tab == t;
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
                    this.pick = None;
                    if let Some(f) = &mut this.form {
                        f.tab = t;
                    }
                    cx.notify();
                }))
                .child(text)
        };
        let row = |label: &str, control: AnyElement| div().flex().items_center().gap(px(12.0)).min_h(px(40.0)).child(div().w(px(190.0)).flex_none().child(label_text(label))).child(div().flex_1().min_w_0().child(control));

        let body: AnyElement = match form.tab {
            DialogTab::General => {
                let seg = |p: Proto, cx: &mut Context<Self>| {
                    let sel = d.proto == p;
                    div()
                        .id(SharedString::from(format!("proto-{}", p.label())))
                        .h(px(26.0))
                        .px(px(14.0))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .rounded(px(4.0))
                        .bg(rgb(if sel { ACCENT } else { BG_CHROME }))
                        .text_color(rgb(if sel { 0xFFFFFF } else { TEXT }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_protocol(p);
                            cx.notify();
                        }))
                        .child(p.label())
                };
                let mut segs = div().flex().gap(px(2.0)).p(px(2.0)).rounded(px(6.0)).border_1().border_color(rgb(BORDER)).w_auto();
                for p in Proto::ALL {
                    segs = segs.child(seg(p, cx));
                }
                div()
                    .flex()
                    .flex_col()
                    .child(row("Display name", self.text_field("f-name", Field::Name, &d.name, "My Mac", false, cx).into_any_element()))
                    .child(row("Protocol", div().flex().child(segs).into_any_element()))
                    .child(row("Host name or IP address", self.text_field("f-host", Field::Host, &d.host, "mac.local or 192.168.1.20", false, cx).into_any_element()))
                    .child(row("Port", self.text_field("f-port", Field::Port, &d.port, &d.proto.default_port().to_string(), false, cx).into_any_element()))
                    .child(row("User name", self.text_field("f-user", Field::User, &d.user, "Optional for VNC", false, cx).into_any_element()))
                    .when(d.proto == Proto::Rdp, |col| {
                        col.child(row("RD Gateway (optional)", self.text_field("f-gateway", Field::Gateway, &d.gateway, "gateway.example.com or host:port", false, cx).into_any_element()))
                            .child(row("RemoteApp program (optional)", self.text_field("f-remoteapp", Field::RemoteApp, &d.remote_app, "||notepad or C:\\Windows\\notepad.exe", false, cx).into_any_element()))
                    })
                    .child(row("Credentials", self.cycle("f-cred", &CredentialMode::ALL, d.credentials, CredentialMode::label, |c, v| c.credentials = v, cx).into_any_element()))
                    .when(d.credentials == CredentialMode::Vault, |col| {
                        col.child(row("Password", self.text_field("f-pass", Field::Password, &form.password, if form.has_saved_password { "Saved. Type to replace it" } else { "Stored in the system keyring" }, true, cx).into_any_element()))
                    })
                    .child(self.check("c-reconnect", "Reconnect automatically if dropped", d.reconnect, |c, v| c.reconnect = v, cx))
                    .into_any_element()
            }
            DialogTab::Display => div()
                .flex()
                .flex_col()
                .child(row("Remote resolution", self.cycle("d-res", &Resolution::ALL, d.resolution, Resolution::label, |c, v| c.resolution = v, cx).into_any_element()))
                .child(row("Scaling", self.cycle("d-scale", &Scaling::ALL, d.scaling, Scaling::label, |c, v| c.scaling = v, cx).into_any_element()))
                .child(row("Color depth", self.cycle("d-depth", &ColorDepth::ALL, d.depth, ColorDepth::label, |c, v| c.depth = v, cx).into_any_element()))
                .child(row("Start in", self.cycle("d-start", &StartIn::ALL, d.start_in, StartIn::label, |c, v| c.start_in = v, cx).into_any_element()))
                .child(row("When the remote PC has more monitors than this PC", self.cycle("d-multi", &MultiMonitor::ALL, d.multi_monitor, MultiMonitor::label, |c, v| c.multi_monitor = v, cx).into_any_element()))
                .when(d.proto == Proto::Apple, |col| {
                    col.child(self.check("d-virtual", "Offer a virtual display after connecting", d.virtual_display, |c, v| c.virtual_display = v, cx))
                        .child(div().text_size(px(12.0)).text_color(rgb(MUTED)).child("Keep this off to view the Mac's existing windows. A virtual display may open an empty desktop."))
                        .when(d.virtual_display, |col| {
                            col.child(row("Virtual displays", self.cycle("d-virtual-count", &[1u8, 2], d.virtual_displays.clamp(1, 2), |n| if n == 2 { "2 (shown as monitors 1 and 2)" } else { "1" }, |c, v| c.virtual_displays = v, cx).into_any_element()))
                        })
                })
                .child(self.check("d-span", "Span all local monitors", d.span_monitors, |c, v| c.span_monitors = v, cx))
                .into_any_element(),
            DialogTab::Local => div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(self.check("l-clip", "Clipboard", d.local.clipboard, |c, v| c.local.clipboard = v, cx))
                .child(self.check("l-drives", "Local drives", d.local.drives, |c, v| c.local.drives = v, cx))
                .child(self.check("l-print", "Printers", d.local.printers, |c, v| c.local.printers = v, cx))
                .child(self.check("l-audio", "Audio playback", d.local.audio, |c, v| c.local.audio = v, cx))
                .child(self.check("l-keys", "Windows key combinations", d.local.key_combinations, |c, v| c.local.key_combinations = v, cx))
                .child(self.check("l-smart", "Smart cards", d.local.smart_cards, |c, v| c.local.smart_cards = v, cx))
                .into_any_element(),
        };

        div()
            .id("dialog-scrim")
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
                    .id("dialog")
                    .debug_selector(|| "dialog".into())
                    .w(px(640.0))
                    .max_w_full()
                    .rounded(px(12.0))
                    .bg(rgb(SURFACE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(div().px(px(20.0)).pt(px(16.0)).text_size(px(16.0)).font_weight(FontWeight::SEMIBOLD).child(if form.is_new { "New connection" } else { "Edit connection" }))
                    .child(div().flex().px(px(12.0)).mt(px(8.0)).border_b_1().border_color(rgb(DIVIDER)).child(tab_btn("t-gen", "General", DialogTab::General, cx)).child(tab_btn("t-disp", "Display", DialogTab::Display, cx)).child(tab_btn("t-local", "Local resources", DialogTab::Local, cx)))
                    .child(div().px(px(20.0)).py(px(12.0)).min_h(px(400.0)).child(body))
                    .children(form.error.clone().map(|e| div().px(px(20.0)).pb(px(8.0)).text_color(rgb(DANGER)).child(e)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(20.0))
                            .py(px(14.0))
                            .border_t_1()
                            .border_color(rgb(DIVIDER))
                            .when(!form.is_new, |d| d.child(div().id("btn-delete").h(px(32.0)).px(px(14.0)).flex().items_center().rounded(px(6.0)).cursor_pointer().text_color(rgb(DANGER)).hover(|s| s.bg(rgb(SELECTED))).on_click(cx.listener(|this, _, window, cx| this.delete_from_form(window, cx))).child("Delete")))
                            .child(div().flex_1())
                            .child(secondary_btn("btn-cancel", "Cancel").on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx))))
                            .child(secondary_btn("btn-save", "Save").on_click(cx.listener(|this, _, window, cx| this.save_form(false, window, cx))))
                            .child(primary_btn("btn-save-connect", "Save & connect").on_click(cx.listener(|this, _, window, cx| this.save_form(true, window, cx)))),
                    ),
            )
            .into_any_element()
    }

    fn password_prompt(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(p) = &self.pw else { return div().into_any_element() };
        let host = self.store.get(&p.conn_id).map(|c| c.display_name().to_string()).unwrap_or_default();
        div()
            .id("pw-scrim")
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
            .child(
                div()
                    .id("pw")
                    .debug_selector(|| "pw".into())
                    .w(px(420.0))
                    .rounded(px(12.0))
                    .bg(rgb(SURFACE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .p(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(div().text_size(px(16.0)).font_weight(FontWeight::SEMIBOLD).child(format!("Sign in to {host}")))
                    .children(p.error.clone().map(|e| div().text_color(rgb(DANGER)).child(e)))
                    .child(label_text("User name (Apple Screen Sharing needs one)"))
                    .child(self.text_field("pw-user", Field::PwUser, &p.user, "User name", false, cx))
                    .child(label_text("Password"))
                    .child(self.text_field("pw-pass", Field::PwPass, &p.pass, "Password", true, cx))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .mt(px(6.0))
                            .child(secondary_btn("pw-cancel", "Cancel").on_click(cx.listener(|this, _, window, cx| this.cancel_password(window, cx))))
                            .child(primary_btn("pw-ok", "Connect").on_click(cx.listener(|this, _, window, cx| this.submit_password(window, cx)))),
                    ),
            )
            .into_any_element()
    }

    fn get_file_prompt(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((path, problem)) = &self.get_file else { return div().into_any_element() };
        div()
            .id("gf-scrim")
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
            .child(
                div()
                    .id("gf")
                    .debug_selector(|| "gf".into())
                    .w(px(480.0))
                    .rounded(px(12.0))
                    .bg(rgb(SURFACE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg()
                    .p(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(div().text_size(px(16.0)).font_weight(FontWeight::SEMIBOLD).child("Get file from Mac"))
                    .child(label_text("Path of the file or folder on the Mac"))
                    .child(div().text_size(px(12.0)).text_color(rgb(MUTED)).child("Type a file name (looked for on the Mac's Desktop, Documents and Downloads) or a full path. Or copy a file in Finder on the Mac with Cmd+C and click Get."))
                    .child(self.text_field("gf-path", Field::GetPath, path, "neopass.txt or /Users/name/Documents/report.pdf", false, cx))
                    .children(problem.clone().map(|e| div().text_color(rgb(DANGER)).text_size(px(12.0)).child(e)))
                    .child(div().text_size(px(12.0)).text_color(rgb(MUTED)).child(format!("It is saved in {}. Change that folder in Settings.", self.settings.download_path().display())))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .mt(px(6.0))
                            .child(secondary_btn("gf-cancel", "Cancel").on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx))))
                            .child(primary_btn("gf-ok", "Get file").on_click(cx.listener(|this, _, window, cx| this.submit_get_file(window, cx)))),
                    ),
            )
            .into_any_element()
    }

    fn resize_handles(&self) -> impl IntoElement {
        let edge = |id: &'static str, e: ResizeEdge, cursor: CursorStyle| {
            div().id(id).absolute().cursor(cursor).on_mouse_down(MouseButton::Left, move |_, window, _| window.start_window_resize(e))
        };
        let t = 4.0;
        div()
            .child(edge("rz-t", ResizeEdge::Top, CursorStyle::ResizeUpDown).top_0().left(px(t)).right(px(t)).h(px(t)))
            .child(edge("rz-b", ResizeEdge::Bottom, CursorStyle::ResizeUpDown).bottom_0().left(px(t)).right(px(t)).h(px(t)))
            .child(edge("rz-l", ResizeEdge::Left, CursorStyle::ResizeLeftRight).left_0().top(px(t)).bottom(px(t)).w(px(t)))
            .child(edge("rz-r", ResizeEdge::Right, CursorStyle::ResizeLeftRight).right_0().top(px(t)).bottom(px(t)).w(px(t)))
            .child(edge("rz-tl", ResizeEdge::TopLeft, CursorStyle::ResizeUpLeftDownRight).top_0().left_0().size(px(t)))
            .child(edge("rz-tr", ResizeEdge::TopRight, CursorStyle::ResizeUpRightDownLeft).top_0().right_0().size(px(t)))
            .child(edge("rz-bl", ResizeEdge::BottomLeft, CursorStyle::ResizeUpRightDownLeft).bottom_0().left_0().size(px(t)))
            .child(edge("rz-br", ResizeEdge::BottomRight, CursorStyle::ResizeUpLeftDownRight).bottom_0().right_0().size(px(t)))
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Remote area = window minus the chrome that is currently visible.
        let vp = window.viewport_size();
        let top = if self.st.full || NATIVE_FRAME { 0.0 } else { TITLE_H } + if self.st.pinned { TOOLBAR_H } else { 0.0 };
        self.area.set(Bounds::new(point(px(0.0), px(top)), size(vp.width, (vp.height - px(top)).max(px(0.0)))));

        let corner = if NATIVE_FRAME || self.st.full || window.is_maximized() { 0.0 } else { 10.0 };
        self.corner_radius = corner;
        let view = self.st.view;
        let capture = self.settings.capture_system_keys && view == View::Idle && self.active_session().map_or(false, |s| s.status == Status::Connected);
        if let Some(s) = &mut self.shortcuts {
            s.set(capture);
        }
        let content = self.remote_content(cx);

        let area = div()
            .id("remote")
            .rounded_b(px(corner))
            .flex_1()
            .min_h_0()
            .relative()
            .bg(rgb(0x000000))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, ev: &MouseDownEvent, window, _| {
                window.focus(&this.focus);
                if let Some(s) = &mut this.shortcuts {
                    s.rearm();
                }
                this.pointer(ev.position, Some(1), Some(true));
            }))
            .on_mouse_down(MouseButton::Middle, cx.listener(|this, ev: &MouseDownEvent, _, _| this.pointer(ev.position, Some(2), Some(true))))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, ev: &MouseDownEvent, _, _| this.pointer(ev.position, Some(4), Some(true))))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, ev: &MouseUpEvent, _, _| this.pointer(ev.position, Some(1), Some(false))))
            .on_mouse_up(MouseButton::Middle, cx.listener(|this, ev: &MouseUpEvent, _, _| this.pointer(ev.position, Some(2), Some(false))))
            .on_mouse_up(MouseButton::Right, cx.listener(|this, ev: &MouseUpEvent, _, _| this.pointer(ev.position, Some(4), Some(false))))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, _| this.pointer(ev.position, None, None)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.drop_files(paths.paths().to_vec(), cx)))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, _| {
                let dy = match ev.delta {
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                    ScrollDelta::Lines(l) => l.y,
                };
                this.scroll(ev.position, dy);
            }))
            .child(content)
            .when(!self.st.pinned, |d| d.child(self.pull_tab(cx)))
            .when_some(self.active_session().and_then(|s| s.file_note.as_ref()), |d, n| d.child(Self::file_note(n, cx)))
            .when(view == View::Idle && self.active_session().map_or(false, |s| s.status == Status::Connected && s.virtual_pending.is_some()), |d| d.child(self.virtual_banner(cx)))
            .when(view == View::Menu, |d| {
                d.child(
                    // Backdrop: a click outside the menu closes it (§5).
                    div().id("menu-backdrop").absolute().top_0().left_0().right_0().bottom_0().on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_overlay(window, cx))),
                )
                .child(self.menu(cx))
            })
            .when(matches!(self.pick, Some("tb-keys") | Some("tb-scale")) && view == View::Idle, |d| d.child(self.toolbar_picker(cx)))
            .when(view == View::Switcher, |d| d.child(self.switcher(cx)))
            .when(view == View::Dialog, |d| d.child(self.dialog(cx)))
            .when(view == View::Manage, |d| d.child(self.manage_panel(cx)))
            .when(view == View::Settings, |d| d.child(self.settings_panel(cx)))
            .when(view == View::Password, |d| d.child(self.password_prompt(cx)))
            .when(view == View::GetFile, |d| d.child(self.get_file_prompt(cx)));

        div()
            .id("root")
            .key_context("SeeMyMac")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .rounded(px(corner))
            .overflow_hidden()
            .bg(rgb(BG_CHROME))
            .text_color(rgb(TEXT))
            .text_size(px(13.0))
            .when_some(self.font.clone(), |d, f| d.font_family(f))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| this.on_key_down(ev, window, cx)))
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, window, cx| this.on_key_up(ev, window, cx)))
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, window, cx| this.on_modifiers(ev, window, cx)))
            .when(!self.st.full && !NATIVE_FRAME, |d| d.child(self.title_bar(cx)))
            .when(self.st.pinned, |d| d.child(self.toolbar(cx)))
            .child(area)
            .when(!NATIVE_FRAME && !self.st.full && !window.is_maximized(), |d| d.child(self.resize_handles()))
    }
}

/// The application icon, decoded once from the embedded PNG (as premultiplied BGRA).
fn app_icon() -> Option<std::sync::Arc<RenderImage>> {
    static ICON: std::sync::OnceLock<Option<std::sync::Arc<RenderImage>>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| {
        let mut buf = image::load_from_memory_with_format(include_bytes!("../assets/icons/seemac.png"), image::ImageFormat::Png).ok()?.into_rgba8();
        for p in buf.pixels_mut() {
            let a = p[3] as u32;
            let (r, g, b) = (p[0] as u32 * a / 255, p[1] as u32 * a / 255, p[2] as u32 * a / 255);
            p.0 = [b as u8, g as u8, r as u8, a as u8];
        }
        Some(std::sync::Arc::new(RenderImage::new(smallvec::SmallVec::from_elem(image::Frame::new(buf), 1))))
    })
    .clone()
}

/// Embedded icons (`assets/ui`).
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Ok(match path {
            "ui/menu.svg" => Some(include_bytes!("../assets/ui/menu.svg").as_slice().into()),
            "ui/new.svg" => Some(include_bytes!("../assets/ui/new.svg").as_slice().into()),
            "ui/connections.svg" => Some(include_bytes!("../assets/ui/connections.svg").as_slice().into()),
            "ui/download.svg" => Some(include_bytes!("../assets/ui/download.svg").as_slice().into()),
            "ui/settings.svg" => Some(include_bytes!("../assets/ui/settings.svg").as_slice().into()),
            "ui/exit.svg" => Some(include_bytes!("../assets/ui/exit.svg").as_slice().into()),
            "ui/keys.svg" => Some(include_bytes!("../assets/ui/keys.svg").as_slice().into()),
            "ui/scale.svg" => Some(include_bytes!("../assets/ui/scale.svg").as_slice().into()),
            "ui/switch.svg" => Some(include_bytes!("../assets/ui/switch.svg").as_slice().into()),
            "ui/pin.svg" => Some(include_bytes!("../assets/ui/pin.svg").as_slice().into()),
            "ui/pin-off.svg" => Some(include_bytes!("../assets/ui/pin-off.svg").as_slice().into()),
            "ui/fullscreen.svg" => Some(include_bytes!("../assets/ui/fullscreen.svg").as_slice().into()),
            "ui/fullscreen-exit.svg" => Some(include_bytes!("../assets/ui/fullscreen-exit.svg").as_slice().into()),
            _ => None,
        })
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(vec![])
    }
}
