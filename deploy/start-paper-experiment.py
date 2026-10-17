#!/usr/bin/env python3
"""Archive the complete paper run, then reset to a common $20k / $5 experiment."""
import base64
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
import urllib.request

ROOT = Path('/opt/polycopier')
UNIT = Path('/etc/systemd/system/polycopier-preview.service')
DROPIN = Path('/etc/systemd/system/polycopier-preview.service.d/paper-bank.conf')
FILES = ('simulation_state.json', 'sim_copy_ledger.json', 'runtime-risk.json', 'config.toml')


def run(*args):
    return subprocess.run(args, check=True, text=True, capture_output=True).stdout.strip()


def client():
    env = {}
    for raw in (ROOT / '.env').read_text(encoding='utf-8').splitlines():
        if '=' in raw and not raw.lstrip().startswith('#'):
            key, value = raw.split('=', 1)
            env[key.strip()] = value.strip().strip("\"'")
    token = env.get('DASHBOARD_TOKEN', '')
    if len(token) < 24:
        raise RuntimeError('Dashboard authentication is not configured')
    auth = 'Basic ' + base64.b64encode(('polycopier:' + token).encode()).decode()

    def request(path):
        req = urllib.request.Request('http://127.0.0.1:3000' + path, headers={'Authorization': auth})
        with urllib.request.urlopen(req, timeout=10) as response:
            return json.load(response)
    return request


def analyse(ledger, state, api_state, stamp):
    stats = {}
    for entry in ledger.get('entries', []):
        wallet = entry.get('source_wallet', 'unknown').lower()
        row = stats.setdefault(wallet, {'entries': 0, 'closed_entries': 0, 'open_lots': 0,
                                        'open_shares': 0.0, 'open_cost': 0.0, 'realized_pnl': 0.0})
        row['entries'] += 1
        row['realized_pnl'] += float(entry.get('realized_pnl') or 0)
        if entry.get('closed'):
            row['closed_entries'] += 1
        else:
            shares = float(entry.get('filled_size') or entry.get('size') or 0)
            if shares > 0:
                row['open_lots'] += 1
                row['open_shares'] += shares
                row['open_cost'] += shares * float(entry.get('entry_price') or 0)
    for row in stats.values():
        for key in ('open_shares', 'open_cost', 'realized_pnl'):
            row[key] = round(row[key], 8)
    return {
        'archived_at_utc': stamp.isoformat(),
        'history_entries': len(ledger.get('entries', [])),
        'history_sources': stats,
        'history_ledger_performance_since': ledger.get('performance_since'),
        'persisted_positions': len(state.get('positions', {})),
        'persisted_balance': state.get('total_balance'),
        'persisted_realized_pnl': state.get('realized_pnl'),
        'persisted_outcomes': len(state.get('outcomes', [])),
        'live_dashboard_snapshot': {
            key: api_state.get(key) for key in (
                'mode', 'paused', 'total_balance', 'starting_balance', 'equity', 'total_pnl',
                'realized_pnl', 'unrealized_pnl', 'open_cost_basis', 'target_positions_total',
                'copies_executed', 'trades_skipped', 'priced_positions')
        } | {'open_positions': len(api_state.get('positions', {}))},
    }


