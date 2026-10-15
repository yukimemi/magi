// Exercise the actual approval controls without a browser dependency.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

function controls() {
  const source = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8');
  const functions = source.slice(source.indexOf('function choiceNamed('), source.indexOf('/* ---- question card'));
  const answers = [];
  const el = (tag, props = {}, ...children) => ({ tag, ...props, children, append(...kids) { this.children.push(...kids); }, addEventListener(event, handler) { this[event] = handler; }, focus() {} });
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
  assert.match(all(row.refs.stakes).find(n => n.class === 'hint' && n.text.includes('does not approve a merge')).text, /does not approve a merge/);
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


test('replacement requires an explicit selection and withdrawal requires a second confirmation', () => {
  const { row, q, render, answers, button, all } = controls();
  q.merge_target = { pr: 'https://github.com/example/repo/pull/42', head: 'abc123', language: 'en' };
  q.choices.push('withdraw: close PR and end task', 'reimplement:alpha', 'reimplement:beta');
  render(row, q);
  button('Reimplement with selected agent').onclick();
  assert.equal(answers.length, 0);
  const select = all(row.refs.stakes).find(n => n.tag === 'select');
  select.value = 'reimplement:beta'; select.change();
  button('Reimplement with selected agent').onclick();
  assert.equal(answers[0].choice, 'reimplement:beta');
  button('Withdraw: close PR and end task…').onclick();
  assert.equal(answers.length, 1);
  button('Cancel withdrawal').onclick();
  assert.equal(button('Confirm: close PR and end task'), undefined);
  button('Withdraw: close PR and end task…').onclick();
  button('Confirm: close PR and end task').onclick();
  assert.equal(answers[1].choice, 'withdraw: close PR and end task');
});

test('Japanese card distinguishes stopping, repair and task withdrawal', () => {
  const { row, q, render, button } = controls();
  q.merge_target = { pr: 'PR 42', head: 'abc', language: 'ja' };
  q.choices.push('withdraw: close PR and end task', 'reimplement:alpha');
  render(row, q);
  for (const label of ['判断を保留して作業を停止', '指摘を修正して再レビュー',
    '取り下げ：PR を閉じてタスクを終了…', '選択した agent で再実装',
    '指摘を確認して、この head のマージを承認…']) assert.ok(button(label), label);
});


test('replacement groups its selector, action and explanation and retains selection on redraw', () => {
  const { row, q, render, answers, all } = controls();
  q.choices.push('reimplement:alpha', 'reimplement:beta');
  render(row, q);
  const block = row.refs.stakes.children.find(n => n.class === 'stakes-action'
    && all(n).some(child => child.tag === 'select'));
  assert.ok(block);
  const agentRow = block.children.find(n => n.class === 'stakes-agent-row');
  assert.deepEqual(agentRow.children.map(n => n.tag), ['select', 'button']);
  assert.ok(block.children.some(n => n.class === 'hint' && n.text.includes('Only the selected implementer')));
  const select = agentRow.children[0];
  assert.equal(select.class, 'stakes-agent-select');
  assert.deepEqual(select.children.map(n => n.value), ['', 'reimplement:alpha', 'reimplement:beta']);
  select.value = 'reimplement:beta';
  select.change();
  render(row, q);
  assert.equal(all(row.refs.stakes).find(n => n.tag === 'select').value, 'reimplement:beta');
  assert.equal(answers.length, 0);
});
