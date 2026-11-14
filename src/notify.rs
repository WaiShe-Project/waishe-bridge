//! System notifications.

use tauri_winrt_notification::Toast;

/// AppUserModelID registered by the installer's Start menu shortcut.
const APP_ID: &str = "WaiShe.Bridge";

/// Windows only shows toasts for an AppUserModelID that has a Start menu shortcut. Until the
/// installer has created ours (portable runs, development), PowerShell's id is used instead.
fn app_id() -> &'static str {
    let installed = std::env::var_os("APPDATA")
        .map(|a| {
            std::path::Path::new(&a)
                .join(r"Microsoft\Windows\Start Menu\Programs\WaiShe.lnk")
                .exists()
        })
        .unwrap_or(false);
    if installed {
        APP_ID
    } else {
        Toast::POWERSHELL_APP_ID
    }
}

pub fn toast(title: &str, body: &str) {
    let r = Toast::new(app_id()).title(title).text1(body).show();
    if let Err(e) = r {
        tracing::warn!("notification failed: {e}");
    }
}

pub fn low_battery(name: &str, percent: u8, english: bool) {
    if english {
        toast("Low battery", &format!("{name}: {percent}% left"));
    } else {
        toast("电量低", &format!("{name} 剩余 {percent}%"));
    }
}
