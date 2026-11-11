#!/usr/bin/env python3
"""Check Help coverage against owning sources; --refresh-settings updates the catalog reference.

Offline and inference-free. It checks real screen/section links, registered operation spelling,
all catalog setting keys/defaults/scopes/adoption, bounded text, and local cross-topic links.
It does not certify a runtime merely because a schema or document exists.
"""
from pathlib import Path
import argparse
import json
import re
import sys

ROOT = Path(__file__).resolve().parents[2]
HELP = ROOT / 'docs/help'


def macro_arguments(source, start):
    depth = 1
    quoted = False
    escaped = False
    args = []
    part = []
    for char in source[start:]:
        if quoted:
            part.append(char)
            if escaped:
                escaped = False
            elif char == '\\':
                escaped = True
            elif char == '"':
                quoted = False
            continue
        if char == '"':
            quoted = True
        if char in '([{':
            depth += 1
        elif char in ')]}':
            depth -= 1
            if depth == 0:
                args.append(''.join(part).strip())
                return args
        if char == ',' and depth == 1:
            args.append(''.join(part).strip())
            part = []
        else:
            part.append(char)
    raise ValueError('unclosed setting macro')


def setting_rows():
    source = (ROOT / 'crates/devworks-core/src/settings/catalog.rs').read_text()
    constants = dict(re.findall(r'pub const (\w+): &str = "([^"]+)";', source))
    rows = []
    for found in re.finditer(r'setting!\(', source):
        args = macro_arguments(source, found.end())
        if args[0].startswith('$'):
            continue
        if len(args) not in (8, 9):
            raise ValueError(f'unknown setting macro shape: {args[0]}')
        key = json.loads(args[0]) if args[0].startswith('"') else constants[args[0]]
        label, description = json.loads(args[1]), json.loads(args[2])
        scope = {'WORKSPACE': 'Workspace only', 'INHERITED': 'Workspace → Project'}[args[4]]
        default = re.sub(r'\s+', ' ', args[6])
        rows.append((key, label, description, args[3], scope, default, args[7], args[8] if len(args) == 9 else 'Configurable'))
    for variant, default in re.findall(r'role\(AgentRole::(\w+), \|\| \{\s*json!\(\s*("(?:[^"\\]|\\.)*")\s*\)\s*\}\)', source):
        mapping = dict(re.findall(r'Self::(\w+) => "(roles\.[^"]+)"', source))
        rows.append((mapping[variant], variant, 'Editable role instructions; policies and selected tools still apply.', 'Instructions', 'Workspace → Project', default, 'NextSafeStep', 'Configurable'))
    if not rows or len({row[0] for row in rows}) != len(rows):
        raise ValueError('missing or duplicate catalog settings')
    return rows


def settings_reference():
    lines = ['# Complete settings reference', '',
             'This local reference is maintained against the app’s actual settings catalog. Workspace-only keys cannot be overridden by a project. Inherited keys resolve Workspace → Project; clearing a project override restores inheritance. Defaults are defaults, not evidence that a runtime is installed.', '',
             'Adoption: Immediate applies on save; NextSafeStep waits for the next safe context boundary; NextAdmission affects later admitted work; NewClonesOnly affects new repository clones. HarnessEnforced values are visible policies, not editable relaxations. Agent proposals and current authority still apply.', '',
             'Use [Settings](help:settings) for safe edits and [Recovery](help:recovery) for conflicts or uncertain saves. Values rendered as Rust expressions retain their exact catalog default; byte limits use bytes.', '']
    for key, label, description, category, scope, default, adoption, policy in setting_rows():
        lines += [f'## {key}', '', label + '.', '',
                  f'- Default: `{default}`', f'- Scope: {scope}', f'- Adoption: {adoption}',
                  f'- Policy: {policy}', f'- Section: {category}', '', description, '']
    return '\n'.join(lines)



