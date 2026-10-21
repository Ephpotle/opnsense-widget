//! "This PC" checks: is a connection hiccup the network, or this machine? Watches the UDP
//! ephemeral port pool (exhaustion breaks DNS, so every new connection fails while the
//! router is fine) and reads Windows' own exhaustion events (Tcpip 4266).

use crate::model::{Level, PcInfo, Shared, Source};
use crate::notify;
use chrono::{DateTime, Local};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedUdpTable, UDP_TABLE_OWNER_PID};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};

/// Windows' dynamic UDP range (49152–65535); part of it is reserved by Hyper-V/WinNAT.
pub const POOL: usize = 16384;
const NOTE_PCT: usize = 25;
const WARN_PCT: usize = 50;
const TOAST_PCT: usize = 75;
/// Exhaustion bursts can be short; sample often (one cheap iphlpapi call).
const SAMPLE: Duration = Duration::from_secs(5);
const EVENTLOG_EVERY: u32 = 6; // samples, i.e. every 30 s

/// UDP sockets per owning PID, IPv4 + IPv6.
fn udp_by_pid() -> HashMap<u32, usize> {
    let mut out = HashMap::new();
    // Row sizes of MIB_UDPROW_OWNER_PID / MIB_UDP6ROW_OWNER_PID; the PID is the last u32.
    for (family, row) in [(AF_INET as u32, 12usize), (AF_INET6 as u32, 28usize)] {
        let mut size = 0u32;
        unsafe { GetExtendedUdpTable(std::ptr::null_mut(), &mut size, 0, family, UDP_TABLE_OWNER_PID, 0) };
        // The table can grow between the size query and the read.
        let mut buf = vec![0u8; size as usize + 4096];
        size = buf.len() as u32;
        if unsafe { GetExtendedUdpTable(buf.as_mut_ptr().cast(), &mut size, 0, family, UDP_TABLE_OWNER_PID, 0) } != 0 {
            continue;
        }
        let n = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize;
        for i in 0..n {
            let end = 4 + (i + 1) * row;
            if end > buf.len() {
                break;
            }
            let pid = u32::from_ne_bytes(buf[end - 4..end].try_into().unwrap());
            *out.entry(pid).or_insert(0) += 1;
        }
    }
    out
}

fn process_name(pid: u32) -> String {
    if pid == 4 {
        return "System".into();
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return format!("pid {pid}");
        }
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        if !ok {
            return format!("pid {pid}");
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().unwrap_or(&path).trim_end_matches(".exe").to_string()
    }
}

/// "chrome 9120, svchost 26" — the top holders, by name.
fn top_holders(counts: &HashMap<u32, usize>, n: usize) -> Vec<(String, usize)> {
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (pid, c) in counts {
        *by_name.entry(process_name(*pid)).or_default() += c;
    }
    let mut v: Vec<_> = by_name.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(n);
    v
}

fn describe(top: &[(String, usize)]) -> String {
    top.iter().map(|(name, c)| format!("{name} {c}")).collect::<Vec<_>>().join(", ")
}

/// Timestamps of recent Tcpip 4266 ("all UDP ports in use") events from the System log.
fn exhaustion_events(count: u32) -> Vec<DateTime<Local>> {
    let mut cmd = Command::new("wevtutil");
    cmd.args([
        "qe",
        "System",
        "/q:*[System[Provider[@Name='Tcpip'] and (EventID=4266)]]",
        &format!("/c:{count}"),
        "/rd:true",
        "/f:text",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(out) = cmd.output() else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().strip_prefix("Date:"))
        .filter_map(|d| parse_wevtutil_date(d.trim()))
        .collect()
}

/// wevtutil's text format prints *local* time but appends a misleading "Z".
fn parse_wevtutil_date(d: &str) -> Option<DateTime<Local>> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(d.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    Local.from_local_datetime(&naive).earliest()
}

pub fn spawn(state: Shared, ctx: eframe::egui::Context, toasts: bool, app_id: String) {
    thread::spawn(move || {
        let mut seen: HashSet<DateTime<Local>> = HashSet::new();
        let mut first = true;
        let mut band = 0usize; // last reported usage band, for hysteresis
        let mut tick = 0u32;
        loop {
            let counts = udp_by_pid();
            let total: usize = counts.values().sum();
            let pct = total * 100 / POOL;
            let new_band = [TOAST_PCT, WARN_PCT, NOTE_PCT].into_iter().find(|b| pct >= *b).unwrap_or(0);
            let top = if pct >= NOTE_PCT { top_holders(&counts, 3) } else { Vec::new() };
            if new_band > band {
                let (level, toast) = match new_band {
                    TOAST_PCT => (Level::Alert, true),
                    WARN_PCT => (Level::Warn, false),
                    _ => (Level::Info, false),
                };
                let text = format!("This PC is using {pct}% of its UDP ports ({total}): {}", describe(&top));
                state.lock().unwrap().insert_event(Local::now(), Source::Pc, level, text.clone());
                if toast && toasts {
                    notify::toast(&app_id, level, &text);
                }
            } else if new_band == 0 && band > 0 {
                state.lock().unwrap().insert_event(Local::now(), Source::Pc, Level::Info, format!("PC UDP ports back to normal ({total})"));
            }
            band = new_band;

            if tick % EVENTLOG_EVERY == 0 {
                const TEXT: &str = "This PC ran out of UDP ports: DNS/new connections fail (Tcpip 4266)";
                let events = exhaustion_events(if first { 60 } else { 5 });
                if first {
                    // History: the newest one as a row, the rest folded into one summary line.
                    let mut sorted = events.clone();
                    sorted.sort();
                    if let Some(newest) = sorted.last() {
                        state.lock().unwrap().insert_event(*newest, Source::Pc, Level::Alert, TEXT);
                    }
                    if sorted.len() > 1 {
                        let text = format!(
                            "…and {} more times since {} (Windows logs once per episode)",
                            sorted.len() - 1,
                            sorted[0].format("%b %d")
                        );
                        state.lock().unwrap().insert_event(sorted[sorted.len() - 2], Source::Pc, Level::Warn, text);
                    }
                    seen.extend(sorted);
                } else {
                    for at in events {
                        if seen.insert(at) {
                            // Name whoever holds the most ports right now: the culprit may
                            // still be mid-burst when Windows logs the event.
                            let holders = describe(&top_holders(&counts, 3));
                            let text = format!("{TEXT} · {total} in use now: {holders}");
                            state.lock().unwrap().insert_event(at, Source::Pc, Level::Alert, text.clone());
                            if toasts {
                                notify::toast(&app_id, Level::Alert, &text);
                            }
                        }
                    }
                }
                first = false;
            }
            {
                let mut s = state.lock().unwrap();
                let pc = s.pc.get_or_insert_with(PcInfo::default);
                pc.udp_in_use = total;
                pc.top = top;
                pc.last_exhaustion = seen.iter().max().copied();
            }
            ctx.request_repaint();
            tick += 1;
            thread::sleep(SAMPLE);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_this_process_sockets() {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let counts = udp_by_pid();
        assert!(counts.get(&std::process::id()).copied().unwrap_or(0) >= 1);
        drop(sock);
    }

    #[test]
    fn wevtutil_dates_are_local_despite_the_z() {
        use chrono::Timelike;
        let t = parse_wevtutil_date("2026-10-21T09:02:27.3850000Z").unwrap();
        assert_eq!((t.hour(), t.minute()), (9, 2));
    }

    #[test]
    fn names_own_process() {
        assert!(process_name(std::process::id()).starts_with("router_widget"));
    }
}
