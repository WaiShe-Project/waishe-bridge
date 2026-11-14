//! HID access: enumeration with container ids, device-change notifications, and the backend
//! handed to driver instances.

use hidapi::{DeviceInfo, HidApi, HidDevice};
use std::collections::HashSet;
use std::ffi::CString;
use waishe_driver_host::HidBackend;
use waishe_driver_types::abi::HidStatus;

/// One HID top-level collection.
#[derive(Debug, Clone)]
pub struct Collection {
    pub path: CString,
    pub vendor_id: u16,
    pub product_id: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub interface_number: i32,
    pub product: String,
    pub manufacturer: String,
    /// Windows container id: every interface of one USB device shares it (verified in M0 S6).
    pub container: String,
}

impl Collection {
    fn from_info(d: &DeviceInfo) -> Self {
        let path = d.path().to_owned();
        let container = container_id(&path).unwrap_or_else(|| {
            // Fallback keeps the collections of one product together.
            format!(
                "{:04x}:{:04x}:{}",
                d.vendor_id(),
                d.product_id(),
                d.serial_number().unwrap_or("")
            )
        });
        Collection {
            path,
            vendor_id: d.vendor_id(),
            product_id: d.product_id(),
            usage_page: d.usage_page(),
            usage: d.usage(),
            interface_number: d.interface_number(),
            product: d.product_string().unwrap_or_default().to_string(),
            manufacturer: d.manufacturer_string().unwrap_or_default().to_string(),
            container,
        }
    }
}

pub fn enumerate(api: &mut HidApi) -> Vec<Collection> {
    if let Err(e) = api.refresh_devices() {
        tracing::warn!("HID enumeration failed: {e}");
    }
    api.device_list().map(Collection::from_info).collect()
}

/// Looks up the container id of a HID device interface path.
fn container_id(path: &CString) -> Option<String> {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_PropertyW, CM_Get_Device_Interface_PropertyW, CM_LOCATE_DEVNODE_NORMAL,
        CM_Locate_DevNodeW, CR_SUCCESS,
    };
    use windows_sys::Win32::Devices::Properties::{
        DEVPKEY_Device_ContainerId, DEVPKEY_Device_InstanceId,
    };
    use windows_sys::core::GUID;

    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut kind = 0u32;
    let mut instance = [0u16; 512];
    let mut size = (instance.len() * 2) as u32;
    unsafe {
        if CM_Get_Device_Interface_PropertyW(
            wide.as_ptr(),
            &DEVPKEY_Device_InstanceId,
            &mut kind,
            instance.as_mut_ptr().cast(),
            &mut size,
            0,
        ) != CR_SUCCESS
        {
            return None;
        }
        let mut devinst = 0u32;
        if CM_Locate_DevNodeW(&mut devinst, instance.as_ptr(), CM_LOCATE_DEVNODE_NORMAL)
            != CR_SUCCESS
        {
            return None;
        }
        let mut guid = GUID::from_u128(0);
        let mut size = std::mem::size_of::<GUID>() as u32;
        if CM_Get_DevNode_PropertyW(
            devinst,
            &DEVPKEY_Device_ContainerId,
            &mut kind,
            (&raw mut guid).cast(),
            &mut size,
            0,
        ) != CR_SUCCESS
        {
            return None;
        }
        Some(format!(
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{}",
            guid.data1,
            guid.data2,
            guid.data3,
            guid.data4[0],
            guid.data4[1],
            guid.data4[2..]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }
}

/// Opens a collection. hidapi falls back to zero access when the system owns the collection
/// (mouse/keyboard), which is what the Razer control channel needs (REL-05, verified in M0).
pub fn open(api: &HidApi, c: &Collection) -> Result<HidDevice, String> {
    api.open_path(&c.path).map_err(|e| e.to_string())
}

type ChangeFn = Box<dyn Fn() + Send + Sync>;

pub struct Notifications(
    windows_sys::Win32::Devices::DeviceAndDriverInstallation::HCMNOTIFICATION,
    *mut ChangeFn,
);

unsafe impl Send for Notifications {}

impl Drop for Notifications {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Devices::DeviceAndDriverInstallation::CM_Unregister_Notification(
                self.0,
            );
            drop(Box::from_raw(self.1));
        }
    }
}

