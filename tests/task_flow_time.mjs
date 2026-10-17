// Exercise the shipped flow renderer with browser-local date formatting.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const flowSource = source.slice(source.indexOf('const FLOW_START ='), source.indexOf('/* ---- run actions sheet'));
const domSource = source.slice(source.indexOf('function el('), source.indexOf('/* Where a task'))
  + source.slice(source.indexOf('function append('), source.indexOf('/* Writing only on change'));
function harness(locale, timeZone) {
  const nodes = new Map();
  const createElement = (tag) => ({ tag, children: [], attributes: {},
    append(...more) { this.children.push(...more); },
    setAttribute(key, value) { this.attributes[key] = value; },
    getAttribute(key) { return this.attributes[key] ?? null; },
    set textContent(value) { this.text = value; this.children = []; },
  });
  const $ = (id) => {
    if (!nodes.has(id)) nodes.set(id, createElement('div'));
    return nodes.get(id);
  };
  let options;
  const api = runInNewContext(`${domSource}\n${flowSource}\n({ flowStartTime, renderTaskFlow })`, {
    Intl: { DateTimeFormat: function (_, opts) {
      options = opts;
      return new Intl.DateTimeFormat(locale, { ...opts, timeZone });
    } },
    $, document: { createElement }, clear: n => { n.children = []; }, setText: (n, text) => { n.text = text; },
    plural: (n, one, many) => `${n} ${n === 1 ? one : many}`,
    chip: text => Object.assign(createElement('span'), { text }), TASK_STATUS: {}, RUN_STATUS: {},
  });
  return { api, $, options };
}

test('start labels contain date, 24-hour clock and browser timezone across locales', () => {
  for (const [locale, timeZone] of [['en-US', 'America/New_York'], ['ja-JP', 'Asia/Tokyo']]) {
    const h = harness(locale, timeZone);
    const started_at = '2026-10-18T04:16:07Z';
    const node = h.api.flowStartTime({ time_expected: true, started_at });
    assert.equal(node.tag, 'time');
    assert.equal(node.getAttribute('datetime'), started_at);
    const formatter = new Intl.DateTimeFormat(locale, { ...h.options, timeZone });
    const parts = formatter.formatToParts(new Date(started_at));
    for (const type of ['year', 'month', 'day', 'hour', 'minute', 'timeZoneName']) {
      const part = parts.find(p => p.type === type);
      assert.ok(part, type);
      assert.ok(node.text.includes(part.value), type);
    }
    assert.equal(h.options.hourCycle, 'h23');
    assert.ok(!parts.some(p => p.type === 'second'));
    assert.ok(!parts.some(p => p.type === 'dayPeriod'));
  }
});

test('missing and invalid starts are explicit without a datetime attribute', () => {
  const { api } = harness('en-US', 'UTC');
  for (const started_at of [null, undefined, '', 'invalid', 0, '2026-99-99T00:00:00Z']) {
    const node = api.flowStartTime({ time_expected: true, started_at });
    assert.equal(node.text, 'Start time not recorded');
    assert.equal(node.tag, 'span');
    assert.equal(node.getAttribute('datetime'), null);
  }
  for (const started_at of [null, '2026-10-18T04:16:07Z']) {
    assert.equal(api.flowStartTime({ time_expected: false, started_at }), null);
  }
});

test('flow rendering preserves links, badges, edges and attempt counts alongside times', () => {
  const { api, $ } = harness('en-US', 'UTC');
  api.renderTaskFlow({ passes: 2, runs: 1, attempts: 1, max_attempts: 2,
    nodes: [
      { key: 'start', kind: 'start', label: 'Task queued', time_expected: true, started_at: '2026-10-18T04:16:07Z' },
      { key: 'run-1', kind: 'run', label: 'Run 1234', status: 'gating', readable: true,
        run_kind: 'competition', href: '#/runs/run-id', time_expected: true, started_at: null },
    ], edges: [{ from: 'start', to: 'run-1', label: 'claimed', attempt: 'refunded' }],
  });
  assert.equal($('task-flow-summary').text, '2 passes over 1 run · 1 of 2 attempts counted');
  const [start, edge, run] = $('task-flow').children;
  assert.equal(start.children[0].children[1].tag, 'time');
  assert.equal(edge.text, 'claimed');
  assert.equal(edge.children[0].text, '↺ refunded');
  const box = run.children[0];
  assert.equal(box.tag, 'a');
  assert.equal(box.getAttribute('href'), '#/runs/run-id');
  assert.equal(box.children[0].children[1].text, 'gating');
  assert.equal(box.children[0].children[2].text, 'competition');
  assert.equal(box.children[1].text, 'Start time not recorded');
  assert.equal(box.children[1].tag, 'span');
  assert.equal(box.children[1].getAttribute('datetime'), null);
});

test('whole flow renders three times and omits labels on context nodes', () => {
  const { api, $ } = harness('en-US', 'UTC');
  const started_at = '2026-10-18T04:16:07Z';
  const nodes = ['start', 'run', 'end', 'chat', 'followup'].map((kind, i) => ({
    key: String(i), kind, label: kind, started_at, time_expected: i < 3,
  }));
  api.renderTaskFlow({ nodes, edges: [], passes: 1, runs: 1, attempts: 1, max_attempts: 2 });
  const labels = $('task-flow').children.map(li => li.children[0].children.slice(1)).flat();
  assert.equal(labels.length, 3);
  for (const li of $('task-flow').children.slice(3)) assert.equal(li.children[0].children.length, 1);
  for (const label of labels) {
    assert.equal(label.tag, 'time');
    assert.equal(label.getAttribute('datetime'), started_at);
  }
});
