//! One thread per endpoint. It owns the driver instance and the endpoint's HID handles, so every
//! operation on the endpoint — polling, input reports, user writes — runs one at a time (CFG-03).

use crate::drivers::LoadedDriver;
use crate::hid::EndpointHid;
use crate::hub::{Command, DeviceId, EndpointKey, Hub, NewEndpoint, WriteReply};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};
use waishe_driver_host::{DriverInstance, HostError, Limits, Runtime};
use waishe_driver_types::cap::{id as cap, link};
use waishe_driver_types::{
    CapabilityDescriptor, EndpointInfo, ProbeError, ProbeResult, ReadError, Unavailable, Value,
    WriteError, WriteOutcome,
};

/// How often a disconnected or asleep device is checked for reconnection. Razer mice do not
/// report waking up, so this bounds how long a woken mouse shows as disconnected.
const RECONNECT_CHECK: Duration = Duration::from_secs(30);
const RECONNECT_CHECK_OBSERVED: Duration = Duration::from_secs(5);
/// Polling interval cap while the Dashboard is open.
const OBSERVED_POLL: Duration = Duration::from_secs(10);
/// Longest time a connected device goes without a link check, so a mouse switched off without
/// notice stops showing live values within a minute (TRAY-07). One cheap query; it does not wake
/// a sleeping Razer mouse (M1 finding).
const LINK_CHECK: Duration = Duration::from_secs(60);
const LISTEN_SLICE_MS: u32 = 200;
/// Retry delay after a timeout or busy answer.
const TRANSIENT_RETRY: Duration = Duration::from_secs(5);
const FAULT_WINDOW: Duration = Duration::from_secs(600);
const FAULT_LIMIT: usize = 3;
const IO_FAILURE_LIMIT: u32 = 5;

/// Why a worker stopped, for the manager.
pub enum Exit {
    /// Shut down on request.
    Stopped,
    /// The driver does not handle this endpoint.
    NotMine,
    /// HID handles failed repeatedly; reopen on the next scan.
    Io,
    /// The driver trapped too often; disable it for this session.
    Faulted(String),
}

pub struct Spec {
    pub key: EndpointKey,
    pub container: String,
    pub driver: Arc<LoadedDriver>,
    pub runtime: Runtime,
    pub endpoint: EndpointInfo,
    pub hid: EndpointHid,
    pub hub: Arc<Hub>,
}

pub fn spawn(
    spec: Spec,
    on_exit: impl FnOnce(EndpointKey, Exit) + Send + 'static,
) -> Sender<Command> {
    let (tx, rx) = channel();
    let tx_for_hub = tx.clone();
    let key = spec.key.clone();
    std::thread::Builder::new()
        .name(format!("endpoint {key}"))
        .spawn(move || {
            let key = spec.key.clone();
            let hub = spec.hub.clone();
            let exit = Worker::run(spec, rx, tx_for_hub);
            hub.endpoint_gone(&key);
            on_exit(key, exit);
        })
        .expect("spawn endpoint thread");
    tx
}

struct Worker {
    spec_key: EndpointKey,
    hub: Arc<Hub>,
    instance: DriverInstance<EndpointHid>,
    device: DeviceId,
    caps: Vec<CapabilityDescriptor>,
    listen: Vec<u32>,
    due: HashMap<String, Instant>,
    /// Capabilities whose last read was a timeout or busy answer; read again soon.
    retry: std::collections::HashSet<String>,
    connected: bool,
}

enum Step {
    Continue,
    Stop(Exit),
    /// Driver trapped; instantiate again.
    Restart(String),
}

