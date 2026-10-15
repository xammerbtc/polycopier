"""Estimate live exposure from paper lots; never reads credentials or trades.

Recorded leader dollars drive new lots; older lots use paper fill cost as a
proxy. Missing partial-exit timestamps limit the historical peak estimate,
and this is not a market replay.
"""

import argparse
import json
import pathlib
import tomllib
from datetime import datetime
from decimal import Decimal


def decimal(value) -> Decimal:
    return Decimal(str(value))


def timestamp(value: str) -> datetime:
    return datetime.fromisoformat(value.replace('Z', '+00:00'))


def budget_for(notional: Decimal, pct: Decimal, cap: Decimal) -> Decimal:
    desired = notional if notional <= 10 else notional * pct
    budget = min(desired, cap)
    return budget if budget >= 1 else Decimal(0)


def analyze(ledger: dict, profile: dict, bank: Decimal) -> dict:
    if profile['sizing']['mode'] != 'bank_scaled':
        raise ValueError('Only bank_scaled sizing is supported')
    pct = decimal(profile['sizing']['copy_size_pct'])
    cap = decimal(profile['execution']['max_trade_size_usd'])
    names = {item.split(':', 1)[0].lower(): item.rsplit(':', 1)[-1]
             for item in profile['targets']['wallets']}
    since = timestamp(ledger['performance_since'])
    events = []
    known_events = []
    known_times = []
    current = Decimal(0)
    known_current = Decimal(0)
    tracked = 0
    source_notional_lots = 0
    source_lots_meeting_five_share_minimum = 0
    source_lots_below_five_share_minimum = 0
    source_lots_below_one_dollar_minimum = 0
    partial_open = 0
    per_source = {name: Decimal(0) for name in names.values()}
    for entry in ledger['entries']:
        address = entry['source_wallet'].lower()
        entered = timestamp(entry['copied_at'])
        if address not in names or entered < since:
            continue
        shares = decimal(entry['size'])
        if shares <= 0:
            continue
        paper_cost = shares * decimal(entry['entry_price'])
        source_notional = entry.get('source_notional_usd')
        has_source_notional = source_notional is not None and decimal(source_notional) > 0
        if has_source_notional:
            exact_source_notional = decimal(source_notional)
            exact_budget = budget_for(exact_source_notional, pct, cap)
            minimum_at_paper_price = Decimal(5) * decimal(entry['entry_price']) * Decimal('1.02')
            if exact_budget >= minimum_at_paper_price:
                source_lots_meeting_five_share_minimum += 1
            else:
                source_lots_below_five_share_minimum += 1
            if exact_budget == 0:
                source_lots_below_one_dollar_minimum += 1
        notional = decimal(source_notional) if has_source_notional else paper_cost
        budget = budget_for(notional, pct, cap)
        if budget == 0:
            continue
        tracked += 1
        source_notional_lots += int(has_source_notional)
        events.append((entered, budget))
        if has_source_notional:
            known_events.append((entered, budget))
            known_times.append(entered)
        if entry.get('closed'):
            if entry.get('closed_at'):
                closed_at = timestamp(entry['closed_at'])
                events.append((closed_at, -budget))
                if has_source_notional:
                    known_events.append((closed_at, -budget))
            continue
        remaining = (decimal(entry['filled_size'])
                     if entry.get('filled_size') is not None else shares)
        remaining = max(Decimal(0), min(shares, remaining))
        estimate = budget * remaining / shares
        current += estimate
        if has_source_notional:
            known_current += estimate
        per_source[names[address]] += estimate
        if decimal(entry.get('realized_shares', 0)) > 0:
            partial_open += 1
    outstanding = Decimal(0)
    peak = Decimal(0)
    peak_at = None
    for at, delta in sorted(events, key=lambda event: event[0]):
        outstanding += delta
        if outstanding > peak:
            peak = outstanding
            peak_at = at.isoformat()
    known_outstanding = Decimal(0)
    known_peak = Decimal(0)
    for _, delta in sorted(known_events, key=lambda event: event[0]):
        known_outstanding += delta
        known_peak = max(known_peak, known_outstanding)
    known_span_seconds = int((max(known_times) - min(known_times)).total_seconds()) if len(known_times) > 1 else 0
    reserve = bank * Decimal('0.20')
    cents = Decimal('0.01')
    return {
        'bank_usd': str(bank),
        'copy_pct': str(pct),
        'planned_lots': tracked,
        'lots_with_source_notional': source_notional_lots,
        'source_lots_meeting_five_share_minimum_at_paper_price': source_lots_meeting_five_share_minimum,
        'source_lots_below_five_share_minimum_at_paper_price': source_lots_below_five_share_minimum,
        'source_lots_below_one_dollar_minimum': source_lots_below_one_dollar_minimum,
        'legacy_lots_using_paper_cost_proxy': tracked - source_notional_lots,
        'known_source_sample_span_seconds': known_span_seconds,
        'known_source_peak_usd': str(known_peak.quantize(cents)),
        'known_source_current_usd': str(known_current.quantize(cents)),
        'known_source_peak_with_reserve_fits_bank': known_peak + reserve <= bank,
        'peak_full_lot_proxy_usd': str(peak.quantize(cents)),
        'peak_at_utc': peak_at,
        'current_remaining_proxy_usd': str(current.quantize(cents)),
        'current_by_source_usd': {name: str(amount.quantize(cents))
                                  for name, amount in per_source.items()},
        'partial_open_lots_without_exit_timestamps': partial_open,
        'cash_reserve_20pct_usd': str(reserve),
        'current_with_reserve_fits_bank': current + reserve <= bank,
        'historical_peak_with_reserve_fits_bank': peak + reserve <= bank,
        'limitation': 'Five-share feasibility uses paper fill price and a 2% fee buffer, not a live order book. Legacy lots use paper cost as a source-notional proxy; skipped paper signals, partial exits and liquidity are not replayed.',
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ledger', type=pathlib.Path,
                        default=pathlib.Path('/opt/polycopier/sim_copy_ledger.json'))
    parser.add_argument('--config', type=pathlib.Path,
                        default=pathlib.Path('/opt/polycopier-live/config.toml'))
    parser.add_argument('--bank', type=Decimal, default=Decimal(1000))
    parser.add_argument('--pct', type=Decimal,
                        help='Override the staged copy fraction for comparison')
    args = parser.parse_args()
    with args.ledger.open(encoding='utf-8') as stream:
        ledger = json.load(stream)
    with args.config.open('rb') as stream:
        profile = tomllib.load(stream)
    if args.pct is not None:
        profile['sizing']['copy_size_pct'] = str(args.pct)
    print(json.dumps(analyze(ledger, profile, args.bank), indent=2))


if __name__ == '__main__':
    main()
