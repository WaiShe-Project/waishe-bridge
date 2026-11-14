//! Tray icon, custom flyout window, and fallback context menu (TRAY-01..08).
//! Runs the main-thread event loop.

use crate::api::ApiState;
use crate::hub::{Hub, TrayIconStyle, TrayVisualState};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, Rect as TrayRect, TrayIcon, TrayIconBuilder,
    TrayIconEvent,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::windows::{CornerPreference, WindowAttributesExtWindows};
use winit::window::{Window, WindowId, WindowLevel};

const FLYOUT_WIDTH: f64 = 380.0;
const FLYOUT_HEIGHT: f64 = 520.0;
/// Longest the flyout stays hidden waiting for its page's "ready" message. After this it is
/// shown anyway, so a page that failed to load is still visible.
const REVEAL_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub enum UserEvent {
    Changed,
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    Ipc(String),
    PanelDeactivated(isize),
    Show,
}

struct PanelHook {
    prev: isize,
    proxy: EventLoopProxy<UserEvent>,
}

thread_local! {
    static PANEL_HOOKS: RefCell<HashMap<isize, PanelHook>> = RefCell::new(HashMap::new());
}

fn window_hwnd(window: &Window) -> Option<isize> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

unsafe extern "system" fn flyout_wndproc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, GA_ROOTOWNER, GetAncestor, IsChild, WA_INACTIVE,
        WM_ACTIVATE, WM_NCDESTROY, WNDPROC,
    };

    let key = hwnd as isize;
    if msg == WM_ACTIVATE && (wparam & 0xFFFF) == WA_INACTIVE as usize {
        let next = lparam as windows_sys::Win32::Foundation::HWND;
        let stay_open = !next.is_null()
            && unsafe {
                next == hwnd
                    || IsChild(hwnd, next) != 0
                    || GetAncestor(next, GA_ROOTOWNER) == hwnd
            };
        if !stay_open {
            PANEL_HOOKS.with(|m| {
                if let Some(h) = m.borrow().get(&key) {
                    let _ = h.proxy.send_event(UserEvent::PanelDeactivated(key));
                }
            });
        }
    }

    let prev = PANEL_HOOKS.with(|m| m.borrow().get(&key).map(|h| h.prev));
    let res = match prev {
        Some(p) if p != 0 => unsafe {
            let proc_ptr: WNDPROC = std::mem::transmute(p);
            CallWindowProcW(proc_ptr, hwnd, msg, wparam, lparam)
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    };

    if msg == WM_NCDESTROY {
        PANEL_HOOKS.with(|m| {
            m.borrow_mut().remove(&key);
        });
    }
    res
}

fn install_panel_deactivate_hook(window: &Window, proxy: EventLoopProxy<UserEvent>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GWLP_WNDPROC, SetWindowLongPtrW};
    let Some(key) = window_hwnd(window) else {
        return;
    };
    let hwnd = key as windows_sys::Win32::Foundation::HWND;
    let prev = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, flyout_wndproc as *const () as isize) };
    if prev != 0 {
        PANEL_HOOKS.with(|m| {
            m.borrow_mut().insert(key, PanelHook { prev, proxy });
        });
    }
}

pub fn proxy_and_loop() -> (EventLoop<UserEvent>, EventLoopProxy<UserEvent>) {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    (event_loop, proxy)
}

/// Borderless tray flyout (or quick-settings) hosted in WebView2 so the Dashboard's
/// `#/panel` view is the single source of truth. Created on first use and then kept, hidden,
/// while closed: starting WebView2 is what makes opening slow (ADR-0005).
struct Panel {
    window: Window,
    webview: wry::WebView,
    /// The user has the flyout open; the window may still be hidden, waiting for its page.
    open: bool,
    /// While the window is still hidden: when to show it without the page's "ready".
    reveal_by: Option<Instant>,
    opened: Instant,
}

impl Drop for Panel {
    /// The WebView closes at once but winit destroys the window later, from a posted message;
    /// hide it first so the bare (white) window is never seen.
    fn drop(&mut self) {
        self.window.set_visible(false);
    }
}

struct App {
    hub: Arc<Hub>,
    api: Arc<ApiState>,
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    open_id: MenuId,
    panel_id: MenuId,
    quit_id: MenuId,
    shown_icon: Option<IconRenderKey>,
    shown_tooltip: String,
    shown_lang_en: Option<bool>,
    open_at_start: bool,
    panel: Option<Panel>,
    last_tray_rect: Option<(TrayRect, tray_icon::dpi::PhysicalPosition<f64>)>,
    last_blur: Option<Instant>,
    last_dblclick: Option<Instant>,
    web_context: wry::WebContext,
}

