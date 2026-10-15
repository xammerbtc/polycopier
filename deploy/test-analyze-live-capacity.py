"""Financial sanity checks for the historical capital proxy."""

import importlib.util
import pathlib
import unittest
from decimal import Decimal

source = pathlib.Path(__file__).with_name('analyze-live-capacity.py')
spec = importlib.util.spec_from_file_location('capacity', source)
capacity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capacity)


class CapacityTests(unittest.TestCase):
    def test_bank_scaled_budget_preserves_ratio_above_small_signal_band(self):
        pct = Decimal('0.005')
        cap = Decimal(100)
        self.assertEqual(capacity.budget_for(Decimal(10), pct, cap), 10)
        self.assertEqual(capacity.budget_for(Decimal(30), pct, cap), 0)
        self.assertEqual(capacity.budget_for(Decimal(1000), pct, cap), 5)
        self.assertEqual(capacity.budget_for(Decimal(100000), pct, cap), 100)

    def test_peak_counts_overlapping_lots_and_current_respects_partial_sale(self):
        profile = {
            'targets': {'wallets': ['a:1:Alpha', 'b:1:Beta']},
            'sizing': {'mode': 'bank_scaled', 'copy_size_pct': 0.005},
            'execution': {'max_trade_size_usd': 100},
        }
        ledger = {
            'performance_since': '2026-10-14T00:00:00Z',
            'entries': [
                {'source_wallet': 'a', 'size': '1000', 'filled_size': '500',
                 'entry_price': '0.5', 'copied_at': '2026-10-14T01:00:00Z',
                 'closed': False, 'realized_shares': '500'},
                {'source_wallet': 'b', 'size': '1000', 'filled_size': '0',
                 'entry_price': '0.5', 'copied_at': '2026-10-14T02:00:00Z',
                 'closed': True, 'closed_at': '2026-10-14T03:00:00Z'},
            ],
        }
        result = capacity.analyze(ledger, profile, Decimal(10))
        self.assertEqual(result['peak_full_lot_proxy_usd'], '5.00')
        self.assertEqual(result['current_remaining_proxy_usd'], '1.25')
        self.assertEqual(result['partial_open_lots_without_exit_timestamps'], 1)

    def test_source_dollar_signal_prevents_partial_paper_fill_underestimate(self):
        profile = {
            'targets': {'wallets': ['a:1:Alpha']},
            'sizing': {'mode': 'bank_scaled', 'copy_size_pct': 0.01},
            'execution': {'max_trade_size_usd': 100},
        }
        ledger = {
            'performance_since': '2026-10-14T00:00:00Z',
            'entries': [{
                'source_wallet': 'a', 'size': '100', 'filled_size': '100',
                'entry_price': '0.5', 'source_notional_usd': '1000',
                'copied_at': '2026-10-14T01:00:00Z', 'closed': False,
            }],
        }
        result = capacity.analyze(ledger, profile, Decimal(1000))
        self.assertEqual(result['peak_full_lot_proxy_usd'], '10.00')
        self.assertEqual(result['lots_with_source_notional'], 1)
        self.assertEqual(result['legacy_lots_using_paper_cost_proxy'], 0)
        self.assertEqual(result['known_source_peak_usd'], '10.00')
        self.assertEqual(result['known_source_current_usd'], '10.00')
        self.assertEqual(result['known_source_sample_span_seconds'], 0)
        self.assertEqual(result['source_lots_meeting_five_share_minimum_at_paper_price'], 1)

    def test_five_share_feasibility_uses_exact_source_notional(self):
        profile = {
            'targets': {'wallets': ['a:1:Alpha']},
            'sizing': {'mode': 'bank_scaled', 'copy_size_pct': 0.01},
            'execution': {'max_trade_size_usd': 100},
        }
        ledger = {
            'performance_since': '2026-10-14T00:00:00Z',
            'entries': [{
                'source_wallet': 'a', 'size': '200', 'filled_size': '200',
                'entry_price': '0.5', 'source_notional_usd': '30',
                'copied_at': '2026-10-14T01:00:00Z', 'closed': False,
            }],
        }
        result = capacity.analyze(ledger, profile, Decimal(1000))
        self.assertEqual(result['source_lots_below_five_share_minimum_at_paper_price'], 1)
        self.assertEqual(result['source_lots_below_one_dollar_minimum'], 1)
        self.assertEqual(result['planned_lots'], 0)


if __name__ == '__main__':
    unittest.main()
