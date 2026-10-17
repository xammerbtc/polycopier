"""Carry paper dashboard counters across the first telemetry-aware deployment."""

import base64
import json
import os
import pathlib
import stat
import sys
import urllib.request

ROOT = pathlib.Path('/opt/polycopier')
FIELDS = ('copies_executed', 'trades_skipped', 'live_feed')


def atomic_json(path, value):
    previous = path.stat() if path.exists() else None
    temporary = path.with_name(path.name + '.telemetry.tmp')
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        if previous:
            os.fchmod(fd, stat.S_IMODE(previous.st_mode))
            os.fchown(fd, previous.st_uid, previous.st_gid)
        with os.fdopen(fd, 'w', encoding='utf-8') as output:
            fd = -1
            json.dump(value, output, ensure_ascii=False)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if fd != -1:
            os.close(fd)
        temporary.unlink(missing_ok=True)


def validate(telemetry):
    for name in FIELDS[:2]:
        value = telemetry.get(name)
        if type(value) is not int or not 0 <= value <= 0xffffffff:
            raise ValueError(f'Invalid {name}')
    if not isinstance(telemetry.get('live_feed'), list) or len(telemetry['live_feed']) > 100:
        raise ValueError('Invalid paper live feed')


def capture(backup, snapshot):
    saved = json.loads(snapshot.read_text(encoding='utf-8'))
    if all(name in saved for name in FIELDS):
        return
    env = dict(line.split('=', 1) for line in (ROOT / '.env').read_text().splitlines() if '=' in line)
    token = base64.b64encode(('polycopier:' + env['DASHBOARD_TOKEN']).encode()).decode()
    base = 'http://' + env.get('DASHBOARD_BIND', '127.0.0.1:3000')
    request = urllib.request.Request(base + '/api/state', headers={'Authorization': 'Basic ' + token})
    with urllib.request.urlopen(request, timeout=20) as response:
        state = json.load(response)
    telemetry = {name: state[name] for name in FIELDS}
    validate(telemetry)
    atomic_json(backup, telemetry)


def merge(backup, snapshot):
    if not backup.exists():
        return
    telemetry = json.loads(backup.read_text(encoding='utf-8'))
    validate(telemetry)
    saved = json.loads(snapshot.read_text(encoding='utf-8'))
    missing = [name for name in FIELDS if name not in saved]
    if missing:
        for name in missing:
            saved[name] = telemetry[name]
        atomic_json(snapshot, saved)


if __name__ == '__main__':
    action, backup_name = sys.argv[1:3]
    backup_path = pathlib.Path(backup_name)
    snapshot_path = ROOT / 'simulation_state.json'
    if action == 'capture':
        capture(backup_path, snapshot_path)
    elif action == 'merge':
        merge(backup_path, snapshot_path)
    else:
        raise SystemExit(f'Unknown action: {action}')
