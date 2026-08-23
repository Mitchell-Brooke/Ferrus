#!/bin/bash
curl -sL "https://api.github.com/repos/pbatard/rufus/git/trees/master?recursive=1" -o /tmp/tree.json
grep -oE '"path": ?"src/[^"]*"' /tmp/tree.json | sed 's/"path": *"//; s/"$//' | grep -v '/' | sort
