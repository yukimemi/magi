// Exercise the idle wording without loading the browser application.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const helper = source.slice(source.indexOf('function daemonIdleWording('), source.indexOf('/* The run the loop is on'));
const idle = runInNewContext(`${helper}\ndaemonIdleWording`);

test('legacy empty queues retain the existing wording only when actually empty', () => {
  assert.equal(idle(null, 0), 'Nothing runnable in the queue');
  assert.equal(idle(undefined, 0), 'Nothing runnable in the queue');
  assert.equal(idle(null, null), 'Queue state unavailable; dispatch diagnostic unavailable');
  assert.equal(idle({ runnable: 0, reasons: [] }, 0), '0 tasks queued, none started: reasons not recorded');
});

test('queued tasks show dispatch counts and known reason wording', () => {
  assert.equal(idle({ runnable: 20, reasons: [
    { reason: 'waiting_on_landing', count: 12 },
    { reason: 'cooling_down', count: 8 },
  ] }, 0), '20 tasks queued, none started: 12 waiting on landing, 8 cooling down');

  for (const [reason, wording] of [
    ['claim_unavailable', 'could not be claimed'],
    ['reread_failed', 'could not be reread'],
    ['no_longer_runnable', 'no longer runnable'],
    ['waiting_on_landing', 'waiting on landing'],
    ['cooling_down', 'cooling down'],
    ['ordinary_capacity', 'waiting for ordinary capacity'],
    ['urgent_capacity', 'waiting for urgent capacity'],
    ['interrupt_gate', 'waiting on interrupt gate'],
  ]) {
    assert.equal(idle({ runnable: 1, reasons: [{ reason, count: 1 }] }, null),
      `1 task queued, none started: 1 ${wording}`);
  }
});

test('positive dispatch counts without reasons explicitly report the missing reasons', () => {
  assert.equal(idle({ runnable: 3, reasons: [] }, 0),
    '3 tasks queued, none started: reasons not recorded');
});

test('unknown reason keys remain literal text, including inherited property names', () => {
  for (const reason of ['future_reason', '__proto__', 'constructor', '<img src=x onerror=alert(1)>']) {
    assert.equal(idle({ runnable: 2, reasons: [{ reason, count: 2 }] }, 2),
      `2 tasks queued, none started: 2 ${reason}`);
  }
});

test('runnable tasks without diagnostics are not described as an empty queue', () => {
  assert.equal(idle(null, 20), '20 tasks queued, none started: dispatch diagnostic unavailable');
  assert.equal(idle(undefined, 1), '1 task queued, none started: dispatch diagnostic unavailable');
});

test('fallback queue counts include runnable retries and parked work', () => {
  const start = source.indexOf('function runnableTasks(');
  const end = source.indexOf('/* Plain text without its final stop', start);
  const state = { queue: null };
  const count = runInNewContext(`${source.slice(start, end)}\nrunnableTasks`, { state });
  assert.equal(count(), null);
  state.queue = ['queued', 'failed', 'parked', 'running', 'held', 'blocked', 'done']
    .map(status => ({ status }));
  assert.equal(count(), 3);
  assert.equal(idle(null, count()), '3 tasks queued, none started: dispatch diagnostic unavailable');
  state.queue = [{ status_str: 'parked', status: 'held' }];
  assert.equal(count(), 1);
});
