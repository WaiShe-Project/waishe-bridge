//! Local API 0.1 (`docs/local-api.md`) and the embedded Dashboard, on the loopback interface only.

use crate::drivers::{DriverStore, InstallError, Status, Trust};
use crate::hub::{self, Command, Event, Hub};
use crate::manager;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value as Json_, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use waishe_driver_types::{Constraint, Value};

pub const API_VERSION: &str = "0.1";
const COOKIE: &str = "ws_session";
const FIRST_PORT: u16 = 27463;
const LAST_PORT: u16 = 27472;
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(rust_embed::RustEmbed)]
#[folder = "dashboard-dist/"]
struct Assets;

pub struct ApiState {
    pub port: u16,
    hub: Arc<Hub>,
    store: Arc<Mutex<DriverStore>>,
    manager: manager::Handle,
    codes: Mutex<HashSet<String>>,
    sessions: Mutex<HashSet<String>>,
}

type Shared = Arc<ApiState>;

fn token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("system random source");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl ApiState {
    /// A one-time URL that signs the browser in (ADR-0004). `fragment` selects a Dashboard view.
    pub fn sign_in_url(&self, fragment: &str) -> String {
        let code = token();
        self.codes.lock().unwrap().insert(code.clone());
        format!(
            "http://127.0.0.1:{}/auth?code={code}&next={fragment}",
            self.port
        )
    }
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message.into() } })),
    )
        .into_response()
}

fn session_of(h: &HeaderMap) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            kv.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_owned)
        })
}

/// Host and Origin on every request; session on `/api/`.
async fn guard(State(s): State<Shared>, req: Request, next: Next) -> Response {
    let allowed = [
        format!("127.0.0.1:{}", s.port),
        format!("localhost:{}", s.port),
    ];
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !allowed.iter().any(|a| a == host) {
        return error(StatusCode::FORBIDDEN, "forbidden_host", "host not allowed");
    }
    if let Some(origin) = req.headers().get(header::ORIGIN)
        && !allowed
            .iter()
            .any(|a| origin.to_str().ok() == Some(&format!("http://{a}")))
    {
        return error(
            StatusCode::FORBIDDEN,
            "forbidden_origin",
            "origin not allowed",
        );
    }
    if req.uri().path().starts_with("/api/") {
        let ok = session_of(req.headers()).is_some_and(|t| s.sessions.lock().unwrap().contains(&t));
        if !ok {
            return error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "open the Dashboard from the tray",
            );
        }
    }
    next.run(req).await
}

#[derive(serde::Deserialize)]
struct AuthQuery {
    code: String,
    #[serde(default)]
    next: String,
}

async fn auth(State(s): State<Shared>, Query(q): Query<AuthQuery>) -> Response {
    if !s.codes.lock().unwrap().remove(&q.code) {
        return error(
            StatusCode::UNAUTHORIZED,
            "invalid_code",
            "the sign-in link was already used",
        );
    }
    let t = token();
    s.sessions.lock().unwrap().insert(t.clone());
    // Only same-document fragments, never another location.
    let target = if q.next.starts_with('#') && !q.next.contains("//") {
        format!("/{}", q.next)
    } else {
        "/".into()
    };
    let mut r = Redirect::to(&target).into_response();
    let cookie = format!("{COOKIE}={t}; HttpOnly; SameSite=Strict; Path=/");
    r.headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    r
}

async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path).or_else(|| Assets::get("index.html")) {
        Some(f) => {
            let mime = mime_guess::from_path(path).first_or(mime_guess::mime::TEXT_HTML);
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::CACHE_CONTROL, "no-cache".into()),
                ],
                f.data,
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------- status

fn vendor_software() -> Vec<&'static str> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    const KNOWN: &[(&str, &str)] = &[
        ("razerappengine.exe", "Razer Synapse"),
        ("ngenuity.exe", "HyperX NGENUITY"),
    ];
    let mut found = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return found;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let len = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
            if let Some((_, label)) = KNOWN.iter().find(|(exe, _)| *exe == name)
                && !found.contains(label)
            {
                found.push(*label);
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    found
}

