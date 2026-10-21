use crate::model::{Event, Level, Source, State, HISTORY_LEN};
use crate::theme::{self, icon, k, Palette};
use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Stroke, Vec2};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Width the single-column layout is designed for (at scale 1.0).
const DESIGN_WIDTH: f32 = 360.0;
/// Above this width the layout splits into two columns.
const TWO_COLUMN_WIDTH: f32 = 640.0;
const MIN_SCALE: f32 = 0.55;
const MAX_SCALE: f32 = 1.6;
/// Activity always keeps room for about this many rows before the layout shrinks.
const MIN_ACTIVITY_ROWS: f32 = 3.0;

pub fn fmt_bps(bps: f64) -> String {
    match bps {
        b if b >= 1e9 => format!("{:.2} Gb/s", b / 1e9),
        b if b >= 1e6 => format!("{:.1} Mb/s", b / 1e6),
        b if b >= 1e3 => format!("{:.0} kb/s", b / 1e3),
        b => format!("{b:.0} b/s"),
    }
}

fn fmt_gb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1u64 << 30) as f64)
}

/// "5 days, 18:53:54" -> "5d 18h"
pub fn short_uptime(s: &str) -> String {
    let (days, rest) = match s.split_once(',') {
        Some((d, r)) => (d.split_whitespace().next().unwrap_or("0"), r.trim()),
        None => ("0", s.trim()),
    };
    let mut hm = rest.split(':');
    let h = hm.next().unwrap_or("0");
    let m = hm.next().unwrap_or("0");
    if days != "0" { format!("{days}d {h}h") } else { format!("{h}h {m}m") }
}

fn short_duration(d: chrono::Duration) -> String {
    let m = d.num_minutes().max(0);
    match m {
        m if m >= 1440 => format!("{}d {}h", m / 1440, (m % 1440) / 60),
        m if m >= 60 => format!("{}h {}m", m / 60, m % 60),
        m => format!("{m}m"),
    }
}

/// One-line summary used for the tray tooltip.
pub fn tooltip(s: &State) -> String {
    let health = s.health();
    let head = s.problems.iter().find(|p| p.0 == health).map(|p| p.1.clone());
    match head {
        Some(p) => format!("Router: {p}"),
        None => format!("Router OK · ↓{} ↑{}", fmt_bps(s.wan_rx_bps), fmt_bps(s.wan_tx_bps)),
    }
}

fn level_fg(p: &Palette, level: Level) -> Color32 {
    match level {
        Level::Ok => p.success,
        Level::Info => p.accent,
        Level::Warn => p.caution,
        Level::Alert => p.critical,
    }
}

fn level_bg(p: &Palette, level: Level) -> Color32 {
    match level {
        Level::Ok | Level::Info => p.success_bg,
        Level::Warn => p.caution_bg,
        Level::Alert => p.critical_bg,
    }
}

fn level_icon(level: Level) -> &'static str {
    match level {
        Level::Ok => icon::OK,
        Level::Info => icon::INFO,
        Level::Warn => icon::WARNING,
        Level::Alert => icon::ERROR,
    }
}

fn level_word(level: Level) -> &'static str {
    match level {
        Level::Ok | Level::Info => "Healthy",
        Level::Warn => "Warning",
        Level::Alert => "Alert",
    }
}

fn glyph(ui: &mut egui::Ui, g: &str, size: f32, color: Color32) {
    ui.label(RichText::new(g).font(theme::icon_font(size)).color(color));
}

fn caption(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).font(theme::caption()).color(color)
}

fn card_header(ui: &mut egui::Ui, p: &Palette, g: &str, title: &str) {
    ui.horizontal(|ui| {
        glyph(ui, g, 13.0, p.text2);
        ui.label(RichText::new(title).font(theme::body_strong()));
    });
}

/// Full-width card (egui frames otherwise shrink to their content).
fn card(ui: &mut egui::Ui, p: &Palette, body: impl FnOnce(&mut egui::Ui)) {
    theme::card(p).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 3.0) * k();
        body(ui);
    });
}

/// Pick the layout scale: as large as the width allows, but small enough that everything
/// except the (self-scrolling) activity list fits the window height. Content height is
/// proportional to the scale, so last frame's measurement predicts the right value.
fn choose_scale(ui: &egui::Ui, wide: bool) -> f32 {
    let id = egui::Id::new("layout_scale");
    let avail = ui.available_size();
    let (k_last, needed_last): (f32, f32) = ui.ctx().data(|d| d.get_temp(id)).unwrap_or((1.0, 0.0));
    let design = if wide { 2.0 * DESIGN_WIDTH } else { DESIGN_WIDTH };
    let by_width = avail.x / design;
    let by_height = if needed_last > 0.0 { k_last * avail.y / needed_last } else { by_width };
    let target = by_width.min(by_height).clamp(MIN_SCALE, MAX_SCALE);
    // Hysteresis: shrink as soon as content overflows, but grow only with clear headroom,
    // so a layout whose height isn't exactly proportional can't oscillate.
    if target < k_last - 0.01 || target > k_last + 0.04 { target } else { k_last }
}

pub fn draw(ui: &mut egui::Ui, s: &State) {
    let p = Palette::of(ui.ctx());
    let wide = ui.available_width() >= TWO_COLUMN_WIDTH;
    let scale = choose_scale(ui, wide);
    theme::set_scale(scale);
    let top = ui.min_rect().top();
    let bottom = ui.max_rect().bottom();
    // Below the minimum scale the content can still overflow; scroll rather than clip.
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| layout(ui, &p, s, scale, wide, top, bottom));
}

fn layout(ui: &mut egui::Ui, p: &Palette, s: &State, scale: f32, wide: bool, top: f32, bottom: f32) {
    let p = *p;
    let gap = 6.0 * scale;
    ui.spacing_mut().item_spacing = egui::vec2(gap, gap);

    header(ui, &p, s);
    for (lvl, text) in s.problems.iter().filter(|x| x.0 >= Level::Warn) {
        info_bar(ui, &p, *lvl, text);
    }

    let min_activity = activity_row_height() * MIN_ACTIVITY_ROWS + 40.0 * scale;
    let needed = if wide {
        let cols_top = ui.cursor().top();
        let mut col_needed = [0.0f32; 2];
        ui.columns(2, |cols| {
            for c in cols.iter_mut() {
                c.spacing_mut().item_spacing = egui::vec2(gap, gap);
            }
            let y0 = cols[0].cursor().top();
            stats(&mut cols[0], &p, s);
            col_needed[0] = cols[0].cursor().top() - y0;
            let y1 = cols[1].cursor().top();
            card(&mut cols[1], &p, |ui| devices(ui, &p, s));
            col_needed[1] = cols[1].cursor().top() - y1 + min_activity;
            let rest = (bottom - cols[1].cursor().top()).max(min_activity);
            card(&mut cols[1], &p, |ui| activity(ui, &p, s, rest));
        });
        (cols_top - top) + col_needed[0].max(col_needed[1])
    } else {
        stats(ui, &p, s);
        card(ui, &p, |ui| devices(ui, &p, s));
        let fixed = ui.cursor().top() - top;
        let rest = (bottom - ui.cursor().top()).max(min_activity);
        card(ui, &p, |ui| activity(ui, &p, s, rest));
        fixed + min_activity
    };
    ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new("layout_scale"), (scale, needed)));
}

/// Internet, router and host cards (the left column in the wide layout).
fn stats(ui: &mut egui::Ui, p: &Palette, s: &State) {
    card(ui, p, |ui| internet(ui, p, s));
    card(ui, p, |ui| router(ui, p, s));
    if s.host.is_some() {
        card(ui, p, |ui| host(ui, p, s));
    }
    if s.wazuh.is_some() {
        card(ui, p, |ui| security(ui, p, s));
    }
}

fn security(ui: &mut egui::Ui, p: &Palette, s: &State) {
    let Some(w) = &s.wazuh else { return };
    card_header(ui, p, icon::SHIELD, "Security (Wazuh)");
    let heard = match (w.connected, w.last_heard) {
        (false, _) => "no ssh".to_string(),
        (true, Some(t)) => {
            let m = (chrono::Local::now() - t).num_minutes();
            if m < 1 { "live".into() } else { format!("{} ago", short_duration(chrono::Local::now() - t)) }
        }
        _ => "—".into(),
    };
    tile_grid(
        ui,
        p,
        &[("Scan bursts/h", w.scans_last_hour.to_string()), ("IDS alerts 24h", w.ids_last_day.to_string()), ("SIEM", heard)],
    );
}

