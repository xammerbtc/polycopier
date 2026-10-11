#!/bin/bash
set -euo pipefail
cd /opt/polycopier
image='node@sha256:d6aa754f16b3197301076f047b5def2f02ea1dbbc2ca920407d46d7ec7f87b20'
install -d -m 755 runtime
runtime_container=$(docker create --label polycopier.runtime=redeemer "$image" /bin/true)
trap 'docker rm "$runtime_container" >/dev/null' EXIT
docker cp "$runtime_container:/usr/local/bin/node" runtime/node24
chmod 755 runtime/node24
runtime/node24 --version
# Only SDK source/dependencies are mounted; no wallet or dashboard credentials.
docker run --rm --cpus=1 --memory=512m -v /opt/polycopier/deploy/redeemer:/redeemer -w /redeemer "$image" bash -c '
    test -f package-lock.json || npm install --ignore-scripts --package-lock-only
    npm ci --ignore-scripts
    npm test
    node --input-type=module -e '\''import {createSecureClient} from "@polymarket/client"; import {privateKey} from "@polymarket/client/viem"; if(typeof createSecureClient!=="function" || typeof privateKey!=="function") process.exit(1); console.log("SDK imports PASS");'\''
    npm audit --audit-level=low
'
