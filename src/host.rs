//! Hyper-V host feed: one PowerShell query over SSH per poll. Covers what the router's own
//! API cannot see: host power loss / crashes (Kernel-Power 41), VM resets (18512), and the
//! host's disk / storage / WHEA hardware errors, which tend to precede its crashes.

use crate::config::HostConfig;
use crate::model::{HostInfo, Level, Shared, Source};
use crate::notify;
use base64::Engine;
use chrono::{DateTime, Local};
use serde::Deserialize;
use std::collections::HashSet;
use std::process::Command;
use std::thread;
use std::time::Duration;

const QUERY: &str = r#"
$ErrorActionPreference='SilentlyContinue'; $ProgressPreference='SilentlyContinue'
$since=(Get-Date).AddDays(-__DAYS__)
$vm=Get-VM -Name '__VM__'
$ev=@()
Get-WinEvent -FilterHashtable @{LogName='System'; Id=41,1074; StartTime=$since} | % {
  $d=@{}; ([xml]$_.ToXml()).Event.EventData.Data | % { $d[$_.Name]=$_.'#text' }
  $ev += [pscustomobject]@{ key="S$($_.RecordId)"; t=$_.TimeCreated.ToString('o'); id=$_.Id
    bugcheck=[int64]$d.BugcheckCode } }
Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Microsoft-Windows-WHEA-Logger','disk','storahci','stornvme','Ntfs','volmgr'; Level=1,2,3; StartTime=$since} | % {
  $ev += [pscustomobject]@{ key="H$($_.RecordId)"; t=$_.TimeCreated.ToString('o'); id=$_.Id; bugcheck=0
    src=$_.ProviderName; msg=(($_.Message -split "`n")[0]).Trim() } }
Get-WinEvent -FilterHashtable @{LogName='Microsoft-Windows-Hyper-V-Worker-Admin'; Id=18500,18502,18504,18512,18560,18590; StartTime=$since} |
  ? { $_.Message -match "'__VM__'" } | % {
  $ev += [pscustomobject]@{ key="W$($_.RecordId)"; t=$_.TimeCreated.ToString('o'); id=$_.Id; bugcheck=0 } }
[pscustomobject]@{ boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o')
  vm_state="$($vm.State)"; vm_uptime_s=[int64]$vm.Uptime.TotalSeconds; events=@($ev) } | ConvertTo-Json -Compress -Depth 4
"#;

#[derive(Deserialize)]
struct Reply {
    boot: String,
    vm_state: String,
    vm_uptime_s: u64,
    #[serde(default)]
    events: Vec<RawEvent>,
}

#[derive(Deserialize)]
struct RawEvent {
    key: String,
    t: String,
    id: u32,
    #[serde(default)]
    bugcheck: i64,
    /// Event provider, set for the hardware events only.
    #[serde(default)]
    src: String,
    #[serde(default)]
    msg: String,
}

/// "\Device\Harddisk3\DR3" in a disk event's message -> "disk 3".
fn disk_name(msg: &str) -> String {
    msg.split("Harddisk")
        .nth(1)
        .map(|r| r.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .filter(|n| !n.is_empty())
        .map(|n| format!("disk {n}"))
        .unwrap_or_else(|| "a disk".into())
}

/// Host hardware events (disk, storage controller, NTFS, WHEA). None = not worth showing.
fn classify_hw(e: &RawEvent) -> Option<(Level, String)> {
    let disk = || disk_name(&e.msg);
    Some(match (e.src.as_str(), e.id) {
        ("Microsoft-Windows-WHEA-Logger", 1 | 18) => (Level::Alert, format!("Host: fatal hardware error recorded (WHEA {})", e.id)),
        ("Microsoft-Windows-WHEA-Logger", id) => (Level::Warn, format!("Host: corrected hardware error (WHEA {id})")),
        ("disk", 7) => (Level::Alert, format!("Host {}: bad block", disk())),
        ("disk", 51) => (Level::Warn, format!("Host {}: I/O error during paging", disk())),
        ("disk", 153) => (Level::Warn, format!("Host {}: I/O had to be retried", disk())),
        ("disk", 157) => (Level::Warn, format!("Host {} was surprise-removed", disk())),
        ("storahci" | "stornvme", 129) => (Level::Warn, "Host storage controller reset".into()),
        ("Ntfs", 55 | 98) => (Level::Alert, format!("Host file system corruption (NTFS {})", e.id)),
        ("volmgr", 161) => (Level::Info, "Host could not write its crash dump".into()),
        _ => return None,
    })
}

/// What a host event means for us. None = not worth showing.
fn classify(e: &RawEvent, vm: &str) -> Option<(Level, String)> {
    if !e.src.is_empty() {
        return classify_hw(e);
    }
    Some(match e.id {
        41 if e.bugcheck == 0 => (Level::Alert, "Host stopped abruptly (no bugcheck, no dump)".into()),
        41 => (Level::Alert, format!("Host crashed (bugcheck 0x{:X})", e.bugcheck)),
        1074 => (Level::Info, "Planned host restart/shutdown".into()),
        18512 => (Level::Alert, format!("{vm} VM was reset (guest hang)")),
        18560 | 18590 => (Level::Alert, format!("{vm} VM crashed (event {})", e.id)),
        18502 => (Level::Warn, format!("{vm} VM turned off")),
        18504 => (Level::Info, format!("{vm} VM shut down")),
        18500 => (Level::Info, format!("{vm} VM started")),
        _ => return None,
    })
}

fn parse_time(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Local))
}

