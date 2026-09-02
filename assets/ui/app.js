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
  questions: "/api/questions",
  answer: (id) => `/api/questions/${encodeURIComponent(id)}/answer`,
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
  /* Derived from RunSummary.waiting rather than trusted from the status
     string: the run parked in some node and the summary still names it. */
  waiting:      { glyph: "?", tone: "wait", note: "An agent stopped to ask you something. Nothing in this run moves until it is answered." },
};

const TASK_STATUS = {
  queued:  { glyph: "\u25cc", tone: "ink" },
  running: { glyph: "\u25b8", tone: "blue", flight: true },
  done:    { glyph: "\u25c6", tone: "gold" },
  failed:  { glyph: "\u2715", tone: "rust" },
  held:    { glyph: "\u2016", tone: "rust", note: "Held. This task will not be claimed until it is released." },
};

/* An unanswered question is the only state in the product that a human, and
   only a human, can clear. `open` therefore borrows the same ringed gold as a
   waiting run, and `answered` reads as settled rather than successful \u2014 a
   decision is not a win. */
const QUESTION_STATUS = {
  open:      { glyph: "?", tone: "wait" },
  answered:  { glyph: "\u2713", tone: "teal" },
  abandoned: { glyph: "\u2296", tone: "ink" },
};

/* Check state on the pull request the land loop is watching. `red` is
   deliberately not called a failure: the loop answers it with another fixer
   round, and the word for that is in landNote() below. Pending carries no
   glyph because CSS spins its ring \u2014 it is the one state that resolves
   without anybody doing anything. */
const CHECKS = {
  pending: { glyph: "",        word: "checks running" },
  green:   { glyph: "\u2713",  word: "checks green" },
  red:     { glyph: "\u2715",  word: "checks red" },
  unknown: { glyph: "\u2013",  word: "checks unknown" },
};

/* A pull request closed without merging is a problem; merged is the verdict
   colour; open is simply where it is. */
const PR_TONE = { open: "ink", merged: "gold", closed: "rust" };

/* Open first, then settled, newest first inside each group. The server sends
   this order already; it is applied again locally so an answer reflected
   before the next revision lands in the right place. */
const ASK_ORDER = { open: 0, answered: 1, abandoned: 2 };

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

/* The one URL in this client that comes from the API rather than from this
   file. Everything else from a run record is rendered as text, which cannot
   execute; an href can, so a `javascript:` value in a run record would be a
   click away from running in the operator's session. Only the two schemes a
   forge actually serves are let through. */
function forgeUrl(value) {
  if (typeof value !== "string") return null;
  try {
    const url = new URL(value, location.origin);
    return url.protocol === "https:" || url.protocol === "http:" ? url.href : null;
  } catch {
    return null;   /* not a URL at all */
  }
}

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

/* A question names the graph node it came from — `implement`, `review`,
   `gate` — while the rail is indexed by run status: `implementing`,
   `reviewing`, `gating`. The node name being the stem of its status is the
   only relationship the two vocabularies have, so it is matched as one. A
   table mapping them by hand would go stale the first time a node is added,
   and the failure would be silent: a parked run with no rail at all. */
function phaseOf(node) {
  if (!node) return null;
  return PHASES.find((phase) => phase === node || phase.startsWith(node)) || null;
}

/* Where an in-flight run has reached. The summary carries no progress field,
   so the position is derived from the status against the fixed node order.
   A parked run passes the node its question came from, so the rail still says
   how far the run got while marking that segment stopped instead of pulsing:
   the same rail cannot mean "working" and "halted" on colour alone. */
