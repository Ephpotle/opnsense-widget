#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod config;
mod dpi;
mod host;
mod model;
mod notify;
mod pcnet;
mod poller;
mod probe;
mod theme;
mod tray;
mod wazuh;
mod ui;

use eframe::egui;
use model::{Shared, State};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromWindow};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IsWindowVisible, SW_HIDE, SW_SHOW, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindow,
};

/// HWND as an integer so it can cross into the tray-event closure.
#[derive(Clone, Copy)]
struct Hwnd(isize);

impl Hwnd {
    fn ptr(self) -> windows_sys::Win32::Foundation::HWND {
        self.0 as _
    }
    /// Hide to the tray. eframe stops running `update()` while the main window is hidden, and
    /// the history window is drawn from it, so that window would freeze (its close button
    /// dead): close it along with the widget.
    fn hide(self) {
        ui::close_history_window();
        unsafe { ShowWindow(self.ptr(), SW_HIDE) };
    }
    /// A persisted position can point past a monitor that is gone or was rearranged.
    fn ensure_on_screen(self) {
        unsafe {
            if MonitorFromWindow(self.ptr(), MONITOR_DEFAULTTONULL).is_null() {
                SetWindowPos(self.ptr(), std::ptr::null_mut(), 100, 100, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
            }
        }
    }
    /// Visible, not minimized, not being dragged or resized with the mouse, and with the DPI
    /// switch for its monitor done (a move lands a frame before the DPI change does) and
    /// already seen by egui as `ppp`.
    fn settled(self, ppp: f32) -> bool {
        use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
        use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
        unsafe {
            if dpi::in_move_size(self.ptr()) || IsIconic(self.ptr()) != 0 || IsWindowVisible(self.ptr()) == 0 {
                return false;
            }
            let (mut dx, mut dy) = (0u32, 0u32);
            let mon = MonitorFromWindow(self.ptr(), MONITOR_DEFAULTTONULL);
            if mon.is_null() || GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) != 0 {
                return false;
            }
            let dpi = GetDpiForWindow(self.ptr());
            dpi == dx && (dpi as f32 / 96.0 - ppp).abs() < 0.01
        }
    }
    /// Pull the window fully onto its monitor's work area, shrinking it if it is larger.
    /// With mixed DPI (150 % 4K next to 100 % 1080p) a window taller than the small screen
    /// keeps flipping back to the big one mid-drag. Works in physical pixels through Win32:
    /// a resize in egui points raced the DPI change and was applied at the old scale.
    fn fit_to_work_area(self) {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
        use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO, MONITOR_DEFAULTTONEAREST};
        use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let zero = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let (mut outer, mut visible) = (zero, zero);
        unsafe {
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            let mon = MonitorFromWindow(self.ptr(), MONITOR_DEFAULTTONEAREST);
            if GetWindowRect(self.ptr(), &mut outer) == 0 || GetMonitorInfoW(mon, &mut mi) == 0 {
                return;
            }
            // The visible frame excludes Windows 11's invisible resize borders.
            if DwmGetWindowAttribute(
                self.ptr(),
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut visible as *mut RECT).cast(),
                std::mem::size_of::<RECT>() as u32,
            ) != 0
            {
                visible = outer;
            }
            let work = mi.rcWork;
            let (vw, vh) = (visible.right - visible.left, visible.bottom - visible.top);
            let (ww, wh) = (work.right - work.left, work.bottom - work.top);
            let (w, h) = (vw.min(ww), vh.min(wh));
            let x = visible.left.clamp(work.left, work.right - w);
            let y = visible.top.clamp(work.top, work.bottom - h);
            if (x, y, w, h) == (visible.left, visible.top, vw, vh) {
                return;
            }
            // Convert back to GetWindowRect coordinates by re-adding the invisible borders.
            let (bl, bt) = (visible.left - outer.left, visible.top - outer.top);
            let (br, bb) = (outer.right - visible.right, outer.bottom - visible.bottom);
            SetWindowPos(self.ptr(), std::ptr::null_mut(), x - bl, y - bt, w + bl + br, h + bt + bb, SWP_NOZORDER);
        }
    }
    fn toggle(self) {
        unsafe {
            if IsWindowVisible(self.ptr()) != 0 {
                self.hide();
            } else {
                ShowWindow(self.ptr(), SW_SHOW);
                SetForegroundWindow(self.ptr());
            }
        }
    }
}

