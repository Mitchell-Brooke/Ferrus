#!/bin/bash
# Ferrus M5 end-to-end tests (run as root inside WSL).
# Windows To Go: dual-partition GPT layout, wimlib-imagex apply (incl.
# multi-edition --wim-index), ESP boot files + generated BCD whose device
# elements reference the real on-disk GPT GUIDs.
set -u
cd /mnt/c/Users/mmjbr/Documents/Ferrus
export CARGO_TARGET_DIR=/root/ferrus-target
LOG=/tmp/m5.log
: > $LOG

echo "== [0] build"
find crates -name '*.rs' -exec touch {} +
cargo build --workspace --examples >>$LOG 2>&1 || { echo BUILD_FAIL; tail -30 $LOG; exit 1; }
# Belt & braces: WSL/drvfs mtime quirks can leave stale bins behind.
cargo build -p ferrus-helper >>$LOG 2>&1
cargo build -p ferrus-core --examples >>$LOG 2>&1
H=$CARGO_TARGET_DIR/debug
EX=$CARGO_TARGET_DIR/debug/examples
export FERRUS_HELPER_PATH=$H/ferrus-helper
export FERRUS_ALLOW_LOOP=1
echo BUILD_OK

echo "== [1] loop rig"
losetup -D >/dev/null 2>&1
umount /mnt/p1 /mnt/p2 /mnt/p3 2>/dev/null
rm -rf /root/e2e/wintree /root/e2e/win-wtg.iso /tmp/wtgsys1 /tmp/wtgsys2 /tmp/m5-a.img
truncate -s 2600M /tmp/m5-a.img   # 512 MiB ESP + 1 GiB data (T14) + Windows remainder
for i in 7; do
  [ -e /dev/loop$i ] || mknod -m 660 /dev/loop$i b 7 $i
done
losetup /dev/loop7 /tmp/m5-a.img || exit 1
mkdir -p /mnt/p1 /mnt/p2 /mnt/p3

echo "== [T11] fixture: two-edition install.wim + boot files"
mkdir -p /root/e2e/wintree/sources /root/e2e/wintree/efi/microsoft/boot \
         /root/e2e/wintree/efi/boot
mkdir -p /tmp/wtgsys1/Windows/System32 /tmp/wtgsys2/Windows/System32
echo base > /tmp/wtgsys1/Windows/System32/base.dll
echo one > /tmp/wtgsys1/Windows/edition-one.txt
echo base > /tmp/wtgsys2/Windows/System32/base.dll
echo two > /tmp/wtgsys2/Windows/edition-two.txt
wimcapture /tmp/wtgsys1 /root/e2e/wintree/sources/install.wim \
  "Windows 11 Home" "Home test edition" --check >>$LOG 2>&1 \
  || { echo FIXTURE_FAIL; tail -20 $LOG; exit 1; }
wimappend /tmp/wtgsys2 /root/e2e/wintree/sources/install.wim \
  "Windows 11 Pro" --check >>$LOG 2>&1 \
  || { echo FIXTURE_FAIL; tail -20 $LOG; exit 1; }
echo fake-loader >/root/e2e/wintree/efi/microsoft/boot/bootmgfw.efi
echo x >/root/e2e/wintree/efi/boot/bootx64.efi
xorriso -as mkisofs -quiet -U -J -joliet-long -volid FERRUS_WTG \
  -o /root/e2e/win-wtg.iso /root/e2e/wintree >>$LOG 2>&1 || { echo ISO_FAIL; exit 1; }
echo FIXTURE_OK

echo "== [T11b] edition-name probe (xorriso extract + wimlib info)"
$EX/flash-dev --list-editions /root/e2e/win-wtg.iso >/tmp/t11b.out 2>&1 \
  && echo T11B_RUN_OK || { echo T11B_FAIL; cat /tmp/t11b.out; exit 1; }
grep -q "^1: .*Home" /tmp/t11b.out && echo T11B_ONE_OK || { echo T11B_ONE_BAD; cat /tmp/t11b.out; }
grep -q "^2: .*Pro"  /tmp/t11b.out && echo T11B_TWO_OK || echo T11B_TWO_BAD

echo "== [T12] WinToGo flash (auto edition, no options)"
$EX/flash-dev /dev/loop7 /root/e2e/win-wtg.iso --plan wtg --no-verify \
  >/tmp/t12.out 2>&1 && echo T12_FLASH_OK || { echo T12_FAIL; cat /tmp/t12.out; tail -20 $LOG; exit 1; }
grep -q "Windows To Go ready" /tmp/t12.out && echo T12_MSG_OK || { echo T12_MSG_BAD; cat /tmp/t12.out; }
partprobe /dev/loop7 >>$LOG 2>&1; sleep 1
[ -b /dev/loop7p1 ] && [ -b /dev/loop7p2 ] && echo T12_PARTS_OK || { echo T12_NO_PARTS; sfdisk -d /dev/loop7; exit 1; }

mount /dev/loop7p1 /mnt/p1
[ -f /mnt/p1/EFI/Microsoft/Boot/bootmgfw.efi ] \
  && [ -f /mnt/p1/EFI/Microsoft/Boot/BCD ] \
  && [ -f /mnt/p1/EFI/Boot/bootx64.efi ] \
  && echo T12_ESP_OK || { echo T12_ESP_BAD; ls -laR /mnt/p1; umount /mnt/p1; exit 1; }
head -c4 /mnt/p1/EFI/Microsoft/Boot/BCD | grep -q regf \
  && echo T12_BCD_SIG_OK || echo T12_BCD_SIG_BAD
if python3 scripts/tmp-bcddump.py /mnt/p1/EFI/Microsoft/Boot/BCD >>$LOG 2>&1; then
  echo T12_BCD_PARSE_OK
else
  echo T12_BCD_PARSE_BAD; tail -40 $LOG; umount /mnt/p1; exit 1
fi
grep -q "NewStoreRoot" $LOG && echo T12_BCD_ROOT_OK || echo T12_BCD_ROOT_BAD
cp /mnt/p1/EFI/Microsoft/Boot/BCD /tmp/m5-bcd.bin
umount /mnt/p1

mount -t ntfs-3g /dev/loop7p2 /mnt/p2
[ -f /mnt/p2/Windows/System32/base.dll ] \
  && echo T12_APPLY_OK || { echo T12_APPLY_BAD; ls -la /mnt/p2; umount /mnt/p2; exit 1; }
[ -f /mnt/p2/Windows/edition-one.txt ] && echo T12_EDITION_ONE_OK || echo T12_EDITION_ONE_BAD
[ ! -e "/mnt/p2/Windows/Panther/unattend.xml" ] \
  && echo T12_NO_UNATTEND_OK || echo T12_UNEXPECTED_UNATTEND
umount /mnt/p2

# The BCD must reference this disk's real GPT GUIDs (raw byte order).
python3 - <<'PYEOF' && echo T12_BCD_GUIDS_OK || { echo T12_BCD_GUIDS_BAD; exit 1; }
d = open('/dev/loop7','rb')
d.seek(512+56);  disk = d.read(16)
d.seek(1024+16); p1   = d.read(16)
d.seek(1024+128+16); p2 = d.read(16)
bcd = open('/tmp/m5-bcd.bin','rb').read()
print('disk', disk.hex(), 'found:', disk in bcd)
print('p1  ', p1.hex(),   'found:', p1 in bcd)
print('p2  ', p2.hex(),   'found:', p2 in bcd)
raise SystemExit(0 if (disk in bcd and p1 in bcd and p2 in bcd) else 1)
PYEOF

echo "== [T13] WinToGo --wim-index 2 + all unattend switches"
$EX/flash-dev /dev/loop7 /root/e2e/win-wtg.iso --plan wtg --wim-index 2 \
  --no-verify --opt-hw --opt-account --opt-bitlocker >/tmp/t13.out 2>&1 \
  && echo T13_FLASH_OK || { echo T13_FAIL; cat /tmp/t13.out; exit 1; }
sleep 1
mount -t ntfs-3g /dev/loop7p2 /mnt/p2
[ -f /mnt/p2/Windows/edition-two.txt ] && echo T13_EDITION_TWO_OK \
  || { echo T13_EDITION_TWO_BAD; ls -R /mnt/p2 | head -20; umount /mnt/p2; exit 1; }
[ ! -e /mnt/p2/Windows/edition-one.txt ] && echo T13_ONLY_TWO_OK || echo T13_STALE_ONE_BAD
U="/mnt/p2/Windows/Panther/unattend.xml"
if [ -f "$U" ]; then
  echo T13_UNATTEND_FILE_OK
  grep -q "BypassTPMCheck" "$U" && grep -q "HideOnlineAccountScreens" "$U" \
    && grep -q "PreventDeviceEncryption" "$U" \
    && echo T13_UNATTEND_CONTENT_OK || { echo T13_UNATTEND_CONTENT_BAD; cat "$U"; }
else
  echo T13_UNATTEND_MISSING
fi
umount /mnt/p2

echo "== [T14] WinToGo --wtg-persist 1024 (extra data partition)"
$EX/flash-dev /dev/loop7 /root/e2e/win-wtg.iso --plan wtg --wim-index 1 \
  --wtg-persist 1024 --no-verify >/tmp/t14.out 2>&1 \
  && echo T14_FLASH_OK || { echo T14_FAIL; cat /tmp/t14.out; exit 1; }
grep -q "+1024 MiB data" /tmp/t14.out && echo T14_MSG_OK || { echo T14_MSG_BAD; cat /tmp/t14.out; }
partprobe /dev/loop7 >>$LOG 2>&1; sleep 1
[ -b /dev/loop7p2 ] && [ -b /dev/loop7p3 ] \
  && echo T14_PARTS_OK || { echo T14_NO_PARTS; sfdisk -d /dev/loop7; exit 1; }
# Layout: p1 ESP 512 MiB, p2 data exactly 1024 MiB (2097152 sectors), p3 Windows.
sfdisk -d /dev/loop7 2>/dev/null | grep -Eq 'start=.*size= *2097152,' \
  && echo T14_SIZE_OK || { echo T14_SIZE_BAD; sfdisk -d /dev/loop7; }
mount -t ntfs-3g /dev/loop7p2 /mnt/p2
echo persist-test >/mnt/p2/probe.txt
grep -q persist-test /mnt/p2/probe.txt && echo T14_RW_OK || echo T14_RW_BAD
rm -f /mnt/p2/probe.txt
umount /mnt/p2
sync; sleep 1   # let the FUSE unmount release the node before probing
if command -v ntfslabel >/dev/null; then
  L=$(ntfslabel /dev/loop7p2 2>/dev/null | tr -d '\r\n')
  [ "$L" = "FERRUSDATA" ] && echo T14_LABEL_OK || { echo "T14_LABEL_BAD got=[$L]"; }
fi
mount -t ntfs-3g /dev/loop7p3 /mnt/p3
[ -f /mnt/p3/Windows/System32/base.dll ] && [ ! -e /mnt/p3/FERRUSDATA ] \
  && echo T14_WIN_ON_P3_OK || { echo T14_WIN_P3_BAD; ls /mnt/p3; }
umount /mnt/p3

echo "== cleanup"
losetup -D >/dev/null 2>&1
rm -rf /tmp/wtgsys1 /tmp/wtgsys2 /tmp/m5-bcd.bin
cp $LOG /mnt/c/Users/mmjbr/Documents/Ferrus/.tmp-m5log.txt
echo DONE
