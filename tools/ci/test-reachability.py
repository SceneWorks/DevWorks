"""Fail when a Rust source file under a crate's tests/ directory is not compiled by any test target.

Cargo builds each test target that `cargo metadata` lists (tests/*.rs, tests/<name>/main.rs and
[[test]] entries). Any other .rs file under tests/ is compiled only when a target reaches it
through `mod` declarations, with or without #[path] and inside inline `mod name { ... }` blocks.
A file that nothing reaches is silently never compiled, so its tests never run.

Conditional compilation: a declaration under a cfg that is false on every platform (`any()`,
`false`, and `all`/`any`/`not` combinations of those) does not count. Any other cfg, such as
`windows` or `feature = "x"`, counts as reachable.

Usage: python3 -I tools/ci/test-reachability.py [REPO_ROOT]
"""

import json
import re
import subprocess
import sys
from pathlib import Path

RAW_STRING = re.compile(r'b?r(#*)"')
CHAR_LITERAL = re.compile(r"b?'(?:\\u\{[0-9a-fA-F]+\}|\\x[0-9a-fA-F]{2}|\\.|[^\\'\n])'")
ESCAPE = re.compile(r'\\(.)', re.S)
# Tokens of lexed text: string placeholders, identifiers (raw or not), single punctuation.
TOKEN = re.compile(r'"\d+"|r#\w+|\w+|[^\s\w]')
IDENT = re.compile(r"(?:r#)?[A-Za-z_]\w*")


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


def closing(tokens, i):
    """Return the index of the bracket that closes the one at tokens[i]."""
    pair = {"(": ")", "[": "]", "{": "}"}[tokens[i]]
    depth = 0
    for j in range(i, len(tokens)):
        if tokens[j] == tokens[i]:
            depth += 1
        elif tokens[j] == pair:
            depth -= 1
            if depth == 0:
                return j
    return len(tokens) - 1


def predicate(tokens, i):
    """Evaluate the cfg predicate at tokens[i]; return (True, False or None if unknown, next index)."""
    name = tokens[i] if i < len(tokens) else ")"
    if name in ("any", "all", "not") and i + 1 < len(tokens) and tokens[i + 1] == "(":
        values, j = [], i + 2
        while j < len(tokens) and tokens[j] != ")":
            value, j = predicate(tokens, j)
            values.append(value)
            if j < len(tokens) and tokens[j] == ",":
                j += 1
        if name == "any":
            result = True if True in values else (False if all(v is False for v in values) else None)
        elif name == "all":
            result = False if False in values else (True if all(v is True for v in values) else None)
        else:
            result = {True: False, False: True}.get(values[0]) if len(values) == 1 else None
        return result, j + 1
    j = i + 1
    if j < len(tokens) and tokens[j] == "=":
        j += 2
    elif j < len(tokens) and tokens[j] == "(":
        j = closing(tokens, j) + 1
    return {"true": True, "false": False}.get(name), j


def never_compiled(attribute):
    """True when the attribute's tokens are a cfg that is false on every platform."""
    return len(attribute) > 2 and attribute[0] == "cfg" and attribute[1] == "(" and predicate(attribute, 2)[0] is False


def path_attribute(attributes, strings):
    for attribute in attributes:
        if len(attribute) == 3 and attribute[0] == "path" and attribute[1] == "=":
            return strings[int(attribute[2].strip('"'))]
    return None


def mod_declarations(source):
    """Yield (enclosing inline module names, #[path] value or None, name) for each live `mod name;`."""
    text, strings = lex(source)
    tokens = TOKEN.findall(text)
    frames = [(None, True)]  # (inline module name or None for other blocks, compiled)
    attributes, i = [], 0
    while i < len(tokens):
        token = tokens[i]
        if token == "#" and i + 1 < len(tokens) and tokens[i + 1] in ("[", "!"):
            inner = tokens[i + 1] == "!"
            start = i + 2 if inner else i + 1
            end = closing(tokens, start)
            attribute = tokens[start + 1:end]
            if inner:
                if never_compiled(attribute):
                    frames[-1] = (frames[-1][0], False)
            else:
                attributes.append(attribute)
            i = end + 1
            continue
        if token == "pub":
            i += 1
            if i < len(tokens) and tokens[i] == "(":
                i = closing(tokens, i) + 1
            continue
        if token == "mod" and i + 2 < len(tokens) and IDENT.fullmatch(tokens[i + 1]) and tokens[i + 2] in (";", "{"):
            name = tokens[i + 1].removeprefix("r#")
            compiled = not any(never_compiled(a) for a in attributes)
            if tokens[i + 2] == ";":
                if compiled and all(live for _, live in frames):
                    inline = [n for n, _ in frames if n is not None]
                    yield inline, path_attribute(attributes, strings), name
            else:
                frames.append((name, compiled))
            attributes, i = [], i + 3
            continue
        if token == "{":
            frames.append((None, True))
        elif token == "}" and len(frames) > 1:
            frames.pop()
        attributes = []
        i += 1


def child_files(file, owns_directory, source):
    """Resolve module declarations in `file` the way rustc does.

    A file "owns its directory" when it is a target root, a mod.rs, or was loaded through #[path];
    its plain `mod x;` children then live next to it. Otherwise (`dir/foo.rs`) they live in
    `dir/foo/`. Enclosing inline modules add one directory each. A #[path] value is relative to
    the declaring file's directory, or, inside inline modules, to that same base plus the inline
    module directories. Returns (child path, child owns its directory) pairs.
    """
    children = []
    for inline, path, name in mod_declarations(source):
        base = file.parent if owns_directory else file.parent / file.stem
        if path is not None:
            parent = base.joinpath(*inline) if inline else file.parent
            children.append(((parent / path).resolve(), True))
            continue
        base = base.joinpath(*inline)
        flat, nested = base / f"{name}.rs", base / name / "mod.rs"
        if flat.is_file():
            children.append((flat.resolve(), False))
        elif nested.is_file():
            children.append((nested.resolve(), True))
        else:
            raise FileNotFoundError(f"{file}: `mod {name};` matches neither {flat} nor {nested}")
    return children


def test_roots(crate):
    """Return the root files of the crate's test targets, as Cargo discovers them."""
    manifest = (crate / "Cargo.toml").resolve()
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1",
         "--manifest-path", str(manifest)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout)
    return {
        Path(target["src_path"]).resolve()
        for package in metadata["packages"]
        if Path(package["manifest_path"]).resolve() == manifest
        for target in package["targets"]
        if "test" in target["kind"]
    }


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
