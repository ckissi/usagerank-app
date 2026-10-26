//! Live subscription limits (5-hour session, weekly, model-scoped weeks) for Claude and Codex, read
//! the way OpenUsage does: reuse the login Claude Code / the Codex CLI already have on this Mac and
//! ask the same usage endpoints those tools use. Nothing here talks to usagerank.com.
//!
//! - Claude: `Claude Code-credentials` in the login keychain (via `/usr/bin/security`, which is how
//!   Claude Code itself stores it), falling back to `~/.claude/.credentials.json`.
//!   If that login is missing or rejected, borrow Claude Desktop's valid access token (read-only).
//!   `GET https://api.anthropic.com/api/oauth/usage`. Reset grants come from the same endpoint with
//!   `?cedar_ember=1&skip_spend=1`, asked at most once an hour per login.
//! - Codex: `$CODEX_HOME/auth.json` (default `~/.codex`), falling back to the CLI's `Codex Auth`
//!   keychain item. `GET https://chatgpt.com/backend-api/wham/usage`, plus
//!   `GET .../wham/rate-limit-reset-credits` for the reset credits and their expiries.
//!
//! An expired token is refreshed once and written back to where it came from (only if that login
//! hasn't changed in the meantime), so both CLIs keep working with the rotated token.

use crate::config::Env;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::future::Future;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const CLAUDE_REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_SCOPES: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// Anthropic only offers reset grants to clients it recognizes as Claude Code.
const CLAUDE_USER_AGENT: &str = "claude-cli/2.1.294 (external, cli)";
const CLAUDE_RESETS_INTERVAL_MS: i64 = 3600 * 1000;

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_RESETS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CODEX_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_KEYCHAIN_SERVICE: &str = "Codex Auth";

const SESSION_MS: i64 = 5 * 3600 * 1000;
const WEEK_MS: i64 = 7 * 24 * 3600 * 1000;

/// One rate-limit window: `used` is a percentage (0–100).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub label: String,
    pub used: f64,
    /// Epoch ms; `None` when the window hasn't started (Claude reports no reset before the first message).
    pub resets_at: Option<i64>,
    pub period_ms: i64,
    /// Session and Weekly are shown up front; the rest sits behind the expander.
    pub primary: bool,
}

/// One-off rate-limit resets the account can spend (Claude reset grants, Codex reset credits).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Resets {
    pub available: u32,
    /// Epoch ms, soonest first, one per reset that has a deadline.
    pub expires_at: Vec<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Provider {
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    /// `None` when the account isn't in a reset program (or it couldn't be read yet).
    pub resets: Option<Resets>,
    /// Set when the last fetch failed. The previous windows are kept and shown as stale.
    pub error: Option<String>,
    /// No login found at all: the card explains how to sign in.
    pub missing: bool,
    pub fetched_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub claude: Option<Provider>,
    pub codex: Option<Provider>,
}

pub struct FetchError {
    pub message: String,
    pub missing: bool,
    /// Seconds to wait before asking again (HTTP 429).
    pub retry_after: Option<u64>,
    credentials_changed: bool,
    auth_fallback: bool,
}

impl FetchError {
    fn new(message: impl Into<String>) -> Self {
        FetchError { message: message.into(), missing: false, retry_after: None, credentials_changed: false, auth_fallback: false }
    }
    fn missing(message: impl Into<String>) -> Self {
        FetchError { message: message.into(), missing: true, retry_after: None, credentials_changed: false, auth_fallback: true }
    }
    fn auth(message: impl Into<String>) -> Self {
        Self { auth_fallback: true, ..Self::new(message) }
    }
}

pub struct Fetched {
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    pub resets: Option<Resets>,
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// ISO-8601 string or epoch (seconds or ms) to epoch ms.
fn epoch_ms(v: Option<&Value>) -> Option<i64> {
    if let Some(Value::String(s)) = v {
        if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s.trim()) {
            return Some(d.timestamp_millis());
        }
    }
    let n = num(v)?;
    Some(if n.abs() < 1e10 { (n * 1000.0) as i64 } else { n as i64 })
}

fn http_error(service: &str, status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> FetchError {
    if status.as_u16() == 429 {
        let retry_after = headers
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        let wait = retry_after.map(|s| format!(" Retrying in ~{}m.", s.div_ceil(60).max(1))).unwrap_or_default();
        return FetchError { message: format!("{service} is rate limiting usage checks.{wait}"), missing: false, retry_after: retry_after.or(Some(300)), credentials_changed: false, auth_fallback: false };
    }
    FetchError::new(format!("{service} answered HTTP {}.", status.as_u16()))
}

fn net_error(service: &str, e: reqwest::Error) -> FetchError {
    if e.is_timeout() {
        FetchError::new(format!("{service} didn't answer in time."))
    } else {
        FetchError::new(format!("Couldn't reach {service}."))
    }
}

// ── keychain (via /usr/bin/security, the same tool Claude Code uses) ────────

/// `security` exits 44 (errSecItemNotFound) when no item matches.
const KEYCHAIN_NOT_FOUND: i32 = 44;

/// `Ok(None)` means there is no such item. Any other failure (a busy or locked keychain, a denied
/// prompt) is an `Err`, so a transient hiccup is never mistaken for being signed out.
fn keychain_lookup(service: &str, account: Option<&str>) -> Result<Option<String>, String> {
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args(["find-generic-password", "-s", service]);
    if let Some(a) = account {
        cmd.args(["-a", a]);
    }
    let out = cmd.arg("-w").output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return match out.status.code() {
            Some(KEYCHAIN_NOT_FOUND) => Ok(None),
            code => Err(format!("security exited with {}", code.map_or("a signal".into(), |c| c.to_string()))),
        };
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!text.is_empty()).then(|| decode_hex_fallback(&text)))
}

