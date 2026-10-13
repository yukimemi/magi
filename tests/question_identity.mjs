// Exercise the shipped identity component without a browser dependency.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

class Node {
  constructor(tag) {
    this.tag = tag;
    this.children = [];
    this.dataset = {};
    this.attributes = {};
    this.listeners = {};
    this.open = false;
  }
  append(...kids) { this.children.push(...kids); }
  setAttribute(key, value) { this.attributes[key] = value; }
  addEventListener(event, handler) { this.listeners[event] = handler; }
  click() { return this.listeners.click(); }
}

function component() {
  const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
  const el = source.slice(source.indexOf('function el('), source.indexOf('/* Where a task'));
  const short = source.match(/^const shortId = .*$/m)[0];
  const helpers = source.slice(source.indexOf('function createQuestionIdentity('), source.indexOf('/* ---- agent-authored panels'));
  const navigator = {};
  const selection = {
    node: null,
    removeAllRanges() { this.node = null; },
    addRange(range) { this.node = range.node; },
  };
  const api = runInNewContext(`${el}\n${short}\n${helpers}\n({ createQuestionIdentity, updateQuestionIdentity })`, {
    document: {
      createElement: tag => new Node(tag),
      createRange: () => ({ selectNodeContents(node) { this.node = node; } }),
    }, navigator, window: { getSelection: () => selection },
    append: (node, kids) => node.append(...kids),
    setText: (node, text) => { node.textContent = text; },
    setAttr: (node, key, value) => node.setAttribute(key, value),
  });
  const root = api.createQuestionIdentity();
  return { ...api, navigator, selection, root, copy: root.children[1].children[1] };
}

const id = '20261012-230524-5d55';
test('persisted identity remains mounted and expanded through repeated updates', () => {
  const { root, updateQuestionIdentity } = component();
  updateQuestionIdentity(root, id);
  assert.equal(root.refs.label.textContent, '#5d55');
  assert.equal(root.refs.full.textContent, id);
  assert.equal(root.refs.label.attributes['aria-label'], 'Question #5d55: show full ID');
  root.open = true;
  const full = root.refs.full;
  updateQuestionIdentity(root, id);
  assert.equal(root.open, true);
  assert.equal(root.refs.full, full);
  updateQuestionIdentity(root, 'plain');
  assert.equal(root.open, false);
  assert.equal(root.refs.label.textContent, '#plain');
  assert.equal(root.refs.full.textContent, 'plain');
});

test('copies the full question ID and only reports success after a successful write', async () => {
  const { root, navigator, selection, copy, updateQuestionIdentity } = component();
  updateQuestionIdentity(root, id);
  let copied;
  navigator.clipboard = { writeText: async text => { copied = text; } };
  await copy.click();
  assert.equal(copied, id);
  assert.equal(root.refs.status.textContent, 'Copied full ID.');
  assert.equal(selection.node, null);
  navigator.clipboard.writeText = async () => { throw new Error('denied'); };
  await copy.click();
  assert.match(root.refs.status.textContent, /Could not copy.*Select the full ID/);
  assert.equal(selection.node, root.refs.full);
  selection.removeAllRanges();
  navigator.clipboard = undefined;
  await copy.click();
  assert.match(root.refs.status.textContent, /Could not copy.*Select the full ID/);
  assert.equal(root.refs.full.textContent, id);
  assert.equal(selection.node, root.refs.full);
});

test('a pending clipboard write does not label a different question as copied', async () => {
  const { root, navigator, copy, updateQuestionIdentity } = component();
  updateQuestionIdentity(root, id);
  let resolve;
  navigator.clipboard = { writeText: () => new Promise(done => { resolve = done; }) };
  const pending = copy.click();
  updateQuestionIdentity(root, '20261012-230525-abcd');
  resolve();
  await pending;
  assert.equal(root.refs.status.textContent, '');
});

test('a pending copy failure does not select a different question', async () => {
  const { root, navigator, selection, copy, updateQuestionIdentity } = component();
  updateQuestionIdentity(root, id);
  let reject;
  navigator.clipboard = { writeText: () => new Promise((_, fail) => { reject = fail; }) };
  const pending = copy.click();
  updateQuestionIdentity(root, '20261012-230525-abcd');
  reject(new Error('denied'));
  await pending;
  assert.equal(selection.node, null);
  assert.equal(root.refs.status.textContent, '');
});
