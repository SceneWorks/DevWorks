import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { test } from 'node:test';
import { extractProject } from './extractor';
import { nativeGraph, span } from './native';

test('native facts preserve package ownership, resolved calls, modern modules and empty declarations', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'typescript-native-'));
  const input = new Map(Object.entries({
    'package.json': '{"name":"native-typescript-fixture","version":"1.2.3","type":"module"}',
    'tsconfig.json': '{"compilerOptions":{"module":"NodeNext","moduleResolution":"NodeNext","allowJs":true}}',
    'lib.mts': '// é and CRLF\r\nexport function nextValue() { return 7; }\r\n',
    'app.mts': 'import { nextValue } from "./lib.mjs"; export function run() { return nextValue(); }\n',
    'other.cts': 'export function other() { return 12; }',
    'example.cjs': 'module.exports.answer = 42;',
    'globals.d.ts': 'declare const count: number;',
    'other.d.cts': 'declare const second: number;',
    'empty.d.mts': '',
  }).map(([file, text]) => [file, Buffer.from(text)] as const));
  try {
    for (const [file, bytes] of input) fs.writeFileSync(path.join(root, file), bytes);
    const result = extractProject('demo', root, path.join(root, 'tsconfig.json'), () => {}, [...input.keys()]);
    assert.equal(result.complete, true, result.diagnostics?.join('\n'));
    const graph = nativeGraph('demo', input, result);
    const units = new Map(graph.units.map(unit => [unit.unitKey, unit]));
    for (const file of ['globals.d.ts', 'other.d.cts', 'empty.d.mts']) assert.equal(units.get(file)?.nodes.length, 1);
    assert.ok(units.get('example.cjs'));
    assert.ok(units.get('other.cts')?.nodes.some(node => node.name === 'other'));
    const module = units.get('package.json');
    assert.ok(module?.nodes.some(node => node.kind === 'Module'));
    assert.ok(module?.edges.some(edge => edge.kind === 'CONTAINS_PROJECT' && edge.properties.retained_source_owner === 'demo'));
    const value = units.get('lib.mts')?.nodes.find(node => node.name === 'nextValue');
    assert.equal(value?.span.startByte, Buffer.byteLength('// é and CRLF\r\n'));
    assert.ok(units.get('app.mts')?.edges.some(edge => edge.kind === 'CALLS' &&
      'node' in edge.target && edge.target.node.unit === 'lib.mts' &&
      edge.target.node.qualified_name === value?.qualifiedName));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('invalid byte spans and foreign or incomplete compiler output refuse the whole result', () => {
  assert.throws(() => span(Buffer.from('é\r\n'), [0, 4], 1, 3), /invalid-definition-span/);
  const result = {
    complete: true, nodes: [], edges: [], workspacePackages: [], resolvedImports: [],
    unresolvedImports: [], unresolvedCalls: [], processedFiles: ['foreign.ts'],
  };
  assert.throws(() => nativeGraph('demo', new Map(), result), /definition-outside-capture/);
  assert.throws(() => nativeGraph('demo', new Map(), { ...result, complete: false }), /typescript-analysis-incomplete/);
});

test('colliding source declarations stay unattributed even when one shares the target file', () => {
  const source = 'function same() {}';
  const input = new Map([['src/a.ts', Buffer.from(source)], ['a.ts', Buffer.from(source)]]);
  const node = (filePath: string, qualifiedName: string) => ({
    label: 'Function', name: qualifiedName, qualifiedName, filePath, startLine: 1, endLine: 1, properties: {},
  });
  const result = nativeGraph('test', input, {
    complete: true, workspacePackages: [], resolvedImports: [], unresolvedImports: [], unresolvedCalls: [],
    processedFiles: [...input.keys()],
    nodes: [node('a.ts', 'a.same'), node('src/a.ts', 'a.same'), node('a.ts', 'a.callee')],
    edges: [{ type: 'CALLS', sourceQN: 'a.same', targetQN: 'a.callee' }],
  });
  const edges = result.units.flatMap(unit => unit.edges);
  assert.equal(edges.length, 1);
  assert.equal(edges[0].source, 0);
  assert.deepEqual(edges[0].target, { symbol: 'a.callee' });
  assert.equal(edges[0].resolution, 'unresolved');
  assert.equal(edges[0].properties.compiler_unresolved, true);
  assert.deepEqual(edges[0].properties.retained_source_candidates, ['a.ts', 'src/a.ts']);
});
