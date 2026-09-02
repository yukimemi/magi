/* magi observation deck — the whole client.
 *
 * No framework and no build step, because magi ships as one Rust binary and
 * these three files are compiled into it with include_str!. A toolchain here
 * would mean a toolchain in `cargo install magi-cli`.
 *
 * Live updates come from the SSE stream at /api/events, which announces the
 * queue and run revisions. Timers are not the mechanism: the only interval in
 * this file re-reads /api/health, because a daemon that dies stops writing its
 * heartbeat and stops emitting revisions, so its death is only ever visible by
 * asking. Everything else reacts to `change`.
 *
 * All DOM is built with createElement and textContent. Nothing from the API is
 * ever interpolated into markup: a task instruction is arbitrary operator text
 * and a review finding is arbitrary agent text.
 */

/* ---- endpoints --------------------------------------------------------- *
 * Written out once so the frozen contract is checkable against one block. */
const API = {
  health: "/api/health",
  runs: (limit) => `/api/runs?limit=${limit}`,
  run: (id) => `/api/runs/${encodeURIComponent(id)}`,
  report: (id) => `/api/runs/${encodeURIComponent(id)}/report`,
  queue: "/api/queue",
  hold: (id) => `/api/queue/${encodeURIComponent(id)}/hold`,
  release: (id) => `/api/queue/${encodeURIComponent(id)}/release`,
  events: "/api/events",
};

const RUN_LIMIT = 50;
/* The daemon's own file is refreshed every 5s and is treated as dead at 30s,
   so asking twice per staleness window is enough to never show a false
   "running" for long. */
const HEALTH_MS = 10000;

/* ---- status vocabulary ------------------------------------------------- *
 * Every status carries a glyph as well as a colour. `stalled` additionally
 * gets a hatched, double-bordered chip in CSS: a panel that collapsed on
 * quota reached no verdict, and must never be skimmable as a `ready`. */
const PHASES = ["prep", "implementing", "judging", "deliberating", "voting", "reviewing", "gating"];

const RUN_STATUS = {
  prep:         { glyph: "\u25cc", tone: "ink" },
  implementing: { glyph: "\u25b8", tone: "blue", flight: true },
  judging:      { glyph: "\u25b8", tone: "blue", flight: true },
  deliberating: { glyph: "\u25b8", tone: "blue", flight: true },
  voting:       { glyph: "\u25b8", tone: "blue", flight: true },
  reviewing:    { glyph: "\u25b8", tone: "blue", flight: true },
  gating:       { glyph: "\u25b8", tone: "blue", flight: true },
  merged:       { glyph: "\u25c6", tone: "gold", note: "Winner merged." },
  ready:        { glyph: "\u25c7", tone: "teal", note: "Winner passed the gate. Merge was not requested." },
  stalled:      { glyph: "\u26a0", tone: "rust", note: "The panel collapsed on agent quota, so no verdict was recorded. The work is kept \u2014 resume it once the limits reset." },
  blocked:      { glyph: "\u2298", tone: "rust", note: "Review rounds ran out with findings still open, or the gate failed." },
  failed:       { glyph: "\u2715", tone: "ink",  note: "The graph could not complete." },
};

const TASK_STATUS = {
  queued:  { glyph: "\u25cc", tone: "ink" },
  running: { glyph: "\u25b8", tone: "blue", flight: true },
  done:    { glyph: "\u25c6", tone: "gold" },
  failed:  { glyph: "\u2715", tone: "rust" },
  held:    { glyph: "\u2016", tone: "rust", note: "Held. This task will not be claimed until it is released." },
};

const SEV_RANK = { blocker: 3, major: 2, minor: 1, nit: 0 };

/* ---- tiny DOM layer ---------------------------------------------------- */
const $ = (id) => document.getElementById(id);

function el(tag, props, ...kids) {
  const node = document.createElement(tag);
  if (props) {
    for (const [key, value] of Object.entries(props)) {
      if (value === null || value === undefined || value === false) continue;
      if (key === "class") node.className = value;
      else if (key === "text") node.textContent = value;
      else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
      else node.setAttribute(key, value === true ? "" : String(value));
    }
  }
  append(node, kids);
  return node;
}

function svg(tag, props, ...kids) {
  const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
  if (props) {
    for (const [key, value] of Object.entries(props)) {
      if (value === null || value === undefined || value === false) continue;
      if (key === "text") node.textContent = value;
      else node.setAttribute(key, String(value));
    }
  }
  append(node, kids);
  return node;
}

function append(node, kids) {
  for (const kid of kids.flat(4)) {
    if (kid === null || kid === undefined || kid === false || kid === "") continue;
    node.append(kid);
  }
}

/* Writing only on change keeps an SSE refresh from invalidating layout for
   rows whose text is identical, which is what keeps the list from jumping. */
function setText(node, value) {
  const next = value === null || value === undefined ? "" : String(value);
  if (node.textContent !== next) node.textContent = next;
}

function setAttr(node, name, value) {
  if (value === null || value === undefined || value === false) {
    if (node.hasAttribute(name)) node.removeAttribute(name);
  } else if (node.getAttribute(name) !== String(value)) {
    node.setAttribute(name, String(value));
  }
}

function show(node, visible) {
  if (node.hidden === !visible) return;
  node.hidden = !visible;
}

function clear(node) {
  node.replaceChildren();
}

/* Marks every visible child but the last, so the CSS separator never leads a
   wrapped line or trails one on its own. */
function separate(container) {
  const visible = [...container.children].filter((child) => !child.hidden);
  visible.forEach((child, i) => setAttr(child, "data-sep", i < visible.length - 1 ? "1" : null));
}

/* A middot-separated row of small facts. Built here so no caller can forget
   the separators. */
