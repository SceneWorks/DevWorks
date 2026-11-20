import * as path from 'node:path';
import type { ExtractProjectResponse, GraphNodeDto, PendingEdgeDto } from './protocol';

export interface Span {
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
  startByte: number;
  endByte: number;
}

interface Definition {
  kind: string;
  name: string;
  qualifiedName: string;
  span: Span;
  parent: number | null;
  properties: Record<string, unknown>;
}

interface Endpoint {
  unit: string;
  kind: string;
  name: string;
  index: number;
}

type Target = { local: number } | { node: { unit: string; kind: string; qualified_name: string } } | { symbol: string };

interface Relationship {
  kind: string;
  source: number;
  target: Target;
  resolution: 'resolved' | 'inferred' | 'unresolved';
  confidence: number;
  properties: Record<string, unknown>;
}

interface Unit {
  unitKey: string;
  nodes: Definition[];
  edges: Relationship[];
}

const ORIGIN: Span = {
  startLine: 1, startColumn: 1, endLine: 1, endColumn: 1, startByte: 0, endByte: 0,
};

export function span(bytes: Buffer, lines: readonly number[], startLine: number, endLine: number): Span {
  if (startLine === 0 && endLine === 0) return ORIGIN;
  const startByte = lines[startLine - 1];
  const endStart = lines[endLine - 1];
  if (startLine < 1 || endLine < startLine || startByte === undefined || endStart === undefined) {
    throw new Error('invalid-definition-span');
  }
  let endByte = lines[endLine] ?? bytes.length;
  if (endByte > endStart && bytes[endByte - 1] === 10) endByte--;
  if (endByte > endStart && bytes[endByte - 1] === 13) endByte--;
  return { startLine, startColumn: 1, endLine, endColumn: endByte - endStart + 1, startByte, endByte };
}

function lineOffsets(bytes: Buffer): number[] {
  const lines = [0];
  for (let index = 0; index < bytes.length; index++) {
    if (bytes[index] === 10) lines.push(index + 1);
  }
  return lines;
}

function targetOf(source: Endpoint, target: Endpoint | undefined, name: string): Target {
  if (!target) return { symbol: name };
  if (source.unit === target.unit) return { local: target.index };
  return { node: { unit: target.unit, kind: target.kind, qualified_name: target.name } };
}

function confidenceOf(properties: Record<string, unknown>, resolved: boolean): number {
  const value = properties.confidence ?? properties.confidence_band;
  if (typeof value === 'number' && Number.isFinite(value) && value >= 0 && value <= 1) return value;
  if (value === 'high') return 0.9;
  if (value === 'medium') return 0.65;
  if (value === 'low') return 0.45;
  return resolved ? 1 : 0;
}

