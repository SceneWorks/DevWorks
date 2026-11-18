"""Generate the test stub for every storage repository port (standard library only).

`crates/devworks-test-support` implements each repository trait of `devworks-core::ports::storage`
for one struct, `Unmodelled`, whose every required method answers "not modelled". The impls are
read from the trait declarations, so a method added to (or changed on) a port needs no hand edit
in any test double: regenerate and the stub follows. A method with a default body in the trait is
not stubbed (the stub inherits the default, as a real adapter would); a required method is never
given a default in production code.

`MODELLED` gives a few methods real behaviour. Edit it to change what the stub answers for a
method that already exists, never to add a method.
"""
import re

PORTS = ('ports/storage.rs', 'ports/storage')

# Traits the in-memory store of the core tests models itself, and traits that are not repository
# ports. Every other trait declared under ports/storage gets a stub.
NOT_STUBBED = {
    'ActorRead', 'ActorRegistry', 'CommandLedger', 'CommandLedgerRead', 'HistoryLog', 'HistoryRead',
    'ApplicationStore', 'ReadSnapshot', 'WriteTransaction', 'WriteJob', 'StoreHarness',
    'StorageMaintenance',
}

# Behaviour the core tests rely on where "not modelled" would fail a path they do not exercise.
GIT_UNSET = 'Err(StorageError::InvalidRecord("Git test store not configured".into()))'
MODELLED = {
    ('OperationRead', 'operation'): 'Ok(None)',
    ('SessionRead', 'work_root'): 'Ok(None)',
    ('SessionRead', 'project_exists'): 'Ok(false)',
    ('SessionRead', 'project_scope_revision'): 'Ok(None)',
    ('GitRead', 'git_worktree_for_branch'): 'Ok(None)',
    ('GitRead', 'git_pending_worktrees'): 'Ok(vec![])',
    ('GitRead', 'git_worktree'): 'Ok(None)',
    ('GitRead', 'git_repository_worktrees'): 'Ok(vec![])',
    ('GitRead', 'git_worktrees'): 'Ok(vec![])',
    ('GitRepository', 'insert_git_worktree'): GIT_UNSET,
    ('GitRepository', 'update_git_worktree'): GIT_UNSET,
}