fn keychain_read(service: &str, account: Option<&str>) -> Option<String> {
    keychain_lookup(service, account).ok().flatten()
}

/// The keychain item's account attribute, needed to update it in place.
fn keychain_account(service: &str) -> Option<String> {
    let out = Command::new("/usr/bin/security").args(["find-generic-password", "-s", service]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.trim_start().starts_with("\"acct\""))?;
    let start = line.find("=\"")? + 2;
    Some(line[start..].trim_end().trim_end_matches('"').to_string())
}

fn keychain_write(service: &str, account: &str, value: &str) -> Result<(), String> {
    let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
    let out = Command::new("/usr/bin/security")
        .args(["add-generic-password", "-U", "-a", account, "-s", service, "-X", &hex])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `security -w` prints the secret as hex when it isn't plain text.
fn decode_hex_fallback(text: &str) -> String {
    if text.starts_with('{') || text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return text.to_string();
    }
    let bytes: Option<Vec<u8>> = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect();
    bytes.and_then(|b| String::from_utf8(b).ok()).unwrap_or_else(|| text.to_string())
}

fn sha256_hex(s: &str, bytes: usize) -> String {
    Sha256::digest(s.as_bytes()).iter().take(bytes).map(|b| format!("{b:02x}")).collect()
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    crate::config::write_atomic(path, text.as_bytes())
}

// ── Claude ─────────────────────────────────────────────────────────────────

enum Source {
    Keychain(String, Option<String>),
    File(PathBuf),
    Desktop,
}

struct Login {
    doc: Value,
    source: Source,
}

impl Login {
    fn field(&self, key: &str) -> Option<&Value> {
        self.doc.get("claudeAiOauth")?.get(key)
    }
    fn str(&self, key: &str) -> Option<String> {
        self.field(key).and_then(Value::as_str).map(String::from).filter(|s| !s.trim().is_empty())
    }
}

fn claude_home(env: &Env) -> Option<String> {
    env.var("CLAUDE_CONFIG_DIR")
}

fn claude_services(env: &Env) -> Vec<String> {
    match claude_home(env) {
        Some(dir) => vec![format!("{CLAUDE_KEYCHAIN_SERVICE}-{}", sha256_hex(&dir, 4)), CLAUDE_KEYCHAIN_SERVICE.into()],
        None => vec![CLAUDE_KEYCHAIN_SERVICE.into()],
    }
}

fn claude_file(env: &Env) -> PathBuf {
    claude_home(env)
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
        .join(".credentials.json")
}

fn has_token(doc: &Value) -> bool {
    doc.pointer("/claudeAiOauth/accessToken").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
}

/// Keychain first (Claude Code's source of truth on macOS), then the credentials file. Claude Code
/// keeps its item under the current user's account, so that one is tried before any match for the
/// service. The error is set when the keychain couldn't be read, as opposed to holding no login.
fn claude_logins(env: &Env) -> (Vec<Login>, Option<String>) {
    let mut out = Vec::new();
    let mut keychain_error = None;
    let user = env.var("USER").filter(|u| !u.trim().is_empty());
    let accounts: Vec<Option<&str>> = match user.as_deref() {
        Some(u) => vec![Some(u), None],
        None => vec![None],
    };
    'services: for service in claude_services(env) {
        for &account in &accounts {
            match keychain_lookup(&service, account) {
                Ok(Some(text)) => match serde_json::from_str::<Value>(&text) {
                    Ok(doc) if has_token(&doc) => {
                        out.push(Login { doc, source: Source::Keychain(service.clone(), account.map(String::from)) });
                        break 'services;
                    }
                    Ok(_) => {}
                    Err(_) => keychain_error = Some("the saved login couldn't be parsed".into()),
                },
                Ok(None) => {}
                Err(e) => keychain_error = Some(e),
            }
        }
    }
    let file = claude_file(env);
    if let Some(doc) = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        if has_token(&doc) {
            out.push(Login { doc, source: Source::File(file) });
        }
    }
    (out, keychain_error)
}

fn reread(source: &Source) -> Result<Value, FetchError> {
    let text = match source {
        Source::Keychain(service, account) => keychain_read(service, account.as_deref())
            .ok_or_else(|| FetchError::new(format!("Cannot read the {service} credentials from the keychain.")))?,
        Source::File(path) => std::fs::read_to_string(path)
            .map_err(|e| FetchError::new(format!("Cannot read {}: {e}", path.display())))?,
        Source::Desktop => return Err(FetchError::new("Claude Desktop credentials are read-only.")),
    };
    serde_json::from_str(&text).map_err(|_| FetchError::new("Cannot parse the saved credentials. The credentials were not changed."))
}

fn write_source(source: &Source, doc: &Value) -> Result<(), FetchError> {
    let text = serde_json::to_string_pretty(doc).map_err(|e| FetchError::new(e.to_string()))?;
    let saved = match source {
        Source::Keychain(service, account) => account.clone().or_else(|| keychain_account(service))
            .ok_or_else(|| "Cannot find the keychain account.".to_string())
            .and_then(|account| keychain_write(service, &account, &text)),
        Source::File(path) => write_atomic(path, &text),
        Source::Desktop => return Err(FetchError::new("Claude Desktop credentials are read-only.")),
    };
    saved.map_err(|e| {
        let mut error = FetchError::new(format!("Cannot save refreshed credentials: {e}. Sign in again if the login expires."));
        error.credentials_changed = true;
        error
    })
}

