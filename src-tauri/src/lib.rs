mod api;
mod claude_desktop;
mod config;
mod limits;
mod local;
mod login_item;
mod pricing;
mod scan;
mod tray_image;
mod updater;

use api::{Api, ApiError};
use config::{AppFile, Config, Display, Env, Settings};
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, WindowEvent};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_positioner::{on_tray_event, Position, WindowExt};

const MAIN_WINDOW_LABEL: &str = "main";
const TRAY_ID: &str = "usagerank";
const POPOVER_WIDTH: f64 = 388.0;
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");
const TRAY_ICON_SYNCING: &[u8] = include_bytes!("../icons/tray-syncing.png");

/// After a failed push, try again this soon rather than waiting a full interval.
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
/// The local view rescans the logs this often in the background.
const LOCAL_EVERY: Duration = Duration::from_secs(2 * 60);
/// Opening the popover rescans / refetches when the data is older than this.
const STALE_AFTER: Duration = Duration::from_secs(30);
/// Live Claude / Codex limits refresh this often once the Limits tab has been opened, and this is
/// also how long a result counts as fresh. Like OpenUsage, opening the popover or the Limits tab only
/// shows what we have; it refetches only when the heartbeat fell behind (e.g. after sleep). The usage
/// endpoints rate limit eagerly, and other tools (Claude Code, OpenUsage) share the same token's
/// budget, so popover-driven fetches quickly earn a 429.
const LIMITS_EVERY: Duration = Duration::from_secs(5 * 60);

struct LoginFlow {
    id: u64,
    user_code: String,
    url: String,
    expires_at: i64,
}

#[derive(Default)]
struct Inner {
    app: AppFile,
    /// Pushing to UsageRank.
    syncing: bool,
    /// Reading the local logs.
    scanning: bool,
    local: Option<local::LocalStats>,
    local_error: Option<String>,
    persistence_error: Option<String>,
    sharing_disabled: bool,
    login: Option<LoginFlow>,
    login_error: Option<String>,
    login_seq: u64,
    sources: Vec<scan::SourceInfo>,
    last_local_scan: Option<Instant>,
    /// Wall-clock, not `Instant`: on macOS `Instant` stops while the Mac sleeps, so a push would
    /// wait a full awake interval after wake instead of going out as soon as it is overdue.
    last_attempt: Option<SystemTime>,
    last_attempt_failed: bool,
    last_stats_fetch: Option<Instant>,
    /// Live subscription limits. Only fetched once the Limits tab asked for them, so nobody who
    /// never opens it gets keychain reads or calls to Anthropic / OpenAI.
    limits: limits::Limits,
    limits_wanted: bool,
    limits_fetching: bool,
    last_limits_fetch: Option<Instant>,
    /// After a 429, don't ask that provider again before this.
    claude_backoff: Option<Instant>,
    codex_backoff: Option<Instant>,
    update: updater::UpdateState,
}

struct Shared {
    env: Env,
    api: Api,
    inner: Mutex<Inner>,
    sync_lock: tokio::sync::Mutex<()>,
    local_lock: tokio::sync::Mutex<()>,
    limits_lock: tokio::sync::Mutex<()>,
    limits_http: reqwest::Client,
}

type SharedState = Arc<Shared>;

