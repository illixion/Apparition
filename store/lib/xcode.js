'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');
const { snapshot, publish, uuidIndex } = require('./artifacts');

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024, ...options });
  if (result.status !== 0) throw new Error(`${command} failed during Xcode handoff`);
  return result.stdout;
}
function info(app) {
  const file = fs.existsSync(path.join(app, 'Contents', 'Info.plist')) ? path.join(app, 'Contents', 'Info.plist') : path.join(app, 'Info.plist');
  return JSON.parse(run('plutil', ['-convert', 'json', '-o', '-', file]));
}
function platformOf(app, plist) {
  const executable = path.join(app, fs.existsSync(path.join(app, 'Contents')) ? 'Contents/MacOS' : '', plist.CFBundleExecutable);
  const output = run('/usr/bin/xcrun', ['vtool', '-show-build', executable]);
  const platforms = [...output.matchAll(/^\s*platform\s+(\S+)/gm)].map(match => match[1]);
  const supported = { MACOS: 'macOS', MACCATALYST: 'macOS', IOS: 'iOS', VISIONOS: 'visionOS', TVOS: 'tvOS' };
  const values = [...new Set(platforms.map(value => supported[value]))];
  if (!platforms.length || values.includes(undefined) || values.length !== 1) throw new Error('Register a device or Mac build, not a simulator or mixed-platform product.');
  return values[0];
}
function signature(app) {
  const details = spawnSync('codesign', ['-d', app], { encoding: 'utf8' });
  if (details.status !== 0) {
    if (/not signed at all/.test(details.stderr)) return { state: 'unsigned', debuggable: false, teamId: null };
    throw new Error('The product has no readable code signature.');
  }
  run('codesign', ['--verify', '--deep', '--strict', app]);
  // codesign describes signatures on stderr; never export its raw output.
  const raw = spawnSync('codesign', ['-dvv', app], { encoding: 'utf8' });
  const teamId = /^TeamIdentifier=(.+)$/m.exec(raw.stderr)?.[1] || null;
  const entitlements = spawnSync('codesign', ['-d', '--entitlements', ':-', app], { encoding: 'utf8' });
  let debuggable = null;
  if (entitlements.stdout.trim()) {
    const converted = spawnSync('plutil', ['-convert', 'json', '-o', '-', '-'], { input: entitlements.stdout, encoding: 'utf8' });
    if (converted.status === 0) {
      const values = JSON.parse(converted.stdout);
      debuggable = values['get-task-allow'] === true || values['com.apple.security.get-task-allow'] === true;
    }
  }
  return { state: 'signed', debuggable, teamId: teamId === 'not set' ? null : teamId };
}

function capture(project, output) {
  const context = { format: 'apparition-xcode-context/1', buildId: crypto.randomUUID().replaceAll('-', ''), project: fs.realpathSync(project), source: snapshot(project) };
  fs.mkdirSync(path.dirname(output), { recursive: true });
  fs.writeFileSync(output, JSON.stringify(context), { mode: 0o600 });
  return context;
}