def example_source():
    lines = [
        '# Complete native echo example source', '',
        'This is the full source of the tested local development example. It is bundled with the app so the source remains readable offline. Building requires a matching DevWorks source checkout, Rust toolchain, and the path dependencies below; Cargo may need cached dependencies or a network during setup. It does not install itself or bypass human package approval.', '',
        'See [Build and test](help:sdk-start) for commands and [Process protocol](help:sdk-protocol) for framing and lifecycle. Create the files at the shown paths under `docs/help/examples/echo` in a matching source checkout.', ''
    ]
    for relative, language in [
        ('Cargo.toml', 'toml'),
        ('src/main.rs', 'rust'),
        ('src/bin/build_bundle.rs', 'rust'),
        ('contributions.json', 'json'),
        ('event-commit.json', 'json'),
    ]:
        lines.extend(['## ' + relative, '', '```' + language,
                      (HELP / 'examples/echo' / relative).read_text().rstrip(), '```', ''])
    lines.extend([
        'The contribution and event JSON files are typed contract examples, not a complete installable package. Their illustrative digests, tokens, IDs, endpoint and resource declarations must come from the actual host/package publication when used. The echo bundle builder above creates its own real file and schema digests.', ''
    ])
    return '\n'.join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--refresh-settings', action='store_true')
    parser.add_argument('--refresh-example', action='store_true')
    args = parser.parse_args()
    reference = HELP / 'topics/settings-reference.md'
    expected = settings_reference()
    if args.refresh_settings:
        reference.write_text(expected)
    example = HELP / 'topics/sdk-echo-source.md'
    expected_example = example_source()
    if args.refresh_example:
        example.write_text(expected_example)
    errors = []
    if not example.exists() or example.read_text() != expected_example:
        errors.append('bundled example source drift: run python3 docs/help/check.py --refresh-example')
    if not reference.exists() or reference.read_text() != expected:
        errors.append('settings reference drift: run python3 docs/help/check.py --refresh-settings')
    catalog = json.loads((HELP / 'catalog.json').read_text())
    ids = [topic['id'] for topic in catalog]
    if len(ids) != len(set(ids)):
        errors.append('duplicate topic ID')
    for topic in catalog:
        if not re.fullmatch(r'[A-Za-z0-9_-]{1,128}', topic['id']):
            errors.append('invalid topic ID: ' + topic['id'])
        path = HELP / topic['file']
        if not path.is_relative_to(HELP / 'topics') or not path.is_file():
            errors.append('missing/unsafe topic path: ' + str(path))
            continue
        text = path.read_text()
        if len(text) > 65536 or len(text.encode()) > 262144 or not text.startswith('# ' + topic['title'] + '\n'):
            errors.append('topic title or bound mismatch: ' + topic['id'])
        for target in re.findall(r'\]\(help:([^)]+)\)', text):
            if target not in ids:
                errors.append(topic['id'] + ': broken local topic link: ' + target)
        if re.search(r'\bDW-\d+\b', text):
            errors.append(topic['id'] + ': product text exposes a tracker item instead of a capability limitation')
    routes = (ROOT / 'crates/devworks-desktop/src/route.rs').read_text()
    screens = re.findall(r'\bid: "([^"]+)"[\s\S]*?\bhelp_topic: "([^"]+)"', routes.split('pub static SCREENS:', 1)[1].split('/// The navigation order', 1)[0])
    for screen, topic_id in screens:
        matches = [topic for topic in catalog if screen in topic['screenIds']]
        if len(matches) != 1 or matches[0]['id'] != topic_id:
            errors.append('screen Help mapping missing/drifted: ' + screen)
    settings = (ROOT / 'crates/devworks-core/src/settings/catalog.rs').read_text().split('pub fn help_id', 1)[1].split('\n}', 1)[0]
    section_ids = re.findall(r'=> "(settings-[^"]+)"', settings)
    for section in section_ids:
        matches = [topic for topic in catalog if section in topic['settingsSections']]
        if len(matches) != 1:
            errors.append('Settings Help mapping missing/duplicated: ' + section)
    registry = (ROOT / 'crates/devworks-core/src/registry/catalog.rs').read_text()
    operations = set(re.findall(r'name: "([a-z_]+\.[a-z_]+)"', registry))
    for topic in catalog:
        for operation in topic['relatedCapabilityIds']:
            if operation not in operations:
                errors.append(topic['id'] + ': unknown operation spelling: ' + operation)
    coverage = json.loads((HELP / 'coverage.json').read_text())
    for owner in coverage['workflowOwners'] + coverage['extensionOwners']:
        if not (ROOT / owner['path']).exists():
            errors.append('missing documented source owner: ' + owner['path'])
        for topic_id in owner['topics']:
            if topic_id not in ids:
                errors.append('missing source-coverage topic: ' + topic_id)
    services = (ROOT / 'crates/devworks-core/src/services/mod.rs').read_text()
    owners = {
        'evals':'evaluations', 'approvals':'approvals', 'queue':'workflow', 'settings':'settings',
        'git':'git-delivery', 'artifacts':'artifacts', 'sessions':'chat', 'history':'overview',
        'resources':'resources', 'runs':'work-controls', 'router':'routing-context', 'jobs':'jobs',
        'connections':'settings-connections', 'knowledge':'knowledge', 'diagnostics':'diagnostics',
        'browser':'browser', 'packages':'addons', 'web':'integrations', 'projects':'projects',
        'repositories':'repositories', 'indexing':'indexing', 'work':'workflow',
        'notifications':'approvals', 'help':'help', 'usage':'usage',
        'prices':'usage', 'graph':'code-exploration', 'instructions':'settings-Instructions'
    }
    for owner in re.findall(r'pub fn with_(\w+)\(', services):
        if owner not in owners or owners[owner] not in ids:
            errors.append('new native service needs authored workflow Help coverage: ' + owner)
    if errors:
        print('\n'.join(errors), file=sys.stderr)
        return 1
    print(f'{len(catalog)} topics; {len(screens)} screens; {len(section_ids)} Settings categories; {len(setting_rows())} settings; local links and operation spellings checked')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
