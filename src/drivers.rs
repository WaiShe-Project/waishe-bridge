//! Installed drivers: `<drivers>/<id>/<version>/{driver.toml, devices.toml, driver.wasm, ...}` plus
//! `<drivers>/index.json` recording the active version, the previous one (for rollback) and whether
//! the user enabled the driver.

use crate::settings;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use waishe_driver_host::{DriverInstance, DriverModule, HidBackend, Limits, Runtime};
use waishe_driver_types::abi::{self, HidStatus};
use waishe_driver_types::manifest::{DeviceTable, DriverManifest};

const MAX_PACKAGE_BYTES: u64 = 16 << 20;
const PACKAGE_FILES: &[&str] = &[
    "driver.toml",
    "devices.toml",
    "driver.wasm",
    "LICENSE",
    "PROVENANCE.md",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    // Order is precedence: earlier wins when several drivers match.
    Official,
    Community,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    active: String,
    previous: Option<String>,
    enabled: bool,
    trust: Trust,
}

type Index = BTreeMap<String, IndexEntry>;

/// A driver ready to instantiate.
pub struct LoadedDriver {
    pub manifest: DriverManifest,
    pub devices: DeviceTable,
    pub module: DriverModule,
    pub trust: Trust,
}

impl LoadedDriver {
    pub fn id(&self) -> &str {
        &self.manifest.driver.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Active,
    Disabled,
    Faulted(String),
}

pub struct Installed {
    pub driver: Arc<LoadedDriver>,
    pub enabled: bool,
    pub status: Status,
}

pub struct DriverStore {
    root: PathBuf,
    runtime: Runtime,
    index: Index,
    pub drivers: BTreeMap<String, Installed>,
}

#[derive(Debug)]
pub enum InstallError {
    Invalid(String),
    Incompatible(String),
    Io(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Incompatible(m) | Self::Io(m) => f.write_str(m),
        }
    }
}

/// Backend for the install-time ABI check: the driver gets no device.
struct NoDevice;

