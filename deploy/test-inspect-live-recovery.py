import importlib.util
import json
import pathlib
import tempfile
import unittest


source = pathlib.Path(__file__).with_name('inspect-live-recovery.py')
spec = importlib.util.spec_from_file_location('recovery_inspector', source)
inspector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inspector)


class RecoveryInspectorTests(unittest.TestCase):
    def test_reports_uncertain_orders_without_claiming_account_is_safe(self):
        with tempfile.TemporaryDirectory() as temporary:
            live = pathlib.Path(temporary)
            (live / 'execution-journal.json').write_text(json.dumps({
                'one': {'status': 'accepted'},
                'two': {'status': 'uncertain'},
            }))
            (live / 'settlement-fills.json').write_text(json.dumps([
                {'side': 'BUY'}, {'side': 'SELL'},
            ]))
            report = inspector.inspect(live)
            self.assertEqual(report['unreconciled_order_records'], 1)
            self.assertEqual(report['fill_side_counts'], {'BUY': 1, 'SELL': 1})
            self.assertFalse(report['on_chain_inventory_verified'])
            self.assertFalse(report['safe_to_resume_trading'])

    def test_rejects_corrupt_record_shape(self):
        with tempfile.TemporaryDirectory() as temporary:
            live = pathlib.Path(temporary)
            (live / 'execution-journal.json').write_text('[]')
            with self.assertRaises(ValueError):
                inspector.inspect(live)


if __name__ == '__main__':
    unittest.main()
