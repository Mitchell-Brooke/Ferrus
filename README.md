# Ferrus

A native Linux bootable-USB creator with Rufus-grade feature coverage.
Rust + GTK4 + libadwaita. GPL-3.0-or-later.

## Status: M7 (Windows To Go from VHDX)

- [x] M0: workspace, udev device enumeration, helper protocol skeleton, Adwaita GUI
- [x] M1: raw DD write engine — 4 MiB chunks, live progress, mid-write cancel,
      byte-exact verify pass (tolerates destination larger than image)
- [x] M2: automatic plan selection (Rufus-style)
  - Linux hybrid ISO → raw DD
  - Windows ISO → GPT + FAT32 with full tree copy
  - oversized WIM/ESD → split into `.swm` parts via `wimlib-imagex`
  - otherwise-oversized files → UEFI:NTFS dual-partition layout
    (16 MiB FAT16 `uefi-ntfs.img` boot partition + NTFS data partition)
- [x] M3:
  - Non-bootable mode: partition + format without an image
    (`FormatDevice` plan; FAT32/NTFS/exFAT/UDF/ext2/3/4 via `mkfs_any`)
  - Partition scheme matrix: GPT **and** MBR for every Windows layout
    (MBR Windows = active type-0c FAT32; old-BIOS fix starts at sector 63)
  - Volume labels (per-FS sanitising) and cluster-size selection
  - Bad-block scan before flashing (`badblocks -sv` fast /
    `-wsv` destructive), abort on any defect
  - GUI consumes scheme/fs/label/cluster/bad-blocks controls;
    explicit "NTFS" choice upgrades a Windows ISO to the UEFI:NTFS layout
  - pkexec-based privileged helper launch with polkit policy
  - Debian packaging without dpkg-deb (`pack/build-deb.sh`)