impl App {
    fn english(&self) -> bool {
        self.hub.settings().language == "en"
    }

    /// Minimal native context menu reserved for right-click fallback.
    fn menu(&self) -> Menu {
        let en = self.english();
        let menu = Menu::new();
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
            if en { "Settings" } else { "设置" },
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
        let en = self.english();
        let Some(tray) = &self.tray else { return };

        let key = IconRenderKey {
            style: s.style,
            state: s.state,
            light_taskbar: system_uses_light_taskbar(),
            size: system_tray_icon_size(),
        };
        if self.shown_icon != Some(key) {
            let _ = tray.set_icon(Some(render_tray_icon(key)));
            self.shown_icon = Some(key);
        }

        if s.tooltip != self.shown_tooltip {
            let _ = tray.set_tooltip(Some(&s.tooltip));
            self.shown_tooltip = s.tooltip;
        }

        if self.shown_lang_en != Some(en) {
            tray.set_menu(Some(Box::new(self.menu())));
            self.shown_lang_en = Some(en);
        }
    }

    fn open(&self, fragment: &str) {
        let url = self.api.sign_in_url(fragment);
        if let Err(e) = open::that(&url) {
            tracing::warn!("cannot open the browser: {e}");
        }
    }

    /// Computes top-left physical coordinates so the flyout docks neatly near the tray icon
    /// without overflowing the monitor bounds.
    fn flyout_position(&self, el: &ActiveEventLoop) -> Option<PhysicalPosition<i32>> {
        let anchor = self.last_tray_rect.map(|(rect, pos)| {
            if rect.size.width > 0 && rect.size.height > 0 {
                (
                    rect.position.x + rect.size.width as f64 / 2.0,
                    rect.position.y,
                    rect.position.y + rect.size.height as f64,
                )
            } else {
                (pos.x, pos.y - 16.0, pos.y + 16.0)
            }
        });

        let monitor = anchor
            .and_then(|(ax, ay_top, _)| {
                el.available_monitors().find(|m| {
                    let mp = m.position();
                    let ms = m.size();
                    ax >= mp.x as f64
                        && ax < (mp.x + ms.width as i32) as f64
                        && ay_top >= mp.y as f64
                        && ay_top < (mp.y + ms.height as i32) as f64
                })
            })
            .or_else(|| el.primary_monitor())
            .or_else(|| el.available_monitors().next())?;

        let scale = monitor.scale_factor();
        let mp = monitor.position();
        let ms = monitor.size();
        let mon_x = mp.x;
        let mon_y = mp.y;
        let mon_w = ms.width as i32;
        let mon_h = ms.height as i32;

        let win_w = (FLYOUT_WIDTH * scale).round() as i32;
        let win_h = (FLYOUT_HEIGHT * scale).round() as i32;
        let margin = (12.0 * scale).round() as i32;
        let icon_gap = (8.0 * scale).round() as i32;
        let taskbar_pad = (52.0 * scale).round() as i32;

        let (x, y) = if let Some((ax, ay_top, ay_bottom)) = anchor {
            let desired_x = ax.round() as i32 - win_w / 2;
            let clamped_x = desired_x.clamp(mon_x + margin, mon_x + mon_w - win_w - margin);
            let mon_mid_y = mon_y + mon_h / 2;
            let desired_y = if (ay_top as i32) >= mon_mid_y {
                // Taskbar at bottom (standard Windows 11): dock above the icon and clear the taskbar.
                let above_icon = ay_top.round() as i32 - icon_gap - win_h;
                above_icon.min(mon_y + mon_h - taskbar_pad - win_h)
            } else {
                // Taskbar near top or overflow flyout near top: dock below the icon.
                ay_bottom.round() as i32 + icon_gap
            };
            let clamped_y = desired_y.clamp(mon_y + margin, mon_y + mon_h - win_h - margin);
            (clamped_x, clamped_y)
        } else {
            (
                mon_x + mon_w - win_w - margin,
                mon_y + mon_h - taskbar_pad - win_h,
            )
        };

        Some(PhysicalPosition::new(x, y))
    }

    fn panel_open(&self) -> bool {
        self.panel.as_ref().is_some_and(|p| p.open)
    }

    /// Hides the flyout and unloads its page, keeping WebView2 itself. Without a page there is
    /// no event stream, so Bridge stops polling devices at the Dashboard-open rate.
    fn close_panel(&mut self) {
        use wry::{MemoryUsageLevel, WebViewExtWindows};
        let Some(p) = &mut self.panel else { return };
        if !p.open {
            return;
        }
        p.open = false;
        p.reveal_by = None;
        p.window.set_visible(false);
        let _ = p.webview.load_url("about:blank");
        let _ = p.webview.set_memory_usage_level(MemoryUsageLevel::Low);
    }

