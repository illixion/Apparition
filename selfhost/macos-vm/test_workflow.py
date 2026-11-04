import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('vm_workflow',Path(__file__).with_name('vm.py'))
workflow=importlib.util.module_from_spec(spec)
spec.loader.exec_module(workflow)


class WorkflowTests(unittest.TestCase):
    def test_guest_report_from_previous_boot_cannot_advance_workflow(self):
        with tempfile.TemporaryDirectory() as d:
            bundle=Path(d)
            (bundle/'guest-status').mkdir()
            (bundle/'guest-status/boot-id').write_text('new-boot')
            (bundle/'guest-status/report.json').write_text(json.dumps(dict(boot='old-boot',owner=True,relay=True)))
            self.assertIsNone(workflow.report(bundle))
            (bundle/'guest-status/report.json').write_text(json.dumps(dict(boot='new-boot',owner=True)))
            self.assertTrue(workflow.report(bundle)['owner'])

    def test_unmanaged_existing_disk_is_never_restored(self):
        with tempfile.TemporaryDirectory() as d:
            class Args: pass
            args=Args();args.bundle=Path(d);args.ipsw=Path(d)/'restore.ipsw';args.public_key=Path(__file__)
            (args.bundle/'disk.img').write_bytes(b'user-data')
            with patch.object(workflow,'ensure_runner'), patch.object(workflow,'payload_hash',return_value='same'), patch.object(workflow.subprocess,'run') as run:
                with self.assertRaisesRegex(RuntimeError,'Unmanaged existing bundle'):
                    workflow.prepare(args)
                run.assert_not_called()
            self.assertEqual((args.bundle/'disk.img').read_bytes(),b'user-data')

    def test_resume_prepared_install_does_not_repeat_restore_or_rotate_credentials(self):
        with tempfile.TemporaryDirectory() as d:
            class Args: pass
            args=Args();args.bundle=Path(d);args.ipsw=Path(d)/'restore.ipsw';args.public_key=Path(__file__)
            for name in ['restore-build','installed']:(args.bundle/name).touch()
            (args.bundle/'prepared').write_text('same')
            (args.bundle/'credentials.txt').write_text('retained')
            with patch.object(workflow,'ensure_runner'), patch.object(workflow,'payload_hash',return_value='same'), patch.object(workflow.subprocess,'run') as run:
                workflow.prepare(args)
                run.assert_not_called()
            self.assertEqual((args.bundle/'credentials.txt').read_text(),'retained')


if __name__=='__main__':unittest.main()
