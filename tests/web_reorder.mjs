import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const logic = source.slice(source.indexOf('const priorityOrder ='), source.indexOf('async function moveListRow('));
function harness(order = {}) {
  return runInNewContext(`${logic}\n({priorityEdits, queueMoveEdits, queueOrder, orderedRuns, rememberRunMove})`, {
    runOrder: order, queueSection: task => task.status || 'upnext',
  });
}
test('every insertion preserves exact dispatch order at ties and integer boundaries', () => {
  const { priorityEdits, queueOrder } = harness();
  for (const priorities of [[0, 0, 0, 0], [9, 3, 0, -5], [2147483647, 2147483647, 2147483646, -2147483648], [-2147483647, -2147483648, -2147483648, -2147483648]]) {
    const tasks = priorities.map((priority, i) => ({ id: String(i), priority }));
    for (let from = 0; from < tasks.length; from++) for (let to = 0; to < tasks.length; to++) {
      if (from === to) continue;
      const desired = tasks.map(t => t.id);
      const [id] = desired.splice(from, 1);
      desired.splice(to, 0, id);
      const edits = priorityEdits(tasks, from, to);
      const saved = tasks.map(t => ({ ...t, priority: edits.find(e => e.id === t.id)?.priority ?? t.priority }));
      assert.ok(saved.every(t => Number.isInteger(t.priority) && t.priority >= -2147483648 && t.priority <= 2147483647));
      assert.deepEqual(saved.sort(queueOrder).map(t => t.id), desired);
    }
  }
});
test('a free integer gap only writes the moved task', () => {
  const { priorityEdits } = harness();
  const edits = priorityEdits([{ id: 'a', priority: 10 }, { id: 'b', priority: 5 }, { id: 'c', priority: 0 }], 2, 1);
  assert.deepEqual(JSON.parse(JSON.stringify(edits)), [{ id: 'c', priority: 6 }]);
});
test('an ID tie-break allows a single write without raising its neighbours', () => {
  const { priorityEdits } = harness();
  const edits = priorityEdits([{ id: 'b', priority: 1 }, { id: 'c', priority: 0 }, { id: 'a', priority: -1 }], 2, 1);
  assert.deepEqual(JSON.parse(JSON.stringify(edits)), [{ id: 'a', priority: 0 }]);
});
test('drop planning uses fresh priorities and surviving neighbour IDs', () => {
  const { queueMoveEdits, queueOrder } = harness();
  const tasks = [{ id: 'a', priority: 100 }, { id: 'new', priority: 90 }, { id: 'b', priority: 80 }, { id: 'c', priority: 0 }];
  const edits = queueMoveEdits(tasks, 'c', 'a', 'b', 'upnext');
  const saved = tasks.map(task => ({ ...task, priority: edits.find(edit => edit.id === task.id)?.priority ?? task.priority }));
  assert.deepEqual(saved.sort(queueOrder).map(task => task.id), ['a', 'c', 'new', 'b']);
  assert.throws(() => queueMoveEdits(tasks, 'c', 'gone', 'missing', 'upnext'), /list changed/);
  assert.throws(() => queueMoveEdits(tasks.map(task => task.id === 'c' ? { ...task, status: 'running' } : task), 'c', 'a', 'b', 'upnext'), /no longer movable/);
});
test('filtered and unloaded runs keep their remembered slots after visible moves', () => {
  const order = { flight: ['a', 'hidden', 'b', 'unloaded'] };
  const { rememberRunMove, orderedRuns } = harness(order);
  rememberRunMove('flight', ['b', 'a']);
  assert.deepEqual(Array.from(order.flight), ['b', 'hidden', 'a', 'unloaded']);
  assert.deepEqual(Array.from(orderedRuns('flight', ['a', 'hidden', 'b', 'unloaded'].map(id => ({ id }))), row => row.id), ['b', 'hidden', 'a', 'unloaded']);
});
test('upgrade resumes stay ahead of priority and order by dispatch time', () => {
  const { queueOrder } = harness();
  const tasks = [{ id: 'ordinary', priority: 2147483647 }, { id: 'later', priority: 0, upgrade_resume_at: '2026-01-02T00:00:00Z' }, { id: 'earlier', priority: -10, upgrade_resume_at: '2026-01-01T00:00:00Z' }];
  assert.deepEqual(tasks.sort(queueOrder).map(t => t.id), ['earlier', 'later', 'ordinary']);
});
test('browser order ignores absent IDs and puts new runs first in default order', () => {
  const { orderedRuns } = harness({ active: ['gone', 'b', 'a'], done: 'corrupt' });
  const rows = ['new2', 'a', 'new1', 'b'].map(id => ({ id }));
  assert.deepEqual(Array.from(orderedRuns('active', rows), r => r.id), ['new2', 'new1', 'b', 'a']);
  assert.deepEqual(Array.from(orderedRuns('done', rows), r => r.id), rows.map(r => r.id));
});
