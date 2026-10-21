use crate::api::{num, Api};
use crate::config::Config;
use crate::model::{FirmwareInfo, IptvInfo, Level, Service, Shared, Talker};
use crate::notify;
use crate::probe::fmt_secs;
use std::collections::{HashMap, HashSet};
use std::thread;
use std::time::{Duration, Instant};

const MEDIUM_EVERY: u64 = 5; // ticks
const SLOW_EVERY: u64 = 15;
const FAILS_BEFORE_DOWN: u32 = 2;
/// A LAN device moving more than this (either direction) counts as a heavy transfer...
const HEAVY_BPS: f64 = 20e6;
/// ...and is logged once it has lasted this long.
const HEAVY_MIN: Duration = Duration::from_secs(5 * 60);
const FIRMWARE_STATUS_EVERY: Duration = Duration::from_secs(6 * 3600);
const FIRMWARE_CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);

/// One device's ongoing heavy transfer.
struct Heavy {
    started: Instant,
    last: Instant,
    bytes_in: f64,
    bytes_out: f64,
    quiet: u32,
}

#[derive(Clone, Copy)]
struct Counters {
    wan_rx: f64,
    wan_tx: f64,
    lan_rx: f64,
    lan_tx: f64,
    lan_drops: f64,
}

struct Poller {
    api: Api,
    cfg: Config,
    state: Shared,
    ctx: eframe::egui::Context,
    prev_counters: Option<(Instant, Counters)>,
    /// Last seen uptime in seconds. `boottime` from the API is derived (now - uptime) and
    /// jitters by a second, so a reboot is detected as uptime going *down* instead.
    prev_uptime: Option<u64>,
    prev_config: Option<String>,
    prev_services: HashMap<String, bool>,
    prev_mbuf: Option<u64>,
    last_mbuf_rise: Option<Instant>,
    fail_streak: u32,
    names: HashMap<String, String>,
    wan_dev: String,
    lan_dev: String,
    prev_wan_ip: Option<String>,
    prev_prefix6: Option<String>,
    known: KnownDevices,
    /// Consecutive medium polls without the IPTV helper process; None until first checked.
    iptv_missing: Option<u32>,
    heavy: HashMap<String, Heavy>,
    /// WAN syslog lines already handled (timestamp + text).
    wan_log_seen: HashSet<String>,
    wan_log_primed: bool,
    firmware_status_at: Option<Instant>,
    firmware_check_at: Option<Instant>,
    /// Named devices' presence on the LAN: ip -> (present, consecutive misses).
    presence: HashMap<String, (bool, u32)>,
}

pub fn spawn(cfg: Config, state: Shared, ctx: eframe::egui::Context) {
    let api = match Api::new(&cfg.url, cfg.api_key.as_deref().unwrap_or(""), cfg.api_secret.as_deref().unwrap_or("")) {
        Ok(a) => a,
        Err(e) => {
            state.lock().unwrap().config_error = Some(e);
            return;
        }
    };
    crate::probe::spawn(cfg.clone(), state.clone(), ctx.clone());
    let mut p = Poller {
        api,
        cfg,
        state,
        ctx,
        prev_counters: None,
        prev_uptime: None,
        prev_config: None,
        prev_services: HashMap::new(),
        prev_mbuf: None,
        last_mbuf_rise: None,
        fail_streak: 0,
        names: HashMap::new(),
        // Filled from the API on the first poll.
        wan_dev: String::new(),
        lan_dev: String::new(),
        prev_wan_ip: None,
        prev_prefix6: None,
        known: KnownDevices::load(),
        iptv_missing: None,
        heavy: HashMap::new(),
        wan_log_seen: HashSet::new(),
        wan_log_primed: false,
        firmware_status_at: None,
        firmware_check_at: None,
        presence: HashMap::new(),
    };
    thread::spawn(move || p.run());
}

impl Poller {
    fn run(&mut self) {
        let period = Duration::from_secs(self.cfg.poll_secs.max(1));
        let mut tick: u64 = 0;
        loop {
            let started = Instant::now();
            let ok = self.fast();
            self.track_connectivity(ok);
            if ok {
                if tick % SLOW_EVERY == 0 {
                    self.slow();
                }
                if tick % MEDIUM_EVERY == 0 {
                    self.medium();
                }
            }
            self.evaluate();
            self.ctx.request_repaint();
            tick += 1;
            thread::sleep(period.saturating_sub(started.elapsed()));
        }
    }

    fn raise(&self, level: Level, text: &str, toast: bool) {
        self.state.lock().unwrap().push_event(level, text);
        if toast && self.cfg.toasts {
            notify::toast(&self.cfg.toast_app_id, level, text);
        }
    }

