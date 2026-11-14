//! WaiShe Bridge.
//!
//! ```text
//! waishe-bridge                       start and open the Dashboard
//! waishe-bridge --background          start in the tray only (autostart)
//! waishe-bridge --install-driver F    install a local .wsdrv package and exit
//! ```
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod autostart;
mod drivers;
mod hid;
mod hub;
mod instance;
mod manager;
mod notify;
mod paths;
mod settings;
mod tray;
mod worker;

use std::sync::{Arc, Mutex};

fn init_logging(paths: &paths::Paths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("WAISHE_LOG").unwrap_or_else(|_| "info".into());
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("bridge")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&paths.logs)
        .ok();
    let (layer, guard) = match file {
        Some(f) => {
            let (w, g) = tracing_appender::non_blocking(f);
            (
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(w),
                ),
                Some(g),
            )
        }
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
    guard
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let background = args.iter().any(|a| a == "--background");
    let install = args
        .iter()
        .position(|a| a == "--install-driver")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let paths = paths::Paths::resolve();
    let _log = init_logging(&paths);

    if let Some(file) = install {
        let bytes = std::fs::read(&file).unwrap_or_else(|e| {
            eprintln!("{file}: {e}");
            std::process::exit(1)
        });
        match drivers::DriverStore::open(paths.drivers.clone())
            .install_package(&bytes, drivers::Trust::Local)
        {
            Ok(id) => println!("installed {id}"),
            Err(e) => {
                eprintln!("install failed: {e}");
                std::process::exit(1)
            }
        }
        instance::signal_existing();
        return;
    }

    let Some(instance) = instance::acquire() else {
        tracing::info!("Bridge already running; asking it to open the Dashboard");
        instance::signal_existing();
        return;
    };
    tracing::info!("WaiShe Bridge {} starting", env!("CARGO_PKG_VERSION"));

    let hub = hub::Hub::new(paths.settings.clone(), paths.devices.clone());
    autostart::apply(hub.settings().autostart);
    {
        let h = Arc::downgrade(&hub);
        hub.set_on_low_battery(move |name, p| {
            let en = h.upgrade().is_some_and(|h| h.settings().language == "en");
            notify::low_battery(name, p, en);
        });
    }
    let store = Arc::new(Mutex::new(drivers::DriverStore::open(
        paths.drivers.clone(),
    )));
    let (manager, manager_thread) = manager::start(hub.clone(), store.clone());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let api = match runtime.block_on(api::serve(hub.clone(), store.clone(), manager.clone())) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("cannot start the local API: {e}");
            notify::toast("WaiShe", &format!("本地接口无法启动：{e}"));
            return;
        }
    };

    // Development builds only: lets scripts sign in without a browser.
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os("WAISHE_DEV_SIGNIN") {
        let _ = std::fs::write(p, api.sign_in_url(""));
    }

    let (event_loop, proxy) = tray::proxy_and_loop();
    {
        let p = proxy.clone();
        hub.set_on_change(move || {
            let _ = p.send_event(tray::UserEvent::Changed);
        });
        let p = proxy.clone();
        instance.on_show(move || {
            let _ = p.send_event(tray::UserEvent::Show);
        });
    }
    tray::run(
        event_loop,
        proxy,
        hub,
        api,
        !background,
        paths.webview.clone(),
    );

    manager.shutdown();
    let _ = manager_thread.join();
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    tracing::info!("WaiShe Bridge stopped");
    drop(instance);
}
