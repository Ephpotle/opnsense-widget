use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

const POWERSHELL_APP_ID: &str = "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";

/// Loaded from `%APPDATA%\router-widget\config.toml` (or `$ROUTER_WIDGET_CONFIG`); see
/// `examples/config.toml`. API credentials go in that file, or in a JSON file in the
/// opnsense-mcp-server format that `mcp_json` points at.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub url: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub mcp_json: Option<PathBuf>,
    pub poll_secs: u64,
    /// host:port pairs; a TCP connect through the router is the "is the internet up" probe.
    pub probe_targets: Vec<String>,
    /// Services whose stopping raises a warning (ids from /api/core/service/search).
    pub watch_services: Vec<String>,
    pub toasts: bool,
    pub toast_app_id: String,
    pub always_on_top: bool,
    /// "system" (follow Windows), "light" or "dark"; the tray menu choice overrides it.
    pub theme: String,
    /// IP -> friendly label, overrides DHCP hostnames. Named IPv4 devices also get
    /// "left the LAN" / "is back" events.
    pub names: HashMap<String, String>,
    /// A process on the router to watch, e.g. an IPTV multicast helper (matched against the
    /// command lines in Diagnostics > Activity). Omitted = no tile, no alerts.
    pub iptv_process: Option<String>,
    /// Hyper-V host feed over SSH; omitted = disabled.
    pub host: Option<HostConfig>,
    /// Wazuh alert stream over SSH (forced-command key); omitted = disabled.
    pub wazuh: Option<WazuhConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WazuhConfig {
    /// user@host for ssh.exe
    pub ssh: String,
    /// private key restricted on the server to `command="/usr/local/bin/widget-alerts"`
    pub key: String,
    /// Wazuh agent name (prefix) of the OPNsense box: its file-integrity events are shown.
    #[serde(default)]
    pub router_agent: Option<String>,
    /// Agent name prefixes whose disconnect/stop is a warning with a toast (others: info).
    #[serde(default)]
    pub important_agents: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    /// user@host for ssh.exe
    pub ssh: String,
    /// private key path (gitignored key file, never inline)
    pub key: String,
    /// Hyper-V VM name of the router
    pub vm: String,
    pub poll_secs: u64,
    /// How much host event history to show at startup.
    pub backfill_days: u32,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            ssh: String::new(),
            key: String::new(),
            vm: "OPNsense".into(),
            poll_secs: 60,
            backfill_days: 14,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: "https://192.168.1.1".into(),
            api_key: None,
            api_secret: None,
            mcp_json: None,
            poll_secs: 2,
            probe_targets: vec!["1.1.1.1:443".into(), "8.8.8.8:443".into()],
            // Ids from /api/core/service/search; ones not installed are simply never seen.
            watch_services: ["pf", "unbound", "dnsmasq", "suricata", "cloudflared", "wazuh-agentd"]
                .map(String::from)
                .to_vec(),
            toasts: true,
            // Windows drops toasts from an unregistered AUMID; PowerShell's is always there.
            toast_app_id: POWERSHELL_APP_ID.into(),
            always_on_top: true,
            theme: "system".into(),
            names: HashMap::new(),
            iptv_process: None,
            host: None,
            wazuh: None,
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("ROUTER_WIDGET_CONFIG") {
            return Some(PathBuf::from(p));
        }
        dirs::config_dir().map(|d| d.join("router-widget").join("config.toml"))
    }

    pub fn load() -> Result<Self, String> {
        let path = Self::path().ok_or("no config dir")?;
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(_) => Config::default(),
        };
        if cfg.api_key.is_none() || cfg.api_secret.is_none() {
            let mcp = cfg.mcp_json.clone().ok_or_else(|| {
                format!("no API credentials: set api_key/api_secret or mcp_json in {}", path.display())
            })?;
            let raw = std::fs::read_to_string(&mcp).map_err(|e| format!("{}: {e}", mcp.display()))?;
            let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", mcp.display()))?;
            let env = &v["mcpServers"]["opnsense"]["env"];
            cfg.api_key = env["OPNSENSE_API_KEY"].as_str().map(String::from);
            cfg.api_secret = env["OPNSENSE_API_SECRET"].as_str().map(String::from);
            if let Some(u) = env["OPNSENSE_URL"].as_str() {
                cfg.url = u.trim_end_matches('/').to_string();
            }
            if cfg.api_key.is_none() || cfg.api_secret.is_none() {
                return Err(format!("{} has no opnsense API key/secret", mcp.display()));
            }
        }
        Ok(cfg)
    }
}
