# opnsense-widget

A small always-on-top Windows widget, plus a tray icon, for an **OPNsense** firewall. It shows
at a glance whether the network is healthy and what it is doing right now, logs every hiccup
with its duration and **where** it happened (this PC, the LAN/router, the ISP link, or beyond),
and raises a Windows toast when something changes.

Written in Rust (egui/eframe, tray-icon), styled after Fluent 2 / Windows 11, light and dark.

> **Status: early.** It runs daily against one home network. Expect rough edges, and read
> [Security](#security) before pointing it at your firewall.

## What it shows

| Card | Content |
|------|---------|
| Header | status (healthy / warning / alert), hostname, version, uptime, Light/Auto/Dark switch |
| Internet | WAN ↓/↑ and a 5-minute sparkline; RTT and loss; a hop line (`Router 0 ms · ISP gateway 3 ms · DNS ok`); this PC's UDP-port use |
| Router | CPU, memory, pf states, blocked inbound/min, services, Suricata, mbuf failures, LAN drops, updates |
| Busiest devices | the top LAN hosts right now, named from DHCP leases or your `[names]` |
| Activity | timestamped events (below) |
| Hyper-V host *(optional)* | host/VM uptime, last crash, hardware errors; a history link (hover to preview, click for a window) |
| Security *(optional)* | Wazuh: scan bursts/hour, IDS alerts, SIEM liveness |

### Events

- **Connection:** every outage, DNS failure, or slow/lossy stretch, with its duration and the
  nearest broken hop. The probe runs from the PC every 5 s: TCP to `1.1.1.1:443` and
  `8.8.8.8:443`, ICMP to the router and to the ISP's gateway, and a DNS lookup through the
  router. No admin rights needed.
- **Router:** API unreachable, reboot, service stopped/started, mbuf failures, config changed
  (with the change's description and user from the config history), WAN link down/up and DHCP
  lease problems (from the system log), WAN IPv4 / delegated IPv6 prefix changed, new device
  on the LAN, named devices leaving/rejoining, heavy transfers (≥ 20 Mb/s for ≥ 5 min),
  updates available.
- **This PC:** UDP ephemeral-port exhaustion (Windows `Tcpip 4266`). When it happens every new
  connection fails, which looks exactly like a network outage. The widget names the processes
  holding the ports.
- **Optional:** a watched router process (e.g. an IPTV multicast helper), Hyper-V host crashes,
  VM resets and disk/WHEA errors, and Wazuh alerts.

## Install

Needs Windows 10/11 and a Rust toolchain (`rustup`, MSVC).

```
git clone https://github.com/Ephpotle/opnsense-widget
cd opnsense-widget
cargo build --release
```

The binary is `target\release\router-widget.exe`. Copy it anywhere. To start it at logon,
create a Scheduled Task ("At log on", run only when the user is logged on) that runs it.

Closing the window sends it to the tray. Click the tray dot to show/hide it; the tray menu
has the theme choice and Quit.

## Configure

Copy [`examples/config.toml`](examples/config.toml) to `%APPDATA%\router-widget\config.toml`
and set at least `url`, `api_key` and `api_secret`. `ROUTER_WIDGET_CONFIG` points it at another
file.

### The API key

Create a dedicated OPNsense user (System › Access › Users) with an API key, and grant only the
privileges that cover these endpoints. An admin key works, but don't. Everything is read-only,
except one POST that refreshes the update list (the same as the GUI's *Check for updates*).

| Endpoint | Method | Every |
|----------|--------|-------|
| `/api/diagnostics/traffic/interface` | GET | 2 s |
| `/api/diagnostics/activity/getActivity`, `system/systemResources`, `firewall/pf_states`, `system/systemMbuf`, `traffic/top/lan` | GET | 10 s |
| `/api/diagnostics/system/systemTime`, `/api/core/service/search`, `/api/ids/service/status`, `/api/dnsmasq/leases/search`, `/api/diagnostics/interface/get_interface_config`, `get_arp`, `get_ndp`, `/api/diagnostics/firewall/log/`, `/api/routes/gateway/status` | GET | 30 s |
| `/api/diagnostics/log/core/system` (search) | POST | 30 s |
| `/api/core/backup/backups/this` | GET | on config change |
| `/api/core/firmware/status` / `/api/core/firmware/check` | GET / POST | 6 h / 24 h |
| `/api/diagnostics/system/systemInformation` | GET | once |

### Optional: Hyper-V host (`[host]`)

If OPNsense runs as a Hyper-V VM, the widget can read the host's event log over SSH:
crashes vs. power loss (Kernel-Power 41 with/without a bugcheck), planned restarts, the VM's
start/stop/reset (Hyper-V-Worker 18500–18590), and disk/storage/NTFS/WHEA errors. The host
needs the OpenSSH server, and a key-authenticated user that may run `Get-VM` and read the
System log. The widget runs one `powershell -EncodedCommand` query per `poll_secs`.

### Optional: Wazuh (`[wazuh]`)

A long-lived SSH session to the Wazuh manager, whose key is restricted to a forced command
that streams a filtered `alerts.json`:

1. Copy [`server/widget-alerts.py`](server/widget-alerts.py) to `/usr/local/bin/widget-alerts`
   on the manager (`chmod 755`). The SSH user needs passwordless `sudo` for
   `/usr/bin/tail -F /var/ossec/logs/alerts/alerts.json`.
2. Create a key pair. **If Wazuh runs in FIPS mode, ed25519 keys are refused; use ECDSA P-256
   or RSA.** Then add it to that user's `~/.ssh/authorized_keys`:
   `restrict,command="/usr/local/bin/widget-alerts" ecdsa-sha2-nistp256 AAAA... widget`
3. Set `ssh`, `key`, `router_agent` and `important_agents` in `[wazuh]`.

Shown: unusual hours of inbound scan bursts (rule 87702, against a 24 h baseline), Suricata
signature alerts, agent start/stop/disconnect, failed logons with the reason in words, SSH
login attempts, remote logons from outside the LAN, file-integrity changes on the router
(minus files OPNsense rewrites itself), and service crashes. A "SIEM went quiet" warning
fires if Wazuh produces nothing for 30 minutes. Rule `100201` is a custom router-health rule;
without it, it simply never fires.

## Security

- **TLS:** the widget currently accepts the router's certificate **without verifying it**
  (OPNsense ships a self-signed one). Use it only on a LAN you trust. Certificate pinning is
  the first item on the roadmap.
- The API key sits in plain text in `%APPDATA%\router-widget\config.toml`. Restrict that
  file's ACL to your user, and give the key the minimum privileges (above).
- SSH keys for the optional feeds should be dedicated, key-only, and (for Wazuh) forced-command.

## Notes

- **Fluent 2:** Segoe UI Variable, Segoe Fluent Icons, the Windows accent colour, and Fluent's
  neutral surfaces (`#F3F3F3`/`#202020`, cards `#FBFBFB`/`#2B2B2B`). Status fills are tints of
  the status colour over the card colour. Light/dark follows Windows, or the switch in the
  header.
- **Scale:** the layout scales to the window size (two columns when wide) and fits the window
  height. Only the activity list scrolls.
- **Toasts** use PowerShell's AUMID by default, because Windows silently drops toasts from an
  unregistered app id. Set `toast_app_id` to your own registered AUMID to change the sender name.

### Gotchas (for contributors)

- **eframe stops calling `update()` while the window is hidden.** The tray therefore runs on its
  own thread with its own message pump, and show/hide use `ShowWindow` on the HWND. Anything
  drawn from `update()`, such as the host history window, is closed when the widget hides.
- **Dragging between monitors with different scaling** (e.g. 150 % 4K next to 100 % 1080p):
  Windows expects `WM_DPICHANGED` to be answered with exactly the rectangle it suggests, and its
  drag loop relies on that. winit computes its own rectangle, so at the screen edge the window
  bounced between scales and its size compounded with each bounce. `dpi.rs` subclasses the
  window: winit takes the new scale factor, and its `SetWindowPos` is rewritten (in
  `WM_WINDOWPOSCHANGING`) to the suggested rectangle. VLC hit and fixed the same loop.
- **Window placement is saved by the widget** (`geometry` in `data/app.ron`, physical pixels).
  eframe saved a minimized window as `-32000,-32000` / 0×0, and loads its own `window` key even
  with `persist_window` off. At startup the saved placement is applied again once the DPI has
  settled, because the window is created on the primary monitor first.
- **Dark mode goes through winit** (`ViewportCommand::SetTheme`). winit owns the immersive-dark
  title-bar attribute and re-applies the Windows app mode on DPI and settings changes.
- `wevtutil` prints local times with a `Z` suffix.

### Test aids

`ROUTER_WIDGET_CONFIG` (alternate config), `ROUTER_WIDGET_GEOMETRY=x,y,w,h` (start placement, in
physical pixels), and `ROUTER_WIDGET_HISTORY=1` (open the host history window at start).

## Roadmap

1. Certificate pinning (or a CA file) for the router API.
2. A first-run setup screen (URL, key, test connection) instead of hand-editing TOML.
3. Release binaries with an embedded icon; an autostart toggle.
4. Lighter polling: the firewall-log sample (~800 KB per 30 s) for the blocked rate is the
   biggest cost.

## License

MIT. See [LICENSE](LICENSE).
