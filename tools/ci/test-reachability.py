"""Fail when a Rust source file under a crate's tests/ directory is not compiled by any test target.

Cargo builds each test target that `cargo metadata` lists (tests/*.rs, tests/<name>/main.rs and
[[test]] entries). Any other .rs file under tests/ is compiled only when a target reaches it
through `mod` declarations, with or without #[path] and inside inline `mod name { ... }` blocks.
A file that nothing reaches is silently never compiled, so its tests never run.

Conditional compilation: a declaration under a cfg that is false on every platform (`any()`,
`false`, and `all`/`any`/`not` combinations of those) does not count. Any other cfg, such as
`windows` or `feature = "x"`, counts as reachable.

Path overrides: `#[cfg_attr(pred, path = "...")]` sets the path when `pred` is true. `test` and
`any(...)` containing it are true, since tests/ files compile only in test targets; a false predicate
(`not(test)`) sets nothing. A predicate it cannot evaluate (`windows`, `feature = "x"`) keeps both the
path and the default file open: the check follows each one that exists, and fails only when none does.

Macros: `macro_rules!` bodies are skipped, since they compile only when expanded. When a macro
that declares modules is invoked, or an invocation's tokens declare modules, the check fails and
says the declaration is unsupported, instead of guessing what the expansion compiles.

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
# Keywords that can precede `!(`, `![` or `!{` without being a macro invocation (`if !(x)`).
KEYWORDS = {"if", "while", "match", "return", "in", "let", "else", "break", "yield", "move", "mut", "ref"}


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


def predicate(tokens, i, true_names=()):
    """Evaluate the cfg predicate at tokens[i]; return (True, False or None if unknown, next index).

    Bare names in `true_names` evaluate to True; every other name except `true` and `false` is unknown.
    """
    name = tokens[i] if i < len(tokens) else ")"
    if name in ("any", "all", "not") and i + 1 < len(tokens) and tokens[i + 1] == "(":
        values, j = [], i + 2
        while j < len(tokens) and tokens[j] != ")":
            value, j = predicate(tokens, j, true_names)
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
    elif name in true_names:
        return True, j
    return {"true": True, "false": False}.get(name), j


def never_compiled(attribute):
    """True when the attribute's tokens are a cfg that is false on every platform."""
    return len(attribute) > 2 and attribute[0] == "cfg" and attribute[1] == "(" and predicate(attribute, 2)[0] is False


def path_overrides(attribute, strings, definite):
    """Yield (path, certain) for each `path = "..."` the attribute sets, in source order.

    `certain` is False when the path applies only if an unevaluable cfg_attr predicate holds. A
    cfg_attr whose predicate is false yields nothing. `test` counts as true: the files under tests/
    are compiled only as part of test targets.
    """
    if len(attribute) == 3 and attribute[0] == "path" and attribute[1] == "=":
        yield strings[int(attribute[2].strip('"'))], definite
    elif len(attribute) > 3 and attribute[0] == "cfg_attr" and attribute[1] == "(":
        value, j = predicate(attribute, 2, ("test",))
        if value is False:
            return
        group, depth = [], 0
        for token in attribute[j + 1:-1]:  # skip the comma after the predicate and the final ")"
            if token == ",":
                if depth == 0:
                    yield from path_overrides(group, strings, definite and value is True)
                    group = []
                    continue
            depth += (token in "([{") - (token in ")]}")
            group.append(token)
        yield from path_overrides(group, strings, definite and value is True)


def path_candidates(attributes, strings):
    """Return ([(path, optional)], default possible): the #[path] values that may apply and whether the default may.

    rustc uses the first #[path] that is in effect. A path is optional when a cfg_attr predicate this
    checker cannot evaluate guards it; the default is possible unless an unconditional path is in effect. A predicate this checker cannot evaluate (for
    example `windows`) keeps both outcomes open, so the checker follows every candidate that exists
    instead of rejecting code that may be valid; it can then miss an orphan hidden behind such a cfg.
    """
    paths = []
    for attribute in attributes:
        for path, certain in path_overrides(attribute, strings, True):
            paths.append((path, not certain))
            if certain:
                return paths, False
    return paths, True


class Unsupported(Exception):
    """A module declaration the checker cannot resolve without expanding macros."""


def declares_module(tokens):
    """True when a token tree contains an out-of-line `mod name;` (or `mod $name;` in a macro)."""
    for k, token in enumerate(tokens):
        rest = tokens[k + 1:k + 4]
        if token == "mod" and len(rest) >= 2 and IDENT.fullmatch(rest[0]) and rest[1] == ";":
            return True
        if token == "mod" and len(rest) == 3 and rest[0] == "$" and rest[2] == ";":
            return True
    return False