    fn track_connectivity(&mut self, ok: bool) {
        if ok {
            if self.fail_streak >= FAILS_BEFORE_DOWN {
                self.raise(Level::Info, "Router API reachable again", true);
            }
            self.fail_streak = 0;
            let mut s = self.state.lock().unwrap();
            s.connected = true;
            s.last_ok = Some(Instant::now());
        } else {
            self.fail_streak += 1;
            if self.fail_streak == FAILS_BEFORE_DOWN {
                self.state.lock().unwrap().connected = false;
                self.raise(Level::Alert, "Router API unreachable", true);
            }
        }
    }

    /// Every tick: interface byte counters -> throughput.
    fn fast(&mut self) -> bool {
        let t0 = Instant::now();
        let Ok(v) = self.api.get("/api/diagnostics/traffic/interface") else { return false };
        let ms = t0.elapsed().as_millis() as u32;
        let ifs = &v["interfaces"];
        if let Some(d) = ifs["wan"]["device"].as_str() {
            self.wan_dev = d.to_string();
        }
        if let Some(d) = ifs["lan"]["device"].as_str() {
            self.lan_dev = d.to_string();
        }
        let c = Counters {
            wan_rx: num(&ifs["wan"]["bytes received"]).unwrap_or(0.0),
            wan_tx: num(&ifs["wan"]["bytes transmitted"]).unwrap_or(0.0),
            lan_rx: num(&ifs["lan"]["bytes received"]).unwrap_or(0.0),
            lan_tx: num(&ifs["lan"]["bytes transmitted"]).unwrap_or(0.0),
            lan_drops: num(&ifs["lan"]["send queue drops"]).unwrap_or(0.0),
        };
        let now = Instant::now();
        let mut s = self.state.lock().unwrap();
        s.api_ms = Some(ms);
        if let Some((then, p)) = self.prev_counters {
            let dt = now.duration_since(then).as_secs_f64();
            // Counters reset on reboot; a negative delta just means "skip this sample".
            let rate = |a: f64, b: f64| if a >= b && dt > 0.0 { (a - b) * 8.0 / dt } else { 0.0 };
            s.wan_rx_bps = rate(c.wan_rx, p.wan_rx);
            s.wan_tx_bps = rate(c.wan_tx, p.wan_tx);
            s.lan_rx_bps = rate(c.lan_rx, p.lan_rx);
            s.lan_tx_bps = rate(c.lan_tx, p.lan_tx);
            s.lan_txdrops_per_min = if c.lan_drops >= p.lan_drops { (c.lan_drops - p.lan_drops) * 60.0 / dt } else { 0.0 };
            let (rx, tx) = (s.wan_rx_bps, s.wan_tx_bps);
            s.push_history(rx, tx);
        }
        self.prev_counters = Some((now, c));
        true
    }

