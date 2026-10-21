//! Fluent 2 (Windows 11) styling for egui: Segoe UI Variable + Segoe Fluent Icons, the
//! Windows neutral/status colour tokens, the user's accent colour, 4/8 px corner radii and a
//! Mica backdrop when available. Falls back to egui's fonts and solid colours elsewhere.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

static CHOICE: AtomicU8 = AtomicU8::new(Choice::System as u8);

/// Light/dark selection; `System` follows the Windows app theme live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    System = 0,
    Light = 1,
    Dark = 2,
}

impl Choice {
    pub fn name(self) -> &'static str {
        match self {
            Choice::System => "system",
            Choice::Light => "light",
            Choice::Dark => "dark",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "system" => Some(Choice::System),
            "light" => Some(Choice::Light),
            "dark" => Some(Choice::Dark),
            _ => None,
        }
    }

    pub fn system_theme(self) -> egui::SystemTheme {
        match self {
            Choice::System => egui::SystemTheme::SystemDefault,
            Choice::Light => egui::SystemTheme::Light,
            Choice::Dark => egui::SystemTheme::Dark,
        }
    }

    pub fn preference(self) -> egui::ThemePreference {
        match self {
            Choice::System => egui::ThemePreference::System,
            Choice::Light => egui::ThemePreference::Light,
            Choice::Dark => egui::ThemePreference::Dark,
        }
    }
}

pub fn set_choice(c: Choice) {
    CHOICE.store(c as u8, Ordering::Relaxed);
}

pub fn choice() -> Choice {
    match CHOICE.load(Ordering::Relaxed) {
        1 => Choice::Light,
        2 => Choice::Dark,
        _ => Choice::System,
    }
}

/// App icon for every window (title bar, taskbar, Alt-Tab): 64x64 RGBA drawn by
/// `assets/mkicon.py`. Without it the main window showed egui's "e" and the history window
/// a blank icon.
pub fn app_icon() -> std::sync::Arc<egui::IconData> {
    static ICON: OnceLock<std::sync::Arc<egui::IconData>> = OnceLock::new();
    ICON.get_or_init(|| {
        std::sync::Arc::new(egui::IconData { rgba: include_bytes!("../assets/icon_64.rgba").to_vec(), width: 64, height: 64 })
    })
    .clone()
}

