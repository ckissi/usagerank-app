//! Read-only Claude Desktop fallback, following OpenUsage's ClaudeDesktopAuthStore.
//! Only the current account/organization's access token is borrowed. Desktop owns renewal.
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

#[cfg(target_os = "macos")]
mod native;

#[derive(Debug, PartialEq)]
pub(crate) enum DesktopError { NotFound, PermissionRequired, Stale, Invalid, Keychain }

impl DesktopError {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::NotFound => "No Claude Desktop login found.",
            Self::PermissionRequired => "Claude Desktop login found. Refresh limits and choose Always Allow when macOS asks for access.",
            Self::Stale => "Claude Desktop login is stale. Open Claude Desktop, then refresh limits.",
            Self::Invalid => "Claude Desktop login couldn't be read. Open Claude Desktop, then refresh limits.",
            Self::Keychain => "Couldn't read Claude Desktop's Keychain item. Unlock the keychain, then refresh limits.",
        }
    }
}

pub(crate) struct DesktopLogin {
    pub doc: Value,
    pub account: Option<Uuid>,
    pub organization: Uuid,
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn load(_allow_interaction: bool) -> Result<DesktopLogin, DesktopError> {
    Err(DesktopError::NotFound)
}

#[cfg(target_os = "macos")]
pub(crate) fn load(allow_interaction: bool) -> Result<DesktopLogin, DesktopError> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let root = dirs::home_dir().ok_or(DesktopError::NotFound)?.join("Library/Application Support/Claude");
    let text = std::fs::read_to_string(root.join("config.json")).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { DesktopError::NotFound } else { DesktopError::Invalid }
    })?;
    let config: Value = serde_json::from_str(&text).map_err(|_| DesktopError::Invalid)?;
    if !["oauth:tokenCacheV2", "oauth:tokenCache"].iter().any(|k| config.get(*k).is_some_and(Value::is_string)) {
        return Err(DesktopError::NotFound);
    }
    let key = native::storage_key(allow_interaction)?;
    let organization = active_organization(&root, &key)?;
    let account = config.get("lastKnownAccountUuid").and_then(Value::as_str).and_then(|s| Uuid::parse_str(s).ok());
    let decode = |name| -> Result<Option<Map<String, Value>>, DesktopError> {
        let Some(stored) = config.get(name).and_then(Value::as_str) else { return Ok(None) };
        let encrypted = STANDARD.decode(stored).map_err(|_| DesktopError::Invalid)?;
        let cache: Value = serde_json::from_slice(&native::decrypt(&encrypted, &key)?).map_err(|_| DesktopError::Invalid)?;
        cache.as_object().cloned().map(Some).ok_or(DesktopError::Invalid)
    };
    let doc = select(organization, account, decode("oauth:tokenCacheV2")?.as_ref(),
        decode("oauth:tokenCache")?.as_ref(), chrono::Utc::now().timestamp_millis())?;
    Ok(DesktopLogin { doc, account, organization })
}

#[cfg(target_os = "macos")]
fn active_organization(root: &std::path::Path, key: &[u8; 16]) -> Result<Uuid, DesktopError> {
    use sha2::{Digest, Sha256};
    for relative in ["Cookies", "Network/Cookies"] {
        let db = root.join(relative);
        if !db.exists() { continue; }
        for host in [".claude.ai", "claude.ai"] {
            let sql = format!("SELECT CASE WHEN length(value)>0 THEN 'plain:' || hex(CAST(value AS BLOB)) ELSE 'encrypted:' || hex(encrypted_value) END FROM cookies WHERE name='lastActiveOrg' AND host_key='{host}' ORDER BY last_update_utc DESC LIMIT 1;");
            let output = std::process::Command::new("/usr/bin/sqlite3")
                .args(["-readonly", "-cmd", ".timeout 1000"]).arg(&db).arg(sql).output()
                .map_err(|_| DesktopError::Invalid)?;
            if !output.status.success() { return Err(DesktopError::Invalid); }
            let text = String::from_utf8_lossy(&output.stdout);
            let Some((mode, hex)) = text.trim().split_once(':') else { continue };
            let stored = decode_hex(hex).ok_or(DesktopError::Invalid)?;
            let value = if mode == "plain" { stored } else {
                let decrypted = native::decrypt(&stored, key)?;
                let hash = Sha256::digest(host.as_bytes());
                let Some(value) = decrypted.strip_prefix(hash.as_slice()) else { continue };
                value.to_vec()
            };
            if let Some(org) = std::str::from_utf8(&value).ok().and_then(|s| Uuid::parse_str(s).ok()) {
                return Ok(org);
            }
        }
    }
    Err(DesktopError::Invalid)
}

#[cfg(target_os = "macos")]
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i+2], 16).ok()).collect()
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct CacheKey { client: Uuid, organization: Uuid, scopes: Vec<String> }

fn normalize(cache: Option<&Map<String, Value>>, account: Option<Uuid>) -> BTreeMap<CacheKey, &Value> {
    let mut legacy = BTreeMap::new();
    let mut scoped = BTreeMap::new();
    for (raw, value) in cache.into_iter().flatten() {
        let (key, is_scoped) = if let Some(rest) = raw.strip_prefix("acct:") {
            let Some((owner, key)) = rest.split_once('|') else { continue };
            let Ok(owner) = Uuid::parse_str(owner) else { continue };
            if Some(owner) != account { continue; }
            (key, true)
        } else { (raw.as_str(), false) };
        let Some((prefix, scopes)) = key.split_once(":https://api.anthropic.com:") else { continue };
        let Some((client, organization)) = prefix.split_once(':') else { continue };
        let (Ok(client), Ok(organization)) = (Uuid::parse_str(client), Uuid::parse_str(organization)) else { continue };
        let key = CacheKey { client, organization, scopes: scopes.split_whitespace().map(String::from).collect() };
        if is_scoped { scoped.insert(key, value); } else { legacy.insert(key, value); }
    }
    legacy.extend(scoped);
    legacy
}

fn select(organization: Uuid, account: Option<Uuid>, v2: Option<&Map<String, Value>>,
    v1: Option<&Map<String, Value>>, now: i64) -> Result<Value, DesktopError> {
    let v2 = normalize(v2, account);
    let mut v1 = normalize(v1, account);
    v1.retain(|key, _| !v2.contains_key(key));
    let mut stale = false;
    let mut invalid = false;
    for cache in [&v2, &v1] {
        let mut best = None;
        for (key, entry) in cache {
            if key.organization != organization || !key.scopes.iter().any(|s| s == "user:profile") || entry.is_null() { continue; }
            let token = entry.get("token").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
            let expires = entry.get("expiresAt").and_then(Value::as_f64).filter(|n| n.is_finite());
            let (Some(token), Some(expires)) = (token, expires) else { invalid = true; continue };
            if expires <= now as f64 + 120_000.0 { stale = true; continue; }
            let full = key.scopes.iter().any(|s| s == "user:inference");
            let production = key.client.to_string() == "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
            let rank = (full && production, full, key.scopes.len(), expires);
            // Copy ONLY supported access-token fields. Never carry Desktop's refresh token.
            let doc = json!({"claudeAiOauth": {"accessToken": token, "expiresAt": expires,
                "scopes": key.scopes, "subscriptionType": entry.get("subscriptionType"),
                "rateLimitTier": entry.get("rateLimitTier")}});
            if best.as_ref().is_none_or(|(r, _)| rank > *r) { best = Some((rank, doc)); }
        }
        if let Some((_, doc)) = best { return Ok(doc); }
    }
    Err(if stale { DesktopError::Stale } else if invalid { DesktopError::Invalid } else { DesktopError::NotFound })
}

#[cfg(test)]
mod tests;
