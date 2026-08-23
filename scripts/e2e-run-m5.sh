#!/bin/bash
# Converts CRLF and runs the M5 e2e suite from the Windows checkout.
SRC=/mnt/c/Users/mmjbr/Documents/Ferrus/scripts/e2e-m5.sh
perl -i -pe 'BEGIN{$cr=chr(13)} s/$cr$//' "$SRC"
exec bash "$SRC"
