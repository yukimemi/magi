// Exercise the actual approval controls without a browser dependency.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

function controls() {
  const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
  const functions = source.slice(source.indexOf('function choiceNamed('), source.indexOf('/* ---- question card'));
  const answers = [];
  const el = (tag, props = {}, ...children) => ({ tag, ...props, children, append(...kids) { this.children.push(...kids); }, focus() {} });
  const render = runInNewContext(`${functions}\nrenderStakes`, {
    el, clear: node => { node.children = []; }, requestAnimationFrame: f => f(),
    answerQuestion: (id, answer) => answers.push({ id, ...answer }),
  });
  const row = { dataset: {}, refs: { stakes: el('div'), consultBox: el('div') }, insertBefore() {} };
  const q = { id: 'question', choices: ['merge', 'hold', 'fix findings and re-review'] };
  render(row, q);
  const all = node => [node, ...(node.children || []).flatMap(all)];
  const button = text => all(row.refs.stakes).find(n => n.tag === 'button' && n.text === text);
  return { row, q, render, answers, button, all };
}

test('repair and hold send distinct choices immediately; merge still needs confirmation', () => {
  const { row, answers, button, all } = controls();
  button('fix findings and re-review').onclick();
  assert.deepEqual(answers, [{ id: 'question', choice: 'fix findings and re-review' }]);
  assert.match(all(row.refs.stakes).find(n => n.class === 'hint').text, /does not approve a merge/);
  button('Hold — do not merge').onclick();
  assert.equal(answers[1].choice, 'hold');
  button('Merge this pull request…').onclick();
  assert.equal(answers.length, 2);
  button('Yes, merge now').onclick();
  assert.equal(answers[2].choice, 'merge');
});

test('an upgraded card shows repair without changing the merge guard', () => {
  const { row, q, render, button } = controls();
  q.choices = ['merge', 'hold'];
  render(row, q);
  assert.equal(button('fix findings and re-review'), undefined);
  q.choices.push('fix findings and re-review');
  render(row, q);
  assert.ok(button('fix findings and re-review'));
  assert.equal(button('Yes, merge now'), undefined);
});
