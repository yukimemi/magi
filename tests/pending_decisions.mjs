// Exercise the shipped read-only decision renderer without dependencies.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

class Node {
  constructor(tag = 'div') { this.tagName = tag.toUpperCase(); this.children = []; this.dataset = {}; this.attrs = {}; this.content = ''; }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; this.content = ''; }
  set textContent(text) { this.content = text; this.children = []; }
  get textContent() { return this.content + this.children.map(n => typeof n === 'string' ? n : n.textContent).join(''); }
  setAttribute(key, value) { this.attrs[key] = value; if (key === 'data-decision-id') this.dataset.decisionId = value; }
  getAttribute(key) { return this.attrs[key]; }
  addEventListener() {}
  contains() { return false; }
  querySelector() { return this.children.find(n => n.tagName === 'SUMMARY'); }
  querySelectorAll() { return []; }
}
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const html = readFileSync(new URL('../assets/ui/index.html', import.meta.url), 'utf8');
const section = source.slice(source.indexOf('/* ---- durable pending decisions'), source.indexOf('async function loadNotifications()'));
function fixture(getJson = async () => { throw new Error('offline'); }) {
  const nodes = new Map();
  const get = id => { if (!nodes.has(id)) nodes.set(id, new Node()); return nodes.get(id); };
  get('decisions-history').append(new Node('summary'));
  const api = runInNewContext(`${source.slice(source.indexOf('function el('), source.indexOf('/* Where a task'))}
    ${section}
    ({ decisionCard, decisionGuidance, renderDecisions, loadDecisions, loadInquiry, decisionState })`, {
    document: { createElement: tag => new Node(tag), activeElement: null },
    append: (parent, kids) => parent.append(...kids.flat(4).filter(Boolean)),
    $: get, show: (node, visible) => { node.hidden = !visible; },
    setText: (node, text) => { node.textContent = text; },
    renderNoticeDisplay: display => { const n = new Node(); n.textContent = display.facts; return n; },
    getJson, API: { decisions: '/api/decisions' }, requestAnimationFrame: () => {},
  });
  return { ...api, get };
}
const item = (status, extra = {}) => ({ id: status, task: 'task', chat: 'chat', display: '',
  decision: { reason: 'Review outcome', status, request: null, history: [], ...extra } });

test('all phases explain progress and preserve unresolved saved replies', () => {
  const f = fixture();
  const statuses = ['awaiting_owner', 'queued', 'in_progress', 'retry_needed', 'resolved'];
  f.decisionState.loaded = true;
  f.decisionState.items = statuses.map(s => item(s));
  f.renderDecisions();
  assert.equal(f.get('decisions-count').textContent, '4 unresolved · 1 resolved (history)');
  assert.equal(f.get('decisions-history').querySelector('summary').textContent, 'Resolved history (1)');
  assert.equal(f.get('decisions-list').children.length, 4);
  for (const [i, label] of ['Awaiting owner', 'Queued', 'In progress', 'Needs retry or result check', 'Resolved'].entries()) {
    assert.ok(f.decisionCard(item(statuses[i])).textContent.includes(label));
    assert.doesNotMatch(f.decisionCard(item(statuses[i])).textContent, /[\u3040-\u30ff\u3400-\u9fff]/u);
  }
  const saved = item('retry_needed', { error: 'Saved reply has no confirmed effect receipt.' });
  assert.match(f.decisionCard(saved).textContent, /outcome unconfirmed/);
  assert.match(f.decisionCard(saved).textContent, /reconcile-decision/);
  assert.doesNotMatch(f.decisionGuidance(saved.decision), /retry the same recorded request/);
  assert.match(f.decisionGuidance({ ...saved.decision, request: 'resume', retry_safe: true }), /retry the same recorded request/);
  assert.doesNotMatch(f.decisionGuidance({ ...saved.decision, request: 'resume' }), /retry the same recorded request/);
  assert.match(f.decisionGuidance({ status: 'resolved', legacy_settled: true }), /does not confirm/);
});

test('owner titles, context and literal outcome evidence remain unchanged', () => {
  const f = fixture();
  const input = item('resolved', { reason: '所有者の題名', request: '<request>',
    history: [{ status: 'resolved', at: '2026-11-11T00:00:00Z', evidence: '理由 <script>literal</script>' }] });
  input.display = '元の本文';
  const card = f.decisionCard(input);
  assert.match(card.textContent, /所有者の題名/);
  assert.match(card.textContent, /元の本文/);
  assert.match(card.textContent, /理由 <script>literal<\/script>/);
  assert.match(card.textContent, /2026-11-11T00:00:00Z/);
  assert.match(card.textContent, /Recorded request \(not proof of completion\)/);
  assert.match(card.textContent, /does not always mean an operation was applied/);
});

