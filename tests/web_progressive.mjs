// Execute the shipped loaders with controlled transport completion order.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const sections = source.slice(source.indexOf('const detailSections ='), source.indexOf('function sectionNotice('));
const runs = source.slice(source.indexOf('const runDetailRequests ='), source.indexOf('/* Named with a trailing underscore'));
const talks = source.slice(source.indexOf('// A frame callback runs before paint;'), source.indexOf('/* The server uses Done'));
const tasks = source.slice(source.indexOf('const taskDetailRequests ='), source.indexOf('function renderTask()'));
const stats = source.slice(source.indexOf('async function loadStats('), source.indexOf('/* The repository selector'));
const flush = () => new Promise(resolve => setImmediate(resolve));
function harness({ manualPaint = false, visibility } = {}) {
  const frames = [];
  const sectionRenders = [];
  const requests = [];
  const renders = [];
  const notices = [];
  const failures = [];
  const successes = [];
  const state = { detail: { id: null }, runs: [], talkDetail: { id: 'a' }, talkWaits: new Map(), stats: null, queue: [], taskDetail: { id: null } };
  const get = (url, options) => new Promise((resolve, reject) => requests.push({ url, options, resolve, reject }));
  const api = runInNewContext(`${sections}\n${runs}\n${talks}\n${tasks}\n${stats}\n({loadRun, loadTalk, loadTask, loadStats, cancelDetailSections, sectionRequest, acceptTalkUpdate})`, {
    document: visibility,
    requestAnimationFrame: callback => manualPaint ? frames.push(callback) : callback(), setTimeout: callback => callback(),
    state, $: () => null, AbortController, URLSearchParams, API: { run: id => `run/${id}`, report: id => `raw/${id}`, reportJson: id => `report/${id}`, talk: id => `talk/${id}`, stats: 'stats', task: id => `task/${id}` },
    REPORT_VIEW_SCHEMA: 1, getJson: get, getText: get,
    renderRunDetail: () => renders.push(state.detail.run), renderReportCards: () => renders.push(state.detail.reportView),
    renderTask: () => renders.push(state.taskDetail.task), renderTaskActions() {},
    renderTalkSections: () => sectionRenders.push(state.talkDetail.sections), renderTalk: () => renders.push(state.talkDetail.talk), renderStats: () => renders.push(state.stats),
    renderStatsQueue: counts => renders.push({ queue: counts }),
    show() {}, sectionNotice: (...args) => notices.push(args), trackTalkThinking() {}, rememberTalkChoices() {}, endTalkTurn() {},
    ok: (...args) => successes.push(args), failFor: (...args) => failures.push(args), shortId: id => id,
  });
  return { api, requests, renders, sectionRenders, frames, notices, failures, successes, state };
}
function assertNoFailures(h) {
  assert.equal(h.failures.length, 0, 'successful loads must not call failFor');
  assert.equal(h.notices.filter(n => typeof n[3] === 'function').length, 0, 'successful loads must not offer failure retries');
}
test('run core renders before slow report; successful cards do not fetch text', async () => {
  const h = harness();
  const done = h.api.loadRun('a');
  h.requests.find(r => r.url === 'run/a/core').resolve({ id: 'a', updated_at: 'now', useful: true });
  await flush();
  assert.equal(h.state.detail.run.useful, true);
  assert.equal(h.state.detail.reportLoading, true);
  h.requests.find(r => r.url === 'run/a').resolve({ id: 'a', updated_at: 'now', useful: true });
  h.requests.find(r => r.url === 'report/a').resolve({ schema: 1, sections: [] });
  await done;
  assert.equal(h.state.detail.reportLoading, false);
  assert.equal(h.requests.length, 3);
  assertNoFailures(h);
});
test('unknown schema triggers real text fallback independently', async () => {
  const h = harness();
  const done = h.api.loadRun('a');
  h.requests[0].resolve({ id: 'a', updated_at: 'now' });
  h.requests[1].resolve({ id: 'a', updated_at: 'now' });
  h.requests[2].resolve({ schema: 999 });
  await flush();
  assert.equal(h.requests[3].url, 'raw/a');
  h.requests[3].resolve('Readable report');
  await done;
  assert.equal(h.state.detail.report, 'Readable report');
  assertNoFailures(h);
});
test('navigation aborts reads and rejects late responses even on return to same id', async () => {
  const h = harness();
  const old = h.api.loadRun('a');
  h.api.cancelDetailSections();
  h.state.detail = { id: null };
  const next = h.api.loadRun('a');
  assert.equal(h.requests[0].options.signal.aborted, true);
  h.requests[3].resolve({ id: 'a', updated_at: 'now', version: 2 });
  h.requests[4].resolve({ id: 'a', updated_at: 'now', version: 2 });
  h.requests[5].resolve({ schema: 1, sections: [] });
  await next;
  h.requests[0].resolve({ id: 'a', updated_at: 'now', version: 1 });
  h.requests[1].reject(new Error('late failure'));
  h.requests[2].resolve({ schema: 1, sections: [] });
  await old;
  assert.equal(h.state.detail.run.version, 2);
  assert.ok(!h.notices.some(n => n[2].includes('late failure')));
  assertNoFailures(h);
});
test('pending refresh joins work, and failure retains useful report and run', async () => {
  const h = harness();
  const first = h.api.loadRun('a');
  const second = h.api.loadRun('a');
  assert.equal(h.requests.length, 3);
  h.requests[0].resolve({ id: 'a', updated_at: 'now', useful: true });
  await flush();
  const core = h.state.detail.core;
  assert.equal(core.useful, true);
  const third = h.api.loadRun('a');
  assert.equal(h.requests.length, 3, 'an ordinary join must not start or queue more reads');
  assert.equal(h.state.detail.core, core, 'joining must preserve staged core');
  h.requests[1].resolve({ id: 'a', updated_at: 'now', useful: true });
  h.requests[2].resolve({ schema: 1, sections: ['kept'] });
  await Promise.all([first, second, third]);
  assertNoFailures(h);
  assert.equal(h.requests.length, 3);
  const refresh = h.api.loadRun('a', { background: true });
  h.requests[3].reject(new Error('core unavailable'));
  h.requests[4].reject(new Error('evidence unavailable'));
  h.requests[5].reject(new Error('report unavailable'));
  await refresh;
  assert.equal(h.state.detail.run.useful, true);
  assert.equal(h.state.detail.reportView.sections[0], 'kept');
  for (const message of ['core unavailable', 'evidence unavailable', 'report unavailable']) {
    assert.ok(h.notices.some(n => n[2].includes(message) && typeof n[3] === 'function'));
  }
});
test('conversation transcript survives failed secondary information in either order', async () => {
  const h = harness();
  const done = h.api.loadTalk('a');
  h.requests[0].resolve({ id: 'a', turns: [{ body: 'Useful content' }] });
  await flush();
  assert.equal(h.state.talkDetail.talk.turns[0].body, 'Useful content');
  h.requests[1].reject(new Error('queue unavailable'));
  await done;
  assert.equal(h.state.talkDetail.talk.turns.length, 1);
  assert.ok(h.notices.some(n => n[2].includes('queue unavailable') && typeof n[3] === 'function'));
  assert.equal(h.failures.length, 0);
});
test('dashboard filter changes discard reversed responses', async () => {
  const h = harness();
  h.state.statsRepo = 'first';
  const first = h.api.loadStats();
  h.state.statsRepo = 'second';
  const second = h.api.loadStats();
  h.requests.find(r => r.url.includes('repo=second')).resolve({ repo: 'second' });
  h.requests.find(r => r.url === 'stats/queue').resolve({ queued: 3 });
  await flush();
  // Coalesced background queue refresh also completes.
  h.requests.filter(r => r.url === 'stats/queue').at(-1).resolve({ queued: 3 });
  await second;
  h.requests.find(r => r.url.includes('repo=first')).resolve({ repo: 'first' });
  await first;
  assert.equal(h.state.stats.repo, 'second');
  assert.equal(h.requests.find(r => r.url.includes('repo=first')).options.signal.aborted, true);
  assertNoFailures(h);
});

