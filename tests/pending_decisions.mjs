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
