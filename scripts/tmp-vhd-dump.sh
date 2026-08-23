#!/bin/bash
python3 /mnt/c/Users/mmjbr/Documents/Ferrus/scripts/tmp-bcddump.py \
  /mnt/c/Users/mmjbr/AppData/Local/Temp/opencode/vhdx-len.bin 2>&1 |
  grep "raw(+4)=" | sed 's/^ *raw(+4)=//' > /tmp/blobs.txt
python3 - <<'PY'
lines = [l.strip() for l in open('/tmp/blobs.txt') if l.strip()]
blobs = [bytes.fromhex(l) for l in lines]
print('total blobs:', len(blobs), 'lens:', [len(b) for b in blobs])

# Device-element candidates: contain a UTF-16 path or exactly the partition shape.
def utf16_at(b, off):
    try:
        t = b[off:].decode('utf-16-le')
        return t.split('\x00')[0] if t and all(c.isprintable() or c == '\x00' for c in t[:40]) else None
    except Exception:
        return None

for i, b in enumerate(blobs):
    # scan for "\\.vhdx" or drive-letter-ish utf16 strings anywhere
    hits = []
    for off in range(0, len(b) - 2, 2):
        if b[off:off+2] == b'\x5c\x00':  # backslash
            s = utf16_at(b, off)
            if s and len(s) > 3 and '.vhdx' in s.lower():
                hits.append((off, s))
    if hits or len(b) in (88,):
        print(f"blob {i}: len={len(b)}")
        print(" ", b.hex())
        for off, s in hits:
            print(f"  utf16@{off:#x}: {s!r}")
PY