struct App {
    state: Shared,
    hwnd: Option<Hwnd>,
    /// Scale factor and when it last changed: sizes are inconsistent for a frame or two
    /// after a DPI switch, so fitting waits until the scale has settled.
    scale: (f32, std::time::Instant),
    /// Saved placement, applied again once the DPI has settled: the window is created on the
    /// primary monitor, and the jump to a monitor with another scale rescales the size.
    boot: Option<Geometry>,
    /// Theme choice last applied to egui, and dark-ness last applied to the title bar.
    applied_choice: Option<theme::Choice>,
    applied_dark: Option<bool>,
    /// Last normal (not minimized/hidden) window position and size, physical pixels.
    geometry: Option<Geometry>,
}

/// Saved window placement. eframe's own `persist_window` also saves a minimized window's
/// -32000,-32000 / 0x0 placement, which then restores as an unusable sliver.
#[derive(Clone, Copy, PartialEq)]
struct Geometry {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Geometry {
    fn parse(s: &str) -> Option<Self> {
        let v: Vec<i32> = s.split(',').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
        let &[x, y, w, h] = v.as_slice() else { return None };
        (w >= 200 && h >= 200 && x > -30000 && y > -30000).then_some(Self { x, y, w, h })
    }
    fn to_string(self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.w, self.h)
    }
    fn current(hwnd: Hwnd) -> Option<Self> {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsIconic};
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe {
            if IsIconic(hwnd.ptr()) != 0 || IsWindowVisible(hwnd.ptr()) == 0 || GetWindowRect(hwnd.ptr(), &mut r) == 0 {
                return None;
            }
        }
        Self::parse(&format!("{},{},{},{}", r.left, r.top, r.right - r.left, r.bottom - r.top))
    }
    /// Move first, then size: a move onto a monitor with another scale makes winit rescale
    /// the window (WM_DPICHANGED, handled inside SetWindowPos), which would distort a size
    /// set in the same call.
    fn apply(self, hwnd: Hwnd) {
        use windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOMOVE;
        unsafe {
            SetWindowPos(hwnd.ptr(), std::ptr::null_mut(), self.x, self.y, 0, 0, SWP_NOZORDER | SWP_NOSIZE);
            SetWindowPos(hwnd.ptr(), std::ptr::null_mut(), 0, 0, self.w, self.h, SWP_NOZORDER | SWP_NOMOVE);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Closing the window sends it to the tray; Quit lives in the tray menu.
        if ctx.input(|i| i.viewport().close_requested()) {
            if let Some(h) = self.hwnd {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                h.hide();
            }
        }
        let choice = theme::choice();
        if self.applied_choice != Some(choice) {
            ctx.set_theme(choice.preference());
            // Tell winit too: it owns the window's dark-mode attribute and re-applies its own
            // idea of the theme (the Windows app mode) on DPI and settings changes, which
            // flipped the title bar back to light behind the dark panels.
            ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(choice.system_theme()));
            self.applied_choice = Some(choice);
        }
        // Resolves "follow Windows" too, and tracks live Windows theme switches. Re-assert
        // after a monitor move as well, in case anything reset the attribute.
        let dark = ctx.style().visuals.dark_mode;
        let ppp = ctx.input(|i| i.viewport().native_pixels_per_point.unwrap_or(1.0));
        if self.applied_dark != Some(dark) || ppp != self.scale.0 {
            if let Some(h) = self.hwnd {
                theme::dark_title_bar(h.0, dark);
            }
            self.applied_dark = Some(dark);
        }
        if ppp != self.scale.0 {
            self.scale = (ppp, std::time::Instant::now());
        } else if self.scale.1.elapsed() > Duration::from_millis(300) {
            if let Some(h) = self.hwnd.filter(|h| h.settled(ppp)) {
                if let Some(g) = self.boot.take() {
                    g.apply(h);
                }
                h.fit_to_work_area();
            }
        }
        if let Some(g) = self.hwnd.and_then(Geometry::current) {
            self.geometry = Some(g);
        }
        let s = self.state.lock().unwrap();
        let frame = egui::Frame::central_panel(&ctx.style()).inner_margin(10);
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| ui::draw(ui, &s));
        ui::history_window(ctx, &s);
        drop(s);
        // Keep the activity dot fresh even without poller wakeups.
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string("theme", theme::choice().name().into());
        if let Some(g) = self.geometry {
            storage.set_string("geometry", g.to_string());
        }
        // eframe loads its "window" key even with `persist_window` off; blank the stale one.
        storage.set_string("window", String::new());
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