function numbers(parts) {
  const row = el("div", { class: "cand-nums" }, parts.filter(Boolean).map((part) => el("span", { text: part })));
  separate(row);
  return row;
}

/* Keyed reconcile. Rows are reused by id and mutated in place, so an update
   arriving while the operator is reading does not reflow the page under their
   thumb or drop their scroll position. */
function syncList(parent, items, keyOf, create, update) {
  const existing = new Map();
  for (const child of parent.children) existing.set(child.dataset.key, child);

  let previous = null;
  for (const item of items) {
    const key = keyOf(item);
    let node = existing.get(key);
    if (node) {
      existing.delete(key);
    } else {
      node = create(item);
      node.dataset.key = key;
    }
    /* Applied to new and reused rows alike; a freshly created row is a blank
       shell until its fields are written. */
    update(node, item);
    const wanted = previous ? previous.nextSibling : parent.firstChild;
    if (node !== wanted) parent.insertBefore(node, wanted);
    previous = node;
  }
  for (const stale of existing.values()) stale.remove();
}

/* ---- formatting -------------------------------------------------------- */
const RELATIVE = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
const ABSOLUTE = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const CLOCK = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", hour12: false });

function when(iso) {
  const at = Date.parse(iso);
  if (Number.isNaN(at)) return { text: "\u2014", title: "" };
  const seconds = (at - Date.now()) / 1000;
  const size = Math.abs(seconds);
  let text;
  if (size < 45) text = "just now";
  else if (size < 3600) text = RELATIVE.format(Math.round(seconds / 60), "minute");
  else if (size < 86400) text = RELATIVE.format(Math.round(seconds / 3600), "hour");
  else if (size < 6 * 86400) text = RELATIVE.format(Math.round(seconds / 86400), "day");
  else text = ABSOLUTE.format(at);
  return { text, title: ABSOLUTE.format(at) };
}

function clock(iso) {
  const at = Date.parse(iso);
  return Number.isNaN(at) ? "\u2014" : CLOCK.format(at);
}

/* Mirrors queue::short and run::short: the trailing segment of the id. */
const shortId = (id) => (typeof id === "string" && id.includes("-") ? id.split("-").pop() : id || "");

const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;

const candTone = (index) => `var(--cand-${"abcde"[index % 5]})`;

function seconds(ms) {
  if (!ms) return null;
  return ms < 1000 ? `${ms}ms` : ms < 60000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.round(ms / 60000)}m`;
}

/* ---- shared pieces ----------------------------------------------------- */
function chip(status, table) {
  const meta = table[status] || { glyph: "\u25cc", tone: "ink" };
  return el("span", {
    class: "chip",
    "data-status": status,
    "data-glyph": meta.glyph,
    "data-flight": meta.flight ? "1" : null,
    text: status,
  });
}

function toneOf(status, table) {
  return (table[status] || { tone: "ink" }).tone;
}

/* Where an in-flight run has reached. The summary carries no progress field,
   so the position is derived from the status against the fixed node order. */
function phaseRail(status) {
  const at = PHASES.indexOf(status);
  if (at < 0) return null;
  const rail = el("div", {
    class: "phases",
    role: "img",
    "aria-label": `Phase ${at + 1} of ${PHASES.length}: ${status}`,
  });
  for (let i = 0; i < PHASES.length; i += 1) {
    rail.append(el("span", {
      class: "phase",
      "data-on": i <= at ? "1" : null,
      "data-now": i === at ? "1" : null,
    }));
  }
  return rail;
}

/* ---- state ------------------------------------------------------------- */
const state = {
  route: { name: "runs", id: null },
  health: null,
  runs: null,
  queue: null,
  detail: { id: null, run: null, report: null },
  rev: { queue: null, runs: null },
  streamOpen: false,
  wrap: false,
};

let fallbackTimer = null;

/* ---- transport --------------------------------------------------------- */
async function request(url, init) {
  const res = await fetch(url, init);
  if (!res.ok) {
    let message = `${res.status} ${res.statusText || "request failed"}`;
    try {
      const body = await res.json();
      if (body && typeof body.error === "string") message = body.error;
    } catch {
      /* an error body is not guaranteed to be JSON; the status stands in */
    }
    const error = new Error(message);
    error.status = res.status;
    throw error;
  }
  return res;
}

const getJson = (url) => request(url).then((r) => r.json());
const getText = (url) => request(url).then((r) => r.text());

const postJson = (url, body) =>
  request(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body === undefined ? {} : body),
  }).then((r) => r.json());

/* ---- alert ------------------------------------------------------------- */
function fail(message) {
  const box = $("alert");
  setText(box.querySelector(".alert-text"), message);
  show(box, true);
}

function ok() {
  show($("alert"), false);
}

function announce(message) {
  setText($("live"), message);
}

/* ---- daemon strip ------------------------------------------------------ */
function renderDaemon() {
  const box = $("daemon");
  const text = box.querySelector(".daemon-text");

  if (!state.health) {
    setAttr(box, "data-state", null);
    setText(text, "Connecting\u2026");
    return;
  }

  const daemon = state.health.daemon || {};
  clear(text);

  if (!daemon.running) {
    setAttr(box, "data-state", "down");
    text.append(
      el("b", { text: "Loop not running." }),
      " Queued tasks wait until \u2018magi serve\u2019 is started.",
    );
    return;
  }

  const done = Number(daemon.completed) || 0;
  const tail = ` \u00b7 ${plural(done, "task done", "tasks done")}`;

  if (daemon.current && daemon.current.run) {
    setAttr(box, "data-state", "working");
    text.append(
      el("b", { text: "Working" }),
      " on ",
      el("a", {
        class: "daemon-run",
        href: `#/runs/${daemon.current.run}`,
        text: shortId(daemon.current.run),
        title: `run ${daemon.current.run}`,
      }),
      daemon.current.task
        ? el("span", { class: "daemon-run", text: ` \u2190 task ${shortId(daemon.current.task)}` })
        : null,
      tail,
    );
  } else if (daemon.idle) {
    setAttr(box, "data-state", "idle");
    text.append(el("b", { text: "Loop idle." }), " Nothing runnable in the queue.", tail);
  } else {
    setAttr(box, "data-state", "working");
    text.append(el("b", { text: "Working." }), " Claiming a task.", tail);
  }
}

