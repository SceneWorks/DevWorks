import * as fs from 'node:fs';
import * as path from 'node:path';
import { extractProject } from './extractor';
import { nativeGraph } from './native';

interface Job {
  repository: string;
  root: string;
  files: string[];
  readable: string[];
}

const [requestPath, resultPath] = process.argv.slice(2);
try {
  if (!requestPath || !resultPath) throw new Error('invalid-typescript-job');
  const { repository, root, files, readable } = JSON.parse(fs.readFileSync(requestPath, 'utf8')) as Job;
  const config = files.filter(file => /(?:^|\/)(?:tsconfig|jsconfig)\.json$/.test(file))
    .sort((left, right) => left.split('/').length - right.split('/').length || left.localeCompare(right, 'en'))[0];
  const result = extractProject(repository, root, path.join(root, config ?? 'tsconfig.json'),
    () => {}, files, { maxFileSizeKb: 4096, maxAnalysisTextKb: 4096 });
  const originals = new Map(readable.map(file => [file, fs.readFileSync(path.join(root, file))]));
  fs.writeFileSync(resultPath, JSON.stringify(nativeGraph(repository, originals, result)), { flag: 'wx' });
} catch (error) {
  const allowed = new Set(['invalid-typescript-job', 'typescript-analysis-incomplete',
    'invalid-definition-span', 'definition-outside-capture']);
  const message = error instanceof Error && allowed.has(error.message) ? error.message : 'typescript-compiler-failed';
  process.stderr.write(message + '\n');
  process.exitCode = 1;
}
