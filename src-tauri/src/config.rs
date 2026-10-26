//! Local state, shared with the `usagerank` CLI so one machine has one key and one scan history.
//!
//! `~/.usagerank/config.json` holds the handle and the ed25519 key, `state.json` the per-file daily
//! rollups. Both use the CLI's exact format and are only written once the user links this Mac.
//! This app's own settings and its local scan cache live in its data folder
//! (`~/Library/Application Support/com.usagerank.menubar`), so the local view works without an account.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// DER prefix of an ed25519 SubjectPublicKeyInfo; the raw 32-byte key follows it.
const SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
/// DER prefix of an ed25519 PKCS#8 private key; the 32-byte seed follows it.
const PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

const DEFAULT_API_URL: &str = "https://usagerank.com/api/cli/usage";
const DEFAULT_WEB_URL: &str = "https://usagerank.com";

/// Where state lives and which server to talk to. Real environment variables win over
/// `~/.usagerank/.env`, the same precedence the CLI uses.
#[derive(Debug, Clone)]
pub struct Env {
    pub home: PathBuf,
    pub app_dir: PathBuf,
    pub api_url: String,
    pub web_url: String,
    vars: HashMap<String, String>,
}

impl Env {
    pub fn load() -> Self {
        let user_home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let home = std::env::var("USAGERANK_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| user_home.join(".usagerank"));
        let vars = read_env_file(&home.join(".env"));
        let app_dir = std::env::var("USAGERANK_APP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::data_dir().unwrap_or_else(|| user_home.clone()).join("com.usagerank.menubar"));
        let mut env = Env { home, app_dir, api_url: String::new(), web_url: String::new(), vars };
        env.api_url = env.var("USAGERANK_API_URL").unwrap_or_else(|| DEFAULT_API_URL.into());
        env.web_url = env
            .var("USAGERANK_URL")
            .unwrap_or_else(|| DEFAULT_WEB_URL.into())
            .trim_end_matches('/')
            .to_string();
        env
    }

    pub fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty()).or_else(|| self.vars.get(key).cloned())
    }

    pub fn config_path(&self) -> PathBuf {
        self.home.join("config.json")
    }

    pub fn state_path(&self) -> PathBuf {
        self.home.join("state.json")
    }

    pub fn app_path(&self) -> PathBuf {
        self.app_dir.join("app.json")
    }

    /// Per-file rollups for the local view. Separate from `state.json`, which records what was pushed.
    pub fn local_cache_path(&self) -> PathBuf {
        self.app_dir.join("scan-cache.json")
    }
}

fn read_env_file(file: &Path) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    let Ok(text) = fs::read_to_string(file) else { return vars };
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || key.starts_with('#') || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = value.trim();
        let unquoted = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
            .unwrap_or(value);
        vars.entry(key.to_string()).or_insert_with(|| unquoted.to_string());
    }
    vars
}

pub fn read_json<T: for<'de> Deserialize<'de>>(file: &Path) -> Result<Option<T>, String> {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Cannot read {}: {e}", file.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("Cannot parse {}: {e}. The file was not changed.", file.display()))
}

