"""Read-only checks for staged live preflight reporting."""

import importlib.util
import pathlib
import unittest

source = pathlib.Path(__file__).with_name('verify-live-stage.py')
spec = importlib.util.spec_from_file_location('live_gate', source)
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class PreflightStatusTests(unittest.TestCase):
    def test_connected_unfunded_is_reported_separately(self):
        self.assertEqual(
            gate.preflight_status(0, 'PASS: Available CLOB USDC: $0.'),
            (True, '0', False),
        )

    def test_connected_funded_meets_bank(self):
        self.assertEqual(
            gate.preflight_status(0, 'PASS: Available CLOB USDC: $1000.00.'),
            (True, '1000.00', True),
        )

    def test_failed_preflight_cannot_pass_with_balance_text(self):
        self.assertEqual(
            gate.preflight_status(1, 'Available CLOB USDC: $2000'),
            (False, '2000', False),
        )

    def test_source_sample_requires_executable_coverage_and_capacity(self):
        sample = {
            'lots_with_source_notional': 100,
            'source_lots_meeting_five_share_minimum_at_paper_price': 20,
            'source_five_share_feasibility_pct': '20.0',
            'known_source_sample_span_seconds': 90000,
            'known_source_peak_with_reserve_fits_bank': True,
        }
        self.assertFalse(gate.source_sample_sufficient(sample))
        sample['source_five_share_feasibility_pct'] = '80.0'
        self.assertTrue(gate.source_sample_sufficient(sample))
        sample['known_source_peak_with_reserve_fits_bank'] = False
        self.assertFalse(gate.source_sample_sufficient(sample))


if __name__ == '__main__':
    unittest.main()
