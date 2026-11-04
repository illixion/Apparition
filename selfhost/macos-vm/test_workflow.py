import importlib.util
import json
import re
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

    def test_shutdown_of_a_suspended_vm_whose_runner_is_gone_resumes_it_first(self):
        with tempfile.TemporaryDirectory() as d:
            bundle=Path(d)
            workflow.saved_state(bundle).write_bytes(b'state')
            states=iter([None, {'ok': True}, {'ok': True}, None])
            with patch.object(workflow,'live',side_effect=lambda b: next(states)), patch.object(workflow,'start') as start, \
                 patch.object(workflow,'control') as control:
                workflow.shutdown(bundle)
            start.assert_called_once_with(bundle, background=True)
            control.assert_called_once_with(bundle, 'shutdown')


class RestoreImageTests(unittest.TestCase):
    image = dict(version='26.0', build='TEST', size=11, sha256=__import__('hashlib').sha256(b'macos image').hexdigest(),
                 url='https://example.invalid/path/Test_Restore.ipsw')

    def test_the_pinned_build_is_the_one_the_runner_accepts(self):
        source = Path(__file__).with_name('main.swift').read_text()
        builds = re.search(r'let verifiedBuilds = \[([^\]]*)\]', source).group(1)
        self.assertEqual(re.findall(r'"([^"]+)"', builds)[0], workflow.RESTORE_IMAGE['build'])
        self.assertTrue(workflow.RESTORE_IMAGE['url'].startswith('https://updates.cdn-apple.com/'))
        self.assertIn(workflow.RESTORE_IMAGE['build'], workflow.RESTORE_IMAGE['url'])

    def test_download_is_checked_and_a_checked_file_is_not_hashed_again(self):
        with tempfile.TemporaryDirectory() as d:
            calls=[]
            def download(url, partial):
                calls.append(url); partial.write_bytes(b'macos image')
            path = workflow.fetch_ipsw(self.image, Path(d), download)
            self.assertEqual(path.read_bytes(), b'macos image')
            self.assertEqual(path.name, 'Test_Restore.ipsw')
            with patch.object(workflow.hashlib, 'sha256', side_effect=AssertionError('hashed again')):
                self.assertEqual(workflow.fetch_ipsw(self.image, Path(d), download), path)
            self.assertEqual(len(calls), 1)

    def test_a_download_that_does_not_match_is_set_aside_not_used(self):
        with tempfile.TemporaryDirectory() as d:
            def download(url, partial): partial.write_bytes(b'macos imagE')
            with self.assertRaisesRegex(RuntimeError, 'does not match'):
                workflow.fetch_ipsw(self.image, Path(d), download)
            self.assertFalse(Path(d, 'Test_Restore.ipsw').exists())
            self.assertTrue(Path(d, 'Test_Restore.ipsw.bad').exists())

    def test_a_short_download_is_kept_to_resume(self):
        with tempfile.TemporaryDirectory() as d:
            def download(url, partial): partial.write_bytes(b'macos')
            with self.assertRaisesRegex(RuntimeError, 'run this again'):
                workflow.fetch_ipsw(self.image, Path(d), download)
            self.assertEqual(Path(d, 'Test_Restore.ipsw.partial').read_bytes(), b'macos')


import os
import plistlib
import re
import subprocess

# Services whose absence the guest cannot take: loginwindow logs the session out without the shell trio, and the
# others are looked up again and again by processes that stay (a disabled biometrickitd was hit 9,000 times in four
# minutes, a disabled bluetoothd made coreaudiod leak 40 MB a minute). Never put these in the lists.
NEVER_DISABLE = {'com.apple.Finder', 'com.apple.Dock.agent', 'com.apple.SystemUIServer.agent', 'com.apple.biometrickitd',
                 'com.apple.bluetoothd', 'com.apple.storagekitd', 'com.apple.analyticsd', 'com.apple.symptomsd',
                 'com.apple.sysmond', 'com.apple.loginwindow', 'com.apple.WindowServer', 'com.apple.opendirectoryd',
                 'com.apple.securityd', 'com.apple.cfprefsd.xpc.daemon', 'com.apple.mobile.keybagd',
                 'com.illixion.apparition.anisette', 'com.illixion.apparition.vm-anisette', 'com.illixion.apparition.vm-bridge'}


def labels(name):
    text = (Path(__file__).with_name(name)).read_text()
    return [line for line in text.splitlines() if line and not line.startswith('#')]


