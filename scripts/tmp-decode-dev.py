#!/usr/bin/env python3
import struct, sys

def load(path):
    return open(path,'rb').read()

data = load('/root/.ferrus-ref/sample.bcd')
BASE = 0x1000
u32 = lambda o: struct.unpack_from('<I', data, o)[0]
u64 = lambda o: struct.unpack_from('<Q', data, o)[0]

# collect (name, blob) pairs manually located from prior dump:
# {bootmgr}.11000001 cell idx data=0x748 size 0x58 ; osloader dev/osdev idx 0x1390/0x11b0 & uefi 0x2020/0x1dd0 size 0xAA
def blob(idx, size):
    off = BASE + idx + 4
    return data[off:off+size]

blobs = [
    ("bootmgr.device(boot)", blob(0x748, 0x58)),
    ("bios.dev",  blob(0x1390, 0xAA)),
    ("bios.osdev",blob(0x11b0, 0xAA)),
    ("uefi.dev",  blob(0x2020, 0xAA)),
    ("uefi.osdev",blob(0x1dd0, 0xAA)),
]

# libbcd0 win7 partition element (typed from README)
part = bytes([
0,0,0,0, 0,0,0,0, 0,0,0,0, 0,0,0,0,
6,0,0,0, 0,0,0,0,
0x48,0,0,0, 0,0,0,0,
0,0,0x50,6, 0,0,0,0, 0,0,0,0, 0,0,0,0,
0,0,0,0,
1,0,0,0,
0x11,0xfb,0xac,0xfd, 0,0,0,0, 0,0,0,0, 0,0,0,0,
0,0,0,0, 0,0,0,0, 0,0,0,0, 0,0,0,0])
blobs.append(("win7.partition(C:)", part))

for name, b in blobs:
    print(f"\n=== {name} len={len(b)} (0x{len(b):x}) ===")
    n = len(b)//4
    for i in range(n):
        v = struct.unpack_from('<I', b, i*4)[0]
        if v != 0:
            print(f"  +0x{i*4:02x}: {v:#010x} ({v})")
    # utf16 strings anywhere?
    s = b.decode('utf-16-le', errors='ignore')
    printable = ''.join(c if c.isprintable() else '.' for c in s)
    if any(c != '.' for c in printable):
        print(f"  utf16-ish: {printable}")
