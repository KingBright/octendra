#!/usr/bin/env bash
set -e
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/rust-target}"
# Honor an explicit package selection for bounded development checks. With no
# selection, keep the existing full-workspace command used by releases.
status=0
selected=0
for arg in "$@"; do
    case "$arg" in
        --) break ;;
        -p|--package|--package=*|-p?*|--workspace|--all)
            selected=1
            break
            ;;
    esac
done
if [ "$selected" -eq 1 ]; then
    cargo test "$@" || status=$?
else
    cargo test --workspace "$@" || status=$?
fi
if [ -x "$HOME/.local/bin/cargo-cache-prune" ]; then
    "$HOME/.local/bin/cargo-cache-prune" "$CARGO_TARGET_DIR" >/dev/null 2>&1 || true
fi
exit "$status"
