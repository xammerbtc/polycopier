#!/bin/bash
# Run as root under the existing production lock; this does not restart the copier.
set -euo pipefail
cd /opt/polycopier
python3 deploy/test_storage_maintenance.py
install -d -o root -g root -m 755 /usr/local/lib/polycopier-storage /var/lib/polycopier-storage
install -o root -g root -m 644 deploy/storage-maintenance.py /usr/local/lib/polycopier-storage/storage-maintenance.py
install -o root -g root -m 644 deploy/polycopier.logrotate /etc/logrotate.d/polycopier
install -o root -g root -m 644 deploy/polycopier-storage-maintenance.service /etc/systemd/system/polycopier-storage-maintenance.service
install -o root -g root -m 644 deploy/polycopier-storage-maintenance.timer /etc/systemd/system/polycopier-storage-maintenance.timer
systemd-analyze verify /etc/systemd/system/polycopier-storage-maintenance.service /etc/systemd/system/polycopier-storage-maintenance.timer
systemctl daemon-reload
systemctl enable --now polycopier-storage-maintenance.timer
systemctl start polycopier-storage-maintenance.service
