// Exercise the shipped editor's retry behavior without browser dependencies.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const editor = source.slice(source.indexOf('let editingTaskId = null;'), source.indexOf('/* ---- theme'));
function harness() {
  const nodes = new Map();
  const $ = (id) => {
    if (!nodes.has(id)) nodes.set(id, { value: '', validity: { valid: true },
      showModal() { this.open = true; }, close() { this.open = false; }, focus() {} });
    return nodes.get(id);
  };
  const calls = [];
  let rejectAttempts = true;
  const context = { $, setText: (n, text) => { n.textContent = text; },
    show() {}, setAttr() {}, requestAnimationFrame() {}, ok() {}, announce() {},
    shortId: (id) => id, refreshTask: async () => {}, fail() {},
    API: { editTask: () => 'text', implementers: () => 'seats', attemptLimit: () => 'limit' },
    postJson: async (url, body) => {
      calls.push({ url, body });
      assert.equal($('task-edit-attempt-limit').disabled, true);
      if (url === 'limit' && rejectAttempts) throw new Error('claimed');
    },
  };
  const api = runInNewContext(`${editor}\n({ openTaskEdit, saveTaskEdit })`, context);
  return { $, calls, api, allow: () => { rejectAttempts = false; } };
}

test('attempt limit keeps input after partial success and retries only unsaved fields', async () => {
  const h = harness();
  h.api.openTaskEdit({ id: 'task', title: 'old', instruction: 'keep', attempts: 2, attempt_limit: 3 });
  assert.equal(h.$('task-edit-attempt-limit').value, '3');
  h.$('task-edit-title').value = 'new';
  h.$('task-edit-attempt-limit').value = '4';
  await h.api.saveTaskEdit();
  assert.deepEqual(h.calls.map(c => c.url), ['text', 'limit']);
  assert.equal(h.$('task-edit-attempt-limit').value, '4');
  assert.equal(h.$('task-edit-attempt-limit').disabled, false);
  assert.match(h.$('task-edit-error').textContent, /Title and instruction saved/);
  h.allow();
  await h.api.saveTaskEdit();
  assert.deepEqual(h.calls.map(c => c.url), ['text', 'limit', 'limit']);
  assert.equal(h.calls[2].body.attempt_limit, 4);
  assert.equal(h.$('task-edit-sheet').open, false);
});

test('blank restores inheritance and unchanged limits are not posted', async () => {
  const h = harness();
  h.allow();
  const task = { id: 'task', title: 'old', instruction: 'keep', attempt_limit: 3 };
  h.api.openTaskEdit(task);
  await h.api.saveTaskEdit();
  assert.equal(h.calls.length, 0);
  h.api.openTaskEdit(task);
  h.$('task-edit-attempt-limit').value = '';
  await h.api.saveTaskEdit();
  assert.equal(h.calls[0].body.attempt_limit, null);
});

test('invalid limits prevent all writes', async () => {
  for (const value of ['0', '-1', '1.5', '9007199254740992']) {
    const h = harness();
    h.api.openTaskEdit({ id: 'task', title: 'old', instruction: 'keep' });
    h.$('task-edit-title').value = 'new';
    h.$('task-edit-attempt-limit').value = value;
    await h.api.saveTaskEdit();
    assert.equal(h.calls.length, 0);
    assert.match(h.$('task-edit-error').textContent, /positive whole number/);
  }
});