for (const staleFails of [false, true]) {
  test(`a change during a slow read keeps trailing evidence gated (staleFails=${staleFails})`, async () => {
    const h = harness();
    const first = h.api.loadRun('a');
    const second = h.api.loadRun('a', { background: true });
    const third = h.api.loadRun('a', { background: true });
    const joined = h.api.loadRun('a');
    assert.equal(h.requests.length, 3);
    if (staleFails) {
      for (const request of h.requests) request.reject(new Error('superseded read failed'));
    } else {
      h.requests[0].resolve({ id: 'a', updated_at: 'now', version: 1 });
      h.requests[1].resolve({ id: 'a', updated_at: 'now', version: 1 });
      h.requests[2].resolve({ schema: 1, sections: [] });
    }
    await flush();
    assert.equal(h.requests.length, 6, 'change signals coalesce into one trailing read per section');
    assert.equal(h.state.detail.core, null);
    assert.equal(h.state.detail.evidence, null);
    assert.equal(h.state.detail.evidencePending, true);
    for (const key of ['run-core-state', 'run-evidence-state']) {
      assert.equal(h.notices.filter(n => n[1] === key).at(-1)[4]?.pending, true);
    }
    h.requests[3].resolve({ id: 'a', updated_at: 'now', version: 2 });
    await flush();
    assert.equal(h.state.detail.core.version, 2);
    assert.equal(h.state.detail.evidence, null);
    assert.equal(h.state.detail.evidencePending, true, 'current core alone cannot enable stale actions');
    h.requests[4].resolve({ id: 'a', updated_at: 'now', version: 2 });
    h.requests[5].resolve({ schema: 1, sections: [] });
    await Promise.all([first, second, third, joined]);
    assert.equal(h.state.detail.run.version, 2);
    assert.equal(h.state.detail.evidencePending, false);
    assert.equal(h.requests.length, 6);
    assertNoFailures(h);
  });
}

test('task instructions are useful while history fails, without complete action evidence', async () => {
  const h = harness();
  const done = h.api.loadTask('a');
  h.requests[0].resolve({ id: 'a', instruction: 'Useful instructions' });
  await flush();
  assert.equal(h.state.taskDetail.task.instruction, 'Useful instructions');
  assert.equal(h.state.taskDetail.task.coreOnly, true);
  h.requests[1].reject(new Error('history unavailable'));
  await done;
  assert.equal(h.state.taskDetail.task.instruction, 'Useful instructions');
  assert.equal(h.state.taskDetail.error, 'history unavailable');
  assert.ok(h.notices.some(n => n[2].includes('history unavailable') && typeof n[3] === 'function'));
});

test('late task core cannot downgrade an already complete detail', async () => {
  const h = harness();
  const done = h.api.loadTask('a');
  h.requests[1].resolve({ id: 'a', history: ['current'], instruction: 'Full' });
  await flush();
  h.requests[0].resolve({ id: 'a', instruction: 'Earlier' });
  await done;
  assert.equal(h.state.taskDetail.task.instruction, 'Full');
  assert.equal(h.state.taskDetail.task.coreOnly, undefined);
  assertNoFailures(h);
});

for (const evidenceFirst of [false, true]) {
  for (const matches of [false, true]) {
    test(`run actions require matching saved evidence (evidenceFirst=${evidenceFirst}, matches=${matches})`, async () => {
      const h = harness();
      const done = h.api.loadRun('a');
      const core = { id: 'a', updated_at: 'now', useful: true };
      const evidence = { id: 'a', updated_at: matches ? 'now' : 'earlier', live: 'dead' };
      const first = evidenceFirst ? 1 : 0;
      h.requests[first].resolve(evidenceFirst ? evidence : core);
      await flush();
      assert.equal(h.state.detail.evidenceError, true);
      if (!evidenceFirst) assert.equal(h.state.detail.run.useful, true);
      h.requests[1 - first].resolve(evidenceFirst ? core : evidence);
      h.requests[2].resolve({ schema: 1, sections: [] });
      await done;
      assert.equal(h.state.detail.evidenceError, !matches);
      if (!matches) assert.equal(h.state.detail.run.useful, true);
      else assertNoFailures(h);
    });
  }
}

for (const failure of [false, true]) {
  test(`background run refresh retains snapshot until evidence completes (failure=${failure})`, async () => {
    const h = harness();
    const initial = h.api.loadRun('a');
    const saved = { id: 'a', updated_at: 'before', live: 'dead', instruction: 'Kept' };
    h.requests[0].resolve(saved);
    h.requests[1].resolve(saved);
    h.requests[2].resolve({ schema: 1, sections: [] });
    await initial;
    assertNoFailures(h);
    const rendered = h.renders.length;
    const refresh = h.api.loadRun('a', { background: true });
    assert.equal(h.state.detail.run, saved);
    assert.equal(h.state.detail.evidenceError, false);
    assert.equal(h.state.detail.evidencePending, true);
    assert.equal(h.notices.filter(n => n[1] === 'run-evidence-state' && n[4]?.pending).at(-1)[4].snapshot, true);
    assert.equal(h.renders.length, rendered);
    h.requests[3].resolve({ ...saved, updated_at: 'after' });
    await flush();
    assert.equal(h.state.detail.run, saved);
    assert.equal(h.renders.length, rendered);
    if (failure) h.requests[4].reject(new Error('temporary failure'));
    else h.requests[4].resolve({ ...saved, updated_at: 'after' });
    h.requests[5].resolve({ schema: 1, sections: [] });
    await refresh;
    assert.equal(h.state.detail.evidencePending, false);
    assert.equal(h.state.detail.evidenceError, failure);
    assert.equal(h.state.detail.run.updated_at, failure ? 'before' : 'after');
    if (failure) assert.ok(h.notices.some(n => n[2].includes('temporary failure') && typeof n[3] === 'function'));
    else assertNoFailures(h);
  });
}

test('unchanged background evidence does not rebuild focused action controls', async () => {
  const h = harness();
  const saved = { id: 'a', updated_at: 'before', live: 'dead' };
  const initial = h.api.loadRun('a');
  h.requests[0].resolve(saved); h.requests[1].resolve(saved);
  h.requests[2].resolve({ schema: 1, sections: [] });
  await initial;
  const rendered = h.renders.filter(r => r === saved).length;
  const refresh = h.api.loadRun('a', { background: true });
  h.requests[3].resolve({ ...saved }); h.requests[4].resolve({ ...saved });
  h.requests[5].resolve({ schema: 1, sections: [] });
  await refresh;
  assert.equal(h.renders.filter(r => r && r.id === 'a').length, rendered);
  assertNoFailures(h);
});


test('dashboard queue is useful while history is pending and survives its failure', async () => {
  const h = harness();
  const done = h.api.loadStats();
  h.requests.find(r => r.url === 'stats/queue').resolve({ queued: 17 });
  await flush();
  assert.equal(h.renders[0].queue.queued, 17);
  assert.equal(h.state.stats, null);
  h.requests.find(r => r.url !== 'stats/queue').reject(new Error('history offline'));
  await done;
  assert.equal(h.renders[0].queue.queued, 17);
  assert.ok(h.notices.some(n => n[2].includes('history offline') && typeof n[3] === 'function'));
  assert.equal(h.failures.length, 1);
  assert.ok(h.failures[0][1].includes('history offline'));
  assert.equal(h.failures[0][2].message, 'history offline');
});


