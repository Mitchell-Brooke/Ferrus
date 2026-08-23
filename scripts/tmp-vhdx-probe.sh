#!/bin/bash
set -eu
cd /tmp
qemu-img create -f vhdx -o subformat=fixed,size=64M gt-fixed.vhdx >/dev/null 2>&1
qemu-io -f vhdx -c 'write -P 0xab 0 4k' gt-fixed.vhdx 2>&1
python3 - <<'PY'
data = open('/tmp/gt-fixed.vhdx','rb').read()
pat = b'\xab'*4096
i = data.find(pat)
print('payload found at offset:', hex(i) if i >= 0 else 'NOT FOUND', f'({i} = {i/1048576:.1f} MB)')
# Also scan for ANY nonzero in each MB
for mb in range(0, 10):
    blk = data[mb*1048576:(mb+1)*1048576]
    nz = sum(1 for x in blk if x)
    print(f"  MB{mb}: nonzero={nz}")
PY