    fn open_panel(&mut self, el: &ActiveEventLoop, fragment: &str) {
        use wry::{MemoryUsageLevel, WebViewExtWindows};
        let opened = Instant::now();
        let position = self.flyout_position(el);
        let url = self.api.sign_in_url(fragment);
        // Loading the page afresh resets its view state and route (#/panel vs #/panel/settings).
        if let Some(p) = &mut self.panel {
            if let Some(pos) = position {
                p.window.set_outer_position(pos);
            }
            let _ = p.webview.set_memory_usage_level(MemoryUsageLevel::Normal);
            if p.webview.load_url(&url).is_ok() {
                p.open = true;
                p.reveal_by = Some(opened + REVEAL_TIMEOUT);
                p.opened = opened;
                return;
            }
            self.panel = None;
        }
        let title = if self.english() { "WaiShe" } else { "WaiShe" };
        let mut attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(FLYOUT_WIDTH, FLYOUT_HEIGHT))
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_skip_taskbar(true)
            .with_undecorated_shadow(true)
            .with_corner_preference(CornerPreference::Round)
            // A window without its WebView is white: `reveal_panel` shows it once the page is up.
            .with_visible(false);
        if let Some(pos) = position {
            attrs = attrs.with_position(pos);
        }

        let window = match el.create_window(attrs) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("tray panel window unavailable ({e}); using the browser");
                return self.open(fragment);
            }
        };
        install_panel_deactivate_hook(&window, self.proxy.clone());

        let ipc_proxy = self.proxy.clone();
        match wry::WebViewBuilder::new_with_web_context(&mut self.web_context)
            .with_background_color(shell_color())
            .with_ipc_handler(move |req| {
                let _ = ipc_proxy.send_event(UserEvent::Ipc(req.into_body()));
            })
            .with_url(url)
            .build(&window)
        {
            Ok(webview) => {
                self.panel = Some(Panel {
                    window,
                    webview,
                    open: true,
                    reveal_by: Some(Instant::now() + REVEAL_TIMEOUT),
                    opened,
                });
            }
            Err(e) => {
                tracing::warn!("WebView2 unavailable ({e}); using the browser");
                self.open(fragment);
            }
        }
    }

    /// Shows the flyout once: on the page's "ready" message, or at the timeout.
    fn reveal_panel(&mut self, why: &str) {
        let Some(p) = &mut self.panel else { return };
        if p.reveal_by.take().is_none() {
            return;
        }
        p.window.set_visible(true);
        p.window.focus_window();
        tracing::debug!("tray panel shown after {:?} ({why})", p.opened.elapsed());
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if cause != StartCause::Init {
            return;
        }
        let init_key = IconRenderKey {
            style: TrayIconStyle::Battery,
            state: TrayVisualState::Unknown,
            light_taskbar: system_uses_light_taskbar(),
            size: system_tray_icon_size(),
        };
        let tray = TrayIconBuilder::new()
            .with_icon(render_tray_icon(init_key))
            .with_tooltip("WaiShe")
            .with_menu(Box::new(self.menu()))
            .with_menu_on_left_click(false)
            .build();
        match tray {
            Ok(t) => self.tray = Some(t),
            Err(e) => tracing::error!("tray icon unavailable: {e}"),
        }
        self.shown_icon = None;
        self.shown_tooltip.clear();
        self.refresh();
        if self.open_at_start {
            self.open("");
        }
    }

    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, e: WindowEvent) {
        if !self.panel.as_ref().is_some_and(|p| p.window.id() == id) {
            return;
        }
        if matches!(e, WindowEvent::CloseRequested) {
            self.last_blur = Some(Instant::now());
            self.close_panel();
        }
    }

    /// Wakes the loop at the flyout's reveal deadline, if one is pending.
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let mut pending = self.panel.as_ref().and_then(|p| p.reveal_by);
        if pending.is_some_and(|t| t <= Instant::now()) {
            self.reveal_panel("timeout");
            pending = None;
        }
        el.set_control_flow(pending.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn user_event(&mut self, el: &ActiveEventLoop, e: UserEvent) {
        match e {
            UserEvent::Changed => self.refresh(),
            UserEvent::Show => self.open(""),
            UserEvent::PanelDeactivated(hwnd_val) => {
                if let Some(p) = &self.panel
                    && window_hwnd(&p.window) == Some(hwnd_val)
                {
                    self.last_blur = Some(Instant::now());
                    self.close_panel();
                }
            }
            UserEvent::Tray(TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                rect,
                position,
                ..
            }) => {
                self.last_tray_rect = Some((rect, position));
                self.last_dblclick = Some(Instant::now());
                self.close_panel();
                self.open("");
            }
            UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                position,
                ..
            }) => {
                self.last_tray_rect = Some((rect, position));
                self.refresh();
                let suppressed = self
                    .last_blur
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(250))
                    || self
                        .last_dblclick
                        .is_some_and(|t| t.elapsed() < Duration::from_millis(450));
                if self.panel_open() {
                    self.close_panel();
                } else if !suppressed {
                    self.open_panel(el, "#/panel");
                }
            }
            UserEvent::Tray(TrayIconEvent::Enter { .. }) => self.refresh(),
            UserEvent::Tray(_) => {}
            UserEvent::Menu(m) if m.id == self.open_id => self.open(""),
            UserEvent::Menu(m) if m.id == self.panel_id => self.open_panel(el, "#/panel/settings"),
            UserEvent::Menu(m) if m.id == self.quit_id => {
                tracing::info!("quit from tray");
                self.panel = None;
                self.tray = None;
                el.exit();
            }
            UserEvent::Menu(_) => {}
            UserEvent::Ipc(msg) => {
                if msg == "ready" {
                    self.reveal_panel("ready");
                } else if msg == "hide" {
                    self.close_panel();
                } else if msg == "open" {
                    self.close_panel();
                    self.open("");
                } else if let Some(frag) = msg.strip_prefix("open:") {
                    self.close_panel();
                    self.open(frag);
                } else if msg == "quit" {
                    tracing::info!("quit from tray panel");
                    self.panel = None;
                    self.tray = None;
                    el.exit();
                }
            }
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
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_proxy.send_event(UserEvent::Menu(e));
    }));
    let tray_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = tray_proxy.send_event(UserEvent::Tray(e));
    }));
    let mut app = App {
        hub,
        api,
        proxy,
        tray: None,
        open_id: MenuId::new("open"),
        panel_id: MenuId::new("panel"),
        quit_id: MenuId::new("quit"),
        shown_icon: None,
        shown_tooltip: String::new(),
        shown_lang_en: None,
        open_at_start,
        panel: None,
        last_tray_rect: None,
        last_blur: None,
        last_dblclick: None,
        web_context: wry::WebContext::new(Some(webview_data)),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::error!("event loop: {e}");
    }
}

// ---------------------------------------------------------------- icon

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconRenderKey {
    pub style: TrayIconStyle,
    pub state: TrayVisualState,
    pub light_taskbar: bool,
    pub size: u32,
}

#[derive(Debug, Clone, Copy)]
struct Palette {
    normal: [u8; 3],
    charging: [u8; 3],
    low: [u8; 3],
    critical: [u8; 3],
    muted: [u8; 3],
    error: [u8; 3],
}

fn palette(light_taskbar: bool) -> Palette {
    if light_taskbar {
        Palette {
            normal: [0x1E, 0x29, 0x3B],
            charging: [0x05, 0x96, 0x69],
            low: [0xD9, 0x77, 0x06],
            critical: [0xDC, 0x26, 0x26],
            muted: [0x64, 0x74, 0x8B],
            error: [0xDC, 0x26, 0x26],
        }
    } else {
        Palette {
            normal: [0xE5, 0xE7, 0xEB],
            charging: [0x34, 0xD3, 0x99],
            low: [0xF5, 0xB5, 0x44],
            critical: [0xF8, 0x71, 0x71],
            muted: [0x85, 0x8B, 0x95],
            error: [0xF8, 0x71, 0x71],
        }
    }
}

fn state_color(state: TrayVisualState, pal: Palette) -> [u8; 3] {
    match state {
        TrayVisualState::Error => pal.error,
        TrayVisualState::Disconnected | TrayVisualState::Unknown => pal.muted,
        TrayVisualState::CriticalLow { .. } => pal.critical,
        TrayVisualState::Charging { .. } => pal.charging,
        TrayVisualState::Low { .. } => pal.low,
        TrayVisualState::Normal { .. } => pal.normal,
    }
}

fn personalize_value(name: &str) -> Option<u32> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    const SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(SUBKEY)
        .ok()?
        .get_value(name)
        .ok()
}

fn system_uses_light_taskbar() -> bool {
    personalize_value("SystemUsesLightTheme") == Some(1)
}

/// The Dashboard's shell colour (`--shell` in its stylesheet) for the system app theme: what
/// the WebView shows until the page has painted.
fn shell_color() -> (u8, u8, u8, u8) {
    if personalize_value("AppsUseLightTheme") == Some(0) {
        (0x16, 0x18, 0x1B, 0xFF)
    } else {
        (0xEE, 0xEF, 0xF2, 0xFF)
    }
}

fn normalize_icon_size(px: i32) -> u32 {
    match px {
        ..=18 => 16,
        19..=22 => 20,
        23..=28 => 24,
        29..=40 => 32,
        _ => 48,
    }
}

fn system_tray_icon_size() -> u32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};
    let sm = unsafe { GetSystemMetrics(SM_CXSMICON) };
    normalize_icon_size(if sm > 0 { sm } else { 16 })
}

struct Canvas {
    size: usize,
    px: Vec<u8>,
}

impl Canvas {
    fn new(size: usize) -> Self {
        Self {
            size,
            px: vec![0u8; size * size * 4],
        }
    }

    fn set_px(&mut self, x: usize, y: usize, color: [u8; 3], alpha: u8) {
        if x >= self.size || y >= self.size || alpha == 0 {
            return;
        }
        let o = (y * self.size + x) * 4;
        if alpha == 255 || self.px[o + 3] == 0 {
            self.px[o] = color[0];
            self.px[o + 1] = color[1];
            self.px[o + 2] = color[2];
            self.px[o + 3] = alpha;
            return;
        }
        // Standard Porter-Duff src-over blending
        let sa = alpha as f32 / 255.0;
        let da = self.px[o + 3] as f32 / 255.0;
        let out_a = sa + da * (1.0 - sa);
        if out_a <= 0.0 {
            return;
        }
        for c in 0..3 {
            let src = color[c] as f32;
            let dst = self.px[o + c] as f32;
            let blended = (src * sa + dst * da * (1.0 - sa)) / out_a;
            self.px[o + c] = blended.round().clamp(0.0, 255.0) as u8;
        }
        self.px[o + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    fn clear_px(&mut self, x: usize, y: usize) {
        if x >= self.size || y >= self.size {
            return;
        }
        let o = (y * self.size + x) * 4;
        self.px[o..o + 4].fill(0);
    }

    fn fill_rect(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, color: [u8; 3], alpha: u8) {
        let x_end = x1.min(self.size);
        let y_end = y1.min(self.size);
        for y in y0..y_end {
            for x in x0..x_end {
                self.set_px(x, y, color, alpha);
            }
        }
    }

    fn draw_polygon(&mut self, pts: &[(f32, f32)], color: [u8; 3], alpha: u8) {
        if pts.len() < 3 {
            return;
        }
        for y in 0..self.size {
            for x in 0..self.size {
                // 4x4 subpixel supersampling gives smooth edges while keeping horizontal/vertical
                // aligned shapes sharp.
                let mut hits = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let px = x as f32 + (sx as f32 + 0.5) * 0.25;
                        let py = y as f32 + (sy as f32 + 0.5) * 0.25;
                        if point_in_poly(px, py, pts) {
                            hits += 1;
                        }
                    }
                }
                if hits > 0 {
                    let a = ((hits as f32 / 16.0) * alpha as f32).round() as u8;
                    self.set_px(x, y, color, a);
                }
            }
        }
    }

    fn draw_segments(&mut self, segs: &[((f32, f32), (f32, f32))], radius: f32, color: [u8; 3], alpha: u8) {
        for y in 0..self.size {
            for x in 0..self.size {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let mut min_d = f32::MAX;
                for &(a, b) in segs {
                    min_d = min_d.min(dist_to_segment(px, py, a, b));
                }
                let cov = (radius + 0.45 - min_d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let a = (cov * alpha as f32).round() as u8;
                    self.set_px(x, y, color, a);
                }
            }
        }
    }

    fn draw_circle_cutout(&mut self, cx: f32, cy: f32, r: f32) {
        for y in 0..self.size {
            for x in 0..self.size {
                let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
                if d <= r {
                    self.clear_px(x, y);
                }
            }
        }
    }

    fn draw_circle(&mut self, cx: f32, cy: f32, r: f32, color: [u8; 3], alpha: u8) {
        for y in 0..self.size {
            for x in 0..self.size {
                let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
                let cov = (r + 0.45 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.set_px(x, y, color, (cov * alpha as f32).round() as u8);
                }
            }
        }
    }
}

