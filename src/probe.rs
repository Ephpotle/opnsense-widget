//! Connection probe, every 5 s from this PC: TCP to well-known anycast hosts (the internet),
//! ICMP to the router and to the ISP's gateway, and a DNS lookup through the router.
//! The hops tell *where* a problem is (this PC, the router/LAN, the ISP link, or beyond),
//! and every outage, DNS failure or degraded stretch is logged with its duration, even short
//! ones, so a "hiccup" leaves a trace.

use crate::config::Config;
use crate::model::{Level, Shared};
use crate::notify;
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

const ROUND: Duration = Duration::from_secs(5);
/// Rounds of total internet failure before the live "unreachable" alert and toast.
const DOWN_ROUNDS: u32 = 3;
/// Clean rounds that end a degraded stretch.
const CLEAN_TO_END: u32 = 2;

/// One probe round's raw results.
#[derive(Clone, Debug, Default)]
pub struct Round {
    /// Per internet target: connect time in ms, or None on failure.
    pub inet: Vec<Option<f32>>,
    pub router_ms: Option<f32>,
    /// None when the gateway is unknown; Some(None) when it did not answer.
    pub gateway_ms: Option<Option<f32>>,
    pub dns_ok: Option<bool>,
    /// The PC could not even open a UDP socket (ephemeral ports exhausted).
    pub pc_ports_out: bool,
}

impl Round {
    fn inet_ok(&self) -> usize {
        self.inet.iter().filter(|r| r.is_some()).count()
    }
    fn best_rtt(&self) -> Option<f32> {
        self.inet.iter().flatten().copied().reduce(f32::min)
    }
}

/// Where a failing round points. Order matters: the nearest broken hop wins.
pub fn cause(r: &Round, pc_ports_low: bool) -> &'static str {
    if r.pc_ports_out || pc_ports_low {
        "this PC (out of UDP ports)"
    } else if r.router_ms.is_none() {
        "this PC ↔ router (LAN)"
    } else if matches!(r.gateway_ms, Some(None)) {
        "the ISP link (gateway not answering)"
    } else if r.gateway_ms.is_some() {
        "the ISP, beyond its gateway"
    } else {
        "beyond the router"
    }
}

#[derive(Default)]
struct Episode {
    started: Option<Instant>,
    bad_rounds: u32,
    clean_streak: u32,
    peak_ms: f32,
    lost: usize,
    cause: Option<&'static str>,
    alerted: bool,
}

impl Episode {
    fn active(&self) -> bool {
        self.started.is_some()
    }
    fn begin(&mut self) {
        *self = Episode { started: Some(Instant::now()), ..Default::default() };
    }
    fn secs(&self) -> u64 {
        self.started.map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }
}

/// Median of the recent good RTTs: what "normal" latency is right now.
fn median(v: &VecDeque<f32>) -> Option<f32> {
    if v.len() < 6 {
        return None;
    }
    let mut s: Vec<f32> = v.iter().copied().collect();
    s.sort_by(f32::total_cmp);
    Some(s[s.len() / 2])
}

pub fn fmt_secs(s: u64) -> String {
    match s {
        0..=89 => format!("{s} s"),
        90..=5399 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s / 60 % 60),
    }
}

