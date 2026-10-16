'use strict';
const { test, describe } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const SCRIPT = path.join(__dirname, '..', 'bin', 'build-and-sign');
const BASH = fs.existsSync('/opt/homebrew/bin/bash') ? '/opt/homebrew/bin/bash' : 'bash';
const bashMajor = Number((spawnSync(BASH, ['-c', 'echo ${BASH_VERSINFO[0]}'], { encoding: 'utf8' }).stdout || '0').trim());

// Run from an empty directory: the script creates ./build and reads the
// enclosing git repo, so never from the checkout itself.
function run(args) {
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), 'bas-test-'));
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'bas-home-'));
  return spawnSync(BASH, [SCRIPT, ...args], {
    cwd, encoding: 'utf8', timeout: 20000,
    env: { PATH: process.env.PATH, HOME: home, BUILD_SIGNING_GLOBAL_CONF: path.join(home, 'none.conf') },
  });
}

describe('bin/build-and-sign', () => {
  test('parses (bash -n)', () => {
    const r = spawnSync(BASH, ['-n', SCRIPT], { encoding: 'utf8' });
    assert.equal(r.status, 0, r.stderr);
  });

  test('is executable and has a bash shebang', () => {
    assert.ok(fs.statSync(SCRIPT).mode & 0o111);
    assert.match(fs.readFileSync(SCRIPT, 'utf8').split('\n')[0], /^#!.*bash/);
  });

  test('contains no machine-specific identifiers', () => {
    const src = fs.readFileSync(SCRIPT, 'utf8');
    assert.doesNotMatch(src, /\/Users\/[a-z]+\//i);
    assert.doesNotMatch(src, /com\.illixion/);
  });

  test('every flag in the usage header is handled by the parser', { skip: bashMajor < 4 && 'needs bash 4+' }, () => {
    const src = fs.readFileSync(SCRIPT, 'utf8');
    const header = src.slice(0, src.indexOf('PROJECT_ROOT='));
    const documented = [...header.matchAll(/^#\s+build-and-sign\s+(--[a-z-]+)/gm)].map(m => m[1]);
    assert.ok(documented.length >= 10, `found only ${documented.length} documented flags`);
    for (const flag of documented) assert.ok(src.includes(`${flag})`) || src.includes(`${flag}|`), `${flag} is documented but not parsed`);
  });

  test('--help prints the usage header and exits 0', { skip: bashMajor < 4 && 'needs bash 4+' }, () => {
    const r = run(['--help']);
    assert.equal(r.status, 0, r.stderr);
    assert.match(r.stdout, /^Build and sign an app for device deployment\./);
    assert.match(r.stdout, /--distribution/);
  });

  test('an unknown option exits non-zero', { skip: bashMajor < 4 && 'needs bash 4+' }, () => {
    // No conf exists, so this must fail either way, but it has to fail cleanly.
    const r = run(['--no-such-flag']);
    assert.notEqual(r.status, 0);
  });

  test('without any signing config it says what is missing', { skip: bashMajor < 4 && 'needs bash 4+' }, () => {
    const r = run([]);
    assert.notEqual(r.status, 0);
    assert.match(r.stderr, /No config found/);
  });
  // The flag parser and the launch/MCP resolution, run on their own: from the
  // flag defaults to the end of the MCP_DEFAULT block.
  function resolveLaunch(args, { mcpDefault = 'on', platform = 'visionOS' } = {}) {
    const src = fs.readFileSync(SCRIPT, 'utf8');
    const part = src.slice(src.indexOf('\nNO_DEPLOY=false'), src.indexOf("# Xcode's incremental build cache."));
    const script = `set -euo pipefail\nPLATFORM=${platform}\nMCP_DEFAULT=${mcpDefault}\nEXTRA_BUILD_SETTINGS=()\n${part}\n`
      + 'echo "run=$RUN_APP log=$LOG_APP mcp=$MCP_SESSION"';
    const r = spawnSync(BASH, ['-c', script, 'build-and-sign', ...args], { encoding: 'utf8' });
    assert.equal(r.status, 0, r.stderr);
    return { out: r.stdout.trim(), err: r.stderr };
  }

  test('only --run (or --log) launches; MCP decides what kind of launch', { skip: bashMajor < 4 && 'needs bash 4+' }, () => {
    const cases = [
      [[], 'on', 'run=false log=false mcp=false'],
      [['--mcp'], 'on', 'run=false log=false mcp=false'],
      [['--mcp'], 'off', 'run=false log=false mcp=false'],
      [['--run'], 'on', 'run=true log=true mcp=true'],
      [['--run'], 'off', 'run=true log=false mcp=false'],
      [['--run', '--mcp'], 'off', 'run=true log=true mcp=true'],
      [['--mcp', '--run'], 'off', 'run=true log=true mcp=true'],
      [['--run', '--no-mcp'], 'on', 'run=true log=false mcp=false'],
      [['--no-mcp', '--run'], 'on', 'run=true log=false mcp=false'],
      [['--log', '--no-mcp'], 'on', 'run=true log=true mcp=false'],
      [['--run', '--distribution'], 'on', 'run=true log=false mcp=false'],
    ];
    for (const [args, mcpDefault, want] of cases) {
      assert.equal(resolveLaunch(args, { mcpDefault }).out, want, `${args.join(' ') || '(none)'} with MCP_DEFAULT=${mcpDefault}`);
    }
    assert.equal(resolveLaunch(['--run'], { platform: 'macOS' }).out, 'run=true log=false mcp=false');
  });

  test('--mcp without --run says it only installs', { skip: bashMajor < 4 && 'needs bash 4+' }, () => {
    assert.match(resolveLaunch(['--mcp']).err, /--mcp without --run installs only/);
    assert.equal(resolveLaunch(['--run', '--mcp']).err, '');
    assert.equal(resolveLaunch(['--mcp', '--no-deploy']).err, '');
  });
});