/// Writes owner-only, like the CLI: the config holds the private key.
pub fn write_json<T: Serialize>(file: &Path, data: &T) -> Result<(), String> {
    let dir = file.parent().ok_or("bad path")?;
    fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| format!("Cannot protect {}: {e}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
    write_atomic(file, json.as_bytes())
}

pub fn write_atomic(file: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = file.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&tmp).map_err(|e| format!("Cannot write {}: {e}", file.display()))?;
        output.write_all(bytes).map_err(|e| format!("Cannot write {}: {e}", file.display()))?;
        output.sync_all().map_err(|e| format!("Cannot save {}: {e}", file.display()))?;
        drop(output);
        fs::rename(&tmp, file).map_err(|e| format!("Cannot replace {}: {e}", file.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logged_in_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `config.json`, field for field what the CLI writes. Unknown keys are kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub handle: String,
    pub machine_id: String,
    pub public_key: String,
    pub private_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Config {
    pub fn load(env: &Env) -> Result<Option<Self>, String> {
        read_json(&env.config_path())
    }

    pub fn save(&self, env: &Env) -> Result<(), String> {
        Self::load(env)?;
        write_json(&env.config_path(), self)
    }

    /// A fresh machine: new ed25519 key pair and machine id, in the CLI's PEM / DER formats.
    pub fn create(env: &Env, handle: &str) -> Result<Self, String> {
        if Self::load(env)?.is_some() {
            return Err("This Mac already has a configuration. Its keys were not changed.".into());
        }
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| format!("no randomness: {e}"))?;
        let key = SigningKey::from_bytes(&seed);

        let mut spki = SPKI_PREFIX.to_vec();
        spki.extend_from_slice(key.verifying_key().as_bytes());
        let mut pkcs8 = PKCS8_PREFIX.to_vec();
        pkcs8.extend_from_slice(&seed);
        let body = B64.encode(&pkcs8);
        let lines: Vec<&str> = body.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();

        let cfg = Config {
            handle: handle.to_string(),
            machine_id: uuid::Uuid::new_v4().to_string(),
            public_key: B64.encode(&spki),
            private_key: format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", lines.join("\n")),
            created_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            account: None,
            extra: Map::new(),
        };
        cfg.save(env)?;
        Ok(cfg)
    }

    fn signing_key(&self) -> Result<SigningKey, String> {
        let body: String = self.private_key.lines().filter(|l| !l.starts_with("-----")).collect();
        let der = B64.decode(body.trim()).map_err(|_| "config.json: private key is not valid PEM".to_string())?;
        if der.len() != 48 || der[..16] != PKCS8_PREFIX {
            return Err("config.json: private key is not an ed25519 key".into());
        }
        let seed: [u8; 32] = der[16..].try_into().map_err(|_| "config.json: bad key length".to_string())?;
        Ok(SigningKey::from_bytes(&seed))
    }

    /// Base64 ed25519 signature of `message`.
    pub fn sign(&self, message: &[u8]) -> Result<String, String> {
        Ok(B64.encode(self.signing_key()?.sign(message).to_bytes()))
    }
}

/// Handles the server accepts: 2–32 chars, a-z 0-9 . _ -, starting with a letter or digit.
pub fn normalize_handle(input: &str) -> Result<String, String> {
    let handle = input.trim().trim_start_matches('@').to_lowercase();
    let mut chars = handle.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if !first_ok || !rest_ok || handle.len() < 2 || handle.len() > 32 {
        return Err("Handles are 2–32 characters: a–z, 0–9, dot, underscore or dash.".into());
    }
    Ok(handle)
}

pub fn default_handle() -> String {
    let user = std::env::var("USER").unwrap_or_default();
    normalize_handle(&user).unwrap_or_default()
}

pub fn wipe_local(env: &Env) -> Result<(), String> {
    match fs::remove_dir_all(&env.home) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Cannot remove {}: {e}", env.home.display())),
    }
}

/// What the menu bar item shows next to the icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Display {
    Tokens,
    Cost,
    Rank,
    Icon,
    /// Claude / Codex session and weekly usage, drawn as an image.
    Limits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub display: Display,
    pub interval_minutes: u32,
    pub claude: bool,
    pub codex: bool,
    /// Show limits as percent used (like Claude's own usage page) instead of percent left.
    pub show_used: bool,
    /// Show the Limit resets row (Claude reset grants, Codex reset credits) on the Limits tab.
    pub show_resets: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { display: Display::Tokens, interval_minutes: 30, claude: true, codex: true, show_used: false, show_resets: true }
    }
}

impl Settings {
    /// Push only one tool when the other is switched off, like `push --only`.
    pub fn only(&self) -> Option<&'static str> {
        match (self.claude, self.codex) {
            (true, false) => Some("claude"),
            (false, true) => Some("codex"),
            _ => None,
        }
    }
}

/// `app.json`: settings plus the last sync result, so the popover has numbers right after launch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppFile {
    pub settings: Settings,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
    pub stats: Option<Value>,
    pub stats_error: Option<String>,
    /// Last successful live limits per provider, so a relaunch or a rate-limited check still has numbers to show.
    pub limits: crate::limits::Limits,
    /// Popover choices (selected tab, period, filters) so they survive a relaunch; the webview's
    /// localStorage doesn't reliably.
    pub ui: Map<String, Value>,
}

impl AppFile {
    pub fn load(env: &Env) -> Result<Self, String> {
        // Early builds kept app.json in ~/.usagerank; read it once.
        match read_json(&env.app_path())? {
            Some(app) => Ok(app),
            None => Ok(read_json(&env.home.join("app.json"))?.unwrap_or_default()),
        }
    }

    pub fn save(&self, env: &Env) -> Result<(), String> {
        // A failed startup read must never overwrite the user's existing settings.
        Self::load(env)?;
        write_json(&env.app_path(), self)
    }
}