test('queue-only dashboard refresh preserves run history without requesting it again', async () => {
  const h = harness();
  h.state.stats = { useful: 'history' };
  const done = h.api.loadStats({ background: true, queueOnly: true });
  assert.equal(h.requests.length, 1);
  assert.equal(h.requests[0].url, 'stats/queue');
  h.requests[0].resolve({ queued: 9 });
  await done;
  assert.equal(h.state.stats.useful, 'history');
  assert.equal(h.renders[0].queue.queued, 9);
  assertNoFailures(h);
  assert.equal(h.successes.length, 1);
});

for (const queueOnly of [false, true]) {
  test(`pending dashboard updates schedule one trailing refresh per invalidated section (queueOnly=${queueOnly})`, async () => {
    const h = harness();
    const first = h.api.loadStats();
    const updates = [
      h.api.loadStats({ background: true, queueOnly }),
      h.api.loadStats({ background: true, queueOnly }),
    ];
    let completed = false;
    const done = Promise.all([first, ...updates]).then(() => { completed = true; });
    assert.equal(h.requests.length, 2);
    h.requests[0].resolve({ queued: 1 });
    h.requests[1].resolve({ version: 1 });
    await flush();
    assert.equal(completed, false);
    assert.equal(h.requests.length, queueOnly ? 3 : 4);
    assert.ok(h.requests.every(r => !r.options.signal.aborted));
    assert.equal(h.requests[2].url, 'stats/queue');
    h.requests[2].resolve({ queued: 2 });
    if (!queueOnly) {
      await flush();
      assert.equal(completed, false);
      assert.equal(h.requests[3].url, h.requests[1].url);
      h.requests[3].resolve({ version: 2 });
    }
    await done;
    assert.equal(h.state.stats.version, queueOnly ? 1 : 2);
    assert.equal(h.renders.filter(r => r && r.queue).at(-1).queue.queued, 2);
    assert.equal(h.renders.filter(r => r && r.version).at(-1).version, queueOnly ? 1 : 2);
    assert.equal(h.requests.length, queueOnly ? 3 : 4);
    assert.equal(h.failures.length, 0);
  });
}

test('ordinary duplicate dashboard loads join pending requests without trailing refreshes', async () => {
  const h = harness();
  const first = h.api.loadStats();
  const second = h.api.loadStats();
  assert.equal(h.requests.length, 2);
  h.requests[0].resolve({ queued: 1 });
  h.requests[1].resolve({ version: 1 });
  await Promise.all([first, second]);
  assert.equal(h.requests.length, 2);
  assert.equal(h.state.stats.version, 1);
});

test('pending notices preserve snapshots and Retry controls, and settle busy state per section', () => {
  const nodes = new Map();
  const anchor = {
    attrs: {}, after: box => nodes.set(box.id, box),
    setAttribute(key, value) { this.attrs[key] = value; },
    removeAttribute(key) { delete this.attrs[key]; },
  };
  nodes.set('anchor', anchor);
  const noticeSource = source.slice(source.indexOf('const sectionNoticePending ='), source.indexOf('// A frame callback runs before paint;'));
  const notice = runInNewContext(`${noticeSource}\nsectionNotice`, {
    $: key => nodes.get(key),
    el: (tag, props) => ({ tag, ...props, dataset: {}, children: [], append(child) { this.children.push(child); } }),
    clear: box => { box.children = []; }, show: (box, visible) => { box.hidden = !visible; },
  });
  notice('anchor', 'core', 'Loading…', null, { pending: true });
  assert.equal(nodes.get('core').hidden, false, 'first load is visible');
  notice('anchor', 'core', '');
  const signature = nodes.get('core').dataset.signature;
  notice('anchor', 'core', 'Loading…', null, { pending: true, snapshot: true });
  notice('anchor', 'evidence', 'Loading…', null, { pending: true, snapshot: true });
  assert.equal(nodes.get('core').hidden, true);
  assert.equal(nodes.get('core').dataset.signature, signature);
  assert.equal(nodes.has('evidence'), false);
  notice('anchor', 'core', ''); // Same signature must still settle its busy state.
  assert.equal(anchor.attrs['aria-busy'], 'true', 'evidence is still pending');
  const retry = () => {};
  notice('anchor', 'evidence', 'Unavailable', retry);
  assert.equal(anchor.attrs['aria-busy'], undefined);
  const error = nodes.get('evidence');
  const button = error.children[1];
  notice('anchor', 'evidence', 'Loading…', null, { pending: true, snapshot: true });
  assert.equal(error.hidden, false);
  assert.equal(error.children[1], button, 'Retry stays in place during refresh');
  notice('anchor', 'evidence', '');
  assert.equal(error.hidden, true);
  assert.equal(anchor.attrs['aria-busy'], undefined);
});

test('dashboard queue snapshot is independent of history, including empty results', async () => {
  const h = harness();
  const first = h.api.loadStats({ queueOnly: true });
  assert.equal(h.notices.at(-1)[4].snapshot, false);
  h.requests[0].resolve({});
  await first;
  const refresh = h.api.loadStats({ queueOnly: true, background: true });
  assert.equal(h.state.stats, null);
  assert.equal(h.notices.at(-1)[4].snapshot, true);
  h.requests[1].resolve({});
  await refresh;
});

for (const report of ['', 'Saved text']) {
  test(`report refresh retains loaded text (${JSON.stringify(report)})`, () => {
    const nodes = new Map();
    const get = key => {
      if (!nodes.has(key)) nodes.set(key, {});
      return nodes.get(key);
    };
    const reportSource = source.slice(source.indexOf('function renderReportCards()'), source.indexOf('function renderRunDetail()'));
    const render = runInNewContext(`${reportSource}\nrenderReportCards`, {
      state: { detail: { id: 'a', reportView: null, reportLoading: true, report } },
      $: get, setText: (node, text) => { node.textContent = text; },
      show: (node, visible) => { node.hidden = !visible; }, setAttr() {},
    });
    render();
    assert.equal(get('run-report').textContent, report);
    assert.equal(get('run-report').hidden, false);
  });
}

test('settings refresh keeps the existing body intact and inert', () => {
  const body = {
    content: 'Saved settings', inert: false,
    toggleAttribute(name, on) { if (name === 'inert') this.inert = on; },
  };
  const settingsState = { data: {}, loading: true };
  const settingsSource = source.slice(source.indexOf('function renderSettings()'), source.indexOf('/* ---- task detail'));
  const render = runInNewContext(`${settingsSource}\nrenderSettings`, {
    settingsState, renderSettingsContext() {},
    $: () => body, clear: () => { body.content = ''; },
  });
  render();
  assert.equal(body.content, 'Saved settings');
  assert.equal(body.inert, true);
  settingsState.data = null;
  settingsState.loading = false;
  try { render(); } catch { /* later rendering needs the full DOM; only inert matters */ }
  assert.equal(body.inert, false);
});

