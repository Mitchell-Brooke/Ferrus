#!/bin/bash
grep -rn -iE 'bcd|embedded.*regf|createstore' /root/.ferrus-ref/format.c | head -n 20
echo ---
python3 - <<'EOF'
import struct
data=open('/root/.ferrus-ref/sample.bcd','rb').read()
BASE=0x1000
i32=lambda o: struct.unpack_from('<i',data,o)[0]
off=BASE+0x80
sz=i32(off)
print('sk size',sz)
b=data[off+4:off+4-sz]
print(b.hex())
for o in range(0,min(len(b),64),4):
    v=struct.unpack_from('<I',b,o)[0]
    print(f'+{o:#05x}: {v:#010x}')
EOF