/* ---- runs list --------------------------------------------------------- */
function createRunCard() {
  const chipSlot = el("span");
  const whenSlot = el("time", { class: "card-when" });
  const title = el("h2", { class: "card-title" });
  const repo = el("span", { class: "repo" });
  const counts = el("span");
  const winner = el("span", { class: "win" });
  const reviews = el("span");
  const meta = el("div", { class: "card-meta" }, repo, counts, winner, reviews);
  const note = el("p", { class: "card-note" });
  const event = el("p", { class: "card-event" });
  const rail = el("div");

  const card = el("a", { class: "card" },
    el("div", { class: "card-top" }, chipSlot, whenSlot),
    title, meta, note, event, rail,
  );
  const row = el("li", {}, card);
  row.refs = { card, chipSlot, whenSlot, title, repo, counts, winner, reviews, note, event, rail };
  return row;
}

function updateRunCard(row, run) {
  const r = row.refs;
  const status = String(run.status || "");
  const meta = RUN_STATUS[status] || {};

  r.card.setAttribute("href", `#/runs/${run.id}`);
  setAttr(r.card, "data-tone", toneOf(status, RUN_STATUS));

  /* The chip is replaced rather than mutated: it is one element and its
     pseudo-element glyph is attribute-driven, so this cannot reflow siblings. */
  const next = chip(status, RUN_STATUS);
  if (r.chipSlot.firstChild) r.chipSlot.firstChild.replaceWith(next);
  else r.chipSlot.append(next);

  const at = when(run.updated_at || run.created_at);
  setText(r.whenSlot, at.text);
  setAttr(r.whenSlot, "datetime", run.updated_at || run.created_at);
  setAttr(r.whenSlot, "title", `updated ${at.title}`);

  setText(r.title, run.title || run.instruction || run.id);
  setText(r.repo, run.repo_name || "");
  setAttr(r.repo, "title", run.repo || "");

  const cands = Number(run.candidates) || 0;
  const viable = Number(run.viable) || 0;
  const judges = Number(run.judges) || 0;
  const bits = [];
  if (cands) bits.push(viable === cands ? plural(cands, "candidate", "candidates") : `${viable}/${cands} viable`);
  if (judges) bits.push(plural(judges, "judge", "judges"));
  setText(r.counts, bits.join(", "));
  show(r.counts, bits.length > 0);

  setText(r.winner, run.winner ? `winner ${run.winner}` : "");
  show(r.winner, Boolean(run.winner));

  const rounds = Number(run.reviews) || 0;
  const losses = Number(run.quota_losses) || 0;
  const extra = [];
  if (rounds) extra.push(plural(rounds, "review round", "review rounds"));
  if (losses) extra.push(`${plural(losses, "seat", "seats")} lost to quota`);
  setText(r.reviews, extra.join(", "));
  show(r.reviews, extra.length > 0);
  separate(r.reviews.parentNode);

  /* Spell out the two endings that look like results but are not. */
  setText(r.note, meta.note && status !== "merged" && status !== "ready" ? meta.note : "");
  show(r.note, Boolean(meta.note) && status !== "merged" && status !== "ready");

  const moving = !run.done;
  setText(r.event, moving && run.event ? run.event : "");
  show(r.event, Boolean(moving && run.event));

  const rail = moving ? phaseRail(status) : null;
  clear(r.rail);
  if (rail) r.rail.append(rail);
}

function renderRuns() {
  const list = $("runs-list");
  const runs = state.runs;

  if (runs === null) {
    setText($("runs-count"), "Loading\u2026");
    if (!list.dataset.skeleton) {
      clear(list);
      for (let i = 0; i < 3; i += 1) {
        list.append(el("li", { class: "card skeleton" },
          el("div", { class: "bar", style: "width:34%" }),
          el("div", { class: "bar", style: "width:88%;height:18px" }),
          el("div", { class: "bar", style: "width:56%" }),
        ));
      }
      list.dataset.skeleton = "1";
    }
    return;
  }

  if (list.dataset.skeleton) {
    clear(list);
    delete list.dataset.skeleton;
  }

  const moving = runs.filter((r) => !r.done).length;
  const unreadable = Number(state.health && state.health.runs_unreadable) || 0;
  const unreadableNote = unreadable
    ? `${unreadable} unreadable`
    : "";
  const counts = runs.length === 0
    ? (unreadable ? `no readable runs, ${unreadableNote}` : "Nothing has run yet")
    : [`${plural(runs.length, "run", "runs")}, ${moving} in flight`, unreadableNote]
        .filter(Boolean)
        .join(", ");
  setText($("runs-count"), counts);

  // An unreadable run is still a run: offer the explanation instead of the
  // "file your first task" prompt, which would be wrong and confusing.
  show($("runs-empty"), runs.length === 0 && unreadable === 0);
  show($("runs-unreadable"), runs.length === 0 && unreadable > 0);
  syncList(list, runs, (r) => r.id, createRunCard, updateRunCard);
}

