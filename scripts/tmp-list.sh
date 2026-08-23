#!/bin/bash
tr -d '\r' < /dev/null
grep -oE '"path": ?"[^"]+"' /tmp/tree.json | sed -E 's/.*"([^"]+)"$/\1/' | grep '^src/' | grep -v '/'