test('conversation navigation rejects old core and enrichment even on return to the same id', async () => {
  const h = harness();
  const old = h.api.loadTalk('a');
  h.api.cancelDetailSections();
  h.state.talkDetail = { id: 'a' };
  const current = h.api.loadTalk('a');
  assert.equal(h.requests[0].options.signal.aborted, true);
  h.requests[1].resolve({ id: 'a', updated_at: 'new', turns: [{ body: 'Current transcript' }] });
  await flush();
  assert.equal(h.state.talkDetail.talk.turns[0].body, 'Current transcript');
  h.requests[2].resolve({ context_for: 'new', roster: [], personas: [], tasks: ['current'] });
  await current;
  h.requests[0].resolve({ id: 'a', updated_at: 'old', turns: [{ body: 'Obsolete transcript' }] });
  assert.equal(h.requests.length, 3, 'obsolete core cannot launch sections');
  await old;
  assert.equal(h.state.talkDetail.talk.turns[0].body, 'Current transcript');
  assert.deepEqual(h.state.talkDetail.talk.tasks, ['current']);
  assertNoFailures(h);
});

function referenceHarness() {
  class Node {
    constructor(tag, text = '') { this.tag = tag; this.text = text; this.children = []; this.parentNode = null; }
    get textContent() { return this.text + this.children.map(node => node.textContent).join(''); }
    get isConnected() { return this.connected || Boolean(this.parentNode?.isConnected); }
    get nextSibling() { return this.parentNode?.children[this.parentNode.children.indexOf(this) + 1]; }
    append(...nodes) {
      for (const node of nodes.flat(Infinity).filter(Boolean)) {
        node.remove();
        node.parentNode = this;
        this.children.push(node);
      }
    }
    before(...nodes) {
      const parent = this.parentNode;
      const position = parent.children.indexOf(this);
      for (const node of nodes) node.parentNode = parent;
      parent.children.splice(position, 0, ...nodes);
    }
    remove() {
      if (!this.parentNode) return;
      const parent = this.parentNode;
      parent.children.splice(parent.children.indexOf(this), 1);
      this.parentNode = null;
    }
  }
  const make = (tag, props = {}, ...children) => {
    const { text = '', ...attrs } = props;
    const node = Object.assign(new Node(tag, text), attrs);
    node.append(...children);
    return node;
  };
  const requests = [];
  const referenceSource = source.slice(source.indexOf('const referenceRanges ='), source.indexOf('function setAttr('));
  const markdownSource = source.slice(source.indexOf('function buildMd('), source.indexOf('/* Replace `container`'));
  const api = runInNewContext(`${referenceSource}\n${markdownSource}\n({buildMd, flushReferences, refreshReferences})`, {
    el: make, WeakRef, setTimeout: () => 1,
    document: { createTextNode: text => new Node('#text', text), createDocumentFragment: () => new Node('#fragment') },
    postJson: (url, body) => new Promise((resolve, reject) => requests.push({ url, body, resolve, reject })),
  });
  const root = make('root');
  root.connected = true;
  const walk = node => [node, ...node.children.flatMap(walk)];
  return { api, root, requests, walk };
}

test('unlinked transcript Markdown is immediately useful and gains validated links after deferred resolution', async () => {
  const h = referenceHarness();
  const nodes = [
    { type: 'paragraph', children: [{ type: 'strong', children: [{ type: 'text', value: 'task@abcd' }] }] },
    { type: 'bullet_list', items: [{ type: 'list_item', children: [{ type: 'code', code: 'abcd' }] }] },
    { type: 'table', rows: [[{ header: true, children: [{ type: 'text', value: 'R2-1-3' }] }]] },
    { type: 'code_block', code: 'dead' },
    { type: 'link', href: 'https://example.org', children: [{ type: 'text', value: 'beef' }] },
    { type: 'image', src: '/attachment.png', alt: 'cafe' },
  ];
  h.root.append(...nodes.map(h.api.buildMd));
  const initial = h.root.textContent;
  assert.equal(initial, 'task@abcdabcdR2-1-3deadbeef');
  const pending = h.api.flushReferences();
  assert.equal(h.requests.length, 1);
  assert.equal(h.requests[0].url, '/api/references');
  assert.equal(JSON.stringify(h.requests[0].body), JSON.stringify([
    { text: 'task@abcd', code: false }, { text: 'abcd', code: true }, { text: 'R2-1-3', code: false },
  ]));
  assert.equal(h.walk(h.root).filter(node => node.tag === 'a').length, 1, 'only the existing external link precedes resolution');
  h.requests[0].resolve(h.requests[0].body.map(item => ({
    watch: true, parts: [{ text: item.text, href: item.text === 'R2-1-3' ? '#/runs/real/report' : '#/tasks/real' }],
  })));
  await pending;
  assert.equal(h.root.textContent, initial);
  const links = h.walk(h.root).filter(node => node.tag === 'a');
  assert.equal(links.length, 4);
  assert.equal(links.filter(node => node.href === '#/tasks/real').length, 2);
  assert.equal(h.walk(h.root).find(node => node.tag === 'pre').textContent, 'dead');
  assert.equal(h.walk(h.root).find(node => node.tag === 'img').src, '/attachment.png');
});

test('late reference decoration never changes a replaced conversation', async () => {
  const h = referenceHarness();
  const previous = h.api.buildMd({ type: 'paragraph', children: [{ type: 'text', value: 'task@abcd' }] });
  h.root.append(previous);
  const pending = h.api.flushReferences();
  previous.remove();
  h.root.append(h.api.buildMd({ type: 'paragraph', children: [{ type: 'text', value: 'task@abcd' }] }));
  h.requests[0].resolve([{ watch: true, parts: [{ text: 'task@abcd', href: '#/tasks/obsolete' }] }]);
  await pending;
  assert.equal(h.root.textContent, 'task@abcd');
  assert.equal(h.walk(h.root).filter(node => node.tag === 'a').length, 0);
  const current = h.api.flushReferences();
  h.requests[1].resolve([{ watch: true, parts: [{ text: 'task@abcd', href: '#/tasks/current' }] }]);
  await current;
  assert.equal(h.walk(h.root).find(node => node.tag === 'a').href, '#/tasks/current');
});

test('conversation detail releases health/list connections when core finishes', async () => {
  const requests = [];
  const transport = source.slice(source.indexOf('const conversationPriority ='), source.indexOf('const getText ='));
  const state = { talkDetail: { id: 'a' }, talkWaits: new Map() };
  const api = runInNewContext(`${transport}\n${sections}\n${talks}\n({getJson, loadTalk})`, {
    requestAnimationFrame: callback => callback(), setTimeout: callback => callback(),
    AbortController, state, API: { talk: id => `/api/talks/${id}` },
    pendingForRequest: () => () => {},
    fetch: (url, options = {}) => new Promise((resolve, reject) => {
      const entry = { url, options, resolve: value => resolve({ ok: true, json: async () => value }) };
      requests.push(entry);
      options.signal?.addEventListener('abort', () => reject(new Error('aborted')));
    }),
    sectionNotice() {}, trackTalkThinking() {}, renderTalk() {}, renderTalkSections() {}, ok() {}, rememberTalkChoices() {},
    shortId: id => id, failFor: () => assert.fail('unexpected detail failure'),
  });
  const health = api.getJson('/api/health');
  const list = api.getJson('/api/talks/page');
  await flush();
  assert.equal(requests.length, 2);
  const detail = api.loadTalk('a');
  await flush();
  assert.ok(requests[0].options.signal.aborted);
  assert.ok(requests[1].options.signal.aborted);
  assert.equal(requests.length, 3, 'only core starts before transcript rendering');
  requests.find(r => r.url.endsWith('/core')).resolve({ id: 'a' });
  await flush();
  assert.equal(state.talkDetail.talk.id, 'a');
  assert.equal(requests.length, 6, 'bulk reads resume while sections remain pending');
  requests.find(r => r.url.endsWith('/sections')).resolve({ roster: [], personas: [] });
  await detail;
  await flush();
  assert.equal(requests.length, 6);
  requests[4].resolve({});
  requests[5].resolve({ rows: [], next: null });
  await Promise.all([health, list]);
});

