//! User settings (API `Settings`) and per-device overrides, persisted as JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub language: String,
    pub autostart: bool,
    pub tray: TraySettings,
    pub low_battery: LowBattery,
    pub poll_interval_seconds: u32,
    pub developer_mode: bool,
    pub network: Network,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TraySettings {
    /// `lowest` or `device`.
    pub mode: String,
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LowBattery {
    pub enabled: bool,
    pub threshold: u8,
    pub interval_minutes: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Network {
    pub registry: bool,
    pub updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            language: "zh-CN".into(),
            autostart: true,
            tray: TraySettings::default(),
            low_battery: LowBattery::default(),
            poll_interval_seconds: 300,
            developer_mode: false,
            network: Network::default(),
        }
    }
}

impl Default for TraySettings {
    fn default() -> Self {
        TraySettings {
            mode: "lowest".into(),
            device_id: None,
        }
    }
}

impl Default for LowBattery {
    fn default() -> Self {
        LowBattery {
            enabled: true,
            threshold: 20,
            interval_minutes: 60,
        }
    }
}

impl Default for Network {
    fn default() -> Self {
        Network {
            registry: true,
            updates: true,
        }
    }
}

impl Settings {
    fn validate(&self) -> Result<(), String> {
        if !["zh-CN", "en"].contains(&self.language.as_str()) {
            return Err("language must be zh-CN or en".into());
        }
        if !["lowest", "device"].contains(&self.tray.mode.as_str()) {
            return Err("tray.mode must be lowest or device".into());
        }
        if !(5..=95).contains(&self.low_battery.threshold) {
            return Err("lowBattery.threshold must be between 5 and 95".into());
        }
        if !(1..=1440).contains(&self.low_battery.interval_minutes) {
            return Err("lowBattery.intervalMinutes must be between 1 and 1440".into());
        }
        if !(30..=3600).contains(&self.poll_interval_seconds) {
            return Err("pollIntervalSeconds must be between 30 and 3600".into());
        }
        Ok(())
    }

    /// Applies a partial JSON object; nested objects merge field by field.
    pub fn patched(&self, patch: &Json) -> Result<Settings, String> {
        let mut base = serde_json::to_value(self).expect("settings serialize");
        merge(&mut base, patch);
        let next: Settings = serde_json::from_value(base).map_err(|e| e.to_string())?;
        next.validate()?;
        Ok(next)
    }
}

fn merge(base: &mut Json, patch: &Json) {
    match (base, patch) {
        (Json::Object(b), Json::Object(p)) => {
            for (k, v) in p {
                merge(b.entry(k.clone()).or_insert(Json::Null), v);
            }
        }
        (b, p) => *b = p.clone(),
    }
}

/// Alias and visibility per device id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DeviceOverride {
    pub alias: Option<String>,
    pub hidden: bool,
}

pub type DeviceOverrides = BTreeMap<String, DeviceOverride>;

/// Loads JSON or returns the default. A file that cannot be parsed is kept as `<name>.corrupt-<unix>`
/// so nothing the user configured is silently lost (APP-03).
pub fn load<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return T::default(),
    };
    match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup = path.with_extension(format!("corrupt-{stamp}"));
            tracing::warn!(
                "{} is not valid ({e}); kept as {} and using defaults",
                path.display(),
                backup.display()
            );
            let _ = std::fs::rename(path, backup);
            T::default()
        }
    }
}

/// Writes atomically: temp file, then rename over the target.
pub fn save<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let tmp: PathBuf = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value).expect("serialize"))?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_merges_nested_fields_and_validates() {
        let s = Settings::default();
        let next = s
            .patched(&serde_json::json!({ "lowBattery": { "threshold": 30 } }))
            .unwrap();
        assert_eq!(next.low_battery.threshold, 30);
        assert_eq!(next.low_battery.interval_minutes, 60);
        assert!(
            s.patched(&serde_json::json!({ "lowBattery": { "threshold": 0 } }))
                .is_err()
        );
        assert!(
            s.patched(&serde_json::json!({ "tray": { "mode": "biggest" } }))
                .is_err()
        );
    }

    #[test]
    fn corrupt_file_is_kept_and_defaults_used() {
        let dir = std::env::temp_dir().join(format!("waishe-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        std::fs::write(&p, "{ not json").unwrap();
        let s: Settings = load(&p);
        assert_eq!(s, Settings::default());
        assert!(!p.exists());
        assert!(
            std::fs::read_dir(&dir).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("corrupt"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