/* ---- queue ------------------------------------------------------------- */
function createTaskCard() {
  const chipSlot = el("span");
  const priority = el("span", { class: "tag", "data-tone": "ink" });
  const whenSlot = el("time", { class: "card-when" });
  const title = el("h2", { class: "card-title" });
  const source = el("span");
  const repo = el("span", { class: "repo" });
  const attempts = el("span");
  const meta = el("div", { class: "card-meta" }, source, repo, attempts);
  const note = el("p", { class: "card-note" });
  const error = el("pre", { class: "err" });
  const instruction = el("details", { class: "advanced" },
    el("summary", { text: "Full instruction" }),
    el("p", { class: "instruction" }));
  const runLink = el("a", { class: "btn btn-quiet" });
  const hold = el("button", { class: "btn btn-quiet", type: "button" });
  const actions = el("div", { class: "card-actions" }, runLink, hold);

  const card = el("li", { class: "card" },
    el("div", { class: "card-top" }, chipSlot, priority, whenSlot),
    title, meta, note, error, instruction, actions,
  );
  card.refs = { card, chipSlot, priority, whenSlot, title, source, repo, attempts, note, error, instruction, runLink, hold };
  return card;
}

function updateTaskCard(row, task) {
  const r = row.refs;
  const status = String(task.status_str || task.status || "");
  const meta = TASK_STATUS[status] || {};

  setAttr(r.card, "data-tone", toneOf(status, TASK_STATUS));

  const next = chip(status, TASK_STATUS);
  if (r.chipSlot.firstChild) r.chipSlot.firstChild.replaceWith(next);
  else r.chipSlot.append(next);

  const priority = Number(task.priority) || 0;
  setText(r.priority, priority > 0 ? `priority +${priority}` : `priority ${priority}`);
  setAttr(r.priority, "data-tone", priority > 0 ? "rust" : "ink");
  show(r.priority, priority !== 0);

  const at = when(task.updated_at || task.created_at);
  setText(r.whenSlot, at.text);
  setAttr(r.whenSlot, "datetime", task.updated_at || task.created_at);
  setAttr(r.whenSlot, "title", `updated ${at.title}`);

  setText(r.title, task.title || task.instruction || task.id);
  setText(r.source, task.source_label || "");
  const repoName = typeof task.repo === "string" ? task.repo.split(/[\\/]/).filter(Boolean).pop() : "";
  setText(r.repo, repoName || "");
  setAttr(r.repo, "title", task.repo || "");

  const attempts = Number(task.attempts) || 0;
  setText(r.attempts, attempts ? plural(attempts, "attempt", "attempts") : "");
  show(r.attempts, attempts > 0);
  separate(r.attempts.parentNode);

  setText(r.note, meta.note || "");
  show(r.note, Boolean(meta.note));

  setText(r.error, task.last_error || "");
  show(r.error, Boolean(task.last_error));

  const full = task.instruction || "";
  setText(r.instruction.querySelector(".instruction"), full);
  show(r.instruction, full.trim() !== (task.title || "").trim() && full !== "");

  const runs = Array.isArray(task.runs) ? task.runs : [];
  const latest = runs.length ? runs[runs.length - 1] : null;
  if (latest) {
    setAttr(r.runLink, "href", `#/runs/${latest}`);
    setText(r.runLink, `Run ${shortId(latest)}`);
  }
  show(r.runLink, Boolean(latest));

  /* Hold and release are the only mutations the contract exposes; there is
     deliberately no delete. */
  const held = status === "held";
  setText(r.hold, held ? "Release" : "Hold");
  setAttr(r.hold, "aria-label", `${held ? "Release" : "Hold"} task ${task.title || task.id}`);
  r.hold.disabled = status === "running" || status === "done";
  r.hold.onclick = () => mutateTask(task.id, held ? "release" : "hold", r.hold);
  show(r.hold, status !== "done");
}

async function mutateTask(id, action, button) {
  const label = button.textContent;
  button.disabled = true;
  setText(button, "\u2026");
  try {
    await postJson(action === "hold" ? API.hold(id) : API.release(id));
    ok();
    announce(`Task ${shortId(id)} ${action === "hold" ? "held" : "released"}.`);
    await loadQueue();
  } catch (error) {
    setText(button, label);
    button.disabled = false;
    fail(`Could not ${action} task ${shortId(id)}: ${error.message}`);
  }
}

function renderQueue() {
  const list = $("queue-list");
  const tasks = state.queue;

  if (tasks === null) {
    setText($("queue-count"), "Loading\u2026");
    return;
  }

  const waiting = tasks.filter((t) => (t.status_str || t.status) === "queued").length;
  const held = tasks.filter((t) => (t.status_str || t.status) === "held").length;
  const parts = [`${plural(tasks.length, "task", "tasks")}`];
  if (waiting) parts.push(`${waiting} runnable`);
  if (held) parts.push(`${held} held`);
  setText($("queue-count"), tasks.length === 0 ? "Nothing waiting" : parts.join(", "));

  show($("queue-empty"), tasks.length === 0);
  syncList(list, tasks, (t) => t.id, createTaskCard, updateTaskCard);
}

/* ---- run detail -------------------------------------------------------- */
function viable(candidate) {
  /* Candidate::viable is a method, so it is not on the wire; the rule it
     encodes is. */
  return !candidate.failed && !candidate.empty;
}

