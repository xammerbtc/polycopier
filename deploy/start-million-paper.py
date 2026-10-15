#!/usr/bin/env python3
"""Archive the current paper run and start a fresh $1m all-leader 1:1 run."""
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import shutil
import subprocess
import time
import tomllib

ROOT = Path('/opt/polycopier')
DROPIN = Path('/etc/systemd/system/polycopier-preview.service.d/paper-bank.conf')
FILES = ('simulation_state.json', 'sim_copy_ledger.json', 'runtime-risk.json', 'config.toml')


def run(*args):
    return subprocess.run(args, check=True, text=True, capture_output=True).stdout.strip()


def replace_line(text, key, value):
    replacement = f'{key}={value}' if key.startswith(('Environment=', 'ExecStart')) else f'{key} = {value}'
    changed, count = re.subn(rf'(?m)^{re.escape(key)}[ \t]*=[ \t]*[^\n]+',
                             replacement, text, count=1)
    if count != 1:
        raise RuntimeError(f'Missing config field: {key}')
    return changed


def main():
    if ROOT.resolve() != Path('/opt/polycopier'):
        raise RuntimeError('Unexpected checkout')
    if run('systemctl', 'is-active', 'polycopier-preview') != 'active':
        raise RuntimeError('Paper service is not active')
    if subprocess.run(['systemctl', 'is-active', 'polycopier'], capture_output=True).returncode == 0:
        raise RuntimeError('Live service is active')
    stamp = dt.datetime.now(dt.timezone.utc)
    archive = ROOT / 'deploy-backups' / (stamp.strftime('%Y%m%dT%H%M%SZ') + '-million-paper-reset')
    archive.mkdir(mode=0o700, parents=True, exist_ok=False)
    old_dropin = DROPIN.read_text(encoding='utf-8')
    stopped = False
    try:
        for name in FILES:
            shutil.copy2(ROOT / name, archive / ('prestop-' + name))
        run('systemctl', 'stop', 'polycopier-preview')
        stopped = True
        for name in FILES:
            shutil.copy2(ROOT / name, archive / name)
        state = json.loads((archive / 'simulation_state.json').read_text(encoding='utf-8'))
        ledger = json.loads((archive / 'sim_copy_ledger.json').read_text(encoding='utf-8'))
        summary = {
            'archived_at_utc': stamp.isoformat(),
            'previous_cash': state.get('total_balance'),
            'previous_realized_pnl': state.get('realized_pnl'),
            'previous_positions': len(state.get('positions', {})),
            'previous_ledger_entries': len(ledger.get('entries', [])),
        }
        (archive / 'summary.json').write_text(json.dumps(summary, indent=2), encoding='utf-8')
        config = (archive / 'config.toml').read_text(encoding='utf-8')
        parsed = tomllib.loads(config)
        if len(parsed['targets']['wallets']) != 14:
            raise RuntimeError('Expected the current 14-leader paper roster')
        config = replace_line(config, 'mode', '"bank_scaled"')
        config = replace_line(config, 'copy_size_pct', '1')
        config = replace_line(config, 'max_trade_size_usd', '1000000')
        config = replace_line(config, 'max_entries_per_cycle', '0')
        config = replace_line(config, 'max_daily_volume_usd', '0')
        config = replace_line(config, 'max_consecutive_losses', '0')
        config = replace_line(config, 'profit_guard_trigger_pct', '0')
        config = replace_line(config, 'profit_guard_loss_pct', '0')
        tomllib.loads(config)
        (ROOT / 'config.toml').write_text(config, encoding='utf-8')

        start = int(time.time()) + 2
        dropin = old_dropin
        dropin = replace_line(dropin, 'Environment=PAPER_EXPERIMENT_START_TIMESTAMP', str(start))
        dropin = replace_line(dropin, 'Environment=PAPER_STARTING_BALANCE', '1000000')
        dropin = replace_line(dropin, 'ExecStart', '/opt/polycopier/target/release/polycopier --ui-reboot --sim --sim-balance 1000000')
        DROPIN.write_text(dropin, encoding='utf-8')

        initial = {'processed_signals': {}, 'positions': {}, 'total_balance': '1000000',
                   'realized_pnl': '0', 'cycle_pnl': {}, 'outcomes': []}
        (ROOT / 'simulation_state.json').write_text(json.dumps(initial), encoding='utf-8')
        (ROOT / 'sim_copy_ledger.json').write_text(json.dumps({
            'entries': [], 'performance_since': dt.datetime.fromtimestamp(start, dt.timezone.utc)
            .isoformat().replace('+00:00', 'Z')
        }), encoding='utf-8')
        (ROOT / 'runtime-risk.json').write_text(json.dumps({
            'date': stamp.date().isoformat(), 'volume': '0', 'losses': 0,
            'cooldown_until': None, 'seen_live_outcomes': [],
            'live_outcome_snapshot': None, 'live_snapshot_limits': None,
        }), encoding='utf-8')
        service_user = pwd.getpwnam('polycopier')
        for name in FILES:
            os.chown(ROOT / name, service_user.pw_uid, service_user.pw_gid)
            os.chmod(ROOT / name, 0o600)
        run('systemctl', 'daemon-reload')
        run('systemctl', 'start', 'polycopier-preview')
        for _ in range(30):
            if run('systemctl', 'is-active', 'polycopier-preview') == 'active':
                try:
                    import urllib.request
                    with urllib.request.urlopen('http://127.0.0.1:3000/health', timeout=3) as response:
                        health = json.load(response)
                    if health.get('mode') == 'simulation':
                        break
                except Exception:
                    pass
            time.sleep(1)
        else:
            raise RuntimeError('New paper experiment failed health check')
        checksums = {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                     for p in archive.iterdir() if p.is_file()}
        (archive / 'SHA256SUMS.json').write_text(json.dumps(checksums, indent=2), encoding='utf-8')
        print(json.dumps({'archive': str(archive), 'starting_bank': 1000000,
                          'leaders': 14, 'sizing': '100% source USD, fee-inclusive',
                          'catchup_entries_per_cycle': 0, 'previous': summary}, indent=2))
    except Exception:
        if stopped:
            subprocess.run(['systemctl', 'stop', 'polycopier-preview'], check=False)
            for name in FILES:
                backup = archive / name
                if backup.exists():
                    shutil.copy2(backup, ROOT / name)
            DROPIN.write_text(old_dropin, encoding='utf-8')
            subprocess.run(['systemctl', 'daemon-reload'], check=False)
            subprocess.run(['systemctl', 'start', 'polycopier-preview'], check=False)
        raise


if __name__ == '__main__':
    main()