    /// ~10 s: CPU, memory, states, mbufs, top talkers.
    fn medium(&mut self) {
        if let Ok(v) = self.api.get("/api/diagnostics/activity/getActivity") {
            let cpu = v["headers"].as_array().and_then(|h| {
                h.iter().filter_map(|l| l.as_str()).find(|l| l.starts_with("CPU:")).and_then(parse_idle)
            });
            self.state.lock().unwrap().cpu_pct = cpu.map(|idle| (100.0 - idle).max(0.0));
            if let (Some(procs), Some(name)) = (v["details"].as_array(), self.cfg.iptv_process.clone()) {
                let running = procs.iter().any(|p| p["COMMAND"].as_str().is_some_and(|c| c.contains(name.as_str())));
                self.track_iptv(running);
            }
        }
        if let Ok(v) = self.api.get("/api/diagnostics/system/systemResources") {
            let mut s = self.state.lock().unwrap();
            s.mem_total = num(&v["memory"]["total"]).unwrap_or(0.0) as u64;
            s.mem_used = num(&v["memory"]["used"]).unwrap_or(0.0) as u64;
        }
        if let Ok(v) = self.api.get("/api/diagnostics/firewall/pf_states") {
            let mut s = self.state.lock().unwrap();
            s.states = num(&v["current"]).unwrap_or(0.0) as u64;
            s.states_limit = num(&v["limit"]).unwrap_or(0.0) as u64;
        }
        if let Ok(v) = self.api.get("/api/diagnostics/system/systemMbuf") {
            let m = &v["mbuf-statistics"];
            let fails = ["mbuf-failures", "cluster-failures", "packet-failures"]
                .iter()
                .filter_map(|k| num(&m[*k]))
                .sum::<f64>() as u64;
            if let Some(prev) = self.prev_mbuf {
                if fails > prev {
                    self.last_mbuf_rise = Some(Instant::now());
                    self.raise(Level::Warn, &format!("mbuf allocation failures +{}", fails - prev), true);
                }
            }
            self.prev_mbuf = Some(fails);
            self.state.lock().unwrap().mbuf_failures = fails;
        }
        if let Ok(v) = self.api.get("/api/diagnostics/traffic/top/lan") {
            let talkers: Vec<Talker> = v["lan"]["records"]
                .as_array()
                .map(|recs| {
                    recs.iter()
                        .map(|r| {
                            let addr = r["address"].as_str().unwrap_or("?").to_string();
                            let name = self.cfg.names.get(&addr).or_else(|| self.names.get(&addr)).cloned();
                            Talker {
                                name,
                                addr,
                                bps_in: num(&r["rate_bits_in"]).unwrap_or(0.0),
                                bps_out: num(&r["rate_bits_out"]).unwrap_or(0.0),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            // TV: an IPTV multicast helper re-sends the ISP's streams onto the LAN.
            let (tv, talkers): (Vec<Talker>, Vec<Talker>) = talkers.into_iter().partition(|t| is_tv_multicast(&t.addr));
            let tv_bps: f64 = tv.iter().map(|t| t.bps_in + t.bps_out).sum();
            let mut talkers = merge_by_name(talkers);
            self.track_heavy(&talkers);
            talkers.sort_by(|a, b| (b.bps_in + b.bps_out).total_cmp(&(a.bps_in + a.bps_out)));
            talkers.truncate(4);
            let mut s = self.state.lock().unwrap();
            s.top_lan = talkers;
            if let Some(i) = &mut s.iptv {
                i.stream_bps = (tv_bps > 0.0).then_some(tv_bps);
            }
        }
    }

    /// ~30 s: identity, uptime/boot, services, IDS, DHCP names.
    fn slow(&mut self) {
        if self.state.lock().unwrap().version.is_empty() {
            if let Ok(v) = self.api.get("/api/diagnostics/system/systemInformation") {
                let mut s = self.state.lock().unwrap();
                s.hostname = v["name"].as_str().unwrap_or("").split('.').next().unwrap_or("").to_string();
                s.version = v["versions"][0].as_str().unwrap_or("").trim_end_matches("-amd64").to_string();
            }
        }
        if let Ok(v) = self.api.get("/api/diagnostics/system/systemTime") {
            let config = v["config"].as_str().unwrap_or("").to_string();
            let uptime = v["uptime"].as_str().unwrap_or("").to_string();
            let secs = uptime_secs(&uptime);
            match (self.prev_uptime, secs) {
                (Some(prev), Some(cur)) if cur + 60 < prev => {
                    self.raise(Level::Alert, &format!("Router rebooted (up {uptime})"), true)
                }
                (None, Some(cur)) if cur < 15 * 60 => {
                    self.raise(Level::Info, &format!("Router booted recently (up {uptime})"), false)
                }
                _ => {}
            }
            if self.prev_config.as_ref().is_some_and(|p| *p != config) {
                let text = self.config_change_text();
                self.raise(Level::Info, &text, false);
            }
            if secs.is_some() {
                self.prev_uptime = secs;
            }
            self.prev_config = Some(config);
            let mut s = self.state.lock().unwrap();
            s.uptime = uptime;
            s.loadavg = v["loadavg"].as_str().unwrap_or("").to_string();
        }
        if let Ok(v) = self.api.get("/api/core/service/search") {
            let services: Vec<Service> = v["rows"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .map(|r| Service {
                            id: r["id"].as_str().unwrap_or("").to_string(),
                            running: num(&r["running"]).unwrap_or(0.0) > 0.0,
                        })
                        .collect()
                })
                .unwrap_or_default();
            for svc in &services {
                let watched = self.cfg.watch_services.contains(&svc.id);
                match self.prev_services.get(&svc.id) {
                    Some(true) if !svc.running => {
                        let lvl = if watched { Level::Warn } else { Level::Info };
                        self.raise(lvl, &format!("Service stopped: {}", svc.id), watched);
                    }
                    Some(false) if svc.running => self.raise(Level::Info, &format!("Service started: {}", svc.id), watched),
                    _ => {}
                }
                self.prev_services.insert(svc.id.clone(), svc.running);
            }
            self.state.lock().unwrap().services = services;
        }
        if let Ok(v) = self.api.get("/api/ids/service/status") {
            self.state.lock().unwrap().ids_running = v["status"].as_str().map(|s| s == "running");
        }
        if let Ok(v) = self.api.get("/api/dnsmasq/leases/search") {
            if let Some(rows) = v["rows"].as_array() {
                for r in rows {
                    if let (Some(a), Some(h)) = (r["address"].as_str(), r["hostname"].as_str()) {
                        if !h.is_empty() {
                            self.names.insert(a.to_string(), h.to_string());
                        }
                    }
                }
            }
        }
        self.resolve_ipv6_names();
        self.check_addresses();
        self.blocked_rate();
        self.gateway();
        self.wan_log();
        self.firmware();
    }

    /// The newest config-history entry: what changed it and who ("/api/firewall/... by admin@...").
    fn config_change_text(&self) -> String {
        let latest = self.api.get("/api/core/backup/backups/this").ok().and_then(|v| {
            let e = v["items"].as_array()?.first()?.clone();
            let what = e["description"].as_str()?.trim_end_matches(" made changes").to_string();
            Some(format!("{what} by {}", e["username"].as_str().unwrap_or("?")))
        });
        match latest {
            Some(d) => format!("Router config changed: {d}"),
            None => "Router config changed".into(),
        }
    }

    fn track_iptv(&mut self, running: bool) {
        // The watchdog cron restarts the helper within a minute; one missed sample is noise.
        let missing = if running { 0 } else { self.iptv_missing.unwrap_or(0) + 1 };
        match (self.iptv_missing, running) {
            (Some(n), true) if n >= 2 => self.raise(Level::Info, "IPTV helper running again", true),
            (_, false) if missing == 2 => self.raise(Level::Alert, "IPTV helper is not running: TV multicast stops", true),
            _ => {}
        }
        self.iptv_missing = Some(missing);
        let mut s = self.state.lock().unwrap();
        let stream_bps = s.iptv.as_ref().and_then(|i| i.stream_bps);
        s.iptv = Some(IptvInfo { running: running || missing < 2, stream_bps });
    }

    /// Log a device's sustained heavy transfer (>= 20 Mb/s for >= 5 min) once it ends.
    fn track_heavy(&mut self, talkers: &[Talker]) {
        let now = Instant::now();
        for t in talkers {
            let name = t.name.clone().unwrap_or_else(|| t.addr.clone());
            let busy = t.bps_in.max(t.bps_out) >= HEAVY_BPS;
            match self.heavy.get_mut(&name) {
                Some(h) => {
                    let dt = now.duration_since(h.last).as_secs_f64();
                    h.bytes_in += t.bps_in * dt / 8.0;
                    h.bytes_out += t.bps_out * dt / 8.0;
                    h.last = now;
                    h.quiet = if busy { 0 } else { h.quiet + 1 };
                }
                None if busy => {
                    self.heavy.insert(name, Heavy { started: now, last: now, bytes_in: 0.0, bytes_out: 0.0, quiet: 0 });
                }
                None => {}
            }
        }
        // A device that drops out of the talker list has gone quiet too.
        let mut done = Vec::new();
        for (name, h) in self.heavy.iter_mut() {
            if !talkers.iter().any(|t| t.name.as_deref().unwrap_or(&t.addr) == name) {
                h.quiet += 1;
                h.last = now;
            }
            if h.quiet >= 3 {
                done.push(name.clone());
            }
        }
        for name in done {
            let h = self.heavy.remove(&name).unwrap();
            let secs = h.last.duration_since(h.started).as_secs();
            if Duration::from_secs(secs) >= HEAVY_MIN {
                let avg = (h.bytes_in + h.bytes_out) * 8.0 / secs.max(1) as f64;
                let text = format!(
                    "Heavy transfer: {name} ↓{} ↑{} over {} (avg {})",
                    fmt_bytes(h.bytes_in),
                    fmt_bytes(h.bytes_out),
                    fmt_secs(secs),
                    crate::ui::fmt_bps(avg)
                );
                self.raise(Level::Info, &text, false);
            }
        }
    }

    /// The ISP-side gateway of the WAN, so the probe can tell "ISP link" from "beyond".
    fn gateway(&mut self) {
        let Ok(v) = self.api.get("/api/routes/gateway/status") else { return };
        let ip = v["items"]
            .as_array()
            .and_then(|items| items.iter().filter_map(|g| g["address"].as_str()?.parse::<std::net::Ipv4Addr>().ok()).next());
        if ip.is_some() {
            self.state.lock().unwrap().gateway_ip = ip;
        }
    }

    /// WAN link and DHCP events from the router's system log (link down/up, lease lost/new).
    fn wan_log(&mut self) {
        if self.wan_dev.is_empty() {
            return;
        }
        let body = serde_json::json!({"current": 1, "rowCount": 50, "searchPhrase": self.wan_dev});
        let Ok(v) = self.api.post("/api/diagnostics/log/core/system", &body) else { return };
        let Some(rows) = v["rows"].as_array() else { return };
        let mut fresh = Vec::new();
        for r in rows.iter().rev() {
            let (Some(ts), Some(process), Some(line)) = (r["timestamp"].as_str(), r["process_name"].as_str(), r["line"].as_str())
            else {
                continue;
            };
            if !self.wan_log_seen.insert(format!("{ts}|{line}")) {
                continue;
            }
            if let Some((lvl, text)) = classify_wan_line(process, line, &self.wan_dev) {
                let at = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S")
                    .ok()
                    .and_then(|t| t.and_local_timezone(chrono::Local).single());
                fresh.push((at, lvl, text));
            }
        }
        // First pass: show the recent history without toasting it.
        let primed = self.wan_log_primed;
        self.wan_log_primed = true;
        for (at, lvl, text) in fresh {
            let at = at.unwrap_or_else(chrono::Local::now);
            self.state.lock().unwrap().insert_event(at, crate::model::Source::Router, lvl, text.clone());
            if primed && lvl >= Level::Warn && self.cfg.toasts {
                notify::toast(&self.cfg.toast_app_id, lvl, &text);
            }
        }
    }

    /// Update status every 6 h; ask the router to refresh its update list once a day.
    fn firmware(&mut self) {
        if self.firmware_check_at.is_none_or(|t| t.elapsed() >= FIRMWARE_CHECK_EVERY) {
            self.firmware_check_at = Some(Instant::now());
            // Only refreshes the package lists (the GUI's "Check for updates"); installs nothing.
            let _ = self.api.post("/api/core/firmware/check", &serde_json::json!({}));
            // The check runs in the background; read its result a couple of slow polls later.
            self.firmware_status_at = Instant::now().checked_sub(FIRMWARE_STATUS_EVERY - Duration::from_secs(90));
            return;
        }
        if self.firmware_status_at.is_some_and(|t| t.elapsed() < FIRMWARE_STATUS_EVERY) {
            return;
        }
        let Ok(v) = self.api.get("/api/core/firmware/status") else { return };
        self.firmware_status_at = Some(Instant::now());
        let patches = ["upgrade_packages", "new_packages", "reinstall_packages"]
            .iter()
            .map(|k| v[*k].as_array().map_or(0, |a| a.len()))
            .sum::<usize>();
        let major = v["upgrade_major_version"].as_str().filter(|m| !m.is_empty()).map(String::from);
        let info = FirmwareInfo { patches, major: major.clone() };
        let prev = self.state.lock().unwrap().firmware.replace(info);
        let prev_patches = prev.as_ref().map_or(0, |p| p.patches);
        if patches > 0 && patches != prev_patches {
            self.raise(Level::Info, &format!("Router updates available: {patches} package(s) for the installed series"), true);
        }
        if let Some(m) = major.filter(|m| prev.as_ref().is_some_and(|p| p.major.as_ref() != Some(m))) {
            self.raise(Level::Info, &format!("OPNsense {m} major upgrade offered (installed series is end-of-life)"), false);
        }
    }

    /// Named devices (config `[names]`) leaving and rejoining the LAN, from the ARP table.
    /// FreeBSD keeps an ARP entry ~20 min after a device goes quiet, so "left" lags that much.
    fn track_presence(&mut self, lan_ips: &HashSet<String>) {
        let mut events = Vec::new();
        for (ip, name) in &self.cfg.names {
            if !ip.contains('.') {
                continue;
            }
            let here = lan_ips.contains(ip);
            match self.presence.get_mut(ip) {
                None => {
                    self.presence.insert(ip.clone(), (here, 0));
                }
                Some((present, misses)) => {
                    if here {
                        if !*present {
                            events.push(format!("{name} is back on the LAN"));
                        }
                        *present = true;
                        *misses = 0;
                    } else if *present {
                        *misses += 1;
                        if *misses >= 2 {
                            *present = false;
                            events.push(format!("{name} left the LAN (no ARP entry)"));
                        }
                    }
                }
            }
        }
        for e in events {
            self.raise(Level::Info, &e, false);
        }
    }


    /// WAN IPv4 (CGNAT, changes on DHCP) and the delegated LAN IPv6 prefix (renumbers devices).
    fn check_addresses(&mut self) {
        let Ok(v) = self.api.get("/api/diagnostics/interface/get_interface_config") else { return };
        let wan_ip = v[&self.wan_dev]["ipv4"][0]["ipaddr"].as_str().map(String::from);
        let prefix = v[&self.lan_dev]["ipv6"].as_array().and_then(|a| {
            a.iter().find_map(|x| {
                let ip: std::net::Ipv6Addr = x["ipaddr"].as_str()?.parse().ok()?;
                let bits = num(&x["subnetbits"])? as u32;
                (!ip.is_unicast_link_local()).then(|| ipv6_prefix(ip, bits))
            })
        });
        if let (Some(old), Some(new)) = (&self.prev_wan_ip, &wan_ip) {
            if old != new {
                self.raise(Level::Info, &format!("WAN address changed {old} → {new}"), false);
            }
        }
        if let (Some(old), Some(new)) = (&self.prev_prefix6, &prefix) {
            if old != new {
                self.raise(Level::Warn, &format!("IPv6 prefix changed {old} → {new}"), true);
            }
        }
        let mut s = self.state.lock().unwrap();
        s.wan_ip = wan_ip.clone().unwrap_or_default();
        s.lan_prefix6 = prefix.clone().unwrap_or_default();
        drop(s);
        self.prev_wan_ip = wan_ip.or(self.prev_wan_ip.take());
        self.prev_prefix6 = prefix.or(self.prev_prefix6.take());
    }

    /// The pf log holds the last ~1000 entries (only seconds of traffic here), so this is a
    /// rate sample, not a count.
    fn blocked_rate(&mut self) {
        let Ok(v) = self.api.get("/api/diagnostics/firewall/log/?limit=1000") else { return };
        let Some(rows) = v.as_array() else { return };
        let ts = |r: &serde_json::Value| {
            chrono::NaiveDateTime::parse_from_str(r["__timestamp__"].as_str()?, "%Y-%m-%dT%H:%M:%S").ok()
        };
        let (Some(newest), Some(oldest)) = (rows.first().and_then(ts), rows.last().and_then(ts)) else { return };
        let blocked = rows
            .iter()
            .filter(|r| r["action"] == "block" && r["dir"] == "in" && r["interface"] == self.wan_dev.as_str())
            .count();
        let span = (newest - oldest).num_seconds().max(1) as f64;
        self.state.lock().unwrap().blocked_per_min = Some(blocked as f64 * 60.0 / span);
    }

    /// DHCP names only cover IPv4. Map each LAN IPv6 address to its device via the MAC
    /// (NDP table -> ARP table -> IPv4 -> name), so v6 talkers show the same name as v4.
    fn resolve_ipv6_names(&mut self) {
        let (Ok(arp), Ok(ndp)) =
            (self.api.get("/api/diagnostics/interface/get_arp"), self.api.get("/api/diagnostics/interface/get_ndp"))
        else {
            return;
        };
        let rows = |v: &serde_json::Value| v.as_array().or_else(|| v["rows"].as_array()).cloned().unwrap_or_default();
        let mut mac_to_v4: HashMap<String, String> = HashMap::new();
        let mut seen = Vec::new();
        for r in rows(&arp) {
            if let (Some(mac), Some(ip)) = (r["mac"].as_str(), r["ip"].as_str()) {
                mac_to_v4.insert(mac.to_lowercase(), ip.to_string());
                if let Some(h) = r["hostname"].as_str().filter(|h| !h.is_empty()) {
                    self.names.entry(ip.to_string()).or_insert_with(|| h.to_string());
                }
                if r["intf"].as_str() == Some(self.lan_dev.as_str()) && mac.contains(':') {
                    seen.push((mac.to_lowercase(), ip.to_string(), r["manufacturer"].as_str().unwrap_or("").to_string()));
                }
            }
        }
        self.state.lock().unwrap().device_count = seen.len();
        let lan_ips: HashSet<String> = seen.iter().map(|(_, ip, _)| ip.clone()).collect();
        self.track_presence(&lan_ips);
        for (mac, ip, vendor) in seen {
            let name = self.cfg.names.get(&ip).or_else(|| self.names.get(&ip)).cloned().unwrap_or_else(|| ip.clone());
            if self.known.add(&mac, &name) {
                let vendor = if vendor.is_empty() { String::new() } else { format!(" · {vendor}") };
                self.raise(Level::Info, &format!("New device on LAN: {name} ({ip}){vendor}"), true);
            }
        }
        for r in rows(&ndp) {
            let (Some(mac), Some(ip)) = (r["mac"].as_str(), r["ip"].as_str()) else { continue };
            if ip.starts_with("fe80") {
                continue;
            }
            if let Some(v4) = mac_to_v4.get(&mac.to_lowercase()) {
                let name = self.cfg.names.get(v4).or_else(|| self.names.get(v4)).cloned().unwrap_or_else(|| v4.clone());
                self.names.insert(ip.to_string(), name);
            }
        }
    }

    /// Turn the current numbers into the list of active problems (drives the colour).
    fn evaluate(&self) {
        let mut s = self.state.lock().unwrap();
        let mut p: Vec<(Level, String)> = Vec::new();
        if let Some(e) = &s.config_error {
            p.push((Level::Alert, e.clone()));
        }
        if !s.connected {
            p.push((Level::Alert, "Router API unreachable".into()));
        }
        if s.internet_down {
            let why = s.outage_cause.as_deref().map(|c| format!(": {c}")).unwrap_or_default();
            p.push((Level::Alert, format!("Internet unreachable{why}")));
        } else if s.internet_loss_pct >= 30.0 {
            p.push((Level::Warn, format!("Internet loss {:.0}%", s.internet_loss_pct)));
        } else if s.internet_rtt_ms.is_some_and(|r| r > 150.0) {
            p.push((Level::Warn, format!("High latency {:.0} ms", s.internet_rtt_ms.unwrap())));
        }
        if let Some(pc) = &s.pc {
            let pct = pc.udp_in_use * 100 / crate::pcnet::POOL;
            if pct >= 50 {
                let who = pc.top.first().map(|t| format!(" ({} holds {})", t.0, t.1)).unwrap_or_default();
                p.push((Level::Warn, format!("This PC is running out of UDP ports: {pct}% used{who}")));
            }
        }
        // Wazuh silently stopped ingesting for a month once (disk full); alerts normally
        // arrive every few seconds, so 30 min of silence while connected is a fault.
        if let Some(w) = &s.wazuh {
            if w.connected && w.last_heard.is_some_and(|t| chrono::Local::now() - t > chrono::Duration::minutes(30)) {
                p.push((Level::Warn, "Wazuh has stopped producing alerts".into()));
            }
        }
        if s.dns_failing {
            p.push((Level::Warn, "DNS through the router is failing".into()));
        }
        if s.iptv.as_ref().is_some_and(|i| !i.running) {
            p.push((Level::Alert, "IPTV helper is not running".into()));
        }
        if s.connected {
            if s.mem_total > 0 && s.mem_total.saturating_sub(s.mem_used) < 512 * 1024 * 1024 {
                p.push((Level::Warn, "Router free memory < 512 MB".into()));
            }
            if s.states_limit > 0 && s.states * 100 / s.states_limit >= 80 {
                p.push((Level::Warn, "State table > 80%".into()));
            }
            if s.cpu_pct.is_some_and(|c| c > 90.0) {
                p.push((Level::Warn, "Router CPU > 90%".into()));
            }
            if self.last_mbuf_rise.is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
                p.push((Level::Warn, "mbuf failures in last 10 min".into()));
            }
            if s.ids_running == Some(false) {
                p.push((Level::Warn, "Suricata IDS not running".into()));
            }
            for svc in s.services.iter().filter(|x| !x.running && self.cfg.watch_services.contains(&x.id)) {
                p.push((Level::Warn, format!("{} stopped", svc.id)));
            }
        }
        s.problems = p;
    }
}

/// MACs ever seen on the LAN, persisted so "new device" survives restarts. The very first
/// run seeds the file silently instead of announcing every existing device.
struct KnownDevices {
    path: Option<std::path::PathBuf>,
    macs: HashMap<String, String>,
    seeding: bool,
}

impl KnownDevices {
    fn load() -> Self {
        let path = dirs::config_dir().map(|d| d.join("router-widget").join("known_devices.json"));
        let existing = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
        let macs = existing.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
        Self { path, macs, seeding: existing.is_none() }
    }

    /// Returns true if this MAC was not known before (and we are past the seeding run).
    fn add(&mut self, mac: &str, name: &str) -> bool {
        if self.macs.contains_key(mac) {
            return false;
        }
        self.macs.insert(mac.to_string(), name.to_string());
        if let Some(p) = &self.path {
            let _ = std::fs::write(p, serde_json::to_string_pretty(&self.macs).unwrap_or_default());
        }
        !self.seeding
    }
}

/// TV multicast (224/4), minus the LAN chatter groups (224.0.0.x, SSDP).
fn is_tv_multicast(addr: &str) -> bool {
    let Ok(ip) = addr.parse::<std::net::Ipv4Addr>() else { return false };
    let o = ip.octets();
    ip.is_multicast() && !(o[0] == 224 && o[1] == 0 && o[2] == 0) && addr != "239.255.255.250"
}

fn fmt_bytes(b: f64) -> String {
    match b {
        b if b >= 1e9 => format!("{:.1} GB", b / 1e9),
        b if b >= 1e6 => format!("{:.0} MB", b / 1e6),
        b => format!("{:.0} kB", b / 1e3),
    }
}

/// What a WAN-related syslog line means. Routine renewals are not news.
fn classify_wan_line(process: &str, line: &str, wan: &str) -> Option<(Level, String)> {
    let l = line.trim();
    match process {
        "kernel" if l.contains(&format!("{wan}: link state changed to DOWN")) => {
            Some((Level::Alert, "WAN link went down (cable / ISP side)".into()))
        }
        "kernel" if l.contains(&format!("{wan}: link state changed to UP")) => Some((Level::Info, "WAN link up".into())),
        "dhclient" => {
            for (needle, lvl, text) in [
                ("DHCPNAK", Level::Warn, "ISP refused the WAN DHCP lease (NAK)"),
                ("No DHCPOFFERS", Level::Alert, "No DHCP offer from the ISP on WAN"),
                ("Reason EXPIRE", Level::Alert, "WAN DHCP lease expired"),
                ("Reason FAIL", Level::Alert, "WAN DHCP failed"),
                ("Reason BOUND", Level::Info, "WAN got a new DHCP lease"),
                ("Reason REBOOT", Level::Info, "WAN DHCP lease re-acquired after restart"),
            ] {
                if l.contains(needle) {
                    return Some((lvl, text.into()));
                }
            }
            None
        }
        "dhcp6c" if l.contains("dhcp6c_script:") && !l.contains("RENEW") && !l.contains("REBIND") => {
            let reason = l.split("dhcp6c_script:").nth(1)?.split_whitespace().next()?.to_string();
            Some((Level::Info, format!("WAN IPv6 lease: {reason}")))
        }
        _ => None,
    }
}

fn ipv6_prefix(ip: std::net::Ipv6Addr, bits: u32) -> String {
    let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits.min(128)) };
    format!("{}/{bits}", std::net::Ipv6Addr::from(u128::from(ip) & mask))
}

/// One row per device: a host talking over both IPv4 and IPv6 shows up twice in `top/lan`.
fn merge_by_name(talkers: Vec<Talker>) -> Vec<Talker> {
    let mut out: Vec<Talker> = Vec::new();
    for t in talkers {
        match out.iter_mut().find(|o| t.name.is_some() && o.name == t.name) {
            Some(o) => {
                o.bps_in += t.bps_in;
                o.bps_out += t.bps_out;
                o.addr = format!("{}\n{}", o.addr, t.addr);
            }
            None => out.push(t),
        }
    }
    out
}

/// "CPU: 1.6% user, 0.0% nice, 3.9% system, 0.0% interrupt, 94.5% idle" -> 94.5
fn parse_idle(line: &str) -> Option<f32> {
    line.split(',').find(|p| p.contains("idle"))?.trim().split('%').next()?.parse().ok()
}

/// "5 days, 18:53:54" / "1 day, 0:07:12" / "0:07:12" -> seconds.
fn uptime_secs(s: &str) -> Option<u64> {
    let (days, clock) = match s.split_once(',') {
        Some((d, rest)) => (d.split_whitespace().next()?.parse::<u64>().ok()?, rest.trim()),
        None => (0, s.trim()),
    };
    let parts: Vec<u64> = clock.split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    let hms = match parts.as_slice() {
        [h, m, sec] => h * 3600 + m * 60 + sec,
        [m, sec] => m * 60 + sec,
        _ => return None,
    };
    Some(days * 86400 + hms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_top_cpu_line() {
        assert_eq!(parse_idle("CPU: 1.6% user, 0.0% nice, 3.9% system, 0.0% interrupt, 94.5% idle"), Some(94.5));
    }

    #[test]
    fn merges_v4_and_v6_rows_of_one_host() {
        let t = |addr: &str, name: Option<&str>, i: f64| Talker { addr: addr.into(), name: name.map(Into::into), bps_in: i, bps_out: 1.0 };
        let m = merge_by_name(vec![t("192.168.1.20", Some("NAS"), 5.0), t("2001:db8::20", Some("NAS"), 7.0), t("10.0.0.9", None, 1.0)]);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].bps_in, 12.0);
        assert_eq!(m[0].addr, "192.168.1.20\n2001:db8::20");
    }

