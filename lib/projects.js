'use strict';
const fs = require('node:fs');
const path = require('node:path');

const MANIFEST_SKIP = new Set(['.build', 'build', 'DerivedData', 'node_modules']);
// Local package directories that the Xcode projects and SwiftPM manifests under
// `top` reference by relative path, resolved to absolute paths.
function localPackageRefs(top) {
  const out = [];
  const read = fp => { try { return fs.readFileSync(fp, 'utf8'); } catch { return ''; } };
  (function walk(dir, depth) {
    let entries;
    try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
    for (const e of entries) {
      const fp = path.join(dir, e.name);
      if (e.name === 'Package.swift') {
        for (const m of read(fp).matchAll(/\.package\(\s*(?:name:\s*"[^"]*"\s*,\s*)?path:\s*"([^"]+)"/g)) out.push(path.resolve(dir, m[1]));
      } else if (e.name.endsWith('.xcodeproj')) {
        for (const m of read(path.join(fp, 'project.pbxproj')).matchAll(/isa = XCLocalSwiftPackageReference;\s*relativePath = "?([^";\n]+)"?;/g)) out.push(path.resolve(dir, m[1]));
      } else if (depth < 4 && !e.name.startsWith('.') && !MANIFEST_SKIP.has(e.name) && (e.isDirectory() || e.isSymbolicLink())) {
        walk(fp, depth + 1);
      }
    }
  })(top, 0);
  return out;
}
// The names in `parent` (the real checkout's parent directory) of the packages a
// clone at `clone` builds against, transitively: ['DebugTrace', 'RAVESDK', ...].
// The clone's own references resolve inside its slot; a package's own, inside
// `parent`. Anything outside that layout can't be mirrored and is left alone.
function siblingPackages(clone, parent) {
  const names = new Set();
  const seen = new Set([path.basename(clone)]);
  const queue = [{ dir: clone, base: path.dirname(clone) }];
  while (queue.length) {
    const { dir, base } = queue.pop();
    for (const ref of localPackageRefs(dir)) {
      const rel = path.relative(base, ref);
      if (!rel || rel.startsWith('..') || path.isAbsolute(rel)) continue;
      const top = rel.split(path.sep)[0];
      if (seen.has(top)) continue;
      seen.add(top);
      if (!fs.existsSync(path.join(parent, top))) continue;
      names.add(top);
      queue.push({ dir: path.join(parent, top), base: parent });
    }
  }
  return [...names].sort();
}

module.exports = { localPackageRefs, siblingPackages };
