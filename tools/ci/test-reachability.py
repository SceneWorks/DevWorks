"""Fail when a Rust source file under a crate's tests/ directory is not compiled by any test target.

Cargo builds each top-level tests/*.rs file (and each [[test]] path in Cargo.toml) as a test
target. Any other .rs file under tests/ is compiled only when a target reaches it through `mod`
declarations, with or without #[path]. A file that nothing reaches is silently never compiled,
so its tests never run.

Usage: python3 -I tools/ci/test-reachability.py [REPO_ROOT]
"""

import re
import subprocess
import sys
import tomllib
from pathlib import Path

# Outer attributes (one of which may be #[path = "..."]), optional visibility, then `mod name;`.
# Runs on lexed text, where every string literal is the placeholder `"<index>"`.
MOD_DECL = re.compile(
    r'((?:#\s*\[[^\]]*\]\s*)*)(?:pub(?:\s*\([^)]*\))?\s+)?\bmod\s+(r#)?(\w+)\s*;'
)
PATH_ATTR = re.compile(r'#\s*\[\s*path\s*=\s*"(\d+)"\s*\]')
RAW_STRING = re.compile(r'b?r(#*)"')
CHAR_LITERAL = re.compile(r"b?'(?:\\u\{[0-9a-fA-F]+\}|\\x[0-9a-fA-F]{2}|\\.|[^\\'\n])'")
ESCAPE = re.compile(r'\\(.)', re.S)


def lex(source):
    """Drop comments and replace each string literal with `"<index>"`; return (text, strings).

    Without this, `mod x;` inside a string fixture or a comment would count as a declaration.
    """
    out, strings, i, n = [], [], 0, len(source)
    while i < n:
        ch = source[i]
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = n if end < 0 else end
        elif source.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if source.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif source.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        elif (raw := RAW_STRING.match(source, i)) and (i == 0 or not (source[i - 1].isalnum() or source[i - 1] == "_")):
            close = '"' + raw.group(1)
            end = source.find(close, raw.end())
            end = n if end < 0 else end
            strings.append(source[raw.end():end])
            out.append(f'"{len(strings) - 1}"')
            i = end + len(close)
        elif ch == '"' or (ch == "b" and source.startswith('b"', i) and (i == 0 or not source[i - 1].isalnum())):
            j = i + (2 if ch == "b" else 1)
            start = j
            while j < n and source[j] != '"':
                j += 2 if source[j] == "\\" else 1
            strings.append(ESCAPE.sub(r"\1", source[start:j]))
            out.append(f'"{len(strings) - 1}"')
            i = j + 1
        elif ch == "'" and (char := CHAR_LITERAL.match(source, i)):
            out.append("' '")
            i = char.end()
        else:
            out.append(ch)
            i += 1
    return "".join(out), strings


def mod_declarations(source):
    """Yield (path attribute or None, module name) for each out-of-line module declaration."""
    text, strings = lex(source)
    for match in MOD_DECL.finditer(text):
        path = PATH_ATTR.search(match.group(1))
        yield (strings[int(path.group(1))] if path else None, match.group(3))


def child_files(file, owns_directory, source):
    """Resolve module declarations in `file` the way rustc does for files outside inline modules.

    A file "owns its directory" when it is a crate root, a mod.rs, or was loaded through #[path];
    its plain `mod x;` children then live next to it. Otherwise (`dir/foo.rs`) they live in
    `dir/foo/`. A #[path] value is always relative to the declaring file's directory.
    Returns (child path, child owns its directory) pairs, or raises on an unresolvable module.
    """
    children = []
    for path, name in mod_declarations(source):
        if path is not None:
            children.append(((file.parent / path).resolve(), True))
            continue
        base = file.parent if owns_directory else file.parent / file.stem
        flat, nested = base / f"{name}.rs", base / name / "mod.rs"
        if flat.is_file():
            children.append((flat.resolve(), False))
        elif nested.is_file():
            children.append((nested.resolve(), True))
        else:
            raise FileNotFoundError(f"{file}: `mod {name};` matches neither {flat} nor {nested}")
    return children


def test_roots(crate):
    """Return the test-target root files Cargo builds for the crate at `crate`."""
    manifest = tomllib.loads((crate / "Cargo.toml").read_text(encoding="utf-8"))
    roots = set()
    if manifest.get("package", {}).get("autotests", True):
        roots.update(p.resolve() for p in (crate / "tests").glob("*.rs"))
    for target in manifest.get("test", []):
        if "path" in target:
            roots.add((crate / target["path"]).resolve())
        elif "name" in target:
            roots.add((crate / "tests" / f"{target['name']}.rs").resolve())
    return roots


def orphans(crate):
    """Return the .rs files under `crate`/tests that no test target reaches, sorted."""
    tests = crate / "tests"
    pending = [(root, True) for root in test_roots(crate) if root.is_file()]
    # A file can be both a target root and another module's child, with different directory rules.
    visited = set()
    while pending:
        node = pending.pop()
        if node in visited:
            continue
        visited.add(node)
        file, owns_directory = node
        pending.extend(child_files(file, owns_directory, file.read_text(encoding="utf-8")))
    present = {p.resolve() for p in tests.rglob("*.rs")}
    return sorted(present - {file for file, _ in visited})


def crates(repo):
    """Return every tracked Cargo package directory that has a tests/ directory."""
    listed = subprocess.run(
        ["git", "ls-files", "--", "Cargo.toml", "*/Cargo.toml"],
        cwd=repo,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.split()
    dirs = [(repo / m).parent for m in listed]
    return sorted(d for d in dirs if (d / "tests").is_dir())


def main(argv):
    repo = Path(argv[1] if len(argv) > 1 else ".").resolve()
    found = []
    checked = 0
    for crate in crates(repo):
        checked += 1
        found.extend(orphans(crate))
    for file in found:
        print(f"unreachable test source (no test target compiles it): {file.relative_to(repo)}")
    if found:
        print(
            "Add a `mod` declaration (with #[path] if needed) from a test target, or delete the file.",
            file=sys.stderr,
        )
        return 1
    print(f"Every test source under {checked} crates' tests/ directories is reachable.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
