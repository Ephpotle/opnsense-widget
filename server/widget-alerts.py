#!/usr/bin/env python3
# Forced command for the widget's SSH key on the Wazuh manager: a read-only, filtered stream of
# Wazuh alerts. Prints the relevant alerts in full and, for everything else, a
# {"heartbeat": <timestamp>} line at most every 30 s so the widget can tell Wazuh is alive.
#
# Install:   cp widget-alerts.py /usr/local/bin/widget-alerts && chmod 755 /usr/local/bin/widget-alerts
# Key line:  restrict,command="/usr/local/bin/widget-alerts" <public key> widget
# Needs passwordless sudo for: /usr/bin/tail -n 20000 -F /var/ossec/logs/alerts/alerts.json
import json, subprocess, sys, time

# Rule ids the widget understands (see classify() in src/wazuh.rs). Suricata signature
# alerts (groups contain "ids", data.event_type == "alert") pass through as well.
WANT = {"87702", "100201", "503", "504", "506", "60122", "60204", "92657", "61107", "550", "553", "554", "5710", "5712"}

tail = subprocess.Popen(["sudo", "-n", "/usr/bin/tail", "-n", "20000", "-F", "/var/ossec/logs/alerts/alerts.json"],
                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, errors="replace")
last_hb = 0.0
for line in tail.stdout:
    try:
        a = json.loads(line)
    except ValueError:
        continue
    r = a.get("rule", {})
    if r.get("id") in WANT or ("ids" in r.get("groups", []) and a.get("data", {}).get("event_type") == "alert"):
        sys.stdout.write(line if line.endswith("\n") else line + "\n")
        sys.stdout.flush()
    elif time.time() - last_hb > 30:
        print(json.dumps({"heartbeat": a.get("timestamp")}), flush=True)
        last_hb = time.time()
