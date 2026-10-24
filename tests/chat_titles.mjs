// Exercise the title shared by conversation cards and detail headings.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const opener = source.slice(source.indexOf('function talkOpener('), source.indexOf('/* ---- unread'));
const firstLine = source.slice(source.indexOf('function firstLine('), source.indexOf('/* `advise::Reflection`'));
const title = runInNewContext(`${opener}\n${firstLine}\n(talk) => firstLine(talkOpener(talk))`, {
  talkTurns: talk => talk.turns || [],
});

test('saved Japanese titles name cards and detail headings, preserving the legacy fallback', () => {
  const talk = { turns: [{ who: 'operator', body: '\n 最初の話題\n本文' }] };
  assert.equal(title(talk), '最初の話題');
  for (const source of ['generated', 'manual']) {
    assert.equal(title({ ...talk, title: { text: '最近の話題', source } }), '最近の話題');
  }
  assert.equal(title({ turns: [] }), '');
  assert.match(source, /setText\(r\.title, firstLine\(talkOpener\(talk\)\)/);
  assert.match(source, /setText\(\$\("talk-h"\), firstLine\(talkOpener\(talk\)\)/);
});
