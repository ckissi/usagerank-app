use super::*;

const ACCOUNT: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
const OTHER: &str = "ffffffff-ffff-4fff-8fff-ffffffffffff";
const ORG: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const NOW: i64 = 1_800_000_000_000;

fn key() -> String { format!("{CLIENT}:{ORG}:https://api.anthropic.com:user:profile user:inference") }
fn scoped() -> String { format!("acct:{ACCOUNT}|{}", key()) }
fn entry(token: &str, expires: i64) -> Value {
    json!({"token": token, "expiresAt": NOW + expires, "refreshToken": "must-never-be-used"})
}
fn pick(v2: Value, v1: Value) -> Result<Value, DesktopError> {
    select(Uuid::parse_str(ORG).unwrap(), Some(Uuid::parse_str(ACCOUNT).unwrap()), v2.as_object(), v1.as_object(), NOW)
}

#[test]
fn both_cache_versions_support_legacy_and_account_scoped_tokens_without_refresh_tokens() {
    for cache_key in [key(), scoped()] {
        for v2 in [true, false] {
            let cache = json!({cache_key.clone(): entry("current", 300_000)});
            let doc = if v2 { pick(cache, Value::Null) } else { pick(Value::Null, cache) }.unwrap();
            assert_eq!(doc.pointer("/claudeAiOauth/accessToken").unwrap(), "current");
            assert!(doc.pointer("/claudeAiOauth/refreshToken").is_none());
        }
    }
}

#[test]
fn scoped_entries_override_legacy_and_v2_overrides_v1_even_with_later_expiry() {
    let doc = pick(json!({key(): entry("legacy", 900_000), scoped(): entry("current", 300_000)}),
        json!({scoped(): entry("v1", 999_000)})).unwrap();
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "current");
}

#[test]
fn deletion_markers_and_expired_tokens_never_revive_old_copies() {
    for new_key in [key(), scoped()] {
        for old_key in [key(), scoped()] {
            assert_eq!(pick(json!({new_key.clone(): null}), json!({old_key.clone(): entry("old", 999_000)})), Err(DesktopError::NotFound));
            assert_eq!(pick(json!({new_key.clone(): entry("stale", 0)}), json!({old_key: entry("old", 999_000)})), Err(DesktopError::Stale));
        }
    }
    assert_eq!(pick(json!({key(): entry("legacy", 999_000), scoped(): null}), Value::Null), Err(DesktopError::NotFound));
}

#[test]
fn foreign_accounts_cannot_supply_tokens_or_suppress_current_account() {
    let foreign = format!("acct:{OTHER}|{}", key());
    for value in [entry("foreign", 999_000), Value::Null] {
        assert_eq!(pick(json!({foreign.clone(): value.clone()}), Value::Null), Err(DesktopError::NotFound));
        let doc = pick(json!({foreign.clone(): value}), json!({scoped(): entry("current", 300_000)})).unwrap();
        assert_eq!(doc["claudeAiOauth"]["accessToken"], "current");
    }
    assert_eq!(select(Uuid::parse_str(ORG).unwrap(), None, json!({scoped(): entry("scoped", 300_000)}).as_object(), None, NOW), Err(DesktopError::NotFound));
}

#[test]
fn wrong_organization_host_scope_and_malformed_keys_are_excluded() {
    for invalid_key in [key().replace(ORG, OTHER), key().replace("api.anthropic.com", "example.com"),
        key().replace("user:profile ", ""), format!("acct:bad|{}", key()), key().replace(CLIENT, "bad")] {
        assert_eq!(pick(json!({invalid_key: entry("wrong", 300_000)}), Value::Null), Err(DesktopError::NotFound));
    }
}

#[test]
fn production_full_scope_token_wins_over_longer_lived_partial_scope_tokens() {
    let profile_only = key().replace(" user:inference", "");
    let foreign_client = key().replace(CLIENT, OTHER);
    let doc = pick(json!({profile_only: entry("partial", 999_000), foreign_client: entry("other-client", 999_000),
        key(): entry("full", 300_000)}), Value::Null).unwrap();
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "full");
}

#[test]
fn near_expiry_and_malformed_entries_are_not_usable() {
    for remaining in [-1, 0, 119_999, 120_000] {
        assert_eq!(pick(json!({key(): entry("expiring", remaining)}), Value::Null), Err(DesktopError::Stale));
    }
    for malformed in [json!({"token":" "}), json!({"token":"x","expiresAt":"tomorrow"})] {
        assert_eq!(pick(json!({scoped(): malformed, key(): entry("old", 999_000)}), Value::Null), Err(DesktopError::Invalid));
    }
}

#[test]
fn uuid_matching_ignores_case() {
    let doc = pick(json!({scoped().replace(ACCOUNT, &ACCOUNT.to_uppercase()).replace(ORG, &ORG.to_uppercase()): entry("current", 300_000)}), Value::Null).unwrap();
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "current");
}

#[cfg(target_os = "macos")]
#[test]
fn electron_crypto_matches_independent_pbkdf2_and_openssl_fixture() {
    let key = native::derive_key(b"test-password").unwrap();
    assert_eq!(key.to_vec(), decode_hex("c0ffe4c25f07f62bfc6ab011d9efa54e").unwrap());
    let ciphertext = decode_hex("763130bbe04dcf0d1cf6762b7fa9a4827a00bcf761eb65d8d34280d1d34a61e5f7d7ca").unwrap();
    assert_eq!(native::decrypt(&ciphertext, &key).unwrap(), br#"{"fixture":true}"#);
    assert!(native::decrypt(b"v11bad", &key).is_err());
    assert!(native::decrypt(b"v10bad", &key).is_err());
    assert!(native::decrypt(&ciphertext, &[0; 16]).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn reads_encrypted_organization_cookie_and_rejects_wrong_host_binding() {
    let root = std::env::temp_dir().join(format!("ur-desktop-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let encrypted = "763130cba8d8b3b813f784aae46dea9258b58b3d19f5f789dc4778df01527afd73e93e7abc5042dc269080fec5adb400e96cdfcc7bfbac96f76c7689be7d9e38d9d52fb3bd79351dbf157fe28d78ee8635e880";
    let execute = |sql: &str| {
        assert!(std::process::Command::new("/usr/bin/sqlite3").arg(root.join("Cookies")).arg(sql).status().unwrap().success());
    };
    execute(&format!("CREATE TABLE cookies(name TEXT,host_key TEXT,value TEXT,encrypted_value BLOB,last_update_utc INTEGER); INSERT INTO cookies VALUES('lastActiveOrg','.claude.ai','',X'{encrypted}',1);"));
    let key = native::derive_key(b"test-password").unwrap();
    assert_eq!(active_organization(&root, &key).unwrap(), Uuid::parse_str(ORG).unwrap());
    execute("UPDATE cookies SET host_key='claude.ai';");
    assert_eq!(active_organization(&root, &key), Err(DesktopError::Invalid));
    execute(&format!("UPDATE cookies SET value='{ORG}';"));
    assert_eq!(active_organization(&root, &key).unwrap(), Uuid::parse_str(ORG).unwrap());
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "reads the local Desktop login without prompting; prints status only, never credentials"]
fn local_desktop_read_is_non_interactive() {
    match load(false) {
        Ok(_) => eprintln!("Desktop fallback: valid access token loaded."),
        Err(DesktopError::Invalid) => panic!("Desktop fallback: local cache could not be decoded."),
        Err(error) => eprintln!("Desktop fallback: {}", error.message()),
    }
}
