#!/usr/bin/env node
/*
 * blocked Task UI の依存なし契約検査。リポジトリのルートから次を実行する:
 *
 *   node assets/ui/blocked-task-contract.fixture.js
 *   node --check assets/ui/app.js
 *
 * 実装を複製せず、名前を挙げた関数を app.js から直接読み込む。以下の小さな DOM
 * は関数が使うブラウザ操作だけを提供するもので、視覚テストではない。
 */
"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const appPath = path.join(__dirname, "app.js");
const app = fs.readFileSync(appPath, "utf8");

function functionFromApp(name, nextName) {
  const start = app.indexOf(`function ${name}(`);
  const end = app.indexOf(`\nfunction ${nextName}(`, start);
  assert.notEqual(start, -1, `${name} は app.js に必要です`);
  assert.notEqual(end, -1, `${nextName} は ${name} の後に必要です`);
  return app.slice(start, end);
}

class Node {
  constructor(tag = "div") {
    this.tag = tag;
    this.children = [];
    this.attributes = new Map();
    this.dataset = {};
    this.hidden = false;
    this.parentNode = null;
    this._text = "";
  }

  get firstChild() { return this.children[0] || null; }
  get childElementCount() { return this.children.length; }
  get textContent() { return this._text + this.children.map((child) => child.textContent).join(""); }
  set textContent(value) { this._text = String(value); this.children = []; }
  append(...nodes) {
    for (const node of nodes.flat(Infinity)) {
      if (node === null || node === undefined || node === false || node === "") continue;
      node.parentNode = this;
      this.children.push(node);
    }
  }
  replaceChildren(...nodes) { this._text = ""; this.children = []; this.append(...nodes); }
  replaceWith(node) {
    const at = this.parentNode.children.indexOf(this);
    node.parentNode = this.parentNode;
    this.parentNode.children.splice(at, 1, node);
  }
  setAttribute(name, value) { this.attributes.set(name, String(value)); }
  getAttribute(name) { return this.attributes.get(name) || null; }
  hasAttribute(name) { return this.attributes.has(name); }
  removeAttribute(name) { this.attributes.delete(name); }
  querySelector(selector) {
    const className = selector.startsWith(".") ? selector.slice(1) : "";
    if (this.className && this.className.split(/\s+/).includes(className)) return this;
    for (const child of this.children) {
      const found = child.querySelector(selector);
      if (found) return found;
    }
    return null;
  }
}

function element(tag, props = {}, ...kids) {
  const node = new Node(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else node.setAttribute(key, value);
  }
  node.append(kids);
  return node;
}

const context = {
  TASK_STATUS: {
    held: { note: "Held. This task will not be claimed until it is released." },
    blocked: { note: "Blocked by the conductor until every listed dependency or question is resolved." },
  },
  state: { queue: [], questions: [], runs: [] },
  el: element,
  clear: (node) => node.replaceChildren(),
  show: (node, visible) => { node.hidden = !visible; },
  setText: (node, value) => { node.textContent = value == null ? "" : String(value); },
  setAttr: (node, name, value) => value == null || value === false ? node.removeAttribute(name) : node.setAttribute(name, value),
  separate: () => {},
  chip: (status) => element("span", { text: status }),
  toneOf: () => "ink",
  when: () => ({ text: "now", title: "now" }),
  plural: (count, one, many) => `${count} ${count === 1 ? one : many}`,
  shortId: (id) => String(id).split("-").pop(),
  renderMd: () => {},
  renderTaskHoldBox: () => {},
  renderTaskDoneBox: () => {},
  changePriority: () => {},
  openTaskEdit: () => {},
  deleteTask: () => {},
  requestAnimationFrame: () => {},
};

const source = [
  functionFromApp("taskBlockDetails", "renderTaskBlockDetails"),
  functionFromApp("renderTaskBlockDetails", "updateTaskCard"),
  functionFromApp("updateTaskCard", "renderTaskHoldBox"),
  functionFromApp("talkTasksSummary", "renderTalkTasks"),
  "globalThis.contract = { taskBlockDetails, renderTaskBlockDetails, updateTaskCard, talkTasksSummary };",
].join("\n");
vm.runInNewContext(source, context, { filename: appPath });

function taskRow() {
  const blockBody = element("div", { class: "task-block-body" });
  const instructionBody = element("div", { class: "instruction" });
  const attemptsParent = element("div");
  const outcomeParent = element("div");
  const attempts = element("span");
  const outcome = element("span");
  attemptsParent.append(attempts);
  outcomeParent.append(outcome);
  return {
    dataset: {},
    refs: {
      card: element("li"), chipSlot: element("span"), priority: element("span"), solo: element("span"), whenSlot: element("time"),
      title: element("h3"), source: element("span"), repo: element("span"), attempts, outcome, note: element("p"),
      blockDetails: element("details", {}, blockBody), error: element("pre"), instruction: element("details", {}, instructionBody),
      runLink: element("a"), priorityDown: element("button"), priorityUp: element("button"), editBtn: element("button"),
      holdBox: element("span"), doneBox: element("span"), deleteBox: element("span"),
    },
  };
}

context.state.queue = [{ id: "20260913-task-dependency", title: "Publish the release" }];
context.state.questions = [{ id: "20260913-question-dependency", summary: "Which registry?" }];
const blocked = {
  id: "20260913-task-main", status: "blocked", title: "Ship it", hold_reason: "stale manual hold",
  block_reason: "Wait for the release and registry decision.",
  blocked_by: ["20260913-task-dependency", "20260913-question-dependency", "missing-reference"],
  answers: [{ question: "Which registry?", answer: "crates.io" }],
};
const row = taskRow();
context.contract.updateTaskCard(row, blocked);
assert.equal(row.refs.note.textContent, context.TASK_STATUS.blocked.note, "blocked に古い hold reason を追加しない");
assert.equal(row.refs.blockDetails.hidden, false, "blocked の詳細は表示する");
assert.match(row.refs.blockDetails.textContent, /Wait for the release/);
assert.match(row.refs.blockDetails.textContent, /Task: Publish the release/);
assert.match(row.refs.blockDetails.textContent, /Question: Which registry/);
assert.match(row.refs.blockDetails.textContent, /Reference: missing-reference/);
assert.match(row.refs.blockDetails.textContent, /Which registry\?: crates\.io/);

const heldRow = taskRow();
context.contract.updateTaskCard(heldRow, { id: "held-task", status: "held", title: "Held", hold_reason: "operator approval" });
assert.match(heldRow.refs.note.textContent, /Waiting on: operator approval/, "held は手動の hold reason を保持する");
assert.equal(heldRow.refs.blockDetails.hidden, true, "held は blocked の詳細を表示しない");

assert.equal(
  context.contract.talkTasksSummary([
    { status: "running" }, { status: "blocked" }, { status: "held" },
    { status: "failed" }, { status: "queued" }, { status: "done" },
  ]),
  "6 filed · 1 running · 1 blocked · 1 held · 1 failed · 1 queued · 1 done",
  "Chat の集計は対応する全 task status を数える",
);

console.log("blocked task UI contract: ok");
