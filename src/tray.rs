//! Tray icon and menu (TRAY-01..08). Runs the main-thread event loop.

use crate::api::ApiState;
use crate::hub::Hub;
use std::sync::Arc;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

#[derive(Debug, Clone)]
pub enum UserEvent {
    Changed,
    Menu(MenuEvent),
    Show,
}

pub fn proxy_and_loop() -> (EventLoop<UserEvent>, EventLoopProxy<UserEvent>) {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    (event_loop, proxy)
}

/// The settings panel (APP-08): a WebView2 window showing the Dashboard's `#/panel` view, so the
/// settings UI exists once. Closing it destroys the WebView and its processes.
struct Panel {
    window: Window,
    _webview: wry::WebView,
}

struct App {
    hub: Arc<Hub>,
    api: Arc<ApiState>,
    tray: Option<TrayIcon>,
    open_id: MenuId,
    panel_id: MenuId,
    quit_id: MenuId,
    shown: (Option<(u8, bool, bool)>, Vec<String>),
    open_at_start: bool,
    panel: Option<Panel>,
    web_context: wry::WebContext,
}

impl App {
    fn english(&self) -> bool {
        self.hub.settings().language == "en"
    }

    fn menu(&self, lines: &[String]) -> Menu {
        let en = self.english();
        let menu = Menu::new();
        if lines.is_empty() {
            let _ = menu.append(&MenuItem::new(
                if en { "No devices" } else { "没有设备" },
                false,
                None,
            ));
        }
        for l in lines {
            let _ = menu.append(&MenuItem::new(l, false, None));
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(
            self.open_id.clone(),
            if en {
                "Open Dashboard"
            } else {
                "打开 Dashboard"
            },
            true,
            None,
        ));
        let _ = menu.append(&MenuItem::with_id(
            self.panel_id.clone(),
            if en { "Settings…" } else { "设置…" },
            true,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(
            self.quit_id.clone(),
            if en { "Quit WaiShe" } else { "退出 WaiShe" },
            true,
            None,
        ));
        menu
    }

    fn refresh(&mut self) {
        let s = self.hub.tray_summary();
        if (s.icon, &s.lines) == (self.shown.0, &self.shown.1) {
            return;
        }
        let Some(tray) = &self.tray else { return };
        let _ = tray.set_icon(Some(battery_icon(s.icon)));
        let tip = if s.lines.is_empty() {
            "WaiShe".to_string()
        } else {
            format!("WaiShe\n{}", s.lines.join("\n"))
        };
        // The tooltip is limited to 127 characters.
        let _ = tray.set_tooltip(Some(tip.chars().take(127).collect::<String>()));
        tray.set_menu(Some(Box::new(self.menu(&s.lines))));
        self.shown = (s.icon, s.lines);
    }

    fn open(&self, fragment: &str) {
        let url = self.api.sign_in_url(fragment);
        if let Err(e) = open::that(&url) {
            tracing::warn!("cannot open the browser: {e}");
        }
    }

    fn open_panel(&mut self, el: &ActiveEventLoop) {
        if let Some(p) = &self.panel {
            p.window.set_minimized(false);
            p.window.focus_window();
            return;
        }
        let title = if self.english() {
            "WaiShe Settings"
        } else {
            "WaiShe 设置"
        };
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(400.0, 640.0))
            .with_resizable(false);
        let window = match el.create_window(attrs) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("settings window unavailable ({e}); using the browser");
                return self.open("#/panel");
            }
        };
        let url = self.api.sign_in_url("#/panel");
        match wry::WebViewBuilder::new_with_web_context(&mut self.web_context)
            .with_url(url)
            .build(&window)
        {
            Ok(webview) => {
                self.panel = Some(Panel {
                    window,
                    _webview: webview,
                })
            }
            Err(e) => {
                tracing::warn!("WebView2 unavailable ({e}); using the browser");
                self.open("#/panel");
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if cause != StartCause::Init {
            return;
        }
        let tray = TrayIconBuilder::new()
            .with_icon(battery_icon(None))
            .with_tooltip("WaiShe")
            .with_menu(Box::new(self.menu(&[])))
            .with_menu_on_left_click(true)
            .build();
        match tray {
            Ok(t) => self.tray = Some(t),
            Err(e) => tracing::error!("tray icon unavailable: {e}"),
        }
        self.shown = (Some((255, false, false)), vec!["-".into()]);
        self.refresh();
        if self.open_at_start {
            self.open("");
        }
    }

    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, e: WindowEvent) {
        if matches!(e, WindowEvent::CloseRequested)
            && self.panel.as_ref().is_some_and(|p| p.window.id() == id)
        {
            self.panel = None;
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, e: UserEvent) {
        match e {
            UserEvent::Changed => self.refresh(),
            UserEvent::Show => self.open(""),
            UserEvent::Menu(m) if m.id == self.open_id => self.open(""),
            UserEvent::Menu(m) if m.id == self.panel_id => self.open_panel(el),
            UserEvent::Menu(m) if m.id == self.quit_id => {
                tracing::info!("quit from tray");
                self.panel = None;
                self.tray = None;
                el.exit();
            }
            UserEvent::Menu(_) => {}
        }
    }
}

