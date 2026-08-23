#!/bin/bash
# Ferrus M4 end-to-end tests (run as root inside WSL).
# Extends the M3 suite: live-USB persistence (Ubuntu + Debian flavours)
# and Windows unattend injection, all against fresh loop devices.
set -u
cd /mnt/c/Users/mmjbr/Documents/Ferrus
export CARGO_TARGET_DIR=/root/ferrus-target
LOG=/tmp/m4.log
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
umount /mnt/p1 /mnt/p2 /mnt/uat 2>/dev/null
rm -f /tmp/m4-*.img /root/e2e/*.iso
truncate -s 512M /tmp/m4-a.img
truncate -s 512M /tmp/m4-b.img
truncate -s 256M /tmp/m4-c.img
for i in 4 5 6; do
  [ -e /dev/loop$i ] || mknod -m 660 /dev/loop$i b 7 $i
done
losetup /dev/loop4 /tmp/m4-a.img || exit 1
losetup /dev/loop5 /tmp/m4-b.img || exit 1
losetup /dev/loop6 /tmp/m4-c.img || exit 1
mkdir -p /mnt/p1 /mnt/p2 /mnt/uat

mk_linux_iso () { # $1=out.iso $2=family(ubuntu|debian) [$3=gpt]
  local out=$1 fam=$2 tree=/tmp/m4-tree ap=""
  [ "${3:-}" = gpt ] && ap="--appended_part_as_gpt"
  rm -rf $tree && mkdir -p $tree/.disk
  if [ "$fam" = ubuntu ]; then
    echo 'Ubuntu 24.04 LTS "Noble Numbat" - Release amd64' > $tree/.disk/info
    mkdir -p $tree/casper
    dd if=/dev/zero of=$tree/casper/filesystem.squashfs bs=1M count=2 status=none
  else
    printf 'Debian GNU-Linux 12 "Bookworm" - Official amd64\n' > $tree/.disk/info
    mkdir -p $tree/live
    dd if=/dev/zero of=$tree/live/filesystem.squashfs bs=1M count=2 status=none
  fi
  dd if=/dev/zero of=/tmp/m4-part.bin bs=1M count=4 status=none
  mkfs.vfat -v -n FERRBOOT /tmp/m4-part.bin >>$LOG 2>&1
  xorriso -as mkisofs -quiet -R -J -volid FERRUS_LIVE $ap \
    -append_partition 1 0FC63DAF-8483-4772-8E79-3D69D8477DE4 /tmp/m4-part.bin \
    -o "$out" $tree >>$LOG 2>&1 || { echo "ISO_FAIL($fam)"; tail -20 $LOG; exit 1; }
}

echo "== [T8] Ubuntu live + persistence slider (GPT hybrid)"
mk_linux_iso /root/e2e/ubuntu.iso ubuntu gpt
$EX/flash-dev /dev/loop4 /root/e2e/ubuntu.iso --persist 2048 --no-verify \
  >/tmp/t8.out 2>&1 && echo T8_FLASH_OK || { echo T8_FAIL; cat /tmp/t8.out; exit 1; }
grep -q "persistence" /tmp/t8.out && echo T8_MSG_OK || { echo T8_MSG_BAD; cat /tmp/t8.out; }
partx -a /dev/loop4 >>$LOG 2>&1; sleep 1
[ -b /dev/loop4p1 ] && echo T8_PARTS_OK || { echo T8_NO_PARTS; exit 1; }
sfdisk -d /dev/loop4 >/tmp/t8.txt 2>&1
if grep -q "label: gpt" /tmp/t8.txt && grep -q 'name="FERRUS-PERSIST"' /tmp/t8.txt; then
  echo T8_SFDISK_OK_GPT
elif grep -Eq "label: (dos|gpt)" /tmp/t8.txt && grep -Eq "loop4p[0-9]+ .*type=(83|0FC63DAF)" /tmp/t8.txt; then
  echo T8_SFDISK_OK_MBR
else
  echo T8_SFDISK_BAD; cat /tmp/t8.txt
fi
P=$(sfdisk -d /dev/loop4 | awk '/FERRUS-PERSIST|type=83/ {print $1}' | tail -1 | tr -d ':')
echo "T8_PERSIST_NODE=$P"
L=$(tune2fs -l "$P" 2>/dev/null | grep "volume name" | sed "s/.*://" | tr -d " ")
[ "$L" = "casper-rw" ] && echo T8_LABEL_OK || echo "T8_LABEL_BAD:$L"
mount "$P" /mnt/p1 && echo x > /mnt/p1/w.txt && sync \
  && [ -f /mnt/p1/w.txt ] && echo T8_RW_OK || echo T8_RW_FAIL
umount /mnt/p1

echo "== [T9] Debian live persistence.conf"
mk_linux_iso /root/e2e/debian.iso debian
$EX/flash-dev /dev/loop5 /root/e2e/debian.iso --persist 4096 --no-verify \
  >/tmp/t9.out 2>&1 && echo T9_FLASH_OK || { echo T9_FAIL; cat /tmp/t9.out; exit 1; }
partx -a /dev/loop5 >>$LOG 2>&1; sleep 1
L=$(tune2fs -l /dev/loop5p2 2>/dev/null | grep "volume name" | sed "s/.*://" | tr -d " ")
[ "$L" = "persistence" ] && echo T9_LABEL_OK || echo "T9_LABEL_BAD:$L"
mount /dev/loop5p2 /mnt/p2
[ "$(cat /mnt/p2/persistence.conf 2>/dev/null)" = "/ union" ] \
  && echo T9_CONF_OK || { echo T9_CONF_BAD; ls -la /mnt/p2; }
umount /mnt/p2

echo "== [T10] unattend injection (+ absence without flags)"
rm -rf /root/e2e/wintree /tmp/wimsrc
mkdir -p /root/e2e/wintree/sources /root/e2e/wintree/boot \
         /root/e2e/wintree/efi/microsoft/boot /root/e2e/wintree/efi/boot
mkdir -p /tmp/wimsrc
echo data-a >/tmp/wimsrc/a.txt
wimcapture /tmp/wimsrc /root/e2e/wintree/sources/install.wim --check >>$LOG 2>&1 \
  || { echo FIXTURE_FAIL; exit 1; }
cp /root/e2e/wintree/sources/install.wim /root/e2e/wintree/sources/boot.wim
echo fake-efi-loader >/root/e2e/wintree/efi/microsoft/boot/bootmgfw.efi
echo x >/root/e2e/wintree/setup.exe; echo x >/root/e2e/wintree/bootmgr
echo x >/root/e2e/wintree/efi/boot/bootx64.efi
xorriso -as mkisofs -quiet -U -J -joliet-long -volid FERRUS_WIN \
  -o /root/e2e/win-small.iso /root/e2e/wintree >>$LOG 2>&1 || { echo ISO_FAIL; exit 1; }

# T10a: no options -> no unattend.xml anywhere
$EX/flash-dev /dev/loop6 /root/e2e/win-small.iso --plan fat32 --no-verify \
  >/tmp/t10a.out 2>&1 && echo T10A_FLASH_OK || { echo T10A_FAIL; cat /tmp/t10a.out; exit 1; }
partx -a /dev/loop6 >>$LOG 2>&1; sleep 1
mount /dev/loop6p1 /mnt/uat
U="/mnt/uat/sources/\$OEM\$/\$\$/Panther/unattend.xml"
[ ! -e "$U" ] && echo T10A_ABSENT_OK || echo T10A_UNEXPECTED_FILE
umount /mnt/uat

# T10b: all three switches -> unattend.xml with all bypasses
$EX/flash-dev /dev/loop6 /root/e2e/win-small.iso --plan fat32 --no-verify \
  --opt-hw --opt-account --opt-bitlocker >/tmp/t10b.out 2>&1 \
  && echo T10B_FLASH_OK || { echo T10B_FAIL; cat /tmp/t10b.out; exit 1; }
sleep 1
mount /dev/loop6p1 /mnt/uat
if [ -f "$U" ]; then
  echo T10B_FILE_OK
  grep -q "BypassTPMCheck" "$U" && grep -q "HideOnlineAccountScreens" "$U" \
    && grep -q "PreventDeviceEncryption" "$U" \
    && echo T10B_CONTENT_OK || { echo T10B_CONTENT_BAD; cat "$U"; }
else
  echo T10B_NO_FILE
fi
umount /mnt/uat

echo "== cleanup"
losetup -D >/dev/null 2>&1
rm -rf /tmp/wimsrc /tmp/m4-tree /tmp/m4-part.bin
cp $LOG /mnt/c/Users/mmjbr/Documents/Ferrus/.tmp-m4log.txt
echo DONE
