#!/usr/bin/env bash
# Fails when the Dockerfile's rust image and rust-toolchain.toml name different
# Rust versions, so the image and the release binaries are built with the same
# compiler. Dependabot bumps the image on its own.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

toolchain=$(sed -nE 's/^channel *= *"([0-9]+\.[0-9]+)(\.[0-9]+)?".*/\1/p' rust-toolchain.toml)
image=$(sed -nE 's/^FROM .*rust:([0-9]+\.[0-9]+)(\.[0-9]+)?.*/\1/p' Dockerfile | head -1)

if [ -z "$toolchain" ] || [ -z "$image" ]; then
  echo "could not read the Rust version from rust-toolchain.toml ($toolchain) or the Dockerfile ($image)" >&2
  exit 1
fi
if [ "$toolchain" != "$image" ]; then
  echo "rust-toolchain.toml builds with Rust $toolchain but the Dockerfile uses rust:$image; update both together" >&2
  exit 1
fi
echo "Rust $toolchain in both rust-toolchain.toml and the Dockerfile"