pub fn spawn(cfg: Config, state: Shared, ctx: eframe::egui::Context) {
    let targets: Vec<SocketAddr> = cfg.probe_targets.iter().filter_map(|t| t.parse().ok()).collect();
    if targets.is_empty() {
        return;
    }
    let router_ip: Option<Ipv4Addr> =
        cfg.url.trim_start_matches("https://").trim_start_matches("http://").split(['/', ':']).next().and_then(|h| h.parse().ok());
    thread::spawn(move || {
        let toast = |lvl: Level, text: &str| {
            if cfg.toasts {
                notify::toast(&cfg.toast_app_id, lvl, text);
            }
        };
        let mut window: VecDeque<bool> = VecDeque::new();
        let mut good_rtts: VecDeque<f32> = VecDeque::new();
        let mut gw_rtts: VecDeque<f32> = VecDeque::new();
        let mut outage = Episode::default();
        let mut dns = Episode::default();
        let mut degraded = Episode::default();
        loop {
            let started = Instant::now();
            let gateway = state.lock().unwrap().gateway_ip;
            let r = run_round(&targets, router_ip, gateway);
            let pc_ports_low = state.lock().unwrap().pc.as_ref().is_some_and(|p| p.udp_in_use * 100 / crate::pcnet::POOL >= 95);
            for x in &r.inet {
                window.push_back(x.is_some());
            }
            while window.len() > 12 * targets.len() {
                window.pop_front();
            }
            let loss = window.iter().filter(|ok| !**ok).count() as f32 * 100.0 / window.len().max(1) as f32;
            let baseline = median(&good_rtts);
            let gw_baseline = median(&gw_rtts);
            let rtt = r.best_rtt();
            let gw_rtt = r.gateway_ms.flatten();
            let mut events: Vec<(Level, String, bool)> = Vec::new();

            // Total outage: no internet target answered.
            if r.inet_ok() == 0 {
                if !outage.active() {
                    outage.begin();
                }
                outage.bad_rounds += 1;
                outage.cause.get_or_insert(cause(&r, pc_ports_low));
                if outage.bad_rounds == DOWN_ROUNDS && !outage.alerted {
                    outage.alerted = true;
                    let text = format!("Internet unreachable: {}", outage.cause.unwrap_or("?"));
                    events.push((Level::Alert, text, true));
                }
            } else if outage.active() {
                let secs = outage.secs().max(ROUND.as_secs());
                let why = outage.cause.unwrap_or("?");
                if outage.alerted {
                    events.push((Level::Info, format!("Internet back after {} (was {why})", fmt_secs(secs)), true));
                } else {
                    let lvl = if secs >= 60 { Level::Alert } else { Level::Warn };
                    events.push((lvl, format!("Internet dropped for {}: {why}", fmt_secs(secs)), false));
                }
                outage = Episode::default();
            }

            // DNS through the router failing while the internet itself answers.
            match r.dns_ok {
                Some(false) if r.inet_ok() > 0 => {
                    if !dns.active() {
                        dns.begin();
                    }
                    dns.bad_rounds += 1;
                    if dns.bad_rounds == DOWN_ROUNDS {
                        events.push((Level::Warn, "DNS through the router is failing".into(), true));
                        dns.alerted = true;
                    }
                }
                Some(true) if dns.active() => {
                    let secs = dns.secs().max(ROUND.as_secs());
                    let text = if r.pc_ports_out || pc_ports_low {
                        format!("DNS failed for {} (this PC out of UDP ports)", fmt_secs(secs))
                    } else {
                        format!("DNS through the router failed for {}", fmt_secs(secs))
                    };
                    events.push((if dns.alerted { Level::Info } else { Level::Warn }, text, dns.alerted));
                    dns = Episode::default();
                }
                _ => {}
            }

            // Degraded: some targets lost, or latency well above normal.
            let partial = r.inet_ok() > 0 && r.inet_ok() < r.inet.len();
            let spike = match (rtt, baseline) {
                (Some(x), Some(b)) => x > (3.0 * b).max(100.0),
                _ => false,
            };
            if r.inet_ok() > 0 && (partial || spike) {
                if !degraded.active() {
                    degraded.begin();
                }
                degraded.bad_rounds += 1;
                degraded.clean_streak = 0;
                degraded.peak_ms = degraded.peak_ms.max(rtt.unwrap_or(0.0));
                degraded.lost += r.inet.len() - r.inet_ok();
                // Gateway slow too: the congestion is on the ISP link / uplink, not further out.
                let gw_slow = match (gw_rtt, gw_baseline) {
                    (Some(g), Some(b)) => g > (3.0 * b).max(30.0),
                    _ => false,
                };
                let router_slow = r.router_ms.is_some_and(|x| x > 50.0);
                degraded.cause = Some(if router_slow {
                    "this PC ↔ router (LAN)"
                } else if gw_slow {
                    "the ISP link / uplink"
                } else {
                    "beyond the ISP gateway"
                });
            } else if degraded.active() && r.inet_ok() > 0 {
                degraded.clean_streak += 1;
                if degraded.clean_streak >= CLEAN_TO_END {
                    if degraded.bad_rounds >= 2 {
                        let secs = (degraded.secs().saturating_sub(CLEAN_TO_END as u64 * ROUND.as_secs())).max(ROUND.as_secs());
                        let mut what = Vec::new();
                        if degraded.peak_ms > 0.0 {
                            what.push(format!("up to {:.0} ms", degraded.peak_ms));
                        }
                        if degraded.lost > 0 {
                            what.push(format!("{} lost probes", degraded.lost));
                        }
                        events.push((
                            Level::Warn,
                            format!("Connection degraded for {}: {} ({})", fmt_secs(secs), what.join(", "), degraded.cause.unwrap_or("?")),
                            false,
                        ));
                    }
                    degraded = Episode::default();
                }
            }

            if let Some(x) = rtt.filter(|_| !spike) {
                good_rtts.push_back(x);
                if good_rtts.len() > 60 {
                    good_rtts.pop_front();
                }
            }
            if let Some(g) = gw_rtt {
                gw_rtts.push_back(g);
                if gw_rtts.len() > 60 {
                    gw_rtts.pop_front();
                }
            }

            {
                let mut s = state.lock().unwrap();
                s.internet_loss_pct = loss;
                s.internet_rtt_ms = rtt;
                s.internet_down = outage.bad_rounds >= DOWN_ROUNDS;
                s.router_rtt_ms = r.router_ms;
                s.gateway_rtt_ms = gw_rtt;
                s.dns_ok = r.dns_ok;
                s.outage_cause = outage.cause.filter(|_| s.internet_down).map(String::from);
                s.dns_failing = dns.bad_rounds >= DOWN_ROUNDS;
                for (lvl, text, _) in &events {
                    s.push_event(*lvl, text.clone());
                }
            }
            for (lvl, text, t) in &events {
                if *t {
                    toast(*lvl, text);
                }
            }
            ctx.request_repaint();
            thread::sleep(ROUND.saturating_sub(started.elapsed()));
        }
    });
}

