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
