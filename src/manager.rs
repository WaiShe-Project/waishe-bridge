//! Matches HID collections to drivers and runs one worker per endpoint. Rescans on device
//! notifications (debounced), on driver changes, and every minute as a fallback.

use crate::drivers::{DriverStore, LoadedDriver};
use crate::hid::{self, Collection, EndpointHid};
use crate::hub::{Command, EndpointKey, Hub};
use crate::worker::{self, Exit, Spec};
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use waishe_driver_types::{EndpointInfo, HidInterface};

const FALLBACK_RESCAN: Duration = Duration::from_secs(60);
const DEBOUNCE: Duration = Duration::from_millis(400);

pub enum Msg {
    Rescan,
    /// Endpoint key, worker generation, driver id, reason.
    WorkerExited(EndpointKey, u64, String, Exit),
    Shutdown,
}

/// A HID collection as listed by `GET /hid-devices`.
#[derive(Clone)]
pub struct Listed {
    pub collection: Collection,
    pub driver: Option<String>,
}

#[derive(Clone)]
pub struct Handle {
    tx: Sender<Msg>,
    pub listed: Arc<Mutex<Vec<Listed>>>,
}

impl Handle {
    pub fn rescan(&self) {
        let _ = self.tx.send(Msg::Rescan);
    }
    pub fn shutdown(&self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

struct Running {
    tx: Sender<Command>,
    driver: String,
    /// Distinguishes a replugged endpoint's new worker from the old one, whose exit message may
    /// arrive late. Also part of the hub endpoint key.
    generation: u64,
}

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn start(
    hub: Arc<Hub>,
    store: Arc<Mutex<DriverStore>>,
) -> (Handle, std::thread::JoinHandle<()>) {
    let (tx, rx) = channel();
    let listed = Arc::new(Mutex::new(Vec::new()));
    let handle = Handle {
        tx: tx.clone(),
        listed: listed.clone(),
    };
    let join = std::thread::Builder::new()
        .name("device manager".into())
        .spawn(move || run(hub, store, tx, rx, listed))
        .expect("spawn manager");
    (handle, join)
}

fn run(
    hub: Arc<Hub>,
    store: Arc<Mutex<DriverStore>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    listed: Arc<Mutex<Vec<Listed>>>,
) {
    let notify_tx = Mutex::new(tx.clone());
    let _notifications = hid::watch(move || {
        let _ = notify_tx.lock().unwrap().send(Msg::Rescan);
    });
    let mut api = match hidapi::HidApi::new() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("HID unavailable: {e}");
            return;
        }
    };
    let mut running: HashMap<EndpointKey, Running> = HashMap::new();
    rescan(&hub, &store, &tx, &mut api, &mut running, &listed);
    loop {
        match rx.recv_timeout(FALLBACK_RESCAN) {
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Msg::Rescan) => {
                // Arrivals come in bursts (one per collection): wait for the burst to end.
                std::thread::sleep(DEBOUNCE);
                let mut stop = false;
                while let Ok(m) = rx.try_recv() {
                    match m {
                        Msg::Shutdown => stop = true,
                        Msg::WorkerExited(k, g, d, e) => {
                            exited(&hub, &store, &mut running, k, g, d, e)
                        }
                        Msg::Rescan => {}
                    }
                }
                if stop {
                    break;
                }
                rescan(&hub, &store, &tx, &mut api, &mut running, &listed);
            }
            Ok(Msg::WorkerExited(key, generation, driver, exit)) => {
                let rescan_after = matches!(exit, Exit::Io | Exit::Faulted(_));
                exited(&hub, &store, &mut running, key, generation, driver, exit);
                if rescan_after {
                    rescan(&hub, &store, &tx, &mut api, &mut running, &listed);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                rescan(&hub, &store, &tx, &mut api, &mut running, &listed)
            }
        }
    }
    for r in running.values() {
        let _ = r.tx.send(Command::Shutdown);
    }
    tracing::info!("device manager stopped");
}

fn exited(
    hub: &Hub,
    store: &Mutex<DriverStore>,
    running: &mut HashMap<EndpointKey, Running>,
    key: EndpointKey,
    generation: u64,
    driver: String,
    exit: Exit,
) {
    if running
        .get(&key)
        .is_some_and(|r| r.generation == generation)
    {
        running.remove(&key);
    }
    match exit {
        Exit::Stopped => {}
        Exit::NotMine => tracing::info!("endpoint {key}: driver {driver} declined it"),
        Exit::Io => tracing::warn!("endpoint {key}: closed after repeated HID errors"),
        Exit::Faulted(why) => {
            store.lock().unwrap().mark_faulted(&driver, why);
            hub.driver_changed(&driver, false);
            // Stop the driver's other endpoints too.
            for (k, other) in running.iter() {
                if other.driver == driver {
                    tracing::warn!("stopping endpoint {k}: its driver is faulted");
                    let _ = other.tx.send(Command::Shutdown);
                }
            }
        }
    }
}

