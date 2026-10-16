//! Launch at login. On macOS this registers the app itself with `SMAppService.mainApp`, so it shows
//! up under its own name and icon in System Settings > General > Login Items > Open at Login.
//! Elsewhere it falls back to the autostart plugin.

use tauri::AppHandle;

#[cfg(target_os = "macos")]
mod imp {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    use tauri::AppHandle;

    /// The LaunchAgent the autostart plugin used to write. macOS lists it under the signing
    /// developer's name in App Background Activity, which is why we moved to SMAppService.
    fn legacy_plist() -> Option<std::path::PathBuf> {
        Some(dirs::home_dir()?.join("Library/LaunchAgents/UsageRank.plist"))
    }

    pub fn is_enabled(_app: &AppHandle) -> Result<bool, String> {
        let status = unsafe { SMAppService::mainAppService().status() };
        if status == SMAppServiceStatus::RequiresApproval {
            return Err("Launch at login needs approval: turn on UsageRank in System Settings > General > Login Items.".into());
        }
        Ok(status == SMAppServiceStatus::Enabled)
    }

    pub fn set_enabled(_app: &AppHandle, enabled: bool) -> Result<(), String> {
        let service = unsafe { SMAppService::mainAppService() };
        let result = unsafe {
            match (enabled, service.status()) {
                (true, SMAppServiceStatus::Enabled) | (false, SMAppServiceStatus::NotRegistered) => Ok(()),
                (true, _) => service.registerAndReturnError(),
                (false, _) => service.unregisterAndReturnError(),
            }
        };
        result.map_err(|e| e.localizedDescription().to_string())
    }

    /// Replace the old LaunchAgent with an SMAppService registration, keeping the user's choice.
    pub fn migrate(app: &AppHandle) -> Result<(), String> {
        let Some(path) = legacy_plist().filter(|p| p.exists()) else { return Ok(()) };
        std::fs::remove_file(&path).map_err(|e| format!("Cannot remove the old login item {}: {e}", path.display()))?;
        set_enabled(app, true)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use tauri::AppHandle;
    use tauri_plugin_autostart::ManagerExt;

    pub fn is_enabled(app: &AppHandle) -> Result<bool, String> {
        app.autolaunch().is_enabled().map_err(|e| e.to_string())
    }

    pub fn set_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
        let launcher = app.autolaunch();
        if enabled { launcher.enable() } else { launcher.disable() }.map_err(|e| e.to_string())
    }

    pub fn migrate(_app: &AppHandle) -> Result<(), String> {
        Ok(())
    }
}

pub fn is_enabled(app: &AppHandle) -> Result<bool, String> {
    imp::is_enabled(app)
}

pub fn set_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    imp::set_enabled(app, enabled)
}

pub fn migrate(app: &AppHandle) -> Result<(), String> {
    imp::migrate(app)
}
