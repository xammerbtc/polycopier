"""Read-only diagnostics for the paper profit guard; never prints credentials."""
import base64
import json
import pathlib
import urllib.request
from decimal import Decimal
from datetime import datetime, timezone

env = dict(line.split('=', 1) for line in pathlib.Path('/opt/polycopier/.env').read_text().splitlines() if '=' in line)
auth = base64.b64encode(('polycopier:' + env['DASHBOARD_TOKEN']).encode()).decode()
request = urllib.request.Request('http://127.0.0.1:3000/api/state', headers={'Authorization': 'Basic ' + auth})
with urllib.request.urlopen(request, timeout=20) as response:
    state = json.load(response)
config_request = urllib.request.Request('http://127.0.0.1:3000/api/config', headers={'Authorization': 'Basic ' + auth})
with urllib.request.urlopen(config_request, timeout=20) as response:
    config = json.load(response)

positions = state.get('positions', {})
if isinstance(positions, list):
    positions = {p['token_id']: p for p in positions}
prices = state.get('position_prices', {})
details = state.get('position_details', {})
updated = state.get('price_updated_at', {})
now = int(datetime.now(timezone.utc).timestamp())
cash = Decimal(str(state.get('total_balance', '0')))
bank = Decimal(str(state.get('starting_balance', '20000')))
equity_floor = cash
fresh = 0
eligible = 0
unpriced = 0
closed = 0
for token, position in positions.items():
    detail = details.get(token) or {}
    if detail.get('closed'):
        closed += 1
        continue
    if detail.get('price_source') != 'CLOB midpoint' or now - int(updated.get(token, 0)) > 30 or token not in prices:
        unpriced += 1
        continue
    price = Decimal(str(prices[token]))
    basis = Decimal(str(position['average_entry_price']))
    size = Decimal(str(position['size']))
    equity_floor += size * price
    fresh += 1
    if basis > 0 and price <= basis * Decimal('0.90'):
        eligible += 1
print(json.dumps({'cash': str(cash), 'bank': str(bank),
                  'trigger_pct': str(Decimal(str(config['risk']['profit_guard_trigger_pct'])) * 100),
                  'loss_pct': str(Decimal(str(config['risk']['profit_guard_loss_pct'])) * 100),
                  'equity_lower_bound': str(equity_floor),
                  'gain_lower_bound_pct': str((equity_floor / bank - 1) * 100) if bank else None,
                  'fresh_open': fresh, 'unpriced_open': unpriced,
                  'closed_awaiting_settlement': closed,
                  'loss_candidates_at_10pct': eligible}, indent=2))
