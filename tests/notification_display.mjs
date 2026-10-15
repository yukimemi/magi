// Exercise the notification renderer with text nodes and the shipped URL policy.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const render = source.slice(source.indexOf('function renderNoticeDisplay('), source.indexOf('function updateTurnRow('));
const links = source.slice(source.indexOf('const URL_RE ='), source.indexOf('function setAttr('));
const node = (tag, attrs = {}, ...children) => ({ tag, ...attrs, children,
  append(...kids) { this.children.push(...kids); },
});
const display = runInNewContext(`${links}\n${render}\nrenderNoticeDisplay`, {
  el: node, URL, document: { createTextNode: text => node('#text', { text }) },
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
