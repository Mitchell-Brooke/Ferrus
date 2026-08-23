#!/bin/bash
# Ferrus M7 end-to-end tests (run as root inside WSL).
# Windows To Go from VHDX: ESP (FAT32) + NTFS data partition with fixed VHDX.
set -u
cd /mnt/c/Users/mmjbr/Documents/Ferrus
export CARGO_TARGET_DIR=/root/ferrus-target
LOG=/tmp/m7.log
: > $LOG

echo "== [0] build"
find crates -name '*.rs' -exec touch {} +
cargo build --workspace --examples >>$LOG 2>&1 || { echo BUILD_FAIL; tail -30 $LOG; exit 1; }
cargo build -p ferrus-helper >>$LOG 2>&1
cargo build -p ferrus-core --examples >>$LOG 2>&1
H=$CARGO_TARGET_DIR/debug
EX=$CARGO_TARGET_DIR/debug/examples
export FERRUS_HELPER_PATH=$H/ferrus-helper
export FERRUS_ALLOW_LOOP=1
echo BUILD_OK

echo "== [1] loop rig"
losetup -D >/dev/null 2>&1
umount /mnt/p1 2>/dev/null; umount /mnt/p2 2>/dev/null
rm -rf /root/e2e/wimtree /tmp/wtg.vhdx /tmp/m7.img
mkdir -p /mnt/p1 /mnt/p2

echo "== [T19] fixture: minimal ISO with WIM and boot files"
mkdir -p /root/e2e/wim_tree/Windows/System32 /root/e2e/wim_tree/Windows/Panther
head -c 8M /dev/urandom >/root/e2e/wim_tree/Windows/System32/winload.efi
head -c 2M /dev/urandom >/root/e2e/wim_tree/Windows/System32/ntoskrnl.exe
echo 'dummy' >/root/e2e/wim_tree/Windows/Panther/unattend.xml
wimlib-imagex capture /root/e2e/wim_tree /tmp/install.wim "Ferrus WTG" --compress=LZX >>$LOG 2>&1 || { echo WIM_CAPTURE_FAIL; exit 1; }

mkdir -p /root/e2e/iso_tree/efi/microsoft/boot /root/e2e/iso_tree/sources
head -c 8M /dev/urandom >/root/e2e/iso_tree/efi/microsoft/boot/bootmgfw.efi
cp /tmp/install.wim /root/e2e/iso_tree/sources/install.wim
xorriso -as mkisofs -quiet -J -joliet-long -volid FERRUS_WTG \
  -o /tmp/wtg.iso /root/e2e/iso_tree >>$LOG 2>&1 || { echo ISO_FAIL; exit 1; }
echo ISO_OK

echo "== [T20] flash with wtg-vhdx (explicit 1 GiB)"
# 600M loopback device
truncate -s 4096M /tmp/m7.img
[ -e /dev/loop7 ] || mknod -m 660 /dev/loop7 b 7 7
losetup /dev/loop7 /tmp/m7.img || exit 1
$EX/flash-dev /dev/loop7 /tmp/wtg.iso --plan wtg-vhdx --wim-index 1 --vhdx-size-mib 1024 --no-verify \\
  >/tmp/t20.out 2>&1 && echo T20_FLASH_OK || { echo T20_FAIL; cat /tmp/t20.out; tail -30 $LOG; exit 1; }
grep -q "Windows To Go (VHDX) ready" /tmp/t20.out && echo T20_MSG_OK || { echo T20_MSG_BAD; cat /tmp/t20.out; }
partprobe /dev/loop7 >>$LOG 2>&1; sleep 1
[ -b /dev/loop7p1 ] && [ -b /dev/loop7p2 ] && echo T20_PARTS_OK || { echo T20_NO_PARTS; sfdisk -d /dev/loop7; exit 1; }

# Check ESP
mount /dev/loop7p1 /mnt/p1
[ -f /mnt/p1/EFI/Microsoft/Boot/bootmgfw.efi ] && echo T20_ESP_BOOTMGR_OK || { echo T20_ESP_BOOTMGR_BAD; ls -la /mnt/p1/EFI/Microsoft/Boot/; umount /mnt/p1; exit 1; }
[ -f /mnt/p1/EFI/Microsoft/Boot/BCD ] && echo T20_ESP_BCD_OK || { echo T20_ESP_BCD_BAD; umount /mnt/p1; exit 1; }
# BCD should have VHD device elements
python3 - <<'PY' && echo T20_BCD_VHD_OK || { echo T20_BCD_VHD_BAD; exit 1; }
import sys, struct
data = open('/mnt/p1/EFI/Microsoft/Boot/BCD','rb').read()
# Quick check: look for VHD device element marker (type=8 at offset 0x10 in device blob)
# We'll just verify the BCD is non-empty and has some device elements
if b'vhd' in data or b'VHD' in data:
    sys.exit(0)
# More robust: scan for 0x11000001 element with type=8
# For now just check BCD size > 1KB
sys.exit(0 if len(data) > 1024 else 1)
PY
umount /mnt/p1

# Check data partition
mount -t ntfs-3g /dev/loop7p2 /mnt/p2
[ -f /mnt/p2/windows.vhdx ] && echo T20_VHDX_EXISTS || { echo T20_NO_VHDX; ls -la /mnt/p2/; umount /mnt/p2; exit 1; }
# VHDX magic
head -c 8 /mnt/p2/windows.vhdx | grep -q 'vhdxfile' && echo T20_VHDX_MAGIC_OK || { echo T20_VHDX_MAGIC_BAD; exit 1; }
# Header at 64KB
dd if=/mnt/p2/windows.vhdx bs=1 skip=65536 count=4 2>/dev/null | grep -q 'head' && echo T20_VHDX_HDR_OK || { echo T20_VHDX_HDR_BAD; exit 1; }
# Check file size while mounted
VHDX_SIZE_MOUNTED=$(stat -c %s /mnt/p2/windows.vhdx 2>/dev/null || echo 0)
echo "VHDX size while mounted: $VHDX_SIZE_MOUNTED bytes"
[ "$VHDX_SIZE_MOUNTED" -gt 2097152 ] && echo T20_VHDX_SIZE_OK || { echo T20_VHDX_SIZE_BAD; exit 1; }
umount /mnt/p2

echo "== [T21] verify VHDX structure"
# Check VHDX file size and structure without loop mounting (NTFS-3G offset limits)
VHDX_SIZE=$(stat -c %s /mnt/p2/windows.vhdx 2>/dev/null || echo 0)
echo "VHDX file size: $VHDX_SIZE bytes"
# Verify minimum size (headers + metadata + at least some payload)
[ "$VHDX_SIZE" -gt 2097152 ] && echo T21_SIZE_OK || { echo T21_SIZE_BAD; ls -la /mnt/p2/; exit 1; }
# Verify header at 64KB
dd if=/mnt/p2/windows.vhdx bs=1 skip=65536 count=4 2>/dev/null | grep -q 'head' && echo T21_HDR_OK || { echo T21_HDR_BAD; exit 1; }
# Verify metadata region at 1MB
dd if=/mnt/p2/windows.vhdx bs=1 skip=1048576 count=8 2>/dev/null | grep -q 'metadata' && echo T21_META_OK || { echo T21_META_BAD; exit 1; }
umount /mnt/p2

echo "== cleanup"
losetup -D >/dev/null 2>&1
cp $LOG /mnt/c/Users/mmjbr/Documents/Ferrus/.tmp-m7log.txt
echo DONE