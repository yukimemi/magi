import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const closeControls = source.slice(source.indexOf('function talkCloseReason('), source.indexOf('function renderTalk()'));
const mutations = source.slice(source.indexOf('async function changeTalkStatus('), source.indexOf('/* Armed by the talk'));
const reconcile = source.slice(source.indexOf('function reconcileTalkStatus('), source.indexOf('let olderTalksFlight'));
function harness() {
  const state = { talkDetail: { id: 'a', talk: { id: 'a', status: 'open' } }, talks: [], talkAttachments: { id: 'a', items: [] }, talkStatusRequests: new Map(), talkStatusUpdates: new Map(), talkSubmissions: new Map() };
  const nodes = new Map();
  const $ = id => { if (!nodes.has(id)) nodes.set(id, { value: '' }); return nodes.get(id); };
  const writes = [], reads = [], errors = [], messages = [], lists = [];
  const api = runInNewContext(`${closeControls}\n${mutations}\n${reconcile}\n({closeTalk,reopenTalk,renderTalkClose,reconcileTalkStatus})`, {
    state, $, Date, API: { talk: id => `/talk/${id}`, talkClose: id => `/talk/${id}/close`, talkReopen: id => `/talk/${id}/reopen` },
    setText: (node, value) => node.text = value, setAttr: (node, key, value) => node[key] = value, show: (node, value) => node.hidden = !value,
    talkAttachmentsBusy: () => false, renderTalks() {}, renderTalk() {}, acceptTalkUpdate: talk => state.talkDetail.talk = talk,
    postJson: url => new Promise((resolve, reject) => writes.push({ url, resolve, reject })),
    getJson: url => new Promise((resolve, reject) => reads.push({ url, resolve, reject })),
    loadTalks: () => new Promise((resolve, reject) => lists.push({ resolve, reject })),
    talkError: value => errors.push(value), announce: value => messages.push(value), ok() {}, failFor() {},
  });
  return { state, $, api, writes, reads, errors, messages, lists };
}
const flush = () => new Promise(resolve => setImmediate(resolve));
test('lost close response plus unreadable core reports unknown, not open', async () => {
  const h = harness();
  const done = h.api.closeTalk();
  h.writes[0].reject(new Error('lost response'));
  await flush();
  h.reads[0].reject(new Error('offline'));
  await done;
  assert.match(h.errors.at(-1), /could not be confirmed/);
  assert.equal(h.state.talkStatusRequests.size, 0);
  assert.equal(h.$('talk-close-go').disabled, false);
});
test('unsent text, attachments, pending input and unacknowledged send refuse close', async () => {
  for (const kind of ['text', 'attachment', 'pending', 'pending_messages', 'pending_attachments', 'submission']) {
    const h = harness();
    if (kind === 'text') h.$('f-talk-say').value = 'keep';
    else if (kind === 'attachment') h.state.talkAttachments.items.push({ status: 'uploading' });
    else if (kind === 'submission') h.state.talkSubmissions.set('a', 1);
    else h.state.talkDetail.talk[kind] = kind === 'pending' ? 'keep' : [{}];
    h.api.renderTalkClose();
    assert.equal(h.$('talk-close-go').disabled, true, kind);
    assert.ok(h.$('talk-close-reason').text, kind);
    await h.api.closeTalk();
    assert.equal(h.writes.length, 0, kind);
  }
});
test('reply/worker thinking alone never disables close; failed list refresh is independent', async () => {
  const h = harness();
  h.state.talkDetail.talk.thinking = true;
  h.api.renderTalkClose();
  assert.equal(h.$('talk-close-go').disabled, false);
  const done = h.api.closeTalk();
  h.writes[0].resolve({ id: 'a', status: 'closed', updated_at: '2026-11-16T10:00:00Z' });
  await done;
  assert.equal(h.state.talkDetail.talk.status, 'closed');
  assert.equal(h.state.talkStatusRequests.size, 0);
  assert.deepEqual(h.messages, ['Conversation closed.']);
  h.lists[0].reject(new Error('list unavailable'));
  await flush();
  assert.equal(h.state.talkDetail.talk.status, 'closed');
});
test('older list snapshots cannot undo confirmed close or reopen; newer server changes win', () => {
  const h = harness();
  for (const status of ['closed', 'open']) {
    h.state.talkStatusUpdates.set('a', { status, updated_at: '2026-11-16T10:00:00Z' });
    assert.equal(h.api.reconcileTalkStatus({ id: 'a', status: status === 'closed' ? 'open' : 'closed', updated_at: '2026-11-16T09:00:00Z' }).status, status);
    assert.equal(h.api.reconcileTalkStatus({ id: 'a', status: 'external', updated_at: '2026-11-16T11:00:00Z' }).status, 'external');
  }
});
