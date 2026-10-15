"""Exercise the deployed API, keeping its token in process memory."""
import base64
import json
import pathlib
import re
import tomllib
import urllib.error
import urllib.request
from decimal import Decimal

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
assert state['paper_integrity_fault'] is False
assert isinstance(state.get('filled_buy_orders'), int) and state['filled_buy_orders'] >= 0
accounting_residual = (
    Decimal(state['total_balance'])
    + Decimal(state['open_cost_basis'])
    - Decimal(state['realized_pnl'])
    - Decimal(state['starting_balance'])
)
assert abs(accounting_residual) <= Decimal('0.01'), (
    f'Paper accounting residual exceeds one cent: {accounting_residual}'
)
assert json.loads(request('/api/env')[1])['private_key'] == ''
assert request('/api/env', {}, method='POST')[0] == 405
cfg = json.loads(request('/api/config')[1])
assert float(cfg['execution']['max_trade_size_usd']) > 0
assert float(cfg['risk']['max_daily_volume_usd']) >= 0
assert not state['paused'], 'Preview must remain running during verification'
assert len(cfg['targets']['wallets']) > 0
with pathlib.Path('/opt/polycopier/config.toml').open('rb') as stream:
    saved_config = tomllib.load(stream)
api_buy_age = cfg['execution'].get('max_buy_delay_seconds')
saved_buy_age = saved_config['execution'].get('max_buy_delay_seconds')
assert api_buy_age == saved_buy_age, 'API BUY age differs from saved configuration'
if api_buy_age is not None:
    assert 0 < api_buy_age <= cfg['execution']['max_delay_seconds']

assert request('/api/ledger')[0] == 200
assert request('/api/logs')[0] == 200
code, page = request('/')
assert code == 200 and b'<html' in page
assets = [name.decode('ascii') for name in re.findall(rb'(?:src|href)="/assets/([^"/]+)"', page)]
assert any(name.endswith('.js') for name in assets), 'Dashboard JavaScript asset missing from HTML'
assert any(name.endswith('.css') for name in assets), 'Dashboard stylesheet missing from HTML'
served_assets = {}
for name in assets:
    assert re.fullmatch(r'[A-Za-z0-9_.-]+\.(?:js|css)', name), 'Unexpected dashboard asset path'
    status, contents = request('/assets/' + name)
    assert status == 200 and len(contents) > 100, f'Dashboard asset unavailable: {name}'
    served_assets[name] = contents
assert any(b'Signals skipped (all reasons)' in contents for name, contents in served_assets.items()
           if name.endswith('.js')), 'Dashboard is serving an outdated JavaScript bundle'
assert any(b'Maximum signal age for BUY' in contents for name, contents in served_assets.items()
           if name.endswith('.js')), 'Dashboard BUY signal age control is missing'
health = json.loads(request('/health', authenticated=False)[1])
report = {'authentication':'PASS','secret_redaction':'PASS','simulation_mode':'PASS','paper_accounting':'PASS','accounting_residual_usd':str(accounting_residual),'running_without_pause':'PASS','config_read':'PASS','buy_sell_age_settings':'PASS','dashboard_asset':'PASS','dashboard_asset_files':assets,'health':health}
pathlib.Path('/opt/polycopier/verification.json').write_text(json.dumps(report, indent=2))
print(json.dumps(report, indent=2))