impl Worker {
    fn run(spec: Spec, rx: Receiver<Command>, tx: Sender<Command>) -> Exit {
        let Spec {
            key,
            container,
            driver,
            runtime,
            endpoint,
            hid,
            hub,
        } = spec;
        let mut hid = Some(hid);
        let mut faults: Vec<Instant> = Vec::new();
        loop {
            let backend = hid.take().expect("backend present");
            let mut instance = match DriverInstance::instantiate(
                &runtime,
                &driver.module,
                backend,
                Limits::default(),
            ) {
                Ok(i) => i,
                Err(e) => return Exit::Faulted(e.to_string()),
            };
            let probe = match probe(&mut instance, &endpoint, &rx) {
                Ok(Some(p)) => p,
                Ok(None) => return Exit::Stopped,
                Err(Step::Stop(e)) => return e,
                Err(Step::Restart(why)) => {
                    faults.push(Instant::now());
                    faults.retain(|t| t.elapsed() < FAULT_WINDOW);
                    if faults.len() >= FAULT_LIMIT {
                        return Exit::Faulted(why);
                    }
                    tracing::warn!("endpoint {key}: driver fault during probe ({why}); restarting");
                    // A trapped instance cannot be reused; its backend can.
                    hid = Some(instance.into_backend());
                    continue;
                }
                Err(Step::Continue) => unreachable!(),
            };
            let caps = match instance.describe() {
                Ok(c) => c,
                Err(e) => return Exit::Faulted(e.to_string()),
            };
            instance.backend_mut().set_listen(&probe.listen);
            let device = hub.endpoint_ready(NewEndpoint {
                key: &key,
                container: &container,
                driver: &driver,
                probe: &probe,
                caps: caps.clone(),
                vendor_id: endpoint.vendor_id,
                product_id: endpoint.product_id,
                tx: tx.clone(),
            });
            let mut w = Worker {
                spec_key: key.clone(),
                hub: hub.clone(),
                instance,
                device,
                caps,
                listen: probe.listen.clone(),
                due: HashMap::new(),
                connected: true,
                retry: Default::default(),
            };
            let step = w.serve(&rx);
            match step {
                Step::Stop(exit) => return exit,
                Step::Restart(why) => {
                    faults.push(Instant::now());
                    faults.retain(|t| t.elapsed() < FAULT_WINDOW);
                    if faults.len() >= FAULT_LIMIT {
                        return Exit::Faulted(why);
                    }
                    tracing::warn!("endpoint {key}: driver fault ({why}); restarting the instance");
                    hid = Some(w.instance.into_backend());
                }
                Step::Continue => unreachable!(),
            }
        }
    }

