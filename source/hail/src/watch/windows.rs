use super::ChangeSource;
use anyhow::{bail, Result};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Register_Notification, CM_Unregister_Notification, CM_NOTIFY_ACTION,
    CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL,
    CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_2,
    CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CR_SUCCESS, HCMNOTIFICATION,
};
use windows_sys::Win32::System::Ioctl::GUID_DEVINTERFACE_VOLUME;

/// Subscribes to Configuration Manager volume device-interface arrival/removal.
///
/// Uses `CM_Register_Notification`, which needs no window or message loop and also
/// works when hail runs as a Windows service (session 0).
pub struct OsEventSource {
    handle: HCMNOTIFICATION,
    sender: *mut Sender<()>,
    receiver: Receiver<()>,
}

unsafe extern "system" fn notify(
    _handle: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    _data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL
        || action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
    {
        // SAFETY: `context` is the boxed Sender, kept alive until after unregistration.
        let sender = unsafe { &*(context as *const Sender<()>) };
        let _ = sender.send(());
    }
    0
}

impl OsEventSource {
    pub fn new() -> Result<Self> {
        let (sender, receiver) = channel();
        let sender = Box::into_raw(Box::new(sender));
        let filter = CM_NOTIFY_FILTER {
            cbSize: size_of::<CM_NOTIFY_FILTER>() as u32,
            Flags: 0,
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
            Reserved: 0,
            u: CM_NOTIFY_FILTER_0 {
                DeviceInterface: CM_NOTIFY_FILTER_0_2 {
                    ClassGuid: GUID_DEVINTERFACE_VOLUME,
                },
            },
        };
        let mut handle: HCMNOTIFICATION = null_mut();
        // SAFETY: filter and handle are valid; the context pointer outlives the registration.
        let result = unsafe {
            CM_Register_Notification(&filter, sender as *const c_void, Some(notify), &mut handle)
        };
        if result != CR_SUCCESS {
            // SAFETY: registration failed, so no callback can still reference `sender`.
            drop(unsafe { Box::from_raw(sender) });
            bail!("CM_Register_Notification for volume interfaces failed (CONFIGRET {result})");
        }
        Ok(Self {
            handle,
            sender,
            receiver,
        })
    }
}

impl ChangeSource for OsEventSource {
    fn wait(&mut self, timeout: Option<Duration>) -> Result<bool> {
        let received = match timeout {
            None => self.receiver.recv().is_ok(),
            Some(timeout) => match self.receiver.recv_timeout(timeout) {
                Ok(()) => true,
                Err(RecvTimeoutError::Timeout) => return Ok(false),
                Err(RecvTimeoutError::Disconnected) => false,
            },
        };
        if !received {
            bail!("volume notification channel closed unexpectedly");
        }
        while self.receiver.try_recv().is_ok() {}
        Ok(true)
    }
}

impl Drop for OsEventSource {
    fn drop(&mut self) {
        // SAFETY: unregistering waits for in-flight callbacks, after which `sender` is unused.
        unsafe {
            CM_Unregister_Notification(self.handle);
            drop(Box::from_raw(self.sender));
        }
    }
}
