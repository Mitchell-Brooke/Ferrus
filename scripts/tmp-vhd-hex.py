#!/usr/bin/env python3
"""Side-by-side hexdump of VHD device blobs."""
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
    out = []
    for b in blobs_from(path):
        if b'\x76\x00\x68\x00\x64\x00\x78\x00' in b:
            out.append(b)
    return sorted(out, key=len)

S = vhdx_blobs('/mnt/c/Users/mmjbr/AppData/Local/Temp/opencode/vhdx-len.bin')[0]
M = vhdx_blobs('/mnt/c/Users/mmjbr/AppData/Local/Temp/opencode/vhdx-bcd.bin')[0]

for tag, b in (('S', S), ('M', M)):
    print(f"===== {tag} len={len(b)}")
    for off in range(0, len(b), 16):
        row = b[off:off+16]
        hexs = ' '.join(f'{x:02x}' for x in row)
        asc = ''.join(chr(x) if 32 <= x < 127 else '.' for x in row)
        marks = ''
        if off == 0x10: marks = ' <- type'
        elif off == 0x18: marks = ' <- size?'
        elif off == 0x20: marks = ' <- locate id'
        elif off == 0x88 or off == 0x80: marks = ' <- guid zone?'
        print(f"  {off:#06x}  {hexs:<48} {asc}{marks}")
