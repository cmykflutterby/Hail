use super::lsblk::parse_lsblk;
use super::Disk;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn probe_usb_devices() -> Result<Vec<PathBuf>> {
    use futures::StreamExt;
    use usb_disk_probe::stream::UsbDiskProbe;

    if !Path::new("/dev/disk/by-path").is_dir() {
        return Ok(Vec::new());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("creating USB probe runtime")?;
    runtime.block_on(async {
        let mut paths = Vec::new();
        let mut probe = UsbDiskProbe::new()
            .await
            .context("initialising USB probe")?;
        while let Some(device) = probe.next().await {
            let device = device.context("enumerating USB probe device")?;
            paths.push(
                std::fs::canonicalize(&device)
                    .with_context(|| format!("resolving USB path {}", device.display()))?,
            );
        }
        Ok(paths)
    })
}

pub fn discover() -> Result<Vec<Disk>> {
    // usb-disk-probe matches PCI USB paths only; lsblk TRAN also covers Raspberry Pi SoC USB.
    let probed = probe_usb_devices()?;
    let output = Command::new("lsblk")
        .args([
            "--json",
            "--paths",
            "--output",
            "PATH,TRAN,LABEL,MOUNTPOINTS",
        ])
        .output()
        .context("starting lsblk")?;
    if !output.status.success() {
        bail!(
            "lsblk failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    parse_lsblk(
        std::str::from_utf8(&output.stdout).context("lsblk output is not UTF-8")?,
        &probed,
    )
}
