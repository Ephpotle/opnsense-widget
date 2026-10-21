use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const HISTORY_LEN: usize = 150; // 5 min at the default 2 s poll
const EVENTS_LEN: usize = 200;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    #[default]
    Ok,
    Info,
    Warn,
    Alert,
}

/// Where an event came from; decides which card lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// OPNsense REST API and the PC-side internet probe.
    Router,
    /// Hyper-V host event log: crashes, restarts, VM start/stop.
    Host,
    /// Wazuh alerts.
    Security,
    /// This PC's own networking (port exhaustion etc.).
    Pc,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub at: chrono::DateTime<chrono::Local>,
    pub source: Source,
    pub level: Level,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Service {
    pub id: String,
    pub running: bool,
}

#[derive(Clone, Debug)]
pub struct Talker {
    pub addr: String,
    pub name: Option<String>,
    pub bps_in: f64,
    pub bps_out: f64,
}

/// Everything the widget knows about the router. Written by the poller, read by the UI.
#[derive(Default)]
pub struct State {
    pub config_error: Option<String>,
    pub connected: bool,
    pub last_ok: Option<Instant>,
    pub api_ms: Option<u32>,

    pub hostname: String,
    pub version: String,
    pub uptime: String,
    pub loadavg: String,

    pub cpu_pct: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub states: u64,
    pub states_limit: u64,
    pub mbuf_failures: u64,

    pub wan_rx_bps: f64,
    pub wan_tx_bps: f64,
    pub lan_rx_bps: f64,
    pub lan_tx_bps: f64,
    pub lan_txdrops_per_min: f64,
    /// (download, upload) bits/s on WAN, oldest first.
    pub history: VecDeque<(f64, f64)>,

    pub wan_ip: String,
    pub lan_prefix6: String,
    /// Unsolicited inbound packets blocked on WAN, per minute (from the pf log).
    pub blocked_per_min: Option<f64>,
    pub device_count: usize,

    pub internet_rtt_ms: Option<f32>,
    pub internet_loss_pct: f32,
    pub internet_down: bool,
    /// Nearest broken hop while the internet is down ("the ISP link (gateway not answering)").
    pub outage_cause: Option<String>,
    /// ICMP round trips from this PC to the router and to the ISP's gateway.
    pub router_rtt_ms: Option<f32>,
    pub gateway_rtt_ms: Option<f32>,
    /// ISP-side gateway address of the WAN (from OPNsense's gateway list).
    pub gateway_ip: Option<std::net::Ipv4Addr>,
    /// Last DNS lookup through the router succeeded; `dns_failing` = for several rounds.
    pub dns_ok: Option<bool>,
    pub dns_failing: bool,

    pub iptv: Option<IptvInfo>,
    pub firmware: Option<FirmwareInfo>,

    pub services: Vec<Service>,
    pub ids_running: Option<bool>,
    pub top_lan: Vec<Talker>,

    /// Hyper-V host, read over SSH. None until the first successful query.
    pub host: Option<HostInfo>,
    /// Wazuh feed; None when not configured.
    pub wazuh: Option<WazuhInfo>,
    pub pc: Option<PcInfo>,

    pub problems: Vec<(Level, String)>,
    pub events: VecDeque<Event>,
}

impl State {
    pub fn health(&self) -> Level {
        self.problems.iter().map(|p| p.0).max().unwrap_or(Level::Ok)
    }

    pub fn push_event(&mut self, level: Level, text: impl Into<String>) {
        self.insert_event(chrono::Local::now(), Source::Router, level, text);
    }

    /// Insert keeping newest-first order; host events arrive with their own (past) timestamps.
    pub fn insert_event(&mut self, at: chrono::DateTime<chrono::Local>, source: Source, level: Level, text: impl Into<String>) {
        let pos = self.events.iter().position(|e| e.at <= at).unwrap_or(self.events.len());
        self.events.insert(pos, Event { at, source, level, text: text.into() });
        self.events.truncate(EVENTS_LEN);
    }

    pub fn push_history(&mut self, rx: f64, tx: f64) {
        self.history.push_back((rx, tx));
        while self.history.len() > HISTORY_LEN {
            self.history.pop_front();
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HostInfo {
    pub reachable: bool,
    pub boot: Option<chrono::DateTime<chrono::Local>>,
    pub vm_state: String,
    pub vm_uptime_s: u64,
    /// Most recent unexpected host shutdown (Kernel-Power 41).
    pub last_crash: Option<chrono::DateTime<chrono::Local>>,
    /// Disk / storage / WHEA hardware errors in the last 7 days.
    pub hw_errors_7d: u32,
}

#[derive(Clone, Debug, Default)]
pub struct IptvInfo {
    pub running: bool,
    /// Multicast (TV) traffic on the LAN right now, bits/s; None if top/lan showed none.
    pub stream_bps: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct FirmwareInfo {
    /// 26.1.x packages waiting (patch updates within the installed series).
    pub patches: usize,
    /// Major upgrade on offer (e.g. "26.7"): the installed series is end-of-life.
    pub major: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct WazuhInfo {
    pub connected: bool,
    /// Newest alert/heartbeat timestamp seen: Wazuh's own liveness.
    pub last_heard: Option<chrono::DateTime<chrono::Local>>,
    pub scans_last_hour: u32,
    pub ids_last_day: u32,
}

#[derive(Clone, Debug, Default)]
pub struct PcInfo {
    pub udp_in_use: usize,
    /// Biggest UDP socket holders (name, count), filled only when usage is notable.
    pub top: Vec<(String, usize)>,
    pub last_exhaustion: Option<chrono::DateTime<chrono::Local>>,
}

pub type Shared = Arc<Mutex<State>>;
