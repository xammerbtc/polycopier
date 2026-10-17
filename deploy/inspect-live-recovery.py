"""Read-only summary of durable live records before manual reconciliation.

Local records do not prove current on-chain ownership or spendable collateral.
This tool never starts the trading service or submits orders.
"""

import argparse
import collections
import json
import pathlib
import sys


FILES = {
    'orders': ('execution-journal.json', dict),
    'fills': ('settlement-fills.json', list),
    'redemptions': ('redemption-journal.json', dict),
}


def inspect(live_dir: pathlib.Path) -> dict:
    data = {}
    for key, (filename, expected_type) in FILES.items():
        path = live_dir / filename
        if not path.exists():
            data[key] = expected_type()
            continue
        value = json.loads(path.read_text(encoding='utf-8'))
        if not isinstance(value, expected_type):
            raise ValueError(f'{filename}: unexpected JSON type')
        data[key] = value
    orders = data['orders']
    fills = data['fills']
    redemptions = data['redemptions']
    order_status = collections.Counter(record.get('status', 'missing')
                                       for record in orders.values())
    redemption_status = collections.Counter(record.get('status', 'missing')
                                            for record in redemptions.values())
    fill_sides = collections.Counter(fill.get('side', 'missing') for fill in fills)
    return {
        'durable_order_records': len(orders),
        'order_status_counts': dict(sorted(order_status.items())),
        'unreconciled_order_records': sum(order_status[status]
                                          for status in ('pending', 'uncertain')),
        'finalized_fill_records': len(fills),
        'fill_side_counts': dict(sorted(fill_sides.items())),
        'redemption_records': len(redemptions),
        'redemption_status_counts': dict(sorted(redemption_status.items())),
        'on_chain_inventory_verified': False,
        'safe_to_resume_trading': False,
        'note': 'Compare these records with CLOB orders, finalized fills, and on-chain balances before resuming live trading.',
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--live-dir', type=pathlib.Path,
                        default=pathlib.Path('/opt/polycopier-live'))
    args = parser.parse_args()
    try:
        report = inspect(args.live_dir)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f'Live recovery records could not be verified: {error}', file=sys.stderr)
        raise SystemExit(1) from None
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
