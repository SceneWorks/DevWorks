"""Coverage regressions: omitted, duplicated or silently unclassified targets fail CI."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('desktop_shard', Path(__file__).with_name('desktop-shard.py'))
shard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shard)


def target(kind, name, test=True, doctest=False):
    return {'kind': [kind], 'name': name, 'test': test, 'doctest': doctest}


class Coverage(unittest.TestCase):
    def test_full_union_and_disjointness_include_units_integrations_docs_and_example_builds(self):
        targets = [target('lib', 'desktop', doctest=True), target('bin', 'desktop'),
                   target('test', 'help'), target('test', 'work'),
                   target('example', 'compile_only', test=False)]
        plans = shard.verify(targets, 2)
        self.assertEqual(set(sum((p['coverage'] for p in plans), [])),
                         {'lib:desktop', 'bin:desktop', 'test:help', 'test:work',
                          'doc:desktop', 'example-build:compile_only'})
        docs = [c for p in plans for c in p['commands'] if '--doc' in c]
        self.assertEqual(docs, [shard.BASE + ['--doc']])
        examples = [c for p in plans for c in p['commands'] if 'build' in c]
        self.assertEqual(examples, [['cargo', 'build', '-p', 'devworks-desktop', '--locked',
                                     '--profile', 'test', '--example', 'compile_only']])

    def test_new_target_is_automatically_included_once(self):
        targets = [target('test', 'a'), target('test', 'b'), target('test', 'new_feature')]
        plans = shard.verify(targets, 2)
        self.assertEqual(sum('test:new_feature' in p['coverage'] for p in plans), 1)

    def test_unknown_target_fails_before_execution(self):
        with self.assertRaises(ValueError):
            shard.verify([target('bench', 'unexpected')], 2)

    def test_invalid_or_empty_shards_fail(self):
        for index, count in [(-1, 2), (2, 2), (0, 0), (1, 2)]:
            with self.assertRaises(ValueError):
                shard.plan([target('test', 'one')], index, count)


if __name__ == '__main__':
    unittest.main()
