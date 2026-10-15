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


if __name__ == '__main__':
    unittest.main()