class TrimTests(unittest.TestCase):
    def test_lists_are_plain_unique_and_keep_what_login_needs(self):
        for name in ['appliance-disabled-user.txt', 'appliance-disabled-system.txt']:
            found = labels(name)
            self.assertGreater(len(found), 50, name)
            self.assertEqual(len(found), len(set(found)), f'duplicates in {name}')
            for label in found:
                self.assertRegex(label, r'^[A-Za-z0-9._-]+$', name)
                self.assertNotRegex(label, r'[0-9A-F]{8}-[0-9A-F]{4}-', f'{label} is machine specific')
            self.assertFalse(NEVER_DISABLE & set(found), f'{name} disables {NEVER_DISABLE & set(found)}')

    @unittest.skipUnless(Path('/usr/libexec/PlistBuddy').exists(), 'needs macOS')
    def test_trim_writes_overrides_merges_and_never_touches_the_shell(self):
        with tempfile.TemporaryDirectory() as d:
            launchd = Path(d, 'private/var/db/com.apple.xpc.launchd')
            launchd.mkdir(parents=True)
            with open(launchd / 'disabled.plist', 'wb') as f:
                plistlib.dump({'com.example.mine': True, 'com.apple.softwareupdated': False}, f)
            env = dict(os.environ, APPARITION_TRIM_TEST='1', APPARITION_KEEP_SSH='1')
            result = subprocess.run(['/bin/bash', str(Path(__file__).with_name('appliance-trim.sh')), d], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            user = plistlib.loads((launchd / 'disabled.501.plist').read_bytes())
            system = plistlib.loads((launchd / 'disabled.plist').read_bytes())
            self.assertEqual({k for k, v in user.items() if v is True}, set(labels('appliance-disabled-user.txt')))
            self.assertTrue(set(labels('appliance-disabled-system.txt')) <= {k for k, v in system.items() if v is True})
            self.assertIs(system['com.example.mine'], True, 'an override the person set must survive')
            self.assertIs(system['com.apple.softwareupdated'], True, 'the trim wins for its own labels')
            self.assertIs(system['com.openssh.sshd'], False, 'the development flow keeps SSH')
            self.assertTrue(Path(d, '.metadata_never_index').exists())

    @unittest.skipUnless(Path('/usr/libexec/PlistBuddy').exists(), 'needs macOS')
    def test_a_label_taken_off_a_list_is_put_back_on_vms_that_already_have_it(self):
        here = Path(__file__).parent
        with tempfile.TemporaryDirectory() as d:
            tool = Path(d, 'tool'); tool.mkdir()
            data = Path(d, 'data'); launchd = data / 'private/var/db/com.apple.xpc.launchd'; launchd.mkdir(parents=True)
            (tool / 'appliance-trim.sh').write_text((here / 'appliance-trim.sh').read_text())
            (tool / 'appliance-disabled-user.txt').write_text('com.example.one\ncom.example.two\n')
            (tool / 'appliance-disabled-system.txt').write_text('com.example.sys\ncom.example.badidea\n')
            with open(launchd / 'disabled.plist', 'wb') as f:
                plistlib.dump({'com.example.mine': True}, f)
            env = dict(os.environ, APPARITION_TRIM_TEST='1')
            def run():
                r = subprocess.run(['/bin/bash', str(tool / 'appliance-trim.sh'), str(data)], env=env, capture_output=True, text=True)
                self.assertEqual(r.returncode, 0, r.stderr)
                return (plistlib.loads((launchd / 'disabled.plist').read_bytes()),
                        plistlib.loads((launchd / 'disabled.501.plist').read_bytes()))
            system, user = run()
            self.assertEqual({k for k, v in system.items() if v}, {'com.example.mine', 'com.example.sys', 'com.example.badidea'})
            # The list is corrected: badidea off it, one added to the user list.
            (tool / 'appliance-disabled-system.txt').write_text('com.example.sys\n')
            (tool / 'appliance-disabled-user.txt').write_text('com.example.one\n')
            system, user = run()
            self.assertEqual({k for k, v in system.items() if v}, {'com.example.mine', 'com.example.sys'})
            self.assertNotIn('com.example.badidea', system)
            self.assertEqual({k for k, v in user.items() if v}, {'com.example.one'})

    def test_trim_refuses_to_run_as_anyone_but_root(self):
        if os.getuid() == 0:
            self.skipTest('running as root')
        with tempfile.TemporaryDirectory() as d:
            result = subprocess.run(['/bin/bash', str(Path(__file__).with_name('appliance-trim.sh')), d], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('run as root', result.stderr)


if __name__=='__main__':unittest.main()