fn run_query(cfg: &HostConfig) -> Result<Reply, String> {
    let script = QUERY.replace("__DAYS__", &cfg.backfill_days.to_string()).replace("__VM__", &cfg.vm);
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let b64 = base64::engine::general_purpose::STANDARD.encode(utf16);
    let mut cmd = Command::new("ssh");
    cmd.args(["-i", &cfg.key, "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-o", "StrictHostKeyChecking=accept-new"])
        .arg(&cfg.ssh)
        .arg(format!("powershell -NoProfile -NonInteractive -EncodedCommand {b64}"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash from the GUI build
    }
    let out = cmd.output().map_err(|e| format!("ssh: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.lines().find(|l| l.trim_start().starts_with('{')).ok_or_else(|| {
        format!("ssh exit {}: {}", out.status, String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or(""))
    })?;
    serde_json::from_str(json).map_err(|e| format!("host json: {e}"))
}

pub fn spawn(cfg: HostConfig, state: Shared, ctx: eframe::egui::Context, toasts: bool, app_id: String) {
    thread::spawn(move || {
        let mut seen: HashSet<String> = HashSet::new();
        let mut first = true;
        loop {
            match run_query(&cfg) {
                Ok(r) => {
                    let mut fresh = Vec::new();
                    let mut last_crash = None;
                    let mut hw_errors_7d = 0;
                    let week_ago = chrono::Local::now() - chrono::Duration::days(7);
                    for e in &r.events {
                        let Some(at) = parse_time(&e.t) else { continue };
                        if e.src.is_empty() && e.id == 41 && last_crash.is_none_or(|c| at > c) {
                            last_crash = Some(at);
                        }
                        if at > week_ago && classify_hw(e).is_some_and(|(l, _)| l >= Level::Warn) && !e.src.is_empty() {
                            hw_errors_7d += 1;
                        }
                        if seen.insert(e.key.clone()) {
                            if let Some((lvl, text)) = classify(e, &cfg.vm) {
                                fresh.push((at, lvl, text));
                            }
                        }
                    }
                    let mut s = state.lock().unwrap();
                    s.host = Some(HostInfo {
                        reachable: true,
                        boot: parse_time(&r.boot),
                        vm_state: r.vm_state,
                        vm_uptime_s: r.vm_uptime_s,
                        last_crash,
                        hw_errors_7d,
                    });
                    for (at, lvl, text) in &fresh {
                        s.insert_event(*at, Source::Host, *lvl, text.clone());
                    }
                    drop(s);
                    // History loaded at startup is shown but not toasted.
                    if toasts && !first {
                        for (_, lvl, text) in fresh.iter().filter(|f| f.1 >= Level::Warn) {
                            notify::toast(&app_id, *lvl, text);
                        }
                    }
                    first = false;
                }
                Err(e) => {
                    eprintln!("host query failed: {e}");
                    let mut s = state.lock().unwrap();
                    match &mut s.host {
                        Some(h) => h.reachable = false,
                        None => s.host = Some(HostInfo::default()),
                    }
                }
            }
            ctx.request_repaint();
            thread::sleep(Duration::from_secs(cfg.poll_secs.max(15)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: u32, bugcheck: i64) -> RawEvent {
        RawEvent { key: "k".into(), t: "2026-10-15T14:56:54.0000000+02:00".into(), id, bugcheck, src: String::new(), msg: String::new() }
    }

    #[test]
    fn classifies_host_hardware_errors() {
        let hw = |src: &str, id: u32, msg: &str| RawEvent { src: src.into(), msg: msg.into(), ..ev(id, 0) };
        let e = hw("disk", 51, r"An error was detected on device \Device\Harddisk3\DR3 during a paging operation.");
        assert_eq!(classify(&e, "Router").unwrap().1, "Host disk 3: I/O error during paging");
        assert_eq!(classify(&hw("Microsoft-Windows-WHEA-Logger", 1, ""), "Router").unwrap().0, Level::Alert);
        assert!(classify(&hw("disk", 158, ""), "Router").is_none());
        // A hardware event with id 41 must not be read as Kernel-Power 41.
        assert!(classify(&hw("Ntfs", 41, ""), "Router").is_none());
    }

    #[test]
    fn classifies_power_loss_vs_bugcheck() {
        assert_eq!(classify(&ev(41, 0), "Router").unwrap().1, "Host stopped abruptly (no bugcheck, no dump)");
        assert_eq!(classify(&ev(41, 0x1CA), "Router").unwrap().1, "Host crashed (bugcheck 0x1CA)");
        assert_eq!(classify(&ev(18512, 0), "Router").unwrap().0, Level::Alert);
        assert!(classify(&ev(9999, 0), "Router").is_none());
    }

    #[test]
    fn parses_powershell_o_format() {
        assert!(parse_time("2026-10-15T14:56:54.1234567+02:00").is_some());
    }
}
