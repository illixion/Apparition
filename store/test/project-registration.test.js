'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { registerProject, validate } = require('../lib/project-registration');

function fixture() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-registration-'));
  const repo = path.join(directory, 'Repo'); fs.mkdirSync(repo);
  execFileSync('git', ['init', '-b', 'main', repo], { stdio: 'ignore' });
  fs.writeFileSync(path.join(repo, 'main.txt'), 'fixture');
  execFileSync('git', ['-C', repo, 'add', 'main.txt']);
  execFileSync('git', ['-C', repo, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgsign=false', 'commit', '-m', 'Fixture'], { stdio: 'ignore' });
  fs.writeFileSync(path.join(repo, 'build-signing.conf'), 'exit 77 # Must never be sourced by registration\n');
  const config = { repos: [], passwordHash: 'fixture-hash', buildSettings: { untouched: true }, bundlePrefix: ['com.example'] };
  const file = path.join(directory, 'config.json'); fs.writeFileSync(file, JSON.stringify(config));
  const input = { name: 'Example', path: repo, platforms: [{ platform: 'visionOS', device: 'Headset', bundleId: 'com.example.app' }] };
  return { directory, repo, config, file, input };
}

test('registration atomically preserves configuration and records a canonical checkout with current branch', () => {
  const f = fixture();
  const alias = path.join(f.directory, 'Alias'); fs.symlinkSync(f.repo, alias);
  const { project, config } = registerProject(f.file, { ...f.input, path: alias });
  assert.equal(project.path, fs.realpathSync(f.repo)); assert.deepEqual(project.branches, ['main']);
  assert.deepEqual(config, { ...f.config, repos: [project] });
  assert.deepEqual(JSON.parse(fs.readFileSync(f.file)), config);
  assert.equal(fs.statSync(f.file).mode & 0o777, 0o600);
  assert.throws(() => registerProject(f.file, { ...f.input, name: 'Another' }), /already registered/);
  assert.deepEqual(JSON.parse(fs.readFileSync(f.file)), config);
});

test('registration rejects unsafe names, missing setup, duplicate targets and nonexistent branches', () => {
  const f = fixture();
  for (const input of [
    { ...f.input, name: '../bad' }, { ...f.input, path: 'relative' },
    { ...f.input, path: path.join(f.repo, 'subdirectory') },
    { ...f.input, branches: ['missing'] }, { ...f.input, branches: ['current'] },
    { ...f.input, platforms: [] }, { ...f.input, platforms: [f.input.platforms[0], f.input.platforms[0]] },
    { ...f.input, platforms: [{ ...f.input.platforms[0], device: 'AVP\nother' }] },
    { ...f.input, buildSettings: ['UNEXPECTED=1'] },
  ]) assert.throws(() => validate(input, []));
  fs.mkdirSync(path.join(f.repo, 'Nested'));
  assert.throws(() => validate({ ...f.input, path: path.join(f.repo, 'Nested') }, []), /root/);
  fs.unlinkSync(path.join(f.repo, 'build-signing.conf'));
  assert.throws(() => registerProject(f.file, f.input), /build-signing.conf/);
  assert.deepEqual(JSON.parse(fs.readFileSync(f.file)), f.config);
});

test('registration can write a minimal conf, keeps it out of Git and undoes it when the config changed', () => {
  const f = fixture();
  fs.unlinkSync(path.join(f.repo, 'build-signing.conf'));
  fs.mkdirSync(path.join(f.repo, 'App.xcodeproj'));
  const generate = { container: 'App.xcodeproj', scheme: "My App's" };
  assert.throws(() => registerProject(f.file, { ...f.input, generate: { container: '../x.xcodeproj', scheme: 'S' } }), /not at the root|Choose/);
  assert.equal(fs.existsSync(path.join(f.repo, 'build-signing.conf')), false);
  // The config file changes between validation and the write: nothing is left behind.
  const original = fs.readFileSync(f.file);
  const { project } = registerProject(f.file, { ...f.input, generate });
  const conf = fs.readFileSync(path.join(f.repo, 'build-signing.conf'), 'utf8');
  assert.match(conf, /^PROJECT_PATH='App.xcodeproj'$/m);
  assert.match(conf, /^SCHEME_NAME='My App'\\''s'$/m);
  assert.match(conf, /PLATFORM="\$\{PLATFORM:-visionOS\}"/);
  assert.equal(execFileSync('git', ['-C', f.repo, 'status', '--porcelain', '--untracked-files=all']).toString().includes('build-signing.conf'), false);
  assert.equal(Object.getOwnPropertySymbols(project).length, 1);
  assert.ok(!JSON.stringify(JSON.parse(fs.readFileSync(f.file))).includes('generate'));
  assert.notEqual(fs.readFileSync(f.file).toString(), original.toString());
});

test('registration never replaces an existing conf with a generated one', () => {
  const f = fixture();
  fs.mkdirSync(path.join(f.repo, 'App.xcodeproj'));
  registerProject(f.file, { ...f.input, generate: { container: 'App.xcodeproj', scheme: 'App' } });
  assert.match(fs.readFileSync(path.join(f.repo, 'build-signing.conf'), 'utf8'), /exit 77/);
});
