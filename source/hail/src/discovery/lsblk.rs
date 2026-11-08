use super::Disk;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
#[derive(Deserialize)]
struct BlockDevices {
    blockdevices: Vec<BlockDevice>,
}

#[derive(Deserialize)]
struct BlockDevice {
    path: String,
    tran: Option<String>,
    label: Option<String>,
    mountpoints: Option<Vec<Option<String>>>,
    #[serde(default)]
    children: Vec<BlockDevice>,
}

/// Parses `lsblk --json --paths --output PATH,TRAN,LABEL,MOUNTPOINTS`; partitions inherit USB transport.
pub fn parse_lsblk(json: &str, probed_devices: &[PathBuf]) -> Result<Vec<Disk>> {
    fn collect(node: BlockDevice, usb_parent: bool, probed: &[PathBuf], disks: &mut Vec<Disk>) {
        let usb = usb_parent
            || node.tran.as_deref() == Some("usb")
            || probed.iter().any(|path| path == Path::new(&node.path));
        if usb {
            for mount_point in node.mountpoints.into_iter().flatten().flatten() {
                if !mount_point.is_empty() {
                    disks.push(Disk {
                        device: node.path.clone(),
                        label: node.label.clone().unwrap_or_else(|| node.path.clone()),
                        mount_point: PathBuf::from(mount_point),
                    });
                }
            }
        }
        for child in node.children {
            collect(child, usb, probed, disks);
        }
    }
    let tree: BlockDevices = serde_json::from_str(json).context("parsing lsblk output")?;
    let mut disks = Vec::new();
    for node in tree.blockdevices {
        collect(node, false, probed_devices, &mut disks);
    }
    Ok(disks)
}
