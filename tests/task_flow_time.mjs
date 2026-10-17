// Exercise the shipped flow renderer with browser-local date formatting.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const flowSource = source.slice(source.indexOf('const FLOW_START ='), source.indexOf('/* ---- run actions sheet'));
function harness(locale, timeZone) {
  const nodes = new Map();
  const el = (tag, attrs = {}, ...children) => ({ tag, ...attrs, children,
    append(...more) { this.children.push(...more); } });
  const $ = (id) => {
    if (!nodes.has(id)) nodes.set(id, el('div'));
    return nodes.get(id);
  };
  let options;
  const api = runInNewContext(`${flowSource}\n({ flowStartTime, renderTaskFlow })`, {
    Intl: { DateTimeFormat: function (_, opts) {
      options = opts;
      return new Intl.DateTimeFormat(locale, { ...opts, timeZone });
    } },
    $, el, clear: n => { n.children = []; }, setText: (n, text) => { n.text = text; },
    plural: (n, one, many) => `${n} ${n === 1 ? one : many}`,
    chip: text => el('span', { text }), TASK_STATUS: {}, RUN_STATUS: {},
  });
  return { api, $, options };
}

test('start labels contain date, 24-hour clock and browser timezone across locales', () => {
  for (const [locale, timeZone] of [['en-US', 'America/New_York'], ['ja-JP', 'Asia/Tokyo']]) {
    const h = harness(locale, timeZone);
    const started_at = '2026-10-18T04:16:07Z';
    const node = h.api.flowStartTime({ time_expected: true, started_at });
    assert.equal(node.tag, 'time');
    assert.equal(node.datetime, started_at);
    const formatter = new Intl.DateTimeFormat(locale, { ...h.options, timeZone });
    const parts = formatter.formatToParts(new Date(started_at));
    for (const type of ['year', 'month', 'day', 'hour', 'minute', 'second', 'timeZoneName']) {
      const part = parts.find(p => p.type === type);
      assert.ok(part, type);
      assert.ok(node.text.includes(part.value), type);
    }
    assert.equal(h.options.hourCycle, 'h23');
    assert.ok(!parts.some(p => p.type === 'dayPeriod'));
  }
});

test('missing and invalid starts are explicit without a datetime attribute', () => {
  const { api } = harness('en-US', 'UTC');
  for (const started_at of [null, undefined, '', 'invalid', 0, '2026-99-99T00:00:00Z']) {
    const node = api.flowStartTime({ time_expected: true, started_at });
    assert.equal(node.text, 'Start time not recorded');
    assert.equal(node.tag, 'div');
    assert.equal(node.datetime, undefined);
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
  assert.equal(box.href, '#/runs/run-id');
  assert.equal(box.children[0].children[1].text, 'gating');
  assert.equal(box.children[0].children[2].text, 'competition');
  assert.equal(box.children[1].text, 'Start time not recorded');
});