for (const expired of [false, true]) test(`chat paging is on demand and recovers expired cursors (expired=${expired})`, async () => {
  const requests = [];
  let recovered = false;
  const state = { talks: null, talkReads: {}, talkWaits: new Map(), rev: {} };
  const loaders = source.slice(source.indexOf('const storeReads ='), source.indexOf('function loadRuns('));
  const chatLoaders = source.slice(source.indexOf('let talksNext ='), source.indexOf('/* Refresh one conversation.'));
  const api = runInNewContext(`${loaders}\n${chatLoaders}\n({ loadTalks, loadOlderTalks, markAllTalksRead: undefined })`, {
    state, API: { talks: '/api/talks' },
    getJson: async url => {
      requests.push(url);
      if (expired && url.includes('before=') && !recovered) {
        recovered = true;
        throw Object.assign(new Error('cursor expired'), { status: 409 });
      }
      return url.includes('before=') ? { rows: [{ id: 'old', turns: [] }], next: null }
        : { rows: [{ id: 'new', turns: [] }], next: 'cursor' };
    },
    $: () => null, sectionNotice() {}, renderTalks() {}, refreshSearch() {}, trackTalkThinking() {},
    sortTalks: list => list, ok() {}, setTimeout, clearTimeout, incrementalListRender: false,
  });
  await api.loadTalks();
  assert.equal(requests.length, 1);
  assert.equal(state.talks.length, 1);
  await api.loadOlderTalks();
  assert.equal(requests.length, expired ? 4 : 2);
  assert.equal(state.talks.length, 2);
});

for (const expired of [false, true]) for (const deltaFirst of [false, true]) {
  test(`older chat pages preserve overlapping deltas (expired=${expired}, deltaFirst=${deltaFirst})`, async () => {
    const requests = [];
    const state = { talks: null, talkReads: {}, talkWaits: new Map(), rev: {} };
    const loaders = source.slice(source.indexOf('const storeReads ='), source.indexOf('function loadRuns('));
    const chatLoaders = source.slice(source.indexOf('let talksNext ='), source.indexOf('/* Refresh one conversation.'));
    const api = runInNewContext(`${loaders}\n${chatLoaders}\n({ loadTalks, loadOlderTalks })`, {
      state, API: { talks: '/api/talks' },
      getJson: url => new Promise((resolve, reject) => requests.push({ url, resolve, reject })),
      $: () => null, sectionNotice() {}, renderTalks() {}, refreshSearch() {}, trackTalkThinking() {},
      sortTalks: list => list, ok() {}, setTimeout: () => 1, clearTimeout() {}, incrementalListRender: false,
    });
    const initial = api.loadTalks({ rev: 'r0' });
    requests[0].resolve({ rows: [{ id: 'new' }, { id: 'updated', version: 0 }], next: 'cursor' });
    await initial;
    const older = api.loadOlderTalks();
    let pageRequest = requests[1];
    if (expired) {
      pageRequest.reject(Object.assign(new Error('expired'), { status: 409 }));
      await flush();
      requests[2].resolve({ rows: [{ id: 'new' }], next: 'replacement' });
      await flush();
      pageRequest = requests[3];
    }
    const delta = api.loadTalks({ rev: 'r1', delta: {
      base: 'r0', changed: ['updated', 'unreadable'], removed: ['deleted'],
    } });
    const deltaRequest = requests.at(-1);
    const page = { rows: ['updated', 'unreadable', 'deleted', 'untouched'].map(id => ({ id, version: 0 })), next: 'older' };
    if (deltaFirst) {
      deltaRequest.resolve([{ id: 'updated', version: 1 }]);
      await delta;
      pageRequest.resolve(page);
      await older;
    } else {
      pageRequest.resolve(page);
      await older;
      deltaRequest.resolve([{ id: 'updated', version: 1 }]);
      await delta;
    }
    assert.equal(state.rev.talks, 'r1');
    assert.equal(state.talks.find(row => row.id === 'updated').version, 1);
    assert.ok(!state.talks.some(row => row.id === 'deleted' || row.id === 'unreadable'));
    assert.ok(state.talks.some(row => row.id === 'untouched'), 'unaffected older rows still load');
    // The accepted page must advance the cursor even when some rows were excluded.
    const next = api.loadOlderTalks();
    assert.equal(requests.at(-1).url, '/api/talks/page?before=older');
    requests.at(-1).resolve({ rows: [], next: null });
    await next;
  });
}

for (const failSecondBatch of [false, true]) test(`coalesced chat deltas fetch all batches before applying (failure=${failSecondBatch})`, async () => {
  const ids = Array.from({ length: 60 }, (_, i) => `chat-${i}`);
  const requests = [];
  const state = {
    talks: ['starter', 'removed', ...ids].map(id => ({ id, version: 0 })),
    talkReads: { 'chat-59': 'read-at' }, talkWaits: new Map(), rev: { talks: 'r0' },
  };
  const loaders = source.slice(source.indexOf('const storeReads ='), source.indexOf('function loadRuns('));
  const chatLoaders = source.slice(source.indexOf('let talksNext ='), source.indexOf('/* Refresh one conversation.'));
  const api = runInNewContext(`${loaders}\n${chatLoaders}\n({ loadTalks })`, {
    state, API: { talks: '/api/talks' },
    getJson: (url, options) => new Promise((resolve, reject) => {
      const requested = new URL(url, 'http://localhost').searchParams.get('ids').split(',');
      requests.push({ ids: requested, options, resolve, reject });
    }),
    $: () => null, sectionNotice() {}, renderTalks() {}, refreshSearch() {}, trackTalkThinking() {},
    sortTalks: list => list, ok() {}, setTimeout: () => 1, clearTimeout() {}, incrementalListRender: false,
  });
  const done = api.loadTalks({ rev: 'r1', delta: { base: 'r0', changed: ['starter'], removed: [] } });
  api.loadTalks({ rev: 'r2', delta: { base: 'r1', changed: ids.slice(0, 30), removed: [] } });
  api.loadTalks({ rev: 'r3', delta: { base: 'r2', changed: ids.slice(30), removed: ['removed'] } });
  requests[0].resolve([{ id: 'starter', version: 1 }]);
  await flush();
  assert.equal(requests[1].ids.length, 50);
  requests[1].resolve(requests[1].ids.map(id => ({ id, version: 1 })));
  await flush();
  assert.equal(requests[2].ids.length, 10);
  assert.equal(state.rev.talks, 'r1', 'revision waits for all batches');
  assert.equal(state.talks.find(row => row.id === ids[0]).version, 0, 'no partial merge');
  if (failSecondBatch) {
    requests[2].reject(new Error('batch failed'));
    await done;
    assert.equal(state.rev.talks, null, 'failed batches require recovery');
    assert.equal(state.talks.length, 62, 'failed delta retains the snapshot');
    assert.ok(state.talks.filter(row => ids.includes(row.id)).every(row => row.version === 0));
  } else {
    // A changed row absent from the server must still be removed.
    requests[2].resolve(requests[2].ids.filter(id => id !== 'chat-58').map(id => ({ id, version: 1 })));
    await flush();
    assert.equal(requests[3].ids.join(','), 'chat-59');
    assert.equal(JSON.parse(requests[3].options.headers['x-talk-reads'])['chat-59'], 'read-at');
    requests[3].resolve([{ id: 'chat-59', version: 1, unread_count: 0 }]);
    await done;
    assert.equal(state.rev.talks, 'r3');
    assert.equal(state.talks.length, 60);
    assert.ok(!state.talks.some(row => row.id === 'removed' || row.id === 'chat-58'));
    assert.ok(state.talks.every(row => row.version === 1));
    assert.equal(state.talks.find(row => row.id === 'chat-59').unread_count, 0);
    assert.deepEqual(requests.slice(1, 3).flatMap(request => request.ids), ids);
  }
});