    fn serve(&mut self, rx: &Receiver<Command>) -> Step {
        // First reading of everything readable, link state first.
        if let Step::Restart(w) = self.read_all() {
            return Step::Restart(w);
        }
        let mut buf = [0u8; 256];
        loop {
            let now = Instant::now();
            let next = self
                .due
                .values()
                .min()
                .copied()
                .unwrap_or(now + Duration::from_secs(60));
            let wait = next.saturating_duration_since(now);
            let cmd = if self.listen.is_empty() {
                match rx.recv_timeout(wait.min(Duration::from_secs(5))) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Step::Stop(Exit::Stopped),
                }
            } else {
                match rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        // Wait for input reports instead of sleeping.
                        let slice = (wait.as_millis() as u32).clamp(1, LISTEN_SLICE_MS);
                        for h in self.listen.clone() {
                            match waishe_driver_host::HidBackend::read(
                                self.instance.backend_mut(),
                                h,
                                &mut buf,
                                slice,
                            ) {
                                Ok(n) if n > 0 => {}
                                _ => continue,
                            }
                        }
                        if let Step::Restart(w) = self.drain_input() {
                            return Step::Restart(w);
                        }
                        None
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Step::Stop(Exit::Stopped);
                    }
                }
            };
            match cmd {
                Some(Command::Shutdown) => return Step::Stop(Exit::Stopped),
                Some(Command::Refresh) => {
                    if let Step::Restart(w) = self.read_all() {
                        return Step::Restart(w);
                    }
                }
                Some(Command::Write {
                    capability,
                    value,
                    reply,
                }) => {
                    let (r, step) = self.write(&capability, &value);
                    let _ = reply.send(r);
                    if let Step::Restart(w) = step {
                        return Step::Restart(w);
                    }
                }
                None => {}
            }
            if self.instance.backend().io_failures >= IO_FAILURE_LIMIT {
                tracing::warn!(
                    "endpoint {}: HID keeps failing; closing it for a rescan",
                    self.spec_key
                );
                return Step::Stop(Exit::Io);
            }
            if let Step::Restart(w) = self.poll_due() {
                return Step::Restart(w);
            }
        }
    }

    fn interval(&self, c: &CapabilityDescriptor) -> Option<Duration> {
        let mut base = Duration::from_secs(c.poll_interval_s? as u64).max(Duration::from_secs(
            self.hub.settings().poll_interval_seconds as u64,
        ));
        if c.id == cap::LINK_STATE {
            base = base.min(LINK_CHECK);
        }
        Some(if self.hub.has_observers() {
            base.min(OBSERVED_POLL)
        } else {
            base
        })
    }

    fn reconnect_check(&self) -> Duration {
        if self.hub.has_observers() {
            RECONNECT_CHECK_OBSERVED
        } else {
            RECONNECT_CHECK
        }
    }

    fn has(&self, id: &str) -> bool {
        self.caps.iter().any(|c| c.id == id && c.access.read)
    }

    /// Reads one capability, publishes it, replays recorded input. Returns the reading.
    ///
    /// Only an explicit "disconnected" ends the connection. A timeout or busy answer is
    /// transient (the headset, for one, ignores queries while it plays a voice prompt): the
    /// connection state is kept and the capability is read again shortly.
    fn read(&mut self, id: &str) -> Result<Result<Value, ReadError>, String> {
        let r = self.instance.read(id).map_err(|e| e.to_string())?;
        let transient = matches!(&r, Err(ReadError::Unavailable(u)) if *u != Unavailable::Disconnected && *u != Unavailable::Asleep);
        if id == cap::LINK_STATE && !transient {
            self.connected = matches!(&r, Ok(Value::Enum(s)) if s == link::CONNECTED);
        }
        if transient {
            self.retry.insert(id.to_string());
        } else {
            self.retry.remove(id);
        }
        self.hub.update(&self.device, &self.spec_key, id, r.clone());
        if let Step::Restart(w) = self.drain_input() {
            return Err(w);
        }
        Ok(r)
    }

    fn read_all(&mut self) -> Step {
        let ids: Vec<String> = self
            .caps
            .iter()
            .filter(|c| c.access.read)
            .map(|c| c.id.clone())
            .collect();
        if self.has(cap::LINK_STATE) {
            match self.read(cap::LINK_STATE) {
                Err(w) => return Step::Restart(w),
                Ok(_) if !self.connected => {
                    self.mark_disconnected();
                    return Step::Continue;
                }
                Ok(_) => {}
            }
        }
        let now = Instant::now();
        for id in ids.iter().filter(|i| *i != cap::LINK_STATE) {
            if let Err(w) = self.read(id) {
                return Step::Restart(w);
            }
        }
        for c in self.caps.clone() {
            if let Some(i) = self.interval(&c) {
                self.due.insert(c.id.clone(), now + i);
            }
        }
        self.schedule_retries();
        Step::Continue
    }

    /// While the device is away, other capabilities cannot be read: mark them without device
    /// traffic, and check the link at the reconnection interval only.
    fn mark_disconnected(&mut self) {
        for c in self.caps.clone() {
            if c.access.read && c.id != cap::LINK_STATE && c.id != cap::DEVICE_FIRMWARE {
                self.hub.update(
                    &self.device,
                    &self.spec_key,
                    &c.id,
                    Err(ReadError::Unavailable(Unavailable::Disconnected)),
                );
            }
        }
        self.due.clear();
        self.due.insert(
            cap::LINK_STATE.into(),
            Instant::now() + self.reconnect_check(),
        );
    }

    fn poll_due(&mut self) -> Step {
        let now = Instant::now();
        // An observer arrived: bring schedules made without one forward.
        if self.hub.has_observers() {
            let cap_at = now
                + if self.connected {
                    OBSERVED_POLL
                } else {
                    RECONNECT_CHECK_OBSERVED
                };
            for t in self.due.values_mut() {
                if *t > cap_at {
                    *t = cap_at;
                }
            }
        }
        let due: Vec<String> = self
            .due
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(k, _)| k.clone())
            .collect();
        if due.is_empty() {
            return Step::Continue;
        }
        if !self.connected || due.iter().any(|d| d == cap::LINK_STATE) {
            let was = self.connected;
            match self.read(cap::LINK_STATE) {
                Err(w) => return Step::Restart(w),
                Ok(_) if !self.connected => {
                    self.mark_disconnected();
                    return Step::Continue;
                }
                Ok(_) if !was => return self.read_all(),
                Ok(_) => {}
            }
        }
        for id in due.iter().filter(|i| *i != cap::LINK_STATE) {
            if let Err(w) = self.read(id) {
                return Step::Restart(w);
            }
        }
        for c in self.caps.clone() {
            if due.contains(&c.id)
                && let Some(i) = self.interval(&c)
            {
                self.due.insert(c.id.clone(), Instant::now() + i);
            }
        }
        self.schedule_retries();
        Step::Continue
    }

    fn schedule_retries(&mut self) {
        let at = Instant::now() + TRANSIENT_RETRY;
        for id in &self.retry {
            self.due.insert(id.clone(), at);
        }
    }

    /// Passes recorded input reports to the driver and publishes the resulting events.
    fn drain_input(&mut self) -> Step {
        for (h, report) in self.instance.backend_mut().take_recorded() {
            match self.instance.on_input(h, &report) {
                Ok(events) => {
                    for e in events {
                        if e.capability == cap::LINK_STATE {
                            let now_connected =
                                matches!(&e.reading, Ok(Value::Enum(s)) if s == link::CONNECTED);
                            let was = self.connected;
                            self.connected = now_connected;
                            self.hub
                                .update(&self.device, &self.spec_key, &e.capability, e.reading);
                            if !now_connected {
                                self.mark_disconnected();
                            } else if !was {
                                // Came back: read everything once more after the event batch.
                                self.due.insert(cap::LINK_STATE.into(), Instant::now());
                            }
                        } else {
                            self.hub
                                .update(&self.device, &self.spec_key, &e.capability, e.reading);
                        }
                    }
                }
                Err(e) => return Step::Restart(e.to_string()),
            }
        }
        Step::Continue
    }

    fn write(&mut self, id: &str, value: &Value) -> (WriteReply, Step) {
        let reply = |outcome, message: Option<String>| WriteReply { outcome, message };
        let r = match self.instance.write(id, value) {
            Ok(r) => r,
            Err(HostError::Poisoned)
            | Err(HostError::Trap(_))
            | Err(HostError::Decode(_))
            | Err(HostError::Module(_))
            | Err(HostError::AbiMismatch { .. }) => {
                return (
                    reply("failed", Some("driver fault".into())),
                    Step::Restart(format!("trap while writing {id}")),
                );
            }
        };
        let step = self.drain_input();
        let out = match r {
            Ok(WriteOutcome::Applied(v)) => {
                self.hub.update(&self.device, &self.spec_key, id, Ok(v));
                reply("applied", None)
            }
            Ok(WriteOutcome::Accepted) => reply("accepted", None),
            Ok(WriteOutcome::PendingReenumeration) => {
                reply("accepted", Some("设备正在重新连接，稍后回读".into()))
            }
            Err(WriteError::Rejected(m)) => reply("rejected", Some(m)),
            Err(WriteError::Unsupported) => reply("rejected", Some("unsupported".into())),
            Err(WriteError::Unavailable(u)) => {
                let _ = self.read(id);
                reply("failed", Some(format!("{u:?}").to_lowercase()))
            }
            Err(WriteError::Protocol(m)) => {
                let _ = self.read(id);
                reply("failed", Some(m))
            }
        };
        (out, step)
    }
}

/// Probes until the driver decides. `Ok(None)`: shutdown requested while waiting.
fn probe(
    instance: &mut DriverInstance<EndpointHid>,
    endpoint: &EndpointInfo,
    rx: &Receiver<Command>,
) -> Result<Option<ProbeResult>, Step> {
    let mut wait = Duration::from_secs(2);
    loop {
        match instance.probe(endpoint) {
            Ok(Ok(p)) => return Ok(Some(p)),
            Ok(Err(ProbeError::NotMine)) => return Err(Step::Stop(Exit::NotMine)),
            Ok(Err(ProbeError::Retry)) | Ok(Err(ProbeError::Io(_))) => {
                match rx.recv_timeout(wait) {
                    Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => return Ok(None),
                    _ => {}
                }
                wait = (wait * 2).min(Duration::from_secs(60));
            }
            Err(e) => return Err(Step::Restart(e.to_string())),
        }
    }
}
