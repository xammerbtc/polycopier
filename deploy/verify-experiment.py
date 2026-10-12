"""Read-only check of the current paper experiment and available public fills."""
import base64
import datetime
import json
import pathlib
import subprocess
import time
import urllib.parse
import urllib.request

root = pathlib.Path('/opt/polycopier')
env = dict(line.split('=', 1) for line in (root / '.env').read_text().splitlines() if '=' in line)
auth = 'Basic ' + base64.b64encode(('polycopier:' + env['DASHBOARD_TOKEN']).encode()).decode()
def get(path):
    req = urllib.request.Request('http://127.0.0.1:3000' + path, headers={'Authorization': auth})
    with urllib.request.urlopen(req, timeout=10) as response:
        return json.load(response)
state, cfg, metrics, health = (get(p) for p in ['/api/state', '/api/config', '/api/metrics', '/health'])
report = json.loads((root / 'paper-experiment.json').read_text())
assert state['mode'] == 'simulation' and not state['paused']
assert report['opening_bank'] == 10000 and report['copy_share_fraction'] == 1
assert cfg['targets']['wallets'][0].endswith(':1.0')
assert float(cfg['sizing']['copy_size_pct']) == 1
assert cfg['execution']['max_delay_seconds'] == 300
assert float(cfg['risk']['max_daily_volume_usd']) == 0 and cfg['risk']['max_consecutive_losses'] == 0
assert health['websocket_connected']
assert time.time() - health['last_poll_success'] < 15
start = int(datetime.datetime.fromisoformat(report['started_at_utc']).timestamp())
wallet = cfg['targets']['wallets'][0].split(':')[0]
query = urllib.parse.urlencode({'user': wallet, 'takerOnly': 'false', 'limit': 1000})
public = json.loads(subprocess.check_output(['curl', '--fail', '--silent', '--show-error', '--max-time', '15', 'https://data-api.polymarket.com/trades?' + query], text=True))
# Allow ten seconds for propagation and observation; only compare the recoverable window.
cutoff = max(start, int(time.time()) - 280)
public = [t for t in public if cutoff <= t['timestamp'] <= time.time() - 10]
feed = state['live_feed']
def key(t, remote=False):
    return (t['transactionHash' if remote else 'transaction_hash'].lower(),
            str(t['asset' if remote else 'token_id']), t['side'])
observed = {key(e['original_event']) for e in feed}
missing = sorted({key(t, True) for t in public} - observed)
summary = {'started_at_utc': report['started_at_utc'], 'opening_bank': report['opening_bank'],
           'balance': state['total_balance'], 'positions': len(state['positions']),
           'realized_pnl': state['realized_pnl'], 'health': health, 'metrics': metrics,
           'public_fills_in_observation_window': len(public), 'missing_transaction_token_sides': missing,
           'feed_reasons': [e['reason'] for e in feed if not e['validated']],
           'backup': report['backup']}
print(json.dumps(summary, indent=2))
assert not missing, 'Public fills absent from the recent evaluated feed; investigate before claiming coverage'