async fn status(State(s): State<Shared>) -> Json<Json_> {
    let vendors = tokio::task::spawn_blocking(vendor_software)
        .await
        .unwrap_or_default();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "apiVersion": API_VERSION,
        "driverAbi": format!("{}.{}", waishe_driver_types::abi::VERSION_MAJOR, waishe_driver_types::abi::VERSION_MINOR),
        "platform": "windows-x64",
        "port": s.port,
        "vendorSoftware": vendors,
    }))
}

// ---------------------------------------------------------------- devices

async fn devices(State(s): State<Shared>) -> Json<Json_> {
    Json(s.hub.devices_json())
}

async fn device(State(s): State<Shared>, Path(id): Path<String>) -> Response {
    match s.hub.device_json(&id) {
        Some(d) => Json(d).into_response(),
        None => error(StatusCode::NOT_FOUND, "device_not_found", id),
    }
}

async fn patch_device(
    State(s): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<Json_>,
) -> Response {
    match s.hub.set_override(&id, &body) {
        Ok(()) => Json(s.hub.device_json(&id).unwrap_or(Json_::Null)).into_response(),
        Err(e) if e == "device_not_found" => error(StatusCode::NOT_FOUND, "device_not_found", id),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "io", e),
    }
}

async fn refresh(State(s): State<Shared>, Path(id): Path<String>) -> Response {
    match s.hub.command_target(&id) {
        Some(tx) => {
            let _ = tx.send(Command::Refresh);
            StatusCode::ACCEPTED.into_response()
        }
        None => error(
            StatusCode::BAD_GATEWAY,
            "device_unavailable",
            "device is not connected",
        ),
    }
}

fn violates(c: &Constraint, v: &Value) -> Option<String> {
    match (c, v) {
        (Constraint::Range { min, max, step }, Value::Integer(i)) => {
            (i < min || i > max || (*step > 0 && (i - min) % step != 0))
                .then(|| format!("must be {min}..{max} in steps of {step}"))
        }
        (Constraint::Choices(list), Value::Integer(i)) => {
            (!list.contains(i)).then(|| format!("must be one of {list:?}"))
        }
        (
            Constraint::Dpi {
                min,
                max,
                independent_xy,
                ..
            },
            Value::Dpi { x, y },
        ) => {
            if !(min..=max).contains(&x) || !(min..=max).contains(&y) {
                Some(format!("DPI must be {min}..{max}"))
            } else if !independent_xy && x != y {
                Some("X and Y must be equal".into())
            } else {
                None
            }
        }
        _ => None,
    }
}

async fn write_capability(
    State(s): State<Shared>,
    Path((id, cap)): Path<(String, String)>,
    Json(body): Json<Json_>,
) -> Response {
    if s.hub.device_json(&id).is_none() {
        return error(StatusCode::NOT_FOUND, "device_not_found", id);
    }
    let Some(desc) = s.hub.descriptor(&id, &cap) else {
        return error(StatusCode::NOT_FOUND, "capability_not_found", cap);
    };
    if !desc.access.write {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "capability_not_writable",
            cap,
        );
    }
    let Some(value) = body
        .get("value")
        .and_then(|v| hub::value_from_json(&desc.kind, v))
    else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
            "value does not match the capability type",
        );
    };
    let reply_json = |status: StatusCode, outcome: &str, message: Option<String>| {
        (status, Json(json!({ "outcome": outcome, "reading": s.hub.reading_json(&id, &cap), "message": message }))).into_response()
    };
    if let Some(m) = desc.constraint.as_ref().and_then(|c| violates(c, &value)) {
        return reply_json(StatusCode::UNPROCESSABLE_ENTITY, "rejected", Some(m));
    }
    let Some(tx) = s.hub.command_target(&id) else {
        return reply_json(
            StatusCode::BAD_GATEWAY,
            "failed",
            Some("device is not connected".into()),
        );
    };
    let (reply, rx) = tokio::sync::oneshot::channel();
    if tx
        .send(Command::Write {
            capability: cap.clone(),
            value,
            reply,
        })
        .is_err()
    {
        return reply_json(
            StatusCode::BAD_GATEWAY,
            "failed",
            Some("device is not connected".into()),
        );
    }
    match tokio::time::timeout(WRITE_TIMEOUT, rx).await {
        Ok(Ok(r)) => {
            let status = match r.outcome {
                "applied" | "accepted" => StatusCode::OK,
                "rejected" => StatusCode::UNPROCESSABLE_ENTITY,
                _ => StatusCode::BAD_GATEWAY,
            };
            reply_json(status, r.outcome, r.message)
        }
        _ => reply_json(StatusCode::BAD_GATEWAY, "failed", Some("timed out".into())),
    }
}

