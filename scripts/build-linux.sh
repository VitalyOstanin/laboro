#!/usr/bin/env bash
# Build the Linux release binary inside podman (proxy forwarding disabled;
# cargo registry/target cached in named volumes).
#
# The target volume is mounted over /work/target, so the build output is not
# visible in the mounted work tree. The binary is copied into dist/ as the last
# step of the same run, which is what a caller outside the container (a build
# run on another machine, a release script) can actually reach.
set -euo pipefail

img=laba-build
podman build --http-proxy=false -t "$img" -f Containerfile .
podman run --rm --http-proxy=false -v "$PWD":/work \
  -v opc-cargo-registry:/root/.cargo/registry \
  -v opc-cargo-target:/work/target \
  "$img" \
  sh -c 'cargo build --release --locked --bin laba \
    && mkdir -p /work/dist \
    && cp /work/target/release/laba /work/dist/laba'
echo "binary: dist/laba (build cache stays in the opc-cargo-target volume)" >&2
