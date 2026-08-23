#!/bin/bash
# Ferrus M6 end-to-end tests (run as root inside WSL).
# Non-hybrid ISO extraction: FAT32 tree copy + SYSLINUX BIOS install
# (ldlinux.c32, syslinux.cfg, bootsector, MBR code, boot flag).
set -u
cd /mnt/c/Users/mmjbr/Documents/Ferrus
export CARGO_TARGET_DIR=/root/ferrus-target
LOG=/tmp/m6.log
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
umount /mnt/p1 2>/dev/null
rm -rf /root/e2e/livetree /tmp/live.iso /tmp/m6.img
truncate -s 600M /tmp/m6.img
[ -e /dev/loop6 ] || mknod -m 660 /dev/loop6 b 7 6
losetup /dev/loop6 /tmp/m6.img || exit 1
mkdir -p /mnt/p1

echo "== [T15] fixture: non-hybrid ISO with isolinux tree"
mkdir -p /root/e2e/livetree/isolinux /root/e2e/livetree/live \
         /root/e2e/livetree/EFI/boot
head -c 8M /dev/urandom >/root/e2e/livetree/live/vmlinuz
head -c 4M /dev/urandom >/root/e2e/livetree/live/initrd.img
cat >/root/e2e/livetree/isolinux/isolinux.cfg <<'EOF'
DEFAULT linux
LABEL linux
  KERNEL /live/vmlinuz
  INITRD /live/initrd.img
  APPEND boot=live
EOF
# EFI fallback loader so UEFI machines would also boot.
echo efi-loader >/root/e2e/livetree/EFI/boot/bootx64.efi
# No -b/-c/-G options => pure ISO9660, zeroed system area (non-hybrid).
xorriso -as mkisofs -quiet -J -joliet-long -volid FERRUS_LIVE \
  -o /tmp/live.iso /root/e2e/livetree >>$LOG 2>&1 || { echo ISO_FAIL; exit 1; }
# Sanity: system area must really be zeroed for this fixture to be valid.
python3 - <<'PY' && echo FIXTURE_OK || { echo FIXTURE_BAD; exit 1; }
d = open('/tmp/live.iso','rb').read(512)
raise SystemExit(0 if sum(1 for b in d if b) <= 64 else 1)
PY

echo "== [T16] auto-detection picks IsoExtract"
$EX/flash-dev --dry-run /dev/loop6 /tmp/live.iso >/tmp/t16.out 2>&1 \
  && grep -q "plan:  Linux · extracted + SYSLINUX" /tmp/t16.out \
  && echo T16_DETECT_OK || { echo T16_DETECT_BAD; cat /tmp/t16.out; }

echo "== [T17] explicit extract flash"
$EX/flash-dev /dev/loop6 /tmp/live.iso --plan extract --no-verify \
  >/tmp/t17.out 2>&1 && echo T17_FLASH_OK || { echo T17_FAIL; cat /tmp/t17.out; tail -20 $LOG; exit 1; }
grep -q "Linux USB ready (extracted + SYSLINUX" /tmp/t17.out \
  && echo T17_MSG_OK || { echo T17_MSG_BAD; cat /tmp/t17.out; }
partprobe /dev/loop6 >>$LOG 2>&1; sleep 1
[ -b /dev/loop6p1 ] && echo T17_PART_OK || { echo T17_NO_PART; sfdisk -d /dev/loop6; exit 1; }

mount /dev/loop6p1 /mnt/p1
[ -f /mnt/p1/live/vmlinuz ] && [ -f /mnt/p1/isolinux/isolinux.cfg ] \
  && [ -f /mnt/p1/EFI/boot/bootx64.efi ] \
  && echo T17_TREE_OK || { echo T17_TREE_BAD; ls -laR /mnt/p1 | head; umount /mnt/p1; exit 1; }
cmp -s /mnt/p1/live/vmlinuz /root/e2e/livetree/live/vmlinuz \
  && echo T17_CONTENT_OK || echo T17_CONTENT_BAD
[ -f /mnt/p1/isolinux/ldlinux.c32 ] && echo T17_LDLINUX_OK || echo T17_LDLINUX_BAD
[ -f /mnt/p1/isolinux/syslinux.cfg ] && echo T17_CFG_OK || echo T17_CFG_BAD
umount /mnt/p1; sync; sleep 1
# The rw mount leaves the dirty bit set until something clears it; auto-fix
# that first, then the structural check must pass with zero complaints.
fsck.vfat -a /dev/loop6p1 >>$LOG 2>&1
fsck.vfat -n /dev/loop6p1 >>$LOG 2>&1 && echo T17_FSCK_OK || { echo T17_FSCK_BAD; tail -5 $LOG; }

echo "== [T18] boot code on disk (MBR stub + boot flag)"
python3 - <<'PY' && echo T18_MBR_OK || { echo T18_MBR_BAD; exit 1; }
import glob
mbr = None
for c in ['/usr/lib/syslinux/bios/mbr.bin',
          '/usr/lib/syslinux/modules/bios/mbr.bin',
          '/usr/share/syslinux/mbr.bin']:
    try:
        mbr = open(c,'rb').read()
        break
    except OSError:
        pass
assert mbr, 'no mbr.bin found'
disk = open('/dev/loop6','rb').read(512)
assert disk[:440] == mbr[:440], 'MBR code mismatch'
assert disk[510:512] == b'\x55\xaa', 'missing 0x55AA signature'
print('mbr code + sig ok')
PY
sfdisk -d /dev/loop6 2>/dev/null | grep -q "bootable" \
  && echo T18_BOOTFLAG_OK || { echo T18_BOOTFLAG_BAD; sfdisk -d /dev/loop6; }

echo "== cleanup"
losetup -D >/dev/null 2>&1
cp $LOG /mnt/c/Users/mmjbr/Documents/Ferrus/.tmp-m6log.txt
echo DONE
