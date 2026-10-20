import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const loaders = source.slice(source.indexOf('let questionOriginGeneration ='), source.indexOf('/* ---- notification centre'));
const flush = () => new Promise(resolve => setImmediate(resolve));
function harness() {
  const requests = [], notices = [], renders = [];
  const state = { questions: [], route: { name: 'questions' }, detail: {} };
  const getJson = (url, options) => new Promise((resolve, reject) => requests.push({ url, options, resolve, reject }));
  const api = runInNewContext(`${loaders}\n({loadQuestions})`, {
    state, API: { questions: '/api/questions' }, AbortController, getJson,
    loadStore: (name, url, change, apply) => getJson(url).then(apply),
    sortQuestions: qs => qs, renderQuestions: () => renders.push(structuredClone(state.questions)),
    renderAskBar() {}, renderRuns() {}, renderQueue() {}, renderRunDetail() {}, renderQuestionOrigins() {},
    sectionNotice: (...args) => notices.push(args),
  });
  return { api, state, requests, renders, notices };
}
const core = (revision = 'one') => ({ id: 'q', record_revision: revision, summary: 'Useful question', can_consult_chat: false });
const origin = (revision = 'one') => ({ id: 'q', record_revision: revision, origin_chat: 'chat', can_consult_chat: true });
test('question content arrives independently of slow and failing origins, with retry', async () => {
  const h = harness();
  const done = h.api.loadQuestions();
  assert.equal(h.requests[0].url, '/api/questions/core');
  h.requests[0].resolve([core()]);
  await done;
  assert.equal(h.renders[0][0].summary, 'Useful question');
  assert.equal(h.state.questions[0].can_consult_chat, false);
  assert.equal(h.requests[1].url, '/api/questions/origins');
  h.requests[1].reject(new Error('queue unavailable'));
  await flush();
  assert.equal(h.state.questions[0].summary, 'Useful question');
  assert.ok(h.notices.some(n => n[2].includes('queue unavailable') && typeof n[3] === 'function'));
});
test('old origins cannot overwrite a refreshed question or expose stale consultation', async () => {
  const h = harness();
  const first = h.api.loadQuestions();
  h.requests[0].resolve([core('one')]);
  await first;
  const second = h.api.loadQuestions();
  assert.equal(h.requests[1].options.signal.aborted, true);
  h.requests[2].resolve([core('two')]);
  await second;
  h.requests[3].resolve([origin('two')]);
  await flush();
  assert.equal(h.state.questions[0].can_consult_chat, true);
  assert.equal(h.renders.length, 2, 'origin enrichment does not rebuild answer controls');
  h.requests[1].resolve([{ ...origin('one'), origin_chat: 'stale' }]);
  await flush();
  assert.equal(h.state.questions[0].origin_chat, 'chat');
  assert.equal(h.state.questions[0].record_revision, 'two');
});
test('question changes during origin lookup leave consultation disabled and offer retry', async () => {
  const h = harness();
  const done = h.api.loadQuestions();
  h.requests[0].resolve([core('one')]);
  await done;
  h.requests[1].resolve([origin('two')]);
  await flush();
  assert.equal(h.state.questions[0].can_consult_chat, false);
  assert.ok(h.notices.some(n => n[2].includes('Questions changed') && typeof n[3] === 'function'));
});
test('an empty question store never asks for task/chat origins', async () => {
  const h = harness();
  const done = h.api.loadQuestions();
  h.requests[0].resolve([]);
  await done;
  assert.equal(h.requests.length, 1);
});

test('background refresh neither clears nor sets the origins notice', async () => {
  const h = harness();
  const first = h.api.loadQuestions();
  h.requests[0].resolve([core()]);
  await first;
  assert.equal(h.notices.length, 1);
  assert.equal(h.notices[0][2], 'Loading question origins…');
  h.requests[1].resolve([origin()]);
  await flush();
  const notices = h.notices.slice();
  const refresh = h.api.loadQuestions();
  assert.deepEqual(h.notices, notices);
  h.requests[2].resolve([core()]);
  await refresh;
  assert.deepEqual(h.notices, notices, 'no notice is cleared or set while origins refresh');
  h.requests[3].resolve([origin()]);
  await flush();
});

