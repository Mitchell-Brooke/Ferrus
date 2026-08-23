#!/bin/bash
set -eu
cd /tmp
qemu-img create -f vhdx -o subformat=fixed,size=64M gt-fixed.vhdx >/dev/null 2>&1
qemu-img convert -f vhdx -O raw gt-fixed.vhdx gt-fixed.raw 2>&1
ls -l gt-fixed.raw
python3 - <<'PY'
d = open('/tmp/gt-fixed.raw','rb').read()
print('raw len:', len(d))
print('first 4K nonzero:', sum(1 for x in d[:4096] if x))
PY