fn rescan(
    hub: &Arc<Hub>,
    store: &Mutex<DriverStore>,
    tx: &Sender<Msg>,
    api: &mut hidapi::HidApi,
    running: &mut HashMap<EndpointKey, Running>,
    listed: &Mutex<Vec<Listed>>,
) {
    let collections = hid::enumerate(api);
    let (claimants, runtime) = {
        let s = store.lock().unwrap();
        (s.claimants(), s.runtime().clone())
    };

    // container -> collections
    let mut by_container: BTreeMap<&str, Vec<&Collection>> = BTreeMap::new();
    for c in &collections {
        by_container.entry(&c.container).or_default().push(c);
    }

    let matches = |d: &LoadedDriver, c: &Collection| {
        d.manifest.matches.iter().any(|m| {
            m.matches(
                c.vendor_id,
                c.product_id,
                c.interface_number,
                c.usage_page,
                c.usage,
            )
        })
    };

    // One driver per container: the first claimant (by precedence) that matches anything.
    let mut desired: BTreeMap<EndpointKey, (Arc<LoadedDriver>, String, Vec<Collection>)> =
        BTreeMap::new();
    let mut listing = Vec::new();
    for (container, cols) in &by_container {
        let claim = claimants
            .iter()
            .find(|d| cols.iter().any(|c| matches(d, c)));
        for c in cols {
            listing.push(Listed {
                collection: (*c).clone(),
                driver: claim.filter(|d| matches(d, c)).map(|d| d.id().to_string()),
            });
        }
        if let Some(d) = claim {
            let mut mine: Vec<Collection> = cols
                .iter()
                .filter(|c| matches(d, c))
                .map(|c| (*c).clone())
                .collect();
            mine.sort_by(|a, b| a.path.cmp(&b.path));
            desired.insert(
                format!("{}@{}", d.id(), container),
                (d.clone(), container.to_string(), mine),
            );
        }
    }
    *listed.lock().unwrap() = listing;

    // Stop endpoints that went away or whose driver is no longer allowed.
    for (key, r) in running.iter() {
        if !desired.contains_key(key) {
            tracing::info!("endpoint {key} gone");
            let _ = r.tx.send(Command::Shutdown);
        }
    }
    running.retain(|k, _| desired.contains_key(k));

    // Start new endpoints.
    for (key, (driver, container, cols)) in desired {
        if running.contains_key(&key) {
            continue;
        }
        let mut devices = Vec::new();
        let mut interfaces = Vec::new();
        for c in &cols {
            match hid::open(api, c) {
                Ok(dev) => {
                    interfaces.push(HidInterface {
                        handle: devices.len() as u32,
                        interface_number: c.interface_number,
                        usage_page: c.usage_page,
                        usage: c.usage,
                    });
                    devices.push(dev);
                }
                Err(e) => tracing::warn!(
                    "endpoint {key}: cannot open collection {:04x}:{:04x}: {e}",
                    c.usage_page,
                    c.usage
                ),
            }
        }
        if devices.is_empty() {
            continue;
        }
        let first = &cols[0];
        let endpoint = EndpointInfo {
            vendor_id: first.vendor_id,
            product_id: first.product_id,
            product: first.product.clone(),
            interfaces,
        };
        tracing::info!(
            "endpoint {key}: starting {} {} on {:04x}:{:04x}",
            driver.id(),
            driver.manifest.driver.version,
            first.vendor_id,
            first.product_id
        );
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let exit_tx = tx.clone();
        let (exit_key, exit_driver) = (key.clone(), driver.id().to_string());
        let worker_tx = worker::spawn(
            Spec {
                key: format!("{key}#{generation}"),
                container,
                driver: driver.clone(),
                runtime: runtime.clone(),
                endpoint,
                hid: EndpointHid::new(devices),
                hub: hub.clone(),
            },
            move |_, e| {
                let _ = exit_tx.send(Msg::WorkerExited(exit_key, generation, exit_driver, e));
            },
        );
        running.insert(
            key,
            Running {
                tx: worker_tx,
                driver: driver.id().to_string(),
                generation,
            },
        );
    }
}