fn claude_settings_path(env: &Env) -> PathBuf {
    let root = env
        .var("CLAUDE_CONFIG_DIR")
        .and_then(|v| v.split(',').next().map(|s| PathBuf::from(s.trim())))
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"));
    root.join("settings.json")
}

/// Claude Code deletes transcripts after `cleanupPeriodDays` (default 30). None when Claude Code
/// isn't installed or its settings can't be read.
pub fn claude_retention_days(env: &Env) -> Option<u64> {
    let file = claude_settings_path(env);
    if !file.parent().is_some_and(|p| p.exists()) {
        return None;
    }
    if !file.exists() {
        return Some(30);
    }
    let settings: Value = read_json(&file).ok().flatten()?;
    Some(settings.get("cleanupPeriodDays").and_then(Value::as_u64).unwrap_or(30))
}

/// Keep Claude Code transcripts for a year, so usage isn't lost between syncs.
pub fn extend_claude_retention(env: &Env) -> Result<(), String> {
    let file = claude_settings_path(env);
    let mut settings: Value = if file.exists() {
        read_json(&file)?.ok_or("Claude Code settings.json is missing.")?
    } else {
        Value::Object(Map::new())
    };
    let obj = settings.as_object_mut().ok_or("Claude Code settings.json is not an object.")?;
    obj.insert("cleanupPeriodDays".into(), Value::from(365));
    let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())? + "\n";
    fs::write(&file, json).map_err(|e| format!("can't write {}: {e}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_pem() {
        let dir = std::env::temp_dir().join(format!("usagerank-test-{}", uuid::Uuid::new_v4()));
        let env = Env { home: dir.clone(), app_dir: dir.clone(), api_url: String::new(), web_url: String::new(), vars: HashMap::new() };
        let cfg = Config::create(&env, "tester").unwrap();
        let loaded = Config::load(&env).unwrap().unwrap();
        assert_eq!(loaded.public_key, cfg.public_key);
        assert!(loaded.private_key.starts_with("-----BEGIN PRIVATE KEY-----\n"));

        let spki = B64.decode(&loaded.public_key).unwrap();
        assert_eq!(spki.len(), 44);
        let verifying = ed25519_dalek::VerifyingKey::from_bytes(spki[12..].try_into().unwrap()).unwrap();
        let sig = B64.decode(loaded.sign(b"hello").unwrap()).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig).unwrap();
        assert!(verifying.verify_strict(b"hello", &sig).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn handles_are_normalized() {
        assert_eq!(normalize_handle("@Mira.Builds").unwrap(), "mira.builds");
        assert!(normalize_handle("a").is_err());
        assert!(normalize_handle("-nope").is_err());
        assert!(normalize_handle("has space").is_err());
    }
}

#[cfg(test)]
pub(crate) fn test_env(root: &Path) -> Env {
    Env {
        home: root.join("home"), app_dir: root.join("app"),
        api_url: "http://127.0.0.1/usage".into(), web_url: "http://127.0.0.1".into(),
        vars: HashMap::from([
            ("CLAUDE_CONFIG_DIR".into(), root.join("claude").to_string_lossy().into()),
            ("CODEX_HOME".into(), root.join("codex").to_string_lossy().into()),
        ]),
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn missing_and_invalid_json_have_different_results() {
        let root = std::env::temp_dir().join(format!("ur-errors-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("config.json");
        assert!(read_json::<Value>(&file).unwrap().is_none());
        fs::write(&file, "{broken").unwrap();
        assert!(read_json::<Value>(&file).unwrap_err().contains("Cannot parse"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_identity_and_settings_are_not_overwritten() {
        let root = std::env::temp_dir().join(format!("ur-errors-{}", uuid::Uuid::new_v4()));
        let env = test_env(&root);
        fs::create_dir_all(&env.home).unwrap();
        fs::create_dir_all(&env.app_dir).unwrap();
        fs::write(env.config_path(), "{broken identity").unwrap();
        fs::write(env.app_path(), "{broken settings").unwrap();
        assert!(Config::create(&env, "tester").is_err());
        assert!(AppFile::default().save(&env).is_err());
        assert_eq!(fs::read_to_string(env.config_path()).unwrap(), "{broken identity");
        assert_eq!(fs::read_to_string(env.app_path()).unwrap(), "{broken settings");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_reports_failure_and_accepts_missing_folder() {
        let root = std::env::temp_dir().join(format!("ur-errors-{}", uuid::Uuid::new_v4()));
        let env = test_env(&root);
        fs::create_dir_all(&root).unwrap();
        assert!(wipe_local(&env).is_ok());
        fs::write(&env.home, "this is a file").unwrap();
        assert!(wipe_local(&env).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
