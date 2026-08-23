#!/bin/bash
# Ferrus M3 end-to-end tests (run as root inside WSL).
# Self-contained: rebuilds binaries, creates fresh loop devices, exercises
# FormatDevice (GPT/MBR, clusters, labels, bad-block scan) and re-runs the
# WinFat32 split regression through the refactored execute_plan.
set -u
cd /mnt/c/Users/mmjbr/Documents/Ferrus
export CARGO_TARGET_DIR=/root/ferrus-target
LOG=/tmp/m3.log
: > $LOG

echo "== [0] build"
find crates -name '*.rs' -exec touch {} +
cargo build --workspace --examples >>$LOG 2>&1 || { echo BUILD_FAIL; tail -30 $LOG; exit 1; }
# /mnt/c mtimes can fool freshness checks; force the deployables.
cargo build -p ferrus-helper >>$LOG 2>&1
cargo build -p ferrus-core --examples >>$LOG 2>&1
H=$CARGO_TARGET_DIR/debug
EX=$CARGO_TARGET_DIR/debug/examples
export FERRUS_HELPER_PATH=$H/ferrus-helper
export FERRUS_ALLOW_LOOP=1
echo BUILD_OK

echo "== [1] loop rig"
losetup -D >/dev/null 2>&1
umount /mnt/t5 /mnt/t6 /mnt/t7 /mnt/reg /mnt/isom 2>/dev/null
rm -f /tmp/d1.img /tmp/d2.img /tmp/d3.img /tmp/d4.img
truncate -s 256M /tmp/d1.img
truncate -s 256M /tmp/d2.img
truncate -s 64M  /tmp/d3.img
truncate -s 512M /tmp/d4.img
for i in 0 1 2 3; do
  [ -e /dev/loop$i ] || mknod -m 660 /dev/loop$i b 7 $i
done
losetup /dev/loop0 /tmp/d1.img || exit 1
losetup /dev/loop1 /tmp/d2.img || exit 1
losetup /dev/loop2 /tmp/d3.img || exit 1
losetup /dev/loop3 /tmp/d4.img || exit 1
mkdir -p /mnt/t5 /mnt/t6 /mnt/t7 /mnt/reg /mnt/isom

echo "== [T5] GPT FAT32, cluster 16 KiB"
$EX/format-dev /dev/loop0 FAT32 FERRTEST gpt 16384 >>$LOG 2>&1 \
  && echo T5_RUN_OK || { echo T5_FAIL; tail -25 $LOG; exit 1; }
partx -a /dev/loop0 >>$LOG 2>&1
sleep 1
[ -b /dev/loop0p1 ] || { echo T5_NO_PART; tail -25 $LOG; exit 1; }
fsck.vfat -vn /dev/loop0p1 >/tmp/t5.txt 2>&1 && echo T5_FSCK_OK || echo T5_FSCK_FAIL
grep -q "16384 bytes per cluster" /tmp/t5.txt && echo T5_CLUSTER_OK || { echo T5_CLUSTER_BAD; cat /tmp/t5.txt; }
L=$(dosfslabel /dev/loop0p1); [ "$L" = "FERRTEST" ] && echo T5_LABEL_OK || echo "T5_LABEL_BAD:$L"
mount /dev/loop0p1 /mnt/t5 && echo hi > /mnt/t5/probe.txt && sync \
  && grep -q hi /mnt/t5/probe.txt && echo T5_MOUNT_OK || echo T5_MOUNT_FAIL
umount /mnt/t5
sfdisk -d /dev/loop1 >/dev/null 2>&1 # noop warmup

echo "== [T6] MBR NTFS, align63, badblocks fast"
$EX/format-dev /dev/loop1 NTFS FERRNTFS mbr fast align63 >>$LOG 2>&1 \
  && echo T6_RUN_OK || { echo T6_FAIL; tail -25 $LOG; exit 1; }
partx -a /dev/loop1 >>$LOG 2>&1
sleep 1
[ -b /dev/loop1p1 ] || { echo T6_NO_PART; tail -25 $LOG; exit 1; }
sfdisk -d /dev/loop1 >/tmp/t6.txt 2>&1
grep -Eq "start=[[:space:]]*63," /tmp/t6.txt && echo T6_START63_OK || echo T6_START63_BAD
grep -q "bootable" /tmp/t6.txt && echo T6_BOOTFLAG_OK || echo T6_BOOTFLAG_BAD
grep -Eq "(Id|type)=[[:space:]]*0?7," /tmp/t6.txt && echo T6_TYPE07_OK || echo T6_TYPE07_BAD
NL=$(ntfslabel /dev/loop1p1 2>/dev/null); [ "$NL" = "FERRNTFS" ] && echo T6_LABEL_OK || echo "T6_LABEL_BAD:$NL"
mount -t ntfs-3g /dev/loop1p1 /mnt/t6 && echo hi > /mnt/t6/probe.txt && sync \
  && grep -q hi /mnt/t6/probe.txt && echo T6_MOUNT_OK || echo T6_MOUNT_FAIL
umount /mnt/t6

echo "== [T7] GPT ext4, badblocks fast"
$EX/format-dev /dev/loop2 ext4 EXTTEST gpt fast >>$LOG 2>&1 \
  && echo T7_RUN_OK || { echo T7_FAIL; tail -25 $LOG; exit 1; }
partx -a /dev/loop2 >>$LOG 2>&1
sleep 1
VL=$(tune2fs -l /dev/loop2p1 2>/dev/null | grep "volume name" | sed "s/.*://" | tr -d " ")
[ "$VL" = "EXTTEST" ] && echo T7_LABEL_OK || echo "T7_LABEL_BAD:$VL"

echo "== [REG] WinFat32-split regression via refactored execute_plan"
rm -rf /root/e2e/wintree /tmp/wimsrc /root/e2e/win.iso
mkdir -p /root/e2e/wintree/sources /root/e2e/wintree/boot \
         /root/e2e/wintree/efi/microsoft/boot /root/e2e/wintree/efi/boot
mkdir -p /tmp/wimsrc
echo data-a >/tmp/wimsrc/a.txt
dd if=/dev/urandom of=/tmp/wimsrc/blob.bin bs=1M count=3 status=none
wimcapture /tmp/wimsrc /root/e2e/wintree/sources/install.wim --check >>$LOG 2>&1 || { echo FIXTURE_FAIL; exit 1; }
cp /root/e2e/wintree/sources/install.wim /root/e2e/wintree/sources/boot.wim
echo fake-efi-loader >/root/e2e/wintree/efi/microsoft/boot/bootmgfw.efi
echo note >/root/e2e/wintree/autorun.inf
echo x >/root/e2e/wintree/setup.exe
echo x >/root/e2e/wintree/bootmgr
echo x >/root/e2e/wintree/bootmgr.efi
echo x >/root/e2e/wintree/efi/boot/bootx64.efi
xorriso -as mkisofs -quiet -U -J -joliet-long -volid FERRUS_TEST \
  -o /root/e2e/win.iso /root/e2e/wintree >>$LOG 2>&1 || { echo ISO_FAIL; exit 1; }

export FERRUS_WIM_SPLIT_LIMIT=100000
$EX/flash-dev /dev/loop3 /root/e2e/win.iso --plan fat32-split --no-verify \
  >/tmp/reg.out 2>&1 && echo REG_FLASH_OK || { echo REG_FAIL; cat /tmp/reg.out; exit 1; }
unset FERRUS_WIM_SPLIT_LIMIT
partx -a /dev/loop3 >>$LOG 2>&1
sleep 1
mount /dev/loop3p1 /mnt/reg || { echo REG_MOUNT_FAIL; exit 1; }
mount -o loop,ro /root/e2e/win.iso /mnt/isom || { echo REG_ISOMOUNT_FAIL; exit 1; }
[ -f /mnt/reg/sources/install.swm ] && [ -f /mnt/reg/sources/install2.swm ] \
  && echo REG_SWM_OK || echo REG_SWM_BAD
# every non-WIM file must match the ISO byte-for-byte; WIM replaced by .swm parts
FAIL=0
cd /mnt/isom
for f in $(find . -type f ! -name "*.wim"); do
  cmp -s "$f" "/mnt/reg/$f" || { echo "REG_DIFF:$f"; FAIL=1; }
done
[ $FAIL = 0 ] && echo REG_TREE_MATCH || echo REG_TREE_MISMATCH
cd /
umount /mnt/reg /mnt/isom

echo "== cleanup"
losetup -D >/dev/null 2>&1
rm -rf /tmp/wimsrc
cp $LOG /mnt/c/Users/mmjbr/Documents/Ferrus/.tmp-m3log.txt
echo DONE
