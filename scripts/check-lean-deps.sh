#!/usr/bin/env bash
# Fails if a normal ArenaNext release dependency graph grows a browser/runtime
# stack that the one-binary distribution contract explicitly excludes.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

dependency_tree="$(cargo tree --locked -p arena-next -e normal --prefix none)"
if printf '%s\n' "$dependency_tree" | rg -i \
  '^(tokio|reqwest|hyper|hyper-util|rustls|tokio-rustls|tauri|wry|webkit|opencv) v'
then
  echo "ArenaNext release dependency policy failed" >&2
  exit 1
fi

echo "ArenaNext release dependency policy passed"
