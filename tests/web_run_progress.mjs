// Controlled observations exercise transport ordering independently of the DOM.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const observationCode = source.slice(source.indexOf('let progressFlight ='), source.indexOf('function progressRow()'));
function harness() {
  const requests = [], renders = [], loads = [];
  const document = { hidden: false };
  const run = { id: 'a', updated_at: 'saved', progress: { activity: 'working' } };
  const state = { route: { name: 'run' }, detail: { run, id: 'a', evidencePending: false } };
  const generations = new Map([['a', 1]]);
  const api = runInNewContext(`${observationCode}\n({observeRunProgress, progressElapsed, progressSnapshot})`, {
    Date, AbortController, setTimeout, clearTimeout, document, state, runDetailRequests: generations, API: { run: id => `run/${id}` },
    getJson: url => new Promise((resolve, reject) => requests.push({url, resolve, reject})),
    renderRunProgress: r => renders.push(r), renderActive() {}, tickRunProgress() {}, loadRun: (...args) => loads.push(args),
  });
  return { api, state, document, generations, requests, renders, loads };
}
test('observation coalesces, throttles and never replaces guarded action evidence', async () => {
  const h = harness(), original = h.state.detail.run;
  const done = h.api.observeRunProgress(true);
  await h.api.observeRunProgress(true);
  assert.equal(h.requests.length, 1);
  h.requests[0].resolve({id: 'a', updated_at: 'saved', live: 'dead', progress: {activity: 'Driver exited'}});
  await done;
  assert.equal(h.state.detail.run, original);
  assert.equal(h.api.progressSnapshot(original).live, 'dead');
  assert.equal(h.state.detail.evidencePending, false);
  await h.api.observeRunProgress();
  assert.equal(h.requests.length, 1);
});
test('wake/reconnect forces reconciliation and a changed record uses the normal detail loader', async () => {
  const h = harness();
  const done = h.api.observeRunProgress(true);
  h.requests[0].resolve({id: 'a', updated_at: 'new'});
  await done;
  assert.equal(h.loads.length, 1);
  assert.equal(h.state.detail.progressObservation, undefined);
  assert.equal(h.requests[0].url, 'run/a/progress');
  const again = h.api.observeRunProgress(true);
  h.requests[1].reject(new Error('offline'));
  await again;
  assert.equal(h.state.detail.progressObservedAt, undefined);
});
for (const invalidate of ['navigate', 'generation', 'newer run']) test(`old observation cannot overwrite ${invalidate}`, async () => {
  const h = harness();
  const done = h.api.observeRunProgress(true);
  if (invalidate === 'navigate') h.state.detail = {run: h.state.detail.run};
  if (invalidate === 'generation') h.generations.set('a', 2);
  if (invalidate === 'newer run') h.state.detail.run = {...h.state.detail.run};
  h.requests[0].resolve({id: 'a', updated_at: 'saved', live: 'dead'});
  await done;
  assert.equal(h.renders.length, 0);
  assert.equal(h.state.detail.progressObservation, undefined);
});
test('hidden, terminal and off-route pages do not observe', async () => {
  const h = harness();
  h.document.hidden = true;
  await h.api.observeRunProgress(true);
  h.document.hidden = false;
  h.state.detail.run.done = true;
  await h.api.observeRunProgress(true);
  h.state.detail.run.done = false;
  h.state.route.name = 'queue';
  await h.api.observeRunProgress(true);
  assert.equal(h.requests.length, 0);
});
test('elapsed formatting is explicit and never produces negative ages or an ETA', () => {
  const {api} = harness();
  assert.equal(api.progressElapsed('2026-01-01T00:00:00Z', Date.parse('2026-01-01T00:01:15Z')), '1m 15s');
  assert.equal(api.progressElapsed('bad'), 'unknown');
  assert.equal(api.progressElapsed('2099-01-01', 0), '0s');
});
const revisionsCode = source.slice(source.indexOf('async function applyRevisions_'), source.indexOf('/* ---- live stream'));
for (const [label, changes, expected] of [
  ['unrelated run', {runs_delta: {base: 1, changed: ['b'], removed: []}}, 0],
  ['current run', {runs_delta: {base: 1, changed: ['a'], removed: []}}, 1],
  ['expired delta', {runs_delta: {base: 0, changed: ['b'], removed: []}}, 1],
  ['reconnect snapshot', {}, 1],
  ['queue and question evidence', {queue_rev: 2, questions_rev: 2}, 1],
]) test(`revision delivery reconciles ${label}`, async () => {
  const reads = [];
  const state = {rev: {runs: 1, queue: 1, questions: 1, talks: 1, notifications: 1}, stats: null,
    route: {name: 'run'}, detail: {id: 'a'}, taskDetail: {}, talkDetail: {}};
  const api = runInNewContext(`${revisionsCode}\n({applyRevisions_})`, {
    state, saidAt: 0, refreshReferences() {}, announce() {},
    loadRuns() {}, loadQueue() {}, loadQuestions() {}, loadNotifications() {},
    loadRun: id => reads.push(id),
  });
  await api.applyRevisions_({runs_rev: 2, queue_rev: 1, questions_rev: 1, talks_rev: 1, notifications_rev: 1, ...changes});
  assert.equal(reads.length, expected);
});
test('stream reconnection observes liveness and failures keep a single fallback refresh', () => {
  const callbacks = {}, observations = [], timers = [], refreshes = [];
  const state = {};
  const streamCode = source.slice(source.indexOf('function subscribe()'), source.indexOf('/* ---- routing'));
  const api = runInNewContext(`${streamCode}\n({subscribe})`, {
    state, API: {events: 'events'}, fallbackTimer: null,
    EventSource: class { addEventListener(name, fn) { callbacks[name] = fn; } },
    observeRunProgress: force => observations.push(force), ok() {},
    applyRevisions_() {}, loadHealth: options => refreshes.push(options),
    setTimeout: callback => { timers.push(callback); return timers.length; },
  });
  api.subscribe();
  callbacks.open();
  assert.equal(state.streamOpen, true);
  assert.deepEqual(observations, [true]);
  callbacks.error(); callbacks.error();
  assert.equal(state.streamOpen, false);
  assert.equal(timers.length, 1);
  timers[0]();
  assert.equal(refreshes.length, 1);
  callbacks.open();
  assert.deepEqual(observations, [true, true]);
});

