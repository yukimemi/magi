// Exercise the shipped question controls without browser dependencies.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const pending = source.slice(source.indexOf('const needsOwnerQuestions ='),
  source.indexOf('\n', source.indexOf('const needsOwnerQuestions =')));
const note = source.slice(source.indexOf('  const holder = open ? question.holder'),
  source.indexOf('  r.saySend.onclick ='));
const controls = source.slice(source.indexOf('  for (const btn of r.choices.querySelectorAll'),
  source.indexOf('  const given = question.answer'));
const render = runInNewContext(`(question) => {
  const open = question.status === 'open';
  const waitingOnAgent = open && question.waiting_on_agent === true;
  const merge = question.node === 'land-approval';
  const choice = {};
  const stakes = {};
  const r = {
    waitingNote: {},
    choices: { querySelectorAll: () => [choice] },
    stakes: { querySelectorAll: () => [stakes] },
  };
  ${note}
  ${controls}
  const openQuestions = () => open ? [question] : [];
  ${pending}
  return { choice, stakes, note: r.waitingNote.text, pending: needsOwnerQuestions().length };
}`, {
  setText: (node, text) => { node.text = text; },
  show: (node, visible) => { node.visible = visible; },
});

const question = {
  status: 'open', node: 'conduct', deputy: { starts: 3 },
  holder: 'nobody', deputies_enabled: true, deputy_exhausted: true,
  waiting_on_agent: true, choices: ['keep', 'discard'],
};

test('exhausted follow-ups keep choices usable and need the owner despite an unread say', () => {
  const view = render(question);
  assert.equal(view.choice.disabled, false);
  assert.equal(view.pending, 1);
  assert.match(view.note, /will not restart/);
  assert.match(view.note, /Answer with one of the choices/);
});

test('the active final attempt stays locked and waits for the agent', () => {
  const view = render({ ...question, holder: 'deputy', deputy_exhausted: false });
  assert.equal(view.choice.disabled, true);
  assert.equal(view.pending, 0);
  assert.match(view.note, /reading your message/);
  assert.doesNotMatch(view.note, /will not restart/);
});

test('exhausted merge approvals also unlock the confirmation controls', () => {
  const view = render({ ...question, node: 'land-approval', choices: ['merge', 'hold'] });
  assert.equal(view.stakes.disabled, false);
  assert.equal(view.pending, 1);
});

test('settled questions are absent from pending questions', () => {
  assert.equal(render({ ...question, status: 'answered', deputy_exhausted: false }).pending, 0);
});
