#!/bin/bash
# Run under the existing VPS production flock; never restarts the donor bot.
set -euo pipefail
cd /opt/polycopier
test "$(cat build.exit)" = 0
test "$(cat build.phase)" = complete
systemctl is-active --quiet polycopier-preview
preview_pid=$(systemctl show polycopier-preview -p MainPID --value)
backup="/opt/polycopier/deploy-backups/$(date -u +%Y%m%dT%H%M%SZ)"
install -d -m 700 "$backup"
cp "/proc/$preview_pid/exe" "$backup/polycopier"
cp config.toml "$backup/config.toml"
python3 deploy/preserve-paper-telemetry.py capture "$backup/paper-telemetry.json"
systemctl stop polycopier-preview
for name in simulation_state.json sim_copy_ledger.json runtime-risk.json; do
    test ! -f "$name" || cp "$name" "$backup/$name"
done
if ! python3 deploy/preserve-paper-telemetry.py merge "$backup/paper-telemetry.json"; then
    cp "$backup/polycopier" target/release/polycopier
    systemctl start polycopier-preview
    echo "Paper telemetry migration failed; previous binary restored" >&2
    exit 1
fi
systemctl start polycopier-preview
ready=0
for attempt in $(seq 1 30); do
    if curl -fsS http://127.0.0.1:3000/health >/dev/null; then ready=1; break; fi
    sleep 1
done
if test "$ready" != 1; then
    systemctl stop polycopier-preview
    cp "$backup/polycopier" target/release/polycopier
    systemctl start polycopier-preview
    echo "Preview health failed; previous binary restored" >&2
    exit 1
fi
python3 deploy/verify-preview.py
echo "Updated isolated preview; rollback binary and stopped-state backup: $backup"