fn commit_refresh(
    doc: &mut Value,
    mut latest: Value,
    refresh_pointer: &str,
    expected: &str,
    apply: impl FnOnce(&mut Value),
    save: impl FnOnce(&Value) -> Result<(), FetchError>,
) -> Result<(), FetchError> {
    if latest.pointer(refresh_pointer).and_then(Value::as_str) == Some(expected) {
        apply(&mut latest);
        save(&latest)?;
    }
    // If another client refreshed the login, use its newer credentials.
    *doc = latest;
    Ok(())
}

/// Prefer the live profile: Claude Code saves `subscriptionType`/`rateLimitTier` at sign-in and never
/// updates them on refresh, so after an upgrade the saved login still names the old plan.
fn claude_plan(login: &Login, profile: Option<&Value>) -> Option<String> {
    let org = profile.and_then(|p| p.get("organization"));
    let live = |key| org.and_then(|o| o.get(key)).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let kind = live("organization_type").map(|k| k.strip_prefix("claude_").unwrap_or(&k).to_string())
        .filter(|k| !k.is_empty())
        .or_else(|| login.str("subscriptionType"))?;
    let tier = live("rate_limit_tier").or_else(|| login.str("rateLimitTier"));
    let mut plan = title_case(&kind.replace('_', " "));
    if let Some(tier) = tier {
        if let Some(mult) = tier.split(|c: char| !c.is_ascii_alphanumeric()).find(|p| {
            p.len() > 1 && p.ends_with('x') && p[..p.len() - 1].chars().all(|c| c.is_ascii_digit())
        }) {
            plan = format!("{plan} {mult}");
        }
    }
    Some(plan)
}

/// The profile Anthropic returned for one access token (hashed), or `None` when that lookup failed.
/// The usage endpoint rate-limits aggressively, so the profile is fetched at most once per token,
/// failures included: a failed lookup keeps the stored plan until the token rotates.
type ProfileCache = Mutex<Option<(String, Option<Value>)>>;
static CLAUDE_PROFILE: ProfileCache = Mutex::new(None);

async fn live_profile<F: Future<Output = Option<Value>>>(cache: &ProfileCache, token: &str, fetch: impl FnOnce() -> F) -> Option<Value> {
    let key = sha256_hex(token.trim(), 16);
    if let Some((cached, profile)) = cache.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if *cached == key {
            return profile.clone();
        }
    }
    let profile = fetch().await;
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, profile.clone()));
    profile
}

/// Best effort: the plan falls back to the saved login when this fails.
async fn claude_profile(http: &reqwest::Client, token: &str) -> Option<Value> {
    let res = http.get(CLAUDE_PROFILE_URL)
        .bearer_auth(token.trim()).header("Accept", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20").send().await.ok()?;
    if !res.status().is_success() { return None; }
    res.json().await.ok()
}

fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c.flat_map(|x| x.to_lowercase())).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn claude_refresh(http: &reqwest::Client, login: &mut Login) -> Result<(), FetchError> {
    if matches!(login.source, Source::Desktop) {
        return Err(FetchError::auth(crate::claude_desktop::DesktopError::Stale.message()));
    }
    let refresh = login.str("refreshToken").ok_or_else(|| FetchError::auth("Claude login expired. Run `claude` to sign in again."))?;
    let res = http
        .post(CLAUDE_REFRESH_URL)
        .json(&json!({ "grant_type": "refresh_token", "refresh_token": refresh, "client_id": CLAUDE_CLIENT_ID, "scope": CLAUDE_SCOPES }))
        .send()
        .await
        .map_err(|e| net_error("Anthropic", e))?;
    let status = res.status();
    if status.as_u16() == 400 || status.as_u16() == 401 {
        let body = res.json::<Value>().await.unwrap_or(Value::Null);
        return Err(claude_refresh_error(status, &body));
    }
    if !status.is_success() {
        return Err(http_error("Anthropic", status, res.headers()));
    }
    let body: Value = res.json().await.map_err(|_| FetchError::new("Anthropic sent an unreadable token response."))?;
    let access = body.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| FetchError::new("Claude login expired. Run `claude` to sign in again."))?;

    let latest = reread(&login.source).map_err(|mut e| { e.credentials_changed = true; e })?;
    let apply = |doc: &mut Value| {
        if let Some(oauth) = doc.get_mut("claudeAiOauth").and_then(Value::as_object_mut) {
            oauth.insert("accessToken".into(), json!(access));
            if let Some(r) = body.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                oauth.insert("refreshToken".into(), json!(r));
            }
            if let Some(secs) = num(body.get("expires_in")).filter(|s| s.is_finite() && *s >= 0.0) {
                oauth.insert("expiresAt".into(), json!(now_ms().saturating_add((secs * 1000.0) as i64)));
            }
        }
    };
    commit_refresh(&mut login.doc, latest, "/claudeAiOauth/refreshToken", &refresh, apply, |doc| write_source(&login.source, doc))
}

fn claude_request(http: &reqwest::Client, url: &str, token: &str) -> reqwest::RequestBuilder {
    http.get(url)
        .bearer_auth(token.trim())
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", CLAUDE_USER_AGENT)
}

async fn claude_usage(http: &reqwest::Client, token: &str) -> Result<Result<Value, (u16, FetchError)>, FetchError> {
    let res = claude_request(http, CLAUDE_USAGE_URL, token)
        .send()
        .await
        .map_err(|e| net_error("Anthropic", e))?;
    let status = res.status();
    if !status.is_success() {
        return Ok(Err((status.as_u16(), http_error("Anthropic", status, res.headers()))));
    }
    res.json::<Value>().await.map(Ok).map_err(|_| FetchError::new("Anthropic sent an unreadable usage response."))
}