function renderRunDetail() {
  const run = state.detail.run;
  const report = state.detail.report;

  $("run-report").dataset.wrap = state.wrap ? "1" : "0";

  if (!run) {
    setText($("run-h"), "Loading run\u2026");
    setText($("run-meta"), "");
    clear($("run-status"));
    setText($("run-report"), report === null ? "Loading\u2026" : report);
    return;
  }

  const status = String(run.status || "");
  const meta = RUN_STATUS[status] || {};

  const head = $("run-status");
  clear(head);
  head.append(chip(status, RUN_STATUS));
  const rail = PHASES.includes(status) ? phaseRail(status) : null;
  if (rail) head.append(rail);
  if (meta.note) head.append(el("p", { class: "card-note", text: meta.note }));

  setText($("run-h"), firstLine(run.instruction) || shortId(run.id));

  const created = when(run.created_at);
  const updated = when(run.updated_at);
  const repoName = typeof run.repo === "string" ? run.repo.split(/[\\/]/).filter(Boolean).pop() : "";
  setText($("run-meta"),
    `${shortId(run.id)} \u00b7 ${repoName} \u00b7 ${run.base_branch || ""} \u00b7 started ${created.text} \u00b7 updated ${updated.text}`);
  setAttr($("run-meta"), "title", `${run.id}\n${run.repo || ""}\nstarted ${created.title}\nupdated ${updated.title}`);

  setText($("run-instruction"), run.instruction || "");

  renderVerdict(run);
  renderCandidates(run);
  renderReviews(run);
  renderQuota(run);
  renderTimeline(run);

  setText($("run-report"), report === null ? "Loading\u2026" : report);
}

function firstLine(text) {
  if (typeof text !== "string") return "";
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (trimmed) return trimmed.length > 96 ? `${trimmed.slice(0, 95)}\u2026` : trimmed;
  }
  return "";
}

function renderVerdict(run) {
  const tally = run.tally;
  const panel = $("run-verdict");
  const candidates = Array.isArray(run.candidates) ? run.candidates : [];
  show(panel, Boolean(tally) || candidates.length > 0);
  if (!tally && candidates.length === 0) return;

  const converge = $("converge");
  clear(converge);
  /* A winner label alone does not mean a verdict. A run whose panel collapsed
     still records the one ranking it got, so the diamond is only drawn as
     decided when the quorum backs it. */
  const decided = Boolean(tally && tally.met_quorum);
  converge.append(convergeDiagram(candidates, tally ? tally.winner : null, decided));

  const facts = $("tally-facts");
  clear(facts);
  if (!tally) {
    facts.append(
      el("dt", { text: "Verdict" }),
      el("dd", { text: "Not reached yet." }),
    );
    return;
  }

  const first = tally.first_choice || {};
  const votes = Object.keys(first)
    .sort()
    .map((label) => `${label}: ${first[label]}`)
    .join("  \u00b7  ");

  const rows = [
    ["Winner", tally.winner
      ? `Candidate ${tally.winner}${decided ? "" : " \u2014 provisional only"}`
      : "\u2014"],
    ["First choices", votes || "\u2014"],
    ["Panel", `${Number(tally.present) || 0} of ${Number(tally.judges) || 0} present, quorum ${Number(tally.quorum) || 0}`],
    /* Quorum is the field that says whether the verdict is worth anything. */
    ["Quorum", tally.met_quorum ? "Met" : "NOT MET \u2014 the verdict is not trustworthy"],
    ["Agreement", tally.unanimous_final
      ? "Unanimous final vote"
      : `Split; ${plural(Number(tally.changed_votes) || 0, "judge", "judges")} moved`],
    ["Deliberated", tally.deliberated ? "Yes" : "No"],
  ];
  if (tally.tie_break) rows.push(["Tie break", tally.tie_break]);

  for (const [term, value] of rows) {
    facts.append(el("dt", { text: term }), el("dd", { text: value }));
  }
}

/* The mark, drawn from the real candidate list: independent bodies at the top,
   one gold verdict at the convergence point. The winner's stroke survives at
   full weight; the others recede, and a candidate that never produced work is
   dashed. `decided` is the quorum: without it the convergence point stays
   hollow, because a stalled run reached no verdict however its ranking read. */
function convergeDiagram(candidates, winner, decided) {
  const width = 320;
  const height = 132;
  const midX = width / 2;
  const knot = 96;
  const count = Math.max(candidates.length, 1);

  const labels = candidates.map((c) => c.label).filter(Boolean).join(", ");
  const root = svg("svg", {
    viewBox: `0 0 ${width} ${height}`,
    role: "img",
    "aria-label": candidates.length
      ? `${plural(candidates.length, "candidate", "candidates")} ${labels}${winner && decided ? `; ${winner} won` : winner ? `; ${winner} leads but the panel reached no quorum` : "; no verdict yet"}`
      : "No candidates yet",
  });

  const span = Math.min(96, (width - 68) / Math.max(count - 1, 1));
  const xs = candidates.map((_, i) => midX + (i - (count - 1) / 2) * span);

  candidates.forEach((candidate, i) => {
    const x = xs[i];
    const won = winner && candidate.label === winner && decided;
    const dead = !viable(candidate);
    const tone = candTone(i);
    const path = x === midX
      ? `M ${x} 44 L ${x} ${knot}`
      : `M ${x} 44 C ${x} ${knot - 22}, ${(x + midX) / 2} ${knot - 8}, ${midX} ${knot}`;

    root.append(svg("path", {
      d: path,
      fill: "none",
      stroke: tone,
      "stroke-width": won ? 5 : 2.5,
      "stroke-linecap": "round",
      "stroke-dasharray": dead ? "3 5" : null,
      opacity: won ? 1 : dead ? 0.35 : 0.55,
    }));
    root.append(svg("circle", {
      cx: x, cy: 26, r: 13,
      fill: dead ? "var(--sunk)" : tone,
      stroke: tone,
      "stroke-width": 2,
      "stroke-dasharray": dead ? "3 3" : null,
    }));
    root.append(svg("text", {
      x, y: 31,
      "text-anchor": "middle",
      fill: dead ? tone : "var(--surface)",
      text: candidate.label || "?",
    }));
  });

  if (winner && decided) {
    root.append(svg("rect", {
      x: midX - 11, y: knot - 11, width: 22, height: 22,
      transform: `rotate(45 ${midX} ${knot})`,
      fill: "var(--gold-line)",
    }));
    root.append(svg("path", {
      d: `M ${midX} ${knot + 16} L ${midX} ${height - 8}`,
      stroke: "var(--gold-line)", "stroke-width": 5, "stroke-linecap": "round",
    }));
  } else {
    /* No verdict: the convergence point is drawn hollow, so an unfinished or
       collapsed run does not display a decided diamond. */
    root.append(svg("rect", {
      x: midX - 10, y: knot - 10, width: 20, height: 20,
      transform: `rotate(45 ${midX} ${knot})`,
      fill: "none", stroke: "var(--line-2)", "stroke-width": 2, "stroke-dasharray": "3 3",
    }));
  }

  return root;
}

