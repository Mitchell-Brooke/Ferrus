#!/bin/bash
curl -sL "https://api.github.com/repos/libyal/documentation/contents/reference" | grep -oE '"name": ?"[^"]+"' | sed -E 's/.*: "(.*)"$/\1/'
