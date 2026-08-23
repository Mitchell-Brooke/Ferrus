//! Image inspection and flashing-strategy selection.
//!
//! The helper loop-mounts an ISO read-only, `scan_tree` walks it to build an
//! [`ImageManifest`], and [`choose_plan`] maps that onto a concrete
//! [`FlashPlan`] — mirroring Rufus's automatic decisions:
//!
//! * not a Windows ISO            → raw DD (hybrid ISO)
//! * Windows, everything ≤ 4 GiB  → GPT + FAT32
//! * Windows, oversized WIM/ESD   → FAT32 + split into .swm parts
//!   (falls back to UEFI:NTFS when `wimlib-imagex` is unavailable)
//! * Windows, other oversized file → UEFI:NTFS dual-partition layout

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::protocol::FlashPlan;

/// Largest file a FAT32 filesystem accepts; keep a safety margin below it.
pub const FAT32_MAX_FILE: u64 = 4 * 1024 * 1024 * 1024 - 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageManifest {
    pub is_windows: bool,
    /// Total bytes of regular-file payload.
    pub total_size: u64,
    /// Size of the largest single file.
    pub max_file_size: u64,
    /// Slash-separated path (relative to the ISO root) of an oversized
    /// `.wim`/`.esd` that would need splitting, if any.
    pub oversized_wim: Option<String>,
    /// Any file at or beyond the FAT32 ceiling exists.
    pub has_oversized_files: bool,
    /// Live-USB persistence label this image expects:
    /// `casper-rw` (Ubuntu family) or `persistence` (Debian). None when
    /// persistence support can't be determined.
    #[serde(default)]
    pub linux_flavor: Option<String>,
    /// Does the source ISO carry MBR boot code (sector-copyable)?
    /// Filled in by the caller that has the raw file; tree scans leave
    /// it false.
    #[serde(default)]
    pub hybrid: bool,
    /// Relative directory (from the ISO root) holding a SYSLINUX/
    /// ISOLINUX configuration file, if any — the prerequisite for
    /// extraction plans to produce a BIOS-bootable stick.
    #[serde(default)]
    pub syslinux_dir: Option<String>,
}

/// Directories SYSLINUX-family loaders conventionally live in, searched
/// in preference order.
const SYSLINUX_DIRS: [&str; 5] = [
    "isolinux",
    "boot/syslinux",
    "syslinux",
    "boot/isolinux",
    "images", // some Debian media keep cfg here
];

/// Find a SYSLINUX/ISOLINUX config file under `root`, returning the
/// containing directory relative to `root`.
fn find_syslinux_cfg(root: &Path) -> Option<String> {
    for dir in SYSLINUX_DIRS {
        let d = root.join(dir);
        if !d.is_dir() {
            continue;
        }
        for cfg in ["syslinux.cfg", "isolinux.cfg"] {
            if d.join(cfg).is_file() {
                return Some(dir.to_string());
            }
        }
    }
    None
}

/// Case-insensitive child lookup — ISO9660 trees frequently arrive uppercase.
fn ci_lookup(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(name);
    if exact.exists() {
        return Some(exact);
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let lower = name.to_ascii_lowercase();
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().to_ascii_lowercase() == lower {
            return Some(e.path());
        }
    }
    None
}

fn is_wimish(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".wim") || lower.ends_with(".esd")
}