impl HidBackend for NoDevice {
    fn send_feature(&mut self, _: u32, _: &[u8]) -> Result<(), HidStatus> {
        Err(HidStatus::NoHandle)
    }
    fn get_feature(&mut self, _: u32, _: &mut [u8]) -> Result<usize, HidStatus> {
        Err(HidStatus::NoHandle)
    }
    fn write(&mut self, _: u32, _: &[u8]) -> Result<usize, HidStatus> {
        Err(HidStatus::NoHandle)
    }
    fn read(&mut self, _: u32, _: &mut [u8], _: u32) -> Result<usize, HidStatus> {
        Err(HidStatus::NoHandle)
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Parses and checks a driver directory's contents without installing it.
fn load_dir(rt: &Runtime, dir: &Path, trust: Trust) -> Result<LoadedDriver, InstallError> {
    let read = |name: &str| {
        std::fs::read(dir.join(name)).map_err(|e| InstallError::Invalid(format!("{name}: {e}")))
    };
    let text = |name: &str| {
        read(name).and_then(|b| {
            String::from_utf8(b).map_err(|e| InstallError::Invalid(format!("{name}: {e}")))
        })
    };
    let manifest: DriverManifest = toml::from_str(&text("driver.toml")?)
        .map_err(|e| InstallError::Invalid(format!("driver.toml: {e}")))?;
    let devices: DeviceTable = toml::from_str(&text("devices.toml")?)
        .map_err(|e| InstallError::Invalid(format!("devices.toml: {e}")))?;
    let d = &manifest.driver;
    if !valid_id(&d.id) {
        return Err(InstallError::Invalid(format!(
            "driver id `{}` must be lowercase letters, digits and `-`",
            d.id
        )));
    }
    semver::Version::parse(&d.version)
        .map_err(|e| InstallError::Invalid(format!("version `{}`: {e}", d.version)))?;
    let want = format!("{}.{}", abi::VERSION_MAJOR, abi::VERSION_MINOR);
    if d.api != want {
        return Err(InstallError::Incompatible(format!(
            "driver ABI {} is not supported (this Bridge supports {want})",
            d.api
        )));
    }
    if manifest.matches.is_empty() {
        return Err(InstallError::Invalid(
            "driver.toml has no [[match]] rule".into(),
        ));
    }
    if manifest.matches.iter().any(|m| m.product_ids.is_empty()) {
        return Err(InstallError::Invalid(
            "a [[match]] rule has no product_ids".into(),
        ));
    }
    let module = DriverModule::compile(rt, &read("driver.wasm")?)
        .map_err(|e| InstallError::Invalid(e.to_string()))?;
    DriverInstance::instantiate(rt, &module, NoDevice, Limits::default()).map_err(|e| match e {
        waishe_driver_host::HostError::AbiMismatch { .. } => {
            InstallError::Incompatible(e.to_string())
        }
        e => InstallError::Invalid(e.to_string()),
    })?;
    Ok(LoadedDriver {
        manifest,
        devices,
        module,
        trust,
    })
}

impl DriverStore {
    pub fn open(root: PathBuf) -> Self {
        let runtime = Runtime::new();
        let index: Index = settings::load(&root.join("index.json"));
        let mut store = DriverStore {
            root,
            runtime,
            index,
            drivers: BTreeMap::new(),
        };
        for (id, entry) in store.index.clone() {
            match load_dir(
                &store.runtime,
                &store.root.join(&id).join(&entry.active),
                entry.trust,
            ) {
                Ok(d) => {
                    let status = if entry.enabled {
                        Status::Active
                    } else {
                        Status::Disabled
                    };
                    store.drivers.insert(
                        id,
                        Installed {
                            driver: Arc::new(d),
                            enabled: entry.enabled,
                            status,
                        },
                    );
                }
                Err(e) => {
                    tracing::error!("installed driver {id} {} failed to load: {e}", entry.active)
                }
            }
        }
        tracing::info!("{} driver(s) loaded", store.drivers.len());
        store
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    fn save_index(&self) -> Result<(), InstallError> {
        settings::save(&self.root.join("index.json"), &self.index)
            .map_err(|e| InstallError::Io(e.to_string()))
    }

    /// Installs a `.wsdrv` package (zip). Installation is atomic: the package is unpacked and
    /// verified in a staging directory and only then moved into place (REL-04).
    pub fn install_package(&mut self, bytes: &[u8], trust: Trust) -> Result<String, InstallError> {
        let staging = self
            .root
            .join(".staging")
            .join(format!("{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| InstallError::Io(e.to_string()))?;
        let result = (|| {
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
                .map_err(|e| InstallError::Invalid(format!("not a driver package: {e}")))?;
            let mut total = 0u64;
            for i in 0..zip.len() {
                let mut f = zip
                    .by_index(i)
                    .map_err(|e| InstallError::Invalid(e.to_string()))?;
                let name = f
                    .name()
                    .map_err(|e| InstallError::Invalid(e.to_string()))?
                    .to_string();
                // Only the known flat files; no paths, so no traversal.
                if !PACKAGE_FILES.contains(&name.as_str()) {
                    continue;
                }
                total += f.size();
                if total > MAX_PACKAGE_BYTES {
                    return Err(InstallError::Invalid("package too large".into()));
                }
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)
                    .map_err(|e| InstallError::Invalid(e.to_string()))?;
                std::fs::write(staging.join(&name), buf)
                    .map_err(|e| InstallError::Io(e.to_string()))?;
            }
            let loaded = load_dir(&self.runtime, &staging, trust)?;
            let id = loaded.manifest.driver.id.clone();
            let version = loaded.manifest.driver.version.clone();
            let target = self.root.join(&id).join(&version);
            let _ = std::fs::remove_dir_all(&target);
            std::fs::create_dir_all(target.parent().unwrap())
                .map_err(|e| InstallError::Io(e.to_string()))?;
            std::fs::rename(&staging, &target).map_err(|e| InstallError::Io(e.to_string()))?;
            let previous = self
                .index
                .get(&id)
                .map(|e| e.active.clone())
                .filter(|v| *v != version);
            let enabled = self.index.get(&id).is_none_or(|e| e.enabled);
            self.index.insert(
                id.clone(),
                IndexEntry {
                    active: version.clone(),
                    previous,
                    enabled,
                    trust,
                },
            );
            self.save_index()?;
            let status = if enabled {
                Status::Active
            } else {
                Status::Disabled
            };
            self.drivers.insert(
                id.clone(),
                Installed {
                    driver: Arc::new(loaded),
                    enabled,
                    status,
                },
            );
            tracing::info!("installed driver {id} {version} ({trust:?})");
            Ok(id)
        })();
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), InstallError> {
        let entry = self
            .index
            .get_mut(id)
            .ok_or_else(|| InstallError::Invalid("no such driver".into()))?;
        entry.enabled = enabled;
        self.save_index()?;
        if let Some(d) = self.drivers.get_mut(id) {
            d.enabled = enabled;
            d.status = if enabled {
                Status::Active
            } else {
                Status::Disabled
            };
        }
        Ok(())
    }

    /// Disables a driver for this session after repeated faults (DRV-05). Not persisted: a restart
    /// retries it, and the user can re-enable it at once.
    pub fn mark_faulted(&mut self, id: &str, reason: String) {
        if let Some(d) = self.drivers.get_mut(id) {
            tracing::error!("driver {id} disabled after repeated faults: {reason}");
            d.status = Status::Faulted(reason);
        }
    }

    pub fn uninstall(&mut self, id: &str) -> Result<(), InstallError> {
        self.index
            .remove(id)
            .ok_or_else(|| InstallError::Invalid("no such driver".into()))?;
        self.drivers.remove(id);
        self.save_index()?;
        let _ = std::fs::remove_dir_all(self.root.join(id));
        tracing::info!("uninstalled driver {id}");
        Ok(())
    }

    /// Drivers allowed to claim devices, in precedence order (DRV-02): trust, then newer version.
    pub fn claimants(&self) -> Vec<Arc<LoadedDriver>> {
        let mut v: Vec<_> = self
            .drivers
            .values()
            .filter(|d| d.status == Status::Active)
            .map(|d| d.driver.clone())
            .collect();
        v.sort_by(|a, b| {
            let ver = |d: &LoadedDriver| semver::Version::parse(&d.manifest.driver.version).ok();
            a.trust.cmp(&b.trust).then_with(|| ver(b).cmp(&ver(a)))
        });
        v
    }
}