function renderCandidates(run) {
  const candidates = Array.isArray(run.candidates) ? run.candidates : [];
  show($("run-cands-panel"), candidates.length > 0);
  if (candidates.length === 0) return;

  const winner = run.tally ? run.tally.winner : null;
  const decided = Boolean(run.tally && run.tally.met_quorum);
  setText($("cand-count"), `${candidates.filter(viable).length} viable of ${candidates.length}`);

  const list = $("run-cands");
  clear(list);
  candidates.forEach((candidate, i) => {
    const dead = !viable(candidate);
    const facts = [];
    if (candidate.commits) facts.push(plural(candidate.commits, "commit", "commits"));
    if (candidate.files) facts.push(plural(candidate.files, "file", "files"));
    const took = seconds(candidate.duration_ms);
    if (took) facts.push(took);
    if (candidate.branch) facts.push(candidate.branch);

    list.append(el("li", {
      class: "cand",
      "data-winner": winner && candidate.label === winner && decided ? "1" : null,
      style: `--cand-tone: ${candTone(i)}`,
    },
      el("div", { class: "cand-head" },
        el("span", { class: "cand-label", text: candidate.label || "?" }),
        el("span", { class: "cand-agent", text: candidate.agent || "" }),
        winner && candidate.label === winner
          ? el("span", {
              class: "crown",
              "data-provisional": decided ? null : "1",
              text: decided ? "winner" : "provisional",
            })
          : null,
      ),
      facts.length ? numbers(facts) : null,
      dead
        ? el("p", { class: "card-note", text: candidate.failed || "Produced no change at all." })
        : null,
      candidate.summary ? el("p", { class: "cand-summary", text: candidate.summary }) : null,
      candidate.stat ? el("pre", { class: "stat", text: candidate.stat }) : null,
    ));
  });
}

function renderReviews(run) {
  /* On the wire this is Vec<ReviewRound>, each round holding the reviewers'
     records. */
  const rounds = Array.isArray(run.reviews) ? run.reviews : [];
  const gate = Array.isArray(run.gate) ? run.gate : [];
  show($("run-reviews-panel"), rounds.length > 0 || gate.length > 0);
  if (rounds.length === 0 && gate.length === 0) return;

  setText($("review-count"), rounds.length ? plural(rounds.length, "round", "rounds") : "gate only");

  const list = $("run-reviews");
  clear(list);

  for (const round of rounds) {
    const blocking = Number(round.blocking) || 0;
    const records = Array.isArray(round.reviews) ? round.reviews : [];

    const node = el("li", { class: "round" },
      el("div", { class: "round-head" },
        el("span", { class: "round-n", text: `Round ${round.round}` }),
        round.clean
          ? el("span", { class: "tag", "data-tone": "teal", text: "clean" })
          : el("span", { class: "tag", "data-tone": "rust", text: `${plural(blocking, "blocker", "blockers")}` }),
        round.head ? el("span", { class: "head-sha", text: String(round.head).slice(0, 7) }) : null,
      ),
    );

    for (const record of records) {
      const findings = Array.isArray(record.findings) ? record.findings : [];
      node.append(el("div", { class: "reviewer" },
        el("p", {},
          el("span", { class: "reviewer-name", text: `reviewer ${record.reviewer} \u00b7 ${record.agent || ""}` }),
        ),
        record.failed ? el("p", { class: "card-note", text: record.failed }) : null,
        record.summary ? el("p", { class: "cand-summary", text: record.summary }) : null,
        findings.length ? el("div", { class: "findings" }, findings
          .slice()
          .sort((a, b) => (SEV_RANK[b.severity] || 0) - (SEV_RANK[a.severity] || 0))
          .map((finding) => el("div", { class: "finding", "data-sev": finding.severity },
            el("div", { class: "finding-top" },
              el("span", { class: "finding-sev", text: finding.severity || "" }),
              el("span", { class: "finding-title", text: finding.title || "" }),
              finding.id ? el("span", { class: "finding-id", text: finding.id }) : null,
            ),
            finding.file
              ? el("p", { class: "finding-where", text: `${finding.file}${finding.line ? `:${finding.line}` : ""}` })
              : null,
            finding.detail ? el("p", { class: "finding-detail", text: finding.detail }) : null,
          ))) : null,
      ));
    }

    const e2e = Array.isArray(round.e2e) ? round.e2e : [];
    if (e2e.length) node.append(commandList("Verification", e2e));

    if (round.fix) {
      const fix = round.fix;
      const addressed = Array.isArray(fix.addressed) ? fix.addressed : [];
      const rejected = Array.isArray(fix.rejected) ? fix.rejected : [];
      node.append(el("div", { class: "reviewer" },
        el("p", {}, el("span", { class: "reviewer-name", text: `fix \u00b7 ${fix.agent || ""}` })),
        numbers([
          `${addressed.length} addressed`,
          `${rejected.length} declined`,
          fix.committed ? "committed" : "no commit",
        ]),
        fix.failed ? el("p", { class: "card-note", text: fix.failed }) : null,
        fix.notes ? el("p", { class: "cand-summary", text: fix.notes }) : null,
        rejected.length ? el("div", { class: "findings" }, rejected.map((r) =>
          el("div", { class: "finding" },
            el("div", { class: "finding-top" },
              el("span", { class: "finding-id", text: r.id || "" }),
              el("span", { class: "finding-title", text: "declined" }),
            ),
            r.why ? el("p", { class: "finding-detail", text: r.why }) : null,
          ))) : null,
      ));
    }

    list.append(node);
  }

  if (gate.length) list.append(el("li", { class: "round" }, commandList("Gate", gate)));
}

