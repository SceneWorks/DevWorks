"""Tests for check.py. Run: python3 -I -m unittest discover -s tools/pr-size"""
import os
import subprocess
import tempfile
import unittest

CHECK = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'check.py')
BODY = ''.join(f'line {i} of the hand-written body\n' for i in range(200))


class Repo:
    def __init__(self, root):
        self.root = root
        self.git('init', '-q', '-b', 'main')
        self.git('config', 'user.email', 't@example.com')
        self.git('config', 'user.name', 't')

    def git(self, *args):
        return subprocess.run(['git', *args], cwd=self.root, check=True,
                              capture_output=True, text=True).stdout.strip()

    def write(self, path, text):
        full = os.path.join(self.root, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, 'w', newline='') as f:
            f.write(text)

    def commit(self, msg):
        self.git('add', '-A')
        self.git('commit', '-q', '-m', msg)
        return self.git('rev-parse', 'HEAD')

    def check(self, base, head, limit=100):
        r = subprocess.run(['python3', '-I', CHECK, base, head, '--limit',
                            str(limit)], cwd=self.root, capture_output=True,
                           text=True)
        return r.returncode, r.stdout


class PrSizeTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.repo = Repo(self._tmp.name)
        self.repo.write('.gitattributes', 'gen/** linguist-generated\n')
        self.repo.write('keep.txt', BODY)
        self.base = self.repo.commit('base')

    def test_pr_cannot_exempt_its_own_files(self):
        self.repo.write('.gitattributes',
                        'gen/** linguist-generated\nsrc/** linguist-generated\n')
        self.repo.write('src/big.rs', BODY)
        head = self.repo.commit('self-exempt')
        code, out = self.repo.check(self.base, head)
        self.assertEqual(code, 1, out)
        self.assertIn('hand-written lines changed: 201', out)

    def test_existing_generated_pattern_still_excluded(self):
        self.repo.write('gen/out.json', BODY)
        head = self.repo.commit('generated')
        code, out = self.repo.check(self.base, head)
        self.assertEqual(code, 0, out)
        self.assertIn('hand-written lines changed: 0', out)

    def test_pure_move_counts_zero(self):
        os.makedirs(os.path.join(self.repo.root, 'moved'))
        self.repo.git('mv', 'keep.txt', 'moved/keep.txt')
        head = self.repo.commit('move')
        code, out = self.repo.check(self.base, head)
        self.assertEqual(code, 0, out)
        self.assertIn('hand-written lines changed: 0', out)

    def test_move_with_edit_counts_only_the_edit(self):
        os.makedirs(os.path.join(self.repo.root, 'moved'))
        self.repo.git('mv', 'keep.txt', 'moved/keep.txt')
        self.repo.write('moved/keep.txt',
                        BODY.replace('line 5 of', 'edited 5 of'))
        head = self.repo.commit('move and edit')
        code, out = self.repo.check(self.base, head)
        self.assertEqual(code, 0, out)
        self.assertIn('hand-written lines changed: 2', out)

    def test_plain_addition_counts(self):
        self.repo.write('new.txt', BODY)
        head = self.repo.commit('add')
        code, out = self.repo.check(self.base, head)
        self.assertEqual(code, 1, out)
        self.assertIn('hand-written lines changed: 200', out)


if __name__ == '__main__':
    unittest.main()
