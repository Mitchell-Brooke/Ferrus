#!/bin/bash
cd /mnt/c/Users/mmjbr/Documents/Ferrus
export FERRUS_HELPER_PATH=/root/ferrus-target/debug/ferrus-helper
export FERRUS_ALLOW_LOOP=1
/root/ferrus-target/debug/ferrus-helper 2>&1 &
HELPER_PID=$!
sleep 2
/root/ferrus-target/debug/examples/flash-dev /dev/loop7 /tmp/wtg.iso --plan wtg-vhdx --wim-index 1 --no-verify 2>&1
RET=$?
echo "flash-dev exit: $RET"
kill $HELPER_PID 2>/dev/null
wait $HELPER_PID 2>/dev/null
echo "helper exit: $?"