function commandList(heading, commands) {
  return el("div", { class: "reviewer" },
    el("p", {}, el("span", { class: "reviewer-name", text: heading })),
    el("div", { class: "findings" }, commands.map((command) => {
      /* CommandOutcome::ok is a method; the wire has the exit code. */
      const passed = command.code === 0;
      return el("div", { class: "finding", "data-sev": passed ? null : "blocker" },
        el("div", { class: "finding-top" },
          el("span", { class: "tag", "data-tone": passed ? "teal" : "rust", text: passed ? "pass" : "fail" }),
          el("span", { class: "finding-where", text: command.command || "" }),
          el("span", { class: "finding-id", text: command.code === null || command.code === undefined ? "timeout" : `exit ${command.code}` }),
        ),
        !passed && command.output_tail
          ? el("pre", { class: "stat", text: command.output_tail })
          : null,
      );
    })),
  );
}

function renderQuota(run) {
  const losses = Array.isArray(run.quota) ? run.quota : [];
  show($("run-quota-panel"), losses.length > 0);
  if (losses.length === 0) return;

  const list = $("run-quota");
  clear(list);
  for (const loss of losses) {
    list.append(el("li", {},
      el("span", { class: "seat", text: loss.seat || "" }),
      el("span", { text: `during ${loss.node || "?"}` }),
      el("span", { text: clock(loss.at) }),
      loss.reset ? el("span", { text: `resets ${loss.reset}` }) : null,
    ));
  }
}

function renderTimeline(run) {
  const events = Array.isArray(run.events) ? run.events : [];
  show($("run-events-panel"), events.length > 0);
  if (events.length === 0) return;

  const list = $("run-events");
  clear(list);
  for (const event of events) {
    list.append(el("li", {},
      el("span", { class: "event-at", text: clock(event.at) }),
      el("div", { class: "event-body" },
        el("p", { class: "event-node", text: event.node || "" }),
        el("p", { class: "event-msg", text: event.message || "" }),
      ),
    ));
  }
}

/* ---- loading ----------------------------------------------------------- */
async function loadRuns() {
  try {
    state.runs = await getJson(API.runs(RUN_LIMIT));
    renderRuns();
    ok();
  } catch (error) {
    fail(`Could not load runs: ${error.message}`);
  }
}

async function loadQueue() {
  try {
    state.queue = await getJson(API.queue);
    renderQueue();
    ok();
  } catch (error) {
    fail(`Could not load the queue: ${error.message}`);
  }
}

async function loadHealth({ applyRevisions = false } = {}) {
  try {
    state.health = await getJson(API.health);
    renderDaemon();
    if (applyRevisions) await applyRevisions_(state.health);
    ok();
  } catch (error) {
    fail(`Cannot reach magi: ${error.message}`);
  }
}

async function loadRun(id) {
  const fresh = state.detail.id !== id;
  if (fresh) state.detail = { id, run: null, report: null };
  renderRunDetail();

  const [run, report] = await Promise.allSettled([getJson(API.run(id)), getText(API.report(id))]);

  if (state.detail.id !== id) return;   /* the operator navigated away */

  if (run.status === "fulfilled") {
    state.detail.run = run.value;
    ok();
  } else {
    fail(`Could not load run ${shortId(id)}: ${run.reason.message}`);
  }
  state.detail.report = report.status === "fulfilled"
    ? report.value
    : `The report could not be rendered: ${report.reason.message}`;

  renderRunDetail();
}

/* Named with a trailing underscore because `applyRevisions` is also the option
   name on loadHealth. */
async function applyRevisions_(source) {
  const queueRev = source.queue_rev;
  const runsRev = source.runs_rev;
  const jobs = [];

  if (queueRev !== state.rev.queue) {
    state.rev.queue = queueRev;
    jobs.push(loadQueue());
  }
  if (runsRev !== state.rev.runs) {
    state.rev.runs = runsRev;
    jobs.push(loadRuns());
    if (state.route.name === "run" && state.detail.id) jobs.push(loadRun(state.detail.id));
  }
  if (jobs.length) {
    await Promise.allSettled(jobs);
    announce("Updated.");
  }
}

/* ---- live stream ------------------------------------------------------- */
function subscribe() {
  let stream;
  try {
    stream = new EventSource(API.events);
  } catch {
    return;   /* no EventSource: the health interval still refreshes revisions */
  }

  stream.addEventListener("open", () => {
    state.streamOpen = true;
    ok();
  });

  stream.addEventListener("change", (message) => {
    let payload;
    try {
      payload = JSON.parse(message.data);
    } catch {
      return;
    }
    state.streamOpen = true;
    applyRevisions_(payload);
  });

  /* EventSource reconnects on its own. The one thing it will not do is repair
     the data that went stale while it was down, so a single refresh is
     scheduled — debounced, because a server that is gone errors repeatedly. */
  stream.addEventListener("error", () => {
    state.streamOpen = false;
    if (fallbackTimer) return;
    fallbackTimer = setTimeout(() => {
      fallbackTimer = null;
      loadHealth({ applyRevisions: true });
    }, 3000);
  });
}