test('unchanged core rows retain origins until refreshed chat state arrives', async () => {
  const h = harness();
  const first = h.api.loadQuestions();
  h.requests[0].resolve([core()]);
  await first;
  h.requests[1].resolve([{ ...origin(), origin_chat_closed: false }]);
  await flush();

  const refresh = h.api.loadQuestions();
  h.requests[2].resolve([core()]);
  await refresh;
  const row = h.renders.at(-1)[0];
  assert.equal(row.origin_chat, 'chat');
  assert.equal(row.origin_chat_closed, false);
  assert.equal(row.can_consult_chat, true, 'retained eligibility overrides the core default');
  assert.equal(h.requests[3].url, '/api/questions/origins');
  h.requests[3].resolve([{ ...origin(), origin_chat: null, origin_chat_closed: true, can_consult_chat: false }]);
  await flush();
  assert.equal(h.state.questions[0].origin_chat, null);
  assert.equal(h.state.questions[0].origin_chat_closed, true);
  assert.equal(h.state.questions[0].can_consult_chat, false);

  const next = h.api.loadQuestions();
  h.requests[4].resolve([core()]);
  await next;
  assert.equal(h.renders.at(-1)[0].origin_chat, null);
  assert.equal(h.renders.at(-1)[0].origin_chat_closed, true);
  assert.equal(h.renders.at(-1)[0].can_consult_chat, false);
  h.requests[5].resolve([origin()]);
  await flush();
});

test('changed revisions and new ids do not inherit stale origin controls', async () => {
  const h = harness();
  h.state.questions = [{ ...origin(), origin_chat_closed: true }];
  const done = h.api.loadQuestions();
  h.requests[0].resolve([core('two'), { ...core(), id: 'new' }]);
  await done;
  for (const row of h.renders.at(-1)) {
    assert.equal(row.origin_chat, undefined);
    assert.equal(row.origin_chat_closed, undefined);
    assert.equal(row.can_consult_chat, false);
  }
  h.requests[1].resolve([origin('two'), { ...origin(), id: 'new' }]);
  await flush();
});

test('retry retains errors and revision warnings until the current lookup finishes', async () => {
  const h = harness();
  const first = h.api.loadQuestions();
  h.requests[0].resolve([core()]);
  await first;
  h.requests[1].reject(new Error('queue unavailable'));
  await flush();
  const errorNotice = h.notices.at(-1);
  const retry = errorNotice[3]();
  assert.equal(h.notices.at(-1), errorNotice);
  h.requests[2].resolve([core()]);
  await retry;
  assert.equal(h.notices.at(-1), errorNotice, 'retry must not replace errors with Loading');
  h.requests[3].resolve([origin('two')]);
  await flush();
  const changedNotice = h.notices.at(-1);
  assert.equal(changedNotice[2], 'Questions changed while loading origins. Retry.');
  const retryChanged = changedNotice[3]();
  h.requests[4].resolve([core('two')]);
  await retryChanged;
  assert.equal(h.notices.at(-1), changedNotice);
  h.requests[5].resolve([origin('two')]);
  await flush();
  assert.equal(h.notices.at(-1)[2], '');
  assert.equal(h.notices.filter(n => n[2] === 'Loading question origins…').length, 1);
});

test('superseded origin failures cannot change notices and an empty refresh clears them', async () => {
  const h = harness();
  const first = h.api.loadQuestions();
  h.requests[0].resolve([core()]);
  await first;
  const second = h.api.loadQuestions();
  h.requests[2].resolve([core()]);
  await second;
  h.requests[3].resolve([origin('two')]);
  await flush();
  const notice = h.notices.at(-1);
  h.requests[1].reject(new Error('stale failure'));
  await flush();
  assert.equal(h.notices.at(-1), notice);
  const empty = h.api.loadQuestions();
  assert.equal(h.notices.at(-1), notice);
  h.requests[4].resolve([]);
  await empty;
  assert.equal(h.notices.at(-1)[2], '');
  assert.equal(h.requests.length, 5);
});
