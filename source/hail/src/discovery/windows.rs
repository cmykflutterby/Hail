use super::Disk;
use anyhow::Result;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BusTypeUsb, CreateFileW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Diagnostics::Debug::{SetErrorMode, SEM_FAILCRITICALERRORS};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceProperty, IOCTL_STORAGE_QUERY_PROPERTY,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn from_wide(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// Queries the storage bus with zero access rights, which cannot read or write volume data.
fn is_usb_volume(letter: char) -> bool {
    let path = wide(&format!(r"\\.\{letter}:"));
    // SAFETY: `path` is NUL-terminated and outlives the call; the handle is closed below.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // SAFETY: zero is a valid bit pattern for this plain C struct.
    let mut descriptor: STORAGE_DEVICE_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: input and output buffers are valid for the sizes passed.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&query as *const STORAGE_PROPERTY_QUERY).cast(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            (&mut descriptor as *mut STORAGE_DEVICE_DESCRIPTOR).cast(),
            size_of::<STORAGE_DEVICE_DESCRIPTOR>() as u32,
            &mut returned,
            null_mut(),
        )
    };
    // SAFETY: `handle` was returned by CreateFileW and is closed once.
    unsafe { CloseHandle(handle) };
    ok != 0 && descriptor.BusType == BusTypeUsb
}

pub fn discover() -> Result<Vec<Disk>> {
    // Suppresses "insert a disk" dialogs for empty card readers.
    // SAFETY: SetErrorMode only changes process error-reporting flags.
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS) };
    // SAFETY: GetLogicalDrives has no preconditions.
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        anyhow::bail!(
            "GetLogicalDrives failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let mut disks = Vec::new();
    for (index, letter) in ('A'..='Z').enumerate() {
        if mask & (1 << index) == 0 {
            continue;
        }
        let root = format!(r"{letter}:\");
        let root_wide = wide(&root);
        let mut volume = [0u16; 64];
        let mut label = [0u16; 261];
        // SAFETY: buffers are valid and their lengths are passed in UTF-16 units.
        let mounted = unsafe {
            GetVolumeNameForVolumeMountPointW(
                root_wide.as_ptr(),
                volume.as_mut_ptr(),
                volume.len() as u32,
            ) != 0
                && GetVolumeInformationW(
                    root_wide.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    0,
                ) != 0
        };
        if !mounted || !is_usb_volume(letter) {
            continue;
        }
        let label = from_wide(&label);
        disks.push(Disk {
            device: from_wide(&volume),
            label: if label.is_empty() {
                format!("{letter}:")
            } else {
                label
            },
            mount_point: PathBuf::from(root),
        });
    }
    Ok(disks)
}
