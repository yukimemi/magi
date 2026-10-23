// Execute the shipped loaders with controlled transport completion order.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const sections = source.slice(source.indexOf('const detailSections ='), source.indexOf('function sectionNotice('));
const runs = source.slice(source.indexOf('const runDetailRequests ='), source.indexOf('/* Named with a trailing underscore'));
const talks = source.slice(source.indexOf('const talkDetailRequests ='), source.indexOf('/* The server uses Done'));
const tasks = source.slice(source.indexOf('const taskDetailRequests ='), source.indexOf('function renderTask()'));
const stats = source.slice(source.indexOf('async function loadStats('), source.indexOf('/* The repository selector'));
const flush = () => new Promise(resolve => setImmediate(resolve));
function harness() {
  const requests = [];
  const renders = [];
  const notices = [];
  const failures = [];
  const successes = [];
  const state = { detail: { id: null }, runs: [], talkDetail: { id: 'a' }, talkWaits: new Map(), stats: null, queue: [], taskDetail: { id: null } };
  const get = (url, options) => new Promise((resolve, reject) => requests.push({ url, options, resolve, reject }));
  const api = runInNewContext(`${sections}\n${runs}\n${talks}\n${tasks}\n${stats}\n({loadRun, loadTalk, loadTask, loadStats, cancelDetailSections, sectionRequest})`, {
    state, $: () => null, AbortController, URLSearchParams, API: { run: id => `run/${id}`, report: id => `raw/${id}`, reportJson: id => `report/${id}`, talk: id => `talk/${id}`, stats: 'stats', task: id => `task/${id}` },
    REPORT_VIEW_SCHEMA: 1, getJson: get, getText: get,
    renderRunDetail: () => renders.push(state.detail.run), renderReportCards: () => renders.push(state.detail.reportView),
    renderTask: () => renders.push(state.taskDetail.task), renderTaskActions() {},
    renderTalk: () => renders.push(state.talkDetail.talk), renderStats: () => renders.push(state.stats),
    renderStatsQueue: counts => renders.push({ queue: counts }),
    show() {}, sectionNotice: (...args) => notices.push(args), trackTalkThinking() {}, rememberTalkChoices() {}, endTalkTurn() {},
    ok: (...args) => successes.push(args), failFor: (...args) => failures.push(args), shortId: id => id,
  });
  return { api, requests, renders, notices, failures, successes, state };
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
  h.requests[1].resolve({ id: 'a', updated_at: 'now', useful: true });
  h.requests[2].resolve({ schema: 1, sections: ['kept'] });
  await Promise.all([first, second]);
  assertNoFailures(h);
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

test('a change during a slow read schedules one trailing refresh without parallel duplicates', async () => {
  const h = harness();
  const first = h.api.loadRun('a');
  h.api.loadRun('a', { background: true });
  h.api.loadRun('a', { background: true });
  assert.equal(h.requests.length, 3);
  h.requests[0].resolve({ id: 'a', updated_at: 'now', version: 1 });
  h.requests[1].resolve({ id: 'a', updated_at: 'now', version: 1 });
  h.requests[2].resolve({ schema: 1, sections: [] });
  await flush();
  assert.equal(h.requests.length, 6);
  h.requests[3].resolve({ id: 'a', updated_at: 'now', version: 2 });
  h.requests[4].resolve({ id: 'a', updated_at: 'now', version: 2 });
  h.requests[5].resolve({ schema: 1, sections: [] });
  await first;
  assert.equal(h.state.detail.run.version, 2);
  assert.equal(h.requests.length, 6);
  assertNoFailures(h);
});

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
  const noticeSource = source.slice(source.indexOf('const sectionNoticePending ='), source.indexOf('const talkDetailRequests ='));
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

test('settings refresh keeps the existing body intact', () => {
  const body = { content: 'Saved settings' };
  const settingsSource = source.slice(source.indexOf('function renderSettings()'), source.indexOf('/* ---- task detail'));
  const render = runInNewContext(`${settingsSource}\nrenderSettings`, {
    settingsState: { data: {}, loading: true }, renderSettingsContext() {},
    $: () => body, clear: () => { body.content = ''; },
  });
  render();
  assert.equal(body.content, 'Saved settings');
});