async fn hid_devices(State(s): State<Shared>) -> Json<Json_> {
    let listed = s.manager.listed.lock().unwrap().clone();
    Json(Json_::Array(
        listed
            .iter()
            .map(|l| {
                let c = &l.collection;
                json!({
                    "vendorId": c.vendor_id,
                    "productId": c.product_id,
                    "product": c.product,
                    "manufacturer": c.manufacturer,
                    "usagePage": c.usage_page,
                    "usage": c.usage,
                    "interfaceNumber": c.interface_number,
                    "driver": l.driver,
                })
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------- drivers

fn driver_json(store: &DriverStore, id: &str) -> Option<Json_> {
    let i = store.drivers.get(id)?;
    let d = &i.driver.manifest.driver;
    let (status, fault) = match &i.status {
        Status::Active => ("active", None),
        Status::Disabled => ("disabled", None),
        Status::Faulted(why) => ("faulted", Some(why.clone())),
    };
    Some(json!({
        "id": d.id,
        "name": d.name,
        "version": d.version,
        "api": d.api,
        "license": d.license,
        "repository": d.repository,
        "description": d.description,
        "trust": match i.driver.trust { Trust::Official => "official", Trust::Community => "community", Trust::Local => "local" },
        "enabled": i.enabled,
        "status": status,
        "fault": fault,
        "models": i.driver.devices.devices.iter().map(|m| json!({ "model": m.model, "name": m.name })).collect::<Vec<_>>(),
    }))
}

async fn drivers(State(s): State<Shared>) -> Json<Json_> {
    let store = s.store.lock().unwrap();
    Json(Json_::Array(
        store
            .drivers
            .keys()
            .filter_map(|id| driver_json(&store, id))
            .collect(),
    ))
}

fn install_error(e: InstallError) -> Response {
    match e {
        InstallError::Incompatible(m) => {
            error(StatusCode::UNPROCESSABLE_ENTITY, "driver_incompatible", m)
        }
        InstallError::Invalid(m) => error(StatusCode::UNPROCESSABLE_ENTITY, "driver_invalid", m),
        InstallError::Io(m) => error(StatusCode::INTERNAL_SERVER_ERROR, "io", m),
    }
}

async fn install_driver(State(s): State<Shared>, body: Bytes) -> Response {
    if !s.hub.settings().developer_mode {
        return error(
            StatusCode::FORBIDDEN,
            "developer_mode_required",
            "local drivers need developer mode",
        );
    }
    let st = s.clone();
    let result = tokio::task::spawn_blocking(move || {
        st.store
            .lock()
            .unwrap()
            .install_package(&body, Trust::Local)
    })
    .await;
    match result {
        Ok(Ok(id)) => {
            s.hub.driver_changed(&id, false);
            s.manager.rescan();
            let j = driver_json(&s.store.lock().unwrap(), &id);
            (StatusCode::CREATED, Json(j)).into_response()
        }
        Ok(Err(e)) => install_error(e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "io", e.to_string()),
    }
}

async fn patch_driver(
    State(s): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<Json_>,
) -> Response {
    let Some(enabled) = body.get("enabled").and_then(Json_::as_bool) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
            "expected { enabled: bool }",
        );
    };
    let r = s.store.lock().unwrap().set_enabled(&id, enabled);
    match r {
        Ok(()) => {
            s.hub.driver_changed(&id, false);
            s.manager.rescan();
            Json(driver_json(&s.store.lock().unwrap(), &id)).into_response()
        }
        Err(_) => error(StatusCode::NOT_FOUND, "driver_not_found", id),
    }
}

async fn delete_driver(State(s): State<Shared>, Path(id): Path<String>) -> Response {
    let r = s.store.lock().unwrap().uninstall(&id);
    match r {
        Ok(()) => {
            s.manager.rescan();
            s.hub.forget_driver_devices(&id);
            s.hub.driver_changed(&id, true);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(_) => error(StatusCode::NOT_FOUND, "driver_not_found", id),
    }
}

// ---------------------------------------------------------------- settings

async fn settings(State(s): State<Shared>) -> Json<Json_> {
    Json(serde_json::to_value(s.hub.settings()).unwrap())
}

async fn patch_settings(State(s): State<Shared>, Json(body): Json<Json_>) -> Response {
    let before = s.hub.settings();
    match s.hub.update_settings(&body) {
        Ok(next) => {
            if next.autostart != before.autostart {
                crate::autostart::apply(next.autostart);
            }
            Json(serde_json::to_value(next).unwrap()).into_response()
        }
        Err(e) => error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings", e),
    }
}

// ---------------------------------------------------------------- events

async fn events(State(s): State<Shared>) -> Response {
    let guard = s.hub.observer_guard();
    let st = s.clone();
    let stream = BroadcastStream::new(s.hub.subscribe())
        // A lagging client ends its stream; it reconnects and re-fetches the baseline.
        .map_while(|e| e.ok())
        .filter_map(move |e| {
            let _keep = &guard;
            let (name, data) = match e {
                Event::DeviceAdded(id) => ("device.added", st.hub.device_json(&id)?),
                Event::DeviceUpdated(id) => match st.hub.device_json(&id) {
                    Some(d) => ("device.updated", d),
                    None => ("device.removed", json!({ "id": id })),
                },
                Event::DeviceRemoved(id) => ("device.removed", json!({ "id": id })),
                Event::Capability(id, cap) => (
                    "capability.changed",
                    json!({ "deviceId": id, "capability": st.hub.capability_json(&id, &cap)? }),
                ),
                Event::Driver(id) => (
                    "driver.changed",
                    driver_json(&st.store.lock().unwrap(), &id)?,
                ),
                Event::DriverRemoved(id) => {
                    ("driver.changed", json!({ "id": id, "removed": true }))
                }
                Event::Settings => (
                    "settings.changed",
                    serde_json::to_value(st.hub.settings()).ok()?,
                ),
            };
            Some(Ok::<_, std::convert::Infallible>(
                SseEvent::default().event(name).data(data.to_string()),
            ))
        });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

// ---------------------------------------------------------------- server

/// Binds the first free port and serves until the runtime stops. Returns the shared state.
pub async fn serve(
    hub: Arc<Hub>,
    store: Arc<Mutex<DriverStore>>,
    manager: manager::Handle,
) -> std::io::Result<Shared> {
    let mut port = FIRST_PORT;
    let listener = loop {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => break l,
            Err(_) if port < LAST_PORT => port += 1,
            Err(e) => return Err(e),
        }
    };
    let state = Arc::new(ApiState {
        port,
        hub,
        store,
        manager,
        codes: Mutex::new(HashSet::new()),
        sessions: Mutex::new(HashSet::new()),
    });
    let api = Router::new()
        .route("/status", get(status))
        .route("/devices", get(devices))
        .route("/devices/{id}", get(device).patch(patch_device))
        .route("/devices/{id}/refresh", post(refresh))
        .route("/devices/{id}/capabilities/{cap}", put(write_capability))
        .route("/hid-devices", get(hid_devices))
        .route("/drivers", get(drivers).post(install_driver))
        .route(
            "/drivers/{id}",
            axum::routing::patch(patch_driver).delete(delete_driver),
        )
        .route("/settings", get(settings).patch(patch_settings))
        .route("/events", get(events))
        .layer(DefaultBodyLimit::max(16 << 20));
    let app = Router::new()
        .nest("/api/v1", api)
        .route("/auth", get(auth))
        .fallback(get(asset))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state.clone());
    tracing::info!("local API on http://127.0.0.1:{port}");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("local API stopped: {e}");
        }
    });
    Ok(state)
}
