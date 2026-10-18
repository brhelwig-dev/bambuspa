#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "${1:-.}" && pwd)
version=0.7.10
case "$(uname -m)" in
  x86_64)
    target=x86_64-unknown-linux-gnu
    sha256=4363e4ed2a3f1eb7f4d38d2d59aed59ce43271c44c16b425e92c89a64761fbe7
    ;;
  aarch64)
    target=aarch64-unknown-linux-gnu
    sha256=8f1a17d3218700ffbe15e6540d936a178b2556fc801121a31082e3ba4ab9ef55
    ;;
  *)
    echo "dx $version has no build for $(uname -m)" >&2
    exit 1
    ;;
esac

tools="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/dx-$version"
if [ ! -x "$tools/dx" ]; then
  mkdir -p "$tools"
  curl -fsSL "https://github.com/DioxusLabs/dioxus/releases/download/v$version/dx-$target.tar.gz" -o "$tools/dx.tar.gz"
  echo "$sha256  $tools/dx.tar.gz" | sha256sum -c -
  tar -xzf "$tools/dx.tar.gz" -C "$tools"
  rm "$tools/dx.tar.gz"
fi

cd "$repo/web"
rustup target add wasm32-unknown-unknown
"$tools/dx" bundle --platform web --release --out-dir dist
