#!/usr/bin/env python3
import struct

data = open('/root/.ferrus-ref/sample.bcd','rb').read()
BASE = 0x1000
u32 = lambda o: struct.unpack_from('<I', data, o)[0]
u64 = lambda o: struct.unpack_from('<Q', data, o)[0]
u16 = lambda o: struct.unpack_from('<H', data, o)[0]
i32 = lambda o: struct.unpack_from('<i', data, o)[0]

# 1) checksum: XOR of dwords [0..0x1FC) ?
c = 0
for o in range(0, 0x1FC, 4):
    c ^= u32(o)
print(f"xor[0..0x1FC) = {c:#010x}   stored={u32(0x1FC):#010x}  match={c==u32(0x1FC)}")

# 2) hbin chain complete walk (next_rel relative to bin-area start)
print("\nhbins:")
h = BASE
seen = set()
while True:
    rel_self = u32(h+4); rel_next = u32(h+8)
    print(f"  file={h:#06x} self={rel_self:#x} next={rel_next:#x}")
    seen.add(h)
    nxt = h + rel_next
    if nxt in seen or nxt >= len(data) or data[nxt:nxt+4] != b'hbin':
        print(f"  -> stop (nxt={nxt:#x}, magic={data[nxt:nxt+4]})")
        break
    h = nxt

# 3) full nk field dump for root + Description + one Element key
def nk(idx, label):
    R = BASE + idx + 4
    print(f"\nnk {label} @{idx:#x}:")
    print("  size", i32(BASE+idx), "sig", data[R:R+2], "flags", hex(u16(R+2)))
    print("  ts", u64(R+4), "spare/access", hex(u32(R+12)))
    print("  parent", hex(u32(R+16)), "nsub_stable", u32(R+20), "nsub_volatile", u32(R+24))
    print("  subkeys_off", hex(i32(R+28)) if i32(R+28)>0 else i32(R+28), "volatile_off", i32(R+32))
    print("  nval", u32(R+36), "vlist_off", hex(u32(R+40)))
    print("  security", i32(R+44), "class", i32(R+48))
    print("  max_subkey_name", u32(R+52), "max_classname", u32(R+56), "max_valname", u32(R+60), "max_valdata", u32(R+64))
    print("  workvar", u32(R+68), "name_len", u16(R+72), "class_len", u16(R+74))
    print("  name", data[R+76:R+76+u16(R+72)])
nk(0x20, "root(NewStoreRoot)")
nk(0x210, "Description")
nk(0x6b8, "Elements/11000001")

# 4) vk raw dumps (one string-val, one inline-dword val)
def vk(vlist_idx, n, label):
    print(f"\nvlist {label}:")
    for i in range(n):
        vo = u32(BASE+vlist_idx+4+4*i)
        sz = i32(BASE+vo)
        print(f"  vkcell {vo:#x} size {sz}")
        R = BASE+vo+4
        print(f"    sig {data[R:R+2]} namelen {u16(R+2)} drawsize {u32(R+4):#x} doff/type {u32(R+8):#x} type {u32(R+12)} flags {hex(u16(R+16))} spare {u16(R+18)} name {data[R+20:R+20+u16(R+2)]}")
vk(0x2d0, 1, "Description.KeyName")     # REG_SZ
vk(0x170, 1, "bootmgr.Description.Type") # inline dword
vk(0x448, 1, "bootmgr.11000001.Element") # binary blob ref

# 5) lf list cell sizes
for idx,label in [(0x270,"root.lf"), (0x898,"bootmgr.Elems.lf"), (0x1350,"osloader.Elems.lf")]:
    print(f"\nlf {label} @{idx:#x}: size {i32(BASE+idx)} sig {data[BASE+idx+4:BASE+idx+6]} count {u16(BASE+idx+6)}")

# 6) free-cell map of bin area (positive sizes)
print("\ncell walk (bin area, sequential):")
pos = 0x20
while pos < 0x3000 - 8:
    sz = i32(BASE+pos)
    kind = data[BASE+pos+4:BASE+pos+6]
    print(f"  idx {pos:#05x} size {sz:>7} {'ALLOC' if sz<0 else 'free'} sig {kind}")
    if sz == 0: break
    pos += abs(sz)
