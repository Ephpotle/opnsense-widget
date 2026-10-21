//! Wazuh feed: a long-lived `ssh` to the Wazuh manager whose key is restricted to a forced
//! command (`/usr/local/bin/widget-alerts`) that streams a filtered `alerts.json` plus
//! heartbeats. Turns the security/network alerts drafted in README into events and tiles.

use crate::config::WazuhConfig;
use crate::model::{Level, Shared, Source, WazuhInfo};
use crate::notify;
use chrono::{DateTime, Duration as ChronoDuration, Local, Timelike};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Only alerts this fresh may raise a toast, so the backfill on (re)connect stays silent.
const TOAST_WINDOW_MIN: i64 = 3;
/// Repeats of the same file-change / service-crash message are folded within this window.
const FOLD_HOURS: i64 = 6;
/// Router files OPNsense rewrites on its own (DHCP/DNS); changes to these are not news.
const ROUTINE_FILES: [&str; 2] = ["/etc/resolv.conf", "/etc/hosts"];
/// An hour of scan bursts is only worth logging above this multiple of the typical hour.
const SCAN_NOTABLE: f32 = 2.0;
const SCAN_TOAST: f32 = 3.0;
const SCAN_FLOOR: u32 = 30;

fn parse_ts(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3f%z").ok().map(|t| t.with_timezone(&Local))
}

fn is_private(ip: &str) -> bool {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80,
        Err(_) => true, // "-", "" and other non-addresses: nothing to flag
    }
}

/// Source address of a pf `filterlog` line. Wazuh's decoder mis-assigns fields for IPv6
/// (its `srcip` holds a port), so read the CSV directly: the source sits at index 18 for
/// IPv4 and 15 for IPv6.
fn filterlog_src(full_log: &str) -> Option<String> {
    let csv = full_log.split("]: ").nth(1)?;
    let f: Vec<&str> = csv.split(',').collect();
    let src = match *f.get(8)? {
        "4" => f.get(18)?,
        "6" => f.get(15)?,
        _ => return None,
    };
    src.parse::<std::net::IpAddr>().ok().map(|ip| ip.to_string())
}

/// Windows 4625 in words: "Failed logon on PC: user (wrong password, via app.exe from 1.2.3.4)".
fn failed_logon_text(agent: &str, win: &Value) -> String {
    let field = |k: &str| win[k].as_str().unwrap_or("").trim().to_string();
    let reason = match field("subStatus").to_lowercase().as_str() {
        "0xc000006a" => "wrong password".to_string(),
        "0xc0000064" => "unknown user".to_string(),
        "0xc0000072" => "account disabled".to_string(),
        "0xc0000234" => "account locked".to_string(),
        "0xc0000071" | "0xc0000224" => "password expired".to_string(),
        "0xc000006f" => "outside logon hours".to_string(),
        "0xc0000133" => "clock skew".to_string(),
        _ => format!("status {}", field("status")),
    };
    let mut detail = reason;
    // processName arrives with doubled backslashes; the last path segment is enough.
    let process = field("processName");
    if let Some(exe) = process.split('\\').rev().find(|x| !x.is_empty()).filter(|x| *x != "-") {
        detail.push_str(&format!(", via {exe}"));
    }
    let ip = field("ipAddress");
    if !ip.is_empty() && ip != "-" && ip != "127.0.0.1" && ip != "::1" {
        detail.push_str(&format!(" from {ip}"));
    }
    format!("Failed logon on {}: {} ({detail})", short_agent(agent), field("targetUserName"))
}

/// "ossec: Agent disconnected: 'OFFICE-PC-any'." -> "OFFICE-PC"
fn agent_from_log(log: &str) -> String {
    let inner = log.split('\'').nth(1).unwrap_or(log);
    inner.trim_end_matches("->any").trim_end_matches("-any").to_string()
}

fn short_agent(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// What one alert means for the widget.
enum Verdict {
    Event { level: Level, text: String, toast: bool, fold_key: Option<String> },
    Scan { src: String },
    Ignore,
}

/// Which agents matter, from the config.
struct Roles<'a> {
    router: Option<&'a str>,
    important: &'a [String],
}

