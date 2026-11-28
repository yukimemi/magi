import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const loading = source.slice(source.indexOf('const storeReads ='), source.indexOf('function loadRuns('));
const ordering = source.slice(source.indexOf('const priorityOrder ='), source.indexOf('function priorityEdits('));
const flush = () => new Promise(resolve => setImmediate(resolve));
function harness(existing = null, name = 'queue') {
  const requests = [], renders = [], notices = [];
  const state = { queue: null, talks: null, [name]: existing, rev: {}, talkWaits: new Map() };
  const get = url => new Promise((resolve, reject) => requests.push({ url, resolve, reject }));
  const api = runInNewContext(`${ordering}\n${loading}\n({loadStore})`, {
    state, getJson: get, getTalkRows: get, talksNext: null, olderTalksRead: null,
    sortTalks: rows => rows.sort((a, b) => b.id.localeCompare(a.id)),
    sectionNotice: (...args) => notices.push(args), ok() {}, showOlderTalks() {},
    setTimeout: () => 1, clearTimeout() {},
  });
  const load = change => api.loadStore(name, `/api/${name}`, change, (rows, partial) => {
    state[name] = rows;
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
test('continuous updates cannot starve older pages and contiguous deltas reconcile after the walk', async () => {
  const h = harness();
  const done = h.load({ rev: 1 });
  h.requests[0].resolve({ rows: [row('new')], next: 'page2' });
  await flush();
  h.load({ rev: 2, delta: { base: 1, changed: ['new', 'retained'], removed: [] } });
  h.requests[1].resolve({ rows: [row('middle')], next: 'page3' });
  await flush();
  assert.equal(h.requests[2].url, '/api/queue/page?before=page3');
  h.load({ rev: 3, delta: { base: 2, changed: ['inserted'], removed: ['middle'] } });
  // A question refresh can enqueue the same event a second time.
  h.load({ rev: 3, delta: { base: 2, changed: ['inserted'], removed: ['middle'] } });
  h.requests[2].resolve({ rows: [row('older')], next: 'page4' });
  await flush();
  assert.ok(h.state.queue.some(r => r.id === 'older'));
  h.load({ rev: 4, delta: { base: 3, changed: ['middle'], removed: ['new'] } });
  assert.equal(h.requests[3].url, '/api/queue/page?before=page4');
  h.requests[3].resolve({ rows: [row('oldest')], next: null });
  await flush();
  assert.equal(h.state.rev.queue, 1, 'only the completed revision is acknowledged');
  assert.ok(h.state.queue.some(r => r.id === 'oldest'));
  assert.equal(h.requests[4].url, '/api/queue?ids=retained%2Cinserted%2Cmiddle');
  h.requests[4].resolve([row('retained'), row('inserted'), row('middle', 2)]);
  await done;
  assert.equal(h.state.rev.queue, 4);
  assert.deepEqual(Array.from(h.state.queue, r => r.id), ['middle', 'inserted', 'older', 'oldest', 'retained']);
});

test('a skipped delta base still finishes enumeration before starting a fresh snapshot', async () => {
  const h = harness();
  const done = h.load({ rev: 1 });
  h.requests[0].resolve({ rows: [row('new')], next: 'next' });
  await flush();
  h.load({ rev: 5, delta: { base: 4, changed: ['new'], removed: [] } });
  h.requests[1].resolve({ rows: [row('old')], next: null });
  await flush();
  assert.ok(h.state.queue.some(r => r.id === 'old'));
  assert.equal(h.state.rev.queue, 1);
  assert.equal(h.requests[2].url, '/api/queue/page');
  h.requests[2].resolve({ rows: [row('new'), row('old')], next: null });
  await done;
  assert.equal(h.state.rev.queue, 5);
});

test('a pending whole status refresh survives a later transcript delta', async () => {
  const h = harness([row('unchanged'), row('changed')]);
  h.state.rev.queue = 1;
  const done = h.load({ rev: 2, delta: { base: 1, changed: ['changed'], removed: [] } });
  h.load({ rev: 2 });
  h.load({ rev: 3, delta: { base: 2, changed: ['changed'], removed: [] } });
  h.requests[0].resolve([row('changed')]);
  await flush();
  assert.equal(h.requests[1].url, '/api/queue/page');
  h.requests[1].resolve({ rows: [row('unchanged', 3), row('changed')], next: null });
  await done;
  assert.equal(h.state.queue.find(r => r.id === 'unchanged').priority, 3);
  assert.equal(h.state.rev.queue, 3);
});

test('owner filter discovers legacy closed goals through later pages, normal list stays paginated', async () => {
  const h = harness(null, 'talks');
  const first = { id: 'new', chat_status: { kind: 'idle' } };
  const old = { id: 'old', chat_status: { kind: 'needs_owner' } };
  const initial = h.load();
  h.requests[0].resolve({ rows: [first], next: 'history' });
  await initial;
  assert.equal(h.requests.length, 1);
  h.state.talksOwnerOnly = true;
  const filtered = h.load();
  h.requests[1].resolve({ rows: [first], next: 'history' });
  await flush();
  assert.equal(h.requests[2].url, '/api/talks/page?before=history');
  h.requests[2].resolve({ rows: [old], next: null });
  await filtered;
  assert.deepEqual(Array.from(h.state.talks.filter(r => r.chat_status.kind === 'needs_owner'), r => r.id), ['old']);
});

test('chat status links round-trip through the actual router', () => {
  const href = source.slice(source.indexOf('function talkStateHref('), source.indexOf('function renderTalkState('));
  const router = source.slice(source.indexOf('function parseRoute()'), source.indexOf('/* ---- two-pane layout'));
  const context = { location: { hash: '' }, URLSearchParams, RUN_TABS: ['overview'] };
  const api = runInNewContext(`${href}\n${router}\n({talkStateHref, parseRoute})`, context);
  for (const kind of ['task', 'run']) {
    const id = 'id /?#';
    context.location.hash = api.talkStateHref({ kind, id });
    assert.equal(api.parseRoute().name, kind);
    assert.equal(api.parseRoute().id, id);
  }
});

for (const existing of [null, [], [row('saved')]]) {
  test(`older-entry notice uses the snapshot from before paging (${JSON.stringify(existing)})`, async () => {
    const h = harness(existing);
    const done = h.load();
    h.requests[0].resolve({ rows: [row('first')], next: 'older' });
    await flush();
    assert.equal(h.notices.at(-1)[4].pending, true);
    assert.equal(h.notices.at(-1)[4].snapshot, existing !== null);
    h.requests[1].resolve({ rows: [], next: null });
    await done;
  });
}
