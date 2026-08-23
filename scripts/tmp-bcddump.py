#!/usr/bin/env python3
import struct, sys

data = open(sys.argv[1], 'rb').read()
BASE = 0x1000
u32 = lambda o: struct.unpack_from('<I', data, o)[0]
i32 = lambda o: struct.unpack_from('<i', data, o)[0]
u16 = lambda o: struct.unpack_from('<H', data, o)[0]
u64 = lambda o: struct.unpack_from('<Q', data, o)[0]
def F(idx): return idx + BASE

print("== BASE BLOCK ==")
print("sig", data[:4], "seq", u32(4), u32(8), "ts", u64(12))
print("ver", u32(20), u32(24), "type", u32(28), "fmt?", hex(u32(32)), "root_cell", hex(u32(36)))
print("length", hex(u32(40)), "cluster", hex(u32(44)), "filename:", data[48:112].split(b'\0')[0])
print("checksum@0x1fc", hex(u32(0x1FC)))

# hbin headers
print("\n== HBIN CHAIN ==")
h = BASE
while h < len(data):
    if data[h:h+4] != b'hbin':
        print(f"@{hex(h)}: no hbin magic ({data[h:h+4]})"); break
    rel_first = u32(h+4); rel_next = u32(h+8)
    print(f"hbin @{hex(h)} self_rel={hex(rel_first)} next_rel={hex(rel_next)} reserved={hex(u64(h+12))}")
    nxt = rel_next + BASE if rel_next else len(data)
    if rel_next == 0 or nxt <= h: break
    h = nxt

TYPES = {1:'REG_SZ',2:'REG_EXPAND_SZ',3:'REG_BINARY',4:'REG_DWORD',5:'REG_DWORD_BE',7:'REG_MULTI_SZ'}
def name_at(o, n): return data[o:o+n].decode('latin1')

def show_data(dsize_raw, doff, dtype):
    inline = bool(dsize_raw & 0x80000000)
    dsize = dsize_raw & 0x7FFFFFFF
    if inline:
        raw = data[F(doff):F(doff)+dsize]
        print(f"     INLINE raw={raw.hex()}")
        return
    csz = i32(F(doff))
    raw_a = data[F(doff)+4:F(doff)+4+dsize]
    raw_b = data[F(doff):F(doff)+dsize]
    print(f"     datacell={hex(doff)} cellsize={csz}")
    print(f"     raw(+4)={raw_a.hex()}")
    if raw_a.hex() != raw_b.hex(): print(f"     raw(+0)={raw_b.hex()}")
    if dtype in (1,2,7):
        try: print(f"     utf16={raw_a.decode('utf-16-le').rstrip(chr(0))!r}")
        except Exception as e: print('     utf16 err', e)

def dump_key(koff, path):
    assert data[F(koff)+4:F(koff)+6] == b'nk'
    flags   = u16(F(koff)+6)
    ts      = u64(F(koff)+8)
    parent  = u32(F(koff)+20)
    nsub    = u32(F(koff)+24)
    sublist = u32(F(koff)+32)
    nv      = u32(F(koff)+40)
    vlist   = u32(F(koff)+44)
    namelen = u16(F(koff)+76)
    kname   = name_at(F(koff)+80, namelen)
    print(f"KEY @{hex(koff)} {path}/{kname} flags={flags:#06x} nsub={nsub} sublist={hex(sublist)} nval={nv} vlist={hex(vlist)}")
    if nv:
        for i in range(nv):
            voff = u32(F(vlist)+4+4*i)
            sz = i32(F(voff))
            assert data[F(voff)+4:F(voff)+6] == b'vk'
            nlen = u16(F(voff)+6)
            draw = u32(F(voff)+8)
            doff = u32(F(voff)+12)
            dtype = u32(F(voff)+16)
            vflags = u16(F(voff)+20)
            spare = u16(F(voff)+22)
            vname = name_at(F(voff)+24, nlen)
            print(f"  VAL '{vname}' type={TYPES.get(dtype,dtype)} drawsize={draw:#x} vkflags={vflags:#06x} spare={spare} dataoff={hex(doff)} vkcell={sz}")
            show_data(draw, doff, dtype)
    if nsub:
        s = sublist
        lsz = i32(F(s)); sig = data[F(s)+4:F(s)+6]; cnt = u16(F(s)+6)
        print(f"  SUBKLIST {hex(s)} sig={sig} count={cnt} cellsize={lsz}")
        if sig == b'lf':
            for j in range(cnt):
                e = s + 8 + 8*j
                ko = u32(F(e)); hint = name_at(F(e)+4, 4)
                print(f"    -> {hex(ko)} hint={hint!r}")
                dump_key(ko, f"{path}/{kname}")
        elif sig == b'lh':
            for j in range(cnt):
                e = s + 8 + 8*j
                dump_key(u32(F(e)), f"{path}/{kname}")

root = u32(36)
dump_key(root, "")
