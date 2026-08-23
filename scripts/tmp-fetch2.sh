#!/bin/bash
cd /root/.ferrus-ref
for f in _boot.wim_bios_tftpblocksize _x64.wim_and_x86.wim_bios _x64.wim_and_x86.wim_uefi _boot.wim_bios; do
  echo "== $f =="
  curl -sL "http://mistyprojects.co.uk/documents/TinyPXEServer/files/bcd_files/$f/cmd.txt" | grep -iE "partition|ramdisksdi|device" | head -n 8
done
echo "== libbcd0 README =="
cat libbcd0/README.txt
echo "== mappings.py head =="
head -n 60 libbcd0/mappings.py