fn claude_refresh_error(status: reqwest::StatusCode, body: &Value) -> FetchError {
    if body.get("error").or_else(|| body.get("error_description")).and_then(Value::as_str) == Some("invalid_grant") {
        FetchError::auth("Claude login expired. Run `claude` to sign in again.")
    } else {
        // A proxy/WAF's 400/401 is not evidence that the login expired (OpenUsage #738).
        http_error("Anthropic", status, &reqwest::header::HeaderMap::new())
    }
}

pub async fn fetch_claude(env: &Env, http: &reqwest::Client, allow_interaction: bool) -> Result<Fetched, FetchError> {
    let cli_error = match fetch_claude_code(env, http).await {
        Ok(fetched) => return Ok(fetched),
        Err(error) if error.auth_fallback => error,
        Err(error) => return Err(error),
    };
    let desktop = tauri::async_runtime::spawn_blocking(move || crate::claude_desktop::load(allow_interaction)).await
        .map_err(|_| FetchError::new("Cannot read the Claude Desktop login."))?;
    let desktop = match desktop {
        Ok(login) => login,
        Err(crate::claude_desktop::DesktopError::NotFound) => return Err(cli_error),
        Err(error) => return Err(FetchError::new(error.message())),
    };
    fetch_claude_desktop(http, desktop).await
}

async fn fetch_claude_desktop(http: &reqwest::Client, desktop: crate::claude_desktop::DesktopLogin) -> Result<Fetched, FetchError> {
    let login = Login { doc: desktop.doc, source: Source::Desktop };
    let token = login.str("accessToken").ok_or_else(|| FetchError::new(crate::claude_desktop::DesktopError::Invalid.message()))?;
    // Confirm the borrowed token matches Desktop's current organization and account before usage.
    let profile = http.get(CLAUDE_PROFILE_URL)
        .bearer_auth(token.trim()).header("Accept", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20").send().await.map_err(|e| net_error("Anthropic", e))?;
    if matches!(profile.status().as_u16(), 401 | 403) {
        return Err(FetchError::new(crate::claude_desktop::DesktopError::Stale.message()));
    }
    if !profile.status().is_success() { return Err(http_error("Anthropic", profile.status(), profile.headers())); }
    let profile: Value = profile.json().await.map_err(|_| FetchError::new("Anthropic sent an unreadable account response."))?;
    if !desktop_identity_matches(&profile, desktop.account, desktop.organization) {
        return Err(FetchError::new("Claude Desktop login changed. Open Claude Desktop, then refresh limits."));
    }
    match claude_usage(http, &token).await? {
        Ok(body) => {
            let resets = claude_resets(&CLAUDE_RESETS, &token, now_ms(), || claude_reset_grants(http, &token)).await;
            Ok(Fetched { plan: claude_plan(&login, Some(&profile)), windows: claude_windows(&body), resets })
        }
        Err((401 | 403, _)) => Err(FetchError::new(crate::claude_desktop::DesktopError::Stale.message())),
        Err((_, error)) => Err(error),
    }
}

fn desktop_identity_matches(profile: &Value, account: Option<uuid::Uuid>, organization: uuid::Uuid) -> bool {
    let id = |path| profile.pointer(path).and_then(Value::as_str).and_then(|s| uuid::Uuid::parse_str(s).ok());
    id("/organization/uuid") == Some(organization) && account.is_none_or(|a| id("/account/uuid") == Some(a))
}

async fn fetch_claude_code(env: &Env, http: &reqwest::Client) -> Result<Fetched, FetchError> {
    let mut logins = Vec::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let env2 = env.clone();
        let (found, keychain_error) = tauri::async_runtime::spawn_blocking(move || claude_logins(&env2)).await
            .map_err(|e| FetchError::new(format!("Cannot read the Claude credentials: {e}")))?;
        match (found.is_empty(), keychain_error) {
            (false, _) => {
                logins = found;
                break;
            }
            // Retry empty reads too: Claude Code can briefly remove its OAuth fields mid-update.
            // After the retries, the caller still checks Desktop before reporting a missing login.
            (true, None) if attempt == 2 => {
                return Err(FetchError::missing("No Claude Code login found. Run `claude` and sign in to see your limits."));
            }
            (true, Some(e)) if attempt == 2 => {
                return Err(FetchError::auth(format!("Couldn't read the Claude login from the keychain ({e}). Will retry.")));
            }
            (true, _) => {}
        }
    }

    let mut last_err = FetchError::auth("Claude login expired. Run `claude` to sign in again.");
    for mut login in logins {
        let scopes = login.field("scopes").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>());
        if scopes.as_ref().is_some_and(|s| !s.is_empty() && !s.contains(&"user:profile")) {
            last_err = FetchError::auth("This Claude login can't read usage. Run `claude` and sign in again.");
            continue;
        }
        let expires = num(login.field("expiresAt")).unwrap_or(f64::MAX);
        let mut refreshed = false;
        if expires - (now_ms() as f64) <= 5.0 * 60.0 * 1000.0 && login.str("refreshToken").is_some() {
            match claude_refresh(http, &mut login).await {
                Ok(()) => refreshed = true,
                Err(e) if !e.auth_fallback => return Err(e),
                Err(e) => {
                    last_err = e;
                    continue;
                }
            }
        }
        loop {
            let token = login.str("accessToken").unwrap_or_default();
            match claude_usage(http, &token).await? {
                Ok(body) => {
                    // Only once usage accepted the token, so a profile failure is never an auth signal.
                    let profile = live_profile(&CLAUDE_PROFILE, &token, || claude_profile(http, &token)).await;
                    let resets = claude_resets(&CLAUDE_RESETS, &token, now_ms(), || claude_reset_grants(http, &token)).await;
                    return Ok(Fetched { plan: claude_plan(&login, profile.as_ref()), windows: claude_windows(&body), resets });
                }
                Err((401, _)) if !refreshed => {
                    match claude_refresh(http, &mut login).await {
                        Ok(()) => refreshed = true,
                        Err(e) if e.auth_fallback => { last_err = e; break; }
                        Err(e) => return Err(e),
                    }
                }
                Err((401 | 403, e)) => {
                    last_err = FetchError::auth(e.message);
                    break;
                }
                Err((_, e)) => return Err(e),
            }
        }
    }
    Err(last_err)
}

