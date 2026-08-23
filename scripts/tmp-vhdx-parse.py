#!/usr/bin/env python3
"""Parse a ground-truth VHDX (qemu-img output) and dump every structure."""
import struct, sys, uuid

path = sys.argv[1]
data = open(path, 'rb').read()
u16 = lambda o: struct.unpack_from('<H', data, o)[0]
u32 = lambda o: struct.unpack_from('<I', data, o)[0]
u64 = lambda o: struct.unpack_from('<Q', data, o)[0]
g = lambda o: str(uuid.UUID(bytes_le=data[o:o+16]))

print(f"file len: {len(data)}")
print(f"[0x00000] magic: {data[:8]!r}  creator: {data[8:24]!r}")

def header(off, tag):
    sig = data[off:off+4]
    crc = u32(off+4); seq = u64(off+8)
    fw = g(off+0x10); dw = g(off+0x20); lg = g(off+0x30)
    logver = u16(off+0x40); ver = u16(off+0x42)
    loglen = u32(off+0x44); logoff = u64(off+0x48)
    nzres = sum(1 for x in data[off+0x50:off+0x1000] if x)
    print(f"[{tag} @{off:#x}] sig={sig!r} crc={crc:#x} seq={seq} ver={ver} "
          f"log=({lg}, v{logver}, len={loglen:#x}, off={logoff:#x}) "
          f"fw={fw} dw={dw} nonzero_reserved_bytes={nzres}")
    return seq

s1 = header(0x10000, 'hdr1@64K')
s2 = header(0x20000, 'hdr2@128K')

for rt_off in (0x30000, 0x40000):
    sig = data[rt_off:rt_off+4]; crc = u32(rt_off+4)
    cnt = u32(rt_off+8); res = u32(rt_off+12)
    print(f"[region-table @{rt_off:#x}] sig={sig!r} crc={crc:#x} entries={cnt} reserved={res}")
    for i in range(cnt):
        e = rt_off + 16 + 32*i
        guid = g(e); fo = u64(e+16); ln = u32(e+24); req = u32(e+28)
        print(f"    entry{i}: guid={guid} offset={fo:#x} length={ln:#x} required={req}")
        if guid.startswith('8b7ca206'):
            m = fo
            print(f"    [metadata @{m:#x}] sig={data[m:m+8]!r} reserved={u16(m+8)} count={u16(m+10)}")
            for j in range(u16(m+10)):
                ee = m + 32 + 32*j
                iguid = g(ee); ioff = u32(ee+16); ilen = u32(ee+20)
                bits = u32(ee+24); r2 = u32(ee+28)
                user, isvd, isreq = bits & 1, (bits >> 1) & 1, (bits >> 2) & 1
                print(f"      item: {iguid} off(rel)={ioff:#x} abs={m+ioff:#x} len={ilen} "
                      f"user={user} vd={isvd} req={isreq}")
                val = data[m+ioff:m+ioff+ilen]
                if ilen == 8:  print(f"        value u64={struct.unpack('<Q', val)[0]:#x}")
                elif ilen == 4: print(f"        value u32={struct.unpack('<I', val)[0]:#x}")
                else: print(f"        value {val.hex()}")

# find where payload starts: scan for first nonzero after 0x50000 in MB steps
print("\nnonzero probe per MB:")
for mb in range(1, min(len(data)//1048576, 12)+1):
    blk = data[mb*1048576:(mb+1)*1048576]
    print(f"  MB{mb}: nonzero_bytes={sum(1 for x in blk if x)}")