impl Shared {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut inner)
    }

    fn save_app(&self) -> Result<(), String> {
        self.with(|i| {
            let result = i.app.save(&self.env);
            i.persistence_error = result.as_ref().err().cloned();
            result
        })
    }

    fn update_app(&self, change: impl FnOnce(&mut AppFile)) -> Result<(), String> {
        self.with(|i| {
            let mut next = i.app.clone();
            change(&mut next);
            let result = next.save(&self.env);
            i.persistence_error = result.as_ref().err().cloned();
            result?;
            i.app = next;
            Ok(())
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginView {
    user_code: String,
    url: String,
    expires_at: i64,
}

/// Everything the popover renders.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    signed_in: bool,
    has_config: bool,
    handle: Option<String>,
    suggested_handle: String,
    account: Option<config::Account>,
    machine_id: Option<String>,
    api_url: String,
    web_url: String,
    syncing: bool,
    scanning: bool,
    last_sync_at: Option<i64>,
    last_error: Option<String>,
    local: Option<local::LocalStats>,
    stats: Option<Value>,
    stats_error: Option<String>,
    settings: Settings,
    autostart: bool,
    retention_days: Option<u64>,
    login: Option<LoginView>,
    login_error: Option<String>,
    sources: Vec<scan::SourceInfo>,
    limits: limits::Limits,
    limits_fetching: bool,
    ui: serde_json::Map<String, Value>,
    errors: Vec<String>,
    version: &'static str,
    update: updater::UpdateView,
}

fn snapshot(app: &AppHandle) -> Snapshot {
    let shared = app.state::<SharedState>();
    let loaded = Config::load(&shared.env);
    let mut errors: Vec<String> = loaded.as_ref().err().cloned().into_iter().collect();
    let cfg = loaded.ok().flatten();
    let autostart = match login_item::is_enabled(app) {
        Ok(enabled) => enabled,
        Err(e) => {
            errors.push(format!("Cannot read the launch-at-login setting: {e}"));
            false
        }
    };
    let retention_days = config::claude_retention_days(&shared.env);
    shared.with(|i| Snapshot {
        signed_in: !i.sharing_disabled && cfg.as_ref().is_some_and(|c| c.account.is_some()),
        has_config: cfg.is_some(),
        handle: cfg.as_ref().map(|c| c.handle.clone()),
        suggested_handle: cfg.as_ref().map(|c| c.handle.clone()).unwrap_or_else(config::default_handle),
        account: cfg.as_ref().and_then(|c| c.account.clone()),
        machine_id: cfg.as_ref().map(|c| c.machine_id.clone()),
        api_url: shared.env.api_url.clone(),
        web_url: shared.env.web_url.clone(),
        syncing: i.syncing,
        scanning: i.scanning,
        last_sync_at: i.app.last_sync_at,
        last_error: i.app.last_error.clone(),
        local: i.local.clone(),
        stats: i.app.stats.clone(),
        stats_error: i.app.stats_error.clone(),
        settings: i.app.settings.clone(),
        autostart,
        retention_days,
        login: i.login.as_ref().map(|l| LoginView { user_code: l.user_code.clone(), url: l.url.clone(), expires_at: l.expires_at }),
        login_error: i.login_error.clone(),
        sources: i.sources.clone(),
        limits: i.limits.clone(),
        limits_fetching: i.limits_fetching,
        ui: i.app.ui.clone(),
        errors: {
            errors.extend(i.local_error.iter().cloned());
            errors.extend(i.persistence_error.iter().cloned());
            errors
        },
        version: env!("CARGO_PKG_VERSION"),
        update: i.update.view.clone(),
    })
}

/// Push the latest state to the popover and the menu bar item.
fn refresh(app: &AppHandle) {
    let snap = snapshot(app);
    update_tray(app, &snap);
    let _ = app.emit("snapshot", &snap);
}

fn fmt_tokens(n: f64) -> String {
    if n >= 1e9 {
        format!("{:.2}B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.1}K", n / 1e3)
    } else {
        format!("{}", n as u64)
    }
}

fn fmt_cost(n: f64) -> String {
    if n >= 100.0 {
        let digits = format!("{}", n.round() as u64);
        let mut out = String::new();
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        format!("${out}")
    } else {
        format!("${n:.2}")
    }
}

/// Tokens and spend come from this Mac's logs; rank needs a linked account and falls back to tokens.
fn tray_title(snap: &Snapshot) -> Option<String> {
    if snap.syncing {
        return Some("Syncing".into());
    }
    let today = snap.local.as_ref().and_then(|l| l.periods.get("1"));
    let tokens = || today.map(|t| fmt_tokens(t.tokens as f64)).unwrap_or_else(|| "—".into());
    let rank = || {
        let server = snap.stats.as_ref().and_then(|s| s.pointer("/periods/1"))?;
        let rank = server.get("rank").and_then(Value::as_f64)?;
        Some(match server.get("previous_rank").and_then(Value::as_f64) {
            Some(prev) if prev > rank => format!("#{rank} ▲{}", prev - rank),
            Some(prev) if prev < rank => format!("#{rank} ▼{}", rank - prev),
            _ => format!("#{rank}"),
        })
    };
    match snap.settings.display {
        Display::Icon | Display::Limits => None,
        Display::Tokens => Some(tokens()),
        Display::Cost => Some(today.map(|t| fmt_cost(t.cost)).unwrap_or_else(|| "—".into())),
        Display::Rank => Some(if snap.signed_in { rank().unwrap_or_else(|| "#—".into()) } else { tokens() }),
    }
}

/// One glyph per enabled provider with its session and weekly usage, once limits have loaded.
fn limit_segments(snap: &Snapshot) -> Vec<tray_image::Segment> {
    let pick = |p: &Option<limits::Provider>, glyph| {
        let provider = p.as_ref()?;
        let mut primary = provider.windows.iter().filter(|w| w.primary);
        let (a, b) = (primary.next(), primary.next());
        if a.is_none() {
            return None;
        }
        let pct = |w: Option<&limits::Window>| w.map(|w| (100.0 - w.used).round().clamp(0.0, 100.0) as u32);
        Some(tray_image::Segment { glyph, lines: [pct(a), pct(b)] })
    };
    let mut out = Vec::new();
    if snap.settings.claude {
        out.extend(pick(&snap.limits.claude, tray_image::Glyph::Claude));
    }
    if snap.settings.codex {
        out.extend(pick(&snap.limits.codex, tray_image::Glyph::Codex));
    }
    out
}

fn update_tray(app: &AppHandle, snap: &Snapshot) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    let segments = if snap.settings.display == Display::Limits && !snap.syncing { limit_segments(snap) } else { Vec::new() };
    let rendered = tray_image::render(&segments);
    let title = if rendered.is_some() { None } else { tray_title(snap).or_else(|| (snap.settings.display == Display::Limits).then(|| "—".into())) };
    let _ = tray.set_title(title.as_deref());
    if let Some((rgba, w, h)) = rendered {
        let _ = tray.set_icon_with_as_template(Some(Image::new_owned(rgba, w, h)), true);
    } else {
        let icon = if snap.syncing { TRAY_ICON_SYNCING } else { TRAY_ICON };
        if let Ok(image) = Image::from_bytes(icon) {
            let _ = tray.set_icon_with_as_template(Some(image), true);
        }
    }
    let tooltip = match &title {
        Some(t) if snap.settings.display == Display::Cost => format!("UsageRank — {t} spent today"),
        Some(t) if snap.settings.display == Display::Rank && snap.signed_in => format!("UsageRank — rank {t} today"),
        Some(t) => format!("UsageRank — {t} tokens today"),
        None if snap.settings.display == Display::Limits => "UsageRank — Claude / Codex remaining: session, weekly".to_string(),
        None => "UsageRank".to_string(),
    };
    let _ = tray.set_tooltip(Some(tooltip));
}

fn signed_in_config(env: &Env) -> Result<Option<Config>, String> {
    Ok(Config::load(env)?.filter(|c| c.account.is_some()))
}

/// The server no longer knows this machine's key (removed on the dashboard, or forgotten elsewhere).
fn is_unlinked(e: &ApiError) -> bool {
    e.status == Some(401) && e.message.contains("not linked")
}

fn mark_unlinked(shared: &Shared) {
    let saved = Config::load(&shared.env).and_then(|cfg| {
        if let Some(mut cfg) = cfg {
            cfg.account = None;
            cfg.save(&shared.env)?;
        }
        Ok(())
    });
    shared.with(|i| {
        i.sharing_disabled = true;
        i.app.stats = None;
        i.app.stats_error = None;
        let mut message = "This Mac is no longer linked to your account. Link it again to keep sharing.".to_string();
        if let Err(e) = saved { message.push_str(&format!(" Cannot save this change: {e}")); }
        i.app.last_error = Some(message);
    });
    let _ = shared.save_app();
}

/// Rescan the logs into this app's own cache and rebuild the local view. Never touches the network
/// or `~/.usagerank`.
async fn scan_local(app: AppHandle) {
    let shared = app.state::<SharedState>().inner().clone();
    let Ok(_guard) = shared.local_lock.try_lock() else { return };
    shared.with(|i| i.scanning = true);

    let env = shared.env.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let path = env.local_cache_path();
        let cache = scan::State::load(&path)?;
        let scanned = scan::scan(&env, None, &cache, false, true)?;
        let stats = local::build(&scanned.files);
        let save_error = scan::State { files: scanned.files, extra: cache.extra }.save(&path).err();
        Ok::<_, String>((stats, scanned.sources, save_error))
    })
    .await
    .map_err(|e| format!("The local scan failed: {e}"))
    .and_then(|result| result);

    shared.with(|i| {
        i.scanning = false;
        match result {
            Ok((stats, sources, save_error)) => {
                i.local = Some(stats);
                i.sources = sources;
                i.local_error = save_error;
                if i.local_error.is_none() {
                    i.last_local_scan = Some(Instant::now());
                }
            }
            Err(message) => i.local_error = Some(message),
        }
    });
    refresh(&app);
}

