#!/usr/bin/env python3
"""Column-diff VHD device blobs across path lengths to find dynamic fields."""
import struct

def blobs_from(path):
    import subprocess
    out = subprocess.run(['python3', '/mnt/c/Users/mmjbr/Documents/Ferrus/scripts/tmp-bcddump.py', path],
                         capture_output=True, text=True).stdout
    res = []
    for line in out.splitlines():
        s = line.strip()
        if s.startswith('raw(+4)='):
            res.append(bytes.fromhex(s[8:]))
    return res

def vhdx_blobs(path):
    """Return device blobs containing '.vhdx' utf16."""
    out = []
    for b in blobs_from(path):
        if b'\x76\x00\x68\x00\x64\x00\x78\x00' in b:  # v h d x utf16
            out.append(b)
    return out

stores = {
    'S(\\w.vhdx)': '/mnt/c/Users/mmjbr/AppData/Local/Temp/opencode/vhdx-len.bin',
    'M(\\ferrus-vhd\\windows.vhdx)': '/mnt/c/Users/mmjbr/AppData/Local/Temp/opencode/vhdx-bcd.bin',
}
parsed = {}
for name, p in stores.items():
    bs = sorted(vhdx_blobs(p), key=len)
    # dedupe device/osdevice pairs (identical except element-id dword)
    uniq = {}
    for b in bs:
        key = b[:0x20] + b[0x28:]  # ignore the 11000001/21000001 marker at +0x24 hi
        # actually marker sits at bytes 0x24..0x27 low? keep simple: dedupe on everything but 0x24
        uniq.setdefault(key, b)
    parsed[name] = list(uniq.values())

names = list(parsed)
print('store blob lens:', {n: [len(b) for b in parsed[n]] for n in names})

b_s = min(parsed[names[0]], key=len)
b_m = min(parsed[names[1]], key=len)

# align from END (paths at tail) and print differing u32/u64 columns
n = min(len(b_s), len(b_m))
print(f"\ncomparing S({len(b_s)}B) vs M({len(b_m)}B), aligned from start up to {n}:")
for off in range(0, n, 4):
    v1 = struct.unpack_from('<I', b_s, off)[0]
    v2 = struct.unpack_from('<I', b_m, off)[0]
    if v1 != v2:
        print(f"  +{off:#05x}: S={v1:<12} M={v2:<12} delta={v2-v1}")

# path facts
for tag, b in (('S', b_s), ('M', b_m)):
    txt = b.decode('utf-16-le', errors='ignore')
    print(f"{tag}: full utf16 scan:", repr(txt))
