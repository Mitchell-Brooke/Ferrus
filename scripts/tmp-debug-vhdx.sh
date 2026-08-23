#!/bin/bash
cd /mnt/c/Users/mmjbr/Documents/Ferrus
export FERRUS_HELPER_PATH=/root/ferrus-target/debug/ferrus-helper
/root/ferrus-target/debug/ferrus-helper 2>&1 &
HELPER_PID=$!
sleep 2
/root/ferrus-target/debug/examples/flash-dev --dry-run /dev/loop7 /tmp/wtg.iso --plan wtg-vhdx --wim-index 1 2>&1
kill $HELPER_PID 2>/dev/null