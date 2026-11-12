#!/usr/bin/env bash
# Incremental build -> Gateway publication -> automatic Agent upgrades.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 "$ROOT/scripts/ship.py" "$@"