test('mark all read includes unloaded pages and uses captured turn timestamps', async () => {
  const saved = [];
  const state = { talks: [{ id: 'new', turns: [{ who: 'agent', at: '2026-10-20T00:00:00Z' }] }], talkReads: {} };
  const unread = source.slice(source.indexOf('const talkTurns ='), source.indexOf('/* Open first, then newest first'));
  const complete = runInNewContext(`let talksNext = 'older';
    async function loadOlderTalks() {
      state.talks.push({ id: 'old', turns: [{ who: 'agent', at: '2026-10-19T00:00:00Z' }] });
      talksNext = null;
    }
    ${unread}\n({ markAllTalksRead, talkUnreadCount })`, {
    state, TALK_READS_KEY: 'reads', saveCollapsed: (_, data) => saved.push({ ...data }), renderTalks() {}, announce() {},
  });
  await complete.markAllTalksRead();
  assert.equal(saved.length, 1);
  assert.equal(saved[0].old, '2026-10-19T00:00:00Z');
  assert.equal(saved[0].new, '2026-10-20T00:00:00Z');
  assert.equal(complete.talkUnreadCount({ id: 'old', unread_count: 7, turns: [{ at: saved[0].old }] }), 0);
  assert.equal(complete.talkUnreadCount(null), 0);
});

test('chat search hydrates matches outside the loaded page in bounded batches', async () => {
  const requests = [];
  const state = { talks: [{ id: 'loaded' }], talkReads: {}, search: { chats: { seq: 0 } } };
  const search = source.slice(source.indexOf('async function runSearch('), source.indexOf('/* After the list behind a search'));
  const rows = source.slice(source.indexOf('function talkReadOptions('), source.indexOf('function showOlderTalks('));
  const api = runInNewContext(`${rows}\n${search}\n({ runSearch })`, {
    state, API: { search: () => '/api/search', talks: '/api/talks' },
    searchText: () => 'needle', renderSearchScope() {},
    mergeStore: (_, rows) => [...state.talks, ...rows],
    getJson: async url => {
      requests.push(url);
      if (url === '/api/search') return { hits: Array.from({ length: 73 }, (_, i) => ({ id: `older-${i}` })), total: 73 };
      const ids = new URL(url, 'http://localhost').searchParams.get('ids').split(',');
      assert.ok(ids.length <= 50);
      return ids.map(id => ({ id }));
    },
  });
  await api.runSearch('chats');
  assert.equal(requests.length, 3);
  assert.equal(state.talks.length, 74);
  assert.equal(state.search.chats.status, 'ok');
  assert.equal(state.search.chats.hits.size, 73);
});

test('conversation paints core before starting sections, and sections never rerender transcript', async () => {
  const h = harness({ manualPaint: true });
  const done = h.api.loadTalk('a');
  assert.equal(h.requests.length, 1);
  h.requests[0].resolve({ id: 'a', updated_at: 'now', turns: [{ body: 'Visible transcript' }] });
  await flush();
  assert.equal(h.renders.length, 1);
  assert.equal(h.requests.length, 1, 'secondary work waits for paint');
  assert.ok(h.notices.some(n => n[1] === 'talk-core-state' && n[2] === ''));
  h.frames.shift()();
  await flush();
  assert.equal(h.requests[1].url, 'talk/a/sections');
  assert.equal(h.renders.length, 1, 'held sections leave transcript visible');
  h.requests[1].resolve({ context_for: 'now', tasks: ['related'], roster: [], personas: [] });
  await done;
  assert.equal(h.renders.length, 1, 'enrichment updates only secondary controls');
  assert.equal(h.sectionRenders.length, 1);
  assert.deepEqual(h.state.talkDetail.talk.tasks, ['related']);
});

test('navigation cancels sections scheduled for after paint', async () => {
  const h = harness({ manualPaint: true });
  const done = h.api.loadTalk('a');
  h.requests[0].resolve({ id: 'a' });
  await flush();
  h.api.cancelDetailSections();
  h.frames.shift()();
  await done;
  assert.equal(h.requests.length, 1);
});

for (const status of [404, 500]) test(`core failure never starts sections (${status})`, async () => {
  const h = harness();
  const done = h.api.loadTalk('a');
  h.requests[0].reject(Object.assign(new Error('Unavailable'), { status }));
  await done;
  assert.equal(h.requests.length, 1);
  if (status === 404) assert.equal(h.state.talkDetail.gone, true);
});

test('sections retry preserves transcript and rejects stale context and persona metadata', async () => {
  const h = harness();
  const done = h.api.loadTalk('a');
  h.requests[0].resolve({ id: 'a', updated_at: 'new', persona_name: 'Current persona' });
  await flush();
  h.requests[1].reject(new Error('Secondary unavailable'));
  await done;
  const retry = h.notices.find(n => n[2].includes('Secondary unavailable'))[3]();
  h.requests[2].resolve({ id: 'a', updated_at: 'new', persona_name: 'Current persona' });
  await flush();
  h.requests[3].resolve({ context_for: 'old', context: 'obsolete', persona_name: 'Old persona', tasks: ['current'], roster: [], personas: [] });
  await retry;
  assert.equal(h.state.talkDetail.talk.context, undefined);
  assert.equal(h.state.talkDetail.talk.persona_name, 'Current persona');
  assert.deepEqual(h.state.talkDetail.talk.tasks, ['current']);
  assert.equal(h.state.talkDetail.sectionsError, false);
});

test('pending sections coalesce refreshes without holding newer transcript updates', async () => {
  const h = harness();
  const first = h.api.loadTalk('a');
  h.requests[0].resolve({ id: 'a', updated_at: 'old' });
  await flush();
  const refresh = h.api.loadTalk('a', { background: true });
  h.requests[2].resolve({ id: 'a', updated_at: 'new', turns: [{ body: 'New transcript' }] });
  await flush();
  assert.equal(h.state.talkDetail.talk.turns[0].body, 'New transcript');
  assert.equal(h.requests.length, 3, 'sections refresh joins existing work');
  h.requests[1].resolve({ context_for: 'old', context: 'old context', tasks: ['old'], roster: [], personas: [] });
  await flush();
  assert.equal(h.requests.length, 4, 'one trailing sections read retains the refresh');
  h.requests[3].resolve({ context_for: 'new', context: 'new context', tasks: ['new'], roster: [], personas: [] });
  await Promise.all([first, refresh]);
  assert.equal(h.state.talkDetail.talk.context, 'new context');
  assert.deepEqual(h.state.talkDetail.talk.tasks, ['new']);
});

