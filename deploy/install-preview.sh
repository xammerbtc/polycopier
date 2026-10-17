#!/bin/bash
set -euo pipefail
cd /opt/polycopier
test "$(cat /opt/polycopier/build.exit)" = 0
test "$(cat /opt/polycopier/build.phase)" = complete
test -x target/release/polycopier
test -f web/dist/index.html
getent passwd polycopier >/dev/null || useradd --system --home-dir /opt/polycopier --shell /usr/sbin/nologin polycopier
if [ ! -f config.toml ]; then cp deploy/config.preview.toml config.toml; fi
python3 deploy/provision-preview.py
chown -R polycopier:polycopier /opt/polycopier
if [ -f .env.credentials ]; then chown root:root .env.credentials; chmod 600 .env.credentials; fi
chmod 600 .env
install -m 644 deploy/polycopier-preview.service /etc/systemd/system/polycopier-preview.service
systemd-analyze verify /etc/systemd/system/polycopier-preview.service
systemctl daemon-reload
systemctl enable --now polycopier-preview.service
systemctl is-active polycopier-preview.service