/* ---- routing ----------------------------------------------------------- */
function parseRoute() {
  const parts = location.hash.replace(/^#\/?/, "").split("/").filter(Boolean);
  if (parts[0] === "queue") return { name: "queue", id: null };
  if (parts[0] === "runs" && parts[1]) return { name: "run", id: decodeURIComponent(parts[1]) };
  return { name: "runs", id: null };
}

function applyRoute() {
  const route = parseRoute();
  const changed = route.name !== state.route.name || route.id !== state.route.id;
  state.route = route;

  show($("view-runs"), route.name === "runs");
  show($("view-run"), route.name === "run");
  show($("view-queue"), route.name === "queue");

  const section = route.name === "queue" ? "queue" : "runs";
  for (const link of document.querySelectorAll("[data-nav]")) {
    setAttr(link, "aria-current", link.dataset.nav === section ? "page" : null);
  }

  if (route.name === "run") {
    if (state.detail.id !== route.id) loadRun(route.id);
  } else {
    state.detail = { id: null, run: null, report: null };
  }

  if (changed) window.scrollTo({ top: 0 });
  document.title = route.name === "queue"
    ? "Backlog — magi"
    : route.name === "run"
      ? `Run ${shortId(route.id)} — magi`
      : "magi — observation deck";
}

/* ---- compose ----------------------------------------------------------- */
function openCompose() {
  const dialog = $("compose");
  show($("compose-error"), false);
  if (!dialog.open) dialog.showModal();
  /* Focus the field that matters, not the first tabbable element. */
  requestAnimationFrame(() => $("f-instruction").focus());
}

function closeCompose() {
  const dialog = $("compose");
  if (dialog.open) dialog.close();
}

async function submitCompose(event) {
  event.preventDefault();
  const submit = $("compose-submit");
  const error = $("compose-error");
  const instruction = $("f-instruction").value;

  if (!instruction.trim()) {
    setText(error, "An instruction is required.");
    show(error, true);
    $("f-instruction").focus();
    return;
  }

  const title = $("f-title").value.trim();
  const repo = $("f-repo").value.trim();
  const priority = Number($("f-priority").value);

  submit.disabled = true;
  setText(submit, "Filing\u2026");
  show(error, false);

  try {
    const task = await postJson(API.queue, {
      instruction,
      title: title || null,
      repo: repo || null,
      priority: Number.isFinite(priority) ? priority : null,
    });

    /* Reflect it immediately rather than waiting for the stream to notice. */
    state.queue = [task, ...(state.queue || []).filter((t) => t.id !== task.id)];
    renderQueue();
    announce(`Task ${shortId(task.id)} filed.`);

    $("compose-form").reset();
    closeCompose();
    if (state.route.name !== "queue") location.hash = "#/queue";
    loadQueue();
  } catch (failure) {
    setText(error, failure.message);
    show(error, true);
  } finally {
    submit.disabled = false;
    setText(submit, "File task");
  }
}

/* ---- theme ------------------------------------------------------------- */
const THEMES = ["auto", "light", "dark"];
const THEME_LABEL = {
  auto: "Colour theme: follow system",
  light: "Colour theme: light",
  dark: "Colour theme: dark",
};

function currentTheme() {
  const value = document.documentElement.dataset.theme;
  return THEMES.includes(value) ? value : "auto";
}

function applyTheme(theme) {
  if (theme === "auto") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
  setAttr($("theme-toggle"), "aria-label", THEME_LABEL[theme]);
  setAttr($("theme-toggle"), "title", THEME_LABEL[theme]);
  try {
    if (theme === "auto") localStorage.removeItem("magi-theme");
    else localStorage.setItem("magi-theme", theme);
  } catch {
    /* localStorage is denied in private mode; the theme still applies now */
  }
}

/* ---- boot -------------------------------------------------------------- */
function wire() {
  for (const button of document.querySelectorAll("[data-compose]")) {
    button.addEventListener("click", openCompose);
  }
  $("compose-form").addEventListener("submit", submitCompose);
  $("compose-close").addEventListener("click", closeCompose);
  $("compose-cancel").addEventListener("click", closeCompose);

  $("theme-toggle").addEventListener("click", () => {
    const next = THEMES[(THEMES.indexOf(currentTheme()) + 1) % THEMES.length];
    applyTheme(next);
  });

  $("wrap-toggle").addEventListener("click", (event) => {
    state.wrap = !state.wrap;
    event.currentTarget.setAttribute("aria-pressed", String(state.wrap));
    $("run-report").dataset.wrap = state.wrap ? "1" : "0";
  });

  $("alert-retry").addEventListener("click", () => {
    ok();
    loadHealth({ applyRevisions: true });
    if (state.route.name === "run" && state.detail.id) loadRun(state.detail.id);
  });

  window.addEventListener("hashchange", applyRoute);

  /* A phone spends most of its time with the screen off. Asking again on wake
     is what stops the operator reading a snapshot from an hour ago. */
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) loadHealth({ applyRevisions: true });
  });
}

async function boot() {
  applyTheme(currentTheme());
  wire();
  applyRoute();

  await loadHealth();
  if (state.health) {
    state.rev.queue = state.health.queue_rev;
    state.rev.runs = state.health.runs_rev;
  }
  await Promise.allSettled([loadRuns(), loadQueue()]);

  subscribe();
  setInterval(() => {
    if (document.hidden) return;
    loadHealth({ applyRevisions: !state.streamOpen });
  }, HEALTH_MS);
}

boot();