fn main() -> eframe::Result {
    let state: Shared = Arc::new(Mutex::new(State::default()));
    let cfg = config::Config::load();
    let on_top = cfg.as_ref().map(|c| c.always_on_top).unwrap_or(true);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Router")
        .with_inner_size([360.0, 640.0])
        .with_min_inner_size([320.0, 360.0])
        .with_icon(theme::app_icon());
    if on_top {
        viewport = viewport.with_always_on_top();
    }
    let options = eframe::NativeOptions { viewport, persist_window: false, ..Default::default() };

    eframe::run_native(
        "router-widget",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            ui::init_history_window();
            // Saved tray choice wins over the config file's default.
            let saved = cc.storage.and_then(|s| s.get_string("theme")).and_then(|s| theme::Choice::parse(&s));
            let configured = cfg.as_ref().ok().and_then(|c| theme::Choice::parse(&c.theme));
            theme::set_choice(saved.or(configured).unwrap_or(theme::Choice::System));
            match cfg {
                Ok(cfg) => {
                    pcnet::spawn(state.clone(), cc.egui_ctx.clone(), cfg.toasts, cfg.toast_app_id.clone());
                    if let Some(w) = cfg.wazuh.clone() {
                        wazuh::spawn(w, state.clone(), cc.egui_ctx.clone(), cfg.toasts, cfg.toast_app_id.clone());
                    }
                    if let Some(h) = cfg.host.clone() {
                        host::spawn(h, state.clone(), cc.egui_ctx.clone(), cfg.toasts, cfg.toast_app_id.clone());
                    }
                    poller::spawn(cfg, state.clone(), cc.egui_ctx.clone())
                }
                Err(e) => state.lock().unwrap().config_error = Some(e),
            }
            let hwnd = cc.window_handle().ok().and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(w) => Some(Hwnd(w.hwnd.get())),
                _ => None,
            });
            // ROUTER_WIDGET_GEOMETRY="x,y,w,h" (physical px) places test instances out of the way.
            let saved = std::env::var("ROUTER_WIDGET_GEOMETRY")
                .ok()
                .or_else(|| cc.storage.and_then(|s| s.get_string("geometry")))
                .and_then(|s| Geometry::parse(&s));
            if let Some(h) = hwnd {
                dpi::install(h.ptr());
                if let Some(g) = saved {
                    g.apply(h);
                }
                h.ensure_on_screen();
                tray::spawn(state.clone(), move || h.toggle());
            }
            Ok(Box::new(App {
                state,
                hwnd,
                scale: (0.0, std::time::Instant::now()),
                boot: saved,
                applied_choice: None,
                applied_dark: None,
                geometry: None,
            }))
        }),
    )
}
