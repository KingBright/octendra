#!/usr/bin/env bash
# Zero-ceremony single-command publisher for Octendra Code.
# Builds all 4 desktop binaries incrementally, publishes to the Service Gateway,
# and lets every online/offline Agent self-upgrade automatically via /device/poll.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/rust-target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-12}"
export OCTENDRA_BUILD_REVISION="${OCTENDRA_BUILD_REVISION:-$(git rev-parse HEAD)}"
GATEWAY_SSH_HOST="${OCTENDRA_GATEWAY_SSH_HOST:?Set the authorized Gateway SSH destination}"
GATEWAY_SSH_PORT="${OCTENDRA_GATEWAY_SSH_PORT:-222}"
GATEWAY_BIN_ROOT="${OCTENDRA_GATEWAY_BIN_ROOT:?Set the installed Gateway binary directory}"
GATEWAY_CFG="${OCTENDRA_GATEWAY_CFG:?Set the installed Gateway configuration path}"
GATEWAY_SERVICE="${OCTENDRA_GATEWAY_SERVICE:-octendra-gateway.service}"

VERSION="$(python3 -c 'import re,pathlib; t=pathlib.Path("crates/octendra-code/Cargo.toml").read_text(); print(re.search(r"^version\s*=\s*\"([^\"]+)\"", t, re.M).group(1))')"
echo "==> Building Octendra Code v${VERSION} (rev ${OCTENDRA_BUILD_REVISION:0:12})"

ulimit -n 65536 || true
RUST_SYSROOT="$(rustc --print sysroot)"
RUST_LLD="$(find "$RUST_SYSROOT" -name rust-lld -type f | head -n 1)"

cargo build -p octendra-code --release --locked
cargo zigbuild -p octendra-code --release --locked --target x86_64-unknown-linux-musl
cargo zigbuild -p octendra-code --release --locked --target aarch64-unknown-linux-musl \
  --config "target.aarch64-unknown-linux-musl.linker=\"$RUST_LLD\"" \
  --config 'target.aarch64-unknown-linux-musl.rustflags=["-C","link-arg=--fix-cortex-a53-843419"]'
cargo xwin build -p octendra-code --release --locked --target x86_64-pc-windows-msvc

STAGE="$(mktemp -d "/tmp/octendra-ship-${VERSION}-XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

cp "$CARGO_TARGET_DIR/release/octendra-code" "$STAGE/octendra-code-macos-arm64"
cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/octendra-code" "$STAGE/octendra-code-linux-amd64"
cp "$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/octendra-code" "$STAGE/octendra-code-linux-arm64"
cp "$CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/octendra-code.exe" "$STAGE/octendra-code-windows-amd64.exe"
chmod 0755 "$STAGE"/octendra-code-*

python3 - "$STAGE" <<'PY'
import hashlib, json, pathlib, subprocess, sys
stage = pathlib.Path(sys.argv[1])
manifest = json.loads(subprocess.check_output([str(stage / "octendra-code-macos-arm64"), "release-manifest"]))
names = [
    "octendra-code-macos-arm64",
    "octendra-code-linux-amd64",
    "octendra-code-linux-arm64",
    "octendra-code-windows-amd64.exe",
]
artifacts = {}
for name in names:
    p = stage / name
    data = p.read_bytes()
    artifacts[name] = {"sha256": hashlib.sha256(data).hexdigest(), "size": len(data)}
manifest["artifacts"] = artifacts
(stage / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
(stage / "SHA256SUMS").write_text("".join("%s  %s\n" % (v["sha256"], k) for k, v in artifacts.items()))
PY

GW_SHA="$(python3 -c 'import json,sys,pathlib; print(json.loads((pathlib.Path(sys.argv[1])/"manifest.json").read_text())["artifacts"]["octendra-code-linux-amd64"]["sha256"])' "$STAGE")"
echo "==> Streaming v${VERSION} release artifacts to Gateway (${GATEWAY_SSH_HOST}:${GATEWAY_SSH_PORT})"

COPYFILE_DISABLE=1 tar -C "$STAGE" -czf - . | ssh -p "$GATEWAY_SSH_PORT" -o BatchMode=yes "$GATEWAY_SSH_HOST" \
  "set -euo pipefail
   REL_ROOT='${GATEWAY_BIN_ROOT}/releases'
   REL_DIR=\"\${REL_ROOT}/${VERSION}\"
   rm -rf \"\${REL_DIR}.staging\"
   mkdir -p \"\${REL_DIR}.staging\" \"\${REL_ROOT}/rollback\"
   tar -C \"\${REL_DIR}.staging\" -xzf -
   \"\${REL_DIR}.staging/octendra-code-linux-amd64\" check --config '${GATEWAY_CFG}'
   rm -rf \"\${REL_DIR}\"
   mv \"\${REL_DIR}.staging\" \"\${REL_DIR}\"
   cp -f '${GATEWAY_BIN_ROOT}/octendra-code' \"\${REL_ROOT}/rollback/octendra-code\" 2>/dev/null || true
   nohup sh -c '
     sleep 1
     cp -f \"${GATEWAY_BIN_ROOT}/releases/${VERSION}/octendra-code-linux-amd64\" \"${GATEWAY_BIN_ROOT}/octendra-code.new\"
     chown 18787:18787 \"${GATEWAY_BIN_ROOT}/octendra-code.new\"
     chmod 0755 \"${GATEWAY_BIN_ROOT}/octendra-code.new\"
     mv -f \"${GATEWAY_BIN_ROOT}/octendra-code.new\" \"${GATEWAY_BIN_ROOT}/octendra-code\"
     printf \"{\\\"version\\\":\\\"${VERSION}\\\",\\\"installed_sha256\\\":\\\"${GW_SHA}\\\",\\\"updated_at\\\":%s}\\n\" \"\$(date +%s)\" > \"${GATEWAY_BIN_ROOT}/releases/current.json\"
     chmod 0755 \"${GATEWAY_BIN_ROOT}\" \"${GATEWAY_BIN_ROOT}/releases\" \"${GATEWAY_BIN_ROOT}/releases/${VERSION}\"
     chown -R 18787:18787 \"${GATEWAY_BIN_ROOT}/releases/${VERSION}\" \"${GATEWAY_BIN_ROOT}/releases/current.json\"
     chmod 0644 \"${GATEWAY_BIN_ROOT}/releases/current.json\" \"${GATEWAY_BIN_ROOT}/releases/${VERSION}/manifest.json\" \"${GATEWAY_BIN_ROOT}/releases/${VERSION}/SHA256SUMS\"
     chmod 0755 \"${GATEWAY_BIN_ROOT}/releases/${VERSION}\"/octendra-code-*
     systemctl restart \"${GATEWAY_SERVICE}\"
     for d in \"${GATEWAY_BIN_ROOT}/releases\"/*; do
       b=\$(basename \"\$d\")
       if [ \"\$b\" != \"${VERSION}\" ] && [ \"\$b\" != \"rollback\" ] && [ \"\$b\" != \"current.json\" ]; then
         rm -rf \"\$d\"
       fi
     done
   ' >/tmp/octendra_ship_cutover.log 2>&1 </dev/null &
   echo '==> Gateway cutover triggered; all Agents will auto-converge on next poll.'"