fn header(ui: &mut egui::Ui, p: &Palette, s: &State) {
    let health = s.health();
    let k = k();
    ui.horizontal(|ui| {
        // Status disc; it swells briefly after each successful poll as an activity tick.
        let fresh = s.last_ok.is_some_and(|t| t.elapsed().as_millis() < 400);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(32.0 * k), Sense::hover());
        let c = level_fg(p, health);
        ui.painter().circle_filled(rect.center(), if fresh { 16.0 } else { 15.0 } * k, level_bg(p, health));
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, level_icon(health), theme::icon_font(15.0), c);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let name = if s.hostname.is_empty() { "router" } else { &s.hostname };
            ui.label(RichText::new(name).font(theme::subtitle()));
            let mut sub = s.version.clone();
            if !s.uptime.is_empty() {
                sub = format!("{sub} · up {}", short_uptime(&s.uptime));
            }
            ui.add(egui::Label::new(caption(sub, p.text2)).truncate());
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(4.0 * k);
            egui::Frame::new()
                .fill(level_bg(p, health))
                .corner_radius(10)
                .inner_margin(egui::Margin::symmetric((9.0 * k) as i8, (2.0 * k) as i8))
                .show(ui, |ui| {
                    ui.label(caption(level_word(health), c));
                });
            ui.add_space(6.0 * k);
            theme_switch(ui, p);
        });
    });
}

/// Fluent-style segmented control: Light / Auto (follow Windows) / Dark. Shares state with
/// the tray's Theme menu; `App::update` applies it and `App::save` remembers it.
fn theme_switch(ui: &mut egui::Ui, p: &Palette) {
    let k = k();
    let current = theme::choice();
    // Built right-to-left, so list the segments in reverse visual order.
    let segments = [
        (theme::Choice::Dark, icon::MOON, "Dark"),
        (theme::Choice::System, "Auto", "Follow the Windows app theme"),
        (theme::Choice::Light, icon::SUN, "Light"),
    ];
    egui::Frame::new()
        .fill(p.track)
        .corner_radius(6)
        .inner_margin(egui::Margin::same((2.0 * k) as i8))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0 * k;
            for (choice, glyph_str, tip) in segments {
                let selected = current == choice;
                // Icons for light/dark; "Auto" has no unambiguous Fluent glyph, so it's a word.
                let is_text = choice == theme::Choice::System;
                let size = Vec2::new(if is_text { 38.0 } else { 24.0 } * k, 20.0 * k);
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                if selected {
                    ui.painter().rect_filled(rect, 4.0, p.card);
                    ui.painter().rect_stroke(rect, 4.0, Stroke::new(1.0, p.card_stroke), egui::StrokeKind::Inside);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 4.0, p.card_stroke.gamma_multiply(0.5));
                }
                let color = if selected { p.accent } else { p.text2 };
                let font = if is_text { theme::caption() } else { theme::icon_font(11.0) };
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph_str, font, color);
                if resp.on_hover_text(tip).clicked() {
                    theme::set_choice(choice);
                }
            }
        });
}

/// Fluent InfoBar: tinted strip with a severity glyph.
fn info_bar(ui: &mut egui::Ui, p: &Palette, level: Level, text: &str) {
    let k = k();
    egui::Frame::new()
        .fill(level_bg(p, level))
        .stroke(Stroke::new(1.0, p.card_stroke))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric((10.0 * k) as i8, (6.0 * k) as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                glyph(ui, level_icon(level), 14.0, level_fg(p, level));
                ui.add(egui::Label::new(RichText::new(text).font(theme::body_strong())).wrap());
            });
        });
}

