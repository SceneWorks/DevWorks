"""A test source that no test target compiles must fail CI; every rustc module form must pass."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('test_reachability', Path(__file__).with_name('test-reachability.py'))
reach = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reach)


def crate(root, files, manifest='[package]\nname = "fixture"\n'):
    (root / 'Cargo.toml').write_text(manifest, encoding='utf-8')
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
                     manifest='[package]\nname = "fixture"\nautotests = false\n'
                              '[[test]]\nname = "custom"\npath = "tests/custom/main.rs"\n')
        self.assertEqual(orphan_names(root), ['dropped.rs'])


if __name__ == '__main__':
    unittest.main()
