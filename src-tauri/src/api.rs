//! Signed requests to usagerank.com, as the CLI's `src/api.js` makes them. Every request carries the
//! machine's public key and an ed25519 signature of `${timestamp}.${sha256hex(raw body)}`.

use crate::config::{Config, Env};
use crate::scan::DayRow;
use flate2::{write::GzEncoder, Compression};
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::time::Duration;

pub const CLIENT_VERSION: &str = concat!("app-", env!("CARGO_PKG_VERSION"));

/// Rows per request. A date is never split across batches because the server replaces whole days.
const BATCH: usize = 2000;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

fn err(message: impl Into<String>) -> ApiError {
    ApiError { status: None, message: message.into() }
}

pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

pub fn timezone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into())
}

pub fn hostname() -> String {
    std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "Mac".into())
}

#[derive(Clone)]
pub struct Api {
    client: reqwest::Client,
    pub env: Env,
}

impl Api {
    pub fn new(env: Env) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(format!("usagerank-app/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Api { client, env }
    }

    fn login_url(&self, suffix: &str) -> String {
        format!("{}/api/cli/login{suffix}", self.env.web_url)
    }

    /// The stats endpoint sits next to the usage endpoint, so a custom API URL points both at the same server.
    fn stats_url(&self) -> String {
        match self.env.api_url.strip_suffix("/usage") {
            Some(base) => format!("{base}/stats"),
            None => format!("{}/api/cli/stats", self.env.web_url),
        }
    }

    /// Shared cards are uploaded next to the usage endpoint too.
    fn shares_url(&self) -> String {
        match self.env.api_url.strip_suffix("/usage") {
            Some(base) => format!("{base}/shares"),
            None => format!("{}/api/cli/shares", self.env.web_url),
        }
    }

    async fn request(&self, cfg: &Config, method: Method, url: &str, payload: Option<&Value>, gzip: bool) -> ApiResult<Value> {
        let body: Vec<u8> = match payload {
            None => Vec::new(),
            Some(p) => {
                let json = serde_json::to_vec(p).map_err(|e| err(e.to_string()))?;
                if gzip {
                    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
                    enc.write_all(&json).map_err(|e| err(e.to_string()))?;
                    enc.finish().map_err(|e| err(e.to_string()))?
                } else {
                    json
                }
            }
        };
        let ts = chrono::Utc::now().timestamp_millis().to_string();
        let digest: String = Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect();
        let signature = cfg.sign(format!("{ts}.{digest}").as_bytes()).map_err(err)?;

        let mut req = self
            .client
            .request(method.clone(), url)
            .header("accept", "application/json")
            .header("x-usagerank-machine", &cfg.machine_id)
            .header("x-usagerank-key", &cfg.public_key)
            .header("x-usagerank-timestamp", &ts)
            .header("x-usagerank-signature", signature);
        if payload.is_some() {
            req = req.header("content-type", "application/json");
            if gzip {
                req = req.header("content-encoding", "gzip");
            }
            req = req.body(body);
        }

        let res = req.send().await.map_err(|e| {
            let why = if e.is_connect() {
                format!("can't reach {}", reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default())
            } else if e.is_timeout() {
                "timed out".into()
            } else {
                e.to_string()
            };
            err(format!("{method} {url} failed: {why}"))
        })?;

        let status = res.status();
        let text = res.text().await.map_err(|e| ApiError {
            status: if status.is_success() { None } else { Some(status.as_u16()) },
            message: format!("Cannot read the server response: {e}"),
        })?;
        let data: Value = if text.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(&text) {
                Ok(data) => data,
                Err(_) if !status.is_success() => json!({ "message": text }),
                Err(_) => return Err(err("The server sent an invalid JSON response.")),
            }
        };
        if !status.is_success() {
            let msg = data
                .get("message")
                .or_else(|| data.get("error"))
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_string());
            return Err(ApiError { status: Some(status.as_u16()), message: msg });
        }
        Ok(data)
    }

    /// Upload daily rows. An empty `days` still claims `cfg.handle`.
    pub async fn push(&self, cfg: &Config, days: &[DayRow], backfill: bool) -> ApiResult<Value> {
        let chunks = batches(days);
        let count = chunks.len();
        let (mut rows, mut cost) = (0u64, 0f64);
        let mut last = json!({});
        for (index, chunk) in chunks.into_iter().enumerate() {
            let payload = json!({
                "client": CLIENT_VERSION,
                "handle": cfg.handle,
                "machine": cfg.machine_id,
                "os": platform(),
                "tz": timezone(),
                "backfill": backfill,
                "batch": { "index": index, "count": count },
                "days": chunk,
            });
            last = self.request(cfg, Method::POST, &self.env.api_url, Some(&payload), true).await?;
            rows += last.get("days").and_then(Value::as_u64).unwrap_or(0);
            cost += last.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
        }
        if let Some(obj) = last.as_object_mut() {
            if obj.contains_key("days") {
                obj.insert("days".into(), json!(rows));
                obj.insert("cost_usd".into(), json!(cost));
            }
        }
        Ok(last)
    }

    /// Delete the user's row and history on the server.
    pub async fn forget(&self, cfg: &Config) -> ApiResult<Value> {
        self.request(cfg, Method::DELETE, &self.env.api_url, None, true).await
    }

    /// Host a usage card (base64 PNG) and return the link to post; its preview image is the card.
    pub async fn share(&self, cfg: &Config, png: &str) -> ApiResult<String> {
        let res = self.request(cfg, Method::POST, &self.shares_url(), Some(&json!({ "image": png })), false).await?;
        res.get("url").and_then(Value::as_str).map(String::from).ok_or_else(|| err("The server did not return a share link."))
    }

    /// The signed-in user's standing, activity and model mix.
    pub async fn stats(&self, cfg: &Config) -> ApiResult<Value> {
        let mut url = reqwest::Url::parse(&self.stats_url()).map_err(|e| err(e.to_string()))?;
        url.query_pairs_mut().append_pair("tz", &timezone());
        let stats = self.request(cfg, Method::GET, url.as_str(), None, false).await?;
        validate_stats(&stats)?;
        Ok(stats)
    }

    /// Start a device-flow login: returns the user code to approve in the browser.
    pub async fn start_login(&self, cfg: &Config) -> ApiResult<Value> {
        let payload = json!({ "machine": cfg.machine_id, "name": hostname(), "platform": platform() });
        self.request(cfg, Method::POST, &self.login_url(""), Some(&payload), false).await
    }

    /// One poll: Ok(Some(user)) once approved, Ok(None) while pending.
    pub async fn poll_login(&self, cfg: &Config, device_code: &str) -> ApiResult<Option<Value>> {
        let payload = json!({ "device_code": device_code });
        let res = self.request(cfg, Method::POST, &self.login_url("/poll"), Some(&payload), false).await?;
        Ok((res.get("status").and_then(Value::as_str) == Some("approved")).then(|| res.get("user").cloned().unwrap_or(json!({}))))
    }
}

