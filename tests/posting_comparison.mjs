// Exercise the shipped renderer without a browser package or dependency.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

class Node {
  constructor(tag = '#text', text = '') {
    this.tag = tag;
    this.children = [];
    this.content = text;
    this.style = { setProperty: (key, value) => { this[key] = value; } };
  }
  append(...children) { this.children.push(...children); }
  set textContent(text) { this.content = text; this.children = []; }
  get textContent() { return this.content + this.children.map(n => n.textContent).join(''); }
  set innerHTML(_) { throw new Error('HTML parsing is forbidden'); }
  setAttribute() {}
}

const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
const el = source.slice(source.indexOf('function el('), source.indexOf('/* Where a task', source.indexOf('function el(')));
const renderer = source.slice(source.indexOf('function renderPostingComparison('), source.indexOf('function updateAskCard('));
const render = runInNewContext(`${el}\n${renderer}\nrenderPostingComparison`, {
  document: { createElement: tag => new Node(tag), createTextNode: text => new Node('#text', text) },
  append: (parent, kids) => parent.append(...kids),
  // The production Markdown renderer clears its destination before rendering.
  renderMd: (parent, nodes) => { parent.children = []; parent.textContent = nodes.join(''); },
});

const descend = node => [node, ...node.children.flatMap(descend)];
test('posting pairs render literal text, matching markers, distinct colors and removal', () => {
  const attack = '<img src=x onerror=alert(1)> & " </pre><script>alert(1)</script>';
  const root = new Node('div');
  render(root, {
    before_md: ['Before'], after_md: ['After'],
    fields: [{ name: 'Description', pairs: [[1, 'sensitive-data: ip'], [2, attack]],
      original: [{ text: attack, pair: null }, { text: '192.0.2.1', pair: 1 }, { text: ' remove', pair: 2 }],
      changed: [{ text: attack, pair: null }, { text: '[redacted-ip]', pair: 1 }, { text: '', pair: 2 }],
    }],
  });
  const nodes = descend(root);
  assert.equal(root.children[0].textContent, 'Before');
  assert.equal(root.children.at(-1).textContent, 'After');
  assert.equal(nodes.filter(n => n.tag === 'section').length, 1);
  assert.equal(nodes.filter(n => n.tag === 'pre').length, 2);
  assert.equal(nodes.filter(n => n.textContent === '#1').length, 3);
  assert.equal(nodes.filter(n => n.textContent === '#2').length, 3);
  assert.equal(nodes.filter(n => n.tag === 'img' || n.tag === 'script').length, 0);
  assert.ok(nodes.find(n => n.tag === 'pre').textContent.startsWith(attack));
  const pairs = nodes.filter(n => n.className === 'posting-pair');
  assert.equal(pairs[0]['--pair-hue'], pairs[2]['--pair-hue']);
  assert.notEqual(pairs[0]['--pair-hue'], pairs[1]['--pair-hue']);
  assert.ok(nodes.some(n => n.className === 'posting-removed' && n.textContent === ' (removed)'));
});


test('whole-field fallback lists individually highlighted original triggers as group members', () => {
  const root = new Node('div');
  render(root, {
    before_md: [], after_md: [],
    fields: [{ name: 'Description', replacement_group: { id: 1, triggers: [2, 3] },
      pairs: [[1, 'body-language: whole field replaced'],
        [2, 'sensitive-data: included in whole-field replacement #1'],
        [3, 'sensitive-data: included in whole-field replacement #1']],
      original: [{ text: '日本語の説明です。 First key: ', pair: null },
        { text: 'secret-one', pair: 2 }, { text: '\nMore context. Second key: ', pair: null },
        { text: 'secret-two', pair: 3 }],
      changed: [{ text: 'Neutral fallback.', pair: 1 }],
    }],
  });
  const nodes = descend(root);
  const pres = nodes.filter(n => n.tag === 'pre');
  assert.equal(pres.length, 2);
  assert.ok(pres[0].textContent.includes('#2secret-one'));
  assert.ok(pres[0].textContent.includes('#3secret-two'));
  assert.equal(pres[1].textContent, '#1Neutral fallback.');
  const headings = nodes.filter(n => n.className === 'posting-label');
  assert.ok(headings[0].textContent.includes('whole-field replacement #1'));
  assert.ok(headings[1].textContent.includes('includes original triggers #2#3'));
  for (const id of [2, 3]) {
    const originalSpan = pres[0].children.find(n => n.children[0]?.textContent === `#${id}`);
    const groupBadge = headings[1].children.find(n => n.children[0]?.textContent === `#${id}`);
    const rule = nodes.find(n => n.tag === 'li' && n.children[0]?.textContent === `#${id}`);
    assert.equal(originalSpan['--pair-hue'], groupBadge['--pair-hue']);
    assert.equal(originalSpan['--pair-hue'], rule['--pair-hue']);
  }
});