/// Store a stats result. A failed stats read never fails the push: the server may predate the endpoint.
fn store_stats(shared: &Shared, result: Result<Value, ApiError>) {
    shared.with(|i| {
        i.last_stats_fetch = Some(Instant::now());
        match result {
            Ok(stats) => {
                i.app.stats = Some(stats);
                i.app.stats_error = None;
            }
            Err(e) if e.status == Some(404) => {
                i.app.stats_error = Some("The leaderboard server doesn't serve stats to the app yet.".into());
            }
            Err(e) => i.app.stats_error = Some(e.message),
        }
    });
}

/// Rescan locally, then — only when this Mac is linked — push what changed (`usagerank push`) and
/// refetch leaderboard stats.
async fn sync(app: AppHandle, all: bool) {
    scan_local(app.clone()).await;

    let shared = app.state::<SharedState>().inner().clone();
    let Ok(_guard) = shared.sync_lock.try_lock() else { return };
    if shared.with(|i| i.sharing_disabled) { return; }
    let cfg = match signed_in_config(&shared.env) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return,
        Err(message) => {
            shared.with(|i| i.app.last_error = Some(message));
            refresh(&app);
            return;
        }
    };

    let settings = shared.with(|i| {
        i.syncing = true;
        i.last_attempt = Some(SystemTime::now());
        i.app.settings.clone()
    });
    refresh(&app);

    let only = settings.only().map(String::from);
    let push: Result<(), ApiError> = async {
        if !(settings.claude || settings.codex) {
            return Ok(());
        }
        let env = shared.env.clone();
        let only_scan = only.clone();
        let (scanned, state, first_push) = tauri::async_runtime::spawn_blocking(move || {
            let state = scan::State::load(&env.state_path())?;
            let first_push = state.files.is_empty();
            let scanned = scan::scan(&env, only_scan.as_deref(), &state, all || first_push, false)?;
            Ok::<_, String>((scanned, state, first_push))
        })
        .await
        .map_err(|e| ApiError { status: None, message: format!("scan failed: {e}") })?
        .map_err(|message| ApiError { status: None, message })?;

        let days = scan::daily_rows(&scanned.files, &scanned.touched, only.as_deref());
        if !days.is_empty() {
            shared.api.push(&cfg, &days, first_push || all).await?;
        }
        // Only remember files after the server accepted them, so a failed push retries everything.
        scan::State { files: scanned.files, extra: state.extra }
            .save(&shared.env.state_path())
            .map_err(|message| ApiError { status: None, message })
    }
    .await;

    match push {
        Ok(()) => {
            shared.with(|i| {
                i.app.last_sync_at = Some(chrono::Utc::now().timestamp_millis());
                i.app.last_error = None;
                i.last_attempt_failed = false;
            });
            let stats = shared.api.stats(&cfg).await;
            match stats {
                Err(e) if is_unlinked(&e) => mark_unlinked(&shared),
                other => store_stats(&shared, other),
            }
        }
        Err(e) if is_unlinked(&e) => mark_unlinked(&shared),
        Err(e) => shared.with(|i| {
            i.app.last_error = Some(e.message);
            i.last_attempt_failed = true;
        }),
    }
    shared.with(|i| i.syncing = false);
    let _ = shared.save_app();
    refresh(&app);
}