/// Dark title bar, the attribute Windows' own apps use.
pub fn dark_title_bar(hwnd: isize, dark: bool) {
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    let value: i32 = dark.into();
    unsafe {
        DwmSetWindowAttribute(
            hwnd as _,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&value as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
    }
}
/// Layout scale chosen per frame by `ui::draw` from the window size (1.0 = design size).
static SCALE: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32

pub fn set_scale(k: f32) {
    SCALE.store(k.to_bits(), Ordering::Relaxed);
}

/// Current layout scale; multiply any size by this.
pub fn k() -> f32 {
    f32::from_bits(SCALE.load(Ordering::Relaxed))
}


/// Segoe Fluent Icons code points used by the UI.
pub mod icon {
    pub const DOWN: &str = "\u{E74B}";
    pub const UP: &str = "\u{E74A}";
    pub const GLOBE: &str = "\u{E774}";
    pub const NETWORK: &str = "\u{E968}";
    pub const PC: &str = "\u{E7F4}";
    pub const DEVICES: &str = "\u{E772}";
    pub const HISTORY: &str = "\u{E81C}";
    pub const SHIELD: &str = "\u{EA18}";
    pub const SUN: &str = "\u{E706}";
    pub const MOON: &str = "\u{E708}";
    pub const OK: &str = "\u{EC61}";
    pub const INFO: &str = "\u{E946}";
    pub const WARNING: &str = "\u{E7BA}";
    pub const ERROR: &str = "\u{EB90}";
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub card: Color32,
    pub card_stroke: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub text3: Color32,
    pub accent: Color32,
    pub success: Color32,
    pub caution: Color32,
    pub critical: Color32,
    pub success_bg: Color32,
    pub caution_bg: Color32,
    pub critical_bg: Color32,
    pub up: Color32,
    pub track: Color32,
}

impl Palette {
    pub fn of(ctx: &egui::Context) -> Self {
        Self::new(ctx.style().visuals.dark_mode)
    }

    pub fn new(dark: bool) -> Self {
        // Read once: the palette is rebuilt every frame.
        static ACCENT: OnceLock<Color32> = OnceLock::new();
        let accent = *ACCENT.get_or_init(system_accent);
        // Solid Fluent surfaces in both modes. Mica tinted the backdrop with the wallpaper's hue,
        // which clashed with the neutral cards (purple-grey in dark, washed out in light).
        if dark {
            Self {
                bg: Color32::from_rgb(0x20, 0x20, 0x20),
                card: Color32::from_rgb(0x2b, 0x2b, 0x2b),
                card_stroke: Color32::from_rgb(0x1d, 0x1d, 0x1d),
                text: Color32::WHITE,
                text2: Color32::from_rgb(0xc8, 0xc8, 0xc8),
                text3: Color32::from_rgb(0x9a, 0x9a, 0x9a),
                accent: mix(accent, Color32::WHITE, 0.45),
                // Upload: the accent's own hue, muted, so the two series read as one family.
                up: mix(mix(accent, Color32::WHITE, 0.45), Color32::from_rgb(0x9a, 0x9a, 0x9a), 0.5),
                // A cooler green than Fluent's yellow-green, to sit with the blue accent; a
                // softer amber than its pure yellow.
                success: Color32::from_rgb(0x5c, 0xc8, 0x8a),
                caution: Color32::from_rgb(0xf2, 0xc6, 0x4b),
                critical: Color32::from_rgb(0xff, 0x99, 0xa4),
                // Status fills are tints of their own colour over the card, so they stay in
                // the same neutral family (Fluent's stock #393D1B read as olive on near-black).
                success_bg: mix(Color32::from_rgb(0x2b, 0x2b, 0x2b), Color32::from_rgb(0x5c, 0xc8, 0x8a), 0.16),
                caution_bg: mix(Color32::from_rgb(0x2b, 0x2b, 0x2b), Color32::from_rgb(0xf2, 0xc6, 0x4b), 0.14),
                critical_bg: mix(Color32::from_rgb(0x2b, 0x2b, 0x2b), Color32::from_rgb(0xff, 0x99, 0xa4), 0.14),
                track: Color32::from_rgb(0x38, 0x38, 0x38),
            }
        } else {
            Self {
                bg: Color32::from_rgb(0xf3, 0xf3, 0xf3),
                card: Color32::from_rgb(0xfb, 0xfb, 0xfb),
                card_stroke: Color32::from_black_alpha(15),
                text: Color32::from_rgb(0x1a, 0x1a, 0x1a),
                text2: Color32::from_rgb(0x5f, 0x5f, 0x5f),
                text3: Color32::from_rgb(0x8a, 0x8a, 0x8a),
                accent: mix(accent, Color32::BLACK, 0.12),
                up: mix(mix(accent, Color32::BLACK, 0.12), Color32::from_rgb(0x8a, 0x8a, 0x8a), 0.5),
                success: Color32::from_rgb(0x10, 0x7c, 0x41),
                caution: Color32::from_rgb(0x9d, 0x5d, 0x00),
                critical: Color32::from_rgb(0xc4, 0x2b, 0x1c),
                success_bg: mix(Color32::from_rgb(0xfb, 0xfb, 0xfb), Color32::from_rgb(0x10, 0x7c, 0x41), 0.10),
                caution_bg: mix(Color32::from_rgb(0xfb, 0xfb, 0xfb), Color32::from_rgb(0x9d, 0x5d, 0x00), 0.10),
                critical_bg: mix(Color32::from_rgb(0xfb, 0xfb, 0xfb), Color32::from_rgb(0xc4, 0x2b, 0x1c), 0.09),
                track: Color32::from_black_alpha(10),
            }
        }
    }
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// The Windows accent colour (HKCU\...\DWM\AccentColor, stored as 0xAABBGGRR).
pub fn system_accent() -> Color32 {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (key, val) = (wide(r"Software\Microsoft\Windows\DWM"), wide("AccentColor"));
    let mut data: u32 = 0;
    let mut len = 4u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut len,
        )
    } == 0;
    if ok {
        Color32::from_rgb(data as u8, (data >> 8) as u8, (data >> 16) as u8)
    } else {
        Color32::from_rgb(0x00, 0x78, 0xd4)
    }
}

