//! Single source of truth for devices, readings and settings. Workers write into it; the API,
//! tray and notifier read from it. Every change is broadcast as an [`Event`].

use crate::drivers::{LoadedDriver, Trust};
use crate::settings::{DeviceOverride, DeviceOverrides, Settings};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::broadcast;
use waishe_driver_types::cap::{charging, id as cap, link};
use waishe_driver_types::{
    CapabilityDescriptor, Constraint, Persistence, Precision, ProbeResult, ReadError, Transport,
    Unavailable, Value, ValueKind,
};

pub type DeviceId = String;
pub type EndpointKey = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Connected,
    Asleep,
    Disconnected,
    Unknown,
}

impl Link {
    fn as_str(self) -> &'static str {
        match self {
            Link::Connected => "connected",
            Link::Asleep => "asleep",
            Link::Disconnected => "disconnected",
            Link::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Ok(Value),
    Unavailable(&'static str),
    Unknown,
}

#[derive(Debug, Clone)]
struct Reading {
    state: State,
    observed_at: Option<SystemTime>,
    last_ok: Option<(Value, SystemTime)>,
}

/// Messages to an endpoint worker.
pub enum Command {
    Write {
        capability: String,
        value: Value,
        reply: tokio::sync::oneshot::Sender<WriteReply>,
    },
    Refresh,
    Shutdown,
}

/// Result of a write, ready for the API (`local-api.md` section 5).
#[derive(Debug)]
pub struct WriteReply {
    /// `applied`, `accepted`, `rejected` or `failed`.
    pub outcome: &'static str,
    pub message: Option<String>,
}

struct Endpoint {
    key: EndpointKey,
    transport: Transport,
    vendor_id: u16,
    product_id: u16,
    link: Link,
    tx: Sender<Command>,
}

struct Device {
    id: DeviceId,
    name: String,
    brand: String,
    model: String,
    kind: String,
    driver: Arc<LoadedDriver>,
    endpoints: Vec<Endpoint>,
    /// Capabilities of the most recent active endpoint; kept after unplug for the history view.
    caps: Vec<CapabilityDescriptor>,
    transport: Transport,
    readings: BTreeMap<String, Reading>,
}

impl Device {
    fn link(&self) -> Link {
        if self.endpoints.iter().any(|e| e.link == Link::Connected) {
            Link::Connected
        } else if self.endpoints.is_empty() {
            Link::Disconnected
        } else if self.endpoints.iter().any(|e| e.link == Link::Asleep) {
            Link::Asleep
        } else if self.endpoints.iter().all(|e| e.link == Link::Disconnected) {
            Link::Disconnected
        } else {
            Link::Unknown
        }
    }

    /// The endpoint that serves reads and writes: a connected one if any.
    fn active(&self) -> Option<&Endpoint> {
        self.endpoints
            .iter()
            .find(|e| e.link == Link::Connected)
            .or(self.endpoints.first())
    }

    fn reading_value(&self, id: &str) -> Option<&Value> {
        match self.readings.get(id).map(|r| &r.state) {
            Some(State::Ok(v)) => Some(v),
            _ => None,
        }
    }

    fn is_charging(&self) -> bool {
        matches!(self.reading_value(cap::BATTERY_CHARGING), Some(Value::Enum(s)) if s == charging::CHARGING || s == charging::FULL)
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    DeviceAdded(DeviceId),
    DeviceUpdated(DeviceId),
    DeviceRemoved(DeviceId),
    Capability(DeviceId, String),
    Driver(String),
    DriverRemoved(String),
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIconStyle {
    Battery,
    Brand,
}

/// Five-tier battery fill level (`0..=5`) per WaiShe tray icon specification section 2.1.
pub fn battery_tier(percent: u8) -> u8 {
    match percent {
        0 => 0,
        1..=20 => 1,
        21..=40 => 2,
        41..=60 => 3,
        61..=80 => 4,
        _ => 5,
    }
}

/// Primary tray visual state ordered by specification section 5 priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayVisualState {
    Error,
    Disconnected,
    Unknown,
    CriticalLow { tier: u8 },
    Charging { tier: u8 },
    Low { tier: u8 },
    Normal { tier: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayAlertLevel {
    CriticalLow,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceTrayStatus {
    Disconnected,
    Error,
    Unknown,
    Valid { percent: u8, charging: bool },
}

#[derive(Debug, Clone)]
struct TrayDeviceSnapshot {
    id: DeviceId,
    name: String,
    hidden: bool,
    status: DeviceTrayStatus,
}

/// Resolved tray icon appearance and single-line tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraySummary {
    pub style: TrayIconStyle,
    pub state: TrayVisualState,
    pub target_device_id: Option<String>,
    pub tooltip: String,
}

#[derive(Default)]
struct LowBatteryState {
    armed: bool,
    last: Option<Instant>,
}

struct Inner {
    devices: BTreeMap<DeviceId, Device>,
    settings: Settings,
    overrides: DeviceOverrides,
    low_battery: BTreeMap<DeviceId, LowBatteryState>,
    tray_alerts: BTreeMap<DeviceId, TrayAlertLevel>,
}

type Callback = Box<dyn Fn() + Send + Sync>;
type LowBatteryCallback = Box<dyn Fn(&str, u8) + Send + Sync>;

pub struct Hub {
    inner: Mutex<Inner>,
    events: broadcast::Sender<Event>,
    observers: AtomicUsize,
    on_change: Mutex<Option<Callback>>,
    on_low_battery: Mutex<Option<LowBatteryCallback>>,
    settings_path: std::path::PathBuf,
    overrides_path: std::path::PathBuf,
}

// ---------------------------------------------------------------- conversions

pub fn value_json(v: &Value) -> Json {
    match v {
        Value::Percent(p) => json!(p),
        Value::Bool(b) => json!(b),
        Value::Integer(i) => json!(i),
        Value::Enum(s) | Value::Text(s) => json!(s),
        Value::Dpi { x, y } => json!({ "x": x, "y": y }),
    }
}

pub fn value_from_json(kind: &ValueKind, j: &Json) -> Option<Value> {
    Some(match kind {
        ValueKind::Percent => Value::Percent(u8::try_from(j.as_u64()?).ok().filter(|p| *p <= 100)?),
        ValueKind::Bool => Value::Bool(j.as_bool()?),
        ValueKind::Integer { .. } => Value::Integer(j.as_i64()?),
        ValueKind::Enum { variants } => Value::Enum(
            j.as_str()
                .filter(|s| variants.iter().any(|v| v == s))?
                .to_string(),
        ),
        ValueKind::Text => Value::Text(j.as_str()?.to_string()),
        ValueKind::Dpi => Value::Dpi {
            x: u32::try_from(j.get("x")?.as_u64()?).ok()?,
            y: u32::try_from(j.get("y")?.as_u64()?).ok()?,
        },
    })
}

fn reason(u: Unavailable) -> &'static str {
    match u {
        Unavailable::Asleep => "asleep",
        Unavailable::Disconnected => "disconnected",
        Unavailable::Busy => "busy",
        Unavailable::Timeout => "timeout",
        Unavailable::Io => "io",
    }
}

fn transport_str(t: Transport) -> &'static str {
    match t {
        Transport::Wired => "wired",
        Transport::Receiver => "receiver",
        Transport::Bluetooth => "bluetooth",
    }
}

fn trust_str(t: Trust) -> &'static str {
    match t {
        Trust::Official => "official",
        Trust::Community => "community",
        Trust::Local => "local",
    }
}

/// ISO 8601 UTC with second precision.
pub fn iso(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn device_id(driver: &str, model: &str, identity: &str) -> DeviceId {
    let mut h = Sha256::new();
    h.update(driver.as_bytes());
    h.update([0]);
    h.update(model.as_bytes());
    h.update([0]);
    h.update(identity.as_bytes());
    let d = h.finalize();
    format!(
        "dev_{}",
        d[..5]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

// ---------------------------------------------------------------- hub

impl Hub {
    pub fn new(settings_path: std::path::PathBuf, overrides_path: std::path::PathBuf) -> Arc<Self> {
        let settings: Settings = crate::settings::load(&settings_path);
        let overrides: DeviceOverrides = crate::settings::load(&overrides_path);
        Arc::new(Hub {
            inner: Mutex::new(Inner {
                devices: BTreeMap::new(),
                settings,
                overrides,
                low_battery: BTreeMap::new(),
                tray_alerts: BTreeMap::new(),
            }),
            events: broadcast::channel(256).0,
            observers: AtomicUsize::new(0),
            on_change: Mutex::new(None),
            on_low_battery: Mutex::new(None),
            settings_path,
            overrides_path,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn set_on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_change.lock().unwrap() = Some(Box::new(f));
    }

    pub fn set_on_low_battery(&self, f: impl Fn(&str, u8) + Send + Sync + 'static) {
        *self.on_low_battery.lock().unwrap() = Some(Box::new(f));
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
        if let Some(f) = self.on_change.lock().unwrap().as_ref() {
            f();
        }
    }

    /// Event-stream subscribers; workers poll faster while someone is watching.
    pub fn observer_guard(self: &Arc<Self>) -> ObserverGuard {
        self.observers.fetch_add(1, Ordering::SeqCst);
        ObserverGuard(self.clone())
    }

    pub fn has_observers(&self) -> bool {
        self.observers.load(Ordering::SeqCst) > 0
    }

    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }

    pub fn update_settings(&self, patch: &Json) -> Result<Settings, String> {
        let next = {
            let mut g = self.inner.lock().unwrap();
            let next = g.settings.patched(patch)?;
            crate::settings::save(&self.settings_path, &next).map_err(|e| e.to_string())?;
            g.settings = next.clone();
            next
        };
        self.emit(Event::Settings);
        Ok(next)
    }

    // ------------------------------------------------------------ endpoints

    /// Registers a probed endpoint and returns its device id. Endpoints with the same driver,
    /// model and stable id are one device.
    pub fn endpoint_ready(&self, n: NewEndpoint<'_>) -> DeviceId {
        let NewEndpoint {
            key,
            container,
            driver,
            probe,
            caps,
            vendor_id,
            product_id,
            tx,
        } = n;
        let identity = probe.stable_id.as_deref().unwrap_or(container);
        let id = device_id(driver.id(), &probe.model, identity);
        let entry = driver
            .devices
            .devices
            .iter()
            .find(|d| d.model == probe.model);
        let endpoint = Endpoint {
            key: key.clone(),
            transport: probe.transport,
            vendor_id,
            product_id,
            link: Link::Unknown,
            tx,
        };
        let added = {
            let mut g = self.inner.lock().unwrap();
            match g.devices.get_mut(&id) {
                Some(d) => {
                    d.endpoints.retain(|e| e.key != *key);
                    d.endpoints.push(endpoint);
                    d.caps = caps;
                    d.transport = probe.transport;
                    d.driver = driver.clone();
                    if let Some(fw) = &probe.firmware {
                        set_reading(d, cap::DEVICE_FIRMWARE, Ok(Value::Text(fw.clone())));
                    }
                    false
                }
                None => {
                    let mut d = Device {
                        id: id.clone(),
                        name: entry.map_or_else(|| probe.model.clone(), |e| e.name.clone()),
                        brand: entry.map_or_else(String::new, |e| e.brand.clone()),
                        model: probe.model.clone(),
                        kind: entry.map_or_else(|| "unknown".into(), |e| e.kind.clone()),
                        driver: driver.clone(),
                        endpoints: vec![endpoint],
                        caps,
                        transport: probe.transport,
                        readings: BTreeMap::new(),
                    };
                    if let Some(fw) = &probe.firmware {
                        set_reading(&mut d, cap::DEVICE_FIRMWARE, Ok(Value::Text(fw.clone())));
                    }
                    g.devices.insert(id.clone(), d);
                    true
                }
            }
        };
        tracing::info!("endpoint {key} serves device {id} ({})", probe.model);
        self.emit(if added {
            Event::DeviceAdded(id.clone())
        } else {
            Event::DeviceUpdated(id.clone())
        });
        id
    }

    /// The endpoint is gone (unplugged, driver disabled, worker stopped). The device stays listed
    /// with its last readings, marked historical.
    pub fn endpoint_gone(&self, key: &EndpointKey) {
        let changed: Vec<DeviceId> = {
            let mut g = self.inner.lock().unwrap();
            g.devices
                .values_mut()
                .filter_map(|d| {
                    let before = d.endpoints.len();
                    d.endpoints.retain(|e| e.key != *key);
                    (d.endpoints.len() != before).then(|| d.id.clone())
                })
                .collect()
        };
        for id in changed {
            self.emit(Event::DeviceUpdated(id));
        }
    }

    /// Removes devices of an uninstalled driver.
    pub fn forget_driver_devices(&self, driver_id: &str) {
        let gone: Vec<DeviceId> = {
            let mut g = self.inner.lock().unwrap();
            let ids: Vec<_> = g
                .devices
                .values()
                .filter(|d| d.driver.id() == driver_id)
                .map(|d| d.id.clone())
                .collect();
            for id in &ids {
                g.devices.remove(id);
            }
            ids
        };
        for id in gone {
            self.emit(Event::DeviceRemoved(id));
        }
    }

    /// A reading from an endpoint. Only the active endpoint's readings count, except `link.state`,
    /// which always describes its own endpoint.
    pub fn update(
        &self,
        device: &DeviceId,
        key: &EndpointKey,
        capability: &str,
        reading: Result<Value, ReadError>,
    ) {
        let mut low_battery: Option<(String, u8)> = None;
        let mut link_changed = false;
        let changed = {
            let mut g = self.inner.lock().unwrap();
            let settings = g.settings.clone();
            let name = g.overrides.get(device).and_then(|o| o.alias.clone());
            let Some(d) = g.devices.get_mut(device) else {
                return;
            };
            if capability == cap::LINK_STATE {
                let new = match &reading {
                    Ok(Value::Enum(s)) if s == link::CONNECTED => Some(Link::Connected),
                    Ok(Value::Enum(s)) if s == link::ASLEEP => Some(Link::Asleep),
                    Ok(Value::Enum(s)) if s == link::DISCONNECTED => Some(Link::Disconnected),
                    Err(ReadError::Unavailable(Unavailable::Disconnected)) => {
                        Some(Link::Disconnected)
                    }
                    Err(ReadError::Unavailable(Unavailable::Asleep)) => Some(Link::Asleep),
                    // Timeouts and busy answers say nothing about the link: keep what we knew.
                    Err(_) => None,
                    Ok(_) => Some(Link::Unknown),
                };
                let new = new.unwrap_or_else(|| {
                    d.endpoints
                        .iter()
                        .find(|e| e.key == *key)
                        .map_or(Link::Unknown, |e| e.link)
                });
                if let Some(e) = d.endpoints.iter_mut().find(|e| e.key == *key)
                    && e.link != new
                {
                    e.link = new;
                    link_changed = true;
                }
            }
            if d.active().map(|e| &e.key) != Some(key) && capability != cap::LINK_STATE {
                return;
            }
            let changed = set_reading(d, capability, reading);
            if capability == cap::BATTERY_LEVEL
                && let Some(Value::Percent(p)) = d.reading_value(cap::BATTERY_LEVEL).cloned()
            {
                let charging = d.is_charging();
                let display = name.unwrap_or_else(|| d.name.clone());
                let st = g
                    .low_battery
                    .entry(device.clone())
                    .or_insert(LowBatteryState {
                        armed: true,
                        last: None,
                    });
                let lb = &settings.low_battery;
                if charging || p >= lb.threshold.saturating_add(5) {
                    st.armed = true;
                } else if lb.enabled && p <= lb.threshold {
                    let interval = Duration::from_secs(lb.interval_minutes as u64 * 60);
                    if st.armed || st.last.is_none_or(|t| t.elapsed() >= interval) {
                        st.armed = false;
                        st.last = Some(Instant::now());
                        low_battery = Some((display, p));
                    }
                }
            }
            changed
        };
        if link_changed {
            self.emit(Event::DeviceUpdated(device.clone()));
        }
        if changed {
            self.emit(Event::Capability(device.clone(), capability.to_string()));
        }
        if let Some((name, p)) = low_battery
            && let Some(f) = self.on_low_battery.lock().unwrap().as_ref()
        {
            f(&name, p);
        }
    }

    /// Where to send commands for a device.
    pub fn command_target(&self, device: &DeviceId) -> Option<Sender<Command>> {
        let g = self.inner.lock().unwrap();
        g.devices.get(device)?.active().map(|e| e.tx.clone())
    }

    pub fn descriptor(&self, device: &DeviceId, capability: &str) -> Option<CapabilityDescriptor> {
        let g = self.inner.lock().unwrap();
        g.devices
            .get(device)?
            .caps
            .iter()
            .find(|c| c.id == capability)
            .cloned()
    }

    pub fn set_override(&self, device: &DeviceId, patch: &Json) -> Result<(), String> {
        {
            let mut g = self.inner.lock().unwrap();
            if !g.devices.contains_key(device) {
                return Err("device_not_found".into());
            }
            let mut o: DeviceOverride = g.overrides.get(device).cloned().unwrap_or_default();
            if let Some(a) = patch.get("alias") {
                o.alias = a
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.chars().take(64).collect());
            }
            if let Some(h) = patch.get("hidden").and_then(Json::as_bool) {
                o.hidden = h;
            }
            g.overrides.insert(device.clone(), o);
            crate::settings::save(&self.overrides_path, &g.overrides).map_err(|e| e.to_string())?;
        }
        self.emit(Event::DeviceUpdated(device.clone()));
        Ok(())
    }

    pub fn driver_changed(&self, id: &str, removed: bool) {
        self.emit(if removed {
            Event::DriverRemoved(id.into())
        } else {
            Event::Driver(id.into())
        });
    }

    // ------------------------------------------------------------ views

    pub fn device_json(&self, id: &DeviceId) -> Option<Json> {
        let g = self.inner.lock().unwrap();
        let d = g.devices.get(id)?;
        Some(render_device(d, g.overrides.get(id), &g.settings))
    }

    pub fn devices_json(&self) -> Json {
        let g = self.inner.lock().unwrap();
        Json::Array(
            g.devices
                .values()
                .map(|d| render_device(d, g.overrides.get(&d.id), &g.settings))
                .collect(),
        )
    }

    pub fn capability_json(&self, id: &DeviceId, capability: &str) -> Option<Json> {
        let g = self.inner.lock().unwrap();
        let d = g.devices.get(id)?;
        let c = d.caps.iter().find(|c| c.id == capability)?;
        Some(render_capability(d, c, &g.settings))
    }

    pub fn reading_json(&self, id: &DeviceId, capability: &str) -> Json {
        self.capability_json(id, capability)
            .and_then(|c| c.get("reading").cloned())
            .unwrap_or(json!({ "state": "unknown" }))
    }

    pub fn tray_summary(&self) -> TraySummary {
        let mut g = self.inner.lock().unwrap();
        let snapshots: Vec<TrayDeviceSnapshot> = g
            .devices
            .values()
            .map(|d| {
                let o = g.overrides.get(&d.id);
                let hidden = o.is_some_and(|o| o.hidden);
                let name = o
                    .and_then(|o| o.alias.clone())
                    .unwrap_or_else(|| d.name.clone());
                let status = evaluate_device_tray_status(d, &g.settings);
                TrayDeviceSnapshot {
                    id: d.id.clone(),
                    name,
                    hidden,
                    status,
                }
            })
            .collect();
        let settings = g.settings.clone();
        resolve_tray_summary(&snapshots, &settings, &mut g.tray_alerts)
    }
}

fn evaluate_device_tray_status(d: &Device, settings: &Settings) -> DeviceTrayStatus {
    match d.link() {
        Link::Disconnected => return DeviceTrayStatus::Disconnected,
        Link::Asleep | Link::Unknown => return DeviceTrayStatus::Unknown,
        Link::Connected => {}
    }

    let Some(r) = d.readings.get(cap::BATTERY_LEVEL) else {
        return DeviceTrayStatus::Unknown;
    };
    match &r.state {
        State::Ok(Value::Percent(p)) => {
            if is_stale(d, cap::BATTERY_LEVEL, settings) {
                DeviceTrayStatus::Unknown
            } else {
                DeviceTrayStatus::Valid {
                    percent: *p,
                    charging: d.is_charging(),
                }
            }
        }
        State::Unavailable(why) if matches!(*why, "protocol" | "io") => {
            if has_fresh_last_ok(d, r, settings) {
                DeviceTrayStatus::Unknown
            } else {
                DeviceTrayStatus::Error
            }
        }
        State::Unavailable(_) | State::Unknown | State::Ok(_) => DeviceTrayStatus::Unknown,
    }
}

fn has_fresh_last_ok(d: &Device, r: &Reading, settings: &Settings) -> bool {
    let Some((_, t)) = r.last_ok else {
        return false;
    };
    let Some(c) = d.caps.iter().find(|c| c.id == cap::BATTERY_LEVEL) else {
        return false;
    };
    match poll_seconds(c, settings) {
        Some(p) => t.elapsed().unwrap_or_default() <= Duration::from_secs(p * 12 / 5),
        None => false,
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_none() {
        prefix
    } else if max_chars > 1 {
        let trimmed: String = s.chars().take(max_chars - 1).collect();
        format!("{trimmed}…")
    } else {
        "…".into()
    }
}

fn classify_valid_battery(
    device_id: &str,
    percent: u8,
    charging: bool,
    threshold: u8,
    alerts: &mut BTreeMap<DeviceId, TrayAlertLevel>,
) -> TrayVisualState {
    let tier = battery_tier(percent);
    if charging {
        alerts.remove(device_id);
        return TrayVisualState::Charging { tier };
    }

    let prev = alerts.get(device_id).copied();
    let critical = percent <= 10 || (prev == Some(TrayAlertLevel::CriticalLow) && percent <= 12);
    if critical {
        alerts.insert(device_id.to_string(), TrayAlertLevel::CriticalLow);
        return TrayVisualState::CriticalLow { tier };
    }

    let low_exit = threshold.saturating_add(2).min(98);
    let low = percent <= threshold || (prev == Some(TrayAlertLevel::Low) && percent <= low_exit);
    if low {
        alerts.insert(device_id.to_string(), TrayAlertLevel::Low);
        return TrayVisualState::Low { tier };
    }

    alerts.remove(device_id);
    TrayVisualState::Normal { tier }
}

fn resolve_tray_summary(
    snapshots: &[TrayDeviceSnapshot],
    settings: &Settings,
    alerts: &mut BTreeMap<DeviceId, TrayAlertLevel>,
) -> TraySummary {
    let style = match settings.tray.style.as_str() {
        "brand" => TrayIconStyle::Brand,
        _ => TrayIconStyle::Battery,
    };
    let en = settings.language == "en";
    let threshold = settings.low_battery.threshold;

    let (state, target_device_id, raw_tooltip) =
        if settings.tray.mode == "device"
            && let Some(pinned_id) = settings.tray.device_id.as_deref()
        {
            match snapshots.iter().find(|s| s.id == pinned_id) {
                None => (
                    TrayVisualState::Disconnected,
                    Some(pinned_id.to_string()),
                    if en {
                        "WaiShe · Pinned device unavailable".into()
                    } else {
                        "WaiShe · 指定设备不可用".into()
                    },
                ),
                Some(dev) => {
                    let name = truncate_chars(&dev.name, 36);
                    match dev.status {
                        DeviceTrayStatus::Disconnected => (
                            TrayVisualState::Disconnected,
                            Some(dev.id.clone()),
                            if en {
                                format!("WaiShe · {name} · Disconnected")
                            } else {
                                format!("WaiShe · {name} · 已断开")
                            },
                        ),
                        DeviceTrayStatus::Error => (
                            TrayVisualState::Error,
                            Some(dev.id.clone()),
                            if en {
                                format!("WaiShe · {name} · Driver error")
                            } else {
                                format!("WaiShe · {name} · 驱动监控异常")
                            },
                        ),
                        DeviceTrayStatus::Unknown => (
                            TrayVisualState::Unknown,
                            Some(dev.id.clone()),
                            if en {
                                format!("WaiShe · {name} · Battery unavailable")
                            } else {
                                format!("WaiShe · {name} · 电量暂不可用")
                            },
                        ),
                        DeviceTrayStatus::Valid { percent, charging } => {
                            let st = classify_valid_battery(
                                &dev.id, percent, charging, threshold, alerts,
                            );
                            let status_text = match st {
                                TrayVisualState::Charging { .. } => {
                                    if en { "Charging" } else { "充电中" }
                                }
                                TrayVisualState::CriticalLow { .. } => {
                                    if en { "Critically low" } else { "严重低电量" }
                                }
                                TrayVisualState::Low { .. } => {
                                    if en { "Low battery" } else { "低电量" }
                                }
                                _ => {
                                    if en { "Not charging" } else { "未充电" }
                                }
                            };
                            (
                                st,
                                Some(dev.id.clone()),
                                format!("WaiShe · {name} · {percent}% · {status_text}"),
                            )
                        }
                    }
                }
            }
        } else {
            let visible: Vec<&TrayDeviceSnapshot> =
                snapshots.iter().filter(|s| !s.hidden).collect();
            let chosen_valid = visible
                .iter()
                .filter_map(|s| match s.status {
                    DeviceTrayStatus::Valid {
                        percent,
                        charging: false,
                    } => Some((s, percent, false)),
                    _ => None,
                })
                .min_by_key(|(_, pct, _)| *pct)
                .or_else(|| {
                    visible
                        .iter()
                        .filter_map(|s| match s.status {
                            DeviceTrayStatus::Valid {
                                percent,
                                charging: true,
                            } => Some((s, percent, true)),
                            _ => None,
                        })
                        .min_by_key(|(_, pct, _)| *pct)
                });

            if let Some((dev, percent, charging)) = chosen_valid {
                let name = truncate_chars(&dev.name, 36);
                let st =
                    classify_valid_battery(&dev.id, percent, charging, threshold, alerts);
                let tip = match (en, charging) {
                    (true, true) => format!("WaiShe · Lowest: {name} · {percent}% · Charging"),
                    (true, false) => format!("WaiShe · Lowest: {name} · {percent}%"),
                    (false, true) => format!("WaiShe · 最低电量：{name} · {percent}% · 充电中"),
                    (false, false) => format!("WaiShe · 最低电量：{name} · {percent}%"),
                };
                (st, Some(dev.id.clone()), tip)
            } else if let Some(err_dev) = visible
                .iter()
                .find(|s| s.status == DeviceTrayStatus::Error)
            {
                let name = truncate_chars(&err_dev.name, 36);
                (
                    TrayVisualState::Error,
                    Some(err_dev.id.clone()),
                    if en {
                        format!("WaiShe · {name} · Driver error")
                    } else {
                        format!("WaiShe · {name} · 驱动监控异常")
                    },
                )
            } else if let Some(unk_dev) = visible
                .iter()
                .find(|s| s.status == DeviceTrayStatus::Unknown)
            {
                let name = truncate_chars(&unk_dev.name, 36);
                (
                    TrayVisualState::Unknown,
                    Some(unk_dev.id.clone()),
                    if en {
                        format!("WaiShe · {name} · Battery unavailable")
                    } else {
                        format!("WaiShe · {name} · 电量暂不可用")
                    },
                )
            } else if visible
                .iter()
                .any(|s| s.status == DeviceTrayStatus::Disconnected)
            {
                (
                    TrayVisualState::Disconnected,
                    None,
                    if en {
                        "WaiShe · Devices disconnected".into()
                    } else {
                        "WaiShe · 设备已断开".into()
                    },
                )
            } else {
                (TrayVisualState::Unknown, None, "WaiShe".into())
            }
        };

    // Windows native tray tooltips are bounded at 127 UTF-16 code units.
    let tooltip = truncate_chars(&raw_tooltip, 127);
    TraySummary {
        style,
        state,
        target_device_id,
        tooltip,
    }
}

/// Arguments of [`Hub::endpoint_ready`].
pub struct NewEndpoint<'a> {
    pub key: &'a EndpointKey,
    pub container: &'a str,
    pub driver: &'a Arc<LoadedDriver>,
    pub probe: &'a ProbeResult,
    pub caps: Vec<CapabilityDescriptor>,
    pub vendor_id: u16,
    pub product_id: u16,
    pub tx: Sender<Command>,
}

pub struct ObserverGuard(Arc<Hub>);

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        self.0.observers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Stores a reading; returns whether anything visible changed.
fn set_reading(d: &mut Device, capability: &str, r: Result<Value, ReadError>) -> bool {
    let now = SystemTime::now();
    let prev = d.readings.get(capability).cloned();
    let last_ok = match &r {
        Ok(v) => Some((v.clone(), now)),
        Err(_) => prev.as_ref().and_then(|p| p.last_ok.clone()),
    };
    let state = match r {
        Ok(v) => State::Ok(v),
        Err(ReadError::Unavailable(u)) => State::Unavailable(reason(u)),
        Err(ReadError::Protocol(m)) => {
            tracing::warn!("device {} {capability}: protocol error: {m}", d.id);
            State::Unavailable("protocol")
        }
        Err(ReadError::Unsupported) => State::Unknown,
    };
    let changed = prev.as_ref().is_none_or(|p| p.state != state);
    d.readings.insert(
        capability.to_string(),
        Reading {
            state,
            observed_at: Some(now),
            last_ok,
        },
    );
    // A fresh value with the same content still refreshes `observedAt`; report it so stale markers clear.
    changed
        || prev.is_some_and(|p| {
            p.observed_at.is_some_and(|t| {
                now.duration_since(t).unwrap_or_default() > Duration::from_secs(60)
            })
        })
}

fn poll_seconds(c: &CapabilityDescriptor, s: &Settings) -> Option<u64> {
    c.poll_interval_s
        .map(|p| (p as u64).max(s.poll_interval_seconds as u64))
}

fn is_stale(d: &Device, capability: &str, s: &Settings) -> bool {
    if d.link() != Link::Connected {
        return true;
    }
    let Some(r) = d.readings.get(capability) else {
        return false;
    };
    let Some(c) = d.caps.iter().find(|c| c.id == capability) else {
        return false;
    };
    match (poll_seconds(c, s), r.observed_at) {
        // Historical after 2.4 polling intervals without a fresh value.
        (Some(p), Some(t)) => t.elapsed().unwrap_or_default() > Duration::from_secs(p * 12 / 5),
        _ => false,
    }
}

fn render_capability(d: &Device, c: &CapabilityDescriptor, s: &Settings) -> Json {
    let mut access = vec![];
    if c.access.read {
        access.push("read");
    }
    if c.access.write {
        access.push("write");
    }
    if c.access.notify {
        access.push("notify");
    }
    let (ty, unit, variants) = match &c.kind {
        ValueKind::Percent => ("percent", Json::Null, Json::Null),
        ValueKind::Bool => ("bool", Json::Null, Json::Null),
        ValueKind::Integer { unit } => ("integer", json!(unit), Json::Null),
        ValueKind::Enum { variants } => ("enum", Json::Null, json!(variants)),
        ValueKind::Text => ("text", Json::Null, Json::Null),
        ValueKind::Dpi => ("dpi", Json::Null, Json::Null),
    };
    let constraint = match &c.constraint {
        None => Json::Null,
        Some(Constraint::Range { min, max, step }) => {
            json!({ "kind": "range", "min": min, "max": max, "step": step })
        }
        Some(Constraint::Choices(v)) => json!({ "kind": "choices", "values": v }),
        Some(Constraint::Dpi {
            min,
            max,
            step,
            independent_xy,
        }) => {
            json!({ "kind": "dpi", "min": min, "max": max, "step": step, "independentXY": independent_xy })
        }
    };
    let persistence = c.persistence.map(|p| match p {
        Persistence::Onboard => "onboard",
        Persistence::Volatile => "volatile",
        Persistence::Unknown => "unknown",
    });
    let precision = c.precision.map(|p| match p {
        Precision::Exact => json!({ "kind": "exact" }),
        Precision::Steps(n) => json!({ "kind": "steps", "count": n }),
    });
    let transport = transport_str(d.transport);
    let verification = d
        .driver
        .devices
        .devices
        .iter()
        .find(|e| e.model == d.model)
        .and_then(|e| {
            e.verifications
                .iter()
                .find(|v| v.capability == c.id && v.transport == transport)
        })
        .map(|v| v.level.clone());
    let reading = match d.readings.get(&c.id) {
        None => json!({ "state": "unknown" }),
        Some(r) => match &r.state {
            State::Ok(v) => json!({
                "state": "ok",
                "value": value_json(v),
                "observedAt": r.observed_at.map(iso),
                "stale": is_stale(d, &c.id, s),
            }),
            State::Unavailable(why) => json!({
                "state": "unavailable",
                "reason": why,
                "observedAt": r.observed_at.map(iso),
                "lastOk": r.last_ok.as_ref().map(|(v, t)| json!({ "value": value_json(v), "observedAt": iso(*t) })),
            }),
            State::Unknown => json!({ "state": "unknown" }),
        },
    };
    json!({
        "id": c.id,
        "access": access,
        "type": ty,
        "unit": unit,
        "variants": variants,
        "constraint": constraint,
        "persistence": persistence,
        "precision": precision,
        "verification": verification,
        "reading": reading,
    })
}

fn render_device(d: &Device, o: Option<&DeviceOverride>, s: &Settings) -> Json {
    let active = d.active().map(|e| e.key.clone());
    json!({
        "id": d.id,
        "name": d.name,
        "alias": o.and_then(|o| o.alias.clone()),
        "hidden": o.is_some_and(|o| o.hidden),
        "brand": d.brand,
        "model": d.model,
        "kind": d.kind,
        "link": d.link().as_str(),
        "driver": {
            "id": d.driver.id(),
            "version": d.driver.manifest.driver.version,
            "trust": trust_str(d.driver.trust),
        },
        "endpoints": d.endpoints.iter().map(|e| json!({
            "transport": transport_str(e.transport),
            "vendorId": e.vendor_id,
            "productId": e.product_id,
            "link": e.link.as_str(),
            "active": Some(&e.key) == active.as_ref(),
        })).collect::<Vec<_>>(),
        "capabilities": d.caps.iter().map(|c| render_capability(d, c, s)).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_formats_utc() {
        assert_eq!(iso(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            iso(SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_645_012)),
            "2026-11-14T08:30:12Z"
        );
        assert_eq!(
            iso(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }

    #[test]
    fn json_values_round_trip_with_kind_checks() {
        let dpi = json!({ "x": 1600, "y": 800 });
        assert_eq!(
            value_from_json(&ValueKind::Dpi, &dpi),
            Some(Value::Dpi { x: 1600, y: 800 })
        );
        assert_eq!(value_from_json(&ValueKind::Percent, &json!(101)), None);
        let e = ValueKind::Enum {
            variants: vec!["a".into()],
        };
        assert_eq!(value_from_json(&e, &json!("b")), None);
        assert_eq!(
            value_json(&Value::Dpi { x: 1, y: 2 }),
            json!({ "x": 1, "y": 2 })
        );
    }

    fn snap(id: &str, name: &str, status: DeviceTrayStatus) -> TrayDeviceSnapshot {
        TrayDeviceSnapshot {
            id: id.into(),
            name: name.into(),
            hidden: false,
            status,
        }
    }

    #[test]
    fn battery_tier_matches_spec() {
        assert_eq!(battery_tier(100), 5);
        assert_eq!(battery_tier(87), 5);
        assert_eq!(battery_tier(81), 5);
        assert_eq!(battery_tier(80), 4);
        assert_eq!(battery_tier(61), 4);
        assert_eq!(battery_tier(60), 3);
        assert_eq!(battery_tier(46), 3);
        assert_eq!(battery_tier(41), 3);
        assert_eq!(battery_tier(40), 2);
        assert_eq!(battery_tier(21), 2);
        assert_eq!(battery_tier(20), 1);
        assert_eq!(battery_tier(1), 1);
        assert_eq!(battery_tier(0), 0);
    }

    #[test]
    fn tray_summary_lowest_prefers_non_charging_and_falls_back_to_charging() {
        let s = Settings::default();
        let mut alerts = BTreeMap::new();
        let list = vec![
            snap(
                "dev_hyperx",
                "HyperX Cloud III Wireless",
                DeviceTrayStatus::Valid {
                    percent: 18,
                    charging: true,
                },
            ),
            snap(
                "dev_razer",
                "Razer DeathAdder V3 Pro",
                DeviceTrayStatus::Valid {
                    percent: 86,
                    charging: false,
                },
            ),
        ];
        let summary = resolve_tray_summary(&list, &s, &mut alerts);
        assert_eq!(summary.style, TrayIconStyle::Battery);
        assert_eq!(summary.state, TrayVisualState::Normal { tier: 5 });
        assert_eq!(summary.target_device_id.as_deref(), Some("dev_razer"));
        assert!(summary.tooltip.contains("Razer DeathAdder V3 Pro"));
        assert!(summary.tooltip.contains("86%"));

        // When all valid devices are charging, pick the lowest charging device.
        let charging_only = vec![
            snap(
                "dev_a",
                "Mouse A",
                DeviceTrayStatus::Valid {
                    percent: 45,
                    charging: true,
                },
            ),
            snap(
                "dev_b",
                "Headset B",
                DeviceTrayStatus::Valid {
                    percent: 15,
                    charging: true,
                },
            ),
        ];
        let summary2 = resolve_tray_summary(&charging_only, &s, &mut alerts);
        assert_eq!(summary2.state, TrayVisualState::Charging { tier: 1 });
        assert_eq!(summary2.target_device_id.as_deref(), Some("dev_b"));
    }

    #[test]
    fn tray_summary_pinned_device_does_not_fallback_when_disconnected() {
        let mut s = Settings::default();
        s.tray.mode = "device".into();
        s.tray.device_id = Some("dev_razer".into());
        let mut alerts = BTreeMap::new();
        let list = vec![
            snap(
                "dev_hyperx",
                "HyperX Cloud III Wireless",
                DeviceTrayStatus::Valid {
                    percent: 47,
                    charging: false,
                },
            ),
            snap(
                "dev_razer",
                "Razer DeathAdder V3 Pro",
                DeviceTrayStatus::Disconnected,
            ),
        ];
        let summary = resolve_tray_summary(&list, &s, &mut alerts);
        assert_eq!(summary.state, TrayVisualState::Disconnected);
        assert_eq!(summary.target_device_id.as_deref(), Some("dev_razer"));
        assert!(summary.tooltip.contains("已断开"));
    }

    #[test]
    fn tray_summary_distinguishes_unknown_and_zero_and_stabilizes_equal_battery() {
        let s = Settings::default();
        let mut alerts = BTreeMap::new();

        // Unknown / stale battery must be Unknown, never CriticalLow/Empty 0%.
        let unk = vec![snap(
            "dev_mouse",
            "Mouse",
            DeviceTrayStatus::Unknown,
        )];
        let summary_unk = resolve_tray_summary(&unk, &s, &mut alerts);
        assert_eq!(summary_unk.state, TrayVisualState::Unknown);
        assert!(summary_unk.tooltip.contains("电量暂不可用"));

        // Equal battery percentages select deterministically without oscillation.
        let equal = vec![
            snap(
                "dev_a",
                "Device A",
                DeviceTrayStatus::Valid {
                    percent: 50,
                    charging: false,
                },
            ),
            snap(
                "dev_b",
                "Device B",
                DeviceTrayStatus::Valid {
                    percent: 50,
                    charging: false,
                },
            ),
        ];
        let sum1 = resolve_tray_summary(&equal, &s, &mut alerts);
        let sum2 = resolve_tray_summary(&equal, &s, &mut alerts);
        assert_eq!(sum1.target_device_id, sum2.target_device_id);
        assert_eq!(sum1.target_device_id.as_deref(), Some("dev_a"));
    }

    #[test]
    fn tray_summary_hysteresis_prevents_boundary_flicker() {
        let s = Settings::default(); // low threshold = 20
        let mut alerts = BTreeMap::new();

        let at_20 = vec![snap(
            "dev_a",
            "Mouse",
            DeviceTrayStatus::Valid {
                percent: 20,
                charging: false,
            },
        )];
        assert_eq!(
            resolve_tray_summary(&at_20, &s, &mut alerts).state,
            TrayVisualState::Low { tier: 1 }
        );

        // Fluctuating up by 1% to 21% stays Low until above threshold + 2 (23%).
        let at_21 = vec![snap(
            "dev_a",
            "Mouse",
            DeviceTrayStatus::Valid {
                percent: 21,
                charging: false,
            },
        )];
        assert_eq!(
            resolve_tray_summary(&at_21, &s, &mut alerts).state,
            TrayVisualState::Low { tier: 2 }
        );

        let at_23 = vec![snap(
            "dev_a",
            "Mouse",
            DeviceTrayStatus::Valid {
                percent: 23,
                charging: false,
            },
        )];
        assert_eq!(
            resolve_tray_summary(&at_23, &s, &mut alerts).state,
            TrayVisualState::Normal { tier: 2 }
        );
    }
}