impl Roles<'_> {
    fn is_router(&self, agent: &str) -> bool {
        self.router.is_some_and(|r| !r.is_empty() && agent.starts_with(r))
    }
    fn is_important(&self, agent: &str) -> bool {
        self.is_router(agent) || self.important.iter().any(|p| !p.is_empty() && agent.starts_with(p.as_str()))
    }
}

fn classify(a: &Value, roles: &Roles) -> Verdict {
    let rule = &a["rule"];
    let id = rule["id"].as_str().unwrap_or("");
    let agent = a["agent"]["name"].as_str().unwrap_or("?");
    let data = &a["data"];
    let win = &data["win"]["eventdata"];
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();

    if rule["groups"].as_array().is_some_and(|g| g.iter().any(|x| x == "ids")) && data["event_type"] == "alert" {
        let sev = data["alert"]["severity"].as_i64().unwrap_or(3);
        let sig = s(&data["alert"]["signature"]);
        let text = format!("IDS: {sig} ({} → {})", s(&data["src_ip"]), s(&data["dest_ip"]));
        // Shown and counted for the tile; only severity 1–2 is worth a toast.
        return Verdict::Event { level: Level::Warn, text, toast: sev <= 2, fold_key: None };
    }
    match id {
        "87702" => Verdict::Scan { src: filterlog_src(a["full_log"].as_str().unwrap_or("")).unwrap_or_else(|| "?".into()) },
        "100201" => Verdict::Event {
            level: Level::Alert,
            text: format!("Router health alert (free {} MB, WAN loss {})", s(&data["free_mb"]), s(&data["wan_loss"])),
            toast: true,
            fold_key: None,
        },
        "503" | "504" | "506" => {
            let who = agent_from_log(a["full_log"].as_str().unwrap_or(""));
            let important = roles.is_important(&who);
            let (level, verb) = match id {
                "503" => (Level::Info, "started"),
                "504" => (Level::Warn, "disconnected"),
                _ => (Level::Warn, "stopped"),
            };
            Verdict::Event {
                level: if important { level } else { Level::Info },
                text: format!("Wazuh agent {verb}: {}", short_agent(&who)),
                toast: important && id != "503",
                fold_key: None,
            }
        }
        "60122" | "60204" => Verdict::Event {
            level: Level::Warn,
            text: failed_logon_text(agent, win),
            toast: id == "60204", // 60204 = multiple failures
            fold_key: None,
        },
        "5710" | "5712" => Verdict::Event {
            level: Level::Warn,
            text: format!("SSH login attempt on {} as '{}' from {}", short_agent(agent), s(&data["srcuser"]), s(&data["srcip"])),
            toast: id == "5712",
            fold_key: None,
        },
        "92657" => {
            let ip = s(&win["ipAddress"]);
            if is_private(&ip) {
                Verdict::Ignore // routine LAN logons to the host (~150/day)
            } else {
                Verdict::Event {
                    level: Level::Alert,
                    text: format!("Remote logon to {} from {ip} as {}", short_agent(agent), s(&win["targetUserName"])),
                    toast: true,
                    fold_key: None,
                }
            }
        }
        "550" | "553" | "554" if roles.is_router(agent) => {
            let path = s(&a["syscheck"]["path"]);
            if ROUTINE_FILES.contains(&path.as_str()) {
                return Verdict::Ignore;
            }
            let verb = match id {
                "553" => "deleted",
                "554" => "added",
                _ => "changed",
            };
            Verdict::Event { level: Level::Info, text: format!("Router file {verb}: {path}"), toast: false, fold_key: Some(format!("fim:{path}")) }
        }
        "61107" => {
            let svc = s(&win["param1"]);
            Verdict::Event {
                level: Level::Info,
                text: format!("Service died on {}: {svc}", short_agent(agent)),
                toast: false,
                fold_key: Some(format!("svc:{agent}:{svc}")),
            }
        }
        _ => Verdict::Ignore,
    }
}