fn internet(ui: &mut egui::Ui, p: &Palette, s: &State) {
    ui.horizontal(|ui| {
        glyph(ui, icon::GLOBE, 13.0, p.text2);
        ui.label(RichText::new("Internet").font(theme::body_strong()));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let net = match (s.internet_down, s.internet_rtt_ms) {
                (true, _) => "unreachable".to_string(),
                (_, Some(r)) => format!("{r:.0} ms · {:.0}% loss", s.internet_loss_pct),
                _ => "—".into(),
            };
            ui.label(caption(net, p.text2)).on_hover_text(format!("WAN {}", s.wan_ip));
        });
    });
    ui.horizontal(|ui| {
        glyph(ui, icon::DOWN, 14.0, p.accent);
        ui.label(RichText::new(fmt_bps(s.wan_rx_bps)).font(theme::title()));
        ui.add_space(8.0 * k());
        glyph(ui, icon::UP, 14.0, p.up);
        ui.label(RichText::new(fmt_bps(s.wan_tx_bps)).font(theme::title()).color(p.text2));
    });
    sparkline(ui, p, s);
    hops(ui, p, s);
    // This PC's side of the connection: UDP port exhaustion looks exactly like a network
    // outage from here, so show it next to the internet numbers.
    if let Some(pc) = &s.pc {
        let pct = pc.udp_in_use * 100 / crate::pcnet::POOL;
        let color = if pct >= 50 { p.caution } else { p.text3 };
        let mut line = format!("This PC: {pct}% UDP ports in use");
        if let Some(t) = pc.last_exhaustion {
            line.push_str(&format!(" · last ran out {} ago", short_duration(chrono::Local::now() - t)));
        }
        ui.add(egui::Label::new(caption(line, color)).truncate()).on_hover_text(
            "Running out of UDP ports breaks DNS on this PC, which looks like a connection drop even when the router is fine.",
        );
    }
}

/// The path out, hop by hop: tells whether a problem is this PC, the router, the ISP link or
/// beyond. While the internet is down, the nearest broken hop is named instead.
fn hops(ui: &mut egui::Ui, p: &Palette, s: &State) {
    let ms = |v: Option<f32>| v.map(|x| format!("{x:.0} ms")).unwrap_or("no reply".into());
    let (line, color) = if let Some(cause) = s.outage_cause.as_deref().filter(|_| s.internet_down) {
        (format!("Down at {cause}"), p.critical)
    } else {
        let dns = match s.dns_ok {
            Some(true) => "DNS ok",
            Some(false) => "DNS failing",
            None => "DNS —",
        };
        let gw = if s.gateway_ip.is_some() { ms(s.gateway_rtt_ms) } else { "—".into() };
        let bad = s.router_rtt_ms.is_none() || s.dns_ok == Some(false) || (s.gateway_ip.is_some() && s.gateway_rtt_ms.is_none());
        (format!("Router {} · ISP gateway {gw} · {dns}", ms(s.router_rtt_ms)), if bad { p.caution } else { p.text3 })
    };
    ui.add(egui::Label::new(caption(line, color)).truncate()).on_hover_text(
        "Pinged from this PC every 5 s: the router, the ISP's first hop, and a DNS lookup through the router.\n\
         Outages, DNS failures and slow or lossy stretches are logged with their duration and where they happened.",
    );
}

fn router(ui: &mut egui::Ui, p: &Palette, s: &State) {
    card_header(ui, p, icon::NETWORK, "Router");
    let cpu = s.cpu_pct.map(|c| format!("{c:.0}%")).unwrap_or("—".into());
    let ids = match s.ids_running {
        Some(true) => "running",
        Some(false) => "stopped",
        None => "—",
    };
    let stopped = s.services.iter().filter(|x| !x.running).count();
    let blocked = s.blocked_per_min.map(|b| format!("{b:.0}/min")).unwrap_or("—".into());
    // Only when `iptv_process` is configured.
    let iptv = s.iptv.as_ref().map(|i| match i {
        i if !i.running => "NOT RUNNING".to_string(),
        i => i.stream_bps.map(|b| format!("on · {}", fmt_bps(b))).unwrap_or("idle".into()),
    });
    let updates = match &s.firmware {
        None => "—".into(),
        Some(f) if f.patches > 0 => format!("{} waiting", f.patches),
        Some(f) if f.major.is_some() => "none (series EOL)".into(),
        Some(_) => "up to date".into(),
    };
    let mut tiles = vec![
        ("CPU", cpu),
        ("Memory", format!("{}/{} GB", fmt_gb(s.mem_used), fmt_gb(s.mem_total))),
        ("States", format!("{:.1}k", s.states as f64 / 1000.0)),
        ("Blocked in", blocked),
        ("Services", format!("{}/{}", s.services.len() - stopped, s.services.len())),
        ("Suricata", ids.into()),
        ("mbuf fails", s.mbuf_failures.to_string()),
        ("LAN drops", format!("{:.0}/min", s.lan_txdrops_per_min)),
        ("Updates", updates),
    ];
    if let Some(iptv) = iptv {
        tiles.insert(tiles.len() - 1, ("IPTV", iptv));
    }
    tile_grid(ui, p, &tiles);
    for svc in s.services.iter().filter(|x| !x.running) {
        ui.horizontal(|ui| {
            glyph(ui, icon::WARNING, 11.0, p.caution);
            ui.label(caption(format!("{} stopped", svc.id), p.text));
        });
    }
}

