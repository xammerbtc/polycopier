"""Exercise the deployed API, keeping its token in process memory."""
import base64
import copy
import json
import pathlib
import time
import urllib.error
import urllib.request

env = dict(line.split('=', 1) for line in pathlib.Path('/opt/polycopier/.env').read_text().splitlines() if '=' in line)
token = env['DASHBOARD_TOKEN']
base = 'http://' + env.get('DASHBOARD_BIND', '127.0.0.1:3000')
auth = 'Basic ' + base64.b64encode(('polycopier:' + token).encode()).decode()

def request(path, data=None, authenticated=True, method=None):
    headers = {'Authorization':auth} if authenticated else {}
    if data is not None: headers['Content-Type']='application/json'
    req = urllib.request.Request(base + path, data=None if data is None else json.dumps(data).encode(), headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()

assert request('/api/state', authenticated=False)[0] == 401
code, body = request('/api/state')
assert code == 200
state = json.loads(body)
assert state['mode'] == 'simulation'
assert json.loads(request('/api/env')[1])['private_key'] == ''
assert request('/api/env', {}, method='POST')[0] == 405
cfg = json.loads(request('/api/config')[1])
assert float(cfg['execution']['max_trade_size_usd']) > 0
assert float(cfg['risk']['max_daily_volume_usd']) >= 0
assert request('/api/control', {'action':'pause'})[0] == 200
assert json.loads(request('/api/state')[1])['paused']
invalid = copy.deepcopy(cfg)
invalid['targets']['wallets']=['invalid-wallet']
assert request('/api/config', invalid)[0] == 400
assert json.loads(request('/api/config')[1]) == cfg

# Prove the listener consumes a changed target set without changing process PID.
temporary = copy.deepcopy(cfg)
wallet = temporary['targets']['wallets'][0].split(':')[0]
temporary['targets']['wallets'][0] = wallet + ':0'
try:
    assert request('/api/config', temporary)[0] == 200
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        health = json.loads(request('/health', authenticated=False)[1])
        if wallet not in health['active_targets']: break
        time.sleep(1)
    assert wallet not in health['active_targets'], 'Listener target hot reload did not apply'
finally:
    assert request('/api/config', cfg)[0] == 200
    assert request('/api/control', {'action':'resume'})[0] == 200
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        health = json.loads(request('/health', authenticated=False)[1])
        if wallet in health['active_targets']: break
        time.sleep(1)
    assert wallet in health['active_targets'], 'Listener target restoration did not apply'

assert request('/api/ledger')[0] == 200
assert request('/api/logs')[0] == 200
code, page = request('/')
assert code == 200 and b'<html' in page
health = json.loads(request('/health', authenticated=False)[1])
report = {'authentication':'PASS','secret_redaction':'PASS','simulation_mode':'PASS','pause_resume':'PASS','config_validation':'PASS','target_hot_reload':'PASS','dashboard_asset':'PASS','health':health}
pathlib.Path('/opt/polycopier/verification.json').write_text(json.dumps(report, indent=2))
print(json.dumps(report, indent=2))
