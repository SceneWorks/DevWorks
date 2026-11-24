'use strict';

const fs = require('node:fs');
const path = require('node:path');
const lock = require('./package-lock.json');
const output = process.argv[2];
if (!output || process.argv.length !== 3) throw new Error('Provide one staging directory path');
fs.mkdirSync(output);

const excluded = /^(?:app|linter|server)\.js$|\.test\.js$/;
const sources = path.join(__dirname, 'dist');
const destination = path.join(output, 'dist');
fs.mkdirSync(destination);
for (const item of fs.readdirSync(sources, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
  if (item.isFile() && item.name.endsWith('.js') && !excluded.test(item.name)) {
    fs.copyFileSync(path.join(sources, item.name), path.join(destination, item.name), fs.constants.COPYFILE_EXCL);
  }
}

// The compiler dependency closure is separate from the preserved test server and build tools.
const included = new Set();
function stage(name) {
  const relative = 'node_modules/' + name;
  if (included.has(relative)) return;
  const declared = lock.packages[relative];
  if (!declared) throw new Error('Missing locked compiler dependency ' + name);
  const root = path.join(__dirname, relative);
  const installed = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
  if (installed.version !== declared.version) throw new Error('Incorrect version for ' + name);
  included.add(relative);
  fs.cpSync(root, path.join(output, relative), {
    recursive: true,
    force: false,
    errorOnExist: true,
    filter(file) {
      const local = path.relative(root, file).split(path.sep);
      if (local.includes('node_modules')) return false;
      // The compiler entrypoint and standard libraries suffice for the TypeScript API.
      // Its language-server executables and automatic type acquisition are separate tools.
      if (name === 'typescript' && (local[0] === 'bin' || local[0] === 'lib' && local.length > 1
          && !/^(?:typescript\.(?:js|d\.ts)|lib\..*\.d\.ts)$/.test(local[1]))) return false;
      const stat = fs.lstatSync(file);
      if (!stat.isFile() && !stat.isDirectory()) throw new Error('Non-regular compiler asset ' + file);
      return true;
    },
  });
  for (const dependency of Object.keys(declared.dependencies || {}).sort()) stage(dependency);
}
for (const root of ['ts-morph', 'typescript']) stage(root);

const processModules = /(?:require|import)\(\s*['"](?:node:)?(?:net|http|https|dgram|dns|tls|child_process)['"]\s*\)/;
for (const file of fs.readdirSync(output, { recursive: true })) {
  if (!/\.[cm]?js$/.test(file)) continue;
  const content = fs.readFileSync(path.join(output, file), 'utf8');
  if (processModules.test(content)) throw new Error('Compiler asset imports a network or process API: ' + file);
}