def mod_declarations(source):
    """Yield (enclosing inline modules, #[path] candidates, default file possible, name) per live `mod name;`.

    Each enclosing inline module is a (name, (#[path] candidates, default possible)) pair. Macro bodies are not
    compiled unless expanded, so `macro_rules!` bodies are skipped; a macro that declares modules
    and is invoked, or an invocation whose tokens declare modules, raises Unsupported.
    """
    text, strings = lex(source)
    tokens = TOKEN.findall(text)
    frames = [(None, ([], True), True)]  # (inline module name or None for other blocks, (#[path] candidates, default), compiled)
    declaring_macros, invoked = set(), set()
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
                    frames[-1] = (*frames[-1][:2], False)
            else:
                attributes.append(attribute)
            i = end + 1
            continue
        if token == "pub":
            i += 1
            if i < len(tokens) and tokens[i] == "(":
                i = closing(tokens, i) + 1
            continue
        if (token == "macro_rules" and i + 3 < len(tokens) and tokens[i + 1] == "!"
                and tokens[i + 3] in ("(", "[", "{")):
            end = closing(tokens, i + 3)
            if declares_module(tokens[i + 4:end]):
                declaring_macros.add(tokens[i + 2])
            attributes, i = [], end + 1
            continue
        if (IDENT.fullmatch(token) and token not in KEYWORDS and i + 2 < len(tokens)
                and tokens[i + 1] == "!" and tokens[i + 2] in ("(", "[", "{")):
            end = closing(tokens, i + 2)
            if declares_module(tokens[i + 3:end]):
                raise Unsupported(f"`{token}!` invocation declares modules; declare them without a macro")
            invoked.add(token)
            attributes, i = [], end + 1
            continue
        if token == "mod" and i + 2 < len(tokens) and IDENT.fullmatch(tokens[i + 1]) and tokens[i + 2] in (";", "{"):
            name = tokens[i + 1].removeprefix("r#")
            compiled = not any(never_compiled(a) for a in attributes)
            paths, default = path_candidates(attributes, strings)
            if tokens[i + 2] == ";":
                if compiled and all(live for _, _, live in frames):
                    yield [(n, p) for n, p, _ in frames if n is not None], paths, default, name
            else:
                frames.append((name, (paths, default), compiled))
            attributes, i = [], i + 3
            continue
        if token == "{":
            frames.append((None, ([], True), True))
        elif token == "}" and len(frames) > 1:
            frames.pop()
        attributes = []
        i += 1
    if used := sorted(declaring_macros & invoked):
        raise Unsupported(f"macro {', '.join(used)} declares modules and is invoked; declare them without a macro")


def child_files(file, owns_directory, source):
    """Resolve module declarations in `file` the way rustc does.

    A file "owns its directory" when it is a target root, a mod.rs, or was loaded through #[path];
    its plain `mod x;` children then live next to it. Otherwise (`dir/foo.rs`) they live in
    `dir/foo/`. Each enclosing inline module adds its name as a directory, or replaces it with its
    own #[path]. A #[path] outside any inline module is relative to the declaring file's directory;
    inside one it is relative to the inline module's directory. Returns (child path, child owns its
    directory) pairs.
    """
    children = []
    for inline, paths, default, name in mod_declarations(source):
        base = file.parent if owns_directory else file.parent / file.stem
        directories = [(None, False)]  # (directory, optional); None: no enclosing inline module
        for module, (module_paths, module_default) in inline:
            grown = []
            for directory, optional in directories:
                grown.extend(((file.parent if directory is None else directory) / p, optional or opt)
                             for p, opt in module_paths)
                if module_default:
                    grown.append(((base if directory is None else directory) / module, optional))
            directories = grown
        found = False
        for directory, directory_optional in directories:
            for path, optional in paths:
                target = ((file.parent if directory is None else directory) / path).resolve()
                if (optional or directory_optional) and not target.is_file():
                    continue  # an unevaluable cfg_attr path may not apply
                children.append((target, True))
                found = True
            if not default:
                continue
            where = base if directory is None else directory
            flat, nested = where / f"{name}.rs", where / name / "mod.rs"
            if flat.is_file():
                children.append((flat.resolve(), False))
                found = True
            elif nested.is_file():
                children.append((nested.resolve(), True))
                found = True
        if not found:
            where = base if directories[0][0] is None else directories[0][0]
            raise FileNotFoundError(
                f"{file}: `mod {name};` matches neither {where / f'{name}.rs'} nor {where / name / 'mod.rs'}")
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
        try:
            found.extend(orphans(crate))
        except Unsupported as error:
            print(f"cannot check {crate.relative_to(repo)}/tests: {error}", file=sys.stderr)
            return 1
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