fn host(ui: &mut egui::Ui, p: &Palette, s: &State) {
    let Some(h) = &s.host else { return };
    card_header(ui, p, icon::PC, "Hyper-V host");
    let up = match (h.reachable, h.boot) {
        (false, _) => "no ssh".to_string(),
        (true, Some(b)) => format!("up {}", short_duration(chrono::Local::now() - b)),
        _ => "—".into(),
    };
    let vm = if h.reachable {
        format!("{} {}", h.vm_state, short_duration(chrono::Duration::seconds(h.vm_uptime_s as i64)))
    } else {
        "—".into()
    };
    let crash = h.last_crash.map(|c| format!("{} ago", short_duration(chrono::Local::now() - c))).unwrap_or("none".into());
    let hw = if h.reachable { h.hw_errors_7d.to_string() } else { "—".into() };
    tile_grid(ui, p, &[("Host", up), ("Router VM", vm), ("Last crash", crash), ("HW errors 7d", hw)]);
    let history: Vec<&Event> = s.events.iter().filter(|e| e.source == Source::Host).collect();
    if !history.is_empty() {
        // One line in the card: hover for the latest entries, click for the full list in its
        // own window (the inline list resized the whole layout).
        let link = ui.add(
            egui::Label::new(caption(format!("{}  Crash, restart & hardware history ({})", icon::HISTORY, history.len()), p.accent))
                .sense(Sense::click()),
        );
        let link = link.on_hover_cursor(egui::CursorIcon::PointingHand);
        // Shown directly while hovered: egui's own tooltips wait for a still pointer and are
        // suppressed by recent clicks/scrolls or another tooltip in the layer, and the
        // preview never appeared.
        if link.hovered() {
            egui::show_tooltip_at_pointer(ui.ctx(), ui.layer_id(), link.id.with("preview"), |ui| {
                ui.set_width(420.0 * k());
                for e in history.iter().take(HISTORY_PREVIEW) {
                    event_row(ui, p, e);
                }
                ui.label(caption(
                    if history.len() > HISTORY_PREVIEW {
                        format!("…{} more. Click to open the full list.", history.len() - HISTORY_PREVIEW)
                    } else {
                        "Click to open in its own window.".to_string()
                    },
                    p.text3,
                ));
            });
        }
        if link.clicked() {
            HISTORY_WINDOW.store(true, Ordering::Relaxed);
        }
    }
}

const HISTORY_PREVIEW: usize = 8;

/// Whether the host history window is open (from the host card; ROUTER_WIDGET_HISTORY opens
/// it at start, for screenshots, since posted clicks don't reach egui).
static HISTORY_WINDOW: AtomicBool = AtomicBool::new(false);
/// Theme choice last sent to that window + 1 (0 = not yet), so it is sent once, not per frame.
static HISTORY_THEME: AtomicU8 = AtomicU8::new(0);
/// The cross-screen drag subclass is installed on the current history window.
static HISTORY_SUBCLASSED: AtomicBool = AtomicBool::new(false);
/// The current history window has been placed beside the widget.
static HISTORY_PLACED: AtomicBool = AtomicBool::new(false);

pub fn init_history_window() {
    if std::env::var_os("ROUTER_WIDGET_HISTORY").is_some() {
        HISTORY_WINDOW.store(true, Ordering::Relaxed);
    }
}

/// Close the history window from outside its own frame loop (the widget is being hidden).
/// Hidden here right away; egui destroys it on the next frame it doesn't draw it.
pub fn close_history_window() {
    if HISTORY_WINDOW.swap(false, Ordering::Relaxed) {
        crate::dpi::hide_by_title("Hyper-V host history");
        HISTORY_THEME.store(0, Ordering::Relaxed);
        HISTORY_SUBCLASSED.store(false, Ordering::Relaxed);
        HISTORY_PLACED.store(false, Ordering::Relaxed);
    }
}