/// Refetch leaderboard stats only (no scan or push), e.g. when the popover opens.
async fn fetch_stats(app: AppHandle) {
    let shared = app.state::<SharedState>().inner().clone();
    if shared.with(|i| i.sharing_disabled) { return; }
    let cfg = match signed_in_config(&shared.env) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return,
        Err(message) => {
            shared.with(|i| i.app.last_error = Some(message));
            refresh(&app);
            return;
        }
    };
    match shared.api.stats(&cfg).await {
        Err(e) if is_unlinked(&e) => mark_unlinked(&shared),
        other => store_stats(&shared, other),
    }
    let _ = shared.save_app();
    refresh(&app);
}

/// Fold one provider's fetch result into the previous state: a failure keeps the last good windows
/// (shown as stale) and a 429 backs off for as long as the server asked.
fn merge_limits(
    prev: Option<limits::Provider>,
    result: Result<limits::Fetched, limits::FetchError>,
    backoff: &mut Option<Instant>,
) -> limits::Provider {
    match result {
        Ok(f) => {
            *backoff = None;
            limits::Provider {
                plan: f.plan,
                windows: f.windows,
                error: None,
                missing: false,
                fetched_at: Some(chrono::Utc::now().timestamp_millis()),
            }
        }
        Err(e) => {
            if let Some(secs) = e.retry_after {
                *backoff = Some(Instant::now() + Duration::from_secs(secs.clamp(60, 3600)));
            }
            let mut p = if e.missing { limits::Provider::default() } else { prev.unwrap_or_default() };
            p.error = Some(e.message);
            p.missing = e.missing;
            p
        }
    }
}

/// Refetch the live Claude and Codex limits, in parallel, skipping a provider that's backing off.
async fn fetch_limits(app: AppHandle, allow_interaction: bool) {
    let shared = app.state::<SharedState>().inner().clone();
    let Ok(_guard) = shared.limits_lock.try_lock() else { return };
    let (claude_ok, codex_ok) = shared.with(|i| {
        i.limits_wanted = true;
        i.limits_fetching = true;
        let now = Instant::now();
        (i.claude_backoff.is_none_or(|t| now >= t), i.codex_backoff.is_none_or(|t| now >= t))
    });
    refresh(&app);

    let env = &shared.env;
    let http = &shared.limits_http;
    let (claude, codex) = tokio::join!(
        async { if claude_ok { Some(limits::fetch_claude(env, http, allow_interaction).await) } else { None } },
        async { if codex_ok { Some(limits::fetch_codex(env, http).await) } else { None } },
    );

    let save = shared.with(|i| {
        if let Some(r) = claude {
            i.limits.claude = Some(merge_limits(i.limits.claude.take(), r, &mut i.claude_backoff));
        }
        if let Some(r) = codex {
            i.limits.codex = Some(merge_limits(i.limits.codex.take(), r, &mut i.codex_backoff));
        }
        i.limits_fetching = false;
        i.last_limits_fetch = Some(Instant::now());
        // Persist only clean results; a failed check keeps the last good ones on disk.
        let mut save = false;
        for (live, kept) in [(&i.limits.claude, &mut i.app.limits.claude), (&i.limits.codex, &mut i.app.limits.codex)] {
            match live {
                Some(p) if p.missing => save |= kept.take().is_some(),
                Some(p) if p.error.is_none() => {
                    *kept = Some(p.clone());
                    save = true;
                }
                _ => {}
            }
        }
        save
    });
    if save {
        let _ = shared.save_app();
    }
    refresh(&app);
}

