// Run the shipped request, confirmation and render paths without a server.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const slice = (from, to) => source.slice(source.indexOf(from), source.indexOf(to));
function harness(saved = null) {
  const nodes = new Map();
  const $ = id => {
    if (!nodes.has(id)) nodes.set(id, { hidden: true, dataset: {}, textContent: '', append() {}, querySelector: () => $('status-text') });
    return nodes.get(id);
  };
  const health = { version: '0.1.0', loop: {}, update: { available: true, to: 'v0.2.0' },
    upgrade: { stage: 'done', to: 'v0.1.0' }, upgrade_request_error: saved };
  const context = { $, state: { health }, API: { upgrade: '/api/upgrade' }, Response,
    fetch: async () => new Response('{}'), Date, setTimeout() {},
    pendingForRequest: () => () => {},
    show: (node, visible) => { node.hidden = !visible; },
    setText: (node, text) => { node.textContent = text; },
    setAttr: (node, name, value) => { node[name] = value; }, clear() {}, el() {},
    announce() {}, ok() {}, runnableTasks: () => 0, startCost: () => '',
    plural: (n, word) => `${n} ${word}`, STOP_ASK_MS: 1000,
    UPGRADE_BUSY_STAGES: new Set(['downloading', 'replaced', 'parking', 'restarting']),
  };
  const client = runInNewContext([
    slice('async function request(', 'const deleteReq ='),
    slice('function upgradeOverdue(', 'function initLoopDetail('),
    slice('let loopInfoOpen =', 'async function setLoop('),
    '({upgrade, renderLoop, renderUpgradeError})',
  ].join('\n'), context);
  client.renderLoop();
  return { ...client, context, $, health, attempt: async () => { await client.upgrade(); await client.upgrade(); } };
}

test('JSON, text HTTP and network failures survive redraw and allow confirmed retry', async () => {
  for (const [reply, reason] of [
    [() => new Response(JSON.stringify({ error: '<img onerror=alert(1)> GitHub rate limit', retry_at: 2000000000 }), {status: 500}), '<img onerror=alert(1)> GitHub rate limit'],
    [() => new Response('Proxy unavailable', {status: 502, headers: {'Retry-After': '60'}}), '502'],
    [() => { throw new Error('Network disconnected'); }, 'Network disconnected'],
  ]) {
    const h = harness();
    h.context.fetch = async () => reply();
    await h.attempt();
    h.context.ok(); h.renderLoop();
    assert.equal(h.$('upgrade-error').hidden, false);
    assert.ok(h.$('upgrade-error-text').textContent.includes(reason));
    assert.ok(h.$('upgrade-error-text').textContent.includes('v0.2.0'));
    if (!reason.includes('Network')) assert.match(h.$('upgrade-error-text').textContent, /Retry after/);
    assert.equal(h.$('loop-upgrade').disabled, false);
    h.$('upgrade-error-dismiss').onclick();
    h.renderLoop();
    assert.equal(h.$('upgrade-error').hidden, true);
    h.context.fetch = async () => new Response(JSON.stringify({to: 'v0.2.0'}), {status: 202});
    await h.attempt();
    h.renderLoop();
    assert.equal(h.$('upgrade-error').hidden, true);
  }
});

test('sending survives redraw and repeated taps produce only one POST', async () => {
  const h = harness();
  let calls = 0, release;
  h.context.fetch = async () => { calls++; return new Promise(r => { release = r; }); };
  await h.upgrade();
  const pending = h.upgrade();
  h.renderLoop();
  assert.equal(h.$('loop-upgrade').disabled, true);
  await h.upgrade();
  assert.equal(calls, 1);
  release(new Response('{}'));
  await pending;
});

test('health restores failure over past success, accepted retry clears it, active progress wins', async () => {
  const h = harness({error: 'Release check failed', occurred_at: 'first'});
  assert.equal(h.$('upgrade-error').hidden, false);
  assert.match(h.$('upgrade-error-text').textContent, /Release check failed/);
  h.context.fetch = async () => new Response(JSON.stringify({to: 'v0.2.0'}), {status: 202});
  await h.attempt();
  h.renderLoop();
  assert.equal(h.$('upgrade-error').hidden, true);
  h.health.upgrade_request_error = {error: 'Later failure', occurred_at: 'second'};
  h.renderLoop();
  assert.equal(h.$('upgrade-error').hidden, false);
  h.health.upgrade = {stage: 'downloading'};
  h.renderUpgradeError();
  assert.equal(h.$('upgrade-error').hidden, true);
});

test('failed progress shares the dismissible panel and retry works without cached release', () => {
  const h = harness();
  h.health.update = {available: false};
  h.health.upgrade = {stage: 'failed', to: 'v0.2.0', detail: 'Download interrupted', started_at: 'first'};
  h.renderLoop();
  assert.match(h.$('upgrade-error-text').textContent, /Download interrupted/);
  assert.equal(h.$('loop-upgrade').hidden, false);
  assert.equal(h.$('loop-toggle').hidden, false);
  h.$('upgrade-error-dismiss').onclick();
  h.renderLoop();
  assert.equal(h.$('upgrade-error').hidden, true);
});

test('active progress received before a lost response prevents a spurious failure', async () => {
  const h = harness();
  let reject;
  h.context.fetch = async () => new Promise((resolve, fail) => { reject = fail; });
  await h.upgrade();
  const pending = h.upgrade();
  h.health.upgrade = {stage: 'downloading'};
  h.renderUpgradeError();
  reject(new Error('Response lost'));
  await pending;
  assert.equal(h.$('upgrade-error').hidden, true);
});

test('accepted response only dismisses failures known before the request began', async () => {
  for (const source of ['progress', 'request']) {
    const h = harness(source === 'request' ? {error: 'Old request failure', occurred_at: 'old'} : null);
    if (source === 'progress') {
      h.health.upgrade = {stage: 'failed', to: 'v0.2.0', detail: 'Old download failure', started_at: 'old'};
    }
    let release;
    h.context.fetch = async () => new Promise(resolve => { release = resolve; });
    await h.upgrade();
    const pending = h.upgrade();
    h.context.state.health = {
      ...h.health,
      upgrade_request_error: source === 'request' ? {error: 'New request failure', occurred_at: 'new'} : null,
      upgrade: source === 'progress'
        ? {stage: 'failed', to: 'v0.2.0', detail: 'New download failure', started_at: 'new'}
        : h.health.upgrade,
    };
    h.renderLoop();
    assert.equal(h.$('upgrade-error').hidden, false, `${source}: failure arrives before acknowledgement`);
    assert.match(h.$('upgrade-error-text').textContent, /New .* failure/);
    release(new Response(JSON.stringify({to: 'v0.2.0'}), {status: 202}));
    await pending;
    for (let poll = 0; poll < 2; poll++) {
      h.context.state.health = JSON.parse(JSON.stringify(h.context.state.health));
      h.context.ok(); h.renderLoop();
      assert.equal(h.$('upgrade-error').hidden, false, `${source}: acknowledgement must retain the new failure`);
      assert.match(h.$('upgrade-error-text').textContent, /New .* failure/);
    }
  }
});

test('standalone workers are not included in queue-wide park controls', () => {
  const h = harness();
  h.health.jobs = [{kind: 'task', queue_owned: false, version: '0.1.0'}];
  h.renderLoop();
  assert.equal(h.$('loop-park').hidden, true);
  h.health.jobs = [{kind: 'task', queue_owned: true, version: '0.1.0'}];
  h.renderLoop();
  assert.equal(h.$('loop-park').hidden, false, 'queue-owned work remains controllable after the scheduler stops');
});
