// Exercise notification presentation and question navigation without a browser.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const historySource = source.slice(source.indexOf('function noticeCoveredBy('), source.indexOf('function noticeLink('));
const renderSource = source.slice(source.indexOf('function renderNotifications()'), source.indexOf('\n$("notifications-read-all")'));
function render(notice, questions) {
  const list = { children: [], append(node) { this.children.push(node); } };
  function el(tag, attrs, ...children) {
    return { tag, ...attrs, children: children.filter(Boolean), click() { this.clicked = true; } };
  }
  const state = { notices: { items: [notice], unread: 1 }, questions };
  runInNewContext(`${historySource}\n${renderSource}\nrenderNotifications();`, {
    shortId: (id) => id.slice(0, 8), state, el, $: (id) => id === 'notifications-list' ? list : {},
    setText() {}, show() {}, clear(node) { node.children = []; },
    when: () => ({ title: 'now', text: 'now' }), plural: () => '1 unread',
    noticeLink: (link) => link ? el('a', { href: '#/tasks/t1', text: 'Task t1' }) : null,
    linkify(node, text) { node.text = text; return node; },
    API: {},
  });
  return list.children[0];
}
function all(node) { return [node, ...(node.children || []).flatMap(all)]; }
const base = { id: 'n1', message: 'A concern remains. No reply to this notification is needed.', severity: 'warn', link: { kind: 'task', id: 't1' }, last_at: 'now', count: 1 };

test('informational cards have a task link and acknowledgement controls, without an answer prompt', () => {
  const card = render({ ...base, covered_by: 'historical-question', answerable_question: null });
  const nodes = all(card);
  assert.deepEqual(nodes.filter(n => n.tag === 'a').map(n => n.text), ['Task t1']);
  assert.deepEqual(nodes.filter(n => n.tag === 'button').map(n => n.text), ['Mark read', 'Dismiss']);
  assert.ok(nodes.some(n => n.text === 'Notification · no reply here'));
  assert.ok(nodes.some(n => n.text === 'Warning'));
  assert.ok(!nodes.some(n => String(n.text).includes('historical-question')));
});

test('only a live registered question supplies the response link and card tap target', () => {
  const card = render({ ...base, answerable_question: 'q/1' });
  const link = all(card).find(n => n.tag === 'a');
  assert.equal(link.href, '#/questions/q%2F1');
  assert.equal(link.text, 'Open question to respond');
  card.onclick({ target: { closest: () => null } });
  assert.equal(link.clicked, true);
  link.clicked = false;
  card.onclick({ target: { closest: () => ({}) } });
  assert.equal(link.clicked, false, 'acknowledgement controls keep their own actions');
});

test('question navigation focuses the named card even when it has no panel', () => {
  const focusSource = source.slice(source.indexOf('function focusFirstAsk()'), source.indexOf('/* When the standing chat'));
  let focused = false;
  let scrolled = false;
  const card = { dataset: { questionId: 'q1' }, querySelector: () => ({ focus() { focused = true; } }), scrollIntoView() { scrolled = true; } };
  runInNewContext(`${focusSource}\nfocusFirstAsk();`, {
    state: { route: { name: 'questions', id: 'q1' } },
    document: { activeElement: null }, requestAnimationFrame: (f) => f(),
    $: () => ({ querySelectorAll: () => [card] }),
  });
  assert.equal(focused, true);
  assert.equal(scrolled, true);
  assert.match(source, /row\.dataset\.questionId = question\.id/);
  assert.match(source, /notificationsRev !== state\.rev\.notifications \|\| questionsRev !== state\.rev\.questions/);
});

test('an expired question link never focuses an unrelated open question', () => {
  const focusSource = source.slice(source.indexOf('function focusFirstAsk()'), source.indexOf('/* When the standing chat'));
  runInNewContext(`${focusSource}\nfocusFirstAsk();`, {
    state: { route: { name: 'questions', id: 'gone' } },
    document: { activeElement: null }, requestAnimationFrame: (f) => f(),
    $: () => ({ querySelectorAll: () => [], querySelector() { assert.fail('must not select an unrelated question'); } }),
  });
});

test('resolved question history remains informational', () => {
  const card = render({ ...base, covered_by: 'q1', answerable_question: null }, [{ id: 'q1', status: 'answered' }]);
  const nodes = all(card);
  assert.ok(nodes.some(n => n.text === 'Question q1 (resolved)'));
  assert.deepEqual(nodes.filter(n => n.tag === 'a').map(n => n.text), ['Task t1']);
});

test('stale client question state never invents a response action', () => {
  const card = render({ ...base, covered_by: 'q1', answerable_question: null }, [{ id: 'q1', status: 'open' }]);
  const nodes = all(card);
  assert.ok(nodes.some(n => n.text === 'Associated question q1'));
  assert.deepEqual(nodes.filter(n => n.tag === 'a').map(n => n.text), ['Task t1']);
});