/// Inbound scan bursts (rule 87702) rolled up per clock hour.
#[derive(Default)]
struct ScanRollup {
    hour: Option<DateTime<Local>>,
    count: u32,
    sources: HashMap<String, u32>,
    emitted: HashSet<DateTime<Local>>,
    recent: VecDeque<DateTime<Local>>,
    /// Counts of the last 24 finished hours: the baseline.
    history: VecDeque<u32>,
}

/// A finished hour that stood out from the baseline.
struct NotableHour {
    end: DateTime<Local>,
    text: String,
    toast: bool,
}

impl ScanRollup {
    fn hour_of(t: DateTime<Local>) -> DateTime<Local> {
        t.with_minute(0).and_then(|t| t.with_second(0)).and_then(|t| t.with_nanosecond(0)).unwrap_or(t)
    }

    fn median(&self) -> f32 {
        let mut v: Vec<u32> = self.history.iter().copied().collect();
        v.sort_unstable();
        v.get(v.len() / 2).copied().unwrap_or(0) as f32
    }

    /// Advance the clock; returns the finished hour if it was unusually busy.
    fn tick(&mut self, now: DateTime<Local>) -> Option<NotableHour> {
        let h = Self::hour_of(now);
        let done = match self.hour {
            Some(cur) if h > cur && self.emitted.insert(cur) => {
                let base = self.median();
                let n = self.count;
                self.history.push_back(n);
                if self.history.len() > 24 {
                    self.history.pop_front();
                }
                let notable = self.history.len() > 3 && n >= SCAN_FLOOR && n as f32 > SCAN_NOTABLE * base.max(1.0);
                notable.then(|| {
                    let (top, c) = self.sources.iter().max_by_key(|e| e.1).map(|(k, v)| (k.clone(), *v)).unwrap_or_default();
                    let end = cur + ChronoDuration::hours(1);
                    NotableHour {
                        end,
                        text: format!(
                            "{n} inbound scan bursts {}–{} (usual ~{base:.0}; top {top} ×{c})",
                            cur.format("%H:%M"),
                            end.format("%H:%M")
                        ),
                        toast: n as f32 > SCAN_TOAST * base.max(1.0),
                    }
                })
            }
            _ => None,
        };
        if self.hour.is_none_or(|cur| h > cur) {
            self.hour = Some(h);
            self.count = 0;
            self.sources.clear();
        }
        done
    }

    fn add(&mut self, at: DateTime<Local>, src: String) {
        self.count += 1;
        *self.sources.entry(if src.is_empty() { "?".into() } else { src }).or_default() += 1;
        self.recent.push_back(at);
    }

    fn last_hour(&mut self, now: DateTime<Local>) -> u32 {
        while self.recent.front().is_some_and(|t| now - *t > ChronoDuration::hours(1)) {
            self.recent.pop_front();
        }
        self.recent.len() as u32
    }
}

struct Feed {
    cfg: WazuhConfig,
    state: Shared,
    toasts: bool,
    app_id: String,
    seen: HashSet<String>,
    folded: HashMap<String, DateTime<Local>>,
    scans: ScanRollup,
    ids: VecDeque<DateTime<Local>>,
}

impl Feed {
    fn handle(&mut self, line: &str) {
        let Ok(a) = serde_json::from_str::<Value>(line) else { return };
        if let Some(hb) = a["heartbeat"].as_str() {
            if let Some(t) = parse_ts(hb) {
                self.heard(t);
            }
            return;
        }
        let Some(at) = a["timestamp"].as_str().and_then(parse_ts) else { return };
        self.heard(at);
        // Reconnects replay today's alerts; skip the ones already processed.
        if !self.seen.insert(a["id"].as_str().unwrap_or("").to_string()) {
            return;
        }
        if self.seen.len() > 100_000 {
            self.seen.clear();
        }
        let fresh = Local::now() - at < ChronoDuration::minutes(TOAST_WINDOW_MIN);
        let roles = Roles { router: self.cfg.router_agent.as_deref(), important: &self.cfg.important_agents };
        match classify(&a, &roles) {
            Verdict::Scan { src } => self.scans.add(at, src),
            Verdict::Event { level, text, toast, fold_key } => {
                if let Some(key) = fold_key {
                    if self.folded.get(&key).is_some_and(|prev| at - *prev < ChronoDuration::hours(FOLD_HOURS)) {
                        return;
                    }
                    self.folded.insert(key, at);
                }
                if text.starts_with("IDS:") {
                    self.ids.push_back(at);
                }
                self.state.lock().unwrap().insert_event(at, Source::Security, level, text.clone());
                if toast && fresh && self.toasts {
                    notify::toast(&self.app_id, level, &text);
                }
            }
            Verdict::Ignore => {}
        }
    }

