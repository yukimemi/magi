import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const loading = source.slice(source.indexOf('const storeReads ='), source.indexOf('function loadRuns('));
const flush = () => new Promise(resolve => setImmediate(resolve));
function harness(existing = null) {
  const requests = [], renders = [], notices = [];
  const state = { queue: existing, talks: null, rev: {}, talkWaits: new Map() };
  const api = runInNewContext(`${loading}\n({loadStore})`, {
    state, getJson: url => new Promise((resolve, reject) => requests.push({ url, resolve, reject })),
    sectionNotice: (...args) => notices.push(args), ok() {},
    setTimeout: () => 1, clearTimeout() {},
  });
  const load = change => api.loadStore('queue', '/api/queue', change, (rows, partial) => {
    state.queue = rows;
    renders.push({ rows: structuredClone(rows), partial });
  });
  return { state, requests, renders, notices, load };
}
const row = (id, priority = 0) => ({ id, priority });
test('first page renders before old history and final rows retain priority order', async () => {
  const h = harness();
  const done = h.load();
  assert.equal(h.requests[0].url, '/api/queue/page');
  h.requests[0].resolve({ rows: [row('new')], next: 'new' });
  await flush();
  assert.equal(h.renders[0].rows[0].id, 'new');
  assert.equal(h.requests[1].url, '/api/queue/page?before=new');
  h.requests[1].resolve({ rows: [row('old', 3)], next: null });
  await done;
  assert.deepEqual(Array.from(h.state.queue, r => r.id), ['old', 'new']);
  assert.ok(h.renders.every(render => render.partial), 'completion retains existing row controls');
});
test('background pages preserve old rows until completion, and failures preserve useful content', async () => {
  const h = harness([row('deleted')]);
  const done = h.load();
  h.requests[0].resolve({ rows: [row('new')], next: 'new' });
  await flush();
  assert.ok(h.state.queue.some(r => r.id === 'deleted'));
  h.requests[1].reject(new Error('old history offline'));
  await done;
  assert.ok(h.state.queue.some(r => r.id === 'new'));
  assert.ok(h.notices.some(n => n[2].includes('old history offline') && typeof n[3] === 'function'));
  const retry = h.load();
  h.requests[2].resolve({ rows: [row('new')], next: null });
  await retry;
  assert.deepEqual(Array.from(h.state.queue, r => r.id), ['new']);
});
test('notification during pagination supersedes walk without acknowledging incomplete revision', async () => {
  const h = harness();
  const done = h.load({ rev: 1 });
  h.requests[0].resolve({ rows: [row('new')], next: 'new' });
  await flush();
  h.load({ rev: 2 });
  h.requests[1].resolve({ rows: [row('old')], next: 'old' });
  await flush();
  assert.equal(h.state.rev.queue, undefined);
  assert.equal(h.requests[2].url, '/api/queue/page');
  h.requests[2].resolve({ rows: [row('newer')], next: null });
  await done;
  assert.equal(h.state.rev.queue, 2);
  assert.deepEqual(Array.from(h.state.queue, r => r.id), ['newer']);
});