fn run_round(targets: &[SocketAddr], router: Option<Ipv4Addr>, gateway: Option<Ipv4Addr>) -> Round {
    let mut r = Round::default();
    for t in targets {
        let t0 = Instant::now();
        r.inet.push(TcpStream::connect_timeout(t, Duration::from_secs(2)).ok().map(|_| t0.elapsed().as_secs_f32() * 1000.0));
    }
    r.router_ms = router.and_then(|ip| ping(ip, 1000));
    r.gateway_ms = gateway.map(|ip| ping(ip, 1000));
    if let Some(ip) = router {
        match dns_lookup(ip) {
            Ok(ok) => r.dns_ok = Some(ok),
            Err(_) => {
                r.pc_ports_out = true;
                r.dns_ok = Some(false);
            }
        }
    }
    r
}

/// ICMP echo through the Windows ICMP helper (no admin rights needed). Round trip in ms.
fn ping(ip: Ipv4Addr, timeout_ms: u32) -> Option<f32> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{ICMP_ECHO_REPLY, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho};
    let data = *b"router-widget";
    let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + data.len() + 8];
    unsafe {
        let h = IcmpCreateFile();
        if h.is_null() || h as isize == -1 {
            return None;
        }
        let n = IcmpSendEcho(
            h,
            u32::from_ne_bytes(ip.octets()),
            data.as_ptr().cast(),
            data.len() as u16,
            std::ptr::null(),
            reply.as_mut_ptr().cast(),
            reply.len() as u32,
            timeout_ms,
        );
        IcmpCloseHandle(h);
        let rep = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
        (n > 0 && rep.Status == 0).then_some(rep.RoundTripTime as f32)
    }
}

/// One A lookup through the router's resolver. Err = this PC could not open a UDP socket.
fn dns_lookup(server: Ipv4Addr) -> Result<bool, std::io::Error> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let id = (nanos >> 8) as u16 ^ std::process::id() as u16;
    let q = dns_query(id, "cloudflare.com");
    if sock.send_to(&q, (server, 53)).is_err() {
        return Ok(false);
    }
    let mut buf = [0u8; 512];
    Ok(match sock.recv(&mut buf) {
        Ok(n) => dns_answer_ok(&buf[..n], id),
        Err(_) => false,
    })
}

fn dns_query(id: u16, name: &str) -> Vec<u8> {
    let mut q = Vec::with_capacity(32);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]); // RD, 1 question
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]); // root, type A, class IN
    q
}

/// Matching id, a response, RCODE 0 and at least one answer.
fn dns_answer_ok(b: &[u8], id: u16) -> bool {
    b.len() >= 12 && u16::from_be_bytes([b[0], b[1]]) == id && b[2] & 0x80 != 0 && b[3] & 0x0f == 0 && u16::from_be_bytes([b[6], b[7]]) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_dns_query() {
        let q = dns_query(0x1234, "a.bc");
        assert_eq!(q, vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, b'a', 2, b'b', b'c', 0, 0, 1, 0, 1]);
    }

    #[test]
    fn checks_dns_answer() {
        let mut ok = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        assert!(dns_answer_ok(&ok, 0x1234));
        assert!(!dns_answer_ok(&ok, 0x9999));
        ok[3] = 0x83; // NXDOMAIN
        assert!(!dns_answer_ok(&ok, 0x1234));
    }

    #[test]
    fn nearest_broken_hop_is_blamed() {
        let mut r = Round { inet: vec![None, None], router_ms: Some(1.0), gateway_ms: Some(Some(3.0)), dns_ok: Some(true), pc_ports_out: false };
        assert_eq!(cause(&r, false), "the ISP, beyond its gateway");
        r.gateway_ms = Some(None);
        assert_eq!(cause(&r, false), "the ISP link (gateway not answering)");
        r.router_ms = None;
        assert_eq!(cause(&r, false), "this PC ↔ router (LAN)");
        assert_eq!(cause(&r, true), "this PC (out of UDP ports)");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_secs(12), "12 s");
        assert_eq!(fmt_secs(125), "2m 05s");
        assert_eq!(fmt_secs(7300), "2h 01m");
    }
}