    /// Any alert or heartbeat: Wazuh is alive as of `t`; also closes finished scan hours.
    fn heard(&mut self, t: DateTime<Local>) {
        if let Some(n) = self.scans.tick(t) {
            self.state.lock().unwrap().insert_event(n.end, Source::Security, Level::Warn, n.text.clone());
            if n.toast && self.toasts && Local::now() - n.end < ChronoDuration::minutes(TOAST_WINDOW_MIN) {
                notify::toast(&self.app_id, Level::Warn, &n.text);
            }
        }
        let now = Local::now();
        while self.ids.front().is_some_and(|x| now - *x > ChronoDuration::hours(24)) {
            self.ids.pop_front();
        }
        let scans = self.scans.last_hour(now);
        let mut s = self.state.lock().unwrap();
        let w = s.wazuh.get_or_insert_with(WazuhInfo::default);
        w.connected = true;
        if w.last_heard.is_none_or(|l| t > l) {
            w.last_heard = Some(t);
        }
        w.scans_last_hour = scans;
        w.ids_last_day = self.ids.len() as u32;
    }

    fn run_once(&mut self) -> Result<(), String> {
        let mut cmd = Command::new("ssh");
        cmd.args(["-i", &self.cfg.key, "-o", "BatchMode=yes", "-o", "ConnectTimeout=6", "-o", "ServerAliveInterval=30"])
            .args(["-o", "StrictHostKeyChecking=accept-new", "-T"])
            .arg(&self.cfg.ssh)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| format!("ssh: {e}"))?;
        let out = child.stdout.take().ok_or("no stdout")?;
        for line in BufReader::new(out).lines() {
            match line {
                Ok(l) => self.handle(&l),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        Err("stream ended".into())
    }
}

pub fn spawn(cfg: WazuhConfig, state: Shared, ctx: eframe::egui::Context, toasts: bool, app_id: String) {
    thread::spawn(move || {
        let mut feed = Feed {
            cfg,
            state,
            toasts,
            app_id,
            seen: HashSet::new(),
            folded: HashMap::new(),
            scans: ScanRollup::default(),
            ids: VecDeque::new(),
        };
        // Repaint while streaming: the reader thread itself blocks on ssh.
        let repaint = ctx.clone();
        thread::spawn(move || loop {
            repaint.request_repaint();
            thread::sleep(Duration::from_secs(2));
        });
        loop {
            if let Err(e) = feed.run_once() {
                eprintln!("wazuh feed: {e}");
            }
            if let Some(w) = feed.state.lock().unwrap().wazuh.as_mut() {
                w.connected = false;
            }
            thread::sleep(Duration::from_secs(30));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    const IMPORTANT: [String; 0] = [];

    fn classify(a: &Value) -> Verdict {
        super::classify(a, &Roles { router: Some("fw"), important: &IMPORTANT })
    }

    #[test]
    fn hides_lan_logons_but_flags_external_ones() {
        let lan = alert(r#"{"rule":{"id":"92657"},"agent":{"name":"hv-host"},"data":{"win":{"eventdata":{"ipAddress":"192.168.1.42","targetUserName":"admin"}}}}"#);
        assert!(matches!(classify(&lan), Verdict::Ignore));
        let ext = alert(r#"{"rule":{"id":"92657"},"agent":{"name":"hv-host"},"data":{"win":{"eventdata":{"ipAddress":"203.0.113.9","targetUserName":"admin"}}}}"#);
        assert!(matches!(classify(&ext), Verdict::Event { level: Level::Alert, toast: true, .. }));
    }

    #[test]
    fn failed_logon_explains_itself() {
        let a = alert(
            r#"{"rule":{"id":"60122"},"agent":{"name":"OFFICE-PC"},"data":{"win":{"eventdata":{"targetUserName":"alice","subStatus":"0xc000006a","processName":"C:\\Program Files (x86)\\Microsoft\\EdgeWebView\\Application\\msedgewebview2.exe"}}}}"#,
        );
        match classify(&a) {
            Verdict::Event { text, .. } => {
                assert_eq!(text, "Failed logon on OFFICE-PC: alice (wrong password, via msedgewebview2.exe)")
            }
            _ => panic!("expected event"),
        }
        let remote = alert(r#"{"rule":{"id":"60122"},"agent":{"name":"hv-host"},"data":{"win":{"eventdata":{"targetUserName":"x","subStatus":"0xC0000064","processName":"-","ipAddress":"192.168.2.28"}}}}"#);
        match classify(&remote) {
            Verdict::Event { text, .. } => assert_eq!(text, "Failed logon on hv-host: x (unknown user from 192.168.2.28)"),
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn agent_events_name_the_agent() {
        let a = alert(r#"{"rule":{"id":"504"},"agent":{"name":"wazuh-server"},"full_log":"ossec: Agent disconnected: 'fw.home.arpa-any'."}"#);
        match classify(&a) {
            Verdict::Event { text, toast, level, .. } => {
                assert_eq!(text, "Wazuh agent disconnected: fw");
                assert!(toast);
                assert_eq!(level, Level::Warn);
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn suricata_anomaly_is_ignored_but_signature_alert_counts() {
        let anomaly = alert(r#"{"rule":{"id":"100107","groups":["suricata","ids"]},"data":{"event_type":"anomaly"}}"#);
        assert!(matches!(classify(&anomaly), Verdict::Ignore));
        let sig = alert(r#"{"rule":{"id":"100101","groups":["suricata","ids"]},"data":{"event_type":"alert","alert":{"severity":3,"signature":"ET SCAN x"}}}"#);
        assert!(matches!(classify(&sig), Verdict::Event { level: Level::Warn, toast: false, .. }));
    }

    #[test]
    fn scan_rollup_only_reports_unusual_hours() {
        let t = |h: u32, m: u32| Local::now().with_hour(h).unwrap().with_minute(m).unwrap();
        let mut r = ScanRollup::default();
        r.tick(t(1, 0));
        for h in 1..8 {
            for _ in 0..15 {
                r.add(t(h, 10), "1.2.3.4".into());
            }
            assert!(r.tick(t(h + 1, 0)).is_none(), "baseline hour {h} must stay quiet");
        }
        for _ in 0..60 {
            r.add(t(8, 10), "5.6.7.8".into());
        }
        let n = r.tick(t(9, 0)).expect("busy hour reported");
        assert!(n.text.starts_with("60 inbound scan bursts 08:00–09:00 (usual ~15; top 5.6.7.8 ×60)"), "{}", n.text);
        assert!(n.toast);
    }

    #[test]
    fn reads_filterlog_source_for_both_ip_versions() {
        let v6 = "Oct 20 00:03:41 fw.home.arpa filterlog[5874]: 9,,,02f4bab0,hn0,match,block,in,6,0x00,0x890a7,47,tcp,6,40,2001:db8:5cc0::1,2001:db8:1::2,443,27374,0";
        assert_eq!(filterlog_src(v6).as_deref(), Some("2001:db8:5cc0::1"));
        let v4 = "Oct 20 00:03:41 fw.home.arpa filterlog[5874]: 8,,,02f4bab0,hn0,match,block,in,4,0x0,,114,14014,0,none,17,udp,132,198.51.100.140,192.0.2.46,59000,22752,112";
        assert_eq!(filterlog_src(v4).as_deref(), Some("198.51.100.140"));
    }

    #[test]
    fn parses_wazuh_timestamps() {
        assert!(parse_ts("2026-10-21T00:03:21.768+0000").is_some());
    }
}