/// Calls `on_change` whenever a HID interface arrives or leaves. The registration lives as long
/// as the returned guard.
pub fn watch(on_change: impl Fn() + Send + Sync + 'static) -> Option<Notifications> {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
    use windows_sys::Win32::Devices::HumanInterfaceDevice::GUID_DEVINTERFACE_HID;

    unsafe extern "system" fn callback(
        _: HCMNOTIFICATION,
        ctx: *const core::ffi::c_void,
        _: CM_NOTIFY_ACTION,
        _: *const CM_NOTIFY_EVENT_DATA,
        _: u32,
    ) -> u32 {
        let f = unsafe { &*(ctx as *const ChangeFn) };
        f();
        0
    }

    let ctx: *mut ChangeFn = Box::into_raw(Box::new(Box::new(on_change)));
    let mut filter: CM_NOTIFY_FILTER = unsafe { std::mem::zeroed() };
    filter.cbSize = std::mem::size_of::<CM_NOTIFY_FILTER>() as u32;
    filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
    filter.u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_HID;
    let mut handle: HCMNOTIFICATION = std::ptr::null_mut();
    let r = unsafe { CM_Register_Notification(&filter, ctx.cast(), Some(callback), &mut handle) };
    if r != CR_SUCCESS {
        tracing::error!(
            "device notifications unavailable (CM_Register_Notification = {r}); relying on periodic rescans"
        );
        unsafe { drop(Box::from_raw(ctx)) };
        return None;
    }
    Some(Notifications(handle, ctx))
}

/// The HID backend of one driver instance: only the collections of its endpoint, by handle.
pub struct EndpointHid {
    devices: Vec<HidDevice>,
    listen: HashSet<u32>,
    /// Input reports read during a driver call on listened handles, replayed to `on_input` after
    /// the call so unsolicited reports are never lost (SDK driver interface 5.3).
    recorded: Vec<(u32, Vec<u8>)>,
    /// Consecutive I/O failures; the endpoint is reopened after a few (stale handles after sleep).
    pub io_failures: u32,
}

impl EndpointHid {
    pub fn new(devices: Vec<HidDevice>) -> Self {
        EndpointHid {
            devices,
            listen: HashSet::new(),
            recorded: Vec::new(),
            io_failures: 0,
        }
    }

    pub fn set_listen(&mut self, handles: &[u32]) {
        self.listen = handles.iter().copied().collect();
    }

    pub fn take_recorded(&mut self) -> Vec<(u32, Vec<u8>)> {
        std::mem::take(&mut self.recorded)
    }

    fn dev(&self, h: u32) -> Result<&HidDevice, HidStatus> {
        self.devices.get(h as usize).ok_or(HidStatus::NoHandle)
    }

    fn io<T>(&mut self, r: hidapi::HidResult<T>) -> Result<T, HidStatus> {
        match r {
            Ok(v) => {
                self.io_failures = 0;
                Ok(v)
            }
            Err(e) => {
                self.io_failures += 1;
                tracing::debug!("HID error: {e}");
                Err(HidStatus::Io)
            }
        }
    }
}

impl HidBackend for EndpointHid {
    fn send_feature(&mut self, h: u32, data: &[u8]) -> Result<(), HidStatus> {
        let r = self.dev(h)?.send_feature_report(data);
        self.io(r)
    }

    fn get_feature(&mut self, h: u32, buf: &mut [u8]) -> Result<usize, HidStatus> {
        let r = self.dev(h)?.get_feature_report(buf);
        self.io(r)
    }

    fn write(&mut self, h: u32, data: &[u8]) -> Result<usize, HidStatus> {
        let r = self.dev(h)?.write(data);
        self.io(r)
    }

    fn read(&mut self, h: u32, buf: &mut [u8], timeout_ms: u32) -> Result<usize, HidStatus> {
        let r = self.dev(h)?.read_timeout(buf, timeout_ms as i32);
        let n = self.io(r)?;
        if n > 0 && self.listen.contains(&h) {
            self.recorded.push((h, buf[..n].to_vec()));
        }
        Ok(n)
    }
}