const presentationCode = source.slice(source.indexOf('function progressOverall('), source.indexOf('function progressRow()'));
const presentation = runInNewContext(`${presentationCode}\n({progressOverall, progressImplementers})`, {
  displayedRunStatus: run => run.badge_status || run.status,
});
test('overall glyphs distinguish waits, holds, exits, failures and completion', () => {
  for (const [run, live, expected] of [
    [{status: 'implementing'}, 'live', ['active', '▸']],
    [{status: 'implementing', waiting: true}, 'live', ['waiting', 'Ⅱ']],
    [{status: 'blocked', done: true}, 'dead', ['held', '■']],
    [{status: 'failed', done: true}, 'dead', ['failed', '×']],
    [{status: 'merged', done: true}, 'dead', ['complete', '✓']],
    [{status: 'implementing'}, 'dead', ['exited', '⊘']],
    [{status: 'implementing'}, 'unknown', ['unknown', '?']],
  ]) assert.deepEqual(Array.from(presentation.progressOverall(run, live)), expected);
});
test('implementation rows retain parallel results and never invent success or live activity', () => {
  const run = {candidates: [
    {label: 'A', duration_ms: 12000}, {label: 'B', duration_ms: 5000, failed: 'exit'},
    {label: 'C', duration_ms: 0},
  ], active: {'implement-D': {node: 'implement', started_at: '2026-01-01'}}};
  const rows = presentation.progressImplementers(run, {state: 'current'}, 'live');
  assert.deepEqual(Array.from(rows, r => r.state), ['complete', 'failed', 'unrecorded', 'running']);
  assert.equal(rows[0].duration, 12000);
  assert.equal(rows[3].start, '2026-01-01');
  assert.equal(presentation.progressImplementers(run, {state: 'paused'}, 'dead')[3].state, 'paused');
  assert.equal(presentation.progressImplementers(run, {state: 'unconfirmed'}, 'unknown')[3].state, 'paused');
});
