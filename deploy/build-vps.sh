#!/bin/bash
set -euo pipefail
cd /opt/polycopier
trap 'printf "%s\n" "$?" > /opt/polycopier/build.exit' EXIT
echo dependency-resolution > build.phase
docker run --rm --name polycopier-build --cpus=2 --memory=6g -v /opt/polycopier:/app -v /opt/polycopier/.cargo-registry:/usr/local/cargo/registry -w /app rust@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 bash -c 'rustup component add rustfmt clippy && (test -f Cargo.lock || cargo generate-lockfile) && cargo fmt --all && cargo test --all --locked && cargo clippy --all-targets --locked -- -D warnings && cargo build --release --locked'
echo frontend > build.phase
cd web
npm ci
npm run build
cd ..
if test -f deploy/redeemer/package.json; then bash deploy/install-redeemer.sh; fi
echo complete > build.phase
