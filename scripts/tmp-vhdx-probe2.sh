#!/bin/bash
set -eu
cd /tmp
qemu-img create -f vhdx -o subformat=fixed,size=64M gt-fixed.vhdx >/dev/null 2>&1
echo "before: $(stat -c%s gt-fixed.vhdx)"
qemu-io -f vhdx -c 'write -P 0xab 0 4k' gt-fixed.vhdx 2>&1
echo "after:  $(stat -c%s gt-fixed.vhdx)"
python3 - <<'PY'
data = open('/tmp/gt-fixed.vhdx','rb').read()
pat = b'\xab'*4096
# Find all occurrences
i = 0
while True:
    i = data.find(pat, i)
    if i < 0:
        break
    print(f'  found at {hex(i)} = {i/1048576:.1f} MB')
    i += 1
# Also print last 64KB of file
print('Last 64KB hex:', data[-65536:].hex()[:200])
PY