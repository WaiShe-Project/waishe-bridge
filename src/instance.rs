//! One Bridge per user session (APP-02). A second start signals the first to show itself.

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent,
    WaitForSingleObject,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

const MUTEX: &str = "Local\\WaiShe.Bridge.Instance";
const SHOW: &str = "Local\\WaiShe.Bridge.Show";

pub struct Instance {
    mutex: HANDLE,
    show: HANDLE,
}

unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.show);
            CloseHandle(self.mutex);
        }
    }
}

/// `None` when another Bridge already runs in this session.
pub fn acquire() -> Option<Instance> {
    unsafe {
        let mutex = CreateMutexW(std::ptr::null(), 1, wide(MUTEX).as_ptr());
        if mutex.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            if !mutex.is_null() {
                CloseHandle(mutex);
            }
            return None;
        }
        let show = CreateEventW(std::ptr::null(), 0, 0, wide(SHOW).as_ptr());
        Some(Instance { mutex, show })
    }
}

/// Asks the running Bridge to open its Dashboard.
pub fn signal_existing() {
    unsafe {
        let e = OpenEventW(EVENT_MODIFY_STATE, 0, wide(SHOW).as_ptr());
        if !e.is_null() {
            SetEvent(e);
            CloseHandle(e);
        }
    }
}

impl Instance {
    /// Calls `f` each time another start signals this instance.
    pub fn on_show(&self, f: impl Fn() + Send + 'static) {
        let show = self.show as usize;
        std::thread::Builder::new()
            .name("show signal".into())
            .spawn(move || {
                loop {
                    if unsafe { WaitForSingleObject(show as HANDLE, INFINITE) } == WAIT_OBJECT_0 {
                        f();
                    } else {
                        return;
                    }
                }
            })
            .expect("spawn show listener");
    }
}
