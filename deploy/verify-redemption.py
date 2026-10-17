#!/usr/bin/env python3
"""Verify authenticated paper redemption and repeat-request idempotency on VPS."""
import base64
import json
from decimal import Decimal
from pathlib import Path
from urllib.request import Request, urlopen

env = dict(line.split('=', 1) for line in Path('.env').read_text().splitlines() if '=' in line and not line.startswith('#'))
auth = 'Basic ' + base64.b64encode(('polycopier:' + env['DASHBOARD_TOKEN']).encode()).decode()
def api(path, method='GET'):
    with urlopen(Request('http://127.0.0.1:3000' + path, method=method, headers={'Authorization': auth}), timeout=60) as response:
        return json.load(response)

before = api('/api/state')
assert before['mode'] == 'simulation'
first = api('/api/redeem', 'POST')
after = api('/api/state')
assert first['on_chain_transactions'] == 0
assert Decimal(after['total_balance']) == Decimal(before['total_balance']) + sum((Decimal(entry[1]) for entry in first['settled']), Decimal(0))
assert Decimal(after['realized_pnl']) == Decimal(before['realized_pnl']) + sum((Decimal(entry[2]) for entry in first['settled']), Decimal(0))
second = api('/api/redeem', 'POST')
assert second['settled'] == []
repeated = api('/api/state')
assert repeated['total_balance'] == after['total_balance']
assert repeated['realized_pnl'] == after['realized_pnl']
print(json.dumps({'paper_redemption': 'PASS', 'settled_count':len(first['settled']), 'lookup_failures':len(first['lookup_failures']), 'repeat_idempotency':'PASS', 'metrics':api('/api/metrics')}))