/// Wakes every 30 s: pushes when linked and the interval has passed, otherwise keeps the local view fresh.
fn start_scheduler(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let shared = app.state::<SharedState>().inner().clone();
            let linked = !shared.with(|i| i.sharing_disabled) && matches!(signed_in_config(&shared.env), Ok(Some(_)));
            let limits_due = shared.with(|i| {
                i.limits_wanted && !i.limits_fetching && i.last_limits_fetch.is_none_or(|t| t.elapsed() >= LIMITS_EVERY)
            });
            if limits_due {
                tauri::async_runtime::spawn(fetch_limits(app.clone(), false));
            }
            let (push_due, local_due) = shared.with(|i| {
                let every = if i.last_attempt_failed {
                    RETRY_AFTER.min(Duration::from_secs(i.app.settings.interval_minutes as u64 * 60))
                } else {
                    Duration::from_secs(i.app.settings.interval_minutes.max(5) as u64 * 60)
                };
                (
                    !i.syncing && i.login.is_none() && i.last_attempt.is_none_or(|t| t.elapsed().map_or(true, |e| e >= every)),
                    i.last_local_scan.is_none_or(|t| t.elapsed() >= LOCAL_EVERY),
                )
            });
            if linked && push_due {
                sync(app.clone(), false).await;
            } else if local_due {
                scan_local(app.clone()).await;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

fn show_popover(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else { return };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }
    let _ = window.move_window_constrained(Position::TrayCenter);
    let _ = window.show();
    let _ = window.set_focus();
    let _ = app.emit("popover-shown", ());

    let shared = app.state::<SharedState>();
    let (local_stale, stats_stale, limits_stale) = shared.with(|i| {
        (
            i.last_local_scan.is_none_or(|t| t.elapsed() > STALE_AFTER),
            !i.syncing && i.last_stats_fetch.is_none_or(|t| t.elapsed() > STALE_AFTER * 2),
            i.limits_wanted && i.last_limits_fetch.is_none_or(|t| t.elapsed() >= LIMITS_EVERY),
        )
    });
    if limits_stale {
        tauri::async_runtime::spawn(fetch_limits(app.clone(), false));
    }
    if local_stale {
        tauri::async_runtime::spawn(scan_local(app.clone()));
    }
    if stats_stale {
        tauri::async_runtime::spawn(fetch_stats(app.clone()));
    }
}

fn open_external(app: &AppHandle, url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("Only web links can be opened.".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

fn leaderboard_url(app: &AppHandle) -> String {
    let shared = app.state::<SharedState>();
    shared
        .with(|i| i.app.stats.as_ref().and_then(|s| s.get("leaderboard_url")).and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| shared.env.web_url.clone())
}

#[tauri::command]
fn get_snapshot(app: AppHandle) -> Snapshot {
    snapshot(&app)
}

/// Rescan the logs; when linked, also push and refetch stats. `all` re-sends every day.
#[tauri::command]
fn sync_now(app: AppHandle, all: Option<bool>) {
    tauri::async_runtime::spawn(sync(app, all.unwrap_or(false)));
}

/// Fetch live limits if the last result is older than `LIMITS_EVERY`. `force` (⌘R on the Limits tab)
/// ignores that but still respects a provider's rate-limit backoff.
#[tauri::command]
fn refresh_limits(app: AppHandle, force: Option<bool>) {
    let shared = app.state::<SharedState>();
    let due = force.unwrap_or(false) || shared.with(|i| i.last_limits_fetch.is_none_or(|t| t.elapsed() >= LIMITS_EVERY));
    if due {
        tauri::async_runtime::spawn(fetch_limits(app.clone(), force.unwrap_or(false)));
    }
}

/// Remember a popover choice (tab, period, …) in app.json.
#[tauri::command]
fn set_ui_pref(app: AppHandle, key: String, value: String) -> Result<(), String> {
    let shared = app.state::<SharedState>();
    let changed = shared.with(|i| i.app.ui.get(&key) != Some(&Value::String(value.clone())));
    if changed {
        shared.update_app(|next| { next.ui.insert(key, Value::String(value)); })?;
    }
    Ok(())
}

#[tauri::command]
fn set_settings(app: AppHandle, settings: Settings) -> Result<Snapshot, String> {
    let shared = app.state::<SharedState>();
    let wants_limits = settings.display == Display::Limits;
    let tools_changed = shared.with(|i| i.app.settings.claude != settings.claude || i.app.settings.codex != settings.codex);
    shared.update_app(|next| {
        next.settings = Settings { interval_minutes: settings.interval_minutes.clamp(15, 24 * 60), ..settings };
    })?;
    if tools_changed {
        tauri::async_runtime::spawn(sync(app.clone(), false));
    }
    if wants_limits {
        tauri::async_runtime::spawn(fetch_limits(app.clone(), false));
    }
    refresh(&app);
    Ok(snapshot(&app))
}

/// Preferences → Check for updates.
#[tauri::command]
fn check_update(app: AppHandle) {
    tauri::async_runtime::spawn(updater::check(app, true));
}

/// Download and install the update found by the last check, then relaunch.
#[tauri::command]
fn install_update(app: AppHandle) {
    tauri::async_runtime::spawn(updater::install(app));
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<Snapshot, String> {
    login_item::set_enabled(&app, enabled)?;
    Ok(snapshot(&app))
}

#[tauri::command]
fn extend_retention(app: AppHandle) -> Result<Snapshot, String> {
    config::extend_claude_retention(&app.state::<SharedState>().env)?;
    Ok(snapshot(&app))
}

/// Opt in to sharing: create the key if needed, start the device flow, open the browser and poll.
#[tauri::command]
async fn start_login(app: AppHandle, handle: String, keep_logs: bool) -> Result<Snapshot, String> {
    let shared = app.state::<SharedState>().inner().clone();
    let handle = config::normalize_handle(&handle)?;
    let cfg = match Config::load(&shared.env)? {
        Some(mut cfg) => {
            if cfg.handle != handle {
                cfg.handle = handle;
                cfg.save(&shared.env)?;
            }
            cfg
        }
        None => Config::create(&shared.env, &handle)?,
    };
    if keep_logs && config::claude_retention_days(&shared.env).is_some_and(|d| d < 365) {
        config::extend_claude_retention(&shared.env)?;
    }

    shared.with(|i| i.login_error = None);
    let start = shared.api.start_login(&cfg).await.map_err(|e| e.message)?;
    let field = |k: &str| -> Result<String, String> {
        start.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
            .map(String::from).ok_or_else(|| format!("The login response is missing {k}. Try again."))
    };
    let device_code = field("device_code")?;
    let user_code = field("user_code")?;
    let url = field("verification_uri_complete")?;
    let parsed_url = reqwest::Url::parse(&url).map_err(|_| "The login response has an invalid browser URL.".to_string())?;
    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err("The login response has an invalid browser URL.".into());
    }
    let expires_in = start.get("expires_in").and_then(Value::as_i64).unwrap_or(600).clamp(1, 3600);
    let mut interval = start.get("interval").and_then(Value::as_u64).unwrap_or(5).clamp(1, 60);

    let id = shared.with(|i| {
        i.login_seq += 1;
        i.login = Some(LoginFlow {
            id: i.login_seq,
            user_code,
            url: url.clone(),
            expires_at: chrono::Utc::now().timestamp_millis() + expires_in * 1000,
        });
        i.login_seq
    });
    if let Err(message) = open_external(&app, &url) {
        shared.with(|i| i.login_error = Some(format!("Cannot open the login page: {message}. Use Open browser again.")));
    }
    refresh(&app);

    let poller = app.clone();
    tauri::async_runtime::spawn(async move {
        let shared = poller.state::<SharedState>().inner().clone();
        let deadline = Instant::now() + Duration::from_secs(expires_in.max(0) as u64);
        let outcome: Result<Value, String> = loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            if shared.with(|i| i.login.as_ref().map(|l| l.id)) != Some(id) {
                return; // cancelled or replaced
            }
            if Instant::now() >= deadline {
                break Err("The login request expired. Try again.".into());
            }
            match shared.api.poll_login(&cfg, &device_code).await {
                Ok(Some(user)) => break Ok(user),
                Ok(None) => {}
                Err(e) if e.status == Some(429) => interval = interval.saturating_add(5).min(60),
                Err(e) if e.status.is_some() => break Err(e.message),
                Err(_) => {} // network blip: keep polling until the code expires
            }
        };

        if shared.with(|i| i.login.as_ref().map(|l| l.id)) != Some(id) {
            return;
        }
        match outcome {
            Ok(user) => {
                let mut cfg = match Config::load(&shared.env) {
                    Ok(Some(latest)) => latest,
                    Ok(None) => cfg,
                    Err(message) => {
                        shared.with(|i| { i.login = None; i.login_error = Some(message); });
                        refresh(&poller);
                        return;
                    }
                };
                cfg.account = Some(config::Account {
                    name: user.get("name").and_then(Value::as_str).unwrap_or_default().into(),
                    email: user.get("email").and_then(Value::as_str).unwrap_or_default().into(),
                    logged_in_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
                    extra: Default::default(),
                });
                let saved = cfg.save(&shared.env);
                shared.with(|i| {
                    i.login = None;
                    i.sharing_disabled = saved.is_err();
                    i.login_error = saved.err();
                    i.app.last_error = None;
                });
                refresh(&poller);
                sync(poller.clone(), false).await;
            }
            Err(message) => {
                shared.with(|i| {
                    i.login = None;
                    i.login_error = Some(message);
                });
                refresh(&poller);
            }
        }
    });

    Ok(snapshot(&app))
}

#[tauri::command]
fn cancel_login(app: AppHandle) -> Snapshot {
    app.state::<SharedState>().with(|i| {
        i.login = None;
        i.login_error = None;
    });
    refresh(&app);
    snapshot(&app)
}

/// Change the public handle. The server claims it on the next push, so push right away (an empty
/// push is enough) and report back whether it stuck.
#[tauri::command]
async fn set_handle(app: AppHandle, handle: String) -> Result<Snapshot, String> {
    let shared = app.state::<SharedState>().inner().clone();
    let _guard = shared.sync_lock.lock().await;
    let handle = config::normalize_handle(&handle)?;
    let mut cfg = Config::load(&shared.env)?.ok_or("This Mac is not set up yet.")?;
    cfg.handle = handle.clone();
    if cfg.account.is_some() {
        shared.api.push(&cfg, &[], false).await.map_err(|e| e.message)?;
        cfg.save(&shared.env)?;
        match shared.api.stats(&cfg).await {
            Ok(stats) => {
                let actual = stats.pointer("/user/handle").and_then(Value::as_str).map(String::from);
                let claimed = actual.as_deref() == Some(handle.as_str());
                store_stats(&shared, Ok(stats));
                if !claimed {
                    if let Some(actual) = actual {
                        cfg.handle = actual;
                        cfg.save(&shared.env)?;
                    }
                    let _ = shared.save_app();
                    refresh(&app);
                    return Err(format!("@{handle} was not accepted. Your profile keeps its current handle."));
                }
            }
            Err(e) => {
                let message = format!("The handle request reached the server, but its result could not be checked: {}", e.message);
                if is_unlinked(&e) { mark_unlinked(&shared); }
                else { store_stats(&shared, Err(e)); }
                let _ = shared.save_app();
                refresh(&app);
                return Err(message);
            }
        }
    } else {
        cfg.save(&shared.env)?;
    }
    shared.save_app()?;
    refresh(&app);
    Ok(snapshot(&app))
}

/// Stop sharing. The key stays bound on the server until the device is removed on the dashboard.
#[tauri::command]
async fn sign_out(app: AppHandle) -> Result<Snapshot, String> {
    let shared = app.state::<SharedState>().inner().clone();
    let _guard = shared.sync_lock.lock().await;
    if let Some(mut cfg) = Config::load(&shared.env)? {
        cfg.account = None;
        cfg.save(&shared.env)?;
    }
    shared.with(|i| {
        i.sharing_disabled = true;
        i.login = None;
        i.app.stats = None;
        i.app.stats_error = None;
        i.app.last_error = None;
        i.app.last_sync_at = None;
    });
    let saved = shared.save_app();
    refresh(&app);
    saved?;
    Ok(snapshot(&app))
}

/// `usagerank forget`: delete the row and history on the server, the CLI's launchd job and
/// `~/.usagerank`. The local view (this app's own cache) stays.
#[tauri::command]
async fn forget(app: AppHandle) -> Result<Snapshot, String> {
    let shared = app.state::<SharedState>().inner().clone();
    // Wait for an active upload before deleting its account data.
    let _guard = shared.sync_lock.lock().await;
    let mut cfg = Config::load(&shared.env)?.ok_or("This Mac has never pushed.")?;
    shared.api.forget(&cfg).await.map_err(|e| e.message)?;
    let mut errors = Vec::new();
    cfg.account = None;
    if let Err(e) = cfg.save(&shared.env) { errors.push(e); }
    if let Err(e) = remove_cli_schedule() { errors.push(e); }
    if let Err(e) = config::wipe_local(&shared.env) { errors.push(e); }
    shared.with(|i| {
        i.sharing_disabled = true;
        i.app.stats = None;
        i.app.stats_error = None;
        i.app.last_error = None;
        i.app.last_sync_at = None;
        i.last_attempt = None;
        i.last_attempt_failed = false;
        i.login = None;
    });
    if let Err(e) = shared.save_app() { errors.push(e); }
    refresh(&app);
    if !errors.is_empty() {
        return Err(format!("The server data was deleted, but local cleanup failed: {}", errors.join(" ")));
    }
    Ok(snapshot(&app))
}

/// Remove the CLI schedule so it cannot upload after data deletion.
fn remove_cli_schedule() -> Result<(), String> {
    let home = dirs::home_dir().ok_or("Cannot find the home folder for the CLI schedule.")?;
    remove_cli_schedule_at(&home.join("Library/LaunchAgents/com.usagerank.push.plist"))
}

fn remove_cli_schedule_at(plist: &std::path::Path) -> Result<(), String> {
    match std::fs::metadata(plist) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("Cannot read {}: {e}", plist.display())),
        Ok(_) => {},
    }
    let unload = std::process::Command::new("launchctl").args(["unload", "-w"]).arg(plist).output()
        .map_err(|e| format!("Cannot stop the CLI schedule: {e}"))
        .and_then(|out| {
            if out.status.success() { Ok(()) }
            else { Err(format!("Cannot stop the CLI schedule: {}", String::from_utf8_lossy(&out.stderr).trim())) }
        });
    let remove = std::fs::remove_file(plist).map_err(|e| format!("Cannot remove {}: {e}", plist.display()));
    match (unload, remove) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(a), Err(b)) => Err(format!("{a} {b}")),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}

