#!/usr/bin/env python3
"""Decode VHD vs partition device element blobs from the bcdedit ground truth."""
import struct, uuid

data = open('/mnt/c/Users/mmjbr/AppData/Local/Temp/opencode/vhdx-bcd.bin', 'rb').read()
BASE = 0x1000
F = lambda idx: idx + BASE
u32 = lambda o: struct.unpack_from('<I', data, o)[0]
u64b = lambda b, o: struct.unpack_from('<Q', b, o)[0]


def key_at(koff):
    namelen = u16(F(koff) + 76)
    name = data[F(koff) + 80:F(koff) + 80 + namelen].decode('latin1')
    vals = []
    nv = u32(F(koff) + 40); vlist = u32(F(koff) + 44)
    for i in range(nv):
        voff = u32(F(vlist) + 4 + 4 * i)
        vk = F(voff) + 4
        assert data[vk:vk + 2] == b'vk', data[vk:vk+2]
        nlen = u16(vk + 2)
        vname = data[vk + 20:vk + 20 + nlen].decode('latin1')
        draw = u32(vk + 8); doff = u32(vk + 12)
        if draw & 0x80000000:
            raw = data[F(doff):F(doff) + (draw & 0x7FFFFFFF)]
        else:
            raw = data[F(doff) + 4:F(doff) + 4 + draw]
        vals.append((vname, u32(vk + 16), raw))
    subs = []
    nsub = u32(F(koff) + 24); sublist = u32(F(koff) + 32)
    for j in range(nsub):
        subs.append(key_at(u32(F(sublist) + 8 + 8 * j)))
    return (name, vals, subs)


def collect(node, elems):
    name, vals, _subs = node
    if len(name) == 8 and vals:
        try:
            elems[int(name, 16)] = vals[0][2]
        except ValueError:
            pass
    for s in node[2]:
        collect(s, elems)


objects = {}
def scan(node):
    name, _, subs = node
    if len(name) == 38 and name.startswith('{'):
        elems = {}
        for s in subs:
            collect(s, elems)
        objects[name] = elems
    for s in subs:
        scan(s)


u16 = lambda o: struct.unpack_from('<H', data, o)[0]
scan(key_at(u32(36)))

for guid, elems in objects.items():
    print(f"=== object {guid}")
    for eid in sorted(elems):
        raw = elems[eid]
        print(f"  {eid:08x}  len={len(raw):<4} {raw[:96].hex()}{'...' if len(raw) > 96 else ''}")

print()
for guid, elems in objects.items():
    for eid in (0x11000001, 0x21000001):
        if eid not in elems:
            continue
        b = elems[eid]
        print(f"--- {guid} {eid:08x} ({len(b)}B)")
        print(f"    +00 res : {b[0x00:0x10].hex()}")
        print(f"    +10 type: {u64b(b, 0x10)}")
        print(f"    +18     : {u64b(b, 0x18)}")
        print(f"    +20     : {struct.unpack_from('<I', b, 0x20)[0]:#x} {struct.unpack_from('<I', b, 0x24)[0]:#x}")
        print(f"    +28     : {u64b(b, 0x28)}")
        print(f"    +30..40 : {b[0x30:0x40].hex()}")
        print(f"    +40..60 : {b[0x40:0x60].hex()}")
        print(f"    +60     : {u64b(b, 0x60)}")
        print(f"    +68..88 : {b[0x68:0x88].hex()}")
        print(f"    +80     : {u64b(b, 0x80)}")
        print(f"    +88 guid: {uuid.UUID(bytes_le=b[0x88:0x98])}")
        print(f"    +98     : {b[0x98:0xA0].hex()}")
        print(f"    +A0 guid: {uuid.UUID(bytes_le=b[0xA0:0xB0])}")
        print(f"    +B0     : {b[0xB0:0xB8].hex()}")
        tail = b[0xB8:]
        try:
            txt = tail.decode('utf-16-le').rstrip(chr(0))
            print(f"    +B8 utf16: {txt!r} ({len(tail)}B)")
        except Exception:
            print(f"    +B8 raw : {tail.hex()}")