function register({ app, project, configuration = 'Unknown', symbols, context, store, symbolStore }) {
  app = fs.realpathSync(app);
  if (!app.endsWith('.app') || !fs.statSync(app).isDirectory()) throw new Error('Select an existing .app product.');
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-xcode-'));
  try {
    const frozen = path.join(directory, path.basename(app));
    const copied = spawnSync('/bin/cp', ['-cR', app, frozen], { stdio: 'ignore' });
    if (copied.status !== 0) { if (fs.existsSync(frozen)) fs.rmSync(frozen, { recursive: true }); fs.cpSync(app, frozen, { recursive: true }); }
    const plist = info(frozen);
    if (typeof plist.CFBundleIdentifier !== 'string' || typeof plist.CFBundleExecutable !== 'string') throw new Error('The product needs a bundle identifier and executable.');
    const platform = platformOf(frozen, plist);
    const signing = signature(frozen);
    let before = null, buildId = crypto.randomUUID().replaceAll('-', '');
    if (context) {
      const recorded = JSON.parse(fs.readFileSync(context));
      if (recorded.format !== 'apparition-xcode-context/1' || !/^[a-f0-9]{32}$/.test(recorded.buildId) || !recorded.source?.fingerprint ||
          !project || recorded.project !== fs.realpathSync(project)) throw new Error('Invalid or mismatched Xcode source context.');
      before = recorded.source; buildId = recorded.buildId;
    }
    const observed = project ? snapshot(project) : null;
    const indices = uuidIndex(frozen);
    if (!Object.keys(indices).length) throw new Error('The product contains no Mach-O binaries.');
    const products = symbols ? path.dirname(fs.realpathSync(symbols)) : path.dirname(app);
    if (symbols && !symbols.endsWith('.dSYM')) throw new Error('Select a dSYM bundle.');
    const archive = symbolStore || process.env.DEBUGTRACE_DSYM_DIR || path.join(process.env.XDG_STATE_HOME || path.join(os.homedir(), '.local', 'state'), 'debugtrace', 'dsyms');
    const result = spawnSync('/opt/homebrew/bin/bash', ['-c', 'source "$1"; archive_dsyms', 'xcode-symbols', path.join(__dirname, 'symbols.sh')], {
      env: { ...process.env, APP_PATH: frozen, APP_BUNDLE: frozen, BUILD_BUNDLE_ID: plist.CFBundleIdentifier,
        BUILD_ID: buildId, TRACE_KEY_ID: '', DEVICE_NAME: 'xcode', PLATFORM: platform, CONFIG: configuration,
        DEBUGTRACE_DSYM_DIR: archive, DSYM_PRODUCTS_OVERRIDE: products, DSYM_SOURCE: symbols ? fs.realpathSync(symbols) : '' }, encoding: 'utf8' });
    if (result.status !== 0) throw new Error('Could not retain the Xcode product symbols.');
    const symbolIndex = path.join(archive, `${plist.CFBundleIdentifier}@${buildId}`, 'uuids.json');
    if (symbols && !fs.existsSync(symbolIndex)) throw new Error('The supplied dSYM does not match the Xcode product.');
    let artifact = frozen;
    if (platform !== 'macOS') {
      const payload = path.join(directory, 'Payload'); fs.mkdirSync(payload);
      fs.renameSync(frozen, path.join(payload, path.basename(app)));
      artifact = path.join(directory, 'product.ipa');
      run('ditto', ['-c', '-k', '--keepParent', payload, artifact]);
      fs.renameSync(path.join(payload, path.basename(app)), frozen);
    }
    return publish({ app: frozen, artifact, project, before, after: before ? observed : null, buildId,
      platform, configuration, originalBundleId: plist.CFBundleIdentifier, store, symbols: symbolIndex,
      origin: { kind: 'xcode-registration', signature: signing, signaturePreserved: true,
        observedSource: before ? null : observed, binaryDigests: Object.fromEntries(Object.entries(indices).map(([id, binary]) => [id, binary.sha256])) } });
  } finally { fs.rmSync(directory, { recursive: true, force: true }); }
}

const environment = 'export PATH="$PATH:$HOME/.local/bin:$HOME/bin:/opt/homebrew/bin:/usr/local/bin"\nif [ -z "$APPARITION_CLI" ]; then APPARITION_CLI=apparition; fi\n';
function configure({ spec, target, output, generate = true }) {
  spec = path.resolve(spec);
  if (!target || /[\r\n]/.test(target)) throw new Error('Choose an Xcode target.');
  output = path.resolve(output || path.join(path.dirname(spec), 'project.apparition.yml'));
  if (output === spec) throw new Error('The optional spec must be separate from the original.');
  const before = environment + 'set -e\n"$APPARITION_CLI" xcode snapshot --project "$SRCROOT" --output "$DERIVED_FILE_DIR/apparition/source-before.json"\n';
  const after = environment + 'set -e\nset -- xcode register --app "$TARGET_BUILD_DIR/$FULL_PRODUCT_NAME" --project "$SRCROOT" --configuration "$CONFIGURATION" --context "$DERIVED_FILE_DIR/apparition/source-before.json"\nif [ -d "$DWARF_DSYM_FOLDER_PATH/$DWARF_DSYM_FILE_NAME" ]; then set -- "$@" --symbols "$DWARF_DSYM_FOLDER_PATH/$DWARF_DSYM_FILE_NAME"; fi\n"$APPARITION_CLI" "$@"\n';
  const name = `${target} (Apparition)`;
  const document = { include: [{ path: spec, relativePaths: false }],
    schemes: { [name]: { build: { targets: { [target]: 'all' },
      preActions: [{ name: 'Capture Apparition source context', settingsTarget: target, script: before }],
      postActions: [{ name: 'Register with Apparition', settingsTarget: target, script: after }] } } } };
  const content = JSON.stringify(document, null, 2) + '\n';
  if (fs.existsSync(output) && fs.readFileSync(output, 'utf8') !== content) throw new Error('The optional spec already exists with different content; choose another output.');
  fs.mkdirSync(path.dirname(output), { recursive: true });
  fs.writeFileSync(output, content);
  if (generate) run('xcodegen', ['--spec', output, '--project', path.dirname(spec), '--project-root', path.dirname(spec)]);
  return { spec: output, scheme: name, originalSpec: spec };
}

module.exports = { capture, register, configure, signature, platformOf };
