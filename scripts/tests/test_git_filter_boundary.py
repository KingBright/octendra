"""Native Git mechanism regression, synthetic repositories only; no Cargo.

Exercises the actual fixed plumbing argv extracted from change_review.rs.
This proves helper execution behavior, not compiled Rust integration.
"""
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[2] / 'crates/octendra-code/src/change_review.rs'


def raw_commands():
    source = SOURCE.read_text()
    index = re.search(r'git\(\s*ws,\s*&\[("ls-files"[^]]+)\],\s*paths,?\s*\)', source)
    batch = re.search(r'git_output\(\s*ws,\s*&\[("cat-file"[^]]+)\],\s*None,\s*Some\(input\)', source)
    if not index or not batch:
        raise AssertionError('production raw plumbing command contract missing')
    return tuple(re.findall(r'"([^"\\]+)"', match.group(1)) for match in (index, batch))


class GitFilterBoundaryTests(unittest.TestCase):
    def exercise(self, mode):
        index_args, batch_args = raw_commands()
        with tempfile.TemporaryDirectory(prefix='octendra-filter-') as directory:
            root = Path(directory)
            marker = root/'marker'
            repo = root/'repository'
            repo.mkdir()
            env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
            env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM='1', GIT_ATTR_NOSYSTEM='1')

            def git(*args, data=None):
                return subprocess.run(['git', '--no-pager', '--no-replace-objects',
                    '-c', 'core.fsmonitor=false', '-c', f'core.hooksPath={os.devnull}',
                    '-c', f'core.attributesFile={os.devnull}', *args],
                    cwd=repo, env=env, input=data, capture_output=True, timeout=10)

            self.assertEqual(git('init', '-q').returncode, 0)
            (repo/'state.txt').write_text('before\n')
            self.assertEqual(git('add', 'state.txt').returncode, 0)
            self.assertEqual(git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture').returncode, 0)
            oid = git('hash-object', '-w', '--no-filters', '--stdin', data=b'staged\n').stdout.strip().decode()
            self.assertEqual(git('update-index', '--cacheinfo', f'100644,{oid},state.txt').returncode, 0)
            (repo/'state.txt').write_text('after\n')
            (repo/'.gitattributes').write_text('state.txt filter=fixture\n')
            command = 'printf fixture > ' + shlex.quote(str(marker)) + '; exit 1'
            self.assertEqual(git('config', f'filter.fixture.{mode}', command).returncode, 0)
            # Red control: the previous command does execute the fixture helper.
            unsafe = git('diff', '--no-ext-diff', '--no-textconv', '--no-color',
                         '--ignore-submodules=all', '--', 'state.txt')
            self.assertTrue(marker.exists(), f'{mode}: red control did not reproduce')
            marker.unlink()
            index = git(*index_args, '--', 'state.txt')
            self.assertEqual(index.returncode, 0)
            indexed_oid = index.stdout.split(b'\t', 1)[0].split()[1]
            # A new driver after index enumeration defeats config-name preflights.
            (repo/'.gitattributes').write_text('state.txt filter=changed-after-index\n')
            self.assertEqual(git('config', f'filter.changed-after-index.{mode}', command).returncode, 0)
            batch = git(*batch_args, data=indexed_oid+b'\n')
            self.assertEqual(batch.returncode, 0)
            header, body = batch.stdout.split(b'\n', 1)
            self.assertEqual(header.split(), [indexed_oid, b'blob', b'7'])
            self.assertEqual(body, b'staged\n\n')
            staged = git('diff', '--cached', '--no-ext-diff', '--no-textconv', '--no-color',
                         '--ignore-submodules=all', '--', 'state.txt')
            self.assertEqual(staged.returncode, 0)
            self.assertIn(b'-before', staged.stdout)
            self.assertIn(b'+staged', staged.stdout)
            self.assertFalse(marker.exists(), f'{mode}: plumbing executed conversion helper')
            return {'mode': mode, 'unsafe_exit': unsafe.returncode, 'unsafe_helper_executed': True,
                    'raw_blob_correct': True, 'staged_diff_correct': True,
                    'post_index_driver_change_helper_executed': False}

    def test_clean_filter_cannot_execute_through_raw_plumbing(self):
        self.exercise('clean')

    def test_process_filter_cannot_execute_through_raw_plumbing(self):
        self.exercise('process')


if __name__ == '__main__':
    unittest.main()
