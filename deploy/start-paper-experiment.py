"""Start the explicitly requested fresh $1000 / 50% paper experiment.

Run only under /opt/arbigab/operator_control/locks/production.lock.
Backups include the stopped experiment state, unit, config and running binary.
"""
import base64
import datetime
import json
import os
import pathlib
import re
import shutil
import subprocess
import time
import urllib.request

root = pathlib.Path('/opt/polycopier')
unit = pathlib.Path('/etc/systemd/system/polycopier-preview.service')
assert (root / 'build.phase').read_text().strip() == 'complete'
assert (root / 'build.exit').read_text().strip() == '0'
pid = subprocess.check_output(['systemctl', 'show', 'polycopier-preview', '-p', 'MainPID', '--value'], text=True).strip()
assert int(pid) > 0
backup = root / 'deploy-backups' / (datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ') + '-experiment-reset')
backup.mkdir(mode=0o700)
shutil.copyfile('/proc/' + pid + '/exe', backup / 'polycopier')
shutil.copy2(unit, backup / 'preview.service')
shutil.copy2(root / 'config.toml', backup / 'config.toml')
subprocess.run(['systemctl', 'stop', 'polycopier-preview'], check=True)
names = ['simulation_state.json', 'sim_copy_ledger.json', 'runtime-risk.json']
for name in names:
    if (root / name).exists():
        shutil.copy2(root / name, backup / name)

wallet = '0x2d2d2b32e5199edf833594a99b9c54e5668ab69d'
start = int(time.time()) + 1
profile = f'''[targets]
wallets = ["{wallet}:0.5"]

[execution]
max_slippage_pct = 0.015
max_trade_size_usd = 1000000000
max_delay_seconds = 6
sell_fee_buffer = 1
ignore_closing_in_mins = 0

[sizing]
mode = "target_scalar"
copy_size_pct = 0.5

[scanner]
max_copy_loss_pct = 1
max_copy_gain_pct = 1
min_entry_price = 0
max_entry_price = 1
max_entries_per_cycle = 0
min_amount = 0
max_amount = 1000000000

[risk]
max_daily_volume_usd = 0
max_consecutive_losses = 0
loss_cooldown_secs = 0

[ledger]
retention_days = 60
'''

try:
    (root / 'config.toml').write_text(profile)
    for name in names:
        (root / name).unlink(missing_ok=True)
    seed = {'positions': {}, 'total_balance': '1000', 'realized_pnl': '0', 'cycle_pnl': {}, 'outcomes': []}
    (root / 'simulation_state.json').write_text(json.dumps(seed, indent=2))
    (root / 'sim_copy_ledger.json').write_text('{"entries": []}')
    for name in names[:2]:
        shutil.chown(root / name, user='polycopier', group='polycopier')
        os.chmod(root / name, 0o600)
    service = unit.read_text()
    service, replacements = re.subn(r'--sim-balance\s+[0-9.]+', '--sim-balance 1000', service)
    assert replacements == 1 and '--sim ' in service
    service = re.sub(r'^Environment=PAPER_EXPERIMENT_START_TIMESTAMP=.*\n', '', service, flags=re.M)
    service = service.replace('[Service]\n', '[Service]\nEnvironment=PAPER_EXPERIMENT_START_TIMESTAMP=' + str(start) + '\n')
    unit.write_text(service)
    subprocess.run(['systemctl', 'daemon-reload'], check=True)
    subprocess.run(['systemctl', 'start', 'polycopier-preview'], check=True)
    env = dict(line.split('=', 1) for line in (root / '.env').read_text().splitlines() if '=' in line)
    auth = 'Basic ' + base64.b64encode(('polycopier:' + env['DASHBOARD_TOKEN']).encode()).decode()
    def get(path):
        request = urllib.request.Request('http://127.0.0.1:3000' + path, headers={'Authorization': auth})
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)
    for attempt in range(30):
        try:
            state = get('/api/state')
            assert state['mode'] == 'simulation'
            break
        except Exception:
            if attempt == 29:
                raise
            time.sleep(1)
    config = get('/api/config')
    assert config['targets']['wallets'] == [wallet + ':0.5']
    assert config['sizing']['mode'] == 'target_scalar'
    assert float(config['risk']['max_daily_volume_usd']) == 0
    assert config['risk']['max_consecutive_losses'] == 0
    report = {'started_at_utc': datetime.datetime.fromtimestamp(start, datetime.timezone.utc).isoformat(),
              'opening_bank': 1000, 'opening_positions': 0, 'opening_realized_pnl': 0,
              'opening_daily_volume': 0, 'copy_share_fraction': 0.5, 'mode': 'simulation',
              'backup': str(backup), 'config': config, 'state_after_start': state}
    (root / 'paper-experiment.json').write_text(json.dumps(report, indent=2))
    print(json.dumps({'experiment': {key: report[key] for key in report if key not in ['config', 'state_after_start']},
                      'current_balance': state['total_balance'], 'realized_pnl': state['realized_pnl'],
                      'positions': len(state['positions'])}, indent=2))
except Exception:
    subprocess.run(['systemctl', 'stop', 'polycopier-preview'], check=False)
    shutil.copy2(backup / 'config.toml', root / 'config.toml')
    shutil.copy2(backup / 'preview.service', unit)
    shutil.copyfile(backup / 'polycopier', root / 'target/release/polycopier')
    os.chmod(root / 'target/release/polycopier', 0o755)
    for name in names:
        (root / name).unlink(missing_ok=True)
        if (backup / name).exists():
            shutil.copy2(backup / name, root / name)
    subprocess.run(['systemctl', 'daemon-reload'], check=False)
    subprocess.run(['systemctl', 'start', 'polycopier-preview'], check=False)
    raise
