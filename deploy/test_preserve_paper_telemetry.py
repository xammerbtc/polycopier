import importlib.util
import json
import pathlib
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location(
    'paper_telemetry', pathlib.Path(__file__).with_name('preserve-paper-telemetry.py')
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PaperTelemetryMigrationTests(unittest.TestCase):
    def test_merge_preserves_accounting_and_is_idempotent(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            state = root / 'simulation_state.json'
            backup = root / 'paper-telemetry.json'
            original = {
                'positions': {'token': {'token_id': 'token', 'size': '5', 'average_entry_price': '0.5'}},
                'total_balance': '19997.5',
                'realized_pnl': '-1',
            }
            telemetry = {
                'copies_executed': 14,
                'trades_skipped': 3,
                'live_feed': [{'original_event': {'token_id': 'token'}, 'validated': False, 'reason': 'No asks'}],
            }
            state.write_text(json.dumps(original))
            backup.write_text(json.dumps(telemetry))
            MODULE.merge(backup, state)
            merged = json.loads(state.read_text())
            self.assertEqual({key: merged[key] for key in original}, original)
            self.assertEqual({key: merged[key] for key in telemetry}, telemetry)
            MODULE.merge(backup, state)
            self.assertEqual(json.loads(state.read_text()), merged)


if __name__ == '__main__':
    unittest.main()