fn point_in_poly(px: f32, py: f32, pts: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let n = pts.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = pts[i];
        let (xj, yj) = pts[j];
        let intersects = ((yi > py) != (yj > py))
            && (px < (xj - xi) * (py - yi) / (yj - yi) + xi);
        if intersects {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn dist_to_segment(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let vx = b.0 - a.0;
    let vy = b.1 - a.1;
    let wx = px - a.0;
    let wy = py - a.1;
    let len_sq = vx * vx + vy * vy;
    let t = if len_sq > 0.0 {
        ((wx * vx + wy * vy) / len_sq).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (px - (a.0 + t * vx)).hypot(py - (a.1 + t * vy))
}

#[derive(Debug, Clone, Copy)]
struct BatteryLayout {
    size: usize,
    body_x0: usize,
    body_x1: usize,
    body_y0: usize,
    body_y1: usize,
    stroke: usize,
    gap: usize,
    cap_x1: usize,
    cap_y0: usize,
    cap_y1: usize,
}

fn battery_layout(size: u32) -> BatteryLayout {
    match normalize_icon_size(size as i32) {
        16 => BatteryLayout {
            size: 16,
            body_x0: 1,
            body_x1: 14,
            body_y0: 3,
            body_y1: 13,
            stroke: 1,
            gap: 1,
            cap_x1: 15,
            cap_y0: 6,
            cap_y1: 10,
        },
        20 => BatteryLayout {
            size: 20,
            body_x0: 1,
            body_x1: 17,
            body_y0: 4,
            body_y1: 16,
            stroke: 1,
            gap: 1,
            cap_x1: 19,
            cap_y0: 7,
            cap_y1: 13,
        },
        24 => BatteryLayout {
            size: 24,
            body_x0: 1,
            body_x1: 21,
            body_y0: 5,
            body_y1: 19,
            stroke: 2,
            gap: 1,
            cap_x1: 23,
            cap_y0: 9,
            cap_y1: 15,
        },
        32 => BatteryLayout {
            size: 32,
            body_x0: 2,
            body_x1: 28,
            body_y0: 7,
            body_y1: 25,
            stroke: 2,
            gap: 1,
            cap_x1: 30,
            cap_y0: 12,
            cap_y1: 20,
        },
        _ => BatteryLayout {
            size: 48,
            body_x0: 3,
            body_x1: 42,
            body_y0: 10,
            body_y1: 38,
            stroke: 3,
            gap: 2,
            cap_x1: 45,
            cap_y0: 18,
            cap_y1: 30,
        },
    }
}

fn draw_battery_outline(c: &mut Canvas, l: BatteryLayout, color: [u8; 3]) {
    let s = l.stroke;
    // Top, bottom, left, right integer borders
    c.fill_rect(l.body_x0, l.body_y0, l.body_x1, l.body_y0 + s, color, 255);
    c.fill_rect(l.body_x0, l.body_y1 - s, l.body_x1, l.body_y1, color, 255);
    c.fill_rect(l.body_x0, l.body_y0, l.body_x0 + s, l.body_y1, color, 255);
    c.fill_rect(l.body_x1 - s, l.body_y0, l.body_x1, l.body_y1, color, 255);

    // Subtle 1px outer corner rounding without reducing interior fill area
    for &(cx, cy) in &[
        (l.body_x0, l.body_y0),
        (l.body_x1 - 1, l.body_y0),
        (l.body_x0, l.body_y1 - 1),
        (l.body_x1 - 1, l.body_y1 - 1),
    ] {
        c.clear_px(cx, cy);
        c.set_px(cx, cy, color, 96);
    }

    // Positive battery cap nub on right
    c.fill_rect(l.body_x1, l.cap_y0, l.cap_x1, l.cap_y1, color, 255);
}

fn draw_battery_icon(c: &mut Canvas, key: IconRenderKey, pal: Palette) {
    let l = battery_layout(key.size);
    let outline_color = state_color(key.state, pal);
    draw_battery_outline(c, l, outline_color);

    let fill_x0 = l.body_x0 + l.stroke + l.gap;
    let fill_x1 = l.body_x1 - l.stroke - l.gap;
    let fill_y0 = l.body_y0 + l.stroke + l.gap;
    let fill_y1 = l.body_y1 - l.stroke - l.gap;
    let max_w = fill_x1.saturating_sub(fill_x0);

    match key.state {
        TrayVisualState::Normal { tier } | TrayVisualState::Low { tier } => {
            let t = tier.min(5) as usize;
            let fill_w = (max_w * t + 2) / 5;
            if fill_w > 0 {
                c.fill_rect(fill_x0, fill_y0, fill_x0 + fill_w, fill_y1, outline_color, 255);
            }
        }
        TrayVisualState::CriticalLow { tier } => {
            // 1/5 bar on left + crisp `!` exclamation on right so both color & form warn.
            let bar_w = if tier > 0 { (max_w / 5).max(2) } else { 0 };
            if bar_w > 0 {
                c.fill_rect(
                    fill_x0,
                    fill_y0,
                    fill_x0 + bar_w.min(max_w / 3),
                    fill_y1,
                    outline_color,
                    255,
                );
            }
            let avail_x0 = fill_x0 + bar_w + (l.size / 12).max(1);
            let avail_cx = (avail_x0 as f32 + fill_x1 as f32) * 0.5;
            let bang_w = ((l.size as f32 / 12.0).round() as usize).max(1);
            let bx0 = (avail_cx - bang_w as f32 * 0.5).round() as usize;
            let bx1 = bx0 + bang_w;
            let h = fill_y1 - fill_y0;
            let dot_h = (h / 5).max(1);
            let stem_y1 = fill_y1 - dot_h * 2;
            c.fill_rect(bx0, fill_y0, bx1, stem_y1.max(fill_y0 + 1), outline_color, 255);
            c.fill_rect(bx0, fill_y1 - dot_h, bx1, fill_y1, outline_color, 255);
        }
        TrayVisualState::Charging { tier } => {
            // Soft translucent level backing + centered high-contrast lightning bolt
            let t = tier.min(5) as usize;
            let fill_w = (max_w * t + 2) / 5;
            if fill_w > 0 {
                c.fill_rect(fill_x0, fill_y0, fill_x0 + fill_w, fill_y1, outline_color, 72);
            }
            let cx = (fill_x0 + fill_x1) as f32 * 0.5;
            let cy = (fill_y0 + fill_y1) as f32 * 0.5;
            let rx = (fill_x1 - fill_x0) as f32 * 0.34;
            let ry = (fill_y1 - fill_y0) as f32 * 0.58;
            let bolt = [
                (cx + 0.16 * rx, cy - ry),
                (cx - 0.85 * rx, cy + 0.08 * ry),
                (cx - 0.05 * rx, cy + 0.08 * ry),
                (cx - 0.22 * rx, cy + ry),
                (cx + 0.85 * rx, cy - 0.08 * ry),
                (cx + 0.05 * rx, cy - 0.08 * ry),
            ];
            let bolt_color = if key.light_taskbar {
                pal.charging
            } else {
                [0xEC, 0xFD, 0xF5]
            };
            c.draw_polygon(&bolt, bolt_color, 255);
        }
        TrayVisualState::Disconnected => {
            // Clear `×` mark inside battery cavity
            let cx = (fill_x0 + fill_x1) as f32 * 0.5;
            let cy = (fill_y0 + fill_y1) as f32 * 0.5;
            let arm = ((fill_y1 - fill_y0) as f32 * 0.34).max(2.0);
            let r = (l.size as f32 * 0.048).max(0.65);
            c.draw_segments(
                &[
                    ((cx - arm, cy - arm), (cx + arm, cy + arm)),
                    ((cx - arm, cy + arm), (cx + arm, cy - arm)),
                ],
                r,
                outline_color,
                255,
            );
        }
        TrayVisualState::Unknown => {
            // Double dashes `- -` inside cavity so Unknown never reads as 0% empty battery
            let h = fill_y1 - fill_y0;
            let dash_h = (h / 3).clamp(2, 4);
            let dy0 = fill_y0 + (h - dash_h) / 2;
            let dash_w = (max_w / 4).max(2);
            let gap_w = (max_w / 5).max(2);
            let total = dash_w * 2 + gap_w;
            let sx0 = fill_x0 + max_w.saturating_sub(total) / 2;
            c.fill_rect(sx0, dy0, sx0 + dash_w, dy0 + dash_h, outline_color, 255);
            c.fill_rect(
                sx0 + dash_w + gap_w,
                dy0,
                sx0 + dash_w * 2 + gap_w,
                dy0 + dash_h,
                outline_color,
                255,
            );
        }
        TrayVisualState::Error => {
            // Bold centered `!` inside alert battery frame (no fill bars)
            let w = ((l.size as f32 / 11.0).round() as usize).max(2);
            let cx = (fill_x0 + fill_x1) / 2;
            let bx0 = cx - w / 2;
            let bx1 = bx0 + w;
            let h = fill_y1 - fill_y0;
            let dot_h = (h / 4).max(1);
            let stem_y1 = fill_y1 - dot_h * 2;
            c.fill_rect(bx0, fill_y0, bx1, stem_y1.max(fill_y0 + 1), outline_color, 255);
            c.fill_rect(bx0, fill_y1 - dot_h, bx1, fill_y1, outline_color, 255);
        }
    }
}

fn draw_brand_icon(c: &mut Canvas, key: IconRenderKey, pal: Palette) {
    let s = c.size as f32;
    let color = state_color(key.state, pal);
    let alpha = match key.state {
        TrayVisualState::Disconnected => 210,
        _ => 255,
    };

    // Abstract geometric `W` hardware mark, engineered for clarity at 16x16..48x48
    let margin_x = s * 0.14;
    let top_y = s * 0.23;
    let bot_y = s * 0.77;
    let peak_y = s * 0.44;
    let lx = margin_x;
    let rx = s - margin_x;
    let v1x = s * 0.34;
    let cx = s * 0.50;
    let v2x = s * 0.66;
    let radius = (s * 0.072).max(1.05);

    c.draw_segments(
        &[
            ((lx, top_y), (v1x, bot_y)),
            ((v1x, bot_y), (cx, peak_y)),
            ((cx, peak_y), (v2x, bot_y)),
            ((v2x, bot_y), (rx, top_y)),
        ],
        radius,
        color,
        alpha,
    );

    // Non-color status reinforcement badge in bottom-right corner
    let dot_r = (s * 0.14).max(2.0);
    let dot_cx = s - dot_r - s * 0.06;
    let dot_cy = s - dot_r - s * 0.06;
    match key.state {
        TrayVisualState::Normal { .. } | TrayVisualState::Unknown => {}
        TrayVisualState::Charging { .. } => {
            c.draw_circle_cutout(dot_cx, dot_cy, dot_r + s * 0.05);
            c.draw_circle(dot_cx, dot_cy, dot_r, pal.charging, 255);
        }
        TrayVisualState::Low { .. } => {
            c.draw_circle_cutout(dot_cx, dot_cy, dot_r + s * 0.05);
            c.draw_circle(dot_cx, dot_cy, dot_r, pal.low, 255);
        }
        TrayVisualState::CriticalLow { .. } | TrayVisualState::Error => {
            c.draw_circle_cutout(dot_cx, dot_cy, dot_r + s * 0.05);
            c.draw_circle(dot_cx, dot_cy, dot_r, pal.error, 255);
        }
        TrayVisualState::Disconnected => {
            let r = (s * 0.055).max(0.75);
            c.draw_circle_cutout(dot_cx, dot_cy, dot_r + s * 0.06);
            c.draw_segments(
                &[
                    (
                        (dot_cx - dot_r * 0.65, dot_cy - dot_r * 0.65),
                        (dot_cx + dot_r * 0.65, dot_cy + dot_r * 0.65),
                    ),
                    (
                        (dot_cx - dot_r * 0.65, dot_cy + dot_r * 0.65),
                        (dot_cx + dot_r * 0.65, dot_cy - dot_r * 0.65),
                    ),
                ],
                r,
                pal.muted,
                255,
            );
        }
    }
}

pub fn render_tray_rgba(key: IconRenderKey) -> Vec<u8> {
    let size = normalize_icon_size(key.size as i32) as usize;
    let mut canvas = Canvas::new(size);
    let pal = palette(key.light_taskbar);
    match key.style {
        TrayIconStyle::Battery => draw_battery_icon(&mut canvas, key, pal),
        TrayIconStyle::Brand => draw_brand_icon(&mut canvas, key, pal),
    }
    canvas.px
}

pub fn render_tray_icon(key: IconRenderKey) -> Icon {
    let size = normalize_icon_size(key.size as i32);
    let rgba = render_tray_rgba(key);
    Icon::from_rgba(rgba, size, size).expect("tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_sizes_themes_styles_and_states_with_transparent_background() {
        let sizes = [16u32, 20, 24, 32, 48];
        let styles = [TrayIconStyle::Battery, TrayIconStyle::Brand];
        let mut states = vec![
            TrayVisualState::Error,
            TrayVisualState::Disconnected,
            TrayVisualState::Unknown,
        ];
        for tier in 0..=5 {
            states.push(TrayVisualState::Normal { tier });
            states.push(TrayVisualState::Low { tier });
            states.push(TrayVisualState::CriticalLow { tier });
            states.push(TrayVisualState::Charging { tier });
        }

        for &size in &sizes {
            for &style in &styles {
                for &light_taskbar in &[false, true] {
                    for &state in &states {
                        let key = IconRenderKey {
                            style,
                            state,
                            light_taskbar,
                            size,
                        };
                        let rgba = render_tray_rgba(key);
                        assert_eq!(rgba.len(), (size * size * 4) as usize);
                        // Outer corner pixels remain transparent (safe margin + no opaque box)
                        let s = size as usize;
                        for &(x, y) in &[(0, 0), (s - 1, 0), (0, s - 1), (s - 1, s - 1)] {
                            assert_eq!(
                                rgba[(y * s + x) * 4 + 3],
                                0,
                                "corner ({x},{y}) not transparent for {key:?}"
                            );
                        }
                        // Foreground contains visible opaque pixels
                        assert!(
                            rgba.chunks_exact(4).any(|p| p[3] >= 200),
                            "empty foreground for {key:?}"
                        );
                        let _icon = render_tray_icon(key);
                    }
                }
            }
        }
    }
}