test('partial loads and failed refreshes cannot claim complete current counts', async () => {
  const f = fixture();
  f.decisionState.loaded = true;
  f.decisionState.items = [item('retry_needed')];
  f.decisionState.unreadable = 2;
  f.decisionState.migrationErrors = [{ id: 'old', reason: 'unreadable source' }];
  f.renderDecisions();
  assert.match(f.get('decisions-count').textContent, /partial list/);
  assert.match(f.get('decisions-error').textContent, /2 records.*excluded/);
  assert.match(f.get('decisions-error').textContent, /only loaded inquiries/);
  const card = f.get('decisions-list').children[0];
  f.renderDecisions();
  assert.equal(f.get('decisions-list').children[0], card, 'unchanged cards retain DOM identity');
  await f.loadDecisions();
  assert.match(f.get('decisions-count').textContent, /Refresh failed · last loaded: 1 unresolved/);
  assert.match(f.get('decisions-error').textContent, /out of date/);
  assert.match(f.get('decisions-error').textContent, /Retry loading/);
  assert.equal(f.get('decisions-list').children.length, 1);
  assert.equal(f.get('decisions-empty').hidden, true);
  const fresh = fixture();
  await fresh.loadDecisions();
  assert.equal(fresh.get('decisions-count').textContent, 'Unable to load decisions');
  await fresh.loadInquiry('chat', 'inquiry');
  assert.match(fresh.get('talk-inquiry').textContent, /Retry loading/);
});

test('navigation, title and decision product copy are English', () => {
  assert.doesNotMatch(section, /[\u3040-\u30ff\u3400-\u9fff]/u);
  assert.doesNotMatch(html.slice(html.indexOf('<section class="view" id="view-decisions"'), html.indexOf('<!-- QUESTIONS')), /[\u3040-\u30ff\u3400-\u9fff]/u);
  assert.match(source, /Pending decisions \\u2014 magi/);
  assert.equal((html.match(/<span>Pending decisions<\/span>/g) || []).length, 1);
  assert.match(source, /Back to pending decisions/);
});

const router = source.slice(source.indexOf('function parseRoute()'), source.indexOf('/* ---- two-pane layout'));
const links = source.slice(source.indexOf('function decisionChatLink('), source.indexOf('function decisionCard('));
const context = { location: { hash: '' }, URLSearchParams };
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


test('pending list retains a reachable foreground legacy entry until retirement completes', () => {
  const html = readFileSync(new URL('../assets/ui/index.html', import.meta.url), 'utf8');
  const view = html.slice(html.indexOf('id="view-decisions"'), html.indexOf('<!-- QUESTIONS'));
  assert.ok(view.includes('href="#/questions"'));
  assert.ok(view.includes('id="ask-badge-rail"'));
});


test('ownerless inquiries retain identity and explain verified ownership and explicit settlement', () => {
  const f = fixture();
  const orphan = { ...item('retry_needed'), id: 'orphan', task: '', chat: null, run: 'old-run' };
  const card = f.decisionCard(orphan);
  assert.match(card.textContent, /Inquiry orphan/);
  assert.match(card.textContent, /existing authorized chat/);
  assert.match(card.textContent, /settle-decision/);
  assert.match(card.textContent, /explicit reason/);
  const links = card.children.flatMap(n => n.children || []).filter(n => n.tagName === 'A');
  assert.ok(links.some(n => n.attrs.href === '#/runs/old-run'));
  assert.ok(!links.some(n => n.attrs.href.includes('#/chat/')));
});

test('warnings remain on repeated visits and empty state appears only after source repair', () => {
  const f = fixture();
  f.decisionState.loaded = true;
  f.decisionState.migrationErrors = [{ id: 'saved-inquiry', reason: 'Malformed source; no action executed' }];
  for (let visit = 0; visit < 2; visit++) {
    f.renderDecisions();
    assert.match(f.get('decisions-error').textContent, /saved-inquiry: Malformed source/);
    assert.equal(f.get('decisions-empty').hidden, true);
  }
  f.decisionState.migrationErrors = [];
  f.renderDecisions();
  assert.equal(f.get('decisions-error').hidden, true);
  assert.equal(f.get('decisions-empty').hidden, false);
});

test('inquiry uses shared status and evidence while retaining the owning-chat check', async () => {
  const inquiry = item('retry_needed', { error: 'Saved reply has no confirmed effect receipt.' });
  inquiry.display = '所有者の問い合わせ';
  const f = fixture(async () => inquiry);
  await f.loadInquiry('chat', inquiry.id);
  assert.match(f.get('talk-inquiry').textContent, /Needs retry or result check/);
  assert.match(f.get('talk-inquiry').textContent, /所有者の問い合わせ/);
  assert.match(f.get('talk-inquiry').textContent, /Back to pending decisions/);
  await f.loadInquiry('another-chat', inquiry.id);
  assert.match(f.get('talk-inquiry').textContent, /This inquiry belongs to another chat/);
  assert.doesNotMatch(f.get('talk-inquiry').textContent, /所有者の問い合わせ/);
  assert.match(f.get('talk-inquiry').textContent, /Retry loading/);
});
