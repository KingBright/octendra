#!/usr/bin/env bash
set -e
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
# Honor an explicit package selection for bounded development checks. With no
# selection, keep the existing full-workspace command used by releases.
for arg in "$@"; do
    case "$arg" in
        --) break ;;
        -p|--package|--package=*|-p?*|--workspace|--all)
            exec cargo test "$@"
            ;;
    esac
done
exec cargo test --workspace "$@"
