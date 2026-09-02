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
  /* Agent-authored HTML, served by its own endpoint so it lands in a
     sandboxed frame of its own document rather than in this one. */
  /* Ends in a filename on purpose: a panel references its attachments by bare
     name, and a document served at `.../panel` would resolve `shot.png` to
     `.../shot.png`, which is not where the assets are. `base-uri 'none'`
     forbids fixing that from inside the frame, which is why it is fixed here. */
  panel: (id) => `/api/questions/${encodeURIComponent(id)}/panel/index.html`,
  chats: "/api/chats",
  chat: (id) => `/api/chats/${encodeURIComponent(id)}`,
  say: (id) => `/api/chats/${encodeURIComponent(id)}/say`,
  file: (id) => `/api/chats/${encodeURIComponent(id)}/file`,
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

/* A planning conversation. `filed` is the only ending that produced
   something, so it is the only one that gets the verdict colour; an open
   interview is not work in flight anywhere, it is waiting on the operator,
   so it does not borrow the pulsing blue of a running run. */
const CHAT_STATUS = {
  open:      { glyph: "\u25cc", tone: "blue" },
  filed:     { glyph: "\u25c6", tone: "gold" },
  abandoned: { glyph: "\u2296", tone: "ink" },
};

/* The graph node the land loop asks its approval question from. Keyed on the
   node rather than on the summary text, and confirmed against the choice
   pair, because a routine question that happened to offer "merge" must not
   inherit the two-step guard and a real merge approval must never miss it. */
const MERGE_NODE = "land-approval";

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
  chats: null,
  chatDetail: { id: null, chat: null },
  /* The id of the conversation whose turn is in flight, and the transcript
     length it started from. One turn at a time is the whole rule: a second
     one fired into the same chat would interleave with the first. */
  chatBusy: null,
  busyTurns: 0,
  /* The message that turn is carrying, shown as the operator's bubble until
     the recorded transcript has caught up with it. */
  pending: null,
  waitFrom: 0,
  waitTimer: null,
  /* Problems the server found in a draft, kept per conversation so a
     re-render does not wipe the list the operator is working through. */
  chatProblems: { id: null, list: [] },
  /* Whether a question's panel endpoint actually answers. A sandboxed frame
     is opaque, so a 404 inside it is indistinguishable from a rendered
     panel; this is the answer to that, asked once per question. */
  panelOk: new Map(),
  rev: { queue: null, runs: null, questions: null, chats: null },
  streamOpen: false,
  wrap: false,
};

let fallbackTimer = null;

