"""A test source that no test target compiles must fail CI; every rustc module form must pass."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('test_reachability', Path(__file__).with_name('test-reachability.py'))
reach = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reach)


def crate(root, files, manifest='[package]\nname = "fixture"\nedition = "2024"\n'):
    root.mkdir(parents=True, exist_ok=True)
    (root / 'Cargo.toml').write_text(manifest, encoding='utf-8')
    (root / 'src').mkdir(exist_ok=True)
    (root / 'src' / 'lib.rs').write_text('', encoding='utf-8')
    for name, body in files.items():
        path = root / 'tests' / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding='utf-8')
    return root


def orphan_names(root):
    return [p.relative_to((root / 'tests').resolve()).as_posix() for p in reach.orphans(root)]


class Reachability(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name).resolve()

    def tearDown(self):
        self.dir.cleanup()

    def test_deliberate_orphan_is_reported(self):
        root = crate(self.root, {
            'providers.rs': 'mod common;\n#[path = "providers/included.rs"]\nmod included;\n',
            'common/mod.rs': '',
            'providers/included.rs': '',
            'providers/orphan.rs': '#[test]\nfn never_compiled() {}\n',
        })
        self.assertEqual(orphan_names(root), ['providers/orphan.rs'])

    def test_rustc_module_resolution_forms_are_reachable(self):
        root = crate(self.root, {
            # Crate root: plain children live beside it.
            'runs.rs': 'mod common;\n#[cfg(test)]\n#[path = "runs/usage.rs"]\nmod usage;\n',
            'common/mod.rs': 'pub(crate) mod flat;\n',  # mod.rs owns its directory
            'common/flat.rs': 'mod child;\n',           # non-mod-rs file: children in flat/
            'common/flat/child.rs': '',
            # A #[path]-loaded file owns its directory; nested #[path] is relative to it.
            'runs/usage.rs': '#[path = "../shared/fences.rs"]\nmod fences;\nmod sibling;\n',
            'runs/sibling.rs': '',
            'shared/fences.rs': '',
        })
        self.assertEqual(orphan_names(root), [])

    def test_declarations_in_comments_and_strings_do_not_count(self):
        root = crate(self.root, {
            'root.rs': 'mod inner;\n',
            'inner/mod.rs': ('// mod a;\n/* mod b; /* nested */ mod c; */\n'
                             'const S: &str = "mod d;";\nconst R: &str = r#"mod e;"#;\n'
                             "const Q: char = '\"';\nfn f<'a>(_: &'a str) {}\nmod real;\n"),
            **{f'inner/{name}.rs': '' for name in ['real', 'a', 'b', 'c', 'd', 'e']},
        })
        self.assertEqual(orphan_names(root), ['inner/a.rs', 'inner/b.rs', 'inner/c.rs', 'inner/d.rs', 'inner/e.rs'])

    def test_manifest_test_targets_are_roots(self):
        root = crate(self.root, {'custom/main.rs': '', 'dropped.rs': ''},
                     manifest='[package]\nname = "fixture"\nedition = "2024"\nautotests = false\n'
                              '[[test]]\nname = "custom"\npath = "tests/custom/main.rs"\n')
        self.assertEqual(orphan_names(root), ['dropped.rs'])

    def test_directory_targets_and_name_only_manifest_targets_are_roots(self):
        # Cargo discovers tests/suite/main.rs as the `suite` target; its plain children sit beside it.
        root = crate(self.root, {'suite/main.rs': 'mod helper;\n', 'suite/helper.rs': ''})
        self.assertEqual(orphan_names(root), [])
        other = crate(self.root / 'named', {'custom/main.rs': '', 'custom/stray.rs': ''},
                      manifest='[package]\nname = "named"\nedition = "2024"\nautotests = false\n'
                               '[[test]]\nname = "custom"\n')
        self.assertEqual(orphan_names(other), ['custom/stray.rs'])

    def test_inline_modules_add_their_directories(self):
        root = crate(self.root, {
            'root.rs': 'mod group { mod child; #[path = "p.rs"] mod p; }\n#[path = "runs/usage.rs"]\nmod usage;\n',
            'group/child.rs': '', 'group/p.rs': '',
            'runs/usage.rs': 'mod blk { #[path = "q.rs"] mod q; mod r; }\n',
            'runs/blk/q.rs': '', 'runs/blk/r.rs': '',
            'nested.rs': 'mod cases;\n',
            'cases/mod.rs': 'mod outer;\n',
            'cases/outer.rs': 'mod inner { #[path = "s.rs"] mod s; }\n',  # not mod-rs
            'cases/outer/inner/s.rs': '',
        })
        self.assertEqual(orphan_names(root), [])

    def test_permanently_disabled_cfg_is_not_reachable_but_platform_cfg_is(self):
        root = crate(self.root, {
            'root.rs': 'mod c;\n',
            'c/mod.rs': ('#[cfg(any())]\n#[path = "disabled/case.rs"]\nmod disabled;\n'
                         '#[cfg(all(any()))] mod d2;\n#[cfg(not(all()))] mod d3;\n#[cfg(false)] mod d4;\n'
                         '#[cfg(all(windows, any()))] mod d5;\n#[cfg(any())] mod blk { mod d6; }\n'
                         '#[cfg(windows)] mod win;\n#[cfg(any(unix, any()))] mod unix;\n'
                         '#[cfg(not(any()))] mod always;\n#[cfg(feature = "x")] pub(crate) mod feature;\n'),
            'c/disabled/case.rs': 'compile_error!("never built");\n',
            **{f'c/{name}.rs': '' for name in ['d2', 'd3', 'd4', 'd5', 'win', 'unix', 'always', 'feature']},
            'c/blk/d6.rs': '',
        })
        self.assertEqual(orphan_names(root), ['c/blk/d6.rs', 'c/d2.rs', 'c/d3.rs', 'c/d4.rs', 'c/d5.rs',
                                              'c/disabled/case.rs'])


if __name__ == '__main__':
    unittest.main()
