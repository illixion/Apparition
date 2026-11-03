const {test} = require('node:test');
const assert = require('node:assert/strict');
const {ProjectList, targetKey} = require('../public/shared/projects');

// Exercise the shared submission state machine without either shell's transport.
function fixture(build) {
  const list = Object.assign(Object.create(ProjectList.prototype), {
    options:{build}, selections:{}, pending:new Set(), messages:new Map(), render() {},
  });
  const repo = {name:'Example', branches:['main'], git:{branch:'main',commit:'abc1234'},
    deviceBranches:{AVP:'feature/headset'}, platforms:[
      {device:'AVP',platform:'visionOS',bundleId:'com.example.app'},
      {device:'AVP',platform:'iPad',bundleId:'com.example.ipad'},
    ]};
  return {list,repo};
}

test('shared deploy distinguishes two platforms on one device and honors its branch override', async () => {
  let sent;
  const {list,repo} = fixture(async (...args) => {sent=args; return {jobs:[{id:12}]};});
  list.selection(repo).chosen.add(targetKey(repo.platforms[1]));
  await list.submit(repo,true);
  assert.deepEqual(sent[1],[{device:'AVP',platform:'iPad',branch:'feature/headset'}]);
  assert.match(list.messages.get(repo.name), /#12/);
});

test('pending feedback precedes the request and duplicate clicks cannot enqueue twice', async () => {
  let finish, calls=0;
  const {list,repo} = fixture(() => {calls++; assert.match(list.messages.get(repo.name),/Starting/); return new Promise(resolve => {finish=resolve;});});
  list.selection(repo).chosen.add(targetKey(repo.platforms[0]));
  const first=list.submit(repo,true);
  await list.submit(repo,true);
  assert.equal(calls,1);
  assert.equal(list.pending.has(repo.name),true);
  finish({jobs:[{id:1}]}); await first;
  assert.equal(list.pending.has(repo.name),false);
});

test('build-only never installs or launches, and failures stay on the project', async () => {
  const {list,repo} = fixture(async (_repo,_targets,flags) => {
    assert.equal(flags.run,false); assert.equal(flags.deploy,false); throw new Error('Build Mac unavailable');
  });
  const selection=list.selection(repo); selection.launch=true; selection.chosen.add(targetKey(repo.platforms[0]));
  await list.submit(repo,false);
  assert.match(list.messages.get(repo.name),/Could not start: Build Mac unavailable/);
  assert.equal(list.pending.size,0);
});

test('shared markup escapes service values and uses the Store freshness result', () => {
  const {list,repo} = fixture();
  repo.name='<img src=x onerror=alert(1)>';
  const state={builds:[],running:[],queued:[],installs:{},deviceApps:{AVP:{apps:{'com.example.app':{version:'1',buildNo:'abc1234'}}}}};
  const markup=list.markup(repo,state,[]);
  assert.ok(!markup.includes('<img'));
  assert.match(markup,/&lt;img/);
  assert.match(markup,/class="dot ok"/);
  repo.git.dirty=true;
  assert.match(list.markup(repo,state,[]),/class="dot warn"/);
});

test('a reply from a previous connection cannot change the new connection', async () => {
  let finish;
  const {list,repo} = fixture(() => new Promise(resolve => {finish=resolve;}));
  list.selection(repo).chosen.add(targetKey(repo.platforms[0]));
  const oldRequest=list.submit(repo,true);
  list.reset();
  finish({jobs:[{id:99}]}); await oldRequest;
  assert.equal(list.messages.size,0);
  assert.equal(list.pending.size,0);
});
