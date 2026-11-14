//! Per-user start at sign-in (TRAY-05): `HKCU\...\Run`, no administrator rights.

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const NAME: &str = "WaiShe";

pub fn apply(enabled: bool) {
    let run = match RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN, KEY_SET_VALUE | KEY_QUERY_VALUE)
    {
        Ok(k) => k,
        Err(e) => return tracing::warn!("autostart: cannot open Run key: {e}"),
    };
    let result = if enabled {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return tracing::warn!("autostart: {e}"),
        };
        run.set_value(NAME, &format!("\"{}\" --background", exe.display()))
    } else {
        match run.delete_value(NAME) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    };
    match result {
        Ok(()) => tracing::info!("autostart {}", if enabled { "enabled" } else { "disabled" }),
        Err(e) => tracing::warn!("autostart: {e}"),
    }
}