fn claude_windows(body: &Value) -> Vec<Window> {
    let mut out = Vec::new();
    let mut push = |key: &str, label: &str, period_ms: i64, primary: bool| {
        if let Some(w) = body.get(key).filter(|w| w.is_object()) {
            if let Some(used) = num(w.get("utilization")) {
                out.push(Window { label: label.into(), used, resets_at: epoch_ms(w.get("resets_at")), period_ms, primary });
            }
        }
    };
    push("five_hour", "Session", SESSION_MS, true);
    push("seven_day", "Weekly", WEEK_MS, true);
    push("seven_day_opus", "Opus", WEEK_MS, false);
    push("seven_day_sonnet", "Sonnet", WEEK_MS, false);
    // Per-model weekly limits moved into `limits[]` (`kind: weekly_scoped`).
    for l in body.get("limits").and_then(Value::as_array).into_iter().flatten() {
        if l.get("kind").and_then(Value::as_str) != Some("weekly_scoped") {
            continue;
        }
        let (Some(name), Some(used)) = (l.pointer("/scope/model/display_name").and_then(Value::as_str), num(l.get("percent"))) else { continue };
        if out.iter().any(|w| w.label == name) {
            continue;
        }
        out.push(Window { label: name.into(), used, resets_at: epoch_ms(l.get("resets_at")), period_ms: WEEK_MS, primary: false });
    }
    out
}

/// The last reset-grants answer for one access token (hashed): when it was asked and what it said.
/// A failed check keeps the previous answer and still counts as the hour's check, so an outage or a
/// 429 is retried an hour later rather than on every refresh (OpenUsage #1355).
type ResetsCache = Mutex<Option<(String, i64, Option<Resets>)>>;
static CLAUDE_RESETS: ResetsCache = Mutex::new(None);

async fn claude_resets<F: Future<Output = Result<Value, ()>>>(cache: &ResetsCache, token: &str, now: i64, fetch: impl FnOnce() -> F) -> Option<Resets> {
    let key = sha256_hex(token.trim(), 16);
    let previous = cache.lock().unwrap_or_else(|e| e.into_inner()).clone().filter(|(k, _, _)| *k == key);
    if let Some((_, checked_at, resets)) = &previous {
        if now - checked_at < CLAUDE_RESETS_INTERVAL_MS {
            return resets.clone();
        }
    }
    let resets = match fetch().await {
        Ok(body) => claude_grants(body.get("cedar_ember"), now),
        Err(()) => previous.and_then(|(_, _, r)| r),
    };
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, now, resets.clone()));
    resets
}

/// Best effort, and only after usage accepted the token, so a failure here only costs the resets row.
async fn claude_reset_grants(http: &reqwest::Client, token: &str) -> Result<Value, ()> {
    let res = claude_request(http, &format!("{CLAUDE_USAGE_URL}?cedar_ember=1&skip_spend=1"), token).send().await.map_err(|_| ())?;
    if !res.status().is_success() { return Err(()); }
    res.json().await.map_err(|_| ())
}

/// `cedar_ember`, Anthropic's one-off usage-limit reset grants. `None` for a missing block (plans
/// outside the program) or a malformed grant: like Claude Code, a half-readable block isn't trusted.
/// Grants past their deadline are skipped and an ineligible account has none available.
fn claude_grants(block: Option<&Value>, now: i64) -> Option<Resets> {
    let block = block?.as_object()?;
    let eligible = block.get("eligible").and_then(Value::as_bool) == Some(true);
    let mut resets = Resets::default();
    for grant in block.get("grants").and_then(Value::as_array).into_iter().flatten() {
        grant.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
        let left = num(grant.get("resets_left")).filter(|n| n.is_finite())?.max(0.0).floor() as u32;
        let ends_at = epoch_ms(grant.get("ends_at"));
        if !eligible || left == 0 || ends_at.is_some_and(|t| t <= now) {
            continue;
        }
        resets.available += left;
        if let Some(t) = ends_at {
            resets.expires_at.extend(std::iter::repeat_n(t, left as usize));
        }
    }
    resets.expires_at.sort_unstable();
    Some(resets)
}

// ── Codex ──────────────────────────────────────────────────────────────────

fn codex_home(env: &Env) -> PathBuf {
    env.var("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
}

fn codex_token(doc: &Value) -> Option<String> {
    doc.pointer("/tokens/access_token").and_then(Value::as_str).map(String::from).filter(|s| !s.is_empty())
}

fn codex_login(env: &Env) -> Option<(Value, Source)> {
    let file = codex_home(env).join("auth.json");
    if let Some(doc) = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        if codex_token(&doc).is_some() {
            return Some((doc, Source::File(file)));
        }
    }
    // Newer CLIs can keep the login in the keychain, keyed by a hash of the canonical home.
    let home = codex_home(env);
    let canonical = std::fs::canonicalize(&home).unwrap_or(home);
    let account = format!("cli|{}", sha256_hex(&canonical.to_string_lossy(), 8));
    let doc: Value = serde_json::from_str(&keychain_read(CODEX_KEYCHAIN_SERVICE, Some(&account))?).ok()?;
    codex_token(&doc).map(|_| (doc, Source::Keychain(CODEX_KEYCHAIN_SERVICE.into(), Some(account))))
}