/** Convert the frozen graph without inferring a target from an ambiguous qualified name. */
export function nativeGraph(
  repository: string,
  files: ReadonlyMap<string, Buffer>,
  result: ExtractProjectResponse,
): { units: Unit[] } {
  if (result.complete !== true) throw new Error('typescript-analysis-incomplete');

  const offsets = new Map<string, number[]>();
  const units = new Map<string, Unit>();
  const endpoints = new Map<string, Endpoint[]>();
  const nodeKeys = new Map<string, Set<string>>();

  function offer(name: string, endpoint: Endpoint): void {
    const matches = endpoints.get(name);
    if (matches) matches.push(endpoint);
    else endpoints.set(name, [endpoint]);
  }

  function open(key: string): Unit {
    const current = units.get(key);
    if (current) return current;
    const bytes = files.get(key);
    if (!bytes) throw new Error('definition-outside-capture');
    const lines = lineOffsets(bytes);
    offsets.set(key, lines);
    const unit: Unit = {
      unitKey: key,
      nodes: [{
        kind: 'File', name: path.basename(key), qualifiedName: key,
        span: span(bytes, lines, 1, lines.length), parent: null, properties: {},
      }],
      edges: [],
    };
    units.set(key, unit);
    nodeKeys.set(key, new Set());
    offer(repository + ':' + key, { unit: key, kind: 'File', name: key, index: 0 });
    return unit;
  }

  for (const file of result.processedFiles) open(file);
  const rootManifest = ['package.json', 'angular.json', 'tsconfig.json', 'jsconfig.json']
    .find(file => files.has(file));
  const importsByPackage = new Map<string, string>();
  for (const entry of result.resolvedImports) {
    if (entry.targetPackageQN && files.has(entry.filePath) && !importsByPackage.has(entry.targetPackageQN)) {
      importsByPackage.set(entry.targetPackageQN, entry.filePath);
    }
  }

  function ownerOf(node: GraphNodeDto): string {
    if (node.filePath && files.has(node.filePath)) return node.filePath;
    const properties = node.properties;
    if (node.label === 'Module' && properties.module_kind === 'typescript_workspace_package') {
      const captured = [properties.package_json_path, properties.tsconfig_path, rootManifest]
        .find((file): file is string => typeof file === 'string' && files.has(file));
      if (captured) return captured;
    }
    if (node.label === 'NuGetPackage' && properties.package_type === 'npm') {
      const importer = importsByPackage.get(node.qualifiedName);
      if (importer) return importer;
    }
    throw new Error('definition-outside-capture');
  }

  for (const node of result.nodes) {
    const key = ownerOf(node);
    const unit = open(key);
    const name = node.qualifiedName;
    const kind = node.label === 'NuGetPackage' && node.properties.package_type === 'npm' ? 'Package' : node.label;
    const keys = nodeKeys.get(key)!;
    const identity = JSON.stringify([kind, name]);
    if (keys.has(identity)) continue;
    keys.add(identity);
    const index = unit.nodes.length;
    unit.nodes.push({
      kind, name: node.name, qualifiedName: name, parent: 0,
      span: span(files.get(key)!, offsets.get(key)!, node.startLine, node.endLine),
      properties: { ...node.properties, ...(kind !== node.label ? { retained_label: node.label } : {}) },
    });
    offer(name, { unit: key, kind, name, index });
  }

  function unique(name: string): Endpoint | undefined {
    const options = endpoints.get(name);
    if (options?.length === 1) return options[0];
    return undefined;
  }

  function relationship(edge: PendingEdgeDto): void {
    const target = unique(edge.targetQN);
    const options = endpoints.get(edge.sourceQN) ?? [];
    const candidates = options.length === 1 ? options : options.filter(source => source.unit === target?.unit);
    const anchor = edge.sourceQN === repository ? rootManifest ?? target?.unit : undefined;
    const source: Endpoint | undefined = candidates.length === 1 ? candidates[0] : anchor
      ? { unit: anchor, kind: 'File', name: anchor, index: 0 } : undefined;
    if (!source) return;
    const properties: Record<string, unknown> = { ...edge.properties, ...(anchor ? { retained_source_owner: edge.sourceQN } : {}) };
    const confidence = confidenceOf(properties, target !== undefined);
    const inferred = properties.candidate === true || properties.confidence_band === 'medium' ||
      properties.confidence_band === 'low';
    open(source.unit).edges.push({
      kind: edge.type, source: source.index, target: targetOf(source, target, edge.targetQN),
      resolution: !target ? 'unresolved' : inferred ? 'inferred' : 'resolved',
      confidence, properties,
    });
  }

  for (const edge of result.edges) relationship(edge);
  for (const unresolved of result.unresolvedCalls) {
    relationship({
      type: 'CALLS', sourceQN: unresolved.callerQN, targetQN: unresolved.calleeName,
      properties: {
        confidence: unresolved.confidence, ...(unresolved.receiverType ? { receiver_type: unresolved.receiverType } : {}),
      },
    });
  }
  const imports = new Map(result.resolvedImports.map(entry =>
    [JSON.stringify([entry.fileQN, entry.importedNamespace]), entry]));
  for (const unresolved of result.unresolvedImports) {
    const entry = imports.get(JSON.stringify([unresolved.fileQN, unresolved.importedNamespace]));
    if (entry && (entry.targetFileQN || entry.targetPackageQN)) continue;
    relationship({
      type: 'IMPORTS', sourceQN: unresolved.fileQN, targetQN: unresolved.importedNamespace,
      properties: { ...(entry ? { classification: entry.classification, import_kind: entry.importKind } : {}) },
    });
  }

  return { units: [...units.values()].sort((left, right) => left.unitKey.localeCompare(right.unitKey, 'en')) };
}
