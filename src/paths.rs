//! Where Bridge keeps its files. `WAISHE_BRIDGE_HOME` puts everything under one directory
//! (development and tests).

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    pub settings: PathBuf,
    pub devices: PathBuf,
    pub drivers: PathBuf,
    pub logs: PathBuf,
    /// WebView2 user data of the settings panel.
    pub webview: PathBuf,
}

impl Paths {
    pub fn resolve() -> Self {
        let (config, data) = match std::env::var_os("WAISHE_BRIDGE_HOME") {
            Some(home) => {
                let home = PathBuf::from(home);
                (home.clone(), home)
            }
            None => {
                let dirs = directories::ProjectDirs::from("", "WaiShe", "Bridge")
                    .expect("no user profile directory");
                (
                    dirs.config_dir().to_path_buf(),
                    dirs.data_local_dir().to_path_buf(),
                )
            }
        };
        let p = Paths {
            settings: config.join("settings.json"),
            devices: config.join("devices.json"),
            drivers: data.join("drivers"),
            logs: data.join("logs"),
            webview: data.join("webview"),
        };
        for dir in [&config, &p.drivers, &p.logs] {
            let _ = std::fs::create_dir_all(dir);
        }
        p
    }
}