    #[test]
    fn picks_tv_multicast() {
        assert!(is_tv_multicast("234.12.3.4"));
        assert!(!is_tv_multicast("239.255.255.250"));
        assert!(!is_tv_multicast("224.0.0.251"));
        assert!(!is_tv_multicast("192.168.1.50"));
    }

    #[test]
    fn classifies_wan_log_lines() {
        assert_eq!(classify_wan_line("kernel", "<6>[2] hn0: link state changed to DOWN", "hn0").unwrap().0, Level::Alert);
        assert!(classify_wan_line("kernel", "<6>[2] hn1: link state changed to DOWN", "hn0").is_none());
        assert!(classify_wan_line("dhclient", "dhclient-script: Reason RENEW on hn0 executing", "hn0").is_none());
        assert!(classify_wan_line("dhcp6c", " dhcp6c_script: RENEW on hn0 executing", "hn0").is_none());
        assert_eq!(classify_wan_line("dhcp6c", " dhcp6c_script: REQUEST on hn0 executing", "hn0").unwrap().1, "WAN IPv6 lease: REQUEST");
    }

    #[test]
    fn computes_ipv6_prefix() {
        let ip: std::net::Ipv6Addr = "2001:db8:28f8:5500:215:5dff:fe00:1".parse().unwrap();
        assert_eq!(ipv6_prefix(ip, 56), "2001:db8:28f8:5500::/56");
    }

    #[test]
    fn parses_uptime() {
        assert_eq!(uptime_secs("0:07:12"), Some(432));
        assert_eq!(uptime_secs("2:01:00"), Some(7260));
        assert_eq!(uptime_secs("1 day, 0:00:05"), Some(86405));
        assert_eq!(uptime_secs("5 days, 18:53:54"), Some(5 * 86400 + 18 * 3600 + 53 * 60 + 54));
        assert_eq!(uptime_secs("garbage"), None);
    }
}