def main():
    if ROOT.resolve() != Path('/opt/polycopier'):
        raise RuntimeError('Unexpected project root; refusing to reset')
    if run('systemctl', 'is-active', 'polycopier-preview') != 'active':
        raise RuntimeError('Preview service must be active before reset')
    if subprocess.run(['systemctl', 'is-active', 'polycopier'], capture_output=True, text=True).returncode == 0:
        raise RuntimeError('A separate live service is active; refusing paper reset')
    api = client()
    live = api('/health')
    current = api('/api/state')
    if live.get('mode') != 'simulation' or current.get('mode') != 'simulation':
        raise RuntimeError('Active service is not in simulation mode')

    stamp = dt.datetime.now(dt.timezone.utc)
    archive = ROOT / 'deploy-backups' / (stamp.strftime('%Y%m%dT%H%M%SZ') + '-clean-paper-reset')
    archive.mkdir(mode=0o700, parents=True, exist_ok=False)
    prior_config = (ROOT / 'config.toml').read_text(encoding='utf-8')
    old_dropin = DROPIN.read_text(encoding='utf-8') if DROPIN.exists() else ''
    old_unit = UNIT.read_text(encoding='utf-8')
    try:
        (archive / 'dashboard-state.json').write_text(json.dumps(current, indent=2), encoding='utf-8')
        for name in FILES:
            shutil.copy2(ROOT / name, archive / name)
        log = ROOT / 'polycopier.log'
        if log.exists():
            with log.open(encoding='utf-8', errors='replace') as source:
                tail = source.readlines()[-10000:]
            (archive / 'recent-log-tail.txt').write_text(''.join(tail), encoding='utf-8')
        ledger = json.loads((archive / 'sim_copy_ledger.json').read_text(encoding='utf-8'))
        saved_state = json.loads((archive / 'simulation_state.json').read_text(encoding='utf-8'))
        report = analyse(ledger, saved_state, current, stamp)
        (archive / 'history-analysis.json').write_text(json.dumps(report, indent=2), encoding='utf-8')

        subprocess.run(['systemctl', 'stop', 'polycopier-preview'], check=True)
        # Copy the final persisted values after the service flushes on shutdown.
        for name in FILES:
            shutil.copy2(ROOT / name, archive / ('stopped-' + name))
        prior_config = (ROOT / 'config.toml').read_text(encoding='utf-8')
        wallet_match = re.search(r'(?m)^wallets\s*=\s*\[([^\]]*)\]', prior_config)
        if not wallet_match:
            raise RuntimeError('Cannot find wallet roster in config.toml')
        wallets = re.findall(r'"([^"]+)"', wallet_match.group(1))
        if len(wallets) != 8:
            raise RuntimeError(f'Expected eight named leaders; found {len(wallets)}')
        normalized = []
        for value in wallets:
            parts = value.split(':', 2)
            if len(parts) != 3 or not re.fullmatch(r'0x[0-9a-fA-F]{40}', parts[0]):
                raise RuntimeError('Wallet roster contains an invalid named target')
            normalized.append(f'"{parts[0].lower()}:1.0:{parts[2]}"')
        config = prior_config[:wallet_match.start(1)] + ', '.join(normalized) + prior_config[wallet_match.end(1):]
        config, count = re.subn(r'(?m)^mode\s*=\s*"[^"]+"', 'mode = "fixed"', config, count=1)
        if count != 1:
            raise RuntimeError('Cannot set fixed-size sizing mode')
        config, count = re.subn(r'(?m)^max_trade_size_usd\s*=\s*[^\n]+', 'max_trade_size_usd = 5.00', config, count=1)
        if count != 1:
            raise RuntimeError('Cannot set $5 trade ceiling')
        config, count = re.subn(r'(?m)^max_entry_price\s*=\s*[^\n]+', 'max_entry_price = 0.98', config, count=1)
        if count != 1:
            raise RuntimeError('Cannot preserve the requested $0.98 max entry')
        config = re.sub(r'(?m)^max_daily_volume_usd\s*=\s*[^\n]+', 'max_daily_volume_usd = 0', config)
        (ROOT / 'config.toml').write_text(config, encoding='utf-8')

        start = int(time.time()) + 2
        dropin = old_dropin
        dropin = re.sub(r'(?m)^Environment=PAPER_EXPERIMENT_START_TIMESTAMP=.*\n?', '', dropin)
        if '[Service]' not in dropin:
            dropin = '[Service]\n' + dropin
        dropin = dropin.replace('[Service]\n', '[Service]\nEnvironment=PAPER_EXPERIMENT_START_TIMESTAMP=' + str(start) + '\n', 1)
        DROPIN.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
        DROPIN.write_text(dropin, encoding='utf-8')

        initial = {'processed_signals': {}, 'positions': {}, 'total_balance': '20000',
                   'realized_pnl': '0', 'cycle_pnl': {}, 'outcomes': []}
        (ROOT / 'simulation_state.json').write_text(json.dumps(initial, indent=2), encoding='utf-8')
        (ROOT / 'sim_copy_ledger.json').write_text(json.dumps({
            'entries': [], 'performance_since': dt.datetime.fromtimestamp(start, dt.timezone.utc).isoformat().replace('+00:00', 'Z')
        }, indent=2), encoding='utf-8')
        (ROOT / 'runtime-risk.json').write_text(json.dumps({
            'date': dt.datetime.fromtimestamp(start, dt.timezone.utc).date().isoformat(),
            'volume': '0', 'losses': 0, 'cooldown_until': None,
            'seen_live_outcomes': [], 'live_outcome_snapshot': None, 'live_snapshot_limits': None,
        }, indent=2), encoding='utf-8')
        service_user = pwd.getpwnam('polycopier')
        for name in ('simulation_state.json', 'sim_copy_ledger.json', 'runtime-risk.json'):
            os.chown(ROOT / name, service_user.pw_uid, service_user.pw_gid)
            os.chmod(ROOT / name, 0o600)
        os.chown(ROOT / 'config.toml', service_user.pw_uid, service_user.pw_gid)
        os.chmod(ROOT / 'config.toml', 0o600)
        os.chown(DROPIN, 0, 0)
        os.chmod(DROPIN, 0o644)
        subprocess.run(['systemctl', 'daemon-reload'], check=True)
        subprocess.run(['systemctl', 'start', 'polycopier-preview'], check=True)

        for _ in range(30):
            try:
                state = api('/api/state')
                if state.get('mode') == 'simulation' and state.get('starting_balance') == '20000':
                    break
            except Exception:
                time.sleep(1)
        else:
            raise RuntimeError('Fresh simulation did not become healthy')
        config_api = api('/api/config')
        if config_api['sizing']['mode'] != 'fixed' or float(config_api['execution']['max_trade_size_usd']) != 5.0:
            raise RuntimeError('Running configuration did not apply the fixed $5 ceiling')
        if len(config_api['targets']['wallets']) != 8:
            raise RuntimeError('Leader roster changed during reset')
        digest = hashlib.sha256
        checks = {}
        for file in archive.iterdir():
            if file.is_file():
                checks[file.name] = digest(file.read_bytes()).hexdigest()
        (archive / 'SHA256SUMS.json').write_text(json.dumps(checks, indent=2), encoding='utf-8')
        print(json.dumps({
            'archive': str(archive), 'previous_entries': report['history_entries'],
            'previous_positions': report['persisted_positions'], 'source_count': len(report['history_sources']),
            'mode': state['mode'], 'starting_balance': state['starting_balance'],
            'cash_after_start': state['total_balance'], 'open_positions_after_start': len(state['positions']),
            'sizing': config_api['sizing']['mode'], 'max_per_signal_usd': config_api['execution']['max_trade_size_usd'],
            'active_leaders': len(config_api['targets']['wallets']), 'websocket': api('/health').get('websocket_connected'),
        }, indent=2))
    except Exception:
        subprocess.run(['systemctl', 'stop', 'polycopier-preview'], check=False)
        (ROOT / 'config.toml').write_text(prior_config, encoding='utf-8')
        DROPIN.write_text(old_dropin, encoding='utf-8')
        for name in FILES:
            backup = archive / ('stopped-' + name)
            if backup.exists():
                shutil.copy2(backup, ROOT / name)
        UNIT.write_text(old_unit, encoding='utf-8')
        subprocess.run(['systemctl', 'daemon-reload'], check=False)
        subprocess.run(['systemctl', 'start', 'polycopier-preview'], check=False)
        raise


if __name__ == '__main__':
    main()