fn walk(root: &Path, dir: &Path, m: &mut ImageManifest) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk(root, &entry.path(), m)?;
        } else if meta.is_file() {
            let len = meta.len();
            m.total_size += len;
            if len > m.max_file_size {
                m.max_file_size = len;
            }
            if len > FAT32_MAX_FILE {
                m.has_oversized_files = true;
                if m.oversized_wim.is_none() && is_wimish(&entry.file_name().to_string_lossy()) {
                    let full = entry.path();
                    let rel = full.strip_prefix(root).unwrap_or(&full);
                    m.oversized_wim = Some(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    Ok(())
}

/// Detect a Windows install medium by its marker files (`bootmgr` plus a
/// `sources/install.{wim,esd}`), tolerating any letter case.
pub fn looks_like_windows(root: &Path) -> bool {
    if !ci_lookup(root, "bootmgr").is_some() {
        return false;
    }
    let sources = match ci_lookup(root, "sources") {
        Some(p) => p,
        None => return false,
    };
    ["install.wim", "install.esd"]
        .iter()
        .any(|n| ci_lookup(&sources, n).is_some())
}

/// Walk an already-mounted tree and classify it.
pub fn scan_tree(root: &Path) -> std::io::Result<ImageManifest> {
    let mut m = ImageManifest::default();
    walk(root, root, &mut m)?;
    m.is_windows = looks_like_windows(root);
    m.linux_flavor = detect_linux_flavor(root);
    m.syslinux_dir = find_syslinux_cfg(root);
    Ok(m)
}

/// Which persistence convention a Linux live ISO follows, based on
/// `.disk/info` and directory layout (Ubuntu's casper vs Debian's live-boot).
/// Returns the ext4 label to use for the persistence partition.
pub fn detect_linux_flavor(root: &Path) -> Option<String> {
    if looks_like_windows(root) {
        return None;
    }
    let disk_info = ci_lookup(root, ".disk")
        .and_then(|d| ci_lookup(&d, "info"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let has_casper = ci_lookup(root, "casper").is_some_and(|p| p.is_dir());
    let has_live = ci_lookup(root, "live").is_some_and(|p| p.is_dir());

    // Debian ships `live/` and names itself in .disk/info; Ubuntu ships
    // `casper/` and mentions Ubuntu/Xubuntu/Kubuntu/Lubuntu… in .disk/info.
    if disk_info.contains("debian") && !disk_info.contains("ubuntu") {
        return Some("persistence".into());
    }
    if has_casper || disk_info.contains("ubuntu") || disk_info.contains("mint") {
        return Some("casper-rw".into());
    }
    if has_live {
        return Some("persistence".into());
    }
    None
}

/// Is `wimlib-imagex` on PATH? Decides whether oversized WIMs can be split.
pub fn have_wimlib() -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    std::env::split_paths(&path).any(|dir| {
        let c = dir.join("wimlib-imagex");
        c.is_file()
    })
}

/// Does the ISO carry x86 boot code in its system area (sector 0)?
/// Hybrid ISOs (isohybrid / xorriso `-isohybrid-mbr`) place an MBR there;
/// pure ISO9660 images leave the 32 KiB system area zeroed, so a
/// sector-copy would produce an unbootable stick.
pub fn is_hybrid(path: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut sector = [0u8; 512];
    f.read_exact(&mut sector)?;
    // A handful of stray nonzero bytes are tolerated; real boot code is
    // dense. Rufus applies the same "more than N bytes" heuristic.
    Ok(sector.iter().filter(|&&b| b != 0).count() > 64)
}

/// Edition names inside a Windows ISO's install.wim/esd.
///
/// Streams the WIM out of the ISO with `xorriso -osirrox` (unprivileged,
/// no root needed) and reads its metadata with `wimlib-imagex info`.
/// Returns an empty Vec when either tool is unavailable or probing
/// fails — callers should fall back to a plain numeric edition picker.
pub fn windows_editions(iso: &Path) -> Vec<String> {
    if !have_wimlib() || !iso.is_file() {
        return Vec::new();
    }
    let work = std::env::temp_dir().join(format!(
        "ferrus-editions-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    if std::fs::create_dir_all(&work).is_err() {
        return Vec::new();
    }
    let result = probe_editions(iso, &work);
    std::fs::remove_dir_all(&work).ok();
    result.unwrap_or_default()
}

fn probe_editions(iso: &Path, work: &Path) -> Option<Vec<String>> {
    // Windows media mixes letter cases between vendors; osirrox paths are
    // literal, so try the realistic spellings until one extracts.
    const CANDIDATES: [&str; 6] = [
        "/sources/install.wim",
        "/SOURCES/INSTALL.WIM",
        "/sources/INSTALL.WIM",
        "/sources/install.esd",
        "/SOURCES/INSTALL.ESD",
        "/sources/INSTALL.ESD",
    ];
    let mut wim_path = None;
    for cand in CANDIDATES {
        let dest = work.join("image.bin");
        let status = std::process::Command::new("xorriso")
            .args(["-osirrox", "on", "-indev"])
            .arg(iso)
            .args(["-extract", cand])
            .arg(&dest)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        if status.success() && dest.is_file() {
            wim_path = Some(dest);
            break;
        }
    }
    let wim_path = wim_path?;

    let out = std::process::Command::new("wimlib-imagex")
        .arg("info")
        .arg(&wim_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut images = parse_wim_info(&text);
    images.sort_by_key(|(i, _)| *i);
    Some(images.into_iter().map(|(_, n)| n).collect())
}

/// Pull `(index, name)` pairs out of `wimlib-imagex info` output.
fn parse_wim_info(text: &str) -> Vec<(u32, String)> {
    let mut images = Vec::new();
    let mut index: Option<u32> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Index:") {
            index = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("Name:") {
            if let Some(i) = index {
                images.push((i, rest.trim().to_string()));
            }
        }
    }
    images
}

/// Is the SYSLINUX BIOS installer on PATH? Gates extraction plans.
pub fn have_syslinux() -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    std::env::split_paths(&path).any(|dir| dir.join("syslinux").is_file())
}

/// Map a manifest onto a concrete flashing plan (Rufus's auto behaviour).
pub fn choose_plan(m: &ImageManifest, image: &Path) -> FlashPlan {
    use crate::protocol::{PartitionScheme, WinOptions};
    let img = || image.to_string_lossy().into_owned();

    if !m.is_windows {
        // Non-hybrid ISO: sector-copying would yield an unbootable stick
        // (no MBR boot code), so extract onto FAT32 + install SYSLINUX
        // instead — but only when a loader config exists to install for
        // and the installer is available.
        if !m.hybrid && m.syslinux_dir.is_some() && have_syslinux() {
            return FlashPlan::IsoExtract {
                image: img(),
                scheme: PartitionScheme::Mbr,
                syslinux_bios: true,
            };
        }
        return FlashPlan::RawDd {
            image: img(),
            verify: true,
            persistence_mb: 0,
            persistence_label: None,
        };
    }

    if m.has_oversized_files {
        if m.oversized_wim.is_some() && have_wimlib() {
            // Only WIM-family files are oversize and we can split them:
            // plain FAT32 with .swm parts keeps maximum firmware compat.
            return FlashPlan::WinFat32 {
                image: img(),
                split_wim: true,
                scheme: PartitionScheme::Gpt,
                options: WinOptions::default(),
            };
        }
        return FlashPlan::WinUefiNtfs {
            image: img(),
            scheme: PartitionScheme::Gpt,
            options: WinOptions::default(),
        };
    }

    FlashPlan::WinFat32 {
        image: img(),
        split_wim: false,
        scheme: PartitionScheme::Gpt,
        options: WinOptions::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FlashPlan;

    fn touch(p: &Path, len: u64) {
        if len == 0 {
            std::fs::File::create(p).unwrap();
        } else {
            std::fs::File::create(p).unwrap().set_len(len).unwrap(); // sparse
        }
    }

    fn linux_tree(base: &Path) {
        std::fs::create_dir_all(base.join("boot/grub")).unwrap();
        touch(&base.join("boot/vmlinuz"), 8 << 20);
        touch(&base.join("boot/grub/grub.cfg"), 4096);
    }

    fn windows_tree(base: &Path, wim_len: u64) {
        std::fs::create_dir_all(base.join("sources")).unwrap();
        touch(&base.join("BOOTMGR"), 409_600);
        touch(&base.join("SETUP.EXE"), 90_112);
        touch(&base.join("sources/install.wim"), wim_len);
        touch(&base.join("sources/boot.wim"), 300 << 20);
    }

    #[test]
    fn linux_hybrid_iso_detected_and_dd_planned() {
        let base = std::env::temp_dir().join("ferrus-test-linux");
        std::fs::remove_dir_all(&base).ok();
        linux_tree(&base);

        let mut m = scan_tree(&base).unwrap();
        assert!(!m.is_windows);
        assert_eq!(m.total_size, (8 << 20) + 4096);
        m.hybrid = true; // sector-copyable image
        let plan = choose_plan(&m, Path::new("/tmp/l.iso"));
        assert_eq!(
            plan,
            FlashPlan::RawDd {
                image: "/tmp/l.iso".into(),
                verify: true,
                persistence_mb: 0,
                persistence_label: None
            }
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn nonhybrid_linux_iso_uses_extract_when_syslinux_present() {
        let base = std::env::temp_dir().join("ferrus-test-linux-nonhybrid");
        std::fs::remove_dir_all(&base).ok();
        linux_tree(&base);

        // No SYSLINUX config in the tree: extraction is impossible, so
        // even a non-hybrid image falls back to raw DD.
        let m = scan_tree(&base).unwrap();
        assert!(m.syslinux_dir.is_none());
        let plan = choose_plan(&m, Path::new("/tmp/l.iso"));
        assert!(matches!(plan, FlashPlan::RawDd { .. }));

        // With isolinux/isolinux.cfg present the extract plan applies
        // (when the syslinux installer exists on this machine).
        std::fs::create_dir_all(base.join("isolinux")).unwrap();
        std::fs::write(
            base.join("isolinux/isolinux.cfg"),
            "DEFAULT linux\nLABEL linux\nKERNEL /boot/vmlinuz\n",
        )
        .unwrap();
        let m = scan_tree(&base).unwrap();
        assert_eq!(m.syslinux_dir.as_deref(), Some("isolinux"));
        let plan = choose_plan(&m, Path::new("/tmp/l.iso"));
        if have_syslinux() {
            assert!(matches!(
                plan,
                FlashPlan::IsoExtract { syslinux_bios: true, .. }
            ));
        } else {
            assert!(matches!(plan, FlashPlan::RawDd { .. }));
        }
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn windows_small_fits_fat32() {
        let base = std::env::temp_dir().join("ferrus-test-win-small");
        std::fs::remove_dir_all(&base).ok();
        windows_tree(&base, 100 << 20); // 100 MiB wim

        let m = scan_tree(&base).unwrap();
        assert!(m.is_windows);
        assert!(!m.has_oversized_files);
        assert_eq!(
            choose_plan(&m, Path::new("/tmp/w.iso")),
            FlashPlan::WinFat32 {
                image: "/tmp/w.iso".into(),
                split_wim: false,
                scheme: Default::default(),
                options: Default::default()
            }
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn windows_big_wim_splits_when_wimlib_present() {
        let base = std::env::temp_dir().join("ferrus-test-win-big");
        std::fs::remove_dir_all(&base).ok();
        windows_tree(&base, FAT32_MAX_FILE + (1 << 30)); // 5 GiB sparse

        let m = scan_tree(&base).unwrap();
        assert!(m.has_oversized_files);
        assert_eq!(
            m.oversized_wim.as_deref(),
            Some("sources/install.wim"),
            "uppercase tree must still yield clean relative path"
        );

        if have_wimlib() {
            assert_eq!(
                choose_plan(&m, Path::new("/w.iso")),
                FlashPlan::WinFat32 {
                    image: "/w.iso".into(),
                    split_wim: true,
                    scheme: Default::default(),
                    options: Default::default()
                }
            );
        }
        // UEFI:NTFS must be chosen whenever splitting is unavailable.
        let forced_no_split = ImageManifest {
            oversized_wim: Some("sources/install.wim".into()),
            ..m.clone()
        };
        let saved_path = std::env::var_os("PATH");
        std::env::set_var("PATH", "/nonexistent-ferrus-test");
        assert!(!have_wimlib(), "PATH override failed");
        assert_eq!(
            choose_plan(&forced_no_split, Path::new("/w.iso")),
            FlashPlan::WinUefiNtfs {
                image: "/w.iso".into(),
                scheme: Default::default(),
                options: Default::default()
            }
        );
        if let Some(p) = saved_path {
            std::env::set_var("PATH", p);
        }
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn nonwim_oversized_file_forces_uefi_ntfs() {
        let base = std::env::temp_dir().join("ferrus-test-win-other");
        std::fs::remove_dir_all(&base).ok();
        windows_tree(&base, 100 << 20);
        touch(&base.join("huge.vhdx"), FAT32_MAX_FILE + 1);

        let m = scan_tree(&base).unwrap();
        assert!(m.has_oversized_files);
        assert!(m.oversized_wim.is_none()); // vhdx is not splittable
        assert_eq!(
            choose_plan(&m, Path::new("/v.iso")),
            FlashPlan::WinUefiNtfs {
                image: "/v.iso".into(),
                scheme: Default::default(),
                options: Default::default()
            }
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn hybrid_sector0_detection() {
        use std::io::Write;
        let p = std::env::temp_dir().join("ferrus-test-hybrid.bin");

        // Zeroed system area -> not hybrid.
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[0u8; 2048]).unwrap();
        drop(f);
        assert!(!is_hybrid(&p).unwrap());

        // Dense x86 boot code (isohybrid MBR) -> hybrid.
        let mut code = [0u8; 512];
        for (i, b) in code.iter_mut().enumerate() {
            *b = if i < 440 { (i % 251) as u8 } else { 0 };
        }
        let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.write_all(&code).unwrap();
        drop(f);
        assert!(is_hybrid(&p).unwrap());

        // Sparse noise below the threshold -> still treated as non-hybrid.
        let mut sparse = [0u8; 512];
        for b in sparse.iter_mut().take(32) {
            *b = 0xAA;
        }
        let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.write_all(&sparse).unwrap();
        drop(f);
        assert!(!is_hybrid(&p).unwrap());

        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn wim_info_parsing() {
        let sample = "\
Image Count: 2
Compression: LZX
Chunk Size: 32768

Index: 1
Name: Windows 11 Home
Description: Windows 11 Home
Flags: Home
Files: 55123

Index: 2
Name: Windows 11 Pro
Description: Windows 11 Pro
";
        assert_eq!(
            parse_wim_info(sample),
            vec![
                (1u32, "Windows 11 Home".to_string()),
                (2, "Windows 11 Pro".to_string())
            ]
        );
        assert!(parse_wim_info("Index: not-a-number\nName: X").is_empty());
        assert!(parse_wim_info("").is_empty());
    }

    #[test]
    fn linux_flavor_detection() {
        // Ubuntu-style: casper dir.
        let base = std::env::temp_dir().join("ferrus-test-flav-ubuntu");
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join(".disk")).unwrap();
        std::fs::create_dir_all(base.join("casper")).unwrap();
        std::fs::write(base.join(".disk/info"), "Ubuntu 24.04 LTS amd64").unwrap();
        let m = scan_tree(&base).unwrap();
        assert!(!m.is_windows);
        assert_eq!(m.linux_flavor.as_deref(), Some("casper-rw"));
        std::fs::remove_dir_all(&base).ok();

        // Debian-style: live dir + Debian in .disk/info.
        let base = std::env::temp_dir().join("ferrus-test-flav-debian");
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join(".disk")).unwrap();
        std::fs::create_dir_all(base.join("live")).unwrap();
        std::fs::write(base.join(".disk/info"), "Debian GNU/Linux 12.5.0").unwrap();
        let m = scan_tree(&base).unwrap();
        assert_eq!(m.linux_flavor.as_deref(), Some("persistence"));
        std::fs::remove_dir_all(&base).ok();

        // Unknown Linux tree → no persistence support offered.
        let base = std::env::temp_dir().join("ferrus-test-flav-unknown");
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("boot")).unwrap();
        let m = scan_tree(&base).unwrap();
        assert_eq!(m.linux_flavor, None);
        std::fs::remove_dir_all(&base).ok();
    }
}