pub const SEMIBOLD: &str = "semibold";
pub const ICONS: &str = "icons";

/// Fluent type ramp.
/// Fluent type ramp, multiplied by the layout scale.
pub fn caption() -> FontId {
    FontId::proportional(12.0 * k())
}
pub fn body() -> FontId {
    FontId::proportional(14.0 * k())
}
pub fn body_strong() -> FontId {
    FontId::new(14.0 * k(), FontFamily::Name(SEMIBOLD.into()))
}
pub fn subtitle() -> FontId {
    FontId::new(18.0 * k(), FontFamily::Name(SEMIBOLD.into()))
}
pub fn title() -> FontId {
    FontId::new(22.0 * k(), FontFamily::Name(SEMIBOLD.into()))
}
pub fn icon_font(size: f32) -> FontId {
    FontId::new(size * k(), FontFamily::Name(ICONS.into()))
}

fn load(fonts: &mut FontDefinitions, name: &str, files: &[&str]) -> bool {
    for f in files {
        if let Ok(bytes) = std::fs::read(format!(r"C:\Windows\Fonts\{f}")) {
            fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
            return true;
        }
    }
    false
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let fallback = fonts.families[&FontFamily::Proportional].clone();
    let mut prop = fallback.clone();
    if load(&mut fonts, "segoe", &["SegUIVar.ttf", "segoeui.ttf"]) {
        prop.insert(0, "segoe".into());
    }
    let mut semi = prop.clone();
    if load(&mut fonts, SEMIBOLD, &["seguisb.ttf"]) {
        semi.insert(0, SEMIBOLD.into());
    }
    let mut icons = fallback.clone();
    if load(&mut fonts, ICONS, &["SegoeIcons.ttf", "segmdl2.ttf"]) {
        icons.insert(0, ICONS.into());
        // Glyphs like ⏷ ⚠ that Segoe UI lacks still resolve through the icon font.
        prop.push(ICONS.into());
        semi.push(ICONS.into());
    }
    if load(&mut fonts, "mono", &["CascadiaMono.ttf", "consola.ttf"]) {
        fonts.families.get_mut(&FontFamily::Monospace).unwrap().insert(0, "mono".into());
    }
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), semi);
    fonts.families.insert(FontFamily::Name(ICONS.into()), icons);
    ctx.set_fonts(fonts);

    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        let p = Palette::new(theme == egui::Theme::Dark);
        ctx.style_mut_of(theme, |style| {
            style.text_styles.insert(egui::TextStyle::Body, body());
            style.text_styles.insert(egui::TextStyle::Button, body());
            style.text_styles.insert(egui::TextStyle::Small, caption());
            style.text_styles.insert(egui::TextStyle::Heading, subtitle());
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.scroll = egui::style::ScrollStyle::floating();
            let v = &mut style.visuals;
            v.panel_fill = p.bg;
            v.window_fill = p.bg;
            v.extreme_bg_color = p.track;
            v.override_text_color = Some(p.text);
            v.hyperlink_color = p.accent;
            v.selection.bg_fill = p.accent;
            v.selection.stroke = Stroke::new(1.0, p.accent);
            v.window_corner_radius = 8.into();
            v.menu_corner_radius = 8.into();
            for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
                w.corner_radius = 4.into();
            }
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.card_stroke);
        });
    }
}

/// A Fluent card: layer fill, hairline stroke, 8 px radius.
pub fn card(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.card_stroke))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric((12.0 * k()) as i8, (9.0 * k()) as i8))
}
