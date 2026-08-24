//! Privileged primitives for plan execution: partitioning, formatting,
//! mounting, tree copying and WIM splitting. Every external command failure
//! carries its stderr so protocol errors stay actionable.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context};
use crc32fast::hash as crc32_hash;
use ferrus_core::iso::{self, ImageManifest};
use ferrus_core::protocol::FlashPlan;
use rand::RngCore;

/// Registered PIDs of external children (e.g. wimlib) so `cancel` can kill them.
pub type Pids = Arc<Mutex<Vec<u32>>>;

fn run(cmd: &str, args: &[&str], stdin_script: Option<&str>) -> anyhow::Result<String> {    let mut c = Command::new(cmd);
    c.args(args);
    if stdin_script.is_some() {
        c.stdin(Stdio::piped());
    }
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().with_context(|| format!("spawn {cmd}"))?;
    if let Some(script) = stdin_script {
        let mut si = child.stdin.take().context("child stdin")?;
        si.write_all(script.as_bytes()).ok();
        drop(si);
    }
    let out = child.wait_with_output().context(format!("run {cmd}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let tail: String = stderr.chars().rev().take(400).collect::<Vec<_>>()
            .into_iter().rev().collect();
        bail!("{cmd} failed ({}): {tail}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Best-effort wipe of leftover signatures so kernel re-scan stays clean.
pub fn wipe_signatures(dev: &Path) -> anyhow::Result<()> {
    let _ = run("wipefs", &["-a", &dev.to_string_lossy()], None);
    Ok(())
}

struct PartSpec {
    name: String,
    /// Size in 512-byte sectors; None = fill remaining space.
    size_sectors: Option<u64>,
    /// GPT type GUID.
    gpt_type: &'static str,
    /// MBR type byte (hex string without 0x).
    mbr_type: &'static str,
    /// MBR only: set the active/boot flag.
    mbr_boot: bool,
    /// MBR only: force a legacy start sector (old-BIOS alignment fix).
    mbr_start: Option<u64>,
}

const MS_BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
const LINUX_FS_DATA: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
const UEFI_NTFS_PART_SECTORS: u64 = 32_768; // 16 MiB — fits res/uefi-ntfs.img

fn data_part(name: &str, size_sectors: Option<u64>, linux_fs: bool) -> PartSpec {
    PartSpec {
        name: name.to_string(),
        size_sectors,
        gpt_type: if linux_fs {
            LINUX_FS_DATA
        } else {
            MS_BASIC_DATA
        },
        mbr_type: if linux_fs { "83" } else { "07" },
        mbr_boot: false,
        mbr_start: None,
    }
}

fn partition_gpt(dev: &Path, parts: &[PartSpec]) -> anyhow::Result<()> {
    let mut script = String::from("label: gpt\n");
    for p in parts {
        let mut line = format!("name=\"{}\"", p.name);
        if let Some(s) = p.size_sectors {
            line.push_str(&format!(", size={s}"));
        }
        line.push_str(&format!(", type={}", p.gpt_type));
        script.push_str(&line);
        script.push('\n');
    }
    run(
        "sfdisk",
        &["-q", "--force", "--wipe=always", &dev.to_string_lossy()],
        Some(&script),
    )
    .context("writing GPT partition table")
    .map(drop)
}

fn partition_mbr(dev: &Path, parts: &[PartSpec]) -> anyhow::Result<()> {
    let mut script = String::from("label: dos\n");
    for p in parts {
        let mut bits: Vec<String> = Vec::new();
        if let Some(s) = p.mbr_start {
            bits.push(format!("start={s}"));
        }
        if let Some(s) = p.size_sectors {
            bits.push(format!("size={s}"));
        }
        bits.push(format!("type={}", p.mbr_type));
        if p.mbr_boot {
            bits.push("bootable".into());
        }
        script.push_str(&bits.join(", "));
        script.push('\n');
    }
    run(
        "sfdisk",
        &["-q", "--force", "--wipe=always", &dev.to_string_lossy()],
        Some(&script),
    )
    .context("writing MBR partition table")
    .map(drop)
}

fn partition(dev: &Path, scheme: PartitionScheme, parts: &[PartSpec]) -> anyhow::Result<()> {
    match scheme {
        PartitionScheme::Gpt => partition_gpt(dev, parts),
        PartitionScheme::Mbr => partition_mbr(dev, parts),
    }
}

use ferrus_core::protocol::{BadBlocks, PartitionScheme};

/// Filesystem-type classification shared by layout + format steps.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FsKind {
    Fat,
    Exfat,
    Ntfs,
    Udf,
    Ext,
}

fn fs_kind(fs: &str) -> FsKind {
    match fs.to_ascii_lowercase().as_str() {
        "fat32" | "vfat" => FsKind::Fat,
        "exfat" => FsKind::Exfat,
        "ntfs" => FsKind::Ntfs,
        "udf" => FsKind::Udf,
        _ => FsKind::Ext,
    }
}

/// Rufus-style volume-label sanitising: uppercase, length-capped per FS.
fn sanitize_label(fs: &str, label: &str) -> String {
    let max = match fs_kind(fs) {
        FsKind::Fat => 11,
        FsKind::Exfat => 15,
        FsKind::Ntfs => 32,
        FsKind::Udf => 30,
        FsKind::Ext => 16,
    };
    label.chars().take(max).collect::<String>().to_uppercase()
}

/// Create a filesystem on `node`, honouring an optional cluster size.
pub fn mkfs_any(
    fs: &str,
    label: &str,
    node: &Path,
    cluster_bytes: Option<u64>,
) -> anyhow::Result<()> {
    let label = sanitize_label(fs, label);
    let n = node.to_string_lossy().into_owned();
    match fs_kind(fs) {
        FsKind::Fat => {
            // -s takes sectors-per-cluster (512-byte sectors).
            let spc = cluster_bytes
                .filter(|c| [512u64, 1024, 2048, 4096, 8192, 16384, 32768, 65536].contains(c))
                .map(|c| (c / 512).to_string());
            let mut args: Vec<String> = vec!["-F32".into(), "-n".into(), label];
            if let Some(s) = spc {
                args.push("-s".into());
                args.push(s);
            }
            args.push(n.clone());
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            with_retries(
                || run("mkfs.vfat", &refs, None).context("creating FAT32 filesystem").map(drop),
                4,
                600,
            )
        }
        FsKind::Ntfs => {
            with_retries(
                || {
                    run("mkfs.ntfs", &["-Q", "-F", "-L", &label, &n], None)
                        .context("creating NTFS filesystem")
                        .map(drop)
                },
                4,
                600,
            )
        }
        FsKind::Exfat => {
            let c = cluster_bytes
                .filter(|c| (512..=131072).contains(c))
                .map(|c| c.to_string());
            let mut args: Vec<String> = vec!["-n".into(), label.clone()];
            if let Some(s) = c {
                args.push("-c".into());
                args.push(s);
            }
            args.push(n.clone());
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            with_retries(
                || run("mkfs.exfat", &refs, None).context("creating exFAT filesystem").map(drop),
                4,
                600,
            )
        }
        FsKind::Udf => {
            let lvid = format!("--lvid={label}");
            run("mkudffs", &[&lvid, &n], None)
                .context("creating UDF filesystem (is udftools installed?)")
                .map(drop)
        }
        FsKind::Ext => {
            let variant = match fs.to_ascii_lowercase().as_str() {
                "ext2" => "mkfs.ext2",
                "ext3" => "mkfs.ext3",
                _ => "mkfs.ext4",
            };
            // e2fsprogs caps block size at the page size; larger clusters ignored.
            let b = cluster_bytes
                .filter(|c| [1024u64, 2048, 4096].contains(c))
                .map(|c| c.to_string());
            let mut args: Vec<String> = vec!["-F".into(), "-L".into(), label];
            if let Some(bs) = b {
                args.push("-b".into());
                args.push(bs);
            }
            args.push(n.clone());
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            with_retries(
                || run(variant, &refs, None).context(format!("creating {variant}")).map(drop),
                4,
                600,
            )
        }
    }
}

/// Pre-flight sector scan (Rufus's bad-block check). Aborts on any defect.
pub fn scan_bad_blocks(
    dev: &Path,
    mode: BadBlocks,
) -> anyhow::Result<u64> {
    let n = dev.to_string_lossy().into_owned();
    let mut args: Vec<String> = match mode {
        BadBlocks::Fast => vec!["-sv".into()],
        BadBlocks::Thorough => vec!["-wsv".into()],
    };
    args.push("-b".into());
    args.push("4096".into());
    args.push(n);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run("badblocks", &refs, None).context("running badblocks")?;
    // Summary lands on stdout: "Pass completed, N bad blocks found."
    let count = out
        .lines()
        .rev()
        .find_map(|l| {
            let idx = l.find("bad blocks found")?;
            l[..idx].rsplit([' ', ',']).next()?.trim().parse::<u64>().ok()
        })
        .unwrap_or(0);
    Ok(count)
}

/// Best-effort unmount of every partition of `dev` before repartitioning.
fn release_partitions(dev: &Path) {
    for i in 1..=8u8 {
        let node = part_node(dev, i);
        let _ = run("umount", &["-l", &node.to_string_lossy()], None);
    }
}

/// Partition node naming differs for loop devices (`loop0p1` vs `sda1`).
pub fn part_node(dev: &Path, n: u8) -> PathBuf {
    let name = dev
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let leaf = if name.starts_with("loop") {
        format!("{name}p{n}")
    } else {
        format!("{name}{n}")
    };
    dev.with_file_name(leaf)
}

fn wait_for_node(node: &Path, timeout_secs: u64) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if node.exists() {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            bail!("partition node {} never appeared", node.display());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn reread_partitions(dev: &Path) {
    let d = dev.to_string_lossy();
    // Kernels (notably WSL2) can keep stale partition registrations for a
    // re-used loop device; delete-then-add recovers where a plain update
    // fails with EINVAL/EBUSY.
    let _ = run("partx", &["-d", &d], None);
    let _ = run("partx", &["-u", &d], None);
    let _ = run("partx", &["-a", &d], None);
    let _ = run("udevadm", &["settle", "--timeout=8"], None);
    // Give udevd a beat to finish its own exclusive probes of new nodes.
    std::thread::sleep(std::time::Duration::from_millis(400));
}

pub fn dd_to_node(file: &Path, node: &Path) -> anyhow::Result<()> {
    with_retries(
        || {
            run(
                "dd",
                &[
                    &format!("if={}", file.display()),
                    &format!("of={}", node.display()),
                    "bs=4M",
                    "conv=fsync",
                    "status=none",
                ],
                None,
            )
            .context("writing bootloader image")
            .map(drop)
        },
        4,
        600,
    )
}

/// Device size in 512-byte sectors.
fn device_sectors(dev: &Path) -> anyhow::Result<u64> {
    let out = run(
        "blockdev",
        &["--getsize64", &dev.to_string_lossy()],
        None,
    )
    .context("querying device size")?;
    let bytes = out.trim().parse::<u64>().context("device size parse")?;
    Ok(bytes / 512)
}

/// (start, size) of every existing partition, plus the table flavour.
fn dump_partitions(dev: &Path) -> anyhow::Result<(bool, Vec<(u64, u64)>)> {
    let out = run("sfdisk", &["-d", &dev.to_string_lossy()], None)
        .context("reading partition table")?;
    let mut is_gpt = false;
    let mut parts = Vec::new();
    for line in out.lines() {
        if line.starts_with("label: gpt") {
            is_gpt = true;
        }
        let Some(colon) = line.find(':') else { continue };
        let (Some(start), Some(size)) = (
            extract_num(line, "start="),
            extract_num(&line[colon..], "size="),
        ) else {
            continue;
        };
        parts.push((start, size));
    }
    Ok((is_gpt, parts))
}

fn extract_num(s: &str, key: &str) -> Option<u64> {
    s.find(key)
        .map(|i| &s[i + key.len()..])
        .and_then(|rest| {
            rest.trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
}

/// Append an ext4 persistence partition in the free space after the image
/// (Rufus's Ubuntu/Debian live-USB slider). `flavor_label` is `casper-rw`
/// or `persistence`; the Debian variant also gets a `persistence.conf`.
pub fn add_persistence(
    dev: &Path,
    persistence_mb: u64,
    flavor_label: &str,
    cancel: &AtomicBool,
    _pids: &Pids,
    send: &dyn Fn(ferrus_core::protocol::Response),
) -> anyhow::Result<String> {
    let (is_gpt, parts) = dump_partitions(dev)?;
    if parts.is_empty() {
        bail!("image has no partitions; cannot add persistence");
    }
    if !is_gpt && parts.len() >= 4 {
        bail!("MBR table already has 4 primary partitions; no room for persistence");
    }

    let next_start = parts
        .iter()
        .map(|(s, sz)| s + sz)
        .max()
        .unwrap_or(2048)
        .next_multiple_of(2048); // 1 MiB alignment
    let total = device_sectors(dev)?;
    // Keep clear of GPT backup header / trailing metadata.
    let last_usable = total.saturating_sub(if is_gpt { 128 } else { 0 });
    if next_start >= last_usable - 1 {
        bail!("not enough space left on the device for a persistence partition");
    }
    let want = persistence_mb.saturating_mul(2).min(last_usable - next_start);
    if want < 4096 {
        bail!("requested persistence size too small for this device");
    }

    progress_tick(send, "adding persistence partition");
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    let mut script = String::new();
    if is_gpt {
        script.push_str(&format!(
            "start={next_start}, size={want}, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=FERRUS-PERSIST\n"
        ));
    } else {
        script.push_str(&format!("start={next_start}, size={want}, type=83\n"));
    }
    run(
        "sfdisk",
        &["-q", "--force", "--append", &dev.to_string_lossy()],
        Some(&script),
    )
    .context("appending persistence partition")?;
    reread_partitions(dev);

    let idx = parts.len() as u8 + 1;
    let node = part_node(dev, idx);
    wait_for_node(&node, 10)?;

    progress_tick(send, "formatting persistence");
    with_retries(
        || {
            run(
                "mkfs.ext4",
                &["-F", "-L", flavor_label, &node.to_string_lossy()],
                None,
            )
            .context(format!("creating ext4 ({flavor_label})"))
            .map(drop)
        },
        4,
        600,
    )?;

    if flavor_label == "persistence" {
        // Debian live-boot needs a persistence.conf marking the overlay root.
        progress_tick(send, "writing persistence.conf");
        let mnt = Mount::rw(&node)?;
        std::fs::write(mnt.path().join("persistence.conf"), "/ union\n")
            .context("writing persistence.conf")?;
        mnt.unmount()?;
    }
    sync_dev(dev);
    Ok(node
        .to_str()
        .unwrap_or("persistence partition")
        .to_string())
}

/// Drop the generated answer file into the copied tree when any Windows
/// user-experience option was requested.
pub fn inject_unattend(target_root: &Path, opts: &ferrus_core::protocol::WinOptions) -> anyhow::Result<()> {
    use ferrus_core::unattend;

    if !opts.any() {
        return Ok(());
    }
    let panther = target_root
        .join("sources")
        .join("$OEM$")
        .join("$$")
        .join("Panther");
    std::fs::create_dir_all(&panther).context("creating $OEM$ Panther directory")?;
    std::fs::write(panther.join("unattend.xml"), unattend::generate(opts))
        .context("writing unattend.xml")?;
    Ok(())
}

/// Scoped mount that unmounts on drop (panic-safe cleanup).
pub struct Mount {
    dir: PathBuf,
}

impl Mount {
    fn new_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ferrus-mnt-{}-{n}-{tag}", std::process::id()))
    }

    /// Read-only loop-mount of an image file.
    pub fn ro(image: &Path) -> anyhow::Result<Self> {
        let dir = Self::new_dir("iso");
        std::fs::create_dir_all(&dir)?;
        run(
            "mount",
            &["-o", "ro,loop", &image.to_string_lossy(), &dir.to_string_lossy()],
            None,
        )
        .with_context(|| format!("loop-mounting {}", image.display()))?;
        Ok(Self { dir })
    }

    /// Mount a freshly formatted partition node (auto FS detection, with an
    /// ntfs-3g FUSE fallback for kernels lacking ntfs3).
    pub fn rw(node: &Path) -> anyhow::Result<Self> {
        let dir = Self::new_dir("tgt");
        std::fs::create_dir_all(&dir)?;
        let n = node.to_string_lossy().into_owned();
        let d = dir.to_string_lossy().into_owned();
        with_retries(
            || {
                if run("mount", &[&n, &d], None).is_ok() {
                    return Ok(());
                }
                run("mount", &["-t", "ntfs-3g", &n, &d], None)
                    .with_context(|| format!("mounting {n}"))
                    .map(drop)
            },
            4,
            600,
        )?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn unmount(self) -> anyhow::Result<()> {
        let d = self.dir.to_string_lossy().into_owned();
        std::mem::forget(self); // take over Drop responsibility
        for attempt in 0..3 {
            if run("umount", &[&d], None).is_ok() {
                let _ = std::fs::remove_dir(&d);
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
        }
        let _ = run("umount", &["-l", &d], None);
        let _ = std::fs::remove_dir(&d);
        bail!("unmount {d}: busy even for lazy unmount")
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = run("umount", &["-l", &self.dir.to_string_lossy()], None);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

pub fn sync_dev(dev: &Path) {
    let _ = run("sync", &["-f", &dev.to_string_lossy()], None);
}

/// Retry an operation a few times — fresh partition nodes are briefly held
/// O_EXCL by udevd's probe, which surfaces as spurious EBUSY.
fn with_retries<T>(
    mut f: impl FnMut() -> anyhow::Result<T>,
    attempts: u32,
    delay_ms: u64,
) -> anyhow::Result<T> {
    let mut last = match f() {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    for _ in 1..attempts {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Locate the vendored uefi:ntfs bootloader image.
pub fn locate_uefi_ntfs_img() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("FERRUS_UEFI_NTFS_IMG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        bail!(
            "FERRUS_UEFI_NTFS_IMG points at {}, which is not a file",
            p.display()
        );
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../../res/uefi-ntfs.img")); // workspace dev tree
            candidates.push(dir.join("../share/ferrus/uefi-ntfs.img")); // deb layout
            candidates.push(dir.join("uefi-ntfs.img"));
        }
    }
    candidates.push(PathBuf::from("/usr/share/ferrus/uefi-ntfs.img"));
    candidates.push(PathBuf::from("/usr/local/share/ferrus/uefi-ntfs.img"));
    candidates
        .into_iter()
        .find(|c| c.is_file())
        .context("uefi-ntfs.img bootloader asset not found")
}

// ---------------------------------------------------------------- copying

#[derive(Debug)]
enum Item {
    Copy { src: PathBuf, rel: PathBuf, size: u64 },
    Split { src: PathBuf, rel: PathBuf, size: u64 },
}

const CHUNK: usize = 1024 * 1024;
const SPLIT_SIZE_MB: &str = "3800"; // Rufus-compatible .swm part size

fn collect_items(
    dir: &Path,
    rel_base: &Path,
    skip_limit: Option<u64>,
    allow_split: bool,
    items: &mut Vec<Item>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("readdir {}", dir.display()))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let rel = rel_base.join(entry.file_name());
        if meta.is_dir() {
            collect_items(&entry.path(), &rel, skip_limit, allow_split, items)?;
        } else if meta.is_file() {
            let size = meta.len();
            let oversize = matches!(skip_limit, Some(l) if size > l);
            if !oversize {
                items.push(Item::Copy {
                    src: entry.path(),
                    rel,
                    size,
                });
            } else {
                let fname = entry.file_name().to_string_lossy().to_ascii_lowercase();
                let splittable = allow_split && (fname.ends_with(".wim") || fname.ends_with(".esd"));
                if splittable {
                    items.push(Item::Split {
                        src: entry.path(),
                        rel,
                        size,
                    });
                } else {
                    bail!(
                        "{} exceeds the FAT32 4 GiB limit and cannot be split",
                        rel.display()
                    );
                }
            }
        }
    }
    Ok(())
}

fn copy_one(src: &Path, dst: &Path, cancel: &AtomicBool) -> anyhow::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    let mut fin = std::fs::File::open(src)
        .with_context(|| format!("open {}", src.display()))?;
    let mut fout = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = fin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        fout.write_all(&buf[..n])?;
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
    }
    fout.sync_all().ok();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_split(
    src: &Path,
    first_swm: &Path,
    size_hint: u64,
    base_done: u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64, &'static str),
    cancel: &AtomicBool,
    pids: &Pids,
) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(first_swm); // stale parts from retries
    // Production keeps Rufus's 3800 MiB part size (plain integer = MiB per
    // wimlib's CLI). When the test knob shrinks the split *threshold*, use
    // 1 MiB parts so tiny fixtures exercise the multi-part path.
    let size_arg = match std::env::var("FERRUS_WIM_SPLIT_LIMIT") {
        Ok(_) => "1".to_string(),
        Err(_) => SPLIT_SIZE_MB.to_string(),
    };
    let err_log = std::env::temp_dir().join("ferrus-wimlib-split.err.log");
    let err_file = std::fs::File::create(&err_log).ok();
    let mut child = Command::new("wimlib-imagex")
        .arg("split")
        .arg(src)
        .arg(first_swm)
        .arg(&size_arg)
        .stdout(Stdio::null())
        .stderr(err_file.map(Stdio::from).unwrap_or(Stdio::null()))
        .spawn()
        .context("spawn wimlib-imagex split")?;
    let pid = child.id();
    pids.lock().unwrap().push(pid);

    loop {
        if cancel.load(Ordering::Relaxed) {
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            let _ = child.wait();
            bail!("cancelled");
        }
        match child.try_wait() {
            Ok(Some(st)) => {
                pids.lock().unwrap().retain(|p| *p != pid);
                if !st.success() {
                    let tail = std::fs::read_to_string(&err_log)
                        .unwrap_or_default()
                        .chars()
                        .rev()
                        .take(300)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<String>();
                    bail!("wimlib-imagex split failed ({st}): {tail}");
                }
                break;
            }
            Ok(None) => {
                // Proxy progress: grown portion of the .swm set. wimlib names
                // continuation parts like MS does: x.swm, x2.swm, x3.swm …
                let mut grown = std::fs::metadata(first_swm).map(|m| m.len()).unwrap_or(0);
                let stem = first_swm
                    .with_extension("")
                    .to_string_lossy()
                    .into_owned();
                for n in 2..10 {
                    let part = PathBuf::from(format!("{stem}{n}.swm"));
                    match std::fs::metadata(&part) {
                        Ok(m) => grown += m.len(),
                        Err(_) => break,
                    }
                }
                progress(base_done + grown.min(size_hint), total, "splitting WIM");
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => {
                pids.lock().unwrap().retain(|p| *p != pid);
                bail!("waiting for wimlib: {e}");
            }
        }
    }
    Ok(())
}

/// Copy `src_root` into `dst_root`, streaming byte progress through `progress`
/// as `(done, total, phase)`. Oversized WIM-family files are split into
/// `.swm` parts when `allow_split`; other oversized files abort the copy.
pub fn copy_tree(
    src_root: &Path,
    dst_root: &Path,
    skip_limit: Option<u64>,
    allow_split: bool,
    cancel: &AtomicBool,
    pids: &Pids,
    progress: &mut dyn FnMut(u64, u64, &'static str),
) -> anyhow::Result<u64> {
    let mut items = Vec::new();
    collect_items(
        src_root,
        Path::new(""),
        skip_limit,
        allow_split,
        &mut items,
    )?;

    let total: u64 = items.iter().map(|i| match i {
        Item::Copy { size, .. } | Item::Split { size, .. } => *size,
    }).sum();

    let mut done: u64 = 0;
    let mut files: u64 = 0;
    for item in &items {
        match item {
            Item::Copy { src, rel, size } => {
                let dst = dst_root.join(rel);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                copy_one(src, &dst, cancel)?;
                done += size;
                files += 1;
                progress(done.min(total), total, "copying files");
            }
            Item::Split { src, rel, size } => {
                let stem = rel.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                let first = dst_root.join(rel).with_file_name(format!("{stem}.swm"));
                if let Some(parent) = first.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                run_split(
                    src,
                    &first,
                    *size,
                    done,
                    total,
                    progress,
                    cancel,
                    pids,
                )?;
                done += size;
                files += 1;
                progress(done.min(total), total, "splitting WIM");
            }
        }
    }

    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    // Flush directories so rename/create metadata reaches the stick.
    if let Ok(d) = std::fs::File::open(dst_root) {
        unsafe {
            libc::fsync(std::os::unix::io::AsRawFd::as_raw_fd(&d));
        }
    }
    Ok(files)
}

// ------------------------------------------------------------- probing

/// Loop-mount `image` read-only and classify its contents.
pub fn probe_manifest(image: &Path) -> anyhow::Result<ImageManifest> {
    let mut manifest = {
        let mnt = Mount::ro(image)?;
        let m = iso::scan_tree(mnt.path()).context("scanning mounted image")?;
        mnt.unmount()?;
        m
    };
    // Tree scans can't see the system area; only the raw file can tell
    // whether a sector copy would boot.
    manifest.hybrid = iso::is_hybrid(image).unwrap_or(true);
    Ok(manifest)
}

// ---------------------------------------------------------- plan execution

fn progress_tick(
    send: &dyn Fn(ferrus_core::protocol::Response),
    phase: &'static str,
) {
    use ferrus_core::protocol::Response;
    send(Response::Progress {
        done: 0,
        total: 0,
        verifying: false,
        phase: Some(phase.into()),
    });
}

/// Execute a complete flashing plan against an already-acquired device.
/// `dev_file` is only used by [`FlashPlan::RawDd`]; the file-based layouts
/// operate on partition nodes derived from `dev`.
#[allow(clippy::too_many_arguments)]
pub fn execute_plan(
    dev: &Path,
    dev_file: &mut std::fs::File,
    plan: &FlashPlan,
    bad_blocks: Option<BadBlocks>,
    cancel: &AtomicBool,
    pids: &Pids,
    send: &dyn Fn(ferrus_core::protocol::Response),
) -> anyhow::Result<String> {
    use ferrus_core::protocol::Response;
    use ferrus_core::write as wengine;

    if let Some(mode) = bad_blocks {
        progress_tick(send, "checking bad blocks");
        let bad = scan_bad_blocks(dev, mode)?;
        if bad > 0 {
            bail!("{bad} bad sectors detected — aborting to protect your data");
        }
        send(Response::Progress {
            done: 0,
            total: 0,
            verifying: false,
            phase: Some("bad-block scan clean".into()),
        });
    }

    match plan {
        FlashPlan::RawDd { verify, persistence_mb, persistence_label, .. } => {
            let outcome = wengine::write_image_to_device(
                plan.image_path().map(Path::new).context("image missing")?,
                dev_file,
                *verify,
                &|p| {
                    send(Response::Progress {
                        done: p.bytes_done,
                        total: p.total,
                        verifying: p.verifying,
                        phase: None,
                    });
                },
                cancel,
            )?;
            match outcome {
                wengine::WriteOutcome::Completed => {
                    let mut msg = if *verify {
                        format!("image written and verified to {}", dev.display())
                    } else {
                        format!("image written to {}", dev.display())
                    };
                    if *persistence_mb > 0 {
                        // Rufus's live-USB slider; label from image probing.
                        let label = persistence_label
                            .clone()
                            .unwrap_or_else(|| "casper-rw".into());
                        add_persistence(dev, *persistence_mb, &label, cancel, pids, send)?;
                        msg.push_str(&format!(
                            " (+{persistence_mb} MiB persistence on {})",
                            part_node(dev, dump_partitions(dev)?.1.len() as u8).display()
                        ));
                    }
                    Ok(msg)
                }
                wengine::WriteOutcome::Cancelled => bail!("cancelled"),
            }
        }

        FlashPlan::FormatDevice { scheme, fs, label, cluster_bytes, old_bios_align } => {
            let kind = fs_kind(fs);
            release_partitions(dev);

            progress_tick(send, "partitioning");
            wipe_signatures(dev)?;
            let mut part = data_part("FERRUS", None, matches!(kind, FsKind::Ext));
            part.mbr_type = match kind {
                FsKind::Fat => "0c", // FAT32 LBA
                FsKind::Ext => "83",
                _ => "07",
            };
            if *scheme == PartitionScheme::Mbr {
                part.mbr_boot = true; // conventional for a plain data stick
                if *old_bios_align {
                    part.mbr_start = Some(63); // Rufus's old-BIOS fix
                }
            }
            partition(dev, *scheme, &[part])?;
            reread_partitions(dev);
            let p1 = part_node(dev, 1);
            wait_for_node(&p1, 10)?;

            progress_tick(send, "formatting");
            mkfs_any(fs, label, &p1, *cluster_bytes)?;

            sync_dev(dev);
            Ok(format!(
                "Formatted {} as {} {}",
                dev.display(),
                fs.to_uppercase(),
                scheme.describe()
            ))
        }

        FlashPlan::WinFat32 { split_wim, scheme, options, .. } => {
            let image = PathBuf::from(plan.image_path().context("image missing")?);

            release_partitions(dev);
            progress_tick(send, "partitioning");
            wipe_signatures(dev)?;
            let mut part = data_part("FERRUS", None, false);
            part.mbr_type = "0c";
            if *scheme == PartitionScheme::Mbr {
                part.mbr_boot = true;
            }
            partition(dev, *scheme, &[part])?;
            reread_partitions(dev);
            let p1 = part_node(dev, 1);
            wait_for_node(&p1, 10)?;

            progress_tick(send, "formatting");
            mkfs_any("FAT32", "FERRUS", &p1, None)?;

            progress_tick(send, "mounting");
            let iso_mnt = Mount::ro(&image)?;
            let tgt = Mount::rw(&p1)?;

            // Test escape hatch: shrink the split threshold so tiny fixture
            // WIMs exercise the wimlib path without 4 GiB payloads.
            let limit = std::env::var("FERRUS_WIM_SPLIT_LIMIT")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(iso::FAT32_MAX_FILE);

            progress_tick(send, "copying files");
            copy_tree(
                iso_mnt.path(),
                tgt.path(),
                Some(limit),
                *split_wim,
                cancel,
                pids,
                &mut |d, t, ph| {
                    send(Response::Progress {
                        done: d,
                        total: t,
                        verifying: false,
                        phase: Some(ph.into()),
                    });
                },
            )?;

            inject_unattend(tgt.path(), options)?;
            if options.any() {
                progress_tick(send, "injected unattend.xml");
            }

            tgt.unmount()?;
            iso_mnt.unmount()?;
            sync_dev(dev);
            Ok(format!(
                "Windows USB ready (FAT32{}, {})",
                if *split_wim { ", split WIM" } else { "" },
                scheme.describe()
            ))
        }

        FlashPlan::WinUefiNtfs { scheme, options, .. } => {
            let image = PathBuf::from(plan.image_path().context("image missing")?);
            let loader = locate_uefi_ntfs_img()?;

            release_partitions(dev);
            progress_tick(send, "partitioning");
            wipe_signatures(dev)?;
            let mut p_boot = data_part("UEFI-NTFS", Some(UEFI_NTFS_PART_SECTORS), false);
            p_boot.mbr_type = "0c";
            let mut p_data = data_part("WININSTALL", None, false);
            p_data.mbr_type = "07";
            partition(dev, *scheme, &[p_boot, p_data])?;
            reread_partitions(dev);
            let p1 = part_node(dev, 1);
            let p2 = part_node(dev, 2);
            wait_for_node(&p1, 10)?;
            wait_for_node(&p2, 10)?;

            progress_tick(send, "writing UEFI:NTFS bootloader");
            dd_to_node(&loader, &p1)?;

            progress_tick(send, "formatting");
            mkfs_any("NTFS", "WININSTALL", &p2, None)?;

            progress_tick(send, "mounting");
            let iso_mnt = Mount::ro(&image)?;
            let tgt = Mount::rw(&p2)?;

            progress_tick(send, "copying files");
            copy_tree(
                iso_mnt.path(),
                tgt.path(),
                None,
                false,
                cancel,
                pids,
                &mut |d, t, ph| {
                    send(Response::Progress {
                        done: d,
                        total: t,
                        verifying: false,
                        phase: Some(ph.into()),
                    });
                },
            )?;

            inject_unattend(tgt.path(), options)?;
            if options.any() {
                progress_tick(send, "injected unattend.xml");
            }

            tgt.unmount()?;
            iso_mnt.unmount()?;
            sync_dev(dev);
            Ok(format!("Windows USB ready (UEFI:NTFS, {})", scheme.describe()))
        }

        FlashPlan::IsoExtract {
            mut scheme,
            syslinux_bios,
            ..
        } => {
            let image = PathBuf::from(plan.image_path().context("image missing")?);
            // BIOS booting via SYSLINUX needs a legacy MBR table (the
            // stock mbr.bin chainloads the active partition; GPT has none).
            let bios = *syslinux_bios;
            if bios && scheme == PartitionScheme::Gpt {
                progress_tick(send, "forcing MBR (SYSLINUX BIOS requirement)");
                scheme = PartitionScheme::Mbr;
            }

            release_partitions(dev);
            progress_tick(send, "partitioning");
            wipe_signatures(dev)?;
            let mut p_live = data_part("LIVE", None, false);
            p_live.mbr_type = "0c";
            p_live.mbr_boot = bios;
            partition(dev, scheme, &[p_live])?;
            reread_partitions(dev);
            let p1 = part_node(dev, 1);
            wait_for_node(&p1, 10)?;

            progress_tick(send, "formatting");
            mkfs_any("FAT32", "LIVE", &p1, None)?;

            progress_tick(send, "mounting");
            let iso_mnt = Mount::ro(&image)?;
            let tgt = Mount::rw(&p1)?;

            progress_tick(send, "copying files");
            copy_tree(
                iso_mnt.path(),
                tgt.path(),
                None,
                false,
                cancel,
                pids,
                &mut |d, t, ph| {
                    send(Response::Progress {
                        done: d,
                        total: t,
                        verifying: false,
                        phase: Some(ph.into()),
                    });
                },
            )?;

            // While still mounted: pick the bootloader directory, drop in
            // ldlinux.c32 and make sure a syslinux.cfg-named config exists.
            let loader_dir = if bios {
                Some(prep_syslinux_tree(tgt.path())?)
            } else {
                None
            };

            tgt.unmount()?;
            iso_mnt.unmount()?;

            if let Some(dir) = loader_dir {
                progress_tick(send, "installing SYSLINUX");
                // Bootsector patcher wants the volume unmounted.
                run("syslinux", &["-i", "-d", &dir, &p1.to_string_lossy()], None)
                    .context("syslinux bootsector install")?;
                write_mbr_code(dev, &locate_syslinux_files()?.1)?;
            }
            sync_dev(dev);
            Ok(format!(
                "Linux USB ready (extracted{}, {})",
                if bios { " + SYSLINUX" } else { "" },
                scheme.describe()
            ))
        }

        FlashPlan::WinToGo {
            wim_index,
            scheme,
            options,
            persist_mib,
            ..
        } => {
            if *scheme == PartitionScheme::Mbr {
                bail!("Windows To Go requires GPT (UEFI boot); pick the GPT scheme");
            }
            let image = PathBuf::from(plan.image_path().context("image missing")?);

            release_partitions(dev);
            progress_tick(send, "partitioning");
            wipe_signatures(dev)?;
            // ESP (FAT32) hosts bootmgfw + BCD; the NTFS partition carries
            // the applied Windows tree and is what actually boots; an
            // optional partition holds user data. The fill-sized WINDOWS
            // entry must come last so sfdisk gives it whatever remains.
            let mut p_esp = data_part("FERRUS", Some(1_048_576), false); // 512 MiB
            p_esp.mbr_type = "0c";
            let mut parts = vec![p_esp];
            if *persist_mib > 0 {
                let mut p_data = data_part("FERRUSDATA", Some(*persist_mib * 2048), false);
                p_data.mbr_type = "07";
                parts.push(p_data);
            }
            let mut p_win = data_part("WINDOWS", None, false);
            p_win.mbr_type = "07";
            let win_n = parts.len() as u8 + 1;
            parts.push(p_win);
            partition(dev, PartitionScheme::Gpt, &parts)?;
            reread_partitions(dev);
            let p_esp_node = part_node(dev, 1);
            let p_win_node = part_node(dev, win_n);
            wait_for_node(&p_esp_node, 10)?;
            wait_for_node(&p_win_node, 10)?;

            progress_tick(send, "formatting");
            mkfs_any("FAT32", "FERRUS", &p_esp_node, None)?;
            if parts.len() > 2 {
                let node = part_node(dev, 2);
                wait_for_node(&node, 10)?;
                mkfs_any("NTFS", "FERRUSDATA", &node, None)?;
            }
            mkfs_any("NTFS", "WINDOWS", &p_win_node, None)?;

            // The BCD device elements reference partitions by their on-disk
            // GPT GUIDs (raw mixed-endian bytes, exactly as stored).
            let disk_guid = gpt_disk_guid(dev_file)?;
            let esp_ref = ferrus_core::bcd::PartitionRef {
                partition_guid: gpt_part_guid(dev_file, 1)?,
                disk_guid,
            };
            let win_ref = ferrus_core::bcd::PartitionRef {
                partition_guid: gpt_part_guid(dev_file, win_n)?,
                disk_guid,
            };

            progress_tick(send, "applying Windows image");
            let iso_mnt = Mount::ro(&image)?;
            let wim = locate_install_wim(iso_mnt.path())?;
            let tgt = Mount::rw(&p_win_node)?;
            wimlib_apply(
                &wim,
                *wim_index,
                tgt.path(),
                cancel,
                pids,
                &mut |d, t, ph| {
                    send(Response::Progress {
                        done: d,
                        total: t,
                        verifying: false,
                        phase: Some(ph.into()),
                    });
                },
            )?;

            if options.any() {
                progress_tick(send, "injecting unattend.xml");
                // A running Windows To Go image reads its answer file from
                // \Windows\Panther\unattend.xml (no setup phase to hook).
                let panther = tgt.path().join("Windows").join("Panther");
                std::fs::create_dir_all(&panther)
                    .context("creating \\Windows\\Panther")?;
                std::fs::write(panther.join("unattend.xml"), ferrus_core::unattend::generate(options))
                    .context("writing Panther unattend.xml")?;
            }

            progress_tick(send, "writing boot files");
            let esp = Mount::rw(&p_esp_node)?;
            let boot_dir = esp.path().join("EFI").join("Microsoft").join("Boot");
            std::fs::create_dir_all(&boot_dir)?;
            std::fs::create_dir_all(esp.path().join("EFI").join("Boot"))?;
            let src_fw = iso_mnt
                .path()
                .join("efi")
                .join("microsoft")
                .join("boot")
                .join("bootmgfw.efi");
            std::fs::copy(&src_fw, boot_dir.join("bootmgfw.efi"))
                .with_context(|| format!("copying {}", src_fw.display()))?;
            // Fallback loader so boards without a NVRAM entry still boot.
            std::fs::copy(&src_fw, esp.path().join("EFI").join("Boot").join("bootx64.efi"))
                .context("copying fallback bootx64.efi")?;

            let entry_guid = new_entry_guid();
            let bcd = ferrus_core::bcd::generate_uefi_bcd(
                &entry_guid,
                "Windows To Go",
                &esp_ref,
                &win_ref,
                10,
            )?;
            std::fs::write(boot_dir.join("BCD"), &bcd).context("writing BCD")?;

            esp.unmount()?;
            tgt.unmount()?;
            iso_mnt.unmount()?;
            sync_dev(dev);
            let storage = if *persist_mib > 0 {
                format!(", +{persist_mib} MiB data")
            } else {
                String::new()
            };
            Ok(format!(
                "Windows To Go ready ({}, entry {entry_guid}{storage})",
                scheme.describe()
            ))
        }

        FlashPlan::WinToGoVhdx {
            vhdx_size_mib,
            wim_index,
            scheme,
            options,
            persist_mib,
            ..
        } => {
            if *scheme == PartitionScheme::Mbr {
                bail!("Windows To Go VHDX requires GPT (UEFI boot); pick the GPT scheme");
            }
            let image = PathBuf::from(plan.image_path().context("image missing")?);

            release_partitions(dev);
            progress_tick(send, "partitioning");
            wipe_signatures(dev)?;
            // ESP (FAT32) hosts bootmgfw + BCD; the NTFS partition carries
            // the VHDX file. The WINDOWS partition comes last so sfdisk gives
            // it whatever remains. Optional trailing data partition for user files.
            let mut p_esp = data_part("FERRUS", Some(1_048_576), false); // 512 MiB
            p_esp.mbr_type = "0c";
            let mut parts = vec![p_esp];
            if *persist_mib > 0 {
                let mut p_data = data_part("FERRUSDATA", Some(*persist_mib * 2048), false);
                p_data.mbr_type = "07";
                parts.push(p_data);
            }
            let mut p_win = data_part("WINDOWS", None, false);
            p_win.mbr_type = "07";
            let win_n = parts.len() as u8 + 1;
            parts.push(p_win);
            partition(dev, PartitionScheme::Gpt, &parts)?;
            reread_partitions(dev);
            let p_esp_node = part_node(dev, 1);
            let p_win_node = part_node(dev, win_n);
            wait_for_node(&p_esp_node, 10)?;
            wait_for_node(&p_win_node, 10)?;

            progress_tick(send, "formatting");
            mkfs_any("FAT32", "FERRUS", &p_esp_node, None)?;
            if *persist_mib > 0 {
                let data_node = part_node(dev, 2);
                wait_for_node(&data_node, 10)?;
                mkfs_any("NTFS", "FERRUSDATA", &data_node, None)?;
            }
            mkfs_any("NTFS", "WINDOWS", &p_win_node, None)?;

            // BCD will reference the WINDOWS partition by its GPT GUID.
            let disk_guid = gpt_disk_guid(dev_file)?;
            let esp_ref = ferrus_core::bcd::PartitionRef {
                partition_guid: gpt_part_guid(dev_file, 1)?,
                disk_guid,
            };
            let win_ref = ferrus_core::bcd::PartitionRef {
                partition_guid: gpt_part_guid(dev_file, win_n)?,
                disk_guid,
            };

            progress_tick(send, "applying Windows image to VHDX");
            let iso_mnt = Mount::ro(&image)?;
            let wim = locate_install_wim(iso_mnt.path())?;

            // Create fixed VHDX on the NTFS partition.
            let vhdx_mib = *vhdx_size_mib;
            let tgt = Mount::rw(&p_win_node)?;
            let vhdx_path = tgt.path().join("windows.vhdx");
            create_fixed_vhdx(&vhdx_path, vhdx_mib, cancel)?;

            // Attach VHDX via loop device, format NTFS inside, apply WIM.
            let loop_dev = attach_vhdx(&vhdx_path)?;
            mkfs_any("NTFS", "WINDOWS", &loop_dev, None)?;
            let vhdx_mnt = Mount::rw(&loop_dev)?;
            wimlib_apply(
                &wim,
                *wim_index,
                vhdx_mnt.path(),
                cancel,
                pids,
                &mut |d, t, ph| {
                    send(Response::Progress {
                        done: d,
                        total: t,
                        verifying: false,
                        phase: Some(ph.into()),
                    });
                },
            )?;

            if options.any() {
                progress_tick(send, "injecting unattend.xml");
                let panther = vhdx_mnt.path().join("Windows").join("Panther");
                std::fs::create_dir_all(&panther)
                    .context("creating \\Windows\\Panther")?;
                std::fs::write(panther.join("unattend.xml"), ferrus_core::unattend::generate(options))
                    .context("writing Panther unattend.xml")?;
            }

            vhdx_mnt.unmount()?;
            detach_vhdx(&loop_dev)?;

            progress_tick(send, "writing boot files");
            let esp = Mount::rw(&p_esp_node)?;
            let boot_dir = esp.path().join("EFI").join("Microsoft").join("Boot");
            std::fs::create_dir_all(&boot_dir)?;
            std::fs::create_dir_all(esp.path().join("EFI").join("Boot"))?;
            let src_fw = iso_mnt
                .path()
                .join("efi")
                .join("microsoft")
                .join("boot")
                .join("bootmgfw.efi");
            std::fs::copy(&src_fw, boot_dir.join("bootmgfw.efi"))
                .with_context(|| format!("copying {}", src_fw.display()))?;
            std::fs::copy(&src_fw, esp.path().join("EFI").join("Boot").join("bootx64.efi"))
                .context("copying fallback bootx64.efi")?;

            let entry_guid = new_entry_guid();
            // VHD device element references the WINDOWS partition GUID + relative path
            let bcd = ferrus_core::bcd::generate_uefi_bcd_vhdx(
                &entry_guid,
                "Windows To Go",
                &esp_ref,
                &win_ref,
                "windows.vhdx",
                10,
            )?;
            std::fs::write(boot_dir.join("BCD"), &bcd).context("writing BCD")?;

            esp.unmount()?;
            iso_mnt.unmount()?;
            sync_dev(dev);
            let storage = if *persist_mib > 0 {
                format!(", +{persist_mib} MiB data")
            } else {
                String::new()
            };
            Ok(format!(
                "Windows To Go (VHDX) ready ({}, entry {entry_guid}, {vhdx_mib} MiB{storage})",
                scheme.describe()
            ))
        }
    }
}

/// Disk GUID from the GPT header (LBA 1, byte offset 56), raw on-disk order.
fn gpt_disk_guid(f: &mut std::fs::File) -> anyhow::Result<[u8; 16]> {
    use std::io::{Seek, SeekFrom};
    f.seek(SeekFrom::Start(512 + 56)).context("seek GPT header")?;
    let mut g = [0u8; 16];
    f.read_exact(&mut g).context("reading disk GUID")?;
    Ok(g)
}

/// Unique partition GUID of GPT partition `n` (1-based) from LBA-2 entries.
fn gpt_part_guid(f: &mut std::fs::File, n: u8) -> anyhow::Result<[u8; 16]> {
    use std::io::{Seek, SeekFrom};
    f.seek(SeekFrom::Start(1024 + ((n as u64) - 1) * 128 + 16))
        .context("seek GPT entry")?;
    let mut g = [0u8; 16];
    f.read_exact(&mut g).context("reading partition GUID")?;
    Ok(g)
}

/// `sources/install.wim`, falling back to `install.esd`.
fn locate_install_wim(iso_root: &Path) -> anyhow::Result<PathBuf> {
    for name in ["install.wim", "install.esd"] {
        let p = iso_root.join("sources").join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    bail!("no sources/install.wim or install.esd in this image")
}

// ------------------------------------------------- SYSLINUX (extract plans)

/// `(ldlinux.c32, mbr.bin)` for BIOS installs, across distro layouts.
fn locate_syslinux_files() -> anyhow::Result<(PathBuf, PathBuf)> {
    const DIRS: [&str; 4] = [
        "/usr/lib/syslinux/bios",      // Arch
        "/usr/lib/syslinux/modules/bios", // Debian syslinux-common
        "/usr/share/syslinux",         // upstream / Fedora
        "/usr/lib/SYSLINUX",           // older Debian
    ];
    let mut ldlinux = None;
    let mut mbr = None;
    for d in DIRS {
        if ldlinux.is_none() {
            let p = PathBuf::from(d).join("ldlinux.c32");
            if p.is_file() {
                ldlinux = Some(p);
            }
        }
        if mbr.is_none() {
            for name in ["mbr.bin", "mbr_br.bin"] {
                let p = PathBuf::from(d).join(name);
                if p.is_file() {
                    mbr = Some(p);
                    break;
                }
            }
        }
    }
    match (ldlinux, mbr) {
        (Some(l), Some(m)) => Ok((l, m)),
        (l, m) => bail!(
            "syslinux files missing (ldlinux.c32: {}, mbr.bin: {}); \
             install the syslinux package",
            l.is_some(),
            m.is_some()
        ),
    }
}

/// Pick the bootloader directory inside the extracted tree, drop
/// `ldlinux.c32` into it and make sure a file named `syslinux.cfg` exists
/// (SYSLINUX looks for that name; ISOLINUX media ship `isolinux.cfg`).
/// Returns the directory path in the form the `-d` option expects.
fn prep_syslinux_tree(root: &Path) -> anyhow::Result<String> {
    use std::path::PathBuf;

    // Preference order mirrors what real ISOs ship.
    let candidates = ["isolinux", "boot/syslinux", "syslinux", "boot/isolinux"];
    let mut chosen: Option<PathBuf> = None;
    for c in candidates {
        let dir = root.join(c);
        if dir.is_dir() {
            chosen = Some(dir);
            break;
        }
    }
    let dir = match chosen {
        Some(d) => d,
        None => {
            // No loader directory at all: create a minimal one so at least
            // the bootsector is functional.
            let d = root.join("isolinux");
            std::fs::create_dir_all(&d).context("creating isolinux dir")?;
            d
        }
    };

    let (ldlinux_c32, _) = locate_syslinux_files()?;
    std::fs::copy(&ldlinux_c32, dir.join("ldlinux.c32"))
        .with_context(|| format!("copying {}", ldlinux_c32.display()))?;

    if !dir.join("syslinux.cfg").exists() {
        if dir.join("isolinux.cfg").exists() {
            std::fs::copy(dir.join("isolinux.cfg"), dir.join("syslinux.cfg"))
                .context("duplicating isolinux.cfg as syslinux.cfg")?;
        } else {
            bail!(
                "no isolinux.cfg/syslinux.cfg under {} — cannot make this ISO BIOS-bootable",
                dir.display()
            );
        }
    }

    let rel = dir.strip_prefix(root).unwrap_or(&dir);
    Ok(format!("/{}", rel.to_string_lossy()))
}

/// Write SYSLINUX's MBR stub (first 440 bytes of code area).
fn write_mbr_code(dev: &Path, mbr_bin: &Path) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let code = std::fs::read(mbr_bin).context("reading mbr.bin")?;
    anyhow::ensure!(code.len() >= 440, "mbr.bin too small ({})", code.len());
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(dev)
        .with_context(|| format!("opening {} for MBR write", dev.display()))?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&code[..440]).context("writing MBR boot code")?;
    Ok(())
}

/// Run `wimlib-imagex apply`, proxying coarse progress. `index` follows the
/// `/index:N` convention; 0 applies the first image in the archive.
fn wimlib_apply(
    wim: &Path,
    index: u32,
    target_root: &Path,
    cancel: &AtomicBool,
    pids: &Pids,
    progress: &mut dyn FnMut(u64, u64, &'static str),
) -> anyhow::Result<()> {
    let err_log = std::env::temp_dir().join("ferrus-wimlib-apply.err.log");
    let err_file = std::fs::File::create(&err_log).ok();
    let mut child = Command::new("wimlib-imagex")
        .arg("apply")
        .arg(wim)
        // wimlib demands an explicit image when the archive holds several;
        // our 0 = "first edition" maps to index 1.
        .arg(index.max(1).to_string())
        .arg(target_root)
        .stdout(Stdio::null())
        .stderr(err_file.map(Stdio::from).unwrap_or(Stdio::null()))
        .spawn()
        .context("spawn wimlib-imagex apply")?;
    let pid = child.id();
    pids.lock().unwrap().push(pid);

    loop {
        if cancel.load(Ordering::Relaxed) {
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            let _ = child.wait();
            bail!("cancelled");
        }
        match child.try_wait() {
            Ok(Some(st)) => {
                pids.lock().unwrap().retain(|p| *p != pid);
                if !st.success() {
                    let tail: String = std::fs::read_to_string(&err_log)
                        .unwrap_or_default()
                        .chars()
                        .rev()
                        .take(300)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    bail!("wimlib-imagex apply failed ({st}): {tail}");
                }
                break;
            }
            Ok(None) => {
                // Indeterminate but alive: report applied-tree growth.
                let grown = dir_size_approx(target_root);
                progress(grown, 0, "applying Windows image");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => {
                pids.lock().unwrap().retain(|p| *p != pid);
                bail!("waiting for wimlib: {e}");
            }
        }
    }
    Ok(())
}

/// Cheap size probe of the applied tree's top level (progress display only).
fn dir_size_approx(root: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(rd) = std::fs::read_dir(root) else {
        return 0;
    };
    for e in rd.flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => continue, // top level only — keeps it cheap
            Ok(_) => total += e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => {}
        }
    }
    total
}

/// Fresh random `{guid}` for the osloader object (lowercase hex).
fn new_entry_guid() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("read /dev/urandom");
    b[6] = (b[6] & 0x0f) | 0x40; // RFC 4122 v4
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{{{}-{}-{}-{}-{}}}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Create a minimal fixed VHDX file (dynamic is not supported by Windows boot).
/// Layout per [MS-VHDX]: File Type Identifier (1 MiB) + Header (1 MiB) +
/// Region Table (variable) + Data Region (logical size) + Footer (512 bytes).
fn create_fixed_vhdx(path: &Path, size_mib: u64, cancel: &AtomicBool) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    use byteorder::{LittleEndian, WriteBytesExt};

    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }

    let logical_bytes = size_mib * 1024 * 1024;
    let sector_size = 512u64;
    let _logical_sectors = logical_bytes / sector_size;

    // File layout:
    // - File Type Identifier: 1 MiB at offset 0 (padded with zeros)
    // - Header: 1 MiB at offset 1 MiB
    // - Region Table: starts at 2 MiB, contains 1 data region entry
    // - Data Region: aligned to 1 MiB after region table
    // - Footer: last 512 bytes of file

    let header_offset = 1024 * 1024; // 1 MiB
let region_table_offset = 2 * 1024u64 * 1024u64; // 2 MiB
let region_table_size = 1024u64 * 1024u64; // 1 MiB (one region entry)
let data_region_offset = (region_table_offset + region_table_size).div_ceil(1048576u64) * 1048576u64;
    let footer_offset = data_region_offset + logical_bytes;
    let file_size = footer_offset + 512;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("creating VHDX {}", path.display()))?;

    // Zero the entire file first (sparse allocation is fine)
    file.set_len(file_size)
        .context("pre-allocating VHDX file")?;

    // 1. File Type Identifier at offset 0 (1 MiB)
    // Signature: "vhdxfile" (8 bytes) + zeros
    file.seek(SeekFrom::Start(0))?;
    file.write_all(b"vhdxfile")?;
    // Rest is already zero from set_len

    // 2. Header at 1 MiB
    file.seek(SeekFrom::Start(header_offset))?;
    // Signature: "vhdxhead" (8 bytes)
    file.write_all(b"vhdxhead")?;
    // Checksum (4 bytes) - computed later, write zeros for now
    let checksum_pos = file.stream_position()?;
    file.write_u32::<LittleEndian>(0)?;
    // Sequence number (8 bytes) - 1
    file.write_u64::<LittleEndian>(1)?;
    // File write GUID (16 bytes)
    let mut write_guid = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut write_guid);
    file.write_all(&write_guid)?;
    // Log file GUID (16 bytes) - all zeros for fixed
    file.write_all(&[0u8; 16])?;
    // Data write GUID (16 bytes)
    let mut data_write_guid = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut data_write_guid);
    file.write_all(&data_write_guid)?;
    // Log version (4 bytes) - 0
    file.write_u32::<LittleEndian>(0)?;
    // Version (4 bytes) - 1
    file.write_u32::<LittleEndian>(1)?;
    // Log length (8 bytes) - 0
    file.write_u64::<LittleEndian>(0)?;
    // Log offset (8 bytes) - 0
    file.write_u64::<LittleEndian>(0)?;

    // Pad header to 1 MiB
    let header_end = file.stream_position()?;
    let pad = header_offset + 1024 * 1024 - header_end;
    if pad > 0 {
        file.write_all(&vec![0u8; pad as usize])?;
    }

    // 3. Region Table at 2 MiB
    file.seek(SeekFrom::Start(region_table_offset))?;
    // Signature: "regt" (4 bytes)
    file.write_all(b"regt")?;
    // Reserved (4 bytes)
    file.write_u32::<LittleEndian>(0)?;
    // Entry count (8 bytes) - 1 data region
    file.write_u64::<LittleEndian>(1)?;

    // Region entry:
    // File offset (8 bytes)
    file.write_u64::<LittleEndian>(data_region_offset)?;
    // Length (8 bytes) - logical size
    file.write_u64::<LittleEndian>(logical_bytes)?;
    // GUID (16 bytes) - data region GUID
    let mut data_region_guid = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut data_region_guid);
    file.write_all(&data_region_guid)?;
    // Reserved (16 bytes)
    file.write_all(&[0u8; 16])?;

    // Pad region table to 1 MiB
    let rt_end = file.stream_position()?;
    let pad = region_table_offset + region_table_size - rt_end;
    if pad > 0 {
        file.write_all(&vec![0u8; pad as usize])?;
    }

    // 4. Data region - already zeroed by set_len, but we need to write
    // the "vhdx" signature at the start of the data region for valid NTFS
    // Actually, the data region is raw disk content; we'll format NTFS on it later

    // 5. Footer at end - 512 bytes
    file.seek(SeekFrom::Start(footer_offset))?;
    // Signature: "vhdxfoot" (8 bytes)
    file.write_all(b"vhdxfoot")?;
    // Checksum (4 bytes) - compute over footer
    let footer_checksum_pos = file.stream_position()?;
    file.write_u32::<LittleEndian>(0)?;
    // Reserved (4 bytes)
    file.write_u32::<LittleEndian>(0)?;
    // Creator info (4 bytes) - "win " = 0x206e6977
    file.write_u32::<LittleEndian>(0x206e6977)?;
    // Creator version (4 bytes) - 0x000a0001 (Windows 10)
    file.write_u32::<LittleEndian>(0x000a0001)?;
    // Creator host OS (4 bytes) - 0x00000004 (Windows)
    file.write_u32::<LittleEndian>(4)?;
    // File size (8 bytes)
    file.write_u64::<LittleEndian>(file_size)?;
    // Data write GUID (16 bytes) - same as header
    file.write_all(&data_write_guid)?;
    // Log GUID (16 bytes) - zeros
    file.write_all(&[0u8; 16])?;
    // Data offset (8 bytes) - data region offset
    file.write_u64::<LittleEndian>(data_region_offset)?;
    // Log offset (8 bytes) - 0
    file.write_u64::<LittleEndian>(0)?;

    // Pad footer to 512 bytes
    let footer_end = file.stream_position()?;
    let pad = footer_offset + 512 - footer_end;
    if pad > 0 {
        file.write_all(&vec![0u8; pad as usize])?;
    }

    // Compute and write header checksum (CRC32 of header, with checksum field zeroed)
    let header_data = std::fs::read(path)?;
    let header_start = header_offset as usize;
    let header_slice = &header_data[header_start..header_start + 1024 * 1024];
    let checksum = crc32_hash(header_slice);
    file.seek(std::io::SeekFrom::Start(checksum_pos))?;
    file.write_u32::<LittleEndian>(checksum)?;

    // Compute and write footer checksum
    let footer_start = footer_offset as usize;
    let footer_data = &header_data[footer_start..footer_start + 512];
    let footer_checksum = crc32_hash(footer_data);
    file.seek(std::io::SeekFrom::Start(footer_checksum_pos))?;
    file.write_u32::<LittleEndian>(footer_checksum)?;

    file.flush()?;
    Ok(())
}

/// Attach VHDX via loop device, return the loop device path (e.g. /dev/loop6).
fn attach_vhdx(vhdx_path: &Path) -> anyhow::Result<PathBuf> {
    let out = run("losetup", &["-f", "--show", "-P", &vhdx_path.to_string_lossy()], None)
        .context("losetup attach VHDX")?;
    let dev = out.trim();
    if dev.is_empty() {
        bail!("losetup returned empty device");
    }
    Ok(PathBuf::from(dev))
}

/// Detach VHDX loop device.
fn detach_vhdx(loop_dev: &Path) -> anyhow::Result<()> {
    run("losetup", &["-d", &loop_dev.to_string_lossy()], None)
        .context("losetup detach VHDX")
        .map(drop)
}