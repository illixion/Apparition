'use strict';
// `node server.js set-password`: asks for the web UI password twice, without
// echoing it, and stores its scrypt hash as `passwordHash` in config.json.
// The running server picks it up through its config watcher, and every
// session made with the old password stops working.
const fs = require('node:fs');
const { hashPassword } = require('./auth');

function ask(prompt) {
  return new Promise((resolve, reject) => {
    const stdin = process.stdin;
    if (!stdin.isTTY) return reject(new Error('run this in a terminal: it reads the password without echoing it'));
    process.stdout.write(prompt);
    stdin.setRawMode(true);
    stdin.resume();
    stdin.setEncoding('utf8');
    let value = '';
    const onData = chunk => {
      for (const ch of chunk) {
        if (ch === '\r' || ch === '\n') {
          stdin.setRawMode(false); stdin.pause(); stdin.off('data', onData);
          process.stdout.write('\n');
          return resolve(value);
        }
        if (ch === '\u0003') { process.stdout.write('\n'); process.exit(130); }
        if (ch === '\u007f' || ch === '\b') value = value.slice(0, -1);
        else value += ch;
      }
    };
    stdin.on('data', onData);
  });
}

async function run(configPath) {
  try {
    const first = await ask('New web UI password: ');
    const second = await ask('Again: ');
    if (first !== second) throw new Error('the two entries differ; nothing changed');
    const hash = hashPassword(first);
    const config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
    config.passwordHash = hash;
    const tmp = configPath + '.tmp';
    fs.writeFileSync(tmp, JSON.stringify(config, null, 2) + '\n', { mode: 0o600 });
    fs.renameSync(tmp, configPath);
    console.log(`Saved to ${configPath}. The page now asks for this password.`);
  } catch (e) {
    console.error(e.message);
    process.exitCode = 1;
  }
}

module.exports = { run };