test('successful operations invalidate both pending responses and scheduled enrichment', async () => {
  const h = harness({ manualPaint: true });
  const old = h.api.loadTalk('a');
  h.requests[0].resolve({ id: 'a', updated_at: 'old', agent: 'old' });
  await flush();
  h.api.acceptTalkUpdate({ id: 'a', updated_at: 'new', agent: 'new' });
  h.frames.shift()();
  await old;
  assert.equal(h.requests.length, 2, 'operation supersedes deferred sections');
  h.requests[1].resolve({ id: 'a', updated_at: 'new', agent: 'new' });
  await flush();
  h.frames.shift()();
  await flush();
  h.requests[2].resolve({ context_for: 'new', roster: [], personas: [], tasks: ['new'] });
  await flush();
  assert.equal(h.state.talkDetail.talk.agent, 'new');
  assert.deepEqual(h.state.talkDetail.talk.tasks, ['new']);
});

test('hiding a chat while its first paint is scheduled does not stall refreshes', async () => {
  const listeners = new Set();
  const visibility = { hidden: false,
    addEventListener: (_, listener) => listeners.add(listener),
    removeEventListener: (_, listener) => listeners.delete(listener) };
  const h = harness({ manualPaint: true, visibility });
  const done = h.api.loadTalk('a');
  h.requests[0].resolve({ id: 'a' });
  await flush();
  visibility.hidden = true;
  for (const listener of listeners) listener();
  await flush();
  assert.equal(listeners.size, 0);
  assert.equal(h.requests[1].url, 'talk/a/sections');
  h.requests[1].resolve({ roster: [], personas: [] });
  await done;
});

test('a successful trailing core retry starts sections after a coalesced failure', { timeout: 5000 }, async () => {
  const h = harness({ manualPaint: true });
  const first = h.api.loadTalk('a');
  const refresh = h.api.loadTalk('a', { background: true });
  assert.equal(h.requests.length, 1);
  h.requests[0].reject(Object.assign(new Error('Temporary failure'), { status: 503 }));
  await flush();
  assert.equal(h.requests[1].url, 'talk/a/core');
  h.requests[1].resolve({ id: 'a', updated_at: 'new', turns: [{ body: 'Recovered transcript' }] });
  await flush();
  assert.equal(h.state.talkDetail.talk.turns[0].body, 'Recovered transcript');
  assert.equal(h.notices.filter(n => n[1] === 'talk-core-state').at(-1)[2], '');
  assert.equal(h.requests.length, 2, 'recovered transcript paints before enrichment');
  assert.ok(h.frames.length > 0, 'successful trailing core must schedule enrichment after paint');
  for (const frame of h.frames.splice(0)) frame();
  await flush();
  assert.equal(h.requests.length, 3, 'coalesced callers start one sections request');
  assert.equal(h.requests[2].url, 'talk/a/sections');
  const roster = [{ id: 'available-agent', runnable: true }];
  const personas = [{ id: 'default', name: 'Default' }];
  const data = { context_for: 'new', tasks: ['related'], roster, personas };
  h.requests[2].resolve(data);
  await flush();
  assert.equal(h.requests.length, 4, 'background invalidation retains one trailing sections read');
  assert.equal(h.requests[3].url, 'talk/a/sections');
  h.requests[3].resolve(data);
  await Promise.all([first, refresh]);
  assert.deepEqual(h.state.talkDetail.talk.tasks, ['related']);
  assert.deepEqual(h.state.talkDetail.roster, roster);
  assert.deepEqual(h.state.talkDetail.personas, personas);
  assert.equal(h.notices.filter(n => n[1] === 'talk-sections-state').at(-1)[2], '');
});

test('a failed trailing core refresh retains retry without starting sections', { timeout: 5000 }, async () => {
  const h = harness();
  const first = h.api.loadTalk('a');
  const refresh = h.api.loadTalk('a', { background: true });
  h.requests[0].resolve({ id: 'a', turns: [{ body: 'Retained transcript' }] });
  await flush();
  h.requests[1].reject(Object.assign(new Error('Refresh unavailable'), { status: 503 }));
  await Promise.all([first, refresh]);
  assert.equal(h.requests.length, 2);
  assert.equal(h.state.talkDetail.talk.turns[0].body, 'Retained transcript');
  const notice = h.notices.filter(n => n[1] === 'talk-core-state').at(-1);
  assert.ok(notice[2].includes('Refresh unavailable'));
  assert.equal(typeof notice[3], 'function');
});

function chatListHarness(initial = {}) {
  const requests = [];
  const state = { talks: null, talkReads: {}, talkWaits: new Map(), rev: {}, ...initial };
  const loaders = source.slice(source.indexOf('const storeReads ='), source.indexOf('function loadRuns('));
  const chatLoaders = source.slice(source.indexOf('let talksNext ='), source.indexOf('/* Refresh one conversation.'));
  const sort = source.slice(source.indexOf('function sortTalks('), source.indexOf('/* The context meter'));
  const search = source.slice(source.indexOf('async function runSearch('), source.indexOf('/* After the list behind a search'));
  const api = runInNewContext(`${loaders}\n${chatLoaders}\n${sort}\n${search}\n({ loadTalks, loadOlderTalks, getTalkRows, runSearch })`, {
    state, API: { talks: '/api/talks', search: () => '/api/search' },
    getJson: (url, options) => new Promise((resolve, reject) => requests.push({ url, options, resolve, reject })),
    $: () => null, sectionNotice() {}, renderTalks() {}, refreshSearch() {}, trackTalkThinking() {},
    searchText: () => 'needle', renderSearchScope() {},
    ok() {}, setTimeout: () => 1, clearTimeout() {}, incrementalListRender: false,
  });
  return { api, state, requests };
}

const chatRow = (id, status = 'open', created_at = '2020-01-01T00:00:00Z') => ({ id, status, created_at });
const requestedIds = request => new URL(request.url, 'http://localhost').searchParams.get('ids').split(',');

test('large initial chat pages count all open rows and preserve creation order and ties', async () => {
  const h = chatListHarness();
  const rows = Array.from({ length: 67 }, (_, i) => chatRow(`old-${i}`));
  rows.push(chatRow('a-newer-open', 'open', '2021-01-01T00:00:00Z'));
  rows.push(chatRow('recent-closed', 'closed', '2026-01-01T00:00:00Z'));
  const done = h.api.loadTalks();
  h.requests[0].resolve({ rows, next: 'closed-history' });
  await done;
  assert.equal(h.requests.length, 1, 'history remains on demand');
  assert.equal(h.state.talks.filter(row => row.status === 'open').length, 68);
  assert.equal(h.state.talks[0].id, 'a-newer-open', 'creation time wins over ID order');
  assert.deepEqual(Array.from(h.state.talks.slice(1, 68), row => row.id), rows.slice(0, 67).map(row => row.id));
  assert.equal(h.state.talks.at(-1).id, 'recent-closed');
  const count = {};
  const render = source.slice(source.indexOf('function renderTalks()'), source.indexOf('/* The rail and dock'));
  runInNewContext(`${render}\nrenderTalks()`, {
    state: h.state, $: id => id === 'talks-count' ? count : {}, renderTalkIndicators() {},
    setText: (node, text) => { node.text = text; }, plural: (n, one, many) => `${n} ${n === 1 ? one : many}`,
    show() {}, searchText: () => '', syncList() {}, sortTalks: list => list,
    createTalkCard() {}, updateTalkCard() {}, markSelected() {},
  });
  assert.equal(count.text, '68 conversations open');
});

test('large initial chat pages refresh every read count in batches of at most 50', async () => {
  const rows = Array.from({ length: 123 }, (_, i) => chatRow(`read-${i}`));
  const h = chatListHarness({ talkReads: Object.fromEntries(rows.map(row => [row.id, 'read-at'])) });
  const done = h.api.loadTalks();
  h.requests[0].resolve({ rows, next: null });
  await flush();
  for (const size of [50, 50, 23]) {
    const request = h.requests.at(-1);
    const ids = requestedIds(request);
    assert.equal(ids.length, size);
    assert.equal(Object.keys(JSON.parse(request.options.headers['x-talk-reads'])).length, size);
    request.resolve(ids.map(id => ({ ...chatRow(id), unread_count: 0 })));
    await flush();
  }
  await done;
  assert.equal(h.state.talks.length, 123);
  assert.ok(h.state.talks.every(row => row.unread_count === 0));
});

