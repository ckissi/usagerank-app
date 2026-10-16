//! In-app updates. Checks `latest.json` on GitHub Releases (endpoint and public key in
//! tauri.conf.json) in the background, and downloads, verifies and installs the signed update
//! only when the user clicks Update in the popover. `scripts/release-mac.sh --publish` makes releases.

use crate::{refresh, SharedState};
use serde::Serialize;
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

/// The first check waits this long after launch, so it doesn't compete with the initial scan.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(30);
/// Then check this often. Wall clock, so time the Mac spent asleep counts.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How often the background loop wakes up to see whether a check is due (or a failed one should be retried).
const WAKE_EVERY: Duration = Duration::from_secs(15 * 60);

/// What the popover shows about updates.
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateView {
    /// A newer version is available.
    pub version: Option<String>,
    pub notes: Option<String>,
    pub checking: bool,
    /// Downloading or installing; the app restarts when done.
    pub installing: bool,
    /// Download progress from 0 to 1, when the server sent a size.
    pub progress: Option<f64>,
    pub error: Option<String>,
    /// The last check the user asked for found nothing newer.
    pub up_to_date: bool,
}

#[derive(Default)]
pub struct UpdateState {
    pub view: UpdateView,
    pending: Option<Update>,
    /// Last successful check. Failed checks are retried after `WAKE_EVERY`.
    last_check: Option<SystemTime>,
}

/// Checks soon after launch, then every `CHECK_EVERY`. Off in dev builds, where Preferences →
/// Check for updates still works.
pub fn start(app: AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_AFTER).await;
        loop {
            let due = app.state::<SharedState>().with(|i| {
                i.update.last_check.is_none_or(|t| t.elapsed().map_or(true, |elapsed| elapsed >= CHECK_EVERY))
            });
            if due {
                check(app.clone(), false).await;
            }
            tokio::time::sleep(WAKE_EVERY).await;
        }
    });
}

/// `manual` (the Preferences button) also reports "up to date" and errors; background checks fail quietly.
pub async fn check(app: AppHandle, manual: bool) {
    let shared = app.state::<SharedState>().inner().clone();
    let busy = shared.with(|i| {
        let u = &mut i.update.view;
        if u.checking || u.installing {
            return true;
        }
        u.checking = true;
        u.up_to_date = false;
        if manual {
            u.error = None;
        }
        false
    });
    if busy {
        return;
    }
    if manual {
        refresh(&app);
    }

    let result = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(e) => Err(e),
    };
    shared.with(|i| {
        let u = &mut i.update;
        u.view.checking = false;
        match result {
            Ok(found) => {
                u.last_check = Some(SystemTime::now());
                u.view.error = None;
                u.view.up_to_date = manual && found.is_none();
                u.view.version = found.as_ref().map(|f| f.version.clone());
                u.view.notes = found.as_ref().and_then(|f| f.body.clone()).filter(|n| !n.trim().is_empty());
                u.pending = found;
            }
            // Offline or a GitHub hiccup: keep whatever was found before.
            Err(e) => {
                eprintln!("UsageRank: update check failed: {e}");
                if manual {
                    u.view.error = Some(format!("Couldn't check for updates: {e}"));
                }
            }
        }
    });
    refresh(&app);
}

/// Downloads the pending update, verifies its signature, replaces the app and relaunches.
pub async fn install(app: AppHandle) {
    let shared = app.state::<SharedState>().inner().clone();
    let update = shared.with(|i| {
        let u = &mut i.update;
        if u.view.installing || u.view.checking {
            return None;
        }
        let update = u.pending.clone()?;
        u.view.installing = true;
        u.view.progress = None;
        u.view.error = None;
        Some(update)
    });
    let Some(update) = update else { return };
    refresh(&app);

    let progress_app = app.clone();
    let mut downloaded: u64 = 0;
    let mut shown: Option<u64> = None;
    let result = update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as u64;
                let Some(total) = total.filter(|t| *t > 0) else { return };
                // Repaint once per percent, not per chunk.
                let percent = (downloaded * 100 / total).min(100);
                if shown != Some(percent) {
                    shown = Some(percent);
                    progress_app.state::<SharedState>().with(|i| i.update.view.progress = Some(percent as f64 / 100.0));
                    refresh(&progress_app);
                }
            },
            || {},
        )
        .await;

    match result {
        Ok(()) => app.restart(),
        Err(e) => {
            eprintln!("UsageRank: update failed: {e}");
            shared.with(|i| {
                let u = &mut i.update.view;
                u.installing = false;
                u.progress = None;
                u.error = Some(format!("Update failed: {e}"));
            });
            refresh(&app);
        }
    }
}
