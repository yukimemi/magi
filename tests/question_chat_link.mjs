// Exercise question chat controls and the shipped router without a browser.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const controls = source.slice(source.indexOf('  const hasChat ='), source.indexOf('  r.sayText.disabled ='));
const router = source.slice(source.indexOf('function parseRoute()'), source.indexOf('/* ---- two-pane layout'));
const consults = [];
const context = {
  show: (node, visible) => { node.hidden = !visible; },
  setText: (node, text) => { node.textContent = text; },
  setAttr: (node, key, value) => { if (value === null) delete node[key]; else node[key] = value; },
  consultChat: (id, row) => consults.push({ id, row }),
  location: { hash: '' },
};
const update = runInNewContext(`(r, question) => {
  const row = { refs: r }, open = question.status === 'open';
  ${controls}
}`, context);
const parseRoute = runInNewContext(`${router}\nparseRoute`, context);
const refs = () => ({ consultBox: {}, consultBtn: {}, chatLink: {} });
const question = { id: 'q', status: 'open', origin_chat: 'chat /?#日本語', can_consult_chat: true };

test('consultation and navigation remain separate before and after handover', () => {
  const r = refs();
  update(r, question);
  assert.equal(r.consultBtn.hidden, false);
  assert.equal(r.consultBtn.disabled, false);
  r.consultBtn.onclick();
  assert.equal(consults.at(-1).id, question.id);
  update(r, { ...question, consult: { talk: question.origin_chat } });
  assert.equal(r.consultBtn.disabled, true);
  assert.equal(r.consultBtn.textContent, 'Asked the chat agent');
  assert.equal(r.chatLink.hidden, false);
  context.location.hash = r.chatLink.href;
  assert.equal(parseRoute().name, 'talk');
  assert.equal(parseRoute().id, question.origin_chat);
});

test('settled and ineligible questions still link to their existing chat', () => {
  for (const status of ['open', 'answered', 'abandoned']) {
    const r = refs();
    update(r, { ...question, status, can_consult_chat: false });
    assert.equal(r.consultBtn.hidden, true);
    assert.equal(r.consultBox.hidden, false);
    assert.equal(r.chatLink.hidden, false);
  }
});

test('closed chats are labelled without reopening them for navigation', () => {
  const r = refs();
  update(r, { ...question, origin_chat_closed: true });
  assert.equal(r.chatLink.textContent, 'Open chat (closed)');
  assert.equal(r.consultBtn.textContent, 'Reopen chat and ask');
});

test('updates remove a stale navigation target when the association disappears', () => {
  const r = refs();
  for (const origin_chat of [null, '', undefined]) {
    update(r, question);
    assert.ok(r.chatLink.href);
    update(r, { ...question, origin_chat });
    assert.equal(r.consultBox.hidden, true);
    assert.equal(r.chatLink.hidden, true);
    assert.equal(r.chatLink.href, undefined);
  }
});
