use reqwest::blocking::Client;
use serde_json::Value;
use std::time::Duration;

pub struct Api {
    http: Client,
    base: String,
    key: String,
    secret: String,
}

impl Api {
    pub fn new(base: &str, key: &str, secret: &str) -> Result<Self, String> {
        let http = Client::builder()
            // The router uses a self-signed cert; it is only ever reached over the LAN.
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(4))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { http, base: base.trim_end_matches('/').into(), key: key.into(), secret: secret.into() })
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .basic_auth(&self.key, Some(&self.secret))
            .send()
            .map_err(|e| format!("{path}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{path}: HTTP {}", resp.status()));
        }
        resp.json().map_err(|e| format!("{path}: {e}"))
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let resp = self
            .http
            .post(format!("{}{}", self.base, path))
            .basic_auth(&self.key, Some(&self.secret))
            .json(body)
            .send()
            .map_err(|e| format!("{path}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{path}: HTTP {}", resp.status()));
        }
        resp.json().map_err(|e| format!("{path}: {e}"))
    }
}

/// OPNsense returns numbers as JSON numbers or as strings, depending on the endpoint.
pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
