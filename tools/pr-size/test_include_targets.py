"""Every file Rust code compiles in with include_str!/include_bytes! is reviewed.

Review skips and the PR size check does not count files marked
`linguist-generated` (AGENTS.md, check.py). A file the product compiles in
must not hide that way unless a generator really produces it, so this test
resolves every include_str!/include_bytes! under crates/ and tools/ and fails
when one names a generated file not listed in GENERATED_INCLUDES with its
generator.

Run: python3 -I -m unittest discover -s tools/pr-size
"""
import os
import re
import subprocess
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SCANNED = ('crates', 'tools')

# Generated files that Rust code may include, each with the generator that
# writes it. Edit the generator's inputs, not the file.
GENERATED_INCLUDES = {
    'docs/architecture/contracts/domain-fixtures.json':
        'tools/contracts-eval/make_domain_fixtures.py',
}

# Sites that write include_str! into generated Rust (a build script's format
# string), so the scanner cannot read the path at the call. Each names the
# repository paths it includes; a path ending in / means every file under it.
INDIRECT_INCLUDES = {
    'crates/devworks-core/build.rs': ['docs/help/catalog.json', 'docs/help/topics/'],
}

MACRO = re.compile(r'\binclude_(?:str|bytes)!\s*\(')
LITERAL = re.compile(r'"((?:[^"\\]|\\.)*)"')
ENV = re.compile(r'\benv!\s*\(\s*"[^"]*"\s*\)')


def git(*args, stdin=None):
    return subprocess.run(['git', *args], cwd=ROOT, input=stdin, check=True,
                          capture_output=True, text=True).stdout


def arguments(text, start):
    """Return the text between the parenthesis at start and its match."""
    depth, i = 0, start
    while i < len(text):
        c = text[i]
        if c == '"':
            i += 1
            while text[i] != '"':
                i += 2 if text[i] == '\\' else 1
        elif c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
            if depth == 0:
                return text[start + 1:i]
        i += 1
    raise ValueError('unbalanced include macro')


def crate_dir(source):
    d = os.path.dirname(source)
    while not os.path.exists(os.path.join(ROOT, d, 'Cargo.toml')):
        if not d:
            raise ValueError(f'{source} is in no crate')
        d = os.path.dirname(d)
    return d


def tracked_under(prefix):
    return [p for p in git('ls-files', '--', prefix).splitlines()
            if p.startswith(prefix)]


def include_targets():
    """Map each included repository file to the sources that include it.

    Returns (targets, unresolved): unresolved lists sites whose path is not
    written as string literals at the call.
    """
    targets, unresolved = {}, []
    sources = [p for p in git('ls-files', '--', *SCANNED).splitlines()
               if p.endswith('.rs')]
    for source in sources:
        with open(os.path.join(ROOT, source), encoding='utf-8') as f:
            text = f.read()
        for match in MACRO.finditer(text):
            args = arguments(text, match.end() - 1)
            if 'OUT_DIR' in args:
                continue  # build output, not a repository file
            parts = LITERAL.findall(ENV.sub('', args))
            if not parts:
                unresolved.append(source)
                continue
            path = ''.join(parts)
            if 'CARGO_MANIFEST_DIR' in args:
                path = os.path.join(crate_dir(source), path.lstrip('/'))
            else:
                path = os.path.join(os.path.dirname(source), path)
            path = os.path.normpath(path).replace(os.sep, '/')
            if '$' in args:
                # A macro appends its argument: every file under the prefix.
                files = tracked_under(path.rstrip('/') + '/')
            else:
                files = [path]
            if not files:
                raise AssertionError(f'{source}: {path!r} names no tracked file')
            for file in files:
                targets.setdefault(file, set()).add(source)
    for source, paths in INDIRECT_INCLUDES.items():
        for path in paths:
            for file in tracked_under(path) if path.endswith('/') else [path]:
                targets.setdefault(file, set()).add(source)
    return targets, unresolved


def generated(paths):
    out = git('check-attr', '--stdin', '-z', 'linguist-generated',
              stdin='\0'.join(paths) + '\0')
    fields = out.split('\0')
    return {fields[i] for i in range(0, len(fields) - 2, 3)
            if fields[i + 2] in ('set', 'true')}


class IncludeTargetsTest(unittest.TestCase):
    def setUp(self):
        self.targets, self.unresolved = include_targets()

    def test_every_included_file_exists(self):
        missing = sorted(p for p in self.targets
                         if not os.path.isfile(os.path.join(ROOT, p)))
        self.assertEqual(missing, [], 'include targets the scanner resolved '
                         'to no file; fix the scanner or the include')

    def test_unresolved_includes_are_listed(self):
        self.assertEqual(sorted(set(self.unresolved) - set(INDIRECT_INCLUDES)), [],
                         'add each site to INDIRECT_INCLUDES with the paths it includes')

    def test_no_included_file_is_generated_unless_listed_with_its_generator(self):
        hidden = {p: sorted(self.targets[p])
                  for p in generated(sorted(self.targets)) - set(GENERATED_INCLUDES)}
        self.assertEqual(hidden, {}, 'compiled-in files marked linguist-generated '
                         'escape review; move them out of the generated pattern or '
                         'list them in GENERATED_INCLUDES with their generator')

    def test_listed_generated_includes_are_current(self):
        stale = sorted(p for p in GENERATED_INCLUDES
                       if p not in self.targets or p not in generated([p]))
        self.assertEqual(stale, [], 'GENERATED_INCLUDES lists files no longer '
                         'included or no longer generated')
        for generator in GENERATED_INCLUDES.values():
            self.assertTrue(os.path.isfile(os.path.join(ROOT, generator)), generator)

    def test_purge_policy_files_are_included_and_hand_written(self):
        policy = [p for p in self.targets if '/project_purge/' in p]
        self.assertIn('crates/devworks-adapters/src/storage/sqlite/project_purge/'
                      'policy/table-selectors.json', policy)
        self.assertEqual(generated(policy), set())


if __name__ == '__main__':
    unittest.main()
