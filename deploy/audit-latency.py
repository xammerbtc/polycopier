"""Read-only latency counters from the loopback paper API; never prints tokens."""

import base64
import json
import pathlib
import urllib.request


def main() -> None:
    env = dict(line.split('=', 1) for line in
               pathlib.Path('/opt/polycopier/.env').read_text().splitlines()
               if '=' in line and not line.lstrip().startswith('#'))
    token = env['DASHBOARD_TOKEN'].strip('"\'')
    bind = env.get('DASHBOARD_BIND', '127.0.0.1:3000').strip('"\'')
    if not bind.startswith('127.0.0.1:'):
        raise ValueError('Dashboard must be bound to loopback')
    auth = base64.b64encode(('polycopier:' + token).encode()).decode()
    request = urllib.request.Request('http://' + bind + '/api/metrics',
                                     headers={'Authorization': 'Basic ' + auth})
    with urllib.request.urlopen(request, timeout=15) as response:
        metrics = json.load(response)
    keys = (
        'execution_mode', 'unique_signals_delivered', 'duplicate_signals',
        'expired_signals', 'polling_errors', 'polling_page_limit_hits',
        'last_poll_duration_ms', 'websocket_target_events', 'polling_target_events',
        'order_dispatches', 'last_receive_to_dispatch_ms',
        'last_fill_to_dispatch_upper_bound_ms', 'order_acceptances',
        'last_receive_to_accept_ms', 'last_fill_to_accept_upper_bound_ms',
        'last_websocket_fill_age_upper_bound_ms', 'last_source',
        'dispatch_latency_samples', 'receive_to_dispatch_p50_ms',
        'receive_to_dispatch_p95_ms', 'receive_to_dispatch_p99_ms',
        'acceptance_latency_samples', 'receive_to_accept_p50_ms',
        'receive_to_accept_p95_ms',
    )
    print(json.dumps({key: metrics.get(key) for key in keys}, indent=2))


if __name__ == '__main__':
    main()
