import importlib.util
import os
import pathlib
import tempfile
import unittest
spec=importlib.util.spec_from_file_location('maintenance', pathlib.Path(__file__).with_name('storage-maintenance.py'))
maintenance=importlib.util.module_from_spec(spec);spec.loader.exec_module(maintenance)

class CleanupTests(unittest.TestCase):
    def test_cleanup_keeps_binary_and_accounting_even_with_hardlinks(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=pathlib.Path(temporary);target=root/'target'
            (target/'release/deps').mkdir(parents=True)
            (target/'debug/deps').mkdir(parents=True)
            binary=target/'release/polycopier';binary.write_bytes(b'RUNNING-BINARY')
            os.link(binary,target/'release/deps/linked-binary')
            (target/'debug/deps/test').write_bytes(b'unneeded')
            state=root/'simulation_state.json';state.write_text('precious-accounting')
            maintenance.clean_build(target)
            self.assertEqual(binary.read_bytes(),b'RUNNING-BINARY')
            self.assertEqual(state.read_text(),'precious-accounting')
            self.assertFalse((target/'debug').exists())
            self.assertFalse((target/'release/deps').exists())

    def test_symlink_escape_aborts_before_any_deletion(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=pathlib.Path(temporary);target=root/'target';outside=root/'outside'
            target.mkdir();outside.mkdir();(outside/'protected').write_text('keep')
            (target/'debug').symlink_to(outside,target_is_directory=True)
            with self.assertRaises(RuntimeError): maintenance.clean_build(target)
            self.assertEqual((outside/'protected').read_text(),'keep')

    def test_backup_retention_preserves_milestones_and_latest_three(self):
        with tempfile.TemporaryDirectory() as temporary:
            base=pathlib.Path(temporary)
            for name in ['20261001T000000Z','20261002T000000Z','20261003T000000Z-fresh-polling','20261004T000000Z','20260901T000000Z-experiment-reset','unknown']:
                (base/name).mkdir()
            self.assertEqual([p.name for p in maintenance.old_backups(base)],['20261001T000000Z'])

if __name__=='__main__': unittest.main()