fn validate_stats(stats: &Value) -> ApiResult<()> {
    let invalid = || err("The server sent an invalid statistics response.");
    if !stats.get("user").is_some_and(Value::is_object) {
        return Err(invalid());
    }
    let periods = stats.get("periods").and_then(Value::as_object).ok_or_else(invalid)?;
    for period in periods.values() {
        if !period.is_object() {
            return Err(invalid());
        }
        for key in ["neighbors", "models"] {
            if let Some(list) = period.get(key).filter(|v| !v.is_null()) {
                if !list.as_array().is_some_and(|a| a.iter().all(Value::is_object)) {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}

fn batches(rows: &[DayRow]) -> Vec<&[DayRow]> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..rows.len() {
        let date_ends = rows.get(i + 1).map(|r| &r.date) != Some(&rows[i].date);
        if date_ends && i + 1 - start >= BATCH {
            out.push(&rows[start..=i]);
            start = i + 1;
        }
    }
    if start < rows.len() || out.is_empty() {
        out.push(&rows[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(date: &str) -> DayRow {
        DayRow {
            date: date.into(),
            tool: "claude".into(),
            model: "m".into(),
            calls: 1,
            sessions: 1,
            input: 0,
            output: 0,
            cache_write: 0,
            cache_read: 0,
            reasoning: 0,
        }
    }

    #[test]
    fn batches_never_split_a_date() {
        let mut rows: Vec<DayRow> = (0..1999).map(|_| row("2026-10-01")).collect();
        rows.extend((0..3).map(|_| row("2026-10-02")));
        rows.extend((0..5).map(|_| row("2026-10-03")));
        let b = batches(&rows);
        assert_eq!(b.iter().map(|c| c.len()).collect::<Vec<_>>(), vec![2002, 5]);
        assert_eq!(batches(&[]).len(), 1);
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;
    use std::io::Read;

    async fn response(raw: &'static [u8]) -> ApiResult<Value> {
        let root = std::env::temp_dir().join(format!("ur-errors-{}", uuid::Uuid::new_v4()));
        let env = crate::config::test_env(&root);
        let cfg = Config::create(&env, "tester").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/stats", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            stream.write_all(raw).unwrap();
        });
        let api = Api { client: reqwest::Client::builder().no_proxy().build().unwrap(), env };
        let result = api.request(&cfg, Method::GET, &url, None, false).await;
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        result
    }

    #[tokio::test]
    async fn truncated_and_invalid_success_responses_are_errors() {
        let truncated = response(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{}").await;
        assert!(truncated.unwrap_err().message.contains("Cannot read"));
        let invalid = response(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad").await;
        assert!(invalid.unwrap_err().message.contains("invalid JSON"));
    }

    #[tokio::test]
    async fn non_json_http_error_keeps_status_and_message() {
        let error = response(b"HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing").await.unwrap_err();
        assert_eq!(error.status, Some(404));
        assert_eq!(error.message, "missing");
    }

    #[test]
    fn stats_reject_malformed_arrays_and_accept_valid_data() {
        for bad in [json!({}), json!({"user":{},"periods":{"1":{"neighbors":{}}}}), json!({"user":{},"periods":{"1":{"models":[null]}}})] {
            assert!(validate_stats(&bad).is_err());
        }
        assert!(validate_stats(&json!({"user":{"handle":"tester"},"periods":{"1":{"neighbors":[],"models":[]}}})).is_ok());
    }
}
