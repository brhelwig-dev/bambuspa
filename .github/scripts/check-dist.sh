#!/usr/bin/env bash
# Checks one platform's release build on the runner that built it: the binary
# starts and answers /healthz, and it runs without libraries the machine may
# not have (Linux: fully static; Windows: no Visual C++ runtime).
#
# Usage: check-dist.sh PLATFORM   (after build-dist.sh has built it)
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

platform=${1:?usage: check-dist.sh PLATFORM}
case "$platform" in
  linux/amd64)   target=x86_64-unknown-linux-musl ;;
  linux/arm64)   target=aarch64-unknown-linux-musl ;;
  windows/amd64) target=x86_64-pc-windows-msvc ;;
  windows/arm64) target=aarch64-pc-windows-msvc ;;
  darwin/amd64)  target=x86_64-apple-darwin ;;
  darwin/arm64)  target=aarch64-apple-darwin ;;
  *) echo "unknown platform $platform" >&2; exit 1 ;;
esac
bin=target/$target/release/bambuspa
[ "${platform%/*}" = windows ] && bin=$bin.exe

case "$platform" in
  linux/*)
    file "$bin"
    if file "$bin" | grep -q "dynamically linked"; then
      echo "$bin is dynamically linked" >&2
      exit 1
    fi
    ;;
  windows/*)
    imports=$(llvm-readobj --coff-imports "$bin")
    if echo "$imports" | grep -qi vcruntime; then
      echo "$bin needs the Visual C++ runtime" >&2
      exit 1
    fi
    ;;
esac

# macos-latest is arm64, so it cannot run an x86-64 build without Rosetta.
if [ "$platform" = darwin/amd64 ]; then
  echo "not running $platform on this arm64 runner"
  exit 0
fi

data=$(mktemp -d)
AUTH_DISABLED=true DATA_DIR="$data" LISTEN_ADDR=127.0.0.1:18081 "$bin" &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT
for _ in $(seq 1 30); do
  if curl -sf http://127.0.0.1:18081/healthz >/dev/null; then
    echo "$platform answers /healthz"
    exit 0
  fi
  sleep 1
done
echo "$platform did not answer /healthz within 30s" >&2
exit 1
