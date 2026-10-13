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
  const helpers = source.slice(source.indexOf('async function copyText('), source.indexOf('/* ---- agent-authored panels'));
  const navigator = {};
  const selection = {
    ranges: [],
    get node() { return this.ranges[0]?.node ?? null; },
    get rangeCount() { return this.ranges.length; },
    getRangeAt(i) { return this.ranges[i]; },
    removeAllRanges() { this.ranges = []; },
    addRange(range) { this.ranges.push(range); },
  };
  const attempts = [];
  const window = { isSecureContext: true, getSelection: () => selection, scrollX: 4, scrollY: 80,
    scrollTo(x, y) { this.scrollX = x; this.scrollY = y; } };
  const document = {
    body: new Node('body'), activeElement: null,
    createElement(tag) {
      const node = new Node(tag);
      node.style = {};
      node.focus = options => { node.focusOptions = options; document.activeElement = node; };
      node.select = () => { node.selected = true; };
      node.setSelectionRange = (start, end, direction) => {
        node.selectionStart = start; node.selectionEnd = end; node.selectionDirection = direction;
      };
      node.remove = () => { document.body.children = document.body.children.filter(child => child !== node); };
      return node;
    },
    createRange: () => ({ selectNodeContents(node) { this.node = node; }, cloneRange() { return { ...this }; } }),
    execCommand(command) { attempts.push({ command, field: document.activeElement }); return false; },
  };
  document.body.appendChild = node => document.body.append(node);
  const api = runInNewContext(`${el}\n${short}\n${helpers}\n({ createQuestionIdentity, updateQuestionIdentity, copyText })`, {
    document, navigator, window,
    append: (node, kids) => node.append(...kids),
    setText: (node, text) => { node.textContent = text; },
    setAttr: (node, key, value) => node.setAttribute(key, value),
  });
  const root = api.createQuestionIdentity();
  return { ...api, navigator, selection, document, window, attempts, root, copy: root.children[1].children[1] };
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
  const { root, navigator, selection, attempts, copy, updateQuestionIdentity } = component();
  updateQuestionIdentity(root, id);
  let copied;
  navigator.clipboard = { writeText: async text => { copied = text; } };
  await copy.click();
  assert.equal(copied, id);
  assert.equal(attempts.length, 0);
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

for (const mode of ['HTTP', 'API absent', 'API rejects']) {
  test(`${mode} copies synchronously through a temporary readonly textarea and restores state`, async () => {
    const { root, navigator, window, document, selection, copy, updateQuestionIdentity } = component();
    updateQuestionIdentity(root, id);
    const input = document.createElement('textarea');
    input.value = 'draft'; input.setSelectionRange(1, 3, 'backward'); input.focus();
    input.scrollLeft = 2; input.scrollTop = 7;
    const range = document.createRange(); range.selectNodeContents(root.refs.label);
    selection.addRange(range);
    let modernCalls = 0, legacyCalls = 0;
    if (mode !== 'API absent') navigator.clipboard = { writeText: async () => {
      modernCalls++; throw new Error('denied');
    } };
    if (mode === 'HTTP') window.isSecureContext = false;
    document.execCommand = command => {
      legacyCalls++;
      assert.equal(command, 'copy');
      const field = document.activeElement;
      assert.equal(field.value, id);
      assert.equal(field.readOnly, true);
      assert.equal(field.attributes.inputmode, 'none');
      assert.match(field.style.cssText, /position:fixed.*opacity:0.*font-size:16px/);
      assert.equal(field.focusOptions.preventScroll, true);
      assert.equal(field.selected, true);
      assert.equal(field.selectionStart, 0);
      assert.equal(field.selectionEnd, id.length);
      // Simulate a browser disturbing both selection and scrolling.
      input.setSelectionRange(0, 0); selection.removeAllRanges();
      input.scrollTop = 100; window.scrollY = 200;
      return true;
    };
    const pending = copy.click();
    if (mode !== 'API rejects') assert.equal(legacyCalls, 1, 'fallback must stay in the click');
    await pending;
    assert.equal(modernCalls, mode === 'API rejects' ? 1 : 0);
    assert.equal(legacyCalls, 1);
    assert.equal(root.refs.status.textContent, 'Copied full ID.');
    assert.equal(document.body.children.length, 0);
    assert.equal(document.activeElement, input);
    assert.equal(input.focusOptions.preventScroll, true);
    assert.deepEqual([input.selectionStart, input.selectionEnd, input.selectionDirection], [1, 3, 'backward']);
    assert.equal(selection.node, root.refs.label);
    assert.equal(input.scrollTop, 7);
    assert.equal(window.scrollY, 80);
  });
}

for (const outcome of [false, 'true', 'throws', 'unavailable']) {
  test(`legacy ${JSON.stringify(outcome)} is a total failure with cleanup and manual guidance`, async () => {
    const { root, document, copy, selection, updateQuestionIdentity } = component();
    updateQuestionIdentity(root, id);
    const input = document.createElement('textarea');
    input.setSelectionRange(1, 2, 'forward'); input.focus();
    if (outcome === 'unavailable') document.execCommand = undefined;
    else document.execCommand = () => {
      if (outcome === 'throws') throw new Error('blocked');
      return outcome;
    };
    await copy.click();
    assert.equal(document.body.children.length, 0);
    assert.equal(document.activeElement, input);
    assert.deepEqual([input.selectionStart, input.selectionEnd], [1, 2]);
    assert.match(root.refs.status.textContent, /Could not copy.*Select the full ID/);
    assert.equal(selection.node, root.refs.full);
  });
}