/// Blocks until the user quits.
pub fn run(
    event_loop: EventLoop<UserEvent>,
    proxy: EventLoopProxy<UserEvent>,
    hub: Arc<Hub>,
    api: Arc<ApiState>,
    open_at_start: bool,
    webview_data: std::path::PathBuf,
) {
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));
    let mut app = App {
        hub,
        api,
        tray: None,
        open_id: MenuId::new("open"),
        panel_id: MenuId::new("panel"),
        quit_id: MenuId::new("quit"),
        shown: (None, Vec::new()),
        open_at_start,
        panel: None,
        web_context: wry::WebContext::new(Some(webview_data)),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::error!("event loop: {e}");
    }
}

// ---------------------------------------------------------------- icon

const FONT: [[u8; 5]; 11] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b010, 0b010, 0b010],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
    [0b000, 0b000, 0b111, 0b000, 0b000],
];

/// The battery number in color: green above 50, amber above 20, red below; blue while charging;
/// gray when the value is historical or missing (TRAY-07).
fn battery_icon(value: Option<(u8, bool, bool)>) -> Icon {
    const S: usize = 32;
    let mut px = vec![0u8; S * S * 4];
    let digits: Vec<usize> = match value {
        Some((v, _, _)) if v <= 100 => v.to_string().bytes().map(|b| (b - b'0') as usize).collect(),
        _ => vec![10, 10],
    };
    let color: [u8; 3] = match value {
        Some((_, _, true)) | None => [0x90, 0x90, 0x90],
        Some((_, true, _)) => [0x3A, 0x9A, 0xF0],
        Some((v, _, _)) if v > 50 => [0x3C, 0xC8, 0x50],
        Some((v, _, _)) if v > 20 => [0xF0, 0xB0, 0x20],
        Some(_) => [0xE8, 0x40, 0x40],
    };
    // Widest case "100": 3 digits x 9 px + 2 gaps = 29 px; two digits: 2 x 15 + 1 = 31 px.
    let scale = if digits.len() >= 3 { 3 } else { 5 };
    const GAP: usize = 1;
    let advance = 3 * scale + GAP;
    let w = digits.len() * advance - GAP;
    let (x0, y0) = ((S - w) / 2, (S - 5 * scale) / 2);
    for (i, &d) in digits.iter().enumerate() {
        for (row, bits) in FONT[d].iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let o =
                            ((y0 + row * scale + dy) * S + x0 + i * advance + col * scale + dx) * 4;
                        px[o..o + 4].copy_from_slice(&[color[0], color[1], color[2], 0xFF]);
                    }
                }
            }
        }
    }
    Icon::from_rgba(px, S as u32, S as u32).expect("icon")
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_battery_value_fits_the_icon() {
        for v in 0..=100u8 {
            super::battery_icon(Some((v, false, false)));
        }
        super::battery_icon(None);
    }
}