test('expired chat cursors merge the renewed open page and continue to new closed history', async () => {
  const h = chatListHarness();
  const initial = h.api.loadTalks();
  h.requests[0].resolve({ rows: [chatRow('known-open'), chatRow('known-closed', 'closed')], next: 'expired' });
  await initial;
  const older = h.api.loadOlderTalks();
  h.requests[1].reject(Object.assign(new Error('expired'), { status: 409 }));
  await flush();
  h.requests[2].resolve({ rows: [chatRow('new-open'), { ...chatRow('known-open'), version: 2 }, chatRow('known-closed', 'closed')], next: 'restarted' });
  await flush();
  assert.equal(h.requests[3].url, '/api/talks/page?before=restarted');
  h.requests[3].resolve({ rows: [chatRow('older-closed', 'closed')], next: null });
  await older;
  assert.equal(h.state.talks.find(row => row.id === 'known-open').version, 2);
  assert.deepEqual(new Set(h.state.talks.map(row => row.id)), new Set(['known-open', 'new-open', 'known-closed', 'older-closed']));
  assert.equal(h.state.talks.length, 4);
});

test('chat reload revalidates loaded history and open/closed transitions without dropping history', async () => {
  const history = Array.from({ length: 63 }, (_, i) => chatRow(`closed-${i}`, 'closed'));
  const h = chatListHarness({ talks: [chatRow('now-closed'), ...history] });
  const done = h.api.loadTalks();
  h.requests[0].resolve({ rows: [chatRow('closed-62'), chatRow('new-open')], next: 'renewed' });
  await flush();
  for (const size of [50, 13]) {
    const request = h.requests.at(-1);
    assert.equal(requestedIds(request).length, size);
    request.resolve(requestedIds(request).filter(id => id !== 'closed-0' && id !== 'closed-1').map(id => chatRow(id, 'closed')));
    await flush();
  }
  await done;
  assert.equal(h.state.talks.length, 63);
  assert.equal(h.state.talks.find(row => row.id === 'now-closed').status, 'closed');
  assert.equal(h.state.talks.find(row => row.id === 'closed-62').status, 'open');
  assert.ok(!h.state.talks.some(row => ['closed-0', 'closed-1'].includes(row.id)), 'deleted and unreadable history disappear');
  assert.equal(new Set(h.state.talks.map(row => row.id)).size, h.state.talks.length);
});

for (const pageFirst of [false, true]) test(`chat reload supersedes an older cursor response (pageFirst=${pageFirst})`, async () => {
  const h = chatListHarness();
  const initial = h.api.loadTalks();
  h.requests[0].resolve({ rows: [chatRow('known')], next: 'old' });
  await initial;
  const older = h.api.loadOlderTalks();
  const reload = h.api.loadTalks();
  const oldPage = { rows: [chatRow('deleted')], next: 'stale' };
  const freshPage = { rows: [chatRow('known'), chatRow('new-open')], next: 'fresh' };
  if (pageFirst) {
    h.requests[1].resolve(oldPage);
    await older;
    h.requests[2].resolve(freshPage);
    await reload;
  } else {
    h.requests[2].resolve(freshPage);
    await reload;
    h.requests[1].resolve(oldPage);
    await older;
  }
  assert.deepEqual(new Set(h.state.talks.map(row => row.id)), new Set(['known', 'new-open']));
  const next = h.api.loadOlderTalks();
  assert.equal(h.requests.at(-1).url, '/api/talks/page?before=fresh');
  h.requests.at(-1).resolve({ rows: [], next: null });
  await next;
});

test('a chat delta prevents stale search hydration from restoring deleted or closed rows', async () => {
  const h = chatListHarness({ talks: [], rev: { talks: 'r0' }, search: { chats: { seq: 0 } } });
  const search = h.api.runSearch('chats');
  h.requests[0].resolve({ hits: [{ id: 'deleted' }, { id: 'updated' }], total: 2 });
  await flush();
  const delta = h.api.loadTalks({ rev: 'r1', delta: { base: 'r0', changed: ['updated'], removed: ['deleted'] } });
  h.requests[2].resolve([chatRow('updated', 'closed')]);
  await delta;
  h.requests[1].resolve([chatRow('deleted'), chatRow('updated')]);
  await flush();
  assert.equal(h.requests[3].url, '/api/search', 'the invalidated search retries');
  h.requests[3].resolve({ hits: [{ id: 'updated' }], total: 1 });
  await search;
  assert.equal(h.state.talks.length, 1);
  assert.equal(h.state.talks[0].status, 'closed');
  assert.equal(h.state.search.chats.total, 1);
});

test('expired chat cursor recovery revalidates retained rows and respects a concurrent delta', async () => {
  const h = chatListHarness();
  const initial = h.api.loadTalks({ rev: 'r0' });
  h.requests[0].resolve({ rows: ['deleted', 'corrupt', 'closed-now', 'updated'].map(id => chatRow(id)), next: 'expired' });
  await initial;
  const older = h.api.loadOlderTalks();
  h.requests[1].reject(Object.assign(new Error('expired'), { status: 409 }));
  await flush();
  h.requests[2].resolve({ rows: [chatRow('new-open'), chatRow('new-closed', 'closed')], next: 'older' });
  await flush();
  assert.deepEqual(requestedIds(h.requests[3]), ['deleted', 'corrupt', 'closed-now', 'updated']);
  const delta = h.api.loadTalks({ rev: 'r1', delta: { base: 'r0', changed: ['updated'], removed: [] } });
  h.requests[4].resolve([{ ...chatRow('updated'), version: 2 }]);
  await delta;
  h.requests[3].resolve([chatRow('closed-now', 'closed'), { ...chatRow('updated', 'closed'), version: 1 }]);
  await older;
  assert.equal(h.state.talks.length, 4);
  assert.ok(!h.state.talks.some(row => ['deleted', 'corrupt'].includes(row.id)));
  assert.equal(h.state.talks.find(row => row.id === 'closed-now').status, 'closed');
  assert.equal(h.state.talks.find(row => row.id === 'updated').version, 2);
  assert.equal(new Set(h.state.talks.map(row => row.id)).size, h.state.talks.length);
});

test('joined conversation reads share enrichment even if it completes before a second paint callback', async () => {
  const h = harness({ manualPaint: true });
  const done = Promise.all([h.api.loadTalk('a'), h.api.loadTalk('a')]);
  assert.equal(h.requests.length, 1);
  h.requests[0].resolve({id: 'a', updated_at: 'now', turns: []});
  await flush();
  let answeredSections = false;
  // Browser frame callbacks schedule separate tasks. An immediate sections
  // response may complete between them, before the second joined caller resumes.
  while (h.frames.length) {
    h.frames.shift()();
    await flush();
    if (!answeredSections && h.requests[1]) {
      h.requests[1].resolve({context_for: 'now', roster: [], personas: [], tasks: ['related']});
      answeredSections = true;
      await flush();
    }
  }
  assert.equal(h.requests.length, 2, 'joined reads must not issue another sections request after enrichment already finished');
  await done;
  assert.deepEqual(h.state.talkDetail.talk.tasks, ['related']);
});