/// The full host history in its own window. Call once per frame from the app.
pub fn history_window(ctx: &egui::Context, s: &State) {
    if !HISTORY_WINDOW.load(Ordering::Relaxed) {
        return;
    }
    let id = egui::ViewportId::from_hash_of("host_history_window");
    let mut builder = egui::ViewportBuilder::default();
    // A first guess beside the widget, so it does not flash up at Windows' default spot;
    // `place_beside` then sets it exactly, in physical pixels.
    if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
        builder = builder.with_position(egui::pos2(r.right(), r.top()));
    }
    let builder = builder
        .with_title("Hyper-V host history")
        .with_icon(theme::app_icon())
        .with_inner_size([560.0, 520.0])
        .with_min_inner_size([360.0, 240.0])
        // The widget itself is always on top; this must not open behind it.
        .with_always_on_top();
    ctx.show_viewport_immediate(id, builder, |ctx, _class| {
        if ctx.input(|i| i.viewport().close_requested()) {
            HISTORY_WINDOW.store(false, Ordering::Relaxed);
            HISTORY_THEME.store(0, Ordering::Relaxed);
            HISTORY_SUBCLASSED.store(false, Ordering::Relaxed);
            HISTORY_PLACED.store(false, Ordering::Relaxed);
            return;
        }
        // Same cross-screen drag handling as the main window (see dpi.rs). The viewport runs
        // on this thread, so the subclass can be installed from here, once per window.
        // Retried each frame until the window exists (it may not on the very first one).
        // Also placed beside the widget then, instead of wherever Windows opens new windows.
        if !HISTORY_SUBCLASSED.load(Ordering::Relaxed) && crate::dpi::install_by_title("Hyper-V host history") {
            HISTORY_SUBCLASSED.store(true, Ordering::Relaxed);
        }
        // Retried too: at startup the widget's own window may not be visible yet.
        if !HISTORY_PLACED.load(Ordering::Relaxed) && crate::dpi::place_beside("Hyper-V host history", "Router") {
            HISTORY_PLACED.store(true, Ordering::Relaxed);
        }
        let choice = theme::choice();
        if HISTORY_THEME.swap(choice as u8 + 1, Ordering::Relaxed) != choice as u8 + 1 {
            ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(choice.system_theme()));
        }
        let p = Palette::of(ctx);
        egui::CentralPanel::default().frame(egui::Frame::central_panel(&ctx.style()).inner_margin(12)).show(ctx, |ui| {
            let history: Vec<&Event> = s.events.iter().filter(|e| e.source == Source::Host).collect();
            ui.label(
                caption(
                    "Hyper-V host event log: crashes and power loss, planned restarts, router VM start/stop/reset, and disk, storage and WHEA hardware errors (last 14 days).",
                    p.text2,
                ),
            );
            ui.add_space(6.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for e in history {
                    event_row(ui, &p, e);
                }
            });
        });
    });
}

fn devices(ui: &mut egui::Ui, p: &Palette, s: &State) {
    ui.horizontal(|ui| {
        glyph(ui, icon::DEVICES, 13.0, p.text2);
        ui.label(RichText::new("Busiest devices").font(theme::body_strong()));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if s.device_count > 0 {
                ui.label(caption(format!("{} on LAN", s.device_count), p.text3));
            }
        });
    });
    if s.top_lan.is_empty() {
        ui.label(caption("—", p.text3));
        return;
    }
    let max = s.top_lan.iter().map(|t| t.bps_in + t.bps_out).fold(1.0, f64::max);
    for t in &s.top_lan {
        let row = ui.horizontal(|ui| {
            ui.label(RichText::new(t.name.as_deref().unwrap_or(&t.addr)).font(theme::body())).on_hover_text(&t.addr);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(caption(fmt_bps(t.bps_out), p.text2));
                glyph(ui, icon::UP, 9.0, p.up);
                ui.label(caption(fmt_bps(t.bps_in), p.text2));
                glyph(ui, icon::DOWN, 9.0, p.accent);
            });
        });
        // Share-of-traffic underline, drawn in the row's bottom edge so it costs no height.
        let r = row.response.rect;
        let track = egui::Rect::from_min_max(egui::pos2(r.left(), r.bottom()), egui::pos2(r.right(), r.bottom() + 2.0));
        ui.painter().rect_filled(track, 1.0, p.track);
        let mut fill = track;
        fill.set_width(track.width() * ((t.bps_in + t.bps_out) / max) as f32);
        ui.painter().rect_filled(fill, 1.0, p.accent);
    }
}

