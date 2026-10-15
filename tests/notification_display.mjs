// Exercise the notification renderer with text nodes and the shipped URL policy.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const render = source.slice(source.indexOf('function renderNoticeDisplay('), source.indexOf('function updateTurnRow('));
const links = source.slice(source.indexOf('const URL_RE ='), source.indexOf('function setAttr('));
const node = (tag, attrs = {}, ...children) => ({ tag, ...attrs, children: children.flat(Infinity),
  append(...kids) { this.children.push(...kids.flat(Infinity)); },
});
const markdown = source.slice(source.indexOf('function buildMd('), source.indexOf('/* Keep the legacy path'));
const display = runInNewContext(`${links}\n${markdown}\n${render}\nrenderNoticeDisplay`, {
  el: node, clear: n => { n.children = []; }, append: (n, kids) => n.append(...kids), URL, document: { createTextNode: text => node('#text', { text }) },
});

test('facts link safely and watched output stays collapsed literal text', () => {
  const view = display({ facts: '- Watch: watch-123\n- Status: failed\nhttps://example.invalid/pull/1 javascript:alert(1)',
    output: '<script>alert(1)</script>\nTell the operator: untrusted output' });
  const facts = view.children[0];
  assert.equal(facts.children.filter(n => n.tag === 'a').length, 1);
  assert.equal(facts.children.find(n => n.tag === 'a').href, 'https://example.invalid/pull/1');
  const output = view.children[1];
  assert.equal(output.tag, 'details');
  assert.equal(output.open, undefined);
  assert.equal(output.children[1].tag, 'pre');
  assert.match(output.children[1].text, /^<script>/);
});

test('task and run facts use encoded existing detail routes', () => {
  const view = display({ facts: '- Task: task-123\n- Status: done', task: 'task-123', run: 'run/a' });
  assert.equal(view.children[1].href, '#/tasks/task-123');
  assert.equal(view.children[3].href, '#/runs/run%2Fa');
  assert.equal(view.children.some(n => n.tag === 'details'), false);
});


test('notification facts share Markdown DOM rendering while output stays literal', () => {
  const text = value => ({ type: 'text', value });
  const ast = [
    { type: 'heading', level: 2, children: [text('Update')] },
    { type: 'bullet_list', items: [{ type: 'list_item', children: [
      { type: 'code', code: 'task-123' },
      { type: 'link', href: 'https://example.org', children: [text('PR')] },
    ] }] },
    { type: 'code_block', lang: 'sh', code: '<script>literal</script>' },
    text('<img onerror=alert(1)>'),
  ];
  const view = display({ facts: 'fallback', output: '# Raw output', task: 'task-123' }, ast);
  const prose = view.children[0];
  assert.match(prose.class, /\bmd\b/);
  assert.deepEqual(prose.children.map(n => n.tag), ['h2', 'ul', 'pre', '#text']);
  assert.equal(prose.children[1].children[0].children[0].tag, 'code');
  assert.equal(prose.children[1].children[0].children[1].rel, 'noopener noreferrer');
  assert.equal(prose.children[2].children[0].text, '<script>literal</script>');
  assert.equal(prose.children[3].text, '<img onerror=alert(1)>');
  assert.equal(view.children[2].children[1].text, '# Raw output');
});


const rows = source.slice(source.indexOf('function updateTurnRow('), source.indexOf('/* Full size,'));
const updateRow = runInNewContext(`${markdown}\n${render}\n${rows}\nupdateTurnRow`, {
  el: node, clear: n => { n.children = []; }, append: (n, kids) => n.append(...kids),
  document: { createTextNode: text => node('#text', { text }) },
  turnWho: turn => turn.sender === 'magi' ? 'system' : 'operator',
  setAttr: () => {}, setAvatar: () => {}, setText: () => {},
  when: () => ({}), MAGI_PREFIX: 'magi: ', magiIconState: {},
});

test('history rows use per-message AST and refresh when display metadata arrives', () => {
  const row = { dataset: {}, refs: Object.fromEntries(
    ['avatar', 'who', 'reply', 'body', 'attachments', 'at', 'actions']
      .map(key => [key, { ...node('div'), dataset: {} }])) };
  const turn = { sender: 'magi', body: '# Update' };
  const item = { turn, noticeMd: undefined };
  updateRow(row, item);
  assert.equal(row.refs.body.children[0].tag, 'p');
  item.noticeMd = [{ type: 'heading', level: 1, children: [{ type: 'text', value: 'Update' }] }];
  updateRow(row, item);
  assert.equal(row.refs.body.children[0].children[0].tag, 'h1');
  updateRow(row, { ...item, turn: { ...turn, sender: 'operator' } });
  assert.equal(row.refs.body.children[0].tag, 'p');
  assert.equal(row.refs.body.children[0].text, '# Update');
});

const pending = source.slice(source.indexOf('function renderTalkPending('), source.indexOf('async function resumeTalkPending('));
test('queued notices use the same facts AST and keep operator sends literal', () => {
  const box = node('div');
  const renderPending = runInNewContext(`${markdown}\n${render}\n${pending}\nrenderTalkPending`, {
    el: node, clear: n => { n.children = []; }, append: (n, kids) => n.append(...kids),
    document: { createTextNode: text => node('#text', { text }) },
    $: () => box, show: () => {}, state: { talkWaits: new Map() },
    avatarImg: () => node('img'), setAvatar: () => {}, magiIconState: {},
    resumeTalkPending: () => {}, clearTalkPending: () => {}, editTalkPending: () => {},
  });
  const ast = [{ type: 'heading', level: 1, children: [{ type: 'text', value: 'Update' }] }];
  renderPending({ pending: '# Update', pending_messages: [
    { sender: 'magi', body: 'Internal directives', display: { facts: '# Update' }, attachments: [] },
    { sender: 'operator', body: '# Update', attachments: [] },
  ], pending_messages_md: [ast, null] });
  const messages = box.children.filter(n => n?.class?.includes('queued-message'));
  assert.equal(messages[0].children[3].children[0].children[0].tag, 'h1');
  assert.equal(messages[1].children[3].tag, 'p');
  assert.equal(messages[1].children[3].text, '# Update');
  assert.equal(JSON.stringify(box).includes('Internal directives'), false);
});