async fn codex_refresh(http: &reqwest::Client, doc: &mut Value, source: &Source) -> Result<(), FetchError> {
    let expired = || FetchError::new("Codex login expired. Run `codex` to sign in again.");
    let refresh = doc.pointer("/tokens/refresh_token").and_then(Value::as_str).map(String::from).ok_or_else(expired)?;
    let form = [("grant_type", "refresh_token"), ("client_id", CODEX_CLIENT_ID), ("refresh_token", refresh.as_str())];
    let res = http.post(CODEX_REFRESH_URL).form(&form).send().await.map_err(|e| net_error("OpenAI", e))?;
    let status = res.status();
    if status.as_u16() == 400 || status.as_u16() == 401 {
        return Err(expired());
    }
    if !status.is_success() {
        return Err(http_error("OpenAI", status, res.headers()));
    }
    let body: Value = res.json().await.map_err(|_| expired())?;
    let access = body.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(expired)?;

    let apply = |doc: &mut Value| {
        if let Some(tokens) = doc.get_mut("tokens").and_then(Value::as_object_mut) {
            tokens.insert("access_token".into(), json!(access));
            for key in ["refresh_token", "id_token"] {
                if let Some(v) = body.get(key).and_then(Value::as_str) {
                    tokens.insert(key.into(), json!(v));
                }
            }
        }
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("last_refresh".into(), json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)));
        }
    };
    let latest = reread(source)?;
    commit_refresh(doc, latest, "/tokens/refresh_token", &refresh, apply, |doc| write_source(source, doc))
}

pub async fn fetch_codex(env: &Env, http: &reqwest::Client) -> Result<Fetched, FetchError> {
    let env2 = env.clone();
    let login = tauri::async_runtime::spawn_blocking(move || codex_login(&env2)).await
        .map_err(|e| FetchError::new(format!("Cannot read the Codex credentials: {e}")))?;
    let Some((mut doc, source)) = login else {
        return Err(FetchError::missing("Not signed in. Run `codex` and log in with ChatGPT to see your limits."));
    };

    let mut refreshed = false;
    loop {
        let token = codex_token(&doc).unwrap_or_default();
        let mut req = http
            .get(CODEX_USAGE_URL)
            .bearer_auth(&token)
            .header("Accept", "application/json")
            .header("User-Agent", format!("usagerank-app/{}", env!("CARGO_PKG_VERSION")));
        if let Some(account) = doc.pointer("/tokens/account_id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            req = req.header("ChatGPT-Account-Id", account);
        }
        let res = req.send().await.map_err(|e| net_error("OpenAI", e))?;
        let status = res.status();
        if (status.as_u16() == 401 || status.as_u16() == 403) && !refreshed {
            codex_refresh(http, &mut doc, &source).await?;
            refreshed = true;
            continue;
        }
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(FetchError::new("Codex login expired. Run `codex` to sign in again."));
        }
        if !status.is_success() {
            return Err(http_error("OpenAI", status, res.headers()));
        }
        let header_pct = |name: &str| res.headers().get(name).and_then(|h| h.to_str().ok()).and_then(|s| s.trim().parse::<f64>().ok());
        let headers = (header_pct("x-codex-primary-used-percent"), header_pct("x-codex-secondary-used-percent"));
        let body: Value = res.json().await.map_err(|_| FetchError::new("OpenAI sent an unreadable usage response."))?;

        let mut windows = codex_windows(body.get("rate_limit"), ("Session", "Weekly"), headers, true);
        let spark = body.get("additional_rate_limits").and_then(Value::as_array).and_then(|a| {
            a.iter().find(|e| {
                ["limit_name", "metered_feature"]
                    .iter()
                    .any(|k| e.get(*k).and_then(Value::as_str).is_some_and(|s| s.to_lowercase().contains("spark")))
            })
        });
        if let Some(spark) = spark {
            windows.extend(codex_windows(spark.get("rate_limit"), ("Spark", "Spark Weekly"), (None, None), false));
        }
        let account = doc.pointer("/tokens/account_id").and_then(Value::as_str);
        let resets = codex_resets(codex_reset_credits(http, &token, account).await.as_ref(), &body);
        return Ok(Fetched { plan: codex_plan(body.get("plan_type")), windows, resets });
    }
}

/// Best effort: the usage body's count stands in when this fails. The headers mirror the Codex
/// desktop client, which the endpoint expects.
async fn codex_reset_credits(http: &reqwest::Client, token: &str, account: Option<&str>) -> Option<Value> {
    let mut req = http
        .get(CODEX_RESETS_URL)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("User-Agent", format!("usagerank-app/{}", env!("CARGO_PKG_VERSION")))
        .header("OpenAI-Beta", "codex-1")
        .header("originator", "Codex Desktop");
    if let Some(account) = account.filter(|s| !s.is_empty()) {
        req = req.header("ChatGPT-Account-Id", account);
    }
    let res = req.send().await.ok()?;
    if !res.status().is_success() { return None; }
    res.json().await.ok()
}

/// Prefers the dedicated reset-credits answer, the only one that lists each credit's expiry, and
/// falls back to the usage body's `rate_limit_reset_credits` (count only). A credit without a status
/// counts as available; only an explicit other state (consumed, expired) is dropped.
fn codex_resets(dedicated: Option<&Value>, usage: &Value) -> Option<Resets> {
    let source = dedicated
        .filter(|d| num(d.get("available_count")).is_some())
        .or_else(|| usage.get("rate_limit_reset_credits"))?;
    let count = num(source.get("available_count")).filter(|n| n.is_finite() && *n >= 0.0)?;
    let mut expires_at: Vec<i64> = source.get("credits").and_then(Value::as_array).into_iter().flatten()
        .filter(|c| c.get("status").and_then(Value::as_str).is_none_or(|s| s == "available"))
        .filter_map(|c| epoch_ms(c.get("expires_at")))
        .collect();
    expires_at.sort_unstable();
    Some(Resets { available: count.floor() as u32, expires_at })
}

