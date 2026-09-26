"""Safe temporary-tree tests: never call prepare or mutate an installed SDK."""
import importlib.util
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location('prepare_sdk', Path(__file__).parents[1] / 'prepare-macos-sdk.py')
sdk = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sdk)


class SDKTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve() / 'MacOSX26.5.sdk'
        self.root.mkdir()
        (self.root / 'file').write_text('sdk')

    def test_metadata_requires_every_pinned_field(self):
        settings = {'CanonicalName': 'macosx26.5', 'Version': '26.5'}
        system = {'ProductName': 'macOS', 'ProductVersion': '26.5', 'ProductBuildVersion': '25F70'}
        sdk.metadata(settings, system)
        for document in (settings, system):
            for key in document:
                changed = dict(document, **{key: 'wrong'})
                with self.subTest(key=key), self.assertRaises(RuntimeError):
                    sdk.metadata(changed if document is settings else settings,
                                 changed if document is system else system)

    def test_discovery_requires_pinned_version_and_build(self):
        for version, build in [('26.6', '25F70'), ('26.5', 'wrong')]:
            with mock.patch.object(sdk, 'command', side_effect=[str(self.root), version, build]):
                with self.assertRaisesRegex(RuntimeError, 'version/build'):
                    sdk.discover(True)

    def test_path_rejects_other_installations_and_malformed_paths(self):
        for path in ['/Applications/Xcode_26.6.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk',
                     'relative', str(sdk.SDKS / 'Other.sdk'), str(sdk.SDKS / 'MacOSX.sdk') + '\n']:
            with self.subTest(path=path), self.assertRaises(RuntimeError):
                sdk.sdk_path(path)

    def test_canonical_path_cannot_escape(self):
        with mock.patch.object(sdk, 'SDKS', self.root.parent):
            self.assertEqual(sdk.sdk_path(str(self.root)), self.root)
            alias = self.root.parent / 'MacOSX.sdk'
            alias.symlink_to(self.root.name)
            self.assertEqual(sdk.sdk_path(str(alias)), self.root)
            alias.unlink()
            alias.symlink_to('/tmp')
            with self.assertRaises(RuntimeError):
                sdk.sdk_path(str(alias))

    def test_plan_preserves_internal_framework_links(self):
        framework = self.root / 'Framework.framework'
        (framework / 'Versions/A').mkdir(parents=True)
        (framework / 'Versions/A/header').write_text('header')
        (framework / 'Versions/Current').symlink_to('A')
        (framework / 'header').symlink_to('Versions/Current/header')
        plan = sdk.plan_tree(self.root)
        self.assertEqual(plan[framework / 'header'][1:], ('symlink', 'Versions/Current/header'))
        self.assertEqual((framework / 'header').read_text(), 'header')

    def test_plan_rejects_external_dangling_cyclic_links(self):
        for target in ['../outside', '/tmp', 'missing', 'link', '../MacOSX26.5.sdk/file']:
            link = self.root / 'link'
            link.symlink_to(target)
            with self.subTest(target=target), self.assertRaises((RuntimeError, OSError)):
                sdk.plan_tree(self.root)
            link.unlink()

    def test_plan_rejects_special_entries_and_hardlinks(self):
        fifo = self.root / 'fifo'
        os.mkfifo(fifo)
        with self.assertRaisesRegex(RuntimeError, 'unsupported'):
            sdk.plan_tree(self.root)
        fifo.unlink()
        os.link(self.root / 'file', self.root / 'hardlink')
        with self.assertRaisesRegex(RuntimeError, 'hardlinked'):
            sdk.plan_tree(self.root)

    def test_plan_rejects_special_permission_bits(self):
        (self.root / 'file').chmod(0o4755)
        with self.assertRaisesRegex(RuntimeError, 'special permission bits'):
            sdk.plan_tree(self.root)

    def test_wrong_metadata_stops_before_any_mutation_or_selection(self):
        with mock.patch.object(sdk.sys, 'platform', 'darwin'), \
                mock.patch.object(sdk.os, 'geteuid', return_value=0), \
                mock.patch.object(sdk.sys, 'argv', ['helper']), \
                mock.patch.object(sdk, 'ACL'), \
                mock.patch.object(sdk, 'discover', return_value=(self.root, ['path', '26.5', '25F70'])), \
                mock.patch.object(sdk, 'inspect_node') as inspect, \
                mock.patch.object(sdk, 'read_metadata', side_effect=RuntimeError('SDK metadata mismatch')), \
                mock.patch.object(sdk, 'command') as command:
            with self.assertRaisesRegex(RuntimeError, 'metadata mismatch'):
                sdk.prepare()
            self.assertTrue(all(not call.kwargs.get('mutate') for call in inspect.call_args_list))
            command.assert_not_called()

    def test_open_node_rejects_symlink_in_any_component(self):
        link = self.root / 'link'
        link.symlink_to('.')
        with self.assertRaises(OSError):
            sdk.open_node(link / 'file')

    def test_metadata_reader_rejects_symlink(self):
        (self.root / 'SDKSettings.json').symlink_to('file')
        with self.assertRaises(OSError):
            sdk.read_metadata(self.root)

    def test_applications_exception_is_narrow(self):
        def info(uid, gid, mode):
            return mock.Mock(st_uid=uid, st_gid=gid, st_mode=stat.S_IFDIR | mode)
        self.assertTrue(sdk.trusted(Path('/Applications'), info(0, 80, 0o775)))
        for path, uid, gid, mode in [(self.root, 0, 80, 0o775), (Path('/Applications'), 0, 0, 0o775),
                                     (self.root, 501, 0, 0o755), (self.root, 0, 0, 0o777)]:
            self.assertFalse(sdk.trusted(path, info(uid, gid, mode)))

    @unittest.skipUnless(sys.platform == 'darwin', 'Darwin ACL API')
    def test_darwin_acl_empty_temporary_file(self):
        acl = sdk.ACL()
        fd = os.open(self.root / 'file', os.O_RDONLY)
        try:
            self.assertFalse(acl.present(fd=fd))
            acl.clear(fd)
            self.assertFalse(acl.present(fd=fd))
        finally:
            os.close(fd)

    def test_command_environment_drops_ambient_overrides(self):
        with mock.patch.object(sdk.subprocess, 'check_output', return_value='26.5\n') as run:
            sdk.command(['/usr/bin/xcrun'], True)
            self.assertEqual(run.call_args.kwargs['env'], dict(sdk.ENV, DEVELOPER_DIR=str(sdk.DEVELOPER)))
            sdk.command(['/usr/bin/xcrun'])
            self.assertEqual(run.call_args.kwargs['env'], sdk.ENV)


if __name__ == '__main__':
    unittest.main()