function phaseRail(status, node) {
  const parked = phaseOf(node);
  const at = PHASES.indexOf(parked || status);
  if (at < 0) return null;
  const rail = el("div", {
    class: "phases",
    role: "img",
    "aria-label": parked
      ? `Stopped at phase ${at + 1} of ${PHASES.length}, ${parked}, waiting for your answer`
      : `Phase ${at + 1} of ${PHASES.length}: ${status}`,
  });
  for (let i = 0; i < PHASES.length; i += 1) {
    const here = i === at;
    rail.append(el("span", {
      class: "phase",
      "data-on": parked ? (i < at ? "1" : null) : (i <= at ? "1" : null),
      "data-now": here && !parked ? "1" : null,
      "data-parked": here && parked ? "1" : null,
    }));
  }
  return rail;
}

/* How many land rounds the loop has spent of its budget. Same vocabulary as
   the phase rail, because it is the same idea: a fixed number of steps and
   the one it is on. */
function roundRail(pr) {
  const rounds = Number(pr.rounds) || 0;
  const round = Number(pr.round) || 0;
  if (rounds <= 0) return null;
  const settled = pr.state !== "open";
  const rail = el("div", {
    class: "phases",
    role: "img",
    "aria-label": `Land round ${round} of ${rounds}`,
  });
  for (let i = 1; i <= rounds; i += 1) {
    rail.append(el("span", {
      class: "phase",
      "data-round": i < round || (i === round && settled) ? "1" : null,
      "data-now": i === round && !settled ? "1" : null,
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
  questions: null,
  rev: { queue: null, runs: null, questions: null },
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

  /* The card is one big anchor, which is the affordance the whole phone
     layout leans on, and an anchor may contain neither another anchor nor a
     button. The pull-request link and the Answer action therefore live in a
     sibling strip that CSS draws as the bottom of the same card. */
  const prLink = el("a", { class: "pr-link", target: "_blank", rel: "noopener noreferrer" });
  const checks = el("span");
  const prRound = el("span", { class: "pr-round" });
  const tailGo = el("a", { class: "btn btn-gold tail-go" });
  const tailNote = el("p", { class: "tail-note" });
  const tail = el("div", { class: "card-tail" }, prLink, checks, prRound, tailGo, tailNote);

  const row = el("li", {}, card, tail);
  row.refs = { card, chipSlot, whenSlot, title, repo, counts, winner, reviews, note, event, rail,
               tail, prLink, checks, prRound, tailGo, tailNote };
  return row;
}

function updateRunCard(row, run) {
  const r = row.refs;
  /* `waiting` is a field of its own on the summary precisely because the
     status string still names the node the run parked in. It wins: a run
     nobody is working on must not read as one that is being worked on. */
  const status = run.waiting ? "waiting" : String(run.status || "");
  const meta = RUN_STATUS[status] || {};
  const parked = isWaiting(run);
  const tone = toneOf(status, RUN_STATUS);

  r.card.setAttribute("href", `#/runs/${run.id}`);
  setAttr(r.card, "data-tone", tone);
  setAttr(row, "data-tone", tone);

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

  /* Spell out the endings that look like results but are not. `waiting` is
     excluded because the strip below says it better, and with a button. */
  const spell = Boolean(meta.note) && status !== "merged" && status !== "ready" && status !== "waiting";
  setText(r.note, spell ? meta.note : "");
  show(r.note, spell);

  const moving = !run.done;
  setText(r.event, moving && run.event ? run.event : "");
  show(r.event, Boolean(moving && run.event));

  /* A parked run keeps its rail so the operator can see how far it got, with
     the node it stopped in drawn halted rather than pulsing. */
  const ask = parked ? openFor(run.id)[0] : null;
  const rail = moving ? phaseRail(status, ask ? ask.node : null) : null;
  clear(r.rail);
  if (rail) r.rail.append(rail);

  updateRunTail(row, run, { parked, ask });
}

/* The strip under the card: where the pull request lives, and where the one
   action the operator can take on a run appears when there is one. */
function updateRunTail(row, run, { parked, ask }) {
  const r = row.refs;
  const pr = run.pr && typeof run.pr === "object" ? run.pr : null;

  // The href goes through `forgeUrl`: a run record is data magi wrote, but a
  // `javascript:` value in it would be one tap from running in the operator's
  // session, and a link that cannot be trusted is not shown at all.
  const prHref = pr ? forgeUrl(pr.url) : null;
  if (prHref) {
    setAttr(r.prLink, "href", prHref);
    setAttr(r.prLink, "title", prHref);
    setText(r.prLink, `PR #${pr.number}`);
  }
  show(r.prLink, Boolean(prHref));

  if (pr) r.checks.replaceChildren(checksChip(pr));
  show(r.checks, Boolean(pr));

  const rounds = pr ? Number(pr.rounds) || 0 : 0;
  setText(r.prRound, rounds ? `land round ${Number(pr.round) || 0} of ${rounds}` : "");
  show(r.prRound, rounds > 0);

  if (ask) {
    setAttr(r.tailGo, "href", "#/questions");
    setText(r.tailGo, "Answer");
    setAttr(r.tailGo, "aria-label", `Answer: ${ask.summary || "the open question"}`);
  }
  show(r.tailGo, Boolean(ask));

  const note = parked
    ? `Waiting on you: ${(ask && ask.summary) || "an agent asked for a decision."}`
    : run.waiting
      ? "Answered. The loop picks this up on its next tick."
      : pr
        ? landNote(pr)
        : "";
  setText(r.tailNote, note);
  show(r.tailNote, note !== "");

  const tailed = Boolean(pr) || Boolean(run.waiting);
  show(r.tail, tailed);
  setAttr(row, "data-tail", tailed ? "1" : null);
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

/* ---- questions --------------------------------------------------------- *
 * An agent inside a run can stop and ask the owner something, and the run
 * parks until it is answered. That makes an open question the only state in
 * this product where nothing anywhere is making progress and no timeout will
 * rescue it: the machine is burning nothing and going nowhere until a human
 * taps. So the question is put in front of the operator from wherever they
 * are — a band above every view, a count on the nav item and in the document
 * title — and the controls to answer it are rendered in place, because a
 * question you have to navigate somewhere else to answer is a question that
 * waits until morning.
 */
const openQuestions = () => (state.questions || []).filter((q) => q.status === "open");
const openFor = (runId) => openQuestions().filter((q) => q.run === runId);

/* Until /api/questions has answered, health's own count is what is known. */
function openCount() {
  return state.questions === null
    ? Number(state.health && state.health.questions_open) || 0
    : openQuestions().length;
}

function sortQuestions(list) {
  return list.slice().sort((a, b) => {
    const rank = (ASK_ORDER[a.status] ?? 3) - (ASK_ORDER[b.status] ?? 3);
    return rank || (Date.parse(b.asked_at) || 0) - (Date.parse(a.asked_at) || 0);
  });
}

/* RunSummary.waiting is one revision behind an answer the operator has just
   given. Once the questions are loaded they are the sharper truth: a run with
   no open question is not parked, whatever the summary still says. This is
   what makes answering read as immediate instead of as a round trip. */
function isWaiting(run) {
  if (!run.waiting) return false;
  return state.questions === null || openFor(run.id).length > 0;
}

/* ---- markdown-ish ------------------------------------------------------ *
 * A question's detail is agent-authored markdown and can be long. There is no
 * library here and there will not be one, so this handles the four things a
 * decision brief actually contains — fenced blocks, headings, dash lists and
 * paragraphs — plus inline code spans, and treats everything else as text.
 *
 * It returns nodes. Nothing from the API is ever assigned as markup anywhere
 * in this client, and least of all here: this string was written by a process
 * the operator did not author, and a fence containing a script tag has to
 * render as the characters of a script tag. */
function markdownish(text) {
  const lines = String(text).replace(/\r\n?/g, "\n").split("\n");
  const out = [];
  let paragraph = [];
  let bullets = null;

  const flush = () => {
    if (paragraph.length) {
      out.push(el("p", {}, inline(paragraph.join(" "))));
      paragraph = [];
    }
    if (bullets) {
      out.push(el("ul", {}, bullets.map((item) => el("li", {}, inline(item)))));
      bullets = null;
    }
  };

  for (let i = 0; i < lines.length; i += 1) {
    const fence = /^\s{0,3}(`{3,}|~{3,})/.exec(lines[i]);
    if (fence) {
      flush();
      const closer = fence[1][0].repeat(3);
      const body = [];
      i += 1;
      while (i < lines.length && !lines[i].trimStart().startsWith(closer)) {
        body.push(lines[i]);
        i += 1;
      }
      out.push(el("pre", {}, el("code", { text: body.join("\n") })));
      continue;
    }

    const heading = /^ {0,3}#{1,6}\s+(.*)$/.exec(lines[i]);
    if (heading) {
      flush();
      out.push(el("h4", {}, inline(heading[1].trim())));
      continue;
    }

    const bullet = /^ {0,3}[-*+]\s+(.*)$/.exec(lines[i]);
    if (bullet) {
      if (paragraph.length) flush();
      if (!bullets) bullets = [];
      bullets.push(bullet[1]);
      continue;
    }

    if (!lines[i].trim()) {
      flush();
      continue;
    }
    /* A wrapped continuation line belongs to whatever is already open. */
    if (bullets) bullets[bullets.length - 1] += ` ${lines[i].trim()}`;
    else paragraph.push(lines[i].trim());
  }

  flush();
  return out;
}

/* Code spans only. Emphasis is left alone on purpose: a parser cheap enough
   to belong in this file would read the asterisks in a glob as italics and
   silently eat them out of a path the operator was meant to see. */
function inline(text) {
  const nodes = [];
  String(text).split(/`([^`]+)`/g).forEach((part, i) => {
    if (part === "") return;
    nodes.push(i % 2 === 1 ? el("code", { text: part }) : document.createTextNode(part));
  });
  return nodes;
}

/* ---- question card ----------------------------------------------------- *
 * Reconciled rather than rebuilt, because the free-text box may hold a
 * half-typed answer: an SSE tick arriving mid-sentence must not throw it
 * away. */
function createAskCard() {
  const chipSlot = el("span");
  const whenSlot = el("time", { class: "ask-when" });
  /* tabindex -1 so the ask bar can put the caret on the question it sent the
     operator here to answer, instead of on the top of the document. */
  const summary = el("h2", { class: "ask-summary", tabindex: "-1" });
  const runLink = el("a", { class: "ask-seat" });
  const node = el("span");
  const seat = el("span", { class: "ask-seat" });
  const where = el("div", { class: "ask-where" }, runLink, node, seat);
  const detail = el("div");
  const hint = el("p", { class: "hint" });
  const choices = el("div", { class: "choices" });
  const text = el("textarea", { rows: "4", "aria-label": "Your answer" });
  const send = el("button", { class: "btn btn-gold", type: "button", text: "Send answer" });
  const free = el("div", { class: "ask-free" }, text, send);
  const error = el("p", { class: "form-error", role: "alert" });
  const answerLabel = el("span", { class: "answer-label" });
  const answerText = el("p", { class: "answer-text" });
  const answer = el("div", { class: "answer" }, answerLabel, answerText);
  const note = el("p", { class: "panel-note" });

  const row = el("li", { class: "ask" },
    el("div", { class: "ask-top" }, chipSlot, whenSlot),
    summary, where, detail, hint, choices, free, error, answer, note,
  );
  row.refs = { chipSlot, whenSlot, summary, runLink, node, seat, where, detail,
               hint, choices, text, send, free, error, answerLabel, answerText, answer, note };
  return row;
}

function updateAskCard(row, question, { compact = false } = {}) {
  const r = row.refs;
  const status = String(question.status || "open");
  const open = status === "open";
  const choices = Array.isArray(question.choices) ? question.choices : [];

  setAttr(row, "data-state", status);

  const next = chip(status, QUESTION_STATUS);
  if (r.chipSlot.firstChild) r.chipSlot.firstChild.replaceWith(next);
  else r.chipSlot.append(next);

  const settledAt = !open && question.answered_at ? question.answered_at : question.asked_at;
  const at = when(settledAt);
  setText(r.whenSlot, `${!open && question.answered_at ? "answered" : "asked"} ${at.text}`);
  setAttr(r.whenSlot, "datetime", settledAt || null);
  setAttr(r.whenSlot, "title", at.title);

  setText(r.summary, question.summary || firstLine(question.detail) || `question ${shortId(question.id)}`);

  setAttr(r.runLink, "href", `#/runs/${question.run}`);
  setText(r.runLink, `run ${shortId(question.run)}`);
  show(r.runLink, Boolean(question.run) && !compact);
  setText(r.node, question.node ? `node ${question.node}` : "");
  show(r.node, Boolean(question.node));
  setText(r.seat, question.seat ? `seat ${question.seat}` : "");
  show(r.seat, Boolean(question.seat));
  separate(r.where);

  /* The detail is immutable for a given question, so it is parsed once. An
     open question shows it outright — it is the case for the decision. A
     settled one folds it away, so the record does not push the next open
     question off a 390px screen. */
  const detail = typeof question.detail === "string" ? question.detail.trim() : "";
  const key = `${open ? "open" : "settled"}:${detail.length}`;
  if (row.dataset.detailKey !== key) {
    row.dataset.detailKey = key;
    clear(r.detail);
    if (detail) {
      const body = el("div", { class: "md" }, markdownish(detail));
      r.detail.append(open
        ? body
        : el("details", { class: "advanced" }, el("summary", { text: "Context" }), body));
    }
  }
  show(r.detail, detail !== "");

  const choiceKey = choices.join("\u0000");
  if (row.dataset.choiceKey !== choiceKey) {
    row.dataset.choiceKey = choiceKey;
    clear(r.choices);
    for (const choice of choices) {
      r.choices.append(el("button", {
        class: "btn", type: "button", text: choice,
        onclick: () => answerQuestion(question.id, { choice }, row),
      }));
    }
  }
  r.send.onclick = () => answerQuestion(question.id, { text: r.text.value }, row);

  setText(r.hint, !open
    ? ""
    : choices.length
      ? "Pick one. The run resumes as soon as you do."
      : "No options were offered \u2014 answer in your own words.");
  show(r.hint, open);
  show(r.choices, open && choices.length > 0);
  show(r.free, open && choices.length === 0);
  show(r.error, open && !r.error.hidden && r.error.textContent !== "");

  const given = question.answer && typeof question.answer === "object" ? question.answer : null;
  const value = given
    ? typeof given.choice === "string" ? given.choice : typeof given.text === "string" ? given.text : ""
    : "";
  if (value) {
    const decided = when(question.answered_at);
    setText(r.answerLabel, `Decided ${decided.text}`);
    setAttr(r.answerLabel, "title", decided.title);
    setText(r.answerText, value);
  }
  show(r.answer, Boolean(value));

  setText(r.note, status === "abandoned"
    ? "The run ended before this was answered, so nothing acted on it."
    : row.dataset.raced === "1"
      ? "This was answered elsewhere while you had it open. The recorded answer is above."
      : "");
  show(r.note, r.note.textContent !== "");
}

/* A 409 is not a failure worth a dialog: it means the operator answered from
   the terminal, or a second phone got there first. What matters is the answer
   that was actually recorded, so the list is refetched and the question is
   shown as settled with a line saying why it changed under them. */
async function answerQuestion(id, body, row) {
  const r = row.refs;
  const buttons = [...row.querySelectorAll("button")];
  const value = typeof body.choice === "string" ? body.choice : String(body.text || "");

  if (!value.trim()) {
    setText(r.error, "An answer cannot be empty.");
    show(r.error, true);
    r.text.focus();
    return;
  }

  show(r.error, false);
  for (const button of buttons) button.disabled = true;

  try {
    reflectQuestion(await postJson(API.answer(id), body));
    announce(`Answered: ${value.trim()}`);
    ok();
  } catch (error) {
    if (error.status === 409) {
      row.dataset.raced = "1";
      announce("That question had already been answered.");
      await loadQuestions();
      return;
    }
    setText(r.error, error.message);
    show(r.error, true);
  }
  for (const button of buttons) button.disabled = false;
}

/* Show the answer without waiting for the stream to confirm it, including in
   the runs list: the run stops reading as blocked-on-you the moment it stops
   being blocked on you. */
function reflectQuestion(question) {
  if (!question || typeof question !== "object" || !question.id) return;
  state.questions = sortQuestions([
    question,
    ...(state.questions || []).filter((q) => q.id !== question.id),
  ]);
  renderQuestions();
  renderAskBar();
  renderRuns();
  if (state.route.name === "run" && state.detail.run) renderRunDetail();
}

/* ---- ask bar and indicators -------------------------------------------- */

/* The band never renders when there is nothing to answer. A permanent "no
   questions" strip would train the operator to look straight past the place a
   real one appears, which is the one failure this feature cannot survive. */
function renderAskBar() {
  const bar = $("ask-bar");
  const count = openCount();
  const open = openQuestions();

  show(bar, count > 0);
  renderIndicators(count);
  if (count === 0) return;

  setText(bar.querySelector(".ask-bar-count"), count === 1
    ? "An agent is waiting on your decision"
    : `${count} agents are waiting on your decision`);

  /* The oldest one is quoted, because it is the one that has been blocking
     longest; the list is newest-first, so that is the last of them. */
  const oldest = open.length ? open[open.length - 1] : null;
  const line = bar.querySelector(".ask-bar-summary");
  setText(line, oldest ? oldest.summary || "" : "");
  show(line, Boolean(oldest && oldest.summary));
}

function renderIndicators(count) {
  for (const id of ["ask-badge-rail", "ask-badge-dock"]) {
    const badge = $(id);
    setText(badge, count > 99 ? "99+" : String(count));
    show(badge, count > 0);
  }
  for (const link of document.querySelectorAll('[data-nav="questions"]')) {
    setAttr(link, "aria-label", count > 0 ? `Questions, ${count} unanswered` : "Questions");
  }
  renderTitle();
}

/* The count rides on the document title as well, because a phone with the
   deck open in a background tab shows the title in the tab strip and in the
   app switcher — which is the only notification channel this UI has. */
function renderTitle() {
  const count = openCount();
  const base = state.route.name === "queue"
    ? "Backlog \u2014 magi"
    : state.route.name === "questions"
      ? "Questions \u2014 magi"
      : state.route.name === "run"
        ? `Run ${shortId(state.route.id)} \u2014 magi`
        : "magi \u2014 observation deck";
  document.title = count > 0 ? `(${count}) ${base}` : base;
}

function renderQuestions() {
  const list = $("questions-list");
  const questions = state.questions;

  if (questions === null) {
    setText($("questions-count"), "Loading\u2026");
    return;
  }

  const open = openQuestions().length;
  const settled = questions.length - open;
  setText($("questions-count"), questions.length === 0
    ? "Nothing asked yet"
    : open === 0
      ? `nothing open \u00b7 ${plural(settled, "decision on record", "decisions on record")}`
      : [`${plural(open, "question is blocking a run", "questions are blocking runs")}`,
         settled ? `${settled} on record` : null].filter(Boolean).join(" \u00b7 "));

  show($("questions-empty"), questions.length === 0);
  syncList(list, questions, (q) => q.id, createAskCard, (row, q) => updateAskCard(row, q));
}

/* The operator followed the band here to answer one specific thing; leaving
   the caret at the top of the document would make them find it again. */
function focusFirstAsk() {
  requestAnimationFrame(() => {
    const first = $("questions-list").querySelector('.ask[data-state="open"] .ask-summary');
    if (first) first.focus({ preventScroll: true });
  });
}

/* ---- landing ----------------------------------------------------------- *
 * After a run wins, the land loop opens a pull request and watches it. The
 * only thing the operator needs from this panel is whether it is their turn,
 * so every state below ends in a sentence that says so in words. */

/* `pr` is frozen on RunSummary; the detail payload is whatever the server
   chose to include, so the run is asked first and the list second. */
function landOf(run) {
  if (run.pr && typeof run.pr === "object") return run.pr;
  const summary = (state.runs || []).find((r) => r.id === run.id);
  return summary && summary.pr && typeof summary.pr === "object" ? summary.pr : null;
}

/* Red checks are not a verdict on the run: the loop answers them with another
   fixer round, and only becomes the operator's problem once the round budget
   is spent. Saying which of those it is, in words, is the part that does not
   depend on colour or on a glyph. */
function landNote(pr) {
  const rounds = Number(pr.rounds) || 0;
  const left = Math.max(rounds - (Number(pr.round) || 0), 0);
  if (pr.state === "merged") return "Merged. The land loop is finished with this run.";
  if (pr.state === "closed") return "The pull request was closed without merging. This one needs you.";
  if (pr.checks === "red") {
    return left > 0
      ? `Checks failed, so a fixer round is coming \u2014 ${plural(left, "round", "rounds")} of ${rounds} left. Nothing is needed from you.`
      : `Checks failed and all ${rounds} fix rounds are spent. This one needs you.`;
  }
  if (pr.checks === "pending") return "Waiting on the checks. Nothing is needed from you.";
  if (pr.checks === "green") return "Checks are green; the loop is taking it to merge.";
  return "The check state could not be read from the forge.";
}

function checksChip(pr) {
  const level = String(pr.checks || "unknown");
  const check = CHECKS[level] || CHECKS.unknown;
  return el("span", {
    class: "checks",
    "data-checks": level,
    "data-glyph": check.glyph,
    text: check.word,
  });
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
    /* Both of these are about a specific run, and a question belonging to a
       different one is not merely stale, it is wrong. */
    show($("run-ask-panel"), false);
    show($("run-land-panel"), false);
    setText($("run-report"), report === null ? "Loading\u2026" : report);
    return;
  }

  /* The detail payload carries the run's own status; the list carries the
     derived `waiting`. Prefer whichever says the run is parked, because that
     is the state the operator has to act on. */
  const summary = (state.runs || []).find((r) => r.id === run.id);
  const parkedNow = Boolean(summary && isWaiting(summary)) || openFor(run.id).length > 0;
  const status = parkedNow ? "waiting" : String(run.status || "");
  const meta = RUN_STATUS[status] || {};

  const head = $("run-status");
  clear(head);
  head.append(chip(status, RUN_STATUS));
  const parkedAt = parkedNow ? (openFor(run.id)[0] || {}).node || null : null;
  const rail = PHASES.includes(status) || parkedAt ? phaseRail(status, parkedAt) : null;
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

  renderAsks(run);
  renderLand(run);
  renderVerdict(run);
  renderCandidates(run);
  renderReviews(run);
  renderQuota(run);
  renderTimeline(run);

  setText($("run-report"), report === null ? "Loading\u2026" : report);
}

/* Every question this run has ever asked, open ones first: the answered ones
   are the record of the decisions that shaped the work below. They are
   answerable right here, so arriving from the runs list is not a detour. */
function renderAsks(run) {
  const mine = (state.questions || []).filter((q) => q.run === run.id);
  show($("run-ask-panel"), mine.length > 0);
  if (mine.length === 0) return;

  const open = mine.filter((q) => q.status === "open").length;
  setText($("run-ask-title"), open > 0 ? "Waiting on you" : "Decisions");
  setText($("run-ask-count"), open > 0 ? `${open} open` : plural(mine.length, "on record", "on record"));
  syncList($("run-asks"), sortQuestions(mine), (q) => q.id, createAskCard,
    (row, q) => updateAskCard(row, q, { compact: true }));
}

function renderLand(run) {
  const pr = landOf(run);
  show($("run-land-panel"), Boolean(pr));
  if (!pr) return;

  const box = $("run-land");
  clear(box);
  // Same reasoning as the card link: an untrusted scheme is rendered as plain
  // text rather than as something tappable.
  const prHref = forgeUrl(pr.url);
  box.append(
    el("div", { class: "land-top" },
      prHref
        ? el("a", {
            class: "pr-link", href: prHref, title: prHref,
            target: "_blank", rel: "noopener noreferrer",
            text: `PR #${pr.number}`,
          })
        : el("span", { class: "ask-seat", text: `PR #${pr.number}` }),
      el("span", { class: "tag", "data-tone": PR_TONE[pr.state] || "ink", text: pr.state || "unknown" }),
      checksChip(pr),
    ),
    Number(pr.rounds) ? el("p", { class: "land-note", text: `Land round ${Number(pr.round) || 0} of ${pr.rounds}.` }) : null,
    roundRail(pr),
    el("p", { class: "land-note", text: landNote(pr) }),
  );
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

/* Questions are loaded whole rather than by id: the list is short by nature —
   a backlog of them would mean the loop had been stalled for days — and one
   fetch keeps the runs list, the ask bar and the open run in agreement about
   which runs are parked. */
async function loadQuestions() {
  try {
    const list = await getJson(API.questions);
    state.questions = sortQuestions(Array.isArray(list) ? list : []);
    renderQuestions();
    renderAskBar();
    /* A run's `waiting` only means something next to the questions, so both
       views are re-rendered from the answer, not from the run revision. */
    renderRuns();
    if (state.route.name === "run" && state.detail.run) renderRunDetail();
    ok();
  } catch (error) {
    fail(`Could not load questions: ${error.message}`);
  }
}

async function loadHealth({ applyRevisions = false } = {}) {
  try {
    state.health = await getJson(API.health);
    renderDaemon();
    /* health.questions_open is the count until /api/questions has answered,
       so the indicator is right on the very first paint. */
    renderAskBar();
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
  const questionsRev = source.questions_rev;
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
  if (questionsRev !== state.rev.questions) {
    state.rev.questions = questionsRev;
    jobs.push(loadQuestions());
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
  if (parts[0] === "questions") return { name: "questions", id: null };
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
  show($("view-questions"), route.name === "questions");

  const section = route.name === "run" ? "runs" : route.name;
  for (const link of document.querySelectorAll("[data-nav]")) {
    setAttr(link, "aria-current", link.dataset.nav === section ? "page" : null);
  }

  if (route.name === "run") {
    if (state.detail.id !== route.id) loadRun(route.id);
  } else {
    state.detail = { id: null, run: null, report: null };
  }

  if (changed) window.scrollTo({ top: 0 });
  /* The operator arrived to answer one specific thing, so the caret goes on
     it rather than on the top of the document. */
  if (changed && route.name === "questions") focusFirstAsk();
  renderTitle();
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
    loadQuestions();
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
    state.rev.questions = state.health.questions_rev;
  }
  await Promise.allSettled([loadRuns(), loadQueue(), loadQuestions()]);

  subscribe();
  setInterval(() => {
    if (document.hidden) return;
    loadHealth({ applyRevisions: !state.streamOpen });
  }, HEALTH_MS);
}

boot();
