#!/usr/bin/env bash
# Builds and packages bambuspa for released platforms. ci runs it on every
# change and release runs it for each version, so a target that stops building
# fails a pull request rather than a release.
#
# Usage: build-dist.sh VERSION OUTDIR PLATFORM...
#        build-dist.sh checksums OUTDIR
#
# PLATFORM is os/arch as the release assets name it. Each builds natively on a
# runner of its own OS: linux/* on Linux with musl-gcc (from musl-tools), so the
# binaries are static; windows/* on Windows with MSVC; darwin/* on macOS.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

version=${1:?usage: build-dist.sh VERSION OUTDIR PLATFORM...}
out=${2:?usage: build-dist.sh VERSION OUTDIR PLATFORM...}
shift 2
mkdir -p "$out"

if [ "$version" = checksums ]; then
  (cd "$out" && sha256sum -- *.tar.gz *.zip > checksums.txt)
  exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

for platform in "$@"; do
  os=${platform%/*}
  arch=${platform#*/}
  case "$platform" in
    linux/amd64)   target=x86_64-unknown-linux-musl ;;
    linux/arm64)   target=aarch64-unknown-linux-musl ;;
    windows/amd64) target=x86_64-pc-windows-msvc ;;
    windows/arm64) target=aarch64-pc-windows-msvc ;;
    darwin/amd64)  target=x86_64-apple-darwin ;;
    darwin/arm64)  target=aarch64-apple-darwin ;;
    *) echo "unknown platform $platform" >&2; exit 1 ;;
  esac
  echo "building $platform ($target)"
  rustup target add "$target"
  # The cc crate looks for a target-prefixed musl compiler on arm64.
  [ "$os" = linux ] && export "CC_${target//-/_}=musl-gcc"
  cargo build --release --locked --target "$target"

  bin=bambuspa
  [ "$os" = windows ] && bin=bambuspa.exe
  cp "target/$target/release/$bin" README.md LICENSE "$work/"
  name="bambuspa_${version}_${os}_${arch}"
  if [ "$os" = windows ]; then
    zip=$(cd "$out" && pwd)/$name.zip
    (cd "$work" && 7z a -tzip -bso0 "$zip" "$bin" README.md LICENSE)
  else
    tar -czf "$out/$name.tar.gz" -C "$work" "$bin" README.md LICENSE
  fi
  rm "$work/$bin"
done