#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    open_external(&app, &url)
}

#[tauri::command]
fn open_leaderboard(app: AppHandle) -> Result<(), String> {
    let url = leaderboard_url(&app);
    open_external(&app, &url)
}

#[tauri::command]
fn set_popover_height(app: AppHandle, height: f64) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        // macOS resizes around the bottom-left corner, so a shrink would push the top edge
        // down away from the menu bar. Pin the top edge back where it was.
        let top_left = window.outer_position();
        let _ = window.set_size(LogicalSize::new(POPOVER_WIDTH, height.clamp(200.0, 1000.0)));
        if let Ok(position) = top_left {
            let _ = window.set_position(position);
        }
    }
}

#[tauri::command]
fn hide_popover(app: AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.hide();
    }
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

fn build_tray(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let show_item = MenuItem::with_id(app, "show", "Show UsageRank", true, None::<&str>)?;
    let sync_item = MenuItem::with_id(app, "sync", "Refresh Now", true, None::<&str>)?;
    let open_item = MenuItem::with_id(app, "leaderboard", "Open Leaderboard", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit UsageRank", true, Some("CmdOrCtrl+Q"))?;
    let menu = Menu::with_items(
        app,
        &[&show_item, &sync_item, &open_item, &PredefinedMenuItem::separator(app)?, &quit_item],
    )?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(TRAY_ICON)?)
        .icon_as_template(true)
        .title("—")
        .tooltip("UsageRank")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "sync" => sync_now(app.clone(), None),
            "leaderboard" => {
                let _ = open_leaderboard(app.clone());
            }
            "show" => show_popover(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            on_tray_event(tray.app_handle(), &event);
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_popover(tray.app_handle());
            }
        })
        .build(app)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let env = Env::load();
    let (app_file, persistence_error) = match AppFile::load(&env) {
        Ok(app_file) => (app_file, None),
        Err(message) => (AppFile::default(), Some(message)),
    };
    // Limits seen before: show them right away and keep them fresh without waiting for the Limits tab.
    let limits = app_file.limits.clone();
    let limits_wanted = limits.claude.is_some() || limits.codex.is_some();
    let shared: SharedState = Arc::new(Shared {
        api: Api::new(env.clone()),
        inner: Mutex::new(Inner { app: app_file, limits, limits_wanted, sharing_disabled: persistence_error.is_some(), persistence_error, ..Default::default() }),
        env,
        sync_lock: tokio::sync::Mutex::new(()),
        local_lock: tokio::sync::Mutex::new(()),
        limits_lock: tokio::sync::Mutex::new(()),
        limits_http: limits::client(),
    });

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_popover(app)))
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build());
    #[cfg(not(target_os = "macos"))]
    let builder = builder.plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None));
    builder
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            sync_now,
            refresh_limits,
            set_ui_pref,
            set_settings,
            set_autostart,
            extend_retention,
            start_login,
            cancel_login,
            set_handle,
            sign_out,
            forget,
            open_url,
            open_leaderboard,
            check_update,
            install_update,
            set_popover_height,
            hide_popover,
            quit,
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            build_tray(app.handle())?;
            if let Err(e) = login_item::migrate(app.handle()) {
                eprintln!("UsageRank: {e}");
            }

            if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                let popover = window.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::Focused(false) = event {
                        let _ = popover.hide();
                    }
                });
            }

            refresh(app.handle());
            if app.state::<SharedState>().with(|i| i.app.settings.display == Display::Limits) {
                tauri::async_runtime::spawn(fetch_limits(app.handle().clone(), false));
            }
            start_scheduler(app.handle().clone());
            updater::start(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running UsageRank");
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn failed_settings_save_keeps_previous_settings_and_reports_error() {
        let root = std::env::temp_dir().join(format!("ur-errors-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let env = config::test_env(&root);
        std::fs::write(&env.app_dir, "blocked directory").unwrap();
        let shared = Shared {
            api: Api::new(env.clone()), env, inner: Mutex::new(Inner::default()),
            sync_lock: tokio::sync::Mutex::new(()), local_lock: tokio::sync::Mutex::new(()),
            limits_lock: tokio::sync::Mutex::new(()), limits_http: limits::client(),
        };
        assert!(shared.update_app(|next| next.settings.claude = false).is_err());
        shared.with(|i| {
            assert!(i.app.settings.claude);
            assert!(i.persistence_error.is_some());
        });
        std::fs::remove_file(&shared.env.app_dir).unwrap();
        assert!(shared.update_app(|next| next.settings.claude = false).is_ok());
        shared.with(|i| {
            assert!(!i.app.settings.claude);
            assert!(i.persistence_error.is_none());
        });
        assert!(!AppFile::load(&shared.env).unwrap().settings.claude);
        std::fs::remove_dir_all(root).unwrap();
    }
}
