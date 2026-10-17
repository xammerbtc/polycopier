#!/bin/bash
set -euo pipefail
cd /opt/polycopier
exec 9>/opt/polycopier/.build.lock
flock -n 9 || { echo "Another build or storage maintenance is running" >&2; exit 1; }
finish_build() {
    build_status=$?
    printf '%s\n' "$build_status" > /opt/polycopier/build.exit
    flock -u 9
    cd /opt/polycopier
    if ! python3 /opt/polycopier/deploy/storage-maintenance.py --apply; then
        echo "Storage maintenance needs attention; see storage-status.json and journal" >&2
    fi
    exit "$build_status"
}
trap finish_build EXIT
echo disk-preflight > build.phase
available_bytes=$(df -B1 --output=avail /opt/polycopier | tail -n 1 | tr -d ' ')
if (( available_bytes < 16106127360 )); then
    echo "Build refused: at least 15 GiB free space is required" >&2
    exit 1
fi
echo dependency-resolution > build.phase
docker run --rm --name polycopier-build -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 -e CARGO_INCREMENTAL=0 --cpus=2 --memory=6g -v /opt/polycopier:/app -v /opt/polycopier/.cargo-registry:/usr/local/cargo/registry -w /app rust@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 bash -c 'rustup component add rustfmt clippy && (test -f Cargo.lock || cargo generate-lockfile) && cargo fmt --all && cargo test --all --locked && cargo clippy --all-targets --locked -- -D warnings && cargo build --release --locked'
echo frontend > build.phase
cd web
npm ci
npm run build
cd ..
if test -f deploy/redeemer/package.json; then bash deploy/install-redeemer.sh; fi
echo complete > build.phase