- [x] M4:
  - Live-USB persistence (Rufus's slider): after a raw-DD write, appends an
    ext4 partition in the remaining space (`sfdisk --append`, GPT or MBR),
    labelled `casper-rw` (Ubuntu family) or `persistence` + `persistence.conf`
    ("/ union") for Debian family; label auto-detected from the ISO
    (`.disk/info` + `casper/` vs `live/` trees); size clamped to free space,
    1 MiB aligned, min 2 MiB
  - Windows user-experience bypasses via generated `unattend.xml`
    (`sources/$OEM$/$$/Panther/`): hardware-check bypass (TPM/SecureBoot/RAM/
    CPU/disk in windowsPE via LabConfig), local-account OOBE
    (HideOnlineAccountScreens + BypassNRO-style reg keys), BitLocker
    auto-encryption off (PreventDeviceEncryption); dropped only when at least
    one option is set; legacy helper JSON still accepted (serde defaults)
  - GUI: persistence SpinRow (visible for live images), "Windows user
    experience" switch group (visible for Windows ISOs), confirmation dialog
    lists persistence size and enabled tweaks
  - flash-dev example: `--persist <MiB>`, `--opt-hw`, `--opt-account`,
    `--opt-bitlocker`; FERRUS_PERSIST_LABEL env removed (auto-detect instead)
- [x] End-to-end validated on loop devices: raw-DD byte compare, FAT32 tree
      diff, multi-part WIM split (`install.swm`, `install2.swm`, …),
      UEFI:NTFS loader bytes + NTFS tree diff, format matrix
      (GPT/vfat/cluster size, MBR/NTFS/boot flag/sector 63, ext4 label),
      Ubuntu+Debian persistence partitions (label, RW, persistence.conf),
      unattend injection content and absence-without-flags
- [x] M5:
  - Windows To Go (`WinToGo` plan): boot the full installed Windows desktop
    from the stick. GPT dual-partition layout — 512 MiB FAT32 ESP
    (`EFI/Microsoft/Boot/{bootmgfw.efi,BCD}` + `EFI/Boot/bootx64.efi`
    fallback) plus an NTFS partition carrying a `wimlib-imagex apply`-ed
    Windows tree; `--wim-index N` selects the edition (0 = first)
  - Native BCD generation (`ferrus-core::bcd`): pure-Rust registry-hive
    writer (regf v1.3, hbin/nk/vk/lf/sk records, XOR base-block checksum,
    GPT `PartitionDevice` elements referencing the real on-disk disk +
    partition GUIDs read from the partition table). Byte-level layout was
    reverse-engineered from genuine bcdedit-created stores; output is
    accepted by `bcdedit /store` itself and by systemd's parser rules
  - Answer file lands in `\Windows\Panther\unattend.xml` of the applied
    image (no setup phase), same three bypass switches as install media
  - Optional trailing data partition (`--wtg-persist MiB`) for user files
  - Edition picker reads real image names out of `install.wim`/`install.esd`
    (`wimlib-imagex info` after an unprivileged `xorriso -osirrox` stream
    extraction); falls back to a plain selector when tools are missing
  - GUI: "Windows To Go" switch + named edition combo + storage SpinRow in
    the Windows group (forces GPT); flash-dev: `--plan wtg [--wim-index N]
    [--wtg-persist MiB]`, `--list-editions <iso>`
- [x] M6:
  - Non-hybrid Linux ISO extraction (`IsoExtract` plan): Rufus-style tree
    copy onto a single FAT32 partition instead of sector-copying; SYSLINUX
    BIOS install (ldlinux.c32, syslinux.cfg, bootsector, MBR stub) when
    requested; auto-detected when an ISO carries no MBR boot code (sector-0
    heuristic) and has an isolinux/ tree; falls back to raw-DD for hybrids
  - SYSLINUX installer: locates `ldlinux.c32` and `mbr.bin` across distro
    layouts (Arch, Debian, Fedora, upstream), duplicates `isolinux.cfg` as
    `syslinux.cfg`, writes MBR code (first 440 bytes), sets boot flag; GPT
    coerced to MBR for BIOS installs
  - GUI: no changes needed (falls through the `_` dynamic-row branch)
  - flash-dev: `--plan extract`, `--dry-run`, hybrid field in probe output
- [x] M7:
  - Windows To Go from VHDX (`WinToGoVhdx` plan): ESP (FAT32) hosts
    `bootmgfw.efi` + BCD; the NTFS partition carries a fixed VHDX file
    containing the full Windows installation (applied via `wimlib-imagex`)
  - Native VHDX writer (`ferrus-core::ops::create_fixed_vhdx`): creates
    minimal fixed VHDX per MS-VHDX spec (File Type Identifier, Header,
    Region Table, Data Region, Footer with CRC32 checksums); attaches via
    `losetup -P`, formats inner NTFS, applies WIM, detaches
  - BCD VHD device elements (type 8): `device` (0x11000001) and `osdevice`
    (0x21000001) reference the WINDOWS partition GUID + relative path
    `\windows.vhdx`; byte-level layout reverse-engineered from real
    `bcdedit /set device vhd=[C:]\path.vhdx` stores
  - GUI: "Windows To Go (VHDX)" switch + VHDX size SpinRow in the Windows
    group (forces GPT); flash-dev: `--plan wtg-vhdx [--wim-index N]
    [--vhdx-size MiB]`
- [x] CI: GitHub Actions — unit tests + clippy on every push, and on tag
      pushes an automatic release job building the `.deb` and attaching it
- [x] Packaging: Debian `.deb`, RPM `.rpm`, Arch `PKGBUILD`, Flatpak manifest,
      AppStream metainfo, man page
- [ ] M8+: persistence for WTG, remaining Rufus features

## Layout

```
crates/ferrus-core     protocol, udev enumeration, iso probing, client, DD engine
crates/ferrus-helper   privileged ops (sfdisk/mkfs/wimlib/dd); JSON on stdio
crates/ferrus-gui      GTK4/libadwaita front-end ("ferrus" binary)
res/                   uefi-ntfs.img bootloader asset
pack/                  deb packaging: control, polkit policy, desktop file, builder
scripts/               e2e test suites (loop-device rigs)
```

### Environment knobs (testing)

| Variable                 | Effect                                                                      |
| ------------------------ | --------------------------------------------------------------------------- |
| `FERRUS_HELPER_PATH`     | helper binary location (else sibling of the client exe)                     |
| `FERRUS_ALLOW_LOOP=1`    | permit loop devices as flash targets (test rigs)                            |
| `FERRUS_UEFI_NTFS_IMG`   | override path of the uefi:ntfs bootloader image                             |                                                              |
| `FERRUS_WIM_SPLIT_LIMIT` | shrink the WIM split threshold in bytes (tests);                            |
                           | also forces 1 MiB `.swm` parts so micro fixtures exercise multi-part splits |
| `FERRUS_NO_PKEXEC=1`     | run the helper directly even when not root                                  |

### Headless tools

```sh
cargo run -p ferrus-core --example flash-dev -- /dev/sdX image.iso [--no-verify]
# force a layout: --plan raw|fat32|fat32-split|ntfs
# live-USB persistence:  --persist 4096            (MiB; raw-DD plans)
# Windows tweaks:        --opt-hw --opt-account --opt-bitlocker

cargo run -p ferrus-core --example format-dev -- /dev/sdX FAT32 MYLABEL \
  gpt 16384 fast          # fs label scheme cluster badblocks align63
```

### Packaging

```sh
cargo build --workspace --release
CARGO_TARGET_DIR=target pack/build-deb.sh        # → ferrus_0.3.0_amd64.deb
```

Installs `/usr/bin/ferrus`, `/usr/libexec/ferrus/ferrus-helper`,
`/usr/share/ferrus/uefi-ntfs.img`, desktop entry and polkit action
(`com.ferrus.ferrus.run-helper`, auth_admin_keep).

Additional formats:
- RPM: `pack/ferrus.spec` (use `rpmbuild -ta ferrus-0.3.0.tar.gz`)
- Arch: `pack/PKGBUILD` (use `makepkg -si`)
- Flatpak: `pack/io.github.Mitchell-Brooke.Ferrus.yml`
- AppStream: `pack/io.github.Mitchell-Brooke.Ferrus.metainfo.xml`
- Man page: `pack/ferrus.1`

### res/uefi-ntfs.img provenance

Built from the signed EFI loaders published by pbatard/uefi-ntfs v2.8
(`bootx64_signed.efi` → `\EFI\BOOT\BOOTX64.EFI`, plus `bootaa64.efi`) packed
into a 16 MiB FAT16 image created with dosfstools. GPLv3, same as Rufus's.

## Build (Debian 12/13 or Arch)

```sh
sudo apt install build-essential pkg-config rust cargo \
  libgtk-4-dev libadwaita-1-dev libudev-dev   # Debian names
# Arch equivalents: sudo pacman -S --needed rust pkgconf gtk4 libadwaita udev

cargo build --workspace
./target/debug/ferrus            # GUI
sudo ./target/debug/ferrus-helper # or let the GUI elevate it via pkexec
cargo test -p ferrus-core

# runtime deps for all filesystem layouts (Debian names):
sudo apt install dosfstools ntfs-3g wimtools exfatprogs udftools e2fsprogs
```

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
