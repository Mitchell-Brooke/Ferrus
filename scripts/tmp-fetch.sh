#!/bin/bash
cd /tmp/rufus-ref
for f in drive.c vhd.c wue.c usb.c; do
  curl -sL "https://raw.githubusercontent.com/pbatard/rufus/master/src/$f" -o "$f"
done
ls -la *.c
echo '--- definitions ---'
grep -n 'SetupWinToGo\|CreateBCD\|bcdelem\|BcdElem' *.c | head -40