def blank(text):
    """Same-length copy of Rust source with comments and string/char literal contents spaced out."""
    out = list(text)
    i, n = 0, len(text)

    def wipe(a, b):
        for k in range(a, b):
            if out[k] != '\n':
                out[k] = ' '

    while i < n:
        c = text[i]
        if text.startswith('//', i):
            j = text.find('\n', i)
            j = n if j < 0 else j
            wipe(i, j)
            i = j
        elif text.startswith('/*', i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith('/*', j):
                    depth, j = depth + 1, j + 2
                elif text.startswith('*/', j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            wipe(i, j)
            i = j
        elif c == 'r' and re.match(r'r#*"', text[i:i + 12]) and not (i and (text[i - 1].isalnum() or text[i - 1] == '_')):
            hashes = len(re.match(r'r(#*)"', text[i:]).group(1))
            close = '"' + '#' * hashes
            j = text.find(close, i + 2 + hashes)
            j = n if j < 0 else j + len(close)
            wipe(i, j)
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == '\\' else 1
            wipe(i + 1, j)
            i = j + 1
        elif c == "'":
            m = re.match(r"'(\\.[^']*|[^'\\])'", text[i:])
            if m:
                wipe(i + 1, i + len(m.group(0)) - 1)
                i += len(m.group(0))
            else:
                i += 1
        else:
            i += 1
    return ''.join(out)


def match_close(blanked, start, open_ch, close_ch):
    depth = 0
    for k in range(start, len(blanked)):
        if blanked[k] == open_ch:
            depth += 1
        elif blanked[k] == close_ch:
            depth -= 1
            if depth == 0:
                return k
    raise ValueError(f'unbalanced {open_ch}')


def split_top(text, sep=','):
    """Split on `sep` outside (), [], <> and braces; `->` does not close an angle bracket."""
    parts, depth, cur, i = [], 0, [], 0
    while i < len(text):
        c = text[i]
        if text.startswith('->', i):
            cur.append('->')
            i += 2
            continue
        if c in '([{<':
            depth += 1
        elif c in ')]}>':
            depth -= 1
        if c == sep and depth == 0:
            parts.append(''.join(cur))
            cur = []
        else:
            cur.append(c)
        i += 1
    if ''.join(cur).strip():
        parts.append(''.join(cur))
    return [p.strip() for p in parts]


def required_methods(source, trait):
    """(name, generics, params, return) of every method of `trait` without a default body."""
    blanked = blank(source)
    m = re.search(r'\bpub trait ' + re.escape(trait) + r'\b[^{;]*\{', blanked)
    if not m:
        raise ValueError(f'trait {trait} not found')
    if re.match(r'pub trait\s+\w+\s*<', blanked[m.start():]):
        raise ValueError(f'trait {trait} has generics: the stub cannot name its parameters')
    body_start = m.end() - 1
    body_end = match_close(blanked, body_start, '{', '}')
    body, body_text = blanked[body_start + 1:body_end], source[body_start + 1:body_end]
    methods, i = [], 0
    while i < len(body):
        if re.match(r'fn\s', body[i:]) and (i == 0 or not (body[i - 1].isalnum() or body[i - 1] == '_')):
            name = re.match(r'fn\s+(\w+)', body[i:]).group(1)
            item_start = max(body.rfind(c, 0, i) for c in '{};') + 1
            front = body[item_start:i]
            if re.search(r'\b(async|unsafe)\b', front):
                raise ValueError(f'{trait}::{name} is async or unsafe: the stub cannot restate it')
            if re.search(r'#\s*\[\s*cfg', front):
                raise ValueError(f'{trait}::{name} has a #[cfg] attribute: the stub cannot follow it')
            j, paren = i, 0
            while True:
                if body[j] in '([':
                    paren += 1
                elif body[j] in ')]':
                    paren -= 1
                elif body[j] in ';{' and paren == 0:
                    break
                j += 1
            if body[j] == ';':
                try:
                    methods.append(parse_signature(name, body_text[i:j], body[i:j]))
                except ValueError as error:
                    raise ValueError(f'{trait}::{name}: {error}') from None
                i = j + 1
            else:
                i = match_close(body, j, '{', '}') + 1
            continue
        if body[i] == '{':
            i = match_close(body, i, '{', '}')
        i += 1
    return methods


def parse_signature(name, signature, blank_sig):
    after_name = blank_sig.index(name) + len(name)
    open_at = blank_sig.index('(', after_name)
    generics = signature[after_name:open_at].strip()
    close_at = match_close(blank_sig, open_at, '(', ')')
    params = split_top(signature[open_at + 1:close_at])
    ret = signature[close_at + 1:].strip()
    ret = re.sub(r'\s+where\b.*$', '', ret, flags=re.S).strip()
    ret = ret[2:].strip() if ret.startswith('->') else ''
    return name, generics, params, ret


def pattern_free(param, where=''):
    if re.match(r'(mut\s+)?self\s*:', param):
        raise ValueError(f'{where or "method"}: receiver {param!r} has an explicit type the stub would mangle')
    if re.fullmatch(r"(&\s*('\w+\s+)?(mut\s+)?)?self", param):
        return param
    _, _, ty = param.partition(':')
    if not ty:
        raise ValueError(f'unreadable parameter: {param!r}')
    return '_: ' + ' '.join(ty.split())


def explicit_imports(source):
    """name -> path for every non-glob `use crate::...` item a trait file imports."""
    found = {}
    for stmt in re.findall(r'^use ([^;]*);', blank(source), re.M | re.S):
        stmt = ' '.join(stmt.split())
        m = re.fullmatch(r'(.*)::\{(.*)\}', stmt)
        prefix, names = (m.group(1), split_top(m.group(2))) if m else (stmt.rsplit('::', 1)[0], [stmt.rsplit('::', 1)[1]])
        for n in names:
            if '{' in n or n.endswith('*') or n == 'self':
                continue
            real, _, alias = n.partition(' as ')
            found[(alias or real).strip()] = f'{prefix}::{real.strip()}'
    return found


def generate(core_src):
    """The text of `unmodelled_generated.rs`."""
    files = [core_src / PORTS[0], *sorted((core_src / PORTS[1]).glob('*.rs'))]
    traits, imports = {}, {}
    for f in files:
        source = f.read_text(encoding='utf-8')
        for t in re.findall(r'^pub trait (\w+)', blank(source), re.M):
            if t not in NOT_STUBBED:
                for name, path in explicit_imports(source).items():
                    path = path.replace('crate::', 'devworks_core::') if f.name == 'storage.rs' else path.replace('crate::', 'devworks_core::').replace('super::', 'devworks_core::ports::storage::')
                    if imports.setdefault(name, path) != path:
                        raise ValueError(f'{name} is imported from two paths: {imports[name]} and {path}')
                traits[t] = source if f.name == 'storage.rs' else source.replace('super::', 'devworks_core::ports::storage::')
    out = [
        '// @generated by tools/core-codegen/generate.py from the repository traits in',
        '// crates/devworks-core/src/ports/storage. Do not edit by hand; regenerate and commit the result.',
        '//! The stub every test double borrows for repositories it does not model: each required',
        '//! method answers `unmodelled()`, except those `tools/core-codegen/stubs.py` gives behaviour.',
        '#![allow(clippy::too_many_arguments, clippy::type_complexity)]',
        'use crate::{Unmodelled, unmodelled};',
        'use devworks_core::ports::storage::*;',
        '',
    ]
    used = set()
    for trait in sorted(traits):
        out.append(f'impl {trait} for Unmodelled {{')
        for name, generics, params, ret in required_methods(traits[trait], trait):
            sig = ', '.join(pattern_free(p, f'{trait}::{name}') for p in params)
            body = MODELLED.get((trait, name))
            if body is not None:
                used.add((trait, name))
            elif ret.startswith('Result<'):
                body = 'unmodelled()'
            else:
                raise ValueError(f'{trait}::{name} returns {ret!r}: give it a MODELLED entry')
            out.append(f'    fn {name}{generics}({sig}) -> {ret} {{ {body} }}'.replace('crate::', 'devworks_core::'))
        out.append('}')
    body = '\n'.join(out)
    wanted = sorted(p for n, p in imports.items() if re.search(r'\b' + re.escape(n) + r'\b', body.split('\n\n', 1)[1]))
    body = body.replace('use crate::{Unmodelled, unmodelled};', 'use crate::{Unmodelled, unmodelled};\n' + '\n'.join(f'use {p};' for p in wanted), 1)
    out = body.split('\n')
    unused = set(MODELLED) - used
    if unused:
        raise ValueError(f'MODELLED names no required method: {sorted(unused)}')
    return '\n'.join(out) + '\n'