/// Classify by window length when present (Codex can move a lone weekly window into the primary
/// slot), falling back to primary = session, secondary = weekly.
fn codex_windows(rate: Option<&Value>, labels: (&str, &str), headers: (Option<f64>, Option<f64>), primary: bool) -> Vec<Window> {
    let empty = Value::Object(Map::new());
    let slot = |key: &str, header: Option<f64>, fallback_session: bool| -> Option<(Value, f64, bool)> {
        let w = rate.and_then(|r| r.get(key)).filter(|w| w.is_object()).cloned().or_else(|| header.map(|_| empty.clone()))?;
        let used = num(w.get("used_percent")).or(header)?;
        let secs = num(w.get("limit_window_seconds"));
        let is_session = match secs.map(|s| (s * 1000.0) as i64) {
            Some(SESSION_MS) => true,
            Some(WEEK_MS) => false,
            _ => fallback_session,
        };
        Some((w, used, is_session))
    };
    let now = now_ms();
    let mut out: Vec<Window> = Vec::new();
    for (w, used, is_session) in [slot("primary_window", headers.0, true), slot("secondary_window", headers.1, false)].into_iter().flatten() {
        let label = if is_session { labels.0 } else { labels.1 };
        if out.iter().any(|x| x.label == label) {
            continue;
        }
        let resets_at = num(w.get("reset_at"))
            .map(|s| (s * 1000.0) as i64)
            .or_else(|| num(w.get("reset_after_seconds")).map(|s| now + (s * 1000.0) as i64));
        let period_ms = num(w.get("limit_window_seconds")).map(|s| (s * 1000.0) as i64).unwrap_or(if is_session { SESSION_MS } else { WEEK_MS });
        out.push(Window { label: label.into(), used, resets_at, period_ms, primary });
    }
    out.sort_by_key(|w| w.label != labels.0);
    out
}