/* ---- transport --------------------------------------------------------- */
async function request(url, init) {
  const res = await fetch(url, init);
  if (!res.ok) {
    let message = `${res.status} ${res.statusText || "request failed"}`;
    let problems = null;
    try {
      const body = await res.json();
      if (body && typeof body.error === "string") message = body.error;
      /* POST /api/chats/{id}/file answers a bad draft with every problem it
         found. They are carried on the error so one pass of fixes is
         possible, instead of a rejection at a time. */
      if (body && Array.isArray(body.problems) && body.problems.length) problems = body.problems;
    } catch {
      /* an error body is not guaranteed to be JSON; the status stands in */
    }
    const error = new Error(message);
    error.status = res.status;
    if (problems) error.problems = problems;
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
  const outcome = el("span");
  const meta = el("div", { class: "card-meta" }, source, repo, attempts, outcome);
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
  card.refs = { card, chipSlot, priority, whenSlot, title, source, repo, attempts, outcome, note, error, instruction, runLink, hold };
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

  /* A task reads `done` the moment its run reaches a terminal success, and
     `ready` is one of those - the winner passed the gate but was never merged,
     because the run was configured not to. Side by side that looked like the
     Queue and the Runs page disagreeing, and it hid the fact that there is
     still something to land. Joined from the runs already loaded, so this
     costs no request and says nothing when the run is too old to be in the
     list. */
  const run = latest ? (state.runs || []).find((x) => x.id === latest) : null;
  const outcome = run && status === "done" && run.status !== "merged"
    ? `run ended ${run.status} — nothing merged it`
    : "";
  setText(r.outcome, outcome);
  show(r.outcome, Boolean(outcome));
  separate(r.outcome.parentNode);

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

/* ---- agent-authored panels --------------------------------------------- *
 * A question may hand over a whole HTML page instead of a paragraph: a table
 * of changed files, a coloured diff, an inline image. This client otherwise
 * refuses to put API data into markup at all, and that rule is not being
 * relaxed here — the panel is never parsed, inspected or inserted by this
 * document. It is fetched by the browser from its own endpoint into an
 * <iframe sandbox> carrying NO tokens, which means no script inside it runs,
 * it has no origin, and it can reach neither this document, nor its cookies,
 * nor localStorage. The endpoint additionally sends a Content-Security-Policy
 * of `default-src 'none'`, so nothing inside the frame can reach the network
 * either: a panel cannot beacon out through a remote image.
 *
 * Nothing below may add a sandbox token. `allow-scripts` would hand a
 * scriptable document to agent-authored HTML and `allow-same-origin` would
 * hand it this session; either one, and rendering the panel stops being
 * defensible. Everything the design wants is done in the frame's own CSS or
 * not at all.
 */

/* A tokenless frame is opaque in both directions, so a 404 inside it looks
   exactly like a rendered panel and would leave a silent blank hole where the
   evidence should be. The status is therefore asked for directly, once per
   question — the panel of a given question never changes — with HEAD, which
   the route answers identically to GET without sending the body. */
async function panelReachable(id) {
  if (state.panelOk.has(id)) return state.panelOk.get(id);
  let reachable = false;
  try {
    const res = await fetch(API.panel(id), { method: "HEAD", cache: "no-store" });
    reachable = res.ok;
  } catch {
    reachable = false;   /* the server went away; that is a failed panel too */
  }
  state.panelOk.set(id, reachable);
  return reachable;
}

/* The one place an iframe is built. `sandbox: ""` is deliberate and load
   bearing: `el` writes an empty attribute value for it, which is a sandbox
   with every capability withheld. An omitted `sandbox` attribute would be no
   sandbox at all, and any token inside it would give some of them back. */
function panelFrame(question, label) {
  return el("iframe", {
    src: API.panel(question.id),
    sandbox: "",
    referrerpolicy: "no-referrer",
    title: `${label}: ${question.summary || shortId(question.id)}`,
  });
}

function mountPanel(row, question) {
  const r = row.refs;
  clear(r.panelBox);

  const assets = Array.isArray(question.assets) ? question.assets : [];
  const full = el("button", {
    class: "btn btn-quiet", type: "button", text: "Full screen",
    "aria-label": `Open the panel full screen: ${question.summary || shortId(question.id)}`,
    onclick: () => openPanel(question),
  });
  const pending = el("p", { class: "frame-note", text: "Loading the panel\u2026" });
  r.panelBox.append(
    el("div", { class: "ask-panel-bar" },
      el("span", { class: "ask-panel-label", text: "Panel from the agent" }),
      full),
    pending,
  );

  panelReachable(question.id).then((reachable) => {
    if (row.dataset.panel !== question.id) return;   /* the row was reused */
    pending.remove();
    if (!reachable) {
      full.disabled = true;
      r.panelBox.append(el("div", { class: "frame-fail" },
        el("span", { text: "The agent attached a panel, but this server cannot serve it." }),
        el("span", { class: "hint", text: "The summary and the context above are all of it that survived \u2014 and the question is still answerable below." }),
      ));
      return;
    }
    r.panelBox.append(
      /* The sill: a fade at the bottom edge saying the panel continues past
         it. It is not the only signal, because a gradient is not a sentence
         and cannot be read out; the note below says the same thing in
         words. */
      el("div", { class: "frame-wrap" }, panelFrame(question, "Panel for"), el("div", { class: "frame-more" })),
      el("p", { class: "frame-note", text: `${assets.length ? `${plural(assets.length, "attachment", "attachments")} \u00b7 ` : ""}The panel scrolls inside this window. Full screen shows all of it.` }),
    );
  });
}

function renderPanel(row, question) {
  const r = row.refs;
  const wanted = question.panel === true;
  show(r.panelBox, wanted);
  if (!wanted) {
    if (row.dataset.panel) {
      row.dataset.panel = "";
      clear(r.panelBox);
    }
    return;
  }
  /* Mounted once. Re-mounting on every SSE tick would restart the frame's
     load and throw away wherever the operator had scrolled inside it. */
  if (row.dataset.panel === question.id) return;
  row.dataset.panel = question.id;
  mountPanel(row, question);
}

/* Full screen, which on a phone is where a unified diff becomes legible at
   all. The frame is built on open and dropped on close, so a dismissed
   dialog holds no live document and no decoded image. */
function openPanel(question) {
  const dialog = $("panel-full");
  setText($("panel-full-h"), question.summary || `Panel ${shortId(question.id)}`);
  const body = $("panel-full-body");
  clear(body);
  body.append(panelFrame(question, "Panel, full screen, for"));
  if (!dialog.open) dialog.showModal();
  requestAnimationFrame(() => $("panel-full-close").focus());
}

function closePanel() {
  const dialog = $("panel-full");
  if (dialog.open) dialog.close();
  clear($("panel-full-body"));
}

/* ---- merge approval ---------------------------------------------------- *
 * The land loop asks before it merges. Every other question in this product
 * chooses between two futures that can both be revisited; this one ends in a
 * merge, and magi has no way to take that back. So it does not get the same
 * card, and it does not get a single tap.
 */
function isMergeQuestion(question) {
  const choices = (Array.isArray(question.choices) ? question.choices : []).map((c) => String(c).toLowerCase());
  const pair = choices.includes("merge") && choices.includes("hold");
  return pair && (question.node === MERGE_NODE || choices.length === 2);
}

/* The answer is sent back exactly as the question spelt it, whatever case the
   node used, so the land loop's own comparison cannot miss it. */
function choiceNamed(question, want) {
  const choices = Array.isArray(question.choices) ? question.choices : [];
  return choices.find((choice) => String(choice).toLowerCase() === want) || want;
}

/* Two taps, not a timer. The first arms; the confirm row it reveals begins
   with the warning sentence and puts Cancel where the arm button just was, so
   the pixel under a thumb that was only scrolling is never the irreversible
   one. Nothing is disabled and nothing counts down, so an operator who means
   it is two deliberate taps away rather than made to wait. */
function renderStakes(row, question) {
  const r = row.refs;
  const armed = row.dataset.armed === "1";
  clear(r.stakes);

  r.stakes.append(el("p", { class: "stakes-what" },
    "Merging closes this run: the branch goes into ",
    el("span", { class: "ask-seat", text: "the base branch" }),
    " and magi has no undo for it.",
  ));

  if (armed) {
    const cancel = el("button", {
      class: "btn btn-quiet", type: "button", text: "Cancel",
      onclick: () => { row.dataset.armed = ""; renderStakes(row, question); },
    });
    r.stakes.append(el("div", { class: "stakes-confirm" },
      el("p", { class: "stakes-warn", text: "Tapping merge now merges it." }),
      el("div", { class: "stakes-row" },
        cancel,
        el("button", {
          class: "btn btn-gold", type: "button", text: "Yes, merge now",
          onclick: () => answerQuestion(question.id, { choice: choiceNamed(question, "merge") }, row),
        }),
      ),
    ));
    /* The caret lands on Cancel, never on the button that merges: a stray
       Enter after arming must not be the last thing that happens. */
    requestAnimationFrame(() => cancel.focus({ preventScroll: true }));
  } else {
    r.stakes.append(el("button", {
      class: "btn btn-gold stakes-arm", type: "button", text: "Merge this pull request\u2026",
      onclick: () => { row.dataset.armed = "1"; renderStakes(row, question); },
    }));
  }

  /* Hold is the safe answer, so it keeps a full target and a real edge
     instead of being demoted to a text link nobody can hit. */
  r.stakes.append(el("button", {
    class: "btn btn-quiet stakes-hold", type: "button", text: "Hold \u2014 do not merge",
    onclick: () => answerQuestion(question.id, { choice: choiceNamed(question, "hold") }, row),
  }));

  /* A land-approval question that also offered something else keeps those
     options: the two-step guard is for merge, not a reason to hide a choice
     the agent asked for. */
  for (const choice of Array.isArray(question.choices) ? question.choices : []) {
    const name = String(choice).toLowerCase();
    if (name === "merge" || name === "hold") continue;
    r.stakes.append(el("button", {
      class: "btn", type: "button", text: choice,
      onclick: () => answerQuestion(question.id, { choice }, row),
    }));
  }
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
  /* Both are always present and hidden until they apply, so reconciling a
     card never has to move a node the operator is mid-tap on. */
  const band = el("p", { class: "stakes-band" });
  const panelBox = el("div", { class: "ask-panel" });
  const stakes = el("div", { class: "stakes" });

  const row = el("li", { class: "ask" },
    band,
    el("div", { class: "ask-top" }, chipSlot, whenSlot),
    /* The panel sits above the prose: when there is one, it is the case for
       the decision and the detail is the footnote. */
    summary, where, panelBox, detail, hint, stakes, choices, free, error, answer, note,
  );
  row.refs = { chipSlot, whenSlot, summary, runLink, node, seat, where, detail,
               hint, choices, text, send, free, error, answerLabel, answerText, answer, note,
               band, panelBox, stakes };
  return row;
}

function updateAskCard(row, question, { compact = false } = {}) {
  const r = row.refs;
  const status = String(question.status || "open");
  const open = status === "open";
  const choices = Array.isArray(question.choices) ? question.choices : [];

  /* The land loop's approval question. It is detected here rather than
     styled by the server, because the client is what knows the difference
     between a card that can be tapped through and one that cannot. */
  const merge = isMergeQuestion(question);
  setAttr(row, "data-state", status);
  setAttr(row, "data-stakes", merge ? "merge" : null);
  setText(r.band, merge
    ? (open ? "Irreversible \u00b7 this merges the pull request" : "Merge decision")
    : "");
  show(r.band, merge);

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

  renderPanel(row, question);

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

  /* The choice set is keyed with the treatment as well, so a question that
     turns out to be a merge approval cannot keep a row of plain buttons. */
  const choiceKey = `${merge ? "merge" : "plain"}:${choices.join("\u0000")}`;
  if (row.dataset.choiceKey !== choiceKey) {
    row.dataset.choiceKey = choiceKey;
    row.dataset.armed = "";
    clear(r.choices);
    clear(r.stakes);
    if (merge) {
      renderStakes(row, question);
    } else {
      for (const choice of choices) {
        r.choices.append(el("button", {
          class: "btn", type: "button", text: choice,
          onclick: () => answerQuestion(question.id, { choice }, row),
        }));
      }
    }
  }
  r.send.onclick = () => answerQuestion(question.id, { text: r.text.value }, row);

  setText(r.hint, !open
    ? ""
    : merge
      ? "Read the panel, then decide. Nothing merges until you say so twice."
      : choices.length
        ? "Pick one. The run resumes as soon as you do."
        : "No options were offered \u2014 answer in your own words.");
  show(r.hint, open);
  show(r.stakes, open && merge);
  show(r.choices, open && !merge && choices.length > 0);
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
  /* Only the answer controls are locked while the answer is in flight. The
     panel's own controls are not part of the decision, and one of them is
     deliberately disabled when the panel failed to load — re-enabling it
     here would offer a full-screen view of something that is not there. */
  const buttons = [...row.querySelectorAll("button")].filter((b) => !b.closest(".ask-panel"));
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
      : state.route.name === "chats"
        ? "Planning \u2014 magi"
        : state.route.name === "chat"
          ? `Planning ${shortId(state.route.id)} \u2014 magi`
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

/* ---- planning conversation --------------------------------------------- *
 * `magi plan` interviews the operator and writes a task file. It does that by
 * handing the terminal to an agent CLI, which is exactly the thing a phone
 * does not have. So the same interview runs here, one turn at a time, driven
 * headlessly by the server, and ends in the same place: a validated task file
 * in the queue.
 *
 * Two facts shape everything below. A turn takes tens of seconds, because a
 * real model is reading and thinking — so the wait is stated in words, with a
 * count that visibly advances, and never as a bare spinner that is
 * indistinguishable from a server that has stopped answering. And a second
 * turn fired into the same conversation while the first is in flight would
 * interleave two half-exchanges, so exactly one is allowed to be outstanding
 * and the composer says why while it is.
 */
const chatTurns = (chat) => (chat && Array.isArray(chat.turns) ? chat.turns : []);

/* What the operator opened with, which is the only thing that names a
   conversation before it has produced a draft. */
function chatOpener(chat) {
  const first = chatTurns(chat).find((turn) => turn.who === "operator");
  return first ? String(first.body || "") : "";
}

const chatDraft = (chat) => (chat && typeof chat.draft === "string" ? chat.draft : "");

/* When the interviewing agent times out, fails or runs out of quota, the
   server still records the exchange and writes the failure as an agent turn
   whose body begins with `magi: `. That is magi speaking, not the model, and
   it must not be read as an answer to the operator's question — so it is
   drawn as a third kind of turn. */
const MAGI_PREFIX = "magi: ";
function turnWho(turn) {
  if (turn.who === "agent" && String(turn.body || "").startsWith(MAGI_PREFIX)) return "system";
  return turn.who === "operator" ? "operator" : "agent";
}

/* Open first, newest first inside each group: the same rule as the question
   list, for the same reason — the ones still needing the operator come
   first, and the filed ones are the record. */
function sortChats(list) {
  return list.slice().sort((a, b) => {
    const rank = (a.status === "open" ? 0 : 1) - (b.status === "open" ? 0 : 1);
    const when_ = (chat) => Date.parse(chat.updated_at || chat.created_at) || 0;
    return rank || when_(b) - when_(a);
  });
}

/* ---- the list ---------------------------------------------------------- */
function createChatCard() {
  const chipSlot = el("span");
  const ready = el("span", { class: "tag chat-ready", "data-tone": "gold", text: "draft ready" });
  const whenSlot = el("time", { class: "card-when" });
  const title = el("h2", { class: "card-title" });
  const agent = el("span", { class: "repo" });
  const turns = el("span");
  const task = el("span", { class: "win" });
  const meta = el("div", { class: "card-meta" }, agent, turns, task);
  const last = el("p", { class: "card-event" });

  const card = el("a", { class: "card" },
    el("div", { class: "card-top" }, chipSlot, ready, whenSlot),
    title, meta, last,
  );
  const row = el("li", {}, card);
  row.refs = { card, chipSlot, ready, whenSlot, title, agent, turns, task, last };
  return row;
}

function updateChatCard(row, chat) {
  const r = row.refs;
  const status = String(chat.status || "open");
  const turns = chatTurns(chat);
  const tone = toneOf(status, CHAT_STATUS);

  r.card.setAttribute("href", `#/plan/${chat.id}`);
  setAttr(r.card, "data-tone", tone);
  setAttr(row, "data-tone", tone);

  const next = chip(status, CHAT_STATUS);
  if (r.chipSlot.firstChild) r.chipSlot.firstChild.replaceWith(next);
  else r.chipSlot.append(next);

  /* A draft waiting to be filed is the one thing in this list that is
     actually the operator's turn, so it is called out on the card rather
     than found by opening each conversation. */
  show(r.ready, status === "open" && chatDraft(chat).trim() !== "");

  const at = when(chat.updated_at || chat.created_at);
  setText(r.whenSlot, at.text);
  setAttr(r.whenSlot, "datetime", chat.updated_at || chat.created_at);
  setAttr(r.whenSlot, "title", `updated ${at.title}`);

  setText(r.title, firstLine(chatOpener(chat)) || `conversation ${shortId(chat.id)}`);
  setText(r.agent, chat.agent || "");
  show(r.agent, Boolean(chat.agent));
  setText(r.turns, plural(turns.length, "turn", "turns"));
  setText(r.task, chat.task ? `task ${shortId(chat.task)}` : "");
  show(r.task, Boolean(chat.task));
  separate(r.turns.parentNode);

  const tail = turns.length ? turns[turns.length - 1] : null;
  setText(r.last, tail && tail.who === "agent" ? firstLine(tail.body) : "");
  show(r.last, Boolean(tail && tail.who === "agent"));
}

function renderChats() {
  const list = $("chats-list");
  const chats = state.chats;

  if (chats === null) {
    const open = Number(state.health && state.health.chats_open) || 0;
    setText($("chats-count"), open ? `${plural(open, "conversation open", "conversations open")}` : "Loading\u2026");
    return;
  }

  const open = chats.filter((c) => c.status === "open").length;
  const filed = chats.filter((c) => c.status === "filed").length;
  setText($("chats-count"), chats.length === 0
    ? "No interviews yet"
    : [open ? `${plural(open, "conversation open", "conversations open")}` : "nothing open",
       filed ? `${plural(filed, "task filed from here", "tasks filed from here")}` : null]
        .filter(Boolean).join(" \u00b7 "));

  show($("chats-empty"), chats.length === 0);
  syncList(list, sortChats(chats), (c) => c.id, createChatCard, updateChatCard);
}

/* ---- one conversation -------------------------------------------------- */
function createTurnRow() {
  const who = el("span", { class: "turn-who" });
  const body = el("div", { class: "turn-body" });
  const at = el("time", { class: "turn-at" });
  const row = el("li", { class: "turn" }, who, body, at);
  row.refs = { who, body, at };
  return row;
}

function updateTurnRow(row, item) {
  const r = row.refs;
  const turn = item.turn;
  const kind = turnWho(turn);
  const body = String(turn.body || "");

  setAttr(row, "data-who", kind);
  setText(r.who, kind === "operator" ? "You" : kind === "system" ? "magi" : "Agent");

  /* A turn never changes once it is on disk, so its body is built once. The
     agent's is markdown-ish prose and is rendered as nodes, never as markup:
     the rule that no API data is ever assigned as HTML holds everywhere
     outside the sandboxed panel frame, and a model that writes a script tag
     into a fence has to see the characters of one. */
  const key = `${kind}:${body.length}`;
  if (row.dataset.turnKey !== key) {
    row.dataset.turnKey = key;
    clear(r.body);
    if (kind === "agent") {
      r.body.append(el("div", { class: "md" }, markdownish(body)));
    } else {
      r.body.append(el("p", {
        class: "turn-text",
        text: kind === "system" ? body.slice(MAGI_PREFIX.length) : body,
      }));
    }
  }

  const at = when(turn.at);
  setText(r.at, at.text);
  setAttr(r.at, "datetime", turn.at || null);
  setAttr(r.at, "title", at.title);
}

function renderProblems(problems) {
  const box = $("chat-problems");
  clear(box);
  if (!problems || problems.length === 0) {
    show(box, false);
    return;
  }
  /* Every problem at once. The operator asked for one pass of fixes, not a
     rejection at a time, and the draft stays on screen above this. */
  box.append(
    el("h3", { text: `Not fileable yet \u2014 ${plural(problems.length, "problem", "problems")}` }),
    /* The validator names sections and symbols in backticks; `inline` turns
       those into code spans as nodes, so the operator reads `## Acceptance`
       as the heading it is and no markup is ever assigned. */
    el("ul", {}, problems.map((problem) => el("li", {}, inline(String(problem))))),
  );
  show(box, true);
}

function chatError(message) {
  const box = $("chat-error");
  setText(box, message || "");
  show(box, Boolean(message));
}

function renderChat() {
  const chat = state.chatDetail.chat;
  const busy = state.chatBusy !== null && state.chatBusy === state.chatDetail.id;

  if (!chat) {
    setText($("chat-h"), "Loading conversation\u2026");
    setText($("chat-meta"), "");
    clear($("chat-status"));
    clear($("chat-turns"));
    show($("chat-draft-panel"), false);
    show($("chat-filed-panel"), false);
    show($("chat-say"), false);
    show($("chat-closed"), false);
    show($("chat-wait"), false);
    show($("chat-problems"), false);
    return;
  }

  const status = String(chat.status || "open");
  /* The operator's own message, while the turn that carries it is still in
     flight. It is composed in here rather than pushed into the loaded chat,
     because the ten-second re-read below replaces that chat wholesale and
     would otherwise make the message the operator just sent vanish for the
     rest of the wait. */
  const pending = busy && state.pending && state.pending.id === chat.id
    && chatTurns(chat).length <= state.busyTurns
    ? [{ who: "operator", body: state.pending.body, at: state.pending.at }]
    : [];
  const turns = [...chatTurns(chat), ...pending];

  const head = $("chat-status");
  clear(head);
  head.append(chip(status, CHAT_STATUS));

  setText($("chat-h"), firstLine(chatOpener(chat)) || `Conversation ${shortId(chat.id)}`);
  const started = when(chat.created_at);
  setText($("chat-meta"),
    `${shortId(chat.id)} \u00b7 ${chat.agent || "agent"} \u00b7 ${plural(turns.length, "turn", "turns")} \u00b7 started ${started.text}`);
  setAttr($("chat-meta"), "title", `${chat.id}\nstarted ${started.title}`);

  /* Turns are append-only, so the index is a stable key and reconciling can
     never rebuild the transcript the operator is reading. */
  syncList($("chat-turns"), turns.map((turn, i) => ({ turn, key: String(i) })),
    (item) => item.key, createTurnRow, updateTurnRow);

  const draft = chatDraft(chat);
  const hasDraft = draft.trim() !== "";
  show($("chat-draft-panel"), hasDraft);
  if (hasDraft) setText($("chat-draft"), draft);
  setText($("chat-draft-tag"), status === "filed" ? "filed" : "draft");
  setAttr($("chat-draft-tag"), "data-tone", status === "filed" ? "teal" : "gold");
  show($("chat-file"), hasDraft && status === "open");
  renderProblems(state.chatProblems.id === chat.id ? state.chatProblems.list : []);

  show($("chat-filed-panel"), status === "filed");
  setText($("chat-filed-note"), chat.task
    ? `Filed as task ${shortId(chat.task)}. It is in the backlog now, and the loop claims it in priority order.`
    : "Filed into the backlog.");

  const canSay = status === "open";
  show($("chat-say"), canSay);
  show($("chat-closed"), !canSay);
  /* The guard against a second turn: the field and the button are both dead
     while one is outstanding, and the button says what it is waiting for
     rather than just greying out. */
  $("f-say").disabled = busy;
  $("chat-send").disabled = busy;
  setText($("chat-send"), busy ? "Thinking\u2026" : "Send");
  show($("chat-wait"), busy);
}

/* The wait, in words. The sentence in the live region changes only twice —
   once when the turn starts and once when it has been going long enough to
   need saying — while the seconds tick in a span that assistive technology
   never reads, because a counter announced every second is unusable. */
function tickWait() {
  const box = $("chat-wait");
  if (state.chatBusy === null) {
    show(box, false);
    return;
  }
  const secs = Math.max(Math.round((Date.now() - state.waitFrom) / 1000), 0);
  setText(box.querySelector(".waiting-text"), secs >= 90
    ? "The agent is still thinking. Long, but not stuck \u2014 it is allowed to take its time, and the reply will appear here."
    : "The agent is thinking about your message. A turn usually takes under a minute.");
  setText(box.querySelector(".waiting-secs"), `${secs}s`);
  show(box, state.chatBusy === state.chatDetail.id);

  /* Cheap insurance for the one case the button cannot cover: the turn was
     started somewhere else, or the stream is down, so nothing will tell this
     page that the reply has landed. */
  if (secs > 0 && secs % 10 === 0) loadChat(state.chatBusy);
}

function beginTurn(id, before) {
  state.chatBusy = id;
  state.busyTurns = before;
  state.waitFrom = Date.now();
  tickWait();
  if (!state.waitTimer) state.waitTimer = setInterval(tickWait, 1000);
}

function endTurn(id) {
  if (state.chatBusy !== id) return;
  state.chatBusy = null;
  state.pending = null;
  if (state.waitTimer) {
    clearInterval(state.waitTimer);
    state.waitTimer = null;
  }
  show($("chat-wait"), false);
}

async function loadChats() {
  try {
    const list = await getJson(API.chats);
    state.chats = Array.isArray(list) ? list : [];
    renderChats();
    ok();
  } catch (error) {
    fail(`Could not load conversations: ${error.message}`);
  }
}

async function loadChat(id) {
  if (state.chatDetail.id !== id) state.chatDetail = { id, chat: null };
  try {
    const chat = await getJson(API.chat(id));
    if (state.chatDetail.id !== id) return;   /* the operator navigated away */
    state.chatDetail.chat = chat;
    /* The transcript having grown by the operator's turn and a reply is what
       proves the turn finished, whoever started it and whether or not this
       page's own request has come back yet. */
    if (state.chatBusy === id && chatTurns(chat).length >= state.busyTurns + 2) endTurn(id);
    renderChat();
    ok();
  } catch (error) {
    fail(`Could not load conversation ${shortId(id)}: ${error.message}`);
  }
}

/* Starting an interview runs the agent's first turn, so this is as slow as
   any other turn and says so instead of leaving a dead button. */
async function startChat() {
  const box = $("f-idea");
  const error = $("chat-start-error");
  const go = $("chat-start-go");
  const idea = box.value;

  if (!idea.trim()) {
    setText(error, "Describe the idea first \u2014 a sentence is enough.");
    show(error, true);
    box.focus();
    return;
  }

  show(error, false);
  go.disabled = true;
  setText(go, "Starting\u2026");
  show($("chat-start-wait"), true);

  try {
    const chat = await postJson(API.chats, { idea, agent: null });
    state.chats = sortChats([chat, ...(state.chats || []).filter((c) => c.id !== chat.id)]);
    state.chatDetail = { id: chat.id, chat };
    box.value = "";
    renderChats();
    announce("The interview has started.");
    location.hash = `#/plan/${chat.id}`;
    ok();
  } catch (failure) {
    setText(error, failure.message);
    show(error, true);
  } finally {
    go.disabled = false;
    setText(go, "Start the interview");
    show($("chat-start-wait"), false);
  }
}

async function sendTurn(event) {
  event.preventDefault();
  const id = state.chatDetail.id;
  const box = $("f-say");
  const text = box.value;

  /* One turn at a time, checked here as well as by the disabled button: a
     double tap can beat a re-render, and a keyboard shortcut does not care
     that the button looks dead. */
  if (!id || state.chatBusy !== null) return;
  if (!text.trim()) {
    chatError("Say something first.");
    box.focus();
    return;
  }

  chatError("");
  const before = chatTurns(state.chatDetail.chat).length;
  beginTurn(id, before);

  /* The operator's own words go up immediately, held as the pending turn
     until the transcript on disk has grown past it. */
  state.pending = { id, body: text, at: new Date().toISOString() };
  box.value = "";
  renderChat();
  $("chat-wait").scrollIntoView({ block: "nearest" });

  try {
    const fresh = await postJson(API.say(id), { text });
    endTurn(id);
    if (state.chatDetail.id === id) {
      state.chatDetail.chat = fresh;
      renderChat();
      const tail = chatTurns(fresh);
      const last = tail.length ? tail[tail.length - 1] : null;
      announce(last && turnWho(last) === "system"
        ? "The interviewing agent could not answer. Its message is in the conversation."
        : "The agent replied.");
      const rows = $("chat-turns").children;
      if (rows.length) rows[rows.length - 1].scrollIntoView({ block: "nearest" });
    }
    loadChats();
    ok();
  } catch (error) {
    if (error.status === 409) {
      /* A turn is already running on this conversation — another phone, or a
         tap that beat the button being disabled. Nothing has gone wrong, so
         the wait stays up: the reply lands when it lands, and the revision
         or the ten-second re-read will bring it. */
      announce("A turn is already running on this conversation. Waiting for it.");
      return;
    }
    endTurn(id);
    chatError(`The turn could not be run: ${error.message}`);
    /* Whatever the server did or did not record is the truth, not the
       optimistic bubble above. */
    await loadChat(id);
  }
}

async function fileDraft() {
  const id = state.chatDetail.id;
  const button = $("chat-file");
  if (!id) return;

  button.disabled = true;
  setText(button, "Filing\u2026");
  state.chatProblems = { id, list: [] };
  renderProblems([]);
  chatError("");

  try {
    const body = await postJson(API.file(id), {});
    const task = body && typeof body.task === "string" ? body.task : null;
    announce(task ? `Filed as task ${shortId(task)}.` : "Filed.");
    await Promise.allSettled([loadChat(id), loadChats(), loadQueue()]);
    ok();
  } catch (error) {
    const list = Array.isArray(error.problems) && error.problems.length
      ? error.problems
      : [error.message];
    state.chatProblems = { id, list };
    renderProblems(list);
    announce(`The draft was not filed: ${plural(list.length, "problem", "problems")} to fix.`);
    $("chat-problems").scrollIntoView({ block: "nearest" });
  } finally {
    button.disabled = false;
    setText(button, "File this task");
  }
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
  const chatsRev = source.chats_rev;
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
  /* A turn landing on disk is what bumps this, so it is also how the reply
     reaches a phone whose own POST is still outstanding. */
  if (chatsRev !== state.rev.chats) {
    state.rev.chats = chatsRev;
    jobs.push(loadChats());
    if (state.route.name === "chat" && state.chatDetail.id) jobs.push(loadChat(state.chatDetail.id));
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
  /* `plan` rather than `chats`, because that is the command it replaces. */
  if (parts[0] === "plan" && parts[1]) return { name: "chat", id: decodeURIComponent(parts[1]) };
  if (parts[0] === "plan") return { name: "chats", id: null };
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
  show($("view-chats"), route.name === "chats");
  show($("view-chat"), route.name === "chat");

  const section = route.name === "run" ? "runs" : route.name === "chat" ? "chats" : route.name;
  for (const link of document.querySelectorAll("[data-nav]")) {
    setAttr(link, "aria-current", link.dataset.nav === section ? "page" : null);
  }

  if (route.name === "run") {
    if (state.detail.id !== route.id) loadRun(route.id);
  } else {
    state.detail = { id: null, run: null, report: null };
  }

  /* A conversation that is not on screen is dropped so the next one cannot
     flash the previous transcript first. The in-flight turn is deliberately
     not cancelled: it is running on the server either way, and coming back
     to the conversation re-reads it. */
  if (route.name === "chat") {
    if (state.chatDetail.id !== route.id) {
      state.chatDetail = { id: route.id, chat: null };
      loadChat(route.id);
    }
    /* Rendered unconditionally: starting an interview sets the conversation
       and then changes the hash, so by the time this runs the chat is already
       loaded and the id already matches. Rendering only on a change left that
       path showing "Loading conversation" forever. */
    renderChat();
  } else if (state.chatDetail.id) {
    state.chatDetail = { id: null, chat: null };
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
    loadChats();
    if (state.route.name === "run" && state.detail.id) loadRun(state.detail.id);
    if (state.route.name === "chat" && state.chatDetail.id) loadChat(state.chatDetail.id);
  });

  $("chat-start-go").addEventListener("click", startChat);
  $("chat-say").addEventListener("submit", sendTurn);
  $("chat-file").addEventListener("click", fileDraft);
  /* Enter inserts a newline, because on a phone that is the only way to type
     a paragraph. Ctrl or Cmd with Enter sends, for the desktop. */
  $("f-say").addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      $("chat-say").requestSubmit();
    }
  });

  $("panel-full-close").addEventListener("click", closePanel);
  /* Escape closes a dialog without a click, so the frame is dropped from the
     close event rather than from the button: a dismissed panel must not go
     on holding a live document. */
  $("panel-full").addEventListener("close", () => clear($("panel-full-body")));

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
    state.rev.chats = state.health.chats_rev;
  }
  await Promise.allSettled([loadRuns(), loadQueue(), loadQuestions(), loadChats()]);

  subscribe();
  setInterval(() => {
    if (document.hidden) return;
    loadHealth({ applyRevisions: !state.streamOpen });
  }, HEALTH_MS);
}

boot();
