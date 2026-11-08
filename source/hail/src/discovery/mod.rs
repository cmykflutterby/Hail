use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
mod lsblk;
#[cfg(target_os = "windows")]
mod windows;

pub use lsblk::parse_lsblk;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Disk {
    pub device: String,
    pub label: String,
    pub mount_point: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiskChange {
    Inserted(Disk),
    Removed(Disk),
}

/// Compares two snapshots; a remount is reported as a removal followed by an insertion.
pub fn changes(previous: &[Disk], current: &[Disk]) -> Vec<DiskChange> {
    let key = |disk: &Disk| (disk.device.clone(), disk.mount_point.clone());
    let old: BTreeMap<_, _> = previous.iter().map(|disk| (key(disk), disk)).collect();
    let new: BTreeMap<_, _> = current.iter().map(|disk| (key(disk), disk)).collect();
    let removed = old
        .iter()
        .filter(|(key, _)| !new.contains_key(*key))
        .map(|(_, disk)| DiskChange::Removed((*disk).clone()));
    let inserted = new
        .iter()
        .filter(|(key, _)| !old.contains_key(*key))
        .map(|(_, disk)| DiskChange::Inserted((*disk).clone()));
    removed.chain(inserted).collect()
}

/// Lists mounted USB mass-storage volumes. Discovery only reads OS metadata; it never writes to a volume.
pub fn discover_usb_volumes() -> Result<Vec<Disk>> {
    #[cfg(target_os = "linux")]
    return linux::discover();
    #[cfg(target_os = "windows")]
    return windows::discover();
}