fn codex_plan(v: Option<&Value>) -> Option<String> {
    let raw = v?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(match raw.to_lowercase().as_str() {
        "prolite" => "Pro 100".into(),
        "pro" => "Pro 200".into(),
        "promax" => "Pro 500".into(),
        "self_serve_business_prolite" => "Business Premium".into(),
        _ => title_case(&raw.replace('_', " ")),
    })
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "reads local Desktop credentials and fetches live limits without changing the login"]
    fn live_desktop_limits_smoke() {
        let desktop = crate::claude_desktop::load(false).unwrap_or_else(|e| panic!("{}", e.message()));
        let fetched = tauri::async_runtime::block_on(fetch_claude_desktop(&client(), desktop))
            .unwrap_or_else(|e| panic!("{}", e.message));
        assert!(!fetched.windows.is_empty(), "Expected subscription usage windows");
        eprintln!("Desktop fallback: account verified, {} usage windows fetched.", fetched.windows.len());
    }

    #[test]
    fn claude_plan_prefers_live_profile() {
        let login = Login { doc: json!({"claudeAiOauth":{"subscriptionType":"max","rateLimitTier":"default_claude_max_5x"}}), source: Source::Desktop };
        assert_eq!(claude_plan(&login, None).as_deref(), Some("Max 5x"));
        let profile = json!({"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_20x"}});
        assert_eq!(claude_plan(&login, Some(&profile)).as_deref(), Some("Max 20x"));
        assert_eq!(claude_plan(&login, Some(&json!({}))).as_deref(), Some("Max 5x"));
    }

    #[test]
    fn live_profile_is_fetched_once_per_token() {
        let cache: ProfileCache = Mutex::new(None);
        let calls = std::cell::Cell::new(0);
        let fetch = |result: Option<Value>| { calls.set(calls.get() + 1); async move { result } };
        let profile = json!({"organization":{"rate_limit_tier":"default_claude_max_20x"}});
        tauri::async_runtime::block_on(async {
            assert_eq!(live_profile(&cache, "a", || fetch(None)).await, None);
            assert_eq!(live_profile(&cache, "a", || fetch(Some(profile.clone()))).await, None);
            assert_eq!(live_profile(&cache, "b", || fetch(Some(profile.clone()))).await, Some(profile.clone()));
            assert_eq!(live_profile(&cache, "b", || fetch(None)).await, Some(profile.clone()));
        });
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn claude_grants_count_live_resets_only() {
        let now = 1_000_000_000_000;
        let block = json!({"eligible":true,"next_grant_id":"a","grants":[
            {"id":"a","resets_left":2,"ends_at":"2001-09-10T01:46:40Z","clears":["five_hour","seven_day"]},
            {"id":"b","resets_left":1,"ends_at":now - 1},
            {"id":"c","resets_left":0,"ends_at":now + 1},
            {"id":"d","resets_left":1}]});
        let resets = claude_grants(Some(&block), now).unwrap();
        assert_eq!(resets.available, 3);
        assert_eq!(resets.expires_at, vec![now + 86_400_000; 2]);

        let ineligible = json!({"eligible":false,"grants":[{"id":"a","resets_left":2}]});
        assert_eq!(claude_grants(Some(&ineligible), now), Some(Resets::default()));
        assert_eq!(claude_grants(Some(&Value::Null), now), None);
        assert_eq!(claude_grants(None, now), None);
        assert_eq!(claude_grants(Some(&json!({"eligible":true,"grants":[{"resets_left":1}]})), now), None);
    }

    #[test]
    fn claude_resets_are_checked_hourly_and_survive_failures() {
        let cache: ResetsCache = Mutex::new(None);
        let calls = std::cell::Cell::new(0);
        let ok = json!({"cedar_ember":{"eligible":true,"grants":[{"id":"a","resets_left":1}]}});
        let fetch = |result: Result<Value, ()>| { calls.set(calls.get() + 1); async move { result } };
        let one = Some(Resets { available: 1, expires_at: vec![] });
        let hour = CLAUDE_RESETS_INTERVAL_MS;
        tauri::async_runtime::block_on(async {
            assert_eq!(claude_resets(&cache, "a", 0, || fetch(Ok(ok.clone()))).await, one);
            assert_eq!(claude_resets(&cache, "a", hour - 1, || fetch(Ok(json!({})))).await, one);
            assert_eq!(claude_resets(&cache, "a", hour, || fetch(Err(()))).await, one);
            assert_eq!(claude_resets(&cache, "a", hour + 1, || fetch(Ok(json!({})))).await, one);
            assert_eq!(claude_resets(&cache, "b", hour + 1, || fetch(Err(()))).await, None);
        });
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn codex_resets_prefer_the_dedicated_answer() {
        let usage = json!({"rate_limit_reset_credits":{"available_count":3}});
        let dedicated = json!({"available_count":2,"credits":[
            {"status":"available","expires_at":"2001-09-09T01:46:41Z"},
            {"expires_at":1_000_000_000},
            {"status":"consumed","expires_at":5}]});
        assert_eq!(codex_resets(Some(&dedicated), &usage), Some(Resets { available: 2, expires_at: vec![1_000_000_000_000, 1_000_000_001_000] }));
        assert_eq!(codex_resets(Some(&json!({"available_count":null})), &usage), Some(Resets { available: 3, expires_at: vec![] }));
        assert_eq!(codex_resets(None, &usage).map(|r| r.available), Some(3));
        assert_eq!(codex_resets(None, &json!({})), None);
    }

    #[test]
    fn desktop_credentials_cannot_be_written_or_refreshed() {
        let doc = json!({"claudeAiOauth":{"accessToken":"desktop"}});
        assert!(write_source(&Source::Desktop, &doc).is_err());
        let mut login = Login { doc, source: Source::Desktop };
        let error = tauri::async_runtime::block_on(claude_refresh(&client(), &mut login)).err().unwrap();
        assert!(error.auth_fallback);
        assert!(error.message.contains("Desktop"));
    }

    #[test]
    fn refresh_auth_fallback_requires_invalid_grant() {
        for status in [reqwest::StatusCode::BAD_REQUEST, reqwest::StatusCode::UNAUTHORIZED] {
            assert!(claude_refresh_error(status, &json!({"error":"invalid_grant"})).auth_fallback);
            for body in [Value::Null, json!({"error":"gateway_failure"})] {
                let error = claude_refresh_error(status, &body);
                assert!(!error.auth_fallback);
                assert!(!error.message.contains("expired"));
            }
        }
        let rate_limit = http_error("Anthropic", reqwest::StatusCode::TOO_MANY_REQUESTS, &reqwest::header::HeaderMap::new());
        assert!(!rate_limit.auth_fallback);
        assert_eq!(rate_limit.retry_after, Some(300));
    }

    #[test]
    fn desktop_profile_must_match_current_account_and_organization() {
        let account = uuid::Uuid::new_v4();
        let organization = uuid::Uuid::new_v4();
        let profile = json!({"account":{"uuid":account},"organization":{"uuid":organization}});
        assert!(desktop_identity_matches(&profile, Some(account), organization));
        assert!(!desktop_identity_matches(&profile, Some(uuid::Uuid::new_v4()), organization));
        assert!(!desktop_identity_matches(&profile, Some(account), uuid::Uuid::new_v4()));
        assert!(!desktop_identity_matches(&Value::Null, Some(account), organization));
    }

    #[test]
    fn refresh_save_failure_is_reported_without_replacing_credentials() {
        let original = json!({"tokens":{"access_token":"old","refresh_token":"old-refresh"}});
        let mut doc = original.clone();
        let result = commit_refresh(&mut doc, original.clone(), "/tokens/refresh_token", "old-refresh",
            |latest| latest["tokens"]["access_token"] = json!("new"),
            |_| Err(FetchError::new("disk full")));
        assert_eq!(result.err().unwrap().message, "disk full");
        assert_eq!(doc, original);
    }

    #[test]
    fn concurrent_refresh_uses_newer_login_without_overwriting_it() {
        let mut doc = json!({"tokens":{"access_token":"old","refresh_token":"old-refresh"}});
        let latest = json!({"tokens":{"access_token":"other-client","refresh_token":"other-refresh"}});
        let result = commit_refresh(&mut doc, latest.clone(), "/tokens/refresh_token", "old-refresh",
            |_| panic!("must not modify a newer login"),
            |_| panic!("must not save over a newer login"));
        assert!(result.is_ok());
        assert_eq!(doc, latest);
    }

    #[test]
    fn file_refresh_saves_all_rotated_tokens() {
        let root = std::env::temp_dir().join(format!("ur-errors-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = Source::File(root.join("auth.json"));
        let mut doc = json!({"tokens":{"access_token":"old","refresh_token":"old-refresh"}});
        assert!(write_source(&source, &doc).is_ok());
        let latest = reread(&source).ok().unwrap();
        let result = commit_refresh(&mut doc, latest, "/tokens/refresh_token", "old-refresh",
            |latest| {
                latest["tokens"]["access_token"] = json!("new");
                latest["tokens"]["refresh_token"] = json!("new-refresh");
            }, |latest| write_source(&source, latest));
        assert!(result.is_ok());
        assert_eq!(reread(&source).ok().unwrap(), doc);
        assert_eq!(doc["tokens"]["refresh_token"], "new-refresh");
        std::fs::remove_dir_all(root).unwrap();
    }
}
