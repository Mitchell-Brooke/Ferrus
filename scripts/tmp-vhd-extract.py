#!/usr/bin/env python3
"""Extract raw device-element blobs from a BCD regf and diff length pairs."""
import struct, sys

path = sys.argv[1]
data = open(path, 'rb').read()
BASE = 0x1000
F = lambda i: i + BASE
u16 = lambda o: struct.unpack_from('<H', data, o)[0]
u32 = lambda o: struct.unpack_from('<I', data, o)[0]


def val_raw(voff):
    vk = F(voff) + 4
    draw = u32(vk + 8); doff = u32(vk + 12)
    if draw & 0x80000000:
        return data[F(doff):F(doff) + (draw & 0x7FFFFFFF)]
    return data[F(doff) + 4:F(doff) + 4 + draw]


def key_at(koff):
    nl = u16(F(koff) + 76)
    name = data[F(koff) + 80:F(koff) + 80 + nl].decode('latin1')
    vals, subs = [], []
    nv = u32(F(koff) + 40); vlist = u32(F(koff) + 44)
    for i in range(nv):
        voff = u32(F(vlist) + 4 + 4 * i)
        vals.append((data[F(voff) + 28:F(voff) + 28 + u16(F(voff) + 6)].decode('latin1'),
                     val_raw(voff)))
    nsub = u32(F(koff) + 24); sublist = u32(F(koff) + 32)
    for j in range(nsub):
        subs.append(key_at(u32(F(sublist) + 8 + 8 * j)))
    return name, vals, subs


objs = {}
def scan(node):
    name, _, subs = node
    if len(name) == 38:
        elems = {}
        stack = [(s, '') for s in subs]
        # element ids live as 8-hex-char subkeys under Elements/
        def collect(n, inelem):
            nm, vs, ss = n
            if inelem and len(nm) == 8 and vs:
                try: elems[int(nm, 16)] = vs[0][1]
                except ValueError: pass
            for s2 in ss:
                collect(s2, inelem or nm == 'Elements')
        for s in subs:
            collect(s, False)
        objs[name] = elems
    for s in subs:
        scan(s)


scan(key_at(u32(36)))
for g, e in sorted(objs.items()):
    for eid in (0x11000001,):
        b = e.get(eid)
        print(f"{g[:8]} {eid:08x} len={len(b)}")
        print(b.hex())