fn activity_row_height() -> f32 {
    16.0 * k()
}

/// Activity fills the remaining height and scrolls on its own.
fn activity(ui: &mut egui::Ui, p: &Palette, s: &State, height: f32) {
    card_header(ui, p, icon::HISTORY, "Activity");
    let list_height = (height - 40.0 * k()).max(activity_row_height() * 2.0);
    egui::ScrollArea::vertical().max_height(list_height).auto_shrink([false, false]).show(ui, |ui| {
        // Host crash/restart history lives on the host card; this is router + security.
        let mut any = false;
        for e in s.events.iter().filter(|e| e.source != Source::Host) {
            event_row(ui, p, e);
            any = true;
        }
        if !any {
            ui.label(caption("Nothing yet", p.text3));
        }
    });
}

fn event_row(ui: &mut egui::Ui, p: &Palette, e: &Event) {
    ui.horizontal(|ui| {
        glyph(ui, level_icon(e.level), 11.0, level_fg(p, e.level));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let today = e.at.date_naive() == chrono::Local::now().date_naive();
            let time = if today { e.at.format("%H:%M").to_string() } else { e.at.format("%b %d %H:%M").to_string() };
            ui.label(caption(time, p.text3));
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(caption(&e.text, p.text)).truncate()).on_hover_text(&e.text);
            });
        });
    });
}

/// Caption/value tiles in as many columns as fit (2–4).
fn tile_grid(ui: &mut egui::Ui, p: &Palette, tiles: &[(&str, String)]) {
    // Fixed-width columns laid out by hand: egui's Grid ignores the computed widths inside
    // `ui.columns` and lets the last column spill out of the card.
    let gap = 8.0 * k();
    let width = ui.available_width();
    // Column count depends on width only (not on the scale): otherwise a scale change can
    // drop a column, change the height, change the scale back, and flicker every frame.
    let cols = (((width + gap) / (100.0 + gap)).floor() as usize).clamp(2, 4).min(tiles.len());
    let col_w = ((width - gap * (cols as f32 - 1.0)) / cols as f32).floor();
    for row in tiles.chunks(cols) {
        // Top-aligned: `ui.horizontal` centres children vertically, which staggers tiles.
        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (label, value) in row {
                ui.allocate_ui_with_layout(Vec2::new(col_w, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(col_w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add(egui::Label::new(caption(*label, p.text2)).truncate());
                    ui.add(egui::Label::new(RichText::new(value).font(theme::body_strong())).truncate());
                });
            }
        });
    }
}

fn sparkline(ui: &mut egui::Ui, p: &Palette, s: &State) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0 * k()), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, p.track);
    if s.history.len() < 2 {
        return;
    }
    let max = s.history.iter().map(|(rx, tx)| rx.max(*tx)).fold(1e5, f64::max);
    let step = rect.width() / (HISTORY_LEN - 1) as f32;
    let x0 = rect.right() - step * (s.history.len() - 1) as f32;
    let pts = |pick: fn(&(f64, f64)) -> f64| -> Vec<egui::Pos2> {
        s.history
            .iter()
            .enumerate()
            .map(|(i, v)| egui::pos2(x0 + i as f32 * step, rect.bottom() - 2.0 - (pick(v) / max) as f32 * (rect.height() - 6.0)))
            .collect()
    };
    painter.line(pts(|v| v.1), Stroke::new(1.2, p.up));
    painter.line(pts(|v| v.0), Stroke::new(1.6, p.accent));
    painter.text(rect.left_top() + Vec2::new(5.0, 2.0), egui::Align2::LEFT_TOP, fmt_bps(max), theme::caption(), p.text3);
    resp.on_hover_text(format!("WAN, last 5 minutes · {}", s.wan_ip));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_bps(12_400_000.0), "12.4 Mb/s");
        assert_eq!(fmt_bps(950.0), "950 b/s");
        assert_eq!(short_uptime("5 days, 18:53:54"), "5d 18h");
        assert_eq!(short_uptime("3:07:12"), "3h 07m");
    }
}
