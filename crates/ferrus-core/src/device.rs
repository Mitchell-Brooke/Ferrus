//! Enumeration of block devices via libudev.

use anyhow::Context;
use serde::Serialize;

/// A whole-disk block device (never a partition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockDevice {
    /// Kernel name, e.g. `sda`.
    pub name: String,
    /// Device node path, e.g. `/dev/sda`.
    pub devnode: String,
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub serial: Option<String>,
    pub size_bytes: u64,
    pub removable: bool,
    pub read_only: bool,
    /// Transport/bus, e.g. `usb`, `ata`, `mmc`.
    pub bus: Option<String>,
}

impl BlockDevice {
    /// Human-readable size, e.g. `57.3 GB`.
    pub fn size_string(&self) -> String {
        const GB: f64 = 1_000_000_000.0;
        let gb = self.size_bytes as f64 / GB;
        if gb >= 100.0 {
            format!("{gb:.0} GB")
        } else if gb >= 1.0 {
            format!("{gb:.1} GB")
        } else {
            format!("{} MB", (self.size_bytes as f64 / 1_000_000.0).round() as u64)
        }
    }

    /// One-line description used in the device selector.
    pub fn display_name(&self) -> String {
        let label = self
            .model
            .as_deref()
            .filter(|m| !m.is_empty())
            .or(self.vendor.as_deref())
            .unwrap_or("Generic USB Device")
            .replace('_', " ");
        format!("{} ({}) [{}]", label, self.size_string(), self.devnode)
    }

    /// True when this looks like a USB-attached stick or card reader.
    pub fn is_usb(&self) -> bool {
        self.bus.as_deref() == Some("usb")
    }
}

fn attr(device: &udev::Device, name: &str) -> Option<String> {
    device
        .attribute_value(name)
        .and_then(|v| v.to_str())
        .map(|s| s.trim().to_string())
}

fn prop(device: &udev::Device, name: &str) -> Option<String> {
    device
        .property_value(name)
        .and_then(|v| v.to_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Sysfs names that are not physical flash media.
const IGNORED_PREFIXES: &[&str] = &[
    "loop", "ram", "zram", "dm-", "md", "nbd", "sr", "fd", "pmem", "rtc", "vdb",
];

/// List candidate target disks visible to the current user.
pub fn list_block_devices() -> anyhow::Result<Vec<BlockDevice>> {
    let mut enumerator = udev::Enumerator::new().context("failed to create udev enumerator")?;
    enumerator.match_subsystem("block").context("match_subsystem")?;

    let mut devices = Vec::new();
    for entry in enumerator
        .scan_devices()
        .context("failed to scan udev devices")?
    {
        // Whole disks only; partitions carry a DEVTYPE.
        if entry.property_value("DEVTYPE").is_some_and(|t| t == "partition") {
            continue;
        }
        let Some(devnode) = entry.devnode() else { continue };
        let name = match devnode.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if IGNORED_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }

        // size is always reported in 512-byte sectors.
        let size_sectors: u64 = attr(&entry, "size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if size_sectors == 0 {
            continue;
        }

        devices.push(BlockDevice {
            name: name.clone(),
            devnode: devnode.display().to_string(),
            model: prop(&entry, "ID_MODEL_FROM_DATABASE")
                .or_else(|| prop(&entry, "ID_MODEL")),
            vendor: prop(&entry, "ID_VENDOR_FROM_DATABASE")
                .or_else(|| prop(&entry, "ID_VENDOR")),
            serial: prop(&entry, "ID_SERIAL_SHORT"),
            size_bytes: size_sectors * 512,
            removable: attr(&entry, "removable").as_deref() == Some("1"),
            read_only: attr(&entry, "ro").as_deref() == Some("1"),
            bus: prop(&entry, "ID_BUS"),
        });
    }

    devices.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(devices)
}
