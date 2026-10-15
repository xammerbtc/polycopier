"""Read-only, secret-free summary of the current paper signal feed."""

import base64
import collections
import json
import pathlib
import urllib.request


def summarize(state: dict) -> dict:
    feed = state.get('live_feed', [])
    reasons = collections.Counter(item.get('reason') or 'no skip reason'
                                  for item in feed if not item.get('validated', False))
    return {
        'mode': state.get('mode'),
        'feed_entries': len(feed),
        'validated_in_feed': sum(bool(item.get('validated')) for item in feed),
        'skipped_in_feed': sum(reasons.values()),
        'skip_reasons_in_feed': dict(reasons.most_common(20)),
        'skip_attempts_by_category_since_tracking': state.get('skip_attempts', {}),
        'trades_skipped_since_reset': state.get('trades_skipped'),
        'signals_validated_since_reset': state.get('copies_executed'),
        'paper_cash_usd': state.get('total_balance'),
        'paper_open_positions': len(state.get('positions', {})),
        'paper_realized_pnl_usd': state.get('realized_pnl'),
        'settlement_last_attempt': state.get('paper_settlement_last_attempt'),
        'settlement_last_lookup': state.get('paper_settlement_last_lookup'),
        'settlement_market_failures': state.get('paper_settlement_market_failures'),
        'settlement_last_error': state.get('paper_settlement_last_error'),
        'feed_is_bounded_sample': True,
    }


def main() -> None:
    env = dict(line.split('=', 1) for line in
               pathlib.Path('/opt/polycopier/.env').read_text().splitlines()
               if '=' in line and not line.lstrip().startswith('#'))
    token = env['DASHBOARD_TOKEN'].strip('"\'')
    bind = env.get('DASHBOARD_BIND', '127.0.0.1:3000').strip('"\'')
    if not bind.startswith('127.0.0.1:'):
        raise ValueError('Dashboard must be bound to loopback')
    auth = base64.b64encode(('polycopier:' + token).encode()).decode()
    request = urllib.request.Request('http://' + bind + '/api/state',
                                     headers={'Authorization': 'Basic ' + auth})
    with urllib.request.urlopen(request, timeout=15) as response:
        state = json.load(response)
    print(json.dumps(summarize(state), indent=2))


if __name__ == '__main__':
    main()
