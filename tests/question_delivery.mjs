// Exercise the shipped question controls and rejection handling without a browser.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const availability = source.slice(source.indexOf('  const delivery = question.text_delivery;'), source.indexOf('  setText(r.waitingNote, waitingText);'));
const controls = source.slice(source.indexOf('  r.sayText.disabled = waitingOnAgent || textUnavailable;'), source.indexOf('  /* The detail is immutable'));
const decisionControls = source.slice(source.indexOf('  r.text.disabled = choicesLocked;'), source.indexOf('  for (const btn of r.choices.querySelectorAll'));
const update = runInNewContext(`(r, question) => {
  const waitingOnAgent = question.waiting_on_agent === true;
  ${availability}
  ${controls}
  ${decisionControls}
  return { choicesLocked, waitingText };
}`);
const refs = () => ({ sayText: { value: 'close the PR' }, saySend: {}, text: {}, send: {} });

test('unavailable text is disabled while offered decisions remain usable', () => {
  for (const reason of ['deputy_exhausted', 'deputy_disabled', 'daemon_unavailable', 'recipient_unavailable']) {
    const r = refs();
    const state = update(r, { waiting_on_agent: true, thread: [{ who: 'operator' }], text_delivery: { state: 'unavailable', reason, message: 'No recipient' } });
    assert.equal(r.sayText.disabled, true);
    assert.equal(r.saySend.disabled, true);
    assert.equal(r.sayText.value, 'close the PR');
    assert.equal(state.choicesLocked, false);
    assert.equal(r.text.disabled, false);
    assert.equal(r.send.disabled, false);
    assert.match(state.waitingText, /recorded and undelivered/);
  }
});

test('live and recoverable recipients accept text and show the shared explanation', () => {
  for (const state of ['live', 'waiting']) {
    const r = refs();
    const view = update(r, { text_delivery: { state, message: 'Starting automatically' } });
    assert.equal(r.saySend.disabled, false);
    assert.equal(view.waitingText, 'Starting automatically');
  }
});

test('a delivery race preserves the draft and does not announce a settled question', async () => {
  const r = { ...refs(), error: {} };
  let refreshes = 0;
  const notices = [];
  const say = runInNewContext(source.slice(source.indexOf('async function sayToQuestion('), source.indexOf('/* Hand the question to the chat')) + '\nsayToQuestion', {
    API: { questionSay: id => id },
    postJson: async () => { throw Object.assign(new Error('自由文を送信できません。'), { status: 422 }); },
    loadQuestions: async () => { refreshes++; },
    setText: (node, text) => { node.textContent = text; },
    show: (node, visible) => { node.hidden = !visible; },
    announce: text => notices.push(text),
  });
  await say('q', r.sayText.value, { refs: r });
  assert.equal(r.sayText.value, 'close the PR');
  assert.equal(r.saySend.disabled, true);
  assert.equal(refreshes, 1);
  assert.deepEqual(notices, []);
  assert.match(r.error.textContent, /送信できません/);
});
