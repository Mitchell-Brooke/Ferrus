import sys
for p in sys.argv[1:]:
    data = open(p, 'rb').read()
    open(p, 'wb').write(data.replace(bytes([13]), b''))
