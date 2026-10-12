"""Upgrade paper preview without replaying fills from the current experiment.
Run under production.lock after a successful build. Captured keys are committed
with the stopped snapshot; subsequent fills persist their own keys atomically.
"""
import base64
import datetime
import decimal
import json
import os
import pathlib
import shutil
import subprocess
import time
import urllib.request
root = pathlib.Path('/opt/polycopier')
assert (root / 'build.phase').read_text().strip() == 'complete'
assert (root / 'build.exit').read_text().strip() == '0'
env = dict(line.split('=', 1) for line in (root / '.env').read_text().splitlines() if '=' in line)
auth = 'Basic ' + base64.b64encode(('polycopier:' + env['DASHBOARD_TOKEN']).encode()).decode()
def request(path, data=None):
    headers = {'Authorization': auth}
    if data is not None: headers['Content-Type'] = 'application/json'
    req = urllib.request.Request('http://127.0.0.1:3000' + path, headers=headers,
                                 data=None if data is None else json.dumps(data).encode())
    with urllib.request.urlopen(req, timeout=10) as response: return json.load(response)
def number(value):
    return format(decimal.Decimal(str(value)).quantize(decimal.Decimal('0.000001')).normalize(), 'f')
def key(event):
    return ':'.join([event['transaction_hash'].lower(), event['taker_address'].lower(),
                     event['token_id'], event['side'], number(event['price']), number(event['size'])])
state = request('/api/state')
assert state['mode'] == 'simulation' and not state['paused']
request('/api/control', {'action': 'pause'})
stopped = False
backup = None
try:
    for _ in range(50):
        state = request('/api/state')
        if not state['pending_orders']: break
        time.sleep(0.1)
    assert not state['pending_orders'], 'Pending fills must finish before checkpoint'
    metrics = request('/api/metrics')
    accepted = {key(e['original_event']): e['original_event']['timestamp']
                for e in state['live_feed'] if e['validated'] and e['reason'] is None}
    prior = json.loads((root / 'simulation_state.json').read_text()).get('processed_signals', {})
    assert len(accepted) == metrics['order_acceptances'], 'Feed cannot prove all prior accepted signals; do not restart'
    if accepted: assert metrics['last_accept_event_key'] in accepted
    accepted.update(prior)
    pid = subprocess.check_output(['systemctl', 'show', 'polycopier-preview', '-p', 'MainPID', '--value'], text=True).strip()
    backup = root / 'deploy-backups' / (datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ') + '-fresh-polling')
    backup.mkdir(mode=0o700)
    shutil.copyfile('/proc/' + pid + '/exe', backup / 'polycopier')
    subprocess.run(['systemctl', 'stop', 'polycopier-preview'], check=True)
    stopped = True
    for name in ['simulation_state.json', 'sim_copy_ledger.json', 'runtime-risk.json', 'config.toml']:
        if (root / name).exists(): shutil.copy2(root / name, backup / name)
    snapshot_path = root / 'simulation_state.json'
    snapshot = json.loads(snapshot_path.read_text())
    assert snapshot['total_balance'] == state['total_balance']
    assert snapshot['positions'] == state['positions']
    snapshot.setdefault('processed_signals', {}).update(accepted)
    with snapshot_path.open('w') as stream:
        json.dump(snapshot, stream, indent=2)
        stream.flush()
        os.fsync(stream.fileno())
    subprocess.run(['systemctl', 'start', 'polycopier-preview'], check=True)
    for attempt in range(30):
        try:
            fresh = request('/api/state')
            assert fresh['mode'] == 'simulation' and not fresh['paused']
            break
        except Exception:
            if attempt == 29: raise
            time.sleep(1)
    print(json.dumps({'backup': str(backup), 'checkpointed_fills': len(accepted),
                      'preserved_balance_before_restart': state['total_balance'],
                      'positions_before_restart': len(state['positions']),
                      'balance_after_restart': fresh['total_balance'],
                      'positions_after_restart': len(fresh['positions'])}, indent=2))
except Exception:
    if not stopped:
        request('/api/control', {'action': 'resume'})
    else:
        # Keep any newly committed inventory; do not blindly replay old binary.
        print('Upgrade needs attention; preserved backup at', backup)
    raise
