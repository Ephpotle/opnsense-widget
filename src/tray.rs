use crate::model::{Level, Shared};
use std::time::Duration;
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};
use crate::theme::{self, Choice};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub struct Tray {
    icon: TrayIcon,
    /// Theme radio items: (choice, item). Checks are synced from the tray thread because
    /// menu items can't be moved into the (Send) event handler.
    themes: Vec<(Choice, CheckMenuItem)>,
    shown_choice: Option<Choice>,
    level: Option<Level>,
    tooltip: String,
}

pub fn level_rgb(level: Level) -> [u8; 3] {
    match level {
        Level::Ok | Level::Info => [0x3f, 0xb9, 0x50],
        Level::Warn => [0xe3, 0xa0, 0x08],
        Level::Alert => [0xe5, 0x48, 0x4d],
    }
}

fn dot(level: Level) -> Icon {
    const N: u32 = 32;
    let [r, g, b] = level_rgb(level);
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            // 1 px anti-aliased edge
            let a = (13.5 - d).clamp(0.0, 1.0);
            rgba.extend_from_slice(&[r, g, b, (a * 255.0) as u8]);
        }
    }
    Icon::from_rgba(rgba, N, N).expect("valid icon")
}

impl Tray {
    /// `toggle` runs on the event-loop thread when the icon is left-clicked or "Show" is picked.
    pub fn new(toggle: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        let menu = Menu::new();
        let show = MenuItem::new("Show / hide", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let themes: Vec<(Choice, CheckMenuItem)> = [
            (Choice::System, "Follow Windows"),
            (Choice::Light, "Light"),
            (Choice::Dark, "Dark"),
        ]
        .into_iter()
        .map(|(c, label)| (c, CheckMenuItem::new(label, true, theme::choice() == c, None)))
        .collect();
        let theme_menu = Submenu::new("Theme", true);
        for (_, item) in &themes {
            theme_menu.append(item).map_err(|e| e.to_string())?;
        }
        menu.append(&show).map_err(|e| e.to_string())?;
        menu.append(&theme_menu).map_err(|e| e.to_string())?;
        menu.append(&PredefinedMenuItem::separator()).map_err(|e| e.to_string())?;
        menu.append(&quit).map_err(|e| e.to_string())?;
        let icon = TrayIconBuilder::new()
            .with_icon(dot(Level::Ok))
            .with_tooltip("Router")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| e.to_string())?;

        let toggle = std::sync::Arc::new(toggle);
        let t = toggle.clone();
        TrayIconEvent::set_event_handler(Some(move |e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                t();
            }
        }));
        let (show_id, quit_id) = (show.id().clone(), quit.id().clone());
        let theme_ids: Vec<_> = themes.iter().map(|(c, item)| (*c, item.id().clone())).collect();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == quit_id {
                std::process::exit(0);
            } else if e.id == show_id {
                toggle();
            } else if let Some((c, _)) = theme_ids.iter().find(|(_, id)| *id == e.id) {
                theme::set_choice(*c);
            }
        }));
        Ok(Self { icon, themes, shown_choice: None, level: None, tooltip: String::new() })
    }

    pub fn update(&mut self, level: Level, tooltip: &str) {
        let current = theme::choice();
        if self.shown_choice != Some(current) {
            for (c, item) in &self.themes {
                item.set_checked(*c == current);
            }
            self.shown_choice = Some(current);
        }
        if self.level != Some(level) {
            let _ = self.icon.set_icon(Some(dot(level)));
            self.level = Some(level);
        }
        if self.tooltip != tooltip {
            let _ = self.icon.set_tooltip(Some(tooltip));
            self.tooltip = tooltip.to_string();
        }
    }
}

/// Runs the tray on its own thread with its own message pump. eframe stops calling
/// `update()` while the window is hidden, so the tray cannot be driven from the UI loop.
pub fn spawn(state: Shared, toggle: impl Fn() + Send + Sync + 'static) {
    std::thread::spawn(move || {
        let mut tray = match Tray::new(toggle) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("tray unavailable: {e}");
                return;
            }
        };
        loop {
            unsafe {
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            {
                let s = state.lock().unwrap();
                tray.update(s.health(), &crate::ui::tooltip(&s));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}
