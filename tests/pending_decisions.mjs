import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const router = source.slice(source.indexOf('function parseRoute()'), source.indexOf('/* ---- two-pane layout'));
const links = source.slice(source.indexOf('function decisionChatLink('), source.indexOf('function decisionCard('));
const context = { location: { hash: '' } };
const parse = runInNewContext(`${router}\nparseRoute`, context);
const link = runInNewContext(`${links}\ndecisionChatLink`, context);
test('dedicated pending list and stable inquiry link preserve encoded identities', () => {
  context.location.hash = '#/decisions';
  assert.equal(parse().name, 'decisions');
  const inquiry = { id: 'ask 日本語/?#', chat: 'chat /?#' };
  context.location.hash = link(inquiry);
  assert.equal(parse().name, 'talk');
  assert.equal(parse().id, inquiry.chat);
  assert.equal(parse().inquiry, inquiry.id);
  assert.equal(link({ id: 'pending', chat: null }), null);
});
test('phone dock and rail point to pending decisions separately from notifications', () => {
  const html = readFileSync(new URL('../assets/ui/index.html', import.meta.url), 'utf8');
  assert.equal((html.match(/href="#\/decisions" data-nav="decisions"/g) || []).length, 2);
  assert.ok(html.includes('id="view-decisions"'));
  assert.ok(html.includes('id="decisions-history"'));
  assert.ok(html.includes('id="talk-inquiry"'));
  assert.ok(html.includes('href="#/notifications"'));
});

test('partial lists keep readable decisions visible and report unreadable records', async () => {
  const section = source.slice(source.indexOf('const decisionState ='), source.indexOf('async function loadInquiry('));
  const nodes = new Map();
  const node = id => {
    if (!nodes.has(id)) nodes.set(id, { children: [], replaceChildren(...children) { this.children = children; } });
    return nodes.get(id);
  };
  let response = { items: [{ id: 'pending', task: 'task', run: null, chat: 'chat', decision: {
    status: 'awaiting_owner', reason: '入力待ち', history: [],
  } }], unreadable: 2 };
  const ui = runInNewContext(`${section}\n({ loadDecisions, decisionState })`, {
    API: { decisions: '/api/decisions' }, getJson: async () => response,
    $: node, setText: (target, text) => { target.text = text; },
    show: (target, visible) => { target.visible = visible; },
    el: (tag, attrs, ...children) => ({ tag, ...attrs, children }),
  });
  await ui.loadDecisions();
  assert.equal(ui.decisionState.error, null);
  assert.equal(node('decisions-list').children.length, 1);
  assert.equal(node('decisions-count').text, '1 件の判断待ち');
  assert.equal(node('decisions-error').visible, true);
  assert.match(node('decisions-error').children[0].text, /2 件の記録を読み込めません/);
  response = { items: [], unreadable: 2 };
  await ui.loadDecisions();
  assert.equal(node('decisions-empty').visible, false);
  response = { items: [], unreadable: 0 };
  await ui.loadDecisions();
  assert.equal(node('decisions-error').visible, false);
  assert.equal(node('decisions-empty').visible, true);
});
