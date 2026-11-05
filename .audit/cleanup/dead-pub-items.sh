#!/usr/bin/env bash
# List pub items in src/ whose name appears nowhere else in src/, tests/, examples/, or build.rs.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
rg --no-heading -n -o -r '$2' '^\s*pub(\([a-z]+\))?\s+(?:async\s+)?(?:fn|struct|enum|trait|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)' src \
  | while IFS=: read -r file line name; do
      uses=$(rg -w -c --no-filename "$name" src tests examples build.rs 2>/dev/null | awk "{s+=\$1} END {print s+0}")
      if [ "$uses" -le 1 ]; then echo "$file:$line $name"; fi
    done
