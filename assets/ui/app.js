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
  deleteRun: (id) => `/api/runs/${encodeURIComponent(id)}`,
  foldRun: (id) => `/api/runs/${encodeURIComponent(id)}/fold`,
  resumeRun: (id) => `/api/runs/${encodeURIComponent(id)}/resume`,
  report: (id) => `/api/runs/${encodeURIComponent(id)}/report`,
  queue: "/api/queue",
  deleteTask: (id) => `/api/queue/${encodeURIComponent(id)}`,
  hold: (id) => `/api/queue/${encodeURIComponent(id)}/hold`,
  release: (id) => `/api/queue/${encodeURIComponent(id)}/release`,
  priority: (id) => `/api/queue/${encodeURIComponent(id)}/priority`,
  editTask: (id) => `/api/queue/${encodeURIComponent(id)}/edit`,
  doneTask: (id) => `/api/queue/${encodeURIComponent(id)}/done`,
  questions: "/api/questions",
  answer: (id) => `/api/questions/${encodeURIComponent(id)}/answer`,
  questionSay: (id) => `/api/questions/${encodeURIComponent(id)}/say`,
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
  /* The standing chat: a separate store from Planning's, so the two never
     appear in each other's lists (see `POST /api/talks`'s doc). */
  talks: "/api/talks",
  talk: (id) => `/api/talks/${encodeURIComponent(id)}`,
  talkSay: (id) => `/api/talks/${encodeURIComponent(id)}/say`,
  talkClose: (id) => `/api/talks/${encodeURIComponent(id)}/close`,
  talkReopen: (id) => `/api/talks/${encodeURIComponent(id)}/reopen`,
  talkDelete: (id) => `/api/talks/${encodeURIComponent(id)}`,
  /* Local checkouts under `[repos] roots`, for the repository pickers on the
     "start a conversation" panel and the "continue in another repository"
     action. `?refresh=1` bypasses the server's cache regardless of its TTL. */
  repos: "/api/repos",
  reposRefresh: "/api/repos?refresh=1",
  /* `magi plan`'s headless design-deliberation stage: run from a terminal, so
     the phone's only way to learn a draft's raw advisor record is to ask for
     the list magi itself wrote to disk. */
  drafts: "/api/drafts",
  draftAdvisors: (id) => `/api/drafts/${encodeURIComponent(id)}/advisors`,
  /* The loop itself: GET reports it, POST {running} starts or stops the one
     inside this server. */
  loop: "/api/loop",
  upgrade: "/api/upgrade",
  events: "/api/events",
};

const RUN_LIMIT = 50;
/* The daemon's own file is refreshed every 5s and is treated as dead at 30s,
   so asking twice per staleness window is enough to never show a false
   "running" for long. */
const HEALTH_MS = 10000;
/* How long this page keeps saying "stopping" on the strength of its own
   request alone. A stop asked of an idle loop lands within one 5s poll and
   the next view reports it, so this only has to outlast that \u2014 and it
   must not be forever, or a request the server dropped would leave a strip
   promising a stop that is never coming and no control to retry with. */
const STOP_ASK_MS = 20000;
/* How long a specific announcement holds the live region against the generic
   "Updated." that follows a refresh. Long enough to cover the refresh a
   change of state triggers, short enough that the next real refresh is still
   announced. */
const QUIET_HOLD_MS = 4000;

/* Stages `health.upgrade.stage` can be while something is actually moving -
   everything between "the binary is being replaced" and "the address has
   been handed to the successor". Not `done` or `failed`: those are the two
   ways an upgrade stops moving. */
const UPGRADE_BUSY_STAGES = new Set(["downloading", "replaced", "parking", "restarting"]);
/* The ceiling on how long this page keeps quietly waiting for an upgrade to
   finish - by reconnecting on its own, or by rendering the busy stages above
   - before it says a human needs to look. A park waits for the run in flight
   to reach its next node boundary, which can take as long as
   `timeout_implement` (an hour, by default) for a run mid-implement, and that
   whole wait is meant to look like patience, not failure. The margin past an
   hour covers the download-and-replace step ahead of it and normal clock
   skew between this page and the deck. */
const UPGRADE_WAIT_LIMIT_MS = 70 * 60 * 1000;

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
  /* Neither of these names a cause. A stall has several, and the run's own
     last line — shown for finished runs too — says which one it was; quota
     losses are counted separately above, from `losses`, so a note that
     assumed them contradicted the card it sat on. */
  stalled:      { glyph: "\u26a0", tone: "rust", note: "The judging panel never reached a quorum, so no verdict was recorded. The work is kept." },
  blocked:      { glyph: "\u2298", tone: "rust", note: "magi stopped short of merging." },
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

/* The standing chat only ever has two states - see `talk::TalkStatus` - so
   there is no third entry here for a filed or abandoned conversation. */
const TALK_STATUS = {
  open:   { glyph: "\u25cc", tone: "blue" },
  closed: { glyph: "\u2296", tone: "ink" },
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
   the same rail cannot mean "working" and "halted" on colour alone.

   `note`, when given, is which seat(s) in the current phase have not answered
   yet — the answer to "is it agy again?" without opening the seats panel.
   It only ever touches the label: the rail's squares are one per *node*, not
   one per seat, so a seat that is still out changes what the current square
   means, not how many squares there are. */
function phaseRail(status, node, note) {
  const parked = phaseOf(node);
  const at = PHASES.indexOf(parked || status);
  if (at < 0) return null;
  const rail = el("div", {
    class: "phases",
    role: "img",
    "aria-label": parked
      ? `Stopped at phase ${at + 1} of ${PHASES.length}, ${parked}, waiting for your answer`
      : `Phase ${at + 1} of ${PHASES.length}: ${status}${note ? ` — ${note}` : ""}`,
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

/* Declared ahead of `state` below on purpose: `state`'s own initializer calls
   loadCollapsed(), which reads these — and a `const` used before its
   declaration line throws (temporal dead zone), even from inside a function,
   the moment that function actually runs. That would have been swallowed by
   loadCollapsed()'s own try/catch and silently read back `{}` every time,
   which is indistinguishable from "localStorage denied" and just as wrong. */
const RUNS_COLLAPSE_KEY = "magi-runs-sections";
const QUEUE_COLLAPSE_KEY = "magi-queue-sections";

/* ---- state ------------------------------------------------------------- */
const state = {
  route: { name: "runs", id: null },
  health: null,
  /* The last LoopView seen. Health carries one too; this is what a POST to
     /api/loop leaves behind, so the strip reflects a start or a stop before
     the next health tick. */
  loop: null,
  /* When this page's own stop request was accepted, in epoch millis, or 0.
     The server only reports `stopping` while a run is actually in flight: a
     loop asked to stop while idle answers "still running" and goes quiet a
     poll later, which without this looked like the tap had done nothing. */
  stopAskedAt: 0,
  runs: null,
  /* Which node of the Runs tree is narrowing the card list, or neither set
     when nothing is picked. Lives only in memory — reloading the page always
     starts from the unfiltered list, since a filter is a lens on what's on
     screen right now, not a saved view. */
  runsFilter: { section: null, repo: null },
  /* Open/closed per Runs section, restored from localStorage so a collapse
     survives a reload; defaults to open (see isSectionOpen()). */
  runsCollapsed: loadCollapsed(RUNS_COLLAPSE_KEY),
  queue: null,
  /* Same idea for the Backlog's sections, kept separately since the two
     views don't share section keys or default open/closed state. */
  queueCollapsed: loadCollapsed(QUEUE_COLLAPSE_KEY),
  detail: { id: null, run: null, report: null },
  questions: null,
  chats: null,
  /* Local checkouts under `[repos] roots`, shared by both repository
     pickers - the one on the "start a conversation" panel and the one on
     "continue in another repository". `null` before the first load. */
  repos: null,
  /* `magi plan` drafts that finished a design-deliberation stage, from
     `GET /api/drafts`. `null` before the first load; fetched once on
     arrival and again on an explicit tap, the same as `repos` above - there
     is no revision to poll, since the CLI process that writes these files is
     not this server. */
  drafts: null,
  /* The raw advisor record currently open below the drafts list, or `null`
     when nothing is open. `error` carries a failed fetch so the panel can say
     so instead of silently staying empty. */
  draftDetail: { id: null, advice: null, error: null },
  chatDetail: { id: null, chat: null },
  /* Conversations with a turn in flight, keyed by chat id: `Map<id, {
       since, target, pending, waitFrom, lastPoll }>`. One turn *per chat* is
     the rule the server enforces (`Ui::begin_turn` refuses a second one on
     the *same* chat, see `chat_say`'s doc) - nothing here refuses sending
     into a different, idle conversation just because this one is busy.
       - `since`: transcript length before this turn's own new turns, used
         only to know when `pending` has been superseded by the real thing.
       - `target`: transcript length once this turn's agent reply (or
         failure note) has landed - always `since-of-the-turn-itself + 1`,
         one more turn than whatever the 202 response already carried.
       - `pending`: the operator's own text, shown as an optimistic bubble
         until the transcript catches up to it - `null` when there is
         nothing to show that is not already in the transcript (starting an
         interview, or a wait rebuilt from `thinking` - see `trackIfThinking`).
       - `waitFrom`: when the wait began, for the "Xs" counter.
       - `lastPoll`: last time this id's ten-second insurance re-read ran.
     An entry's presence is this browser's own belief that a turn is
     running; `thinking` on a fetched [`ChatView`] is the server's - see
     `trackIfThinking` for how the two are reconciled after a reload or on
     another device. */
  chatWaits: new Map(),
  /* Whether the current view was entered via a route change to a chat.
     Cleared after the first scroll, so a subsequent renderChat() with the
     same turn count does not re-scroll. */
  openingChat: false,
  /* Whether a Plan control asked to focus the idea box on arrival. Set by
     openPlan() when the planning page is not showing, cleared by applyRoute
     once the caret has landed. */
  planFocus: false,
  /* Turn count from the previous renderChat() call, used to detect new
     turns arriving while the conversation is already on screen. */
  prevTurnCount: 0,
  /* One shared ticking interval driving every entry in `chatWaits` at once -
     the seconds counter for whichever chat is on screen, and the
     ten-second insurance re-read for every busy chat, on screen or not.
     Started when `chatWaits` gains its first entry, stopped when it empties. */
  waitTimer: null,
  /* Problems the server found in a draft, kept per conversation so a
     re-render does not wipe the list the operator is working through. */
  chatProblems: { id: null, list: [] },
  /* Whether a question's panel endpoint actually answers. A sandboxed frame
     is opaque, so a 404 inside it is indistinguishable from a rendered
     panel; this is the answer to that, asked once per question. */
  panelOk: new Map(),
  /* The standing chat - a separate store and a separate piece of state from
     `chats`/`chatDetail`, on the same reasoning `talk::Talks` documents: two
     conversations with different meanings must not share one list or one
     turn guard. */
  talks: null,
  talkDetail: { id: null, talk: null },
  talkBusy: null,
  talkBusyTurns: 0,
  talkPending: null,
  talkWaitFrom: 0,
  talkWaitTimer: null,
  rev: { queue: null, runs: null, questions: null, chats: null, talks: null, loop: null },
  streamOpen: false,
  wrap: false,
  /* The upgrade stage last rendered, so a transition into "done" can be told
     apart from just being on it already - the loop strip re-renders on every
     health poll, and only a transition is worth announcing. */
  lastUpgradeStage: null,
  /* The draft panel's own view toggle: formatted by default, the exact bytes
     that would be filed one tap away. Global rather than per-chat - there is
     only ever one draft panel on screen at a time. */
  draftRaw: false,
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

const deleteReq = (url) => request(url, { method: "DELETE" });

/* ---- alert ------------------------------------------------------------- */
function fail(message) {
  const box = $("alert");
  setText(box.querySelector(".alert-text"), message);
  show(box, true);
}

function ok() {
  show($("alert"), false);
}

/* When something specific was last announced. A refresh finishing says only
   "Updated.", and it was landing in the live region a fraction of a second
   after a sentence that mattered \u2014 "Loop stopped." \u2014 which for a
   screen reader means the sentence was never read at all. */
let saidAt = 0;

function announce(message) {
  saidAt = Date.now();
  setText($("live"), message);
}

/* A courtesy announcement: it is skipped rather than allowed to overwrite a
   specific one that is still being read. */
function announceQuietly(message) {
  if (Date.now() - saidAt < QUIET_HOLD_MS) return;
  setText($("live"), message);
}

/* ---- loop strip -------------------------------------------------------- *
 * The loop runs inside this server, so this strip is both the report and the
 * control. Before it was, a task filed from a phone sat in the queue until
 * somebody reached a keyboard and typed `magi serve`, and the strip's own
 * wording said so — which is the one place this product still sent its
 * operator to a terminal.
 *
 * Every branch below ends in a sentence about whether anything is going to
 * happen, because that is the question being asked, and the states differ in
 * what the answer costs: `waiting` needs a tap, `stopping` needs patience,
 * and a loop this page does not own needs neither. */

/* Tasks the loop could claim right now, or null while the queue has not
   answered yet. Counted from the queue this page already holds rather than
   from health, because "off, with two tasks waiting" is the state that has to
   be right the moment it becomes true. */
function runnableTasks() {
  if (state.queue === null) return null;
  return state.queue.filter((task) => (task.status_str || task.status) === "queued").length;
}

/* The run the loop is on, as a link when there is one to point at. */
function currentRunLink(daemon) {
  const id = daemon.current && daemon.current.run;
  if (!id) return null;
  return el("a", {
    class: "daemon-run",
    href: `#/runs/${id}`,
    text: shortId(id),
    title: `run ${id}`,
  });
}

/* What starting the loop is about to do. Said in full next to the button
   rather than hidden behind a confirmation: starting is safe to repeat, but
   it opens an implementation competition, and an operator who did not know
   that would be surprised by the bill and not by the interface. */
function startCost(loop) {
  const mode = typeof loop.merge === "string" && loop.merge
    ? ` Merges as \u2018${loop.merge}\u2019.`
    : "";
  return `It claims the highest-priority task and runs an implementation competition, which spends agent calls.${mode}`;
}

/* Has `upgrade` been busy longer than this page is willing to wait quietly?
   See `UPGRADE_WAIT_LIMIT_MS` for why that ceiling is where it is. */
function upgradeOverdue(upgrade) {
  const startedAt = Date.parse(upgrade.started_at);
  return Number.isFinite(startedAt) && Date.now() - startedAt > UPGRADE_WAIT_LIMIT_MS;
}

/* The headline for a busy upgrade stage. Kept short: the sentence that
   actually says what is happening is `upgrade.waiting_on` or
   `upgradeStageDetail`, next to it. */
function upgradeStageLabel(stage) {
  switch (stage) {
    case "downloading": return "Replacing the binary.";
    case "replaced": return "Binary replaced.";
    case "parking": return "Parking before it restarts.";
    case "restarting": return "Restarting.";
    default: return "Upgrading.";
  }
}

/* A generic sentence for a busy stage, used when the server has nothing more
   specific to say - `upgrade.waiting_on` is preferred when it is set, which
   is only while `parking` names a run it is actually waiting on. */
function upgradeStageDetail(stage) {
  switch (stage) {
    case "downloading": return "Fetching and installing the new binary. This takes a few seconds.";
    case "replaced": return "About to hand the address to the successor.";
    case "parking": return "Nothing was in flight; handing the address to the successor next.";
    case "restarting": return "The address is released and the successor is starting. This page reconnects on its own.";
    default: return "";
  }
}

function renderLoop() {
  const box = $("daemon");
  const text = box.querySelector(".daemon-text");
  const why = $("loop-why");
  const button = $("loop-toggle");
  /* A past upgrade failure, folded into whatever note the loop's own state
     below already shows, rather than replacing it. `Stage::Failed` is
     terminal on the server and nothing clears it automatically, so taking
     the whole strip over for it - as the busy stages do, which is fine
     because those are transient - would leave start/stop/park unreachable
     from the phone until a fresh upgrade attempt happened to overwrite the
     record. Declared here, before `quiet`/`control` close over it, and
     assigned once the upgrade stage is known below. */
  let upgradeFailNote = "";

  const quiet = (note) => {
    const full = [note, upgradeFailNote].filter(Boolean).join(" ");
    setText(why, full);
    show(why, Boolean(full));
    show(button, false);
    button.onclick = null;
  };

  const park = $("loop-park");
  show(park, false);
  park.disabled = false;
  const upgradeBtn = $("loop-upgrade");
  show(upgradeBtn, false);

  /* The build answering, straight from /api/health - which is the only place
     that knows it, and until now the only place it could be read at all: an
     operator who tapped Update & restart had to curl the server to find out
     whether the new binary came back. Hidden rather than guessed while the
     first health tick is outstanding. */
  const versionChip = $("loop-version");
  const version = state.health && typeof state.health.version === "string"
    ? state.health.version.trim()
    : "";
  setText(versionChip, version ? `v${version}` : "");
  setAttr(versionChip, "title", version ? `magi ${version} is serving this page` : null);
  show(versionChip, Boolean(version));


  const control = (kind, label, note) => {
    setText(why, [note, upgradeFailNote].filter(Boolean).join(" "));
    show(why, true);
    setAttr(button, "data-kind", kind);
    setText(button, label);
    show(button, true);
    button.disabled = false;
    button.onclick = () => setLoop(kind === "start");
  };

  /* Offered only while a stop is waiting out a run, which is the moment the
     wait is actually felt. A park stops at the run's next node boundary: the
     work already recorded is kept and the run comes back as resumable, so the
     binary can be replaced without waiting out a competition and without
     throwing away an hour of paid agent calls. */
  const parkControl = (label, note) => {
    setText(park, label);
    setAttr(park, "title", note);
    show(park, true);
    park.onclick = () => setLoop(false, true);
  };

  if (!state.health) {
    setAttr(box, "data-state", null);
    setAttr(box, "data-owned", null);
    setText(text, "Connecting\u2026");
    setText(versionChip, "");
    show(versionChip, false);
    quiet(null);
    return;
  }

  /* An upgrade this deck set in motion, ahead of every other loop state:
     while the binary is being replaced or the deck is waiting to hand the
     address over, that is the one fact on screen worth reporting, and the
     states below either do not apply yet (the successor has not started, so
     `loop`/`daemon` here are still this process's own) or say nothing about
     why the deck went quiet. */
  /* Named `upgradeInfo` rather than `upgrade`: this scope also has to say
     `upgradeBtn.onclick = upgrade` further down, naming the function that
     posts `/api/upgrade` - a `const upgrade` here would shadow it for the
     rest of this function and silently turn that click handler into data. */
  const upgradeInfo = state.health.upgrade || null;
  const upgradeStage = upgradeInfo ? upgradeInfo.stage : null;

  if (upgradeStage && UPGRADE_BUSY_STAGES.has(upgradeStage)) {
    const overdue = upgradeOverdue(upgradeInfo);
    setAttr(box, "data-state", overdue ? "failed" : "upgrading");
    setAttr(box, "data-owned", null);
    clear(text);
    text.append(el("b", {
      text: overdue ? "The upgrade is taking longer than expected." : upgradeStageLabel(upgradeStage),
    }));
    quiet(overdue
      ? `Asked for ${upgradeInfo.to || "an update"} more than an hour ago and has not come back. Check on it by hand.`
      : (upgradeInfo.waiting_on || upgradeStageDetail(upgradeStage)));
    state.lastUpgradeStage = upgradeStage;
    return;
  }

  /* The transition into "done" or "failed" is what is worth announcing -
     being on either already (a fresh page load after the fact) is not news.
     `failed` does not take the strip over the way the busy stages above do:
     it is terminal on the server and nothing clears it on its own, so a
     takeover here would have permanently hidden start/stop/park behind an
     upgrade notice the operator has no way to dismiss. `upgradeFailNote`
     carries it into the loop's own note instead, below. */
  if (upgradeStage === "done" && UPGRADE_BUSY_STAGES.has(state.lastUpgradeStage)) {
    announce(`Updated to ${upgradeInfo.to || "the new build"} \u2014 back and running.`);
  }
  if (upgradeStage === "failed" && state.lastUpgradeStage !== "failed") {
    announce(`The upgrade to ${upgradeInfo.to || "a new release"} did not complete.${upgradeInfo.detail ? ` ${upgradeInfo.detail}` : ""} The loop itself is unaffected.`);
  }
  state.lastUpgradeStage = upgradeStage;
  setAttr(box, "data-upgrade-failed", upgradeStage === "failed" ? "yes" : null);
  if (upgradeStage === "failed") {
    upgradeFailNote = `The last upgrade to ${upgradeInfo.to || "a new release"} did not complete${upgradeInfo.detail ? ` (${upgradeInfo.detail})` : ""} \u2014 check on it by hand.`;
  }

  const loop = state.health.loop || state.loop || {};
  const daemon = loop.daemon || state.health.daemon || {};
  clear(text);

  /* `completed` is null until the loop has written a heartbeat, and "0 tasks
     done" is a different claim from "not reporting yet", so the count is
     omitted rather than guessed. */
  const done = Number(daemon.completed);
  const tail = Number.isFinite(done) ? ` \u00b7 ${plural(done, "task done", "tasks done")}` : "";
  /* Running at all, from either side: `loop.running` is this process's own
     flag and flips the instant a start is accepted, while `daemon.running`
     comes from the heartbeat file and lags it by up to a poll. Trusting only
     the file showed "Loop is off", with a Start button, for seconds after the
     operator had already started it. */
  const running = Boolean(loop.running) || Boolean(daemon.running);
  /* A loop somebody else's process owns. It is reported by the heartbeat
     (`daemon.running`) while this process's own flag stays false, which is
     also why `running` above is not enough to tell the two apart. */
  const foreign = Boolean(daemon.running) && loop.owned === false && !loop.running;
  setAttr(box, "data-owned", foreign ? "no" : null);

  /* Offered whenever this process owns the deck, running or not, *and* a
     newer release is actually known to exist: the binary can be replaced
     either way, and an operator with fixes waiting should not have to start
     the loop to install them. Hidden when the loop belongs to somebody else,
     because replacing this binary would leave that process running an old
     one against the same queue - and hidden with nothing to install, because
     restarting for an upgrade that would not happen used to park the run in
     flight and drop every connection for nothing. The version this deck is
     actually running is shown unconditionally, next to the strip, whether or
     not there is anything newer. */
  const update = state.health.update || { available: false, to: null };
  show(upgradeBtn, !foreign && update.available);
  if (!foreign && update.available && upgradeBtn.dataset.armed !== "yes") {
    setText(upgradeBtn, update.to ? `Update to ${update.to}` : "Update & restart");
    upgradeBtn.disabled = false;
    upgradeBtn.onclick = upgrade;
  }

  /* Asked to stop. Two shapes reach here, and both are checked before
     `running`, because the loop is still running while it winds down:

     `loop.stopping` is the server saying a run is in flight, which it will
     finish \u2014 killing the graph mid-node would leave worktrees, branches
     and agent sessions behind and throw away calls already paid for, so magi
     does not do it, and this can last tens of minutes.

     A stop asked of an idle loop never reports `stopping` at all: it answers
     "still running" and goes quiet within a poll. Without `stopAskedAt` that
     read as a button that did nothing, so this page remembers its own
     request \u2014 bounded, so a request that somehow did not take gives the
     control back instead of claiming forever that a stop is coming. */
  const asked = !foreign && state.stopAskedAt > 0 && Date.now() - state.stopAskedAt < STOP_ASK_MS;
  if (running && (loop.stopping || asked)) {
    setAttr(box, "data-state", "stopping");
    const link = loop.stopping ? currentRunLink(daemon) : null;
    text.append(
      el("b", { text: "Stopping." }),
      loop.stopping
        ? [link ? " It is finishing run " : " It is finishing the run it is on", link, " first, and will not abandon it."]
        : " It stops as soon as it finishes the poll it is on.",
    );
    quiet(loop.stopping
      ? `Nothing new will be claimed after it${tail}. You can start it again once it has stopped.`
      : `Nothing new will be claimed${tail}. This takes a few seconds when no run is in flight.`);
    if (loop.parking) {
      text.append(" Parking at the next step.");
    } else if (loop.stopping) {
      parkControl("Park at the next step",
        "Stops the run after the step it is on and leaves it resumable, instead of waiting for the whole competition. Use this when you want to replace the binary.");
    }
    return;
  }

  if (!running) {
    const waiting = runnableTasks();
    const error = typeof loop.last_error === "string" && loop.last_error.trim() ? loop.last_error : null;

    /* The loop died rather than being stopped. Kept until the next start
       clears it, and it is the only way somebody holding a phone learns the
       difference \u2014 so it is rendered as its own state, in the fault
       colour the two ordinary "off" states deliberately avoid. */
    if (error) {
      setAttr(box, "data-state", "failed");
      text.append(el("b", { text: "The loop stopped on an error." }), " ", error);
      control("start", "Start the loop again", `${waiting ? `${plural(waiting, "task", "tasks")} still waiting. ` : ""}Starting it clears this error. ${startCost(loop)}`);
      return;
    }

    if (waiting) {
      /* The state this strip exists for. Gold, not rust: the loop being off
         is not a fault, it is a tap away \u2014 and a fault colour here taught
         the operator to ignore the one band that needed them. */
      setAttr(box, "data-state", "waiting");
      text.append(
        el("b", { text: `${plural(waiting, "task", "tasks")} waiting.` }),
        " The loop is off, so nothing will be claimed until you start it.",
      );
      control("start", "Start the loop", startCost(loop));
      return;
    }
    setAttr(box, "data-state", "off");
    text.append(
      el("b", { text: "Loop is off." }),
      waiting === null
        ? " Nothing has been claimed."
        : " Nothing is queued, so nothing is waiting.",
    );
    control("start", "Start the loop", `${startCost(loop)} Until one is filed it just watches the queue.`);
    return;
  }

  if (daemon.current && daemon.current.run) {
    setAttr(box, "data-state", "working");
    text.append(
      el("b", { text: "Working" }),
      " on ",
      currentRunLink(daemon),
      daemon.current.task
        ? el("span", { class: "daemon-run", text: ` \u2190 task ${shortId(daemon.current.task)}` })
        : null,
      tail,
    );
  } else if (daemon.idle) {
    setAttr(box, "data-state", "idle");
    text.append(el("b", { text: "Loop idle." }), ` Nothing runnable in the queue${tail}.`);
  } else if (daemon.running) {
    setAttr(box, "data-state", "working");
    text.append(el("b", { text: "Working." }), ` Claiming a task${tail}.`);
  } else {
    /* Started here, no heartbeat on disk yet. Said as its own sentence rather
       than borrowed from `idle`, because "nothing runnable in the queue" would
       be a claim about the queue that nothing has checked. */
    setAttr(box, "data-state", "working");
    text.append(el("b", { text: "Started." }), " Waiting for the loop\u2019s first heartbeat.");
  }

  /* Someone started the loop in another process. Both endpoints answer 409
     for a loop this one does not own, so no button is offered: a control that
     silently fails is worse than none. The queue is being drained either way,
     which is the part the operator actually needs to know. */
  if (foreign) {
    const pid = Number(daemon.pid);
    quiet(`${Number.isFinite(pid) && pid ? `Process ${pid} owns` : "Another process owns"} this loop, so this page can watch it but not stop it. The queue is being drained regardless \u2014 nothing is waiting on you.`);
    return;
  }

  /* Two different promises, because they are two different facts: with a run
     in flight the operator is being told they will wait for it, and with none
     they are being told there is nothing to wait for. */
  control("stop", "Stop the loop", daemon.current && daemon.current.run
    ? "It finishes the run it is on first, then stops claiming. Nothing in flight is abandoned."
    : "It stops claiming new tasks. Nothing is in flight, so nothing is interrupted.");
}

/* Start or stop the loop in this server. Neither direction is guarded by a
   second tap: starting is repeatable and stopping is not destructive.

   Neither direction is believed on request, either. A stop is accepted while
   the loop is still running \u2014 reported as `stopping` when a run is in
   flight, and as plain "running" when there is none and it will go quiet a
   poll later \u2014 so "stopped" is announced from a view where `running` has
   actually gone false, not from the tap. A 409 is followed by a refetch,
   which is what replaces a button this page cannot honour with the sentence
   explaining why. */
/* Replace the binary and come back on it.
 *
 * The one thing the deck could not do for itself: `cargo install` cannot
 * overwrite a running executable, so every fix waited for a competition to end
 * or went in with the deck stopped. `kaishin` renames the running image aside
 * instead, so only the restart needs arranging - and the run in flight is
 * parked at its next node boundary first, which is why this costs at most one
 * step rather than a whole competition.
 *
 * The server answers 202 and then exits, so there is nothing to await here
 * beyond that acknowledgement: the phone learns the deck is back the same way
 * it learns everything else, by reconnecting. */
/* The upgrade's own line under the strip. Kept out of `fail`'s alert: this is
   not an error, and it has to survive the change-stream refreshes that redraw
   the strip while the loop parks. */
function quietNote(text) {
  const why = $("loop-why");
  if (!why) return;
  setText(why, text);
  show(why, Boolean(text));
}

async function upgrade() {
  const btn = $("loop-upgrade");
  if (!confirmed(btn, "Replace the binary and restart?")) return;
  btn.disabled = true;
  setText(btn, "Upgrading\u2026");
  try {
    const out = await postJson(API.upgrade);
    ok();
    const detail = out.detail || "The deck is replacing itself and will come back.";
    announce(detail);
    /* Nothing newer to install: say so and give the button back, rather than
       leaving "Upgrading…" on a deck that did not move. */
    if (!out.to) {
      setText(btn, "Update & restart");
      btn.disabled = false;
      quietNote(detail);
      return;
    }
    /* A park waits for the node in flight, which can be an hour. Leaving the
       button reading "Upgrading…" for that long is the same mistake as an
       error rendered off screen: it looks wedged. The strip says what it is
       waiting for, and the phone finds out it is back by reconnecting. */
    setText(btn, "Parking, then restarting\u2026");
    quietNote(detail);
  } catch (error) {
    setText(btn, "Update & restart");
    btn.disabled = false;
    fail(`Could not upgrade: ${error.message}`);
  }
}

/* One tap arms, the second commits, and the label says which state it is in.
   Used for the upgrade because it ends the process the operator is talking
   to - and a mis-tap that restarts the deck mid-competition is the kind of
   thing a phone in a pocket does. */
function confirmed(btn, question) {
  if (btn.dataset.armed === "yes") {
    btn.dataset.armed = "";
    return true;
  }
  btn.dataset.armed = "yes";
  setText(btn, question);
  setTimeout(() => {
    if (btn.dataset.armed === "yes") {
      btn.dataset.armed = "";
      setText(btn, "Update & restart");
    }
  }, 6000);
  return false;
}

async function setLoop(running, park = false) {
  const button = $("loop-toggle");
  const parkBtn = $("loop-park");
  button.disabled = true;
  if (park) {
    parkBtn.disabled = true;
    setText(parkBtn, "Parking\u2026");
  } else {
    setText(button, running ? "Starting\u2026" : "Stopping\u2026");
  }
  try {
    const view = await postJson(API.loop, { running, park });
    /* Set before applyLoop, so the render that follows already knows this
       page asked \u2014 that is what puts an idle loop into `stopping`. */
    state.stopAskedAt = running ? 0 : Date.now();
    applyLoop(view);
    ok();
    announce(running
      ? "Loop started. It claims the highest-priority task next."
      : view.stopping
        ? "Loop asked to stop. It finishes the run it is on first."
        : "Loop asked to stop. It goes quiet within a few seconds.");
  } catch (error) {
    /* 409 is not a failure of this page: the loop is already running, or it
       belongs to another process. The server's own message names which, and
       the pid when there is one, so it is shown verbatim. */
    fail(error.status === 409
      ? `The loop did not change: ${error.message}`
      : `Could not ${running ? "start" : "stop"} the loop: ${error.message}`);
    await loadLoop();
  } finally {
    renderLoop();   /* restores the label, whichever way it went */
  }
}

/* One place where a LoopView lands, so /api/loop and the `loop` block inside
   /api/health cannot disagree about what is on screen. Also where a stop this
   page asked for is finally confirmed: the loop going quiet is the event, and
   it arrives on a later view rather than in the answer to the request. */
function applyLoop(view) {
  const stopped = state.stopAskedAt > 0 && view && !view.running && !view.stopping;
  state.loop = view;
  if (state.health) state.health.loop = view;
  if (stopped) {
    state.stopAskedAt = 0;
    announce("Loop stopped.");
  }
  renderLoop();
}

async function loadLoop() {
  try {
    applyLoop(await getJson(API.loop));
  } catch (error) {
    fail(`Could not read the loop: ${error.message}`);
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
  /* Two attempts at one task are two cards with the same title, and the deck
     used to give no hint which was which - "why are there two of the same,
     one stalled and one blocked?" was the reasonable question. The older one
     now says what replaced it. Sits with the note rather than in the chip
     row: it explains the card's standing, and a chip would read as another
     status. */
  const superseded = el("p", { class: "card-note card-superseded" });
  const event = el("p", { class: "card-event" });
  const rail = el("div");

  const card = el("a", { class: "card" },
    el("div", { class: "card-top" }, chipSlot, whenSlot),
    title, meta, note, superseded, event, rail,
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
  row.refs = { card, chipSlot, whenSlot, title, repo, counts, winner, reviews, note, superseded,
               event, rail, tail, prLink, checks, prRound, tailGo, tailNote };
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

  const later = typeof run.superseded_by === "string" ? run.superseded_by : null;
  setText(r.superseded, later ? `Superseded by ${later} \u2014 a later attempt at the same task.` : "");
  show(r.superseded, Boolean(later));

  /* The run's own last line, on finished runs as well as moving ones. It used
     to be hidden the moment a run stopped, which is exactly when it is worth
     most: a `stalled` card then explained itself with a generic note while
     "verdict rests on 1 of 3 judges (quorum 2)" sat unread in the record, and
     a `blocked` one offered a guess with an "or" in it instead of "no check
     status is readable on the pull request". */
  const moving = !run.done;
  setText(r.event, run.event || "");
  show(r.event, Boolean(run.event));

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

/* ---- runs: grouping into sections ------------------------------------- *
 * The plain, updated-first list stops being readable once a few dozen runs
 * pile up, so it is split into the four questions an operator actually asks:
 * is anything waiting on me, what's moving, what landed, and what didn't.
 * `waiting` (the field, not the refined isWaiting() the card tail uses) wins
 * over status here on purpose \u2014 a run parked on a question is the one
 * thing that needs a human regardless of which node it stopped in. */
const RUN_SECTIONS = [
  { key: "waiting", label: "Waiting on you", defaultOpen: true },
  { key: "flight", label: "In flight", defaultOpen: true },
  { key: "landed", label: "Landed", defaultOpen: true },
  { key: "ended", label: "Ended", defaultOpen: true },
];

function runSection(run) {
  if (run.waiting) return "waiting";
  const status = String(run.status || "");
  if (status === "merged" || status === "ready") return "landed";
  if (status === "stalled" || status === "blocked" || status === "failed") return "ended";
  return "flight";
}

/* Which older attempts fold into which card. `superseded_by` names the
   *successor*'s short id, so a chain is walked forward from an attempt to
   whatever replaced it until nothing newer is known. The run that walk ends
   on is the one shown; everything behind it folds under that card.

   Walking stops the moment a `superseded_by` names a short id this page has
   never heard of \u2014 cut off by `limit`, or unreadable \u2014 and the run
   in hand is shown as-is rather than assumed superseded by something it
   cannot point at. That is what keeps a run from disappearing when the
   response happens to omit the attempt that replaced it.

   It also has to stop on a cycle \u2014 two runs naming each other, however that
   record came to be \u2014 without losing either one. Walking one run at a time
   with only its own path in hand would resolve A to B and B to A: neither
   satisfies "this run is its own head", so neither ever reaches `heads`
   below and the pair vanishes silently. Each walk here instead remembers
   its whole path and, on closing a loop, mints the run the loop closed on as
   the head for every run on that path \u2014 itself included \u2014 so a cycle always
   resolves to one real, present run rather than to none. */
function foldRuns(runs) {
  const byShort = new Map();
  for (const run of runs) if (run.short) byShort.set(run.short, run);
  const nextOf = (run) => (run.superseded_by && byShort.get(run.superseded_by)) || null;

  const headOf = new Map();
  for (const run of runs) {
    if (headOf.has(run.id)) continue;
    const path = [];
    const atIndex = new Map();
    let cur = run;
    while (!headOf.has(cur.id) && !atIndex.has(cur.id)) {
      atIndex.set(cur.id, path.length);
      path.push(cur);
      const next = nextOf(cur);
      if (!next) break;
      cur = next;
    }
    const head = headOf.get(cur.id) || cur;
    for (const node of path) headOf.set(node.id, head);
  }

  const heads = [];
  const childrenOf = new Map();
  for (const run of runs) {
    const head = headOf.get(run.id);
    if (head.id === run.id) {
      heads.push(run);
    } else {
      if (!childrenOf.has(head.id)) childrenOf.set(head.id, []);
      childrenOf.get(head.id).push(run);
    }
  }
  return { heads, childrenOf };
}

/* Section order preserved from RUN_SECTIONS; run order within a section
   preserved from the order `heads` arrived in, which is /api/runs' own
   updated-first order. */
function groupBySection(heads) {
  const bySection = new Map(RUN_SECTIONS.map((s) => [s.key, []]));
  for (const run of heads) bySection.get(runSection(run)).push(run);
  return bySection;
}

const repoLabel = (run) => run.repo_name || run.repo || "Unknown repository";

/* The wide-screen tree: section, then repo, each carrying the count of cards
   \u2014 cards, not raw runs, so this number always means the same thing as
   the section heading it rolls up to. Built from the *unfiltered* heads, so
   picking a node never shrinks the tree out from under the tap that picked
   it. Sections and repos with nothing in them are left out rather than shown
   at zero: an empty branch is not something to file into. */
function buildRunsTree(bySection) {
  const sections = [];
  for (const { key, label } of RUN_SECTIONS) {
    const heads = bySection.get(key);
    if (heads.length === 0) continue;
    const byRepo = new Map();
    for (const run of heads) {
      const repo = repoLabel(run);
      if (!byRepo.has(repo)) byRepo.set(repo, 0);
      byRepo.set(repo, byRepo.get(repo) + 1);
    }
    const repos = [...byRepo.entries()]
      .sort((a, b) => a[0].localeCompare(b[0]))
      .map(([repo, count]) => ({ repo, count }));
    sections.push({ key, label, count: heads.length, repos });
  }
  return sections;
}

/* Only ever one filter active at a time: a section, or a section plus one of
   its repos. There is no URL for it \u2014 the tree is a lens on the list
   already on screen, not a place worth deep-linking to. */
function matchesFilter(run) {
  const { section, repo } = state.runsFilter;
  if (!section) return true;
  if (runSection(run) !== section) return false;
  return !repo || repoLabel(run) === repo;
}

function selectRunsFilter(section, repo) {
  const same = state.runsFilter.section === section && state.runsFilter.repo === (repo || null);
  state.runsFilter = same ? { section: null, repo: null } : { section, repo: repo || null };
  renderRuns();
}

function clearRunsFilter() {
  state.runsFilter = { section: null, repo: null };
  renderRuns();
}

function renderRunsTree(sections) {
  const nav = $("runs-tree");
  show(nav, sections.length > 0);

  /* The tree is rebuilt from scratch below rather than reconciled node by
     node — it is small, at most four sections and a handful of repos each —
     but a full rebuild would otherwise drop keyboard focus on every poll, so
     whichever node has it is found again afterwards by the (section, repo)
     it names rather than by identity. */
  const active = document.activeElement;
  const focused = nav.contains(active)
    ? { section: active.dataset.section, repo: active.dataset.repo || null }
    : null;

  if (sections.length === 0) {
    clear(nav);
    return;
  }
  const root = el("ul", { class: "runs-tree-list" });
  for (const section of sections) {
    const on = state.runsFilter.section === section.key && !state.runsFilter.repo;
    const sub = el("ul", { class: "runs-tree-sub" });
    for (const r of section.repos) {
      const repoOn = state.runsFilter.section === section.key && state.runsFilter.repo === r.repo;
      sub.append(el("li", {},
        el("button", {
          class: "runs-tree-node runs-tree-repo",
          type: "button",
          "data-section": section.key,
          "data-repo": r.repo,
          "aria-current": repoOn ? "true" : null,
          onclick: () => selectRunsFilter(section.key, r.repo),
        },
          el("span", { class: "runs-tree-label", text: r.repo }),
          el("span", { class: "runs-tree-count", text: String(r.count) }),
        ),
      ));
    }
    root.append(el("li", {},
      el("button", {
        class: "runs-tree-node",
        type: "button",
        "data-section": section.key,
        "aria-current": on ? "true" : null,
        onclick: () => selectRunsFilter(section.key, null),
      },
        el("span", { class: "runs-tree-label", text: section.label }),
        el("span", { class: "runs-tree-count", text: String(section.count) }),
      ),
      sub,
    ));
  }
  clear(nav);
  nav.append(root);

  if (focused) {
    const match = [...nav.querySelectorAll(".runs-tree-node")].find((node) =>
      node.dataset.section === focused.section && (node.dataset.repo || null) === focused.repo);
    if (match) match.focus();
  }
}

function renderRunsFilterBar() {
  const bar = $("runs-filter");
  const { section, repo } = state.runsFilter;
  if (!section) {
    show(bar, false);
    return;
  }
  const label = (RUN_SECTIONS.find((s) => s.key === section) || {}).label || section;
  setText($("runs-filter-text"), `Showing ${label}${repo ? ` \u203a ${repo}` : ""}.`);
  show(bar, true);
}

/* ---- shared: section collapse, kept in localStorage --------------------- *
 * One mechanism, two independent users (Runs, Backlog): each keeps its own
 * storage key and its own per-section default open/closed state, since
 * neither shares section keys with the other. */
function loadCollapsed(storageKey) {
  try {
    const raw = localStorage.getItem(storageKey);
    const parsed = raw ? JSON.parse(raw) : null;
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};   /* localStorage denied (private mode) or the value was junk */
  }
}

function saveCollapsed(storageKey, collapsed) {
  try {
    localStorage.setItem(storageKey, JSON.stringify(collapsed));
  } catch {
    /* localStorage denied in private mode; the choice just won't outlive the tab */
  }
}

function isSectionOpen(collapsed, key, defaultOpen) {
  const saved = collapsed[key];
  return typeof saved === "boolean" ? saved : defaultOpen;
}

/* ---- shared: one section (a native <details>, for free keyboard support) *
 * `def` is one entry of a *_SECTIONS array: { key, label, defaultOpen }. */
function createSection(def, collapsed, storageKey) {
  const count = el("span", { class: "list-section-count" });
  const summary = el("summary", { class: "list-section-head" },
    el("h2", { class: "list-section-title", text: def.label }), count);
  const list = el("ol", { class: "cards" });
  const details = el("details", {
    class: "list-section",
    open: isSectionOpen(collapsed, def.key, def.defaultOpen),
  }, summary, list);
  details.dataset.key = def.key;
  details.addEventListener("toggle", () => {
    collapsed[def.key] = details.open;
    saveCollapsed(storageKey, collapsed);
  });
  details.refs = { summary, count, list };
  return details;
}

/* Keyed reconcile across sections, the same shape as syncList() above but one
   level up: a section that empties out (everything in it superseded, or
   filtered away) is removed rather than left on screen at "0 items". */
function syncSections(root, sectionDefs, itemsByKey, createFn, updateFn) {
  const existing = new Map();
  for (const child of root.children) existing.set(child.dataset.key, child);

  let previous = null;
  for (const def of sectionDefs) {
    const items = itemsByKey.get(def.key) || [];
    if (items.length === 0) continue;
    let node = existing.get(def.key);
    if (node) existing.delete(def.key);
    else node = createFn(def);
    updateFn(node, items);
    const wanted = previous ? previous.nextSibling : root.firstChild;
    if (node !== wanted) root.insertBefore(node, wanted);
    previous = node;
  }
  for (const stale of existing.values()) stale.remove();
}

/* ---- runs: one section, plus the folded-attempts count Backlog has no
   equivalent of ------------------------------------------------------- */
function createRunSection(def) {
  const section = createSection(def, state.runsCollapsed, RUNS_COLLAPSE_KEY);
  const folded = el("span", { class: "runs-section-folded" });
  section.refs.summary.append(folded);
  section.refs.folded = folded;
  return section;
}

function updateRunSection(node, heads, childrenOf) {
  const foldedTotal = heads.reduce((sum, run) => sum + (childrenOf.get(run.id) || []).length, 0);
  setText(node.refs.count, plural(heads.length, "run", "runs"));
  setText(node.refs.folded, foldedTotal
    ? `, ${plural(foldedTotal, "earlier attempt", "earlier attempts")} folded`
    : "");
  syncList(node.refs.list, heads, (r) => r.id, createRunRow,
    (row, run) => updateRunRow(row, run, childrenOf.get(run.id) || []));
}

function syncRunSections(root, bySection, childrenOf) {
  syncSections(root, RUN_SECTIONS, bySection, createRunSection,
    (node, heads) => updateRunSection(node, heads, childrenOf));
}

/* ---- runs: one row, a card plus its folded-away earlier attempts ------- *
 * createRunCard()/updateRunCard() build and fill the card itself and are
 * left untouched; the folded list is a sibling appended to the same <li>,
 * because the card is a single <a> and an anchor may not contain another
 * interactive element. */
function createRunRow() {
  const row = createRunCard();
  const summary = el("summary", { class: "run-folded-summary" });
  const list = el("ul", { class: "run-folded-list" });
  const folded = el("details", { class: "run-folded advanced" }, summary, list);
  row.append(folded);
  row.refs.folded = folded;
  row.refs.foldedSummary = summary;
  row.refs.foldedList = list;
  return row;
}

function updateRunRow(row, run, children) {
  updateRunCard(row, run);
  const list = row.refs.foldedList;
  clear(list);
  for (const child of children) {
    const at = when(child.updated_at || child.created_at);
    list.append(el("li", {},
      el("a", { class: "run-folded-link", href: `#/runs/${child.id}` },
        el("span", { class: "run-folded-id", text: child.short || shortId(child.id) }),
        el("span", { class: "run-folded-status", text: child.waiting ? "waiting" : String(child.status || "") }),
        el("time", { class: "run-folded-when", text: at.text, title: at.title }),
      ),
    ));
  }
  setText(row.refs.foldedSummary, children.length
    ? plural(children.length, "earlier attempt", "earlier attempts")
    : "");
  show(row.refs.folded, children.length > 0);
}

function renderRuns() {
  const runs = state.runs;
  const sectionsRoot = $("runs-sections");

  if (runs === null) {
    setText($("runs-count"), "Loading\u2026");
    show($("runs-tree"), false);
    show($("runs-filter"), false);
    if (!sectionsRoot.dataset.skeleton) {
      clear(sectionsRoot);
      const list = el("ol", { class: "cards" });
      for (let i = 0; i < 3; i += 1) {
        list.append(el("li", { class: "card skeleton" },
          el("div", { class: "bar", style: "width:34%" }),
          el("div", { class: "bar", style: "width:88%;height:18px" }),
          el("div", { class: "bar", style: "width:56%" }),
        ));
      }
      sectionsRoot.append(list);
      sectionsRoot.dataset.skeleton = "1";
    }
    return;
  }

  if (sectionsRoot.dataset.skeleton) {
    clear(sectionsRoot);
    delete sectionsRoot.dataset.skeleton;
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

  const { heads, childrenOf } = foldRuns(runs);
  renderRunsTree(buildRunsTree(groupBySection(heads)));
  renderRunsFilterBar();
  const visible = heads.filter(matchesFilter);
  syncRunSections(sectionsRoot, groupBySection(visible), childrenOf);
  show($("runs-filter-empty"), heads.length > 0 && Boolean(state.runsFilter.section) && visible.length === 0);
}

/* ---- queue ------------------------------------------------------------- */
function createTaskCard() {
  const chipSlot = el("span");
  const priority = el("span", { class: "tag", "data-tone": "ink" });
  /* `solo` runs one implementer straight into review instead of the usual
     multi-agent competition - a fact about how the task will be spent that a
     card must show, the same way priority is shown, rather than something
     only visible by opening the full instruction. */
  const solo = el("span", { class: "tag", "data-tone": "teal", text: "solo" });
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
    el("div", { class: "instruction md" }));
  const runLink = el("a", { class: "btn btn-quiet" });
  /* Priority is a step, not a typed value: the operator wants "ahead of
     that other one", not to compose a number. +1/-1 both reach the same
     places a competing task's priority already sits. */
  const priorityDown = el("button", { class: "btn btn-quiet btn-step", type: "button", text: "−" });
  const priorityUp = el("button", { class: "btn btn-quiet btn-step", type: "button", text: "+" });
  const priorityBox = el("span", { class: "task-priority-box" }, priorityDown, priorityUp);
  const editBtn = el("button", { class: "btn btn-quiet", type: "button", text: "Edit" });
  const holdBox = el("span", { class: "task-hold-box" });
  const doneBox = el("span", { class: "task-done-box" });
  const deleteBox = el("span", { class: "task-delete-box" });
  const actions = el("div", { class: "card-actions" },
    runLink, priorityBox, editBtn, holdBox, doneBox, deleteBox);

  const card = el("li", { class: "card" },
    el("div", { class: "card-top" }, chipSlot, priority, solo, whenSlot),
    title, meta, note, error, instruction, actions,
  );
  card.refs = {
    card, chipSlot, priority, solo, whenSlot, title, source, repo, attempts,
    outcome, note, error, instruction, runLink, priorityDown, priorityUp,
    editBtn, holdBox, doneBox, deleteBox,
  };
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

  show(r.solo, Boolean(task.solo));

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

  /* A held task's note gains whatever the operator said it is waiting on,
     since the queue cannot express a dependency between two tasks and this
     is the one place that reason survives. */
  const noteText = task.hold_reason && meta.note
    ? `${meta.note} Waiting on: ${task.hold_reason}`
    : meta.note || (task.hold_reason ? `Waiting on: ${task.hold_reason}` : "");
  setText(r.note, noteText);
  show(r.note, Boolean(noteText));

  setText(r.error, task.last_error || "");
  show(r.error, Boolean(task.last_error));

  const full = task.instruction || "";
  const instructionBox = r.instruction.querySelector(".instruction");
  if (instructionBox.dataset.forTask !== task.id) {
    instructionBox.dataset.forTask = task.id;
    renderMd(instructionBox, task.instruction_md);
  }
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

  /* Priority only ever changes something for a task the loop could still
     claim; a running task has already left that pool (see
     `Task::set_priority`'s doc), so the buttons are disabled rather than
     left to round-trip a 4xx. */
  const priorityNow = Number(task.priority) || 0;
  r.priorityDown.disabled = status === "running";
  r.priorityUp.disabled = status === "running";
  setAttr(r.priorityDown, "aria-label", `Lower priority of ${task.title || task.id}`);
  setAttr(r.priorityUp, "aria-label", `Raise priority of ${task.title || task.id}`);
  r.priorityDown.onclick = () => changePriority(task.id, priorityNow - 1);
  r.priorityUp.onclick = () => changePriority(task.id, priorityNow + 1);

  const editable = status === "queued" || status === "held";
  r.editBtn.disabled = !editable;
  setAttr(
    r.editBtn,
    "title",
    editable ? "" : "Only a queued or held task's instruction can be edited.",
  );
  r.editBtn.onclick = () => openTaskEdit(task);
  show(r.editBtn, status !== "done");

  renderTaskHoldBox(row, task);
  renderTaskDoneBox(row, task);

  /* Two-step delete: first tap arms, second tap sends the DELETE request.
     Cancel takes the position of the initial button and receives focus. */
  clear(r.deleteBox);
  const armed = row.dataset.armedDelete === "1";
  if (armed) {
    const cancel = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Cancel",
      onclick: () => {
        row.dataset.armedDelete = "";
        updateTaskCard(row, task);
      },
    });
    const confirm = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Delete now",
      onclick: () => deleteTask(task.id, row),
    });
    r.deleteBox.append(
      el("div", { class: "stakes-confirm" },
        el("p", { class: "stakes-warn", text: "Deletes the task file. Its id, who filed it, and any run history go with it and cannot be recovered." }),
        el("div", { class: "stakes-row" }, cancel, confirm),
      ),
    );
    requestAnimationFrame(() => cancel.focus({ preventScroll: true }));
  } else {
    const del = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Delete…",
      disabled: status === "running",
      onclick: () => {
        row.dataset.armedDelete = "1";
        updateTaskCard(row, task);
      },
    });
    setAttr(del, "aria-label", `Delete task ${task.title || task.id}`);
    r.deleteBox.append(del);
  }
}

/* Hold takes an optional reason, so unlike release it is not a single tap:
   the first tap opens a short text field rather than acting immediately,
   the same two-step shape delete already uses but for input instead of
   confirmation. Release stays one tap - there is nothing to ask it. */
function renderTaskHoldBox(row, task) {
  const r = row.refs;
  const status = String(task.status_str || task.status || "");
  clear(r.holdBox);
  if (status === "done") return;

  if (status === "held") {
    const release = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Release",
      onclick: () => mutateTask(task.id, "release", release),
    });
    setAttr(release, "aria-label", `Release task ${task.title || task.id}`);
    r.holdBox.append(release);
    return;
  }

  if (row.dataset.armedHold === "1") {
    const reasonInput = el("input", {
      type: "text",
      placeholder: "What is this waiting on? (optional)",
    });
    const cancel = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Cancel",
      onclick: () => {
        row.dataset.armedHold = "";
        updateTaskCard(row, task);
      },
    });
    const confirm = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Hold",
      onclick: () => holdTask(task.id, reasonInput.value, row, confirm),
    });
    r.holdBox.append(
      el("div", { class: "stakes-confirm" },
        reasonInput,
        el("div", { class: "stakes-row" }, cancel, confirm),
      ),
    );
    requestAnimationFrame(() => reasonInput.focus({ preventScroll: true }));
  } else {
    const hold = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Hold…",
      disabled: status === "running",
      onclick: () => {
        row.dataset.armedHold = "1";
        updateTaskCard(row, task);
      },
    });
    setAttr(hold, "aria-label", `Hold task ${task.title || task.id}`);
    r.holdBox.append(hold);
  }
}

async function holdTask(id, reason, row, button) {
  const label = button.textContent;
  button.disabled = true;
  setText(button, "…");
  try {
    await postJson(API.hold(id), reason.trim() ? { reason: reason.trim() } : undefined);
    ok();
    announce(`Task ${shortId(id)} held.`);
    row.dataset.armedHold = "";
    await loadQueue();
  } catch (error) {
    setText(button, label);
    button.disabled = false;
    fail(`Could not hold task ${shortId(id)}: ${error.message}`);
  }
}

/* Done and Delete both clear a task off the backlog, and are the two things
   an operator could tap for "I am finished with this" without reading
   closely - so the confirm text carries the difference, in the same tone
   Delete's already does: one keeps the record, one removes it. */
function renderTaskDoneBox(row, task) {
  const r = row.refs;
  const status = String(task.status_str || task.status || "");
  clear(r.doneBox);
  if (status === "done") return;

  if (row.dataset.armedDone === "1") {
    const cancel = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Cancel",
      onclick: () => {
        row.dataset.armedDone = "";
        updateTaskCard(row, task);
      },
    });
    const confirm = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Yes, mark done",
      onclick: () => doneTask(task.id, row, confirm),
    });
    r.doneBox.append(
      el("div", { class: "stakes-confirm" },
        el("p", { class: "hint", text: "Marks the task finished. Its id, who filed it, and its run history are kept — nothing is deleted." }),
        el("div", { class: "stakes-row" }, cancel, confirm),
      ),
    );
    requestAnimationFrame(() => cancel.focus({ preventScroll: true }));
  } else {
    const done = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Mark done…",
      onclick: () => {
        row.dataset.armedDone = "1";
        updateTaskCard(row, task);
      },
    });
    setAttr(done, "aria-label", `Mark task ${task.title || task.id} done`);
    r.doneBox.append(done);
  }
}

async function doneTask(id, row, button) {
  const label = button.textContent;
  button.disabled = true;
  setText(button, "…");
  try {
    await postJson(API.doneTask(id));
    ok();
    announce(`Task ${shortId(id)} marked done.`);
    row.dataset.armedDone = "";
    await loadQueue();
  } catch (error) {
    setText(button, label);
    button.disabled = false;
    fail(`Could not mark task ${shortId(id)} done: ${error.message}`);
  }
}

async function changePriority(id, priority) {
  try {
    await postJson(API.priority(id), { priority });
    ok();
    announce(`Task ${shortId(id)} priority set to ${priority}.`);
    await loadQueue();
  } catch (error) {
    fail(`Could not change priority of task ${shortId(id)}: ${error.message}`);
  }
}

async function deleteTask(id, row) {
  try {
    await deleteReq(API.deleteTask(id));
    ok();
    announce(`Task ${shortId(id)} removed.`);
    await loadQueue();
  } catch (error) {
    if (row) {
      row.dataset.armedDelete = "";
      const task = (state.queue || []).find((t) => t.id === id);
      if (task) updateTaskCard(row, task);
    }
    fail(`Could not delete task ${shortId(id)}: ${error.message}`);
  }
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

/* ---- queue: grouping into sections ------------------------------------- *
 * The flat, priority-then-recency list stops being readable once a few
 * dozen tasks pile up (in practice: mostly `held`), so it is split into what
 * an operator actually wants to see first: what's running, what's next,
 * what's parked, what's finished. `queued` and `failed` share a section
 * because both are `TaskStatus::runnable` in src/queue.rs - the loop could
 * pick up either next - and the card's own chip/tone/error already tell them
 * apart, so a second section would only duplicate that distinction. */
const QUEUE_SECTIONS = [
  { key: "running", label: "Running", defaultOpen: true },
  { key: "upnext", label: "Up next", defaultOpen: true },
  { key: "held", label: "Held", defaultOpen: false },
  { key: "done", label: "Done", defaultOpen: false },
];

function queueSection(task) {
  const status = String(task.status_str || task.status || "");
  if (status === "running") return "running";
  if (status === "queued" || status === "failed") return "upnext";
  if (status === "held") return "held";
  return "done";
}

/* Section order preserved from QUEUE_SECTIONS; task order within a section
   preserved from the order tasks arrived in, which is /api/queue's own
   priority-then-recency order. */
function groupQueueBySection(tasks) {
  const bySection = new Map(QUEUE_SECTIONS.map((s) => [s.key, []]));
  for (const task of tasks) bySection.get(queueSection(task)).push(task);
  return bySection;
}

function createQueueSection(def) {
  return createSection(def, state.queueCollapsed, QUEUE_COLLAPSE_KEY);
}

function updateQueueSection(node, tasks) {
  setText(node.refs.count, plural(tasks.length, "task", "tasks"));
  syncList(node.refs.list, tasks, (t) => t.id, createTaskCard, updateTaskCard);
}

function syncQueueSections(root, bySection) {
  syncSections(root, QUEUE_SECTIONS, bySection, createQueueSection, updateQueueSection);
}

function renderQueue() {
  const sectionsRoot = $("queue-sections");
  const tasks = state.queue;

  if (tasks === null) {
    setText($("queue-count"), "Loading\u2026");
    return;
  }

  const runnable = tasks.filter((t) => {
    const status = t.status_str || t.status;
    return status === "queued" || status === "failed";
  }).length;
  const held = tasks.filter((t) => (t.status_str || t.status) === "held").length;
  const parts = [`${plural(tasks.length, "task", "tasks")}`];
  if (runnable) parts.push(`${runnable} runnable`);
  if (held) parts.push(`${held} held`);
  setText($("queue-count"), tasks.length === 0 ? "Nothing waiting" : parts.join(", "));

  show($("queue-empty"), tasks.length === 0);
  syncQueueSections(sectionsRoot, groupQueueBySection(tasks));
  /* The strip's wording depends on how many tasks are runnable, so it is
     re-rendered from the queue rather than only from health: "off, with two
     tasks waiting" has to appear the moment the second one is filed. */
  renderLoop();
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

/* ---- markdown ----------------------------------------------------------- *
 * Parsing markdown happens once, server-side (`magi::md`), which hands back a
 * tree of typed nodes rather than a string of HTML. What follows is the one
 * and only place this client turns that tree into DOM, with createElement and
 * textContent exactly as everywhere else — there is no second reader of
 * markdown syntax in this file, and no innerHTML anywhere in it. A run's
 * instruction, a chat's agent turn, a question's detail and a chat's draft
 * all go through `renderMd`; nothing here re-derives structure from the raw
 * string the way the old hand-rolled reader did. */

/* One markdown node to one DOM node. The server already refused to build a
   `link`/`image` node for anything it would be unsafe to render (see
   `magi::md::normalize_link`/`normalize_image`), so this never has to
   inspect a URL itself — it only has to trust the shape it was handed. */
function buildMd(node) {
  switch (node && node.type) {
    case "paragraph":
      return el("p", {}, (node.children || []).map(buildMd));
    case "heading": {
      const level = Math.min(6, Math.max(1, Number(node.level) || 1));
      return el(`h${level}`, {}, (node.children || []).map(buildMd));
    }
    case "bullet_list":
      return el("ul", {}, (node.items || []).map(buildMd));
    case "ordered_list":
      return el("ol", { start: node.start && node.start !== 1 ? node.start : null },
        (node.items || []).map(buildMd));
    case "list_item":
      if (node.checked === null || node.checked === undefined) {
        return el("li", {}, (node.children || []).map(buildMd));
      }
      return el("li", { class: "task" },
        el("input", { type: "checkbox", checked: Boolean(node.checked), disabled: true }),
        (node.children || []).map(buildMd));
    case "block_quote":
      return el("blockquote", {}, (node.children || []).map(buildMd));
    case "thematic_break":
      return el("hr");
    case "code_block":
      return el("pre", { "data-lang": node.lang || null }, el("code", { text: node.code || "" }));
    case "code":
      return el("code", { text: node.code || "" });
    case "emphasis":
      return el("em", {}, (node.children || []).map(buildMd));
    case "strong":
      return el("strong", {}, (node.children || []).map(buildMd));
    case "strikethrough":
      return el("s", {}, (node.children || []).map(buildMd));
    case "link":
      return el("a", { href: node.href, target: "_blank", rel: "noopener noreferrer" },
        (node.children || []).map(buildMd));
    case "image":
      return el("img", { src: node.src, alt: node.alt || "", loading: "lazy" });
    case "table":
      return buildMdTable(node);
    case "soft_break":
      return document.createTextNode(" ");
    case "line_break":
      return el("br");
    case "text":
      return document.createTextNode(node.value ?? "");
    default:
      return document.createTextNode("");
  }
}

function buildMdTable(node) {
  const rows = Array.isArray(node.rows) ? node.rows : [];
  const row = (cells) => el("tr", {}, (Array.isArray(cells) ? cells : []).map((cell) =>
    el(cell.header ? "th" : "td",
      { "data-align": cell.align && cell.align !== "none" ? cell.align : null },
      (cell.children || []).map(buildMd))));
  const head = rows.length ? el("thead", {}, row(rows[0])) : null;
  const body = el("tbody", {}, rows.slice(1).map(row));
  return el("div", { class: "table-scroll" }, el("table", {}, head, body));
}

/* Replace `container`'s children with `nodes` (a server-sent markdown tree)
   turned into DOM. The one call every one of the five markdown surfaces in
   this UI goes through. */
function renderMd(container, nodes) {
  clear(container);
  append(container, (Array.isArray(nodes) ? nodes : []).map(buildMd));
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

  /* The round trip: every turn after the question itself, oldest first, and a
     box to add one without deciding anything. */
  const thread = el("ol", { class: "ask-thread" });
  const waitingNote = el("p", { class: "ask-waiting" });
  const sayText = el("textarea", { rows: "3", "aria-label": "Ask the agent back" });
  const saySend = el("button", { class: "btn", type: "button", text: "Ask back" });
  const sayBox = el("div", { class: "ask-say" },
    el("label", { class: "ask-say-label", text: "Not ready to decide? Ask back instead:" }),
    sayText, saySend);

  const row = el("li", { class: "ask" },
    band,
    el("div", { class: "ask-top" }, chipSlot, whenSlot),
    /* The panel sits above the prose: when there is one, it is the case for
       the decision and the detail is the footnote. */
    summary, where, panelBox, detail, thread, hint, waitingNote, stakes, choices, free, sayBox, error, answer, note,
  );
  row.refs = { chipSlot, whenSlot, summary, runLink, node, seat, where, detail,
               hint, choices, text, send, free, error, answerLabel, answerText, answer, note,
               band, panelBox, stakes, thread, waitingNote, sayText, saySend, sayBox };
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

  /* The round trip after the question itself: the owner talking back, the
     agent replying. `waiting_on_agent` names the one state nothing about
     `status` can - the question is still open, but nobody is waiting on the
     owner right now, they are waiting on the agent's `magi ask --thread`. */
  const waitingOnAgent = open && question.waiting_on_agent === true;
  setAttr(row, "data-waiting-agent", waitingOnAgent ? "1" : null);

  const turns = Array.isArray(question.thread) ? question.thread : [];
  const threadKey = String(turns.length);
  if (row.dataset.threadKey !== threadKey) {
    row.dataset.threadKey = threadKey;
    clear(r.thread);
    for (const turn of turns) {
      const isAgent = turn.who === "agent";
      const at = when(turn.at);
      r.thread.append(el("li", { class: "ask-turn", "data-who": isAgent ? "agent" : "operator" },
        el("span", { class: "ask-turn-who", text: isAgent ? "Agent" : "You" }),
        el("time", { class: "ask-turn-when", datetime: turn.at, title: at.title, text: at.text }),
        el("p", { class: "ask-turn-body", text: turn.body || "" }),
      ));
    }
  }
  show(r.thread, turns.length > 0);

  setText(r.waitingNote, waitingOnAgent
    ? "Waiting for the agent to reply. There is nothing to decide until it does."
    : "");
  show(r.waitingNote, waitingOnAgent);

  r.saySend.onclick = () => sayToQuestion(question.id, r.sayText.value, row);
  show(r.sayBox, open);
  r.sayText.disabled = waitingOnAgent;
  r.saySend.disabled = waitingOnAgent;

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
      const body = el("div", { class: "md" });
      renderMd(body, question.detail_md);
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

  setText(r.hint, !open || waitingOnAgent
    ? ""
    : merge
      ? "Read the panel, then decide. Nothing merges until you say so twice."
      : choices.length
        ? "Pick one. The run resumes as soon as you do."
        : "No options were offered \u2014 answer in your own words.");
  show(r.hint, open && !waitingOnAgent);
  show(r.stakes, open && merge);
  show(r.choices, open && !merge && choices.length > 0);
  show(r.free, open && choices.length === 0);
  show(r.error, open && !r.error.hidden && r.error.textContent !== "");

  // While the agent has not replied yet, deciding is not an option: the
  // controls stay visible - the owner can still see what was on offer - but
  // disabled, with `waitingNote` above saying why.
  r.text.disabled = waitingOnAgent;
  r.send.disabled = waitingOnAgent;
  for (const btn of r.choices.querySelectorAll("button")) btn.disabled = waitingOnAgent;
  for (const btn of r.stakes.querySelectorAll("button")) btn.disabled = waitingOnAgent;

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

/* Talk back without deciding anything: `POST /api/questions/{id}/say`, the
   phone's half of the round trip `magi ask --thread` completes from the
   agent's side. Same 409 handling as `answerQuestion` - answered or abandoned
   from elsewhere between the list and the tap reads as settled, not as an
   error the operator has to parse. */
async function sayToQuestion(id, text, row) {
  const r = row.refs;
  const value = String(text || "").trim();
  if (!value) {
    r.sayText.focus();
    return;
  }

  r.sayText.disabled = true;
  r.saySend.disabled = true;
  try {
    reflectQuestion(await postJson(API.questionSay(id), { body: value }));
    r.sayText.value = "";
    announce("Sent. Waiting for the agent to reply.");
    ok();
  } catch (error) {
    if (error.status === 409) {
      row.dataset.raced = "1";
      announce("That question was already settled.");
      await loadQuestions();
      return;
    }
    setText(r.error, error.message);
    show(r.error, true);
    r.sayText.disabled = false;
    r.saySend.disabled = false;
  }
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
          : state.route.name === "talks"
            ? "Chat \u2014 magi"
            : state.route.name === "talk"
              ? `Chat ${shortId(state.route.id)} \u2014 magi`
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

/* Parallel to `chatTurns`: one parsed markdown tree per real turn, server-
   built so this client never re-derives it. The synthetic "still sending"
   turn `renderChat` appends locally has no entry and needs none - it is
   always the operator's own text, shown as plain text. */
const chatTurnsMd = (chat) => (chat && Array.isArray(chat.turn_bodies_md) ? chat.turn_bodies_md : []);

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

/* Order by when a conversation STARTED, not when it was last touched.

   Sorting on `updated_at` meant every agent reply lifted its conversation to
   the top, so a list the operator was reading rearranged itself under their
   finger — on a phone that reads as the screen switching on its own. Every
   other list in this client is ordered by creation (runs and tasks both sort
   on their ids, which begin with a timestamp) and none of them move; this was
   the only one that did.

   Recency still shows: the card carries its own timestamp and its unread
   marker. Position is identity, and identity should not move because a model
   answered. Open-before-filed stays, because a conversation only changes
   status when the operator files it — a card moving then is a consequence of
   something they just did. */
function sortChats(list) {
  return list.slice().sort((a, b) => {
    const rank = (a.status === "open" ? 0 : 1) - (b.status === "open" ? 0 : 1);
    const started = (chat) => Date.parse(chat.created_at) || 0;
    return rank || started(b) - started(a);
  });
}

/* ---- the list ---------------------------------------------------------- */
function createChatCard() {
  const chipSlot = el("span");
  const ready = el("span", { class: "tag chat-ready", "data-tone": "gold", text: "draft ready" });
  /* A busy conversation's own badge, distinct from `ready`: the two can never
     show at once (a chat mid-turn has not just produced a fresh draft this
     browser has not seen), but they answer different questions, so a shared
     slot would have to pick one to lose. */
  const thinking = el("span", { class: "tag chat-thinking", "data-tone": "blue", text: "thinking…" });
  const whenSlot = el("time", { class: "card-when" });
  const title = el("h2", { class: "card-title" });
  const agent = el("span", { class: "repo" });
  const turns = el("span");
  const task = el("span", { class: "win" });
  const meta = el("div", { class: "card-meta" }, agent, turns, task);
  const last = el("p", { class: "card-event" });

  const card = el("a", { class: "card" },
    el("div", { class: "card-top" }, chipSlot, thinking, ready, whenSlot),
    title, meta, last,
  );
  const row = el("li", {}, card);
  row.refs = { card, chipSlot, thinking, ready, whenSlot, title, agent, turns, task, last };
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

  /* `state.chatWaits`, not `chat.thinking` directly: `trackIfThinking` (run
     over every chat in `loadChats`) is what reconciles the two, and reading
     the map here is what makes the badge agree with the wait strip on the
     conversation's own page - both a reload and a turn started from another
     device land in the same place. */
  show(r.thinking, state.chatWaits.has(chat.id));

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

/* ---- design deliberations -----------------------------------------------
 * `magi plan` runs from a terminal and gathers its three advisors headless,
 * on whatever machine the operator typed the command on - so this is the
 * only place a phone ever gets to read what they argued. There is no
 * revision to poll for these: the files are written by that CLI process, not
 * by this server, so the list is a snapshot fetched on arrival and again on
 * an explicit tap, the same contract `repos` above already has. */
function createDraftCard() {
  const title = el("h2", { class: "card-title" });
  const meta = el("p", { class: "card-meta" });
  const card = el("div", { class: "card", tabindex: "0", role: "button" }, title, meta);
  const row = el("li", {}, card);
  row.refs = { card, title, meta };
  return row;
}

function updateDraftCard(row, draft) {
  const r = row.refs;
  setText(r.title, draft.title || draft.id);
  setText(
    r.meta,
    `${plural(draft.proposals, "proposal", "proposals")} of ${plural(draft.seats, "advisor", "advisors")}`,
  );
  r.card.onclick = () => viewDraftAdvisors(draft.id);
  r.card.onkeydown = (event) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      viewDraftAdvisors(draft.id);
    }
  };
}

function renderDrafts() {
  const list = $("plan-drafts-list");
  const drafts = state.drafts;
  if (drafts === null) return;
  show($("plan-drafts-empty"), drafts.length === 0);
  syncList(list, drafts, (d) => d.id, createDraftCard, updateDraftCard);
}

async function loadDrafts() {
  try {
    state.drafts = await getJson(API.drafts);
    renderDrafts();
  } catch {
    /* Nothing worth interrupting Planning over - the list just stays at
       whatever it last showed, same as a failed repository scan. */
    state.drafts = state.drafts || [];
    renderDrafts();
  }
}

/* A proposal's own fields, one row each - never innerHTML, since every word
   here came from an agent the operator has not read yet. */
function renderProposal(list, proposal) {
  const add = (label, value) => {
    if (!value) return;
    list.append(el("li", {}, el("strong", { text: `${label}: ` }), el("span", { text: value })));
  };
  add("Approach", proposal.approach);
  add("Key tradeoff", proposal.key_tradeoff);
  add("Risks", (proposal.risks || []).join("; "));
  add("Touches", (proposal.touches || []).join(", "));
  add("Why not the naive approach", proposal.why_not_naive);
}

function renderDraftDetail() {
  const panel = $("plan-draft-detail-panel");
  const { id, advice, error } = state.draftDetail;
  show(panel, Boolean(id));
  if (!id) return;

  setText($("plan-draft-detail-title"), `Design deliberation — ${shortId(id)}`);
  const list = $("plan-draft-detail-list");
  const bodyEl = $("plan-draft-detail-body");
  clear(list);
  clear(bodyEl);
  show(bodyEl, false);
  show($("plan-draft-detail-no-body"), false);

  if (error) {
    list.append(el("li", { class: "form-error", text: error }));
    return;
  }
  if (!advice) {
    list.append(el("li", { text: "Loading…" }));
    return;
  }

  /* The task file the deliberation produced - what the planner actually
     kept from the proposals below, not just what was on offer. Rendered the
     same way every other markdown surface in this client is: a server-parsed
     tree, never a client-side parse of agent-authored text. */
  if (advice.draft_md) {
    renderMd(bodyEl, advice.draft_md);
    show(bodyEl, true);
  } else {
    show($("plan-draft-detail-no-body"), true);
  }

  for (const record of advice.records || []) {
    const head = el("h3", { text: `${record.seat} (${record.agent || "—"})` });
    const body = el("ul", { class: "proposal" });
    if (record.proposal) renderProposal(body, record.proposal);
    else body.append(el("li", { class: "form-error", text: record.error || "no proposal" }));
    list.append(el("li", {}, head, body));
  }
  if ((advice.records || []).length === 0) {
    list.append(el("li", { text: "No advisor records." }));
  }
}

async function viewDraftAdvisors(id) {
  state.draftDetail = { id, advice: null, error: null };
  renderDraftDetail();
  try {
    state.draftDetail = { id, advice: await getJson(API.draftAdvisors(id)), error: null };
  } catch (error) {
    state.draftDetail = { id, advice: null, error: error.message };
  }
  renderDraftDetail();
}

function closeDraftDetail() {
  state.draftDetail = { id: null, advice: null, error: null };
  renderDraftDetail();
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
     agent's is markdown prose, already parsed server-side, and is rendered as
     nodes, never as markup: the rule that no API data is ever assigned as
     HTML holds everywhere outside the sandboxed panel frame, and a model that
     writes a script tag into a fence has to see the characters of one. */
  const key = `${kind}:${body.length}`;
  if (row.dataset.turnKey !== key) {
    row.dataset.turnKey = key;
    clear(r.body);
    if (kind === "agent") {
      const div = el("div", { class: "md" });
      renderMd(div, item.md);
      r.body.append(div);
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
    el("ul", {}, problems.map((problem) => el("li", { text: String(problem) }))),
  );
  show(box, true);
}

function chatError(message) {
  const box = $("chat-error");
  setText(box, message || "");
  show(box, Boolean(message));
}

/* Switches the draft panel between its formatted read and the raw bytes that
   would actually be filed. Both stay in the DOM; only `hidden` moves, so
   toggling never re-parses or re-fetches anything. */
function applyDraftView() {
  const raw = state.draftRaw;
  show($("chat-draft-rendered"), !raw);
  show($("chat-draft"), raw);
  setText($("chat-draft-raw-toggle"), raw ? "Show formatted" : "Show raw");
  setAttr($("chat-draft-raw-toggle"), "aria-pressed", String(raw));
}

/* Scroll the page so the last turn's top sits just below the sticky header.
   The header (.top) uses position:sticky;top:0, so scrollIntoView would hide
   the first line behind it. We account for its height with scroll-margin-top
   set via JS on the target element, using getBoundingClientRect for a
   reliable measurement regardless of safe-area insets or zoom level. */
function scrollToLastTurn() {
  const turns = $("chat-turns");
  if (!turns.children.length) return;
  const last = turns.lastElementChild;
  const header = document.querySelector(".top");
  const gap = header ? Math.ceil(header.getBoundingClientRect().height) + 4 : 0;
  last.style.scrollMarginTop = `${gap}px`;
  const motion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  last.scrollIntoView({ behavior: motion ? "auto" : "smooth", block: "start" });
}

function renderChat() {
  const chat = state.chatDetail.chat;
  const wait = chat ? state.chatWaits.get(chat.id) : undefined;
  const busy = Boolean(wait);

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
    show($("chat-derived-from"), false);
    return;
  }

  const status = String(chat.status || "open");
  /* The operator's own message, while the turn that carries it is still in
     flight. It is composed in here rather than pushed into the loaded chat,
     because the ten-second re-read below replaces that chat wholesale and
     would otherwise make the message the operator just sent vanish for the
     rest of the wait. */
  const pending = wait && wait.pending && chatTurns(chat).length <= wait.since
    ? [{ who: "operator", body: wait.pending.body, at: wait.pending.at }]
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

  show($("chat-derived-from"), Boolean(chat.from));
  if (chat.from) {
    const link = $("chat-derived-from-link");
    setText(link, shortId(chat.from));
    setAttr(link, "href", `#/plan/${chat.from}`);
  }

  /* Turns are append-only, so the index is a stable key and reconciling can
     never rebuild the transcript the operator is reading. */
  const turnsMd = chatTurnsMd(chat);
  syncList($("chat-turns"), turns.map((turn, i) => ({ turn, md: turnsMd[i], key: String(i) })),
    (item) => item.key, createTurnRow, updateTurnRow);

  /* Auto-scroll: on first open and when a new turn arrives. Not when the
     turn count is unchanged (status refresh, 10-second re-read, draft
     update) and not for the pending optimistic turn the operator just sent. */
  const turnCount = turns.length;
  const lastIsPending = wait && wait.pending
    && turns.length > 0 && turns[turns.length - 1].who === "operator"
    && turns[turns.length - 1].body === wait.pending.body;
  if (state.openingChat) {
    state.openingChat = false;
    if (turnCount > 0) requestAnimationFrame(scrollToLastTurn);
  } else if (turnCount > state.prevTurnCount && !lastIsPending) {
    requestAnimationFrame(scrollToLastTurn);
  }
  state.prevTurnCount = turnCount;

  const draft = chatDraft(chat);
  const hasDraft = draft.trim() !== "";
  show($("chat-draft-panel"), hasDraft);
  if (hasDraft) {
    setText($("chat-draft"), draft);
    if ($("chat-draft-rendered").dataset.forDraft !== draft) {
      $("chat-draft-rendered").dataset.forDraft = draft;
      renderMd($("chat-draft-rendered"), chat.draft_md);
    }
  }
  applyDraftView();
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

/* The wait, in words, for whichever chat is on screen - and, regardless of
   what is on screen, the ten-second insurance re-read for every entry in
   state.chatWaits. Several conversations can be waiting at once now: the
   sentence and the countdown are only ever drawn for state.chatDetail.id,
   since there is one wait strip in the document, but every busy chat still
   gets polled so its own turn is not left relying solely on the change
   stream to be discovered as finished. The sentence changes only twice — once
   when the turn starts and once when it has been going long enough to need
   saying — while the seconds tick in a span that assistive technology never
   reads, because a counter announced every second is unusable. */
function tickWaits() {
  const now = Date.now();
  for (const [id, wait] of state.chatWaits) {
    if (now - wait.lastPoll >= 10000) {
      wait.lastPoll = now;
      loadChat(id);
    }
  }

  const box = $("chat-wait");
  const wait = state.chatDetail.id ? state.chatWaits.get(state.chatDetail.id) : undefined;
  if (!wait) {
    show(box, false);
    return;
  }
  const secs = Math.max(Math.round((now - wait.waitFrom) / 1000), 0);
  setText(box.querySelector(".waiting-text"), secs >= 90
    ? "The agent is still thinking. Long, but not stuck — it is allowed to take its time, and the reply will appear here."
    : "The agent is thinking about your message. A turn usually takes under a minute.");
  setText(box.querySelector(".waiting-secs"), `${secs}s`);
  show(box, true);
}

/* Start (or restart) waiting for chat id's turn.
 *
 * since is the transcript length the turn started from and target the length
 * its landing will reach - always one more turn than whatever is already
 * known, since a turn only ever appends one reply or failure note. pending,
 * when given, is the operator's own text to show as an optimistic bubble
 * until the transcript reaches since on its own - see renderChat. It is null
 * for a turn this browser did not just send a message into (starting an
 * interview, or a wait rebuilt from the server's own `thinking` - see
 * trackIfThinking), because in both of those cases the transcript already
 * carries everything there is to show. */
function beginChatTurn(id, since, target, pending = null) {
  state.chatWaits.set(id, { since, target, pending, waitFrom: Date.now(), lastPoll: Date.now() });
  if (!state.waitTimer) state.waitTimer = setInterval(tickWaits, 1000);
  tickWaits();
}

function endChatTurn(id) {
  if (!state.chatWaits.has(id)) return;
  state.chatWaits.delete(id);
  if (state.chatDetail.id === id) show($("chat-wait"), false);
  if (state.chatWaits.size === 0 && state.waitTimer) {
    clearInterval(state.waitTimer);
    state.waitTimer = null;
  }
}

/* Reconcile this browser's belief about a chat with a freshly fetched
 * ChatView - from loadChat, from loadChats, or from a POST /api/chats
 * response, all of which carry the same shape.
 *
 * Ending a wait is decided from the transcript, never from `thinking`: a
 * turn's guard (Ui::begin_turn/TurnGuard) is dropped before the CLI's answer
 * is necessarily visible everywhere this reads from, and `thinking` going
 * false is not the same event as the reply landing - see ChatView's doc for
 * that field. Starting a wait *is* taken from `thinking`, because that is the
 * only way this browser learns of a turn it did not itself send - another
 * tab, another device, or a reload that lost the chatWaits entry the first
 * beginChatTurn call made.
 *
 * A fresh reconstruction only fires when the *last recorded turn* is the
 * operator's. A turn is always exactly one operator message followed by one
 * agent reply or failure note (see chat::turn), so an agent turn already on
 * the end of the transcript means that turn already landed - `thinking` can
 * still read `true` for an instant after (chat::turn writes the reply before
 * its TurnGuard drops), and chat_say claims the guard before its own
 * chat::record write lands (so the transcript here can still end on the
 * *previous* agent turn while a new one is already in flight). Either way,
 * there is nothing this browser can safely assume the count of remaining
 * turns to be, so it waits for the next poll - typically the SSE `chats_rev`
 * bump chat::record's own write causes - rather than guess and risk building
 * a `target` that transcript growth can never reach (stuck "thinking") or one
 * a single turn satisfies too early (a busy chat reported free). */
function trackIfThinking(chat) {
  const wait = state.chatWaits.get(chat.id);
  if (wait) {
    if (chatTurns(chat).length >= wait.target) endChatTurn(chat.id);
    return;
  }
  if (!chat.thinking) return;
  const turns = chatTurns(chat);
  const last = turns[turns.length - 1];
  if (last && last.who === "agent") return;
  beginChatTurn(chat.id, turns.length, turns.length + 1);
}

/* ---- repository picker -------------------------------------------------- *
 * One list, `state.repos`, feeds both selects below: the one that starts a
 * new conversation and the one that derives a conversation into a different
 * repository. Neither select is the source of truth for what gets sent -
 * the text input beside each is, so a repository outside `[repos] roots`
 * (nothing scanned, or the operator's own path) is always reachable. */
function renderRepoOptions(select) {
  const current = select.value;
  clear(select);
  select.append(el("option", { value: "", text: select.dataset.placeholder || "" }));
  for (const repo of state.repos || []) {
    select.append(el("option", { value: repo.path, text: repo.name }));
  }
  if ([...select.options].some((option) => option.value === current)) select.value = current;
}

async function loadRepos(refresh) {
  try {
    state.repos = await getJson(refresh ? API.reposRefresh : API.repos);
  } catch {
    /* The pickers just stay empty; typing a path still works. */
    state.repos = state.repos || [];
  }
  renderRepoOptions($("chat-start-repo-select"));
  renderRepoOptions($("chat-derive-repo-select"));
}

async function loadChats() {
  try {
    const list = await getJson(API.chats);
    state.chats = Array.isArray(list) ? list : [];
    /* Reconciles every conversation's wait state, not just the one on screen:
       `thinking` is how this browser learns of a turn it did not send itself
       (another tab, another device, or a chat started before the last
       reload), and a turn landing while its chat is off screen still has to
       clear the busy marker on its card. */
    for (const chat of state.chats) trackIfThinking(chat);
    renderChats();
    ok();
  } catch (error) {
    fail(`Could not load conversations: ${error.message}`);
  }
}

/* Refresh one conversation. This **must not** decide which conversation is on
   screen: that is the router's job, in `applyRoute`.
 *
 * It used to open with `state.chatDetail = { id, chat: null }`, which turned
 * every refresh into a navigation. With a turn in flight, `tickWaits`'
 * ten-second insurance calls this for every *waiting* chat no matter what the
 * operator is reading, so every ten seconds the transcript on screen was
 * replaced by a different conversation while the address bar went on naming
 * the one the operator had chosen. Reported as "the screen switches by itself
 * when a plan reply arrives", and reproduced exactly that way.
 *
 * The turn is still settled from here, before the on-screen check, because
 * that is the whole point of the insurance: the reply may well land while the
 * operator is somewhere else, and the wait strip has to stop either way. */
async function loadChat(id) {
  try {
    const chat = await getJson(API.chat(id));
    /* The transcript having grown past `target` is what proves the turn
       finished, whoever started it and whether or not this page's own
       request has come back yet - see `trackIfThinking`. */
    trackIfThinking(chat);
    if (state.chatDetail.id !== id) return;   /* not on screen: nothing to draw */
    state.chatDetail.chat = chat;
    renderChat();
    ok();
  } catch (error) {
    /* Only complain about the conversation the operator is actually reading:
       the ten-second insurance refreshes one that may be off screen, and an
       alert about that is noise over whatever they chose to look at. */
    if (state.chatDetail.id === id) {
      fail(`Could not load conversation ${shortId(id)}: ${error.message}`);
    }
  }
}

/* Starting an interview no longer waits for the agent's first turn - see
   `chat_post`'s doc: the response carries the operator's idea and
   `thinking: true`, and the reply arrives the way every other turn does,
   through the change stream or `tickWaits`' insurance. The button is
   disabled only for the round trip that records the idea, which is fast. */
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

  const repo = $("chat-start-repo").value.trim();

  try {
    const chat = await postJson(API.chats, { idea, agent: null, repo: repo || null });
    state.chats = sortChats([chat, ...(state.chats || []).filter((c) => c.id !== chat.id)]);
    state.chatDetail = { id: chat.id, chat };
    trackIfThinking(chat);
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
  }
}

/* "Continue in another repository": derives a new conversation from the one
   on screen, carrying its whole transcript and repository into the new one's
   opening briefing as background (see `chat::derived_background` server-side).
   The conversation on screen is left exactly as it is - this only ever
   creates a new one. */
async function deriveChat() {
  const chat = state.chatDetail.chat;
  const error = $("chat-derive-error");
  const go = $("chat-derive-go");
  if (!chat) return;

  const repo = $("chat-derive-repo").value.trim();
  if (!repo) {
    setText(error, "Pick a repository, or type a path, first.");
    show(error, true);
    return;
  }

  show(error, false);
  go.disabled = true;
  setText(go, "Starting\u2026");

  try {
    const derived = await postJson(API.chats, {
      idea: "Continue this conversation in a different repository.",
      repo,
      from: chat.id,
    });
    state.chats = sortChats([derived, ...(state.chats || []).filter((c) => c.id !== derived.id)]);
    trackIfThinking(derived);
    renderChats();
    announce("Started a new conversation in the other repository.");
    location.hash = `#/plan/${derived.id}`;
  } catch (failure) {
    setText(error, failure.message);
    show(error, true);
  } finally {
    go.disabled = false;
    setText(go, "Continue here");
  }
}

async function sendTurn(event) {
  event.preventDefault();
  const id = state.chatDetail.id;
  const box = $("f-say");
  const text = box.value;

  /* One turn at a time *on this chat*, checked here as well as by the
     disabled button: a double tap can beat a re-render, and a keyboard
     shortcut does not care that the button looks dead. A turn running on a
     different conversation is not a reason to refuse this one - the server
     only refuses two turns on the same chat, see `Ui::begin_turn`. */
  if (!id || state.chatWaits.has(id)) return;
  if (!text.trim()) {
    chatError("Say something first.");
    box.focus();
    return;
  }

  chatError("");
  const before = chatTurns(state.chatDetail.chat).length;
  /* The operator's own words go up immediately, held as the pending turn
     until the transcript on disk has grown past it. */
  beginChatTurn(id, before, before + 2, { body: text, at: new Date().toISOString() });
  box.value = "";
  renderChat();
  $("chat-wait").scrollIntoView({ block: "nearest" });

  try {
    /* 202: the server has recorded the message and is running the turn. It
       does NOT wait for the agent, because holding a connection open for the
       23-to-90 seconds a real turn takes is a coin flip on a phone — a screen
       lock or a network handoff dropped it, the browser said "Failed to fetch",
       and the server finished the turn anyway. So the wait stays up and the
       reply arrives the way everything else in this client arrives: the change
       stream, or the ten-second re-read in `tickWaits`. `loadChat` ends the
       turn once the transcript has grown past this wait's `target`. */
    const queued = await postJson(API.say(id), { text });
    if (state.chatDetail.id === id) {
      state.chatDetail.chat = queued;
      renderChat();
      announce("Sent. The agent is answering.");
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
    endChatTurn(id);
    /* The request itself failed, which now means it failed before the server
       recorded anything — the response no longer waits for the agent. Reload
       anyway and say so carefully: the transcript on disk is the truth, not
       the optimistic bubble above. */
    chatError(`The message may not have been sent: ${error.message}`);
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

/* ---- standing chat ------------------------------------------------------ *
 * A conversation that stays open, for questions, investigation and thinking
 * out loud between tasks - as opposed to Planning above, which is a
 * one-shot interview that ends the moment a task file is written. Opening
 * one takes no agent turn, because there is nothing yet to answer (see
 * `talk::begin`'s doc); a turn here can run for `talk::TURN_TIMEOUT`, three
 * times a planning turn's budget, since the agent is expected to run
 * commands and read their output rather than answer from what it already
 * knows. The composer and the wait strip below are otherwise the same
 * pattern as Planning's `sendTurn`/`beginChatTurn`/`endChatTurn`, kept as
 * separate functions and separate state (`talkBusy`, a single slot, not
 * `chatWaits`, a map) because the two surfaces talk to two different stores
 * and must not contend for one turn guard - and because there is only ever
 * one standing chat, where Planning holds many conversations at once.
 */
const talkTurns = (talk) => (talk && Array.isArray(talk.turns) ? talk.turns : []);
const talkTurnsMd = (talk) => (talk && Array.isArray(talk.turn_bodies_md) ? talk.turn_bodies_md : []);

/* What names a conversation before it has a title of its own: the first
   thing the operator said, if anything has been said yet. */
function talkOpener(talk) {
  const first = talkTurns(talk).find((turn) => turn.who === "operator");
  return first ? String(first.body || "") : "";
}

/* Open first, then newest first - the same ordering `Talks::list` uses on
   the server, and the same reasoning `sortChats` gives for Planning: what
   the operator is still using belongs above what they are done with. Unlike
   Planning a talk never reorders itself out from under a filed task, since
   filing one does not change its status - but the rule is kept anyway, so a
   closed conversation does not jump to the top of the list the moment it is
   closed. */
function sortTalks(list) {
  return list.slice().sort((a, b) => {
    const rank = (a.status === "open" ? 0 : 1) - (b.status === "open" ? 0 : 1);
    const started = (talk) => Date.parse(talk.created_at) || 0;
    return rank || started(b) - started(a);
  });
}

function createTalkCard() {
  const chipSlot = el("span");
  const whenSlot = el("time", { class: "card-when" });
  const title = el("h2", { class: "card-title" });
  const agent = el("span", { class: "repo" });
  const turns = el("span");
  const tasks = el("span", { class: "win" });
  const meta = el("div", { class: "card-meta" }, agent, turns, tasks);
  const last = el("p", { class: "card-event" });

  const card = el("a", { class: "card" },
    el("div", { class: "card-top" }, chipSlot, whenSlot),
    title, meta, last,
  );
  const row = el("li", {}, card);
  row.refs = { card, chipSlot, whenSlot, title, agent, turns, tasks, last };
  return row;
}

function updateTalkCard(row, talk) {
  const r = row.refs;
  const status = String(talk.status || "open");
  const turns = talkTurns(talk);
  const tone = toneOf(status, TALK_STATUS);

  r.card.setAttribute("href", `#/chat/${talk.id}`);
  setAttr(r.card, "data-tone", tone);
  setAttr(row, "data-tone", tone);

  const next = chip(status, TALK_STATUS);
  if (r.chipSlot.firstChild) r.chipSlot.firstChild.replaceWith(next);
  else r.chipSlot.append(next);

  const at = when(talk.updated_at || talk.created_at);
  setText(r.whenSlot, at.text);
  setAttr(r.whenSlot, "datetime", talk.updated_at || talk.created_at);
  setAttr(r.whenSlot, "title", `updated ${at.title}`);

  setText(r.title, firstLine(talkOpener(talk)) || `conversation ${shortId(talk.id)}`);
  setText(r.agent, talk.agent || "");
  show(r.agent, Boolean(talk.agent));
  setText(r.turns, plural(turns.length, "turn", "turns"));
  const tasks = Array.isArray(talk.tasks) ? talk.tasks.length : 0;
  setText(r.tasks, tasks ? plural(tasks, "task filed", "tasks filed") : "");
  show(r.tasks, tasks > 0);
  separate(r.turns.parentNode);

  const tail = turns.length ? turns[turns.length - 1] : null;
  setText(r.last, tail && tail.who === "agent" ? firstLine(tail.body) : "");
  show(r.last, Boolean(tail && tail.who === "agent"));
}

function renderTalks() {
  const list = $("talks-list");
  const talks = state.talks;

  if (talks === null) {
    setText($("talks-count"), "Loading…");
    return;
  }

  const open = talks.filter((t) => t.status === "open").length;
  setText($("talks-count"), talks.length === 0
    ? "No conversations yet"
    : open ? `${plural(open, "conversation open", "conversations open")}` : "nothing open");

  show($("talks-empty"), talks.length === 0);
  syncList(list, sortTalks(talks), (t) => t.id, createTalkCard, updateTalkCard);
}

async function loadTalks() {
  try {
    const list = await getJson(API.talks);
    state.talks = Array.isArray(list) ? list : [];
    renderTalks();
    ok();
  } catch (error) {
    fail(`Could not load conversations: ${error.message}`);
  }
}

/* Refresh one conversation, on the same rule `loadChat` documents: this must
   not decide which conversation is on screen, and the wait strip is settled
   from here whether or not the reply landed while the operator was looking
   at something else. */
async function loadTalk(id) {
  try {
    const talk = await getJson(API.talk(id));
    if (state.talkBusy === id && talkTurns(talk).length >= state.talkBusyTurns + 2) endTalkTurn(id);
    if (state.talkDetail.id !== id) return;
    state.talkDetail.talk = talk;
    renderTalk();
    ok();
  } catch (error) {
    if (state.talkDetail.id === id) {
      fail(`Could not load conversation ${shortId(id)}: ${error.message}`);
    }
  }
}

function renderTalkTasks(talk) {
  const panel = $("talk-tasks-panel");
  const tasks = Array.isArray(talk && talk.tasks) ? talk.tasks : [];
  show(panel, tasks.length > 0);
  if (tasks.length === 0) return;
  setText($("talk-tasks-count"), String(tasks.length));
  syncList($("talk-tasks"), tasks, (t) => t.id, createTalkTaskRow, updateTalkTaskRow);
}

function createTalkTaskRow() {
  const chipSlot = el("span");
  const title = el("span");
  const row = el("li", {}, chipSlot, title);
  row.refs = { chipSlot, title };
  return row;
}

function updateTalkTaskRow(row, task) {
  const r = row.refs;
  const status = String(task.status_str || task.status || "");
  const next = chip(status, TASK_STATUS);
  if (r.chipSlot.firstChild) r.chipSlot.firstChild.replaceWith(next);
  else r.chipSlot.append(next);
  setText(r.title, `${task.title || task.id} · ${shortId(task.id)}`);
}

function renderTalk() {
  const talk = state.talkDetail.talk;
  const busy = state.talkBusy !== null && state.talkBusy === state.talkDetail.id;

  if (!talk) {
    setText($("talk-h"), "Loading conversation…");
    setText($("talk-meta"), "");
    clear($("talk-status"));
    clear($("talk-turns"));
    show($("talk-tasks-panel"), false);
    show($("talk-say"), false);
    show($("talk-closed"), false);
    show($("talk-close-go"), false);
    show($("talk-reopen-go"), false);
    show($("talk-wait"), false);
    clear($("talk-delete-box"));
    return;
  }

  const status = String(talk.status || "open");
  /* The operator's own message, shown immediately and held until the
     transcript on disk has grown past it - the same accommodation
     `renderChat` makes, for the same reason: the ten-second re-read below
     replaces the whole conversation and would otherwise make the message
     the operator just sent vanish for the rest of the wait. */
  const pending = busy && state.talkPending && state.talkPending.id === talk.id
    && talkTurns(talk).length <= state.talkBusyTurns
    ? [{ who: "operator", body: state.talkPending.body, at: state.talkPending.at }]
    : [];
  const turns = [...talkTurns(talk), ...pending];

  const head = $("talk-status");
  clear(head);
  head.append(chip(status, TALK_STATUS));

  setText($("talk-h"), firstLine(talkOpener(talk)) || `Conversation ${shortId(talk.id)}`);
  const started = when(talk.created_at);
  setText($("talk-meta"),
    `${shortId(talk.id)} · ${talk.agent || "agent"} · ${plural(turns.length, "turn", "turns")} · started ${started.text}`);
  setAttr($("talk-meta"), "title", `${talk.id}\nstarted ${started.title}`);

  const turnsMd = talkTurnsMd(talk);
  syncList($("talk-turns"), turns.map((turn, i) => ({ turn, md: turnsMd[i], key: String(i) })),
    (item) => item.key, createTurnRow, updateTurnRow);

  renderTalkTasks(talk);

  const canSay = status === "open";
  show($("talk-say"), canSay);
  show($("talk-closed"), !canSay);
  show($("talk-close-go"), canSay);
  show($("talk-reopen-go"), !canSay);
  $("f-talk-say").disabled = busy;
  $("talk-send").disabled = busy;
  setText($("talk-send"), busy ? "Thinking…" : "Send");
  show($("talk-wait"), busy);
  renderTalkDelete(talk);
}

function tickTalkWait() {
  const box = $("talk-wait");
  if (state.talkBusy === null) {
    show(box, false);
    return;
  }
  const secs = Math.max(Math.round((Date.now() - state.talkWaitFrom) / 1000), 0);
  setText(box.querySelector(".waiting-text"), secs >= 90
    ? "Still working — a standing chat turn can run for several minutes while the agent investigates. Long, but not stuck."
    : "The agent is looking into it.");
  setText(box.querySelector(".waiting-secs"), `${secs}s`);
  show(box, state.talkBusy === state.talkDetail.id);

  /* Cheap insurance for the case nothing else will tell this page the reply
     landed: the turn was started somewhere else, or the stream is down. */
  if (secs > 0 && secs % 10 === 0) loadTalk(state.talkBusy);
}

function beginTalkTurn(id, before) {
  state.talkBusy = id;
  state.talkBusyTurns = before;
  state.talkWaitFrom = Date.now();
  tickTalkWait();
  if (!state.talkWaitTimer) state.talkWaitTimer = setInterval(tickTalkWait, 1000);
}

function endTalkTurn(id) {
  if (state.talkBusy !== id) return;
  state.talkBusy = null;
  state.talkPending = null;
  if (state.talkWaitTimer) {
    clearInterval(state.talkWaitTimer);
    state.talkWaitTimer = null;
  }
  show($("talk-wait"), false);
}

function talkError(message) {
  const box = $("talk-error");
  setText(box, message || "");
  show(box, Boolean(message));
}

/* Opening a talk takes no agent turn - see `talk::begin`'s doc - so this is
   as fast as any other write and needs none of `startChat`'s waiting state. */
async function startTalk() {
  const go = $("talk-start-go");
  go.disabled = true;
  setText(go, "Opening…");
  try {
    const talk = await postJson(API.talks, {});
    state.talks = sortTalks([talk, ...(state.talks || []).filter((t) => t.id !== talk.id)]);
    state.talkDetail = { id: talk.id, talk };
    renderTalks();
    announce("Conversation opened.");
    location.hash = `#/chat/${talk.id}`;
    ok();
  } catch (failure) {
    fail(`Could not open a conversation: ${failure.message}`);
  } finally {
    go.disabled = false;
    setText(go, "Start a conversation");
  }
}

async function sendTalkTurn(event) {
  event.preventDefault();
  const id = state.talkDetail.id;
  const box = $("f-talk-say");
  const text = box.value;

  if (!id || state.talkBusy !== null) return;
  if (!text.trim()) {
    talkError("Say something first.");
    box.focus();
    return;
  }

  talkError("");
  const before = talkTurns(state.talkDetail.talk).length;
  beginTalkTurn(id, before);

  state.talkPending = { id, body: text, at: new Date().toISOString() };
  box.value = "";
  renderTalk();
  $("talk-wait").scrollIntoView({ block: "nearest" });

  try {
    /* 202, for exactly the reason `sendTurn` documents: a turn here can run
       for the whole of `talk::TURN_TIMEOUT`, and holding a connection open
       that long is not a thing to ask a phone to do. The reply arrives
       through the change stream's `talks_rev`, or the ten-second insurance
       in `tickTalkWait`. */
    const queued = await postJson(API.talkSay(id), { text });
    if (state.talkDetail.id === id) {
      state.talkDetail.talk = queued;
      renderTalk();
      announce("Sent. The agent is answering.");
    }
    loadTalks();
    ok();
  } catch (error) {
    if (error.status === 409) {
      announce("A turn is already running on this conversation. Waiting for it.");
      return;
    }
    endTalkTurn(id);
    talkError(`The message may not have been sent: ${error.message}`);
    await loadTalk(id);
  }
}

async function closeTalk() {
  const id = state.talkDetail.id;
  const button = $("talk-close-go");
  if (!id) return;
  button.disabled = true;
  try {
    const talk = await postJson(API.talkClose(id), {});
    state.talkDetail.talk = talk;
    renderTalk();
    await loadTalks();
    announce("Conversation closed.");
    ok();
  } catch (error) {
    fail(`Could not close the conversation: ${error.message}`);
  } finally {
    button.disabled = false;
  }
}

async function reopenTalk() {
  const id = state.talkDetail.id;
  const button = $("talk-reopen-go");
  if (!id) return;
  button.disabled = true;
  try {
    const talk = await postJson(API.talkReopen(id), {});
    state.talkDetail.talk = talk;
    renderTalk();
    await loadTalks();
    announce("Conversation reopened.");
    ok();
  } catch (error) {
    fail(`Could not reopen the conversation: ${error.message}`);
  } finally {
    button.disabled = false;
  }
}

/* Armed by the talk's own id, the same two-step confirm `renderRunDelete`
   uses: comparing against `talk.id` rather than a plain boolean means
   navigating to a different conversation resets the confirm state for free,
   with no separate "leaving this view" hook to remember to call. */
let armedTalkDelete = null;
let armedTalkDeleteFocused = null;

function renderTalkDelete(talk) {
  const box = $("talk-delete-box");
  if (!box) return;
  clear(box);

  const armed = armedTalkDelete === talk.id;
  if (!armed) armedTalkDeleteFocused = null;
  if (armed) {
    const cancel = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Cancel",
      onclick: () => {
        armedTalkDelete = null;
        renderTalkDelete(talk);
      },
    });
    const confirm = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Yes, delete conversation",
      onclick: () => deleteTalk(talk.id),
    });
    box.append(
      el("div", { class: "stakes-confirm" },
        el("p", { class: "stakes-warn", text: "Deleting removes the whole conversation and its artifacts. This cannot be undone." }),
        el("div", { class: "stakes-row" }, cancel, confirm),
      ),
    );
    if (armedTalkDeleteFocused !== talk.id) {
      armedTalkDeleteFocused = talk.id;
      requestAnimationFrame(() => cancel.focus({ preventScroll: true }));
    }
  } else {
    box.append(
      el("button", {
        class: "btn btn-quiet",
        type: "button",
        text: "Delete conversation…",
        onclick: () => {
          armedTalkDelete = talk.id;
          renderTalkDelete(talk);
        },
      }),
    );
  }
}

async function deleteTalk(id) {
  try {
    await deleteReq(API.talkDelete(id));
    ok();
    announce("Conversation deleted.");
    armedTalkDelete = null;
    await loadTalks();
    location.hash = "#/chat";
  } catch (error) {
    armedTalkDelete = null;
    fail(`Could not delete the conversation: ${error.message}`);
    renderTalk();
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
    show($("run-active-panel"), false);
    /* The fab and its sheet stay reachable across this route (see
       applyRoute), so a switch to a different run id \u2014 the daemon strip's
       currentRunLink, or back/forward between two run pages \u2014 must not leave
       the previous run's Resume/Fold/Delete buttons sitting in the sheet:
       their onclick closures still carry the old id, and Delete says itself
       "cannot be undone". Clearing here, before the new run's data arrives,
       is what used to happen for free when the whole panel was hidden. */
    clear($("run-actions-box"));
    clear($("run-delete-box"));
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
  const rail = PHASES.includes(status) || parkedAt
    ? phaseRail(status, parkedAt, activeNote(run))
    : null;
  if (rail) head.append(rail);
  if (meta.note) head.append(el("p", { class: "card-note", text: meta.note }));

  setText($("run-h"), firstLine(run.instruction) || shortId(run.id));

  const created = when(run.created_at);
  const updated = when(run.updated_at);
  const repoName = typeof run.repo === "string" ? run.repo.split(/[\\/]/).filter(Boolean).pop() : "";
  setText($("run-meta"),
    `${shortId(run.id)} \u00b7 ${repoName} \u00b7 ${run.base_branch || ""} \u00b7 started ${created.text} \u00b7 updated ${updated.text}`);
  setAttr($("run-meta"), "title", `${run.id}\n${run.repo || ""}\nstarted ${created.title}\nupdated ${updated.title}`);

  const instructionEl = $("run-instruction");
  if (instructionEl.dataset.forRun !== run.id) {
    instructionEl.dataset.forRun = run.id;
    renderMd(instructionEl, run.instruction_md);
  }

  renderAsks(run);
  renderLand(run);
  renderActive(run);
  renderVerdict(run);
  renderCandidates(run);
  renderReviews(run);
  renderQuota(run);
  renderTimeline(run);
  renderRunActions(run);
  renderRunDelete(run);

  setText($("run-report"), report === null ? "Loading\u2026" : report);
}

let armedRunDelete = null;
/* Mirrors armedFoldFocused: only the render that just armed the delete
   confirmation moves focus to Cancel, not every periodic redraw after it. */
let armedRunDeleteFocused = null;

function runDeleteReason(run) {
  const terminal = ["merged", "ready", "stalled", "blocked", "failed"].includes(String(run.status || ""));
  if (!terminal) {
    return "This run is still in flight and cannot be deleted.";
  }
  if (unfolded(run)) {
    return "Fold the candidate worktrees first \u2014 the button below does it.";
  }
  return null;
}

/* Whether any candidate still holds a worktree and a branch. Delete refuses
   these, and folding is how an operator clears them; before there was a
   button, the deck told a phone to go and run `magi fold` in a terminal. */
function unfolded(run) {
  const candidates = Array.isArray(run.candidates) ? run.candidates : [];
  return candidates.some((c) => !c.folded);
}

let armedFold = null;
/* Which armed run last received the focus-on-arm below, so a periodic
   re-render (loadRun runs every 5s) does not steal focus back to Cancel on
   every redraw — only the render that actually just armed does. */
let armedFoldFocused = null;
let foldBusy = null;
let resumeBusy = null;

/* Fold and resume are opposites and share this row, so the copy has to be
   blunt about it: folding throws away the worktrees a resume would continue
   from. Resume is offered first for that reason. */
function renderRunActions(run) {
  const box = $("run-actions-box");
  if (!box) return;
  clear(box);

  const status = String(run.status || "");
  if (["stalled", "blocked"].includes(status)) {
    const busy = resumeBusy === run.id;
    const gone = !unfolded(run);
    box.append(
      el("div", { class: "stakes-confirm" },
        el("button", {
          class: "btn",
          type: "button",
          text: busy ? "Resuming\u2026" : "Resume this run",
          disabled: busy || gone,
          onclick: () => resumeRun(run.id),
        }),
        el("p", { class: "card-note", text: gone
          ? "The candidate worktrees are gone, so there is nothing left to continue from. File the task again instead."
          : "Carries on from where it stopped, re-asking only the seats that went missing. It spends agent calls." }),
      ),
    );
  }

  if (!unfolded(run)) return;

  if (armedFold === run.id) {
    const cancel = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Cancel",
      onclick: () => { armedFold = null; renderRunActions(run); },
    });
    box.append(
      el("div", { class: "stakes-confirm" },
        el("p", { class: "stakes-warn", text: "Folding removes this run's worktrees and branches. Anything not committed goes with them, and the run can no longer be resumed." }),
        el("div", { class: "stakes-row" },
          cancel,
          el("button", {
            class: "btn btn-quiet",
            type: "button",
            text: "Yes, fold worktrees",
            onclick: () => foldRun(run.id),
          }),
        ),
      ),
    );
    if (armedFoldFocused !== run.id) {
      armedFoldFocused = run.id;
      requestAnimationFrame(() => cancel.focus({ preventScroll: true }));
    }
  } else {
    armedFoldFocused = null;
    box.append(
      el("div", { class: "stakes-confirm" },
        el("button", {
          class: "btn btn-quiet",
          type: "button",
          text: foldBusy === run.id ? "Folding\u2026" : "Fold worktrees\u2026",
          disabled: foldBusy === run.id,
          onclick: () => { armedFold = run.id; renderRunActions(run); },
        }),
        el("p", { class: "card-note", text: "Frees the disk this run is holding, and is what the delete button is waiting for." }),
      ),
    );
  }
}

async function foldRun(id) {
  armedFold = null;
  foldBusy = id;
  try {
    const out = await postJson(API.foldRun(id));
    ok();
    const n = Number(out.removed_count || 0);
    announce(n > 0
      ? `Folded ${shortId(id)}: ${n} worktree${n === 1 ? "" : "s"} and branches removed.`
      : `Run ${shortId(id)} had nothing left to fold.`);
    closeRunActions();
    await loadRun(id);
  } catch (error) {
    /* The alert banner sits in normal flow, under the sheet's own top-layer
       backdrop, so it must close first or the failure is unreadable. */
    closeRunActions();
    fail(`Could not fold run ${shortId(id)}: ${error.message}`);
  } finally {
    foldBusy = null;
  }
}

async function resumeRun(id) {
  resumeBusy = id;
  try {
    await postJson(API.resumeRun(id));
    ok();
    announce(`Run ${shortId(id)} is being resumed. The card will follow it.`);
    closeRunActions();
    await loadRun(id);
  } catch (error) {
    closeRunActions();
    fail(`Could not resume run ${shortId(id)}: ${error.message}`);
  } finally {
    resumeBusy = null;
  }
}

function renderRunDelete(run) {
  const box = $("run-delete-box");
  if (!box) return;
  clear(box);

  const reason = runDeleteReason(run);
  if (reason) {
    const disabledBtn = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Delete run\u2026",
      disabled: true,
    });
    box.append(
      el("div", { class: "stakes-confirm" },
        disabledBtn,
        el("p", { class: "card-note", text: reason }),
      ),
    );
    return;
  }

  const armed = armedRunDelete === run.id;
  if (!armed) armedRunDeleteFocused = null;
  if (armed) {
    const cancel = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Cancel",
      onclick: () => {
        armedRunDelete = null;
        renderRunDelete(run);
      },
    });
    const confirm = el("button", {
      class: "btn btn-quiet",
      type: "button",
      text: "Yes, delete run now",
      onclick: () => deleteRun(run.id),
    });
    box.append(
      el("div", { class: "stakes-confirm" },
        el("p", { class: "stakes-warn", text: "Deleting removes all recorded state and artifacts. This cannot be undone." }),
        el("div", { class: "stakes-row" },
          cancel,
          confirm,
        ),
      ),
    );
    if (armedRunDeleteFocused !== run.id) {
      armedRunDeleteFocused = run.id;
      requestAnimationFrame(() => cancel.focus({ preventScroll: true }));
    }
  } else {
    box.append(
      el("button", {
        class: "btn btn-quiet",
        type: "button",
        text: "Delete run\u2026",
        onclick: () => {
          armedRunDelete = run.id;
          renderRunDelete(run);
        },
      }),
    );
  }
}

async function deleteRun(id) {
  try {
    await deleteReq(API.deleteRun(id));
    ok();
    announce(`Run ${shortId(id)} removed.`);
    armedRunDelete = null;
    closeRunActions();
    location.hash = "#/runs";
  } catch (error) {
    armedRunDelete = null;
    closeRunActions();
    fail(`Could not delete run ${shortId(id)}: ${error.message}`);
  }
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
  ];
  /* `uncontested` means no panel was asked \u2014 a single viable candidate, or
     a review-only run. The panel/quorum/agreement rows below all describe a
     panel that sat, so showing them here (0 of 0 present, "still split" with
     nothing to split) would read as the same collapse a real stall produces. */
  if (tally.uncontested) {
    rows.push(["Judging", `Not needed \u2014 ${tally.uncontested}`]);
  } else {
    rows.push(
      ["First choices", votes || "\u2014"],
      ["Panel", `${Number(tally.present) || 0} of ${Number(tally.judges) || 0} present, quorum ${Number(tally.quorum) || 0}`],
      /* Quorum is the field that says whether the verdict is worth anything. */
      ["Quorum", tally.met_quorum ? "Met" : "NOT MET \u2014 the verdict is not trustworthy"],
      ["Agreement", tally.unanimous_final
        ? "Unanimous final vote"
        : `Split; ${plural(Number(tally.changed_votes) || 0, "judge", "judges")} moved`],
      ["Deliberated", tally.deliberated ? "Yes" : "No"],
    );
    if (tally.tie_break) rows.push(["Tie break", tally.tie_break]);
  }

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

/* `ReviewVote` on the wire: "approve" | "approve_with_findings" | "reject".
   Same three-colour scale a finding's severity gets, since a vote is exactly
   that kind of verdict — none, some, or stop. */
function voteTone(vote) {
  switch (vote) {
    case "approve": return "teal";
    case "approve_with_findings": return "gold";
    case "reject": return "rust";
    default: return null;
  }
}

function voteLabel(vote) {
  switch (vote) {
    case "approve": return "approve";
    case "approve_with_findings": return "approve w/ findings";
    case "reject": return "reject";
    default: return String(vote || "");
  }
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
        round.verify_retried
          ? el("span", { class: "tag", "data-tone": "gold", text: "verify retried" })
          : null,
        round.verdict
          ? el("span", { class: "tag", "data-tone": voteTone(round.verdict), text: `verdict: ${voteLabel(round.verdict)}` })
          : null,
        round.vote_split
          ? el("span", { class: "tag", "data-tone": "gold", text: "votes split" })
          : null,
        round.head ? el("span", { class: "head-sha", text: String(round.head).slice(0, 7) }) : null,
      ),
    );

    for (const record of records) {
      const findings = Array.isArray(record.findings) ? record.findings : [];
      node.append(el("div", { class: "reviewer" },
        el("p", {},
          el("span", { class: "reviewer-name", text: `reviewer ${record.reviewer} \u00b7 ${record.agent || ""}` }),
          record.vote
            ? el("span", { class: "tag", "data-tone": voteTone(record.vote), text: voteLabel(record.vote) })
            : null,
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

    // Reconsideration only ever has entries when the round's initial votes
    // split \u2014 an empty array here means the panel already agreed, same as
    // an empty judge `deliberation`.
    const reconsideration = Array.isArray(round.reconsideration) ? round.reconsideration : [];
    if (reconsideration.length) {
      node.append(el("div", { class: "reviewer" },
        el("p", {}, el("span", { class: "reviewer-name", text: "reconsideration" })),
        el("div", { class: "findings" }, reconsideration.map((rv) =>
          el("div", { class: "finding" },
            el("div", { class: "finding-top" },
              el("span", { class: "finding-id", text: `reviewer ${rv.reviewer}` }),
              rv.vote
                ? el("span", { class: "tag", "data-tone": voteTone(rv.vote), text: voteLabel(rv.vote) })
                : el("span", { class: "tag", "data-tone": "rust", text: "no revote" }),
            ),
            rv.reason ? el("p", { class: "finding-detail", text: rv.reason }) : null,
            rv.failed ? el("p", { class: "card-note", text: rv.failed }) : null,
          ))),
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
        // A lost adoption report is not "0 addressed / 0 declined": that
        // literal reads as every finding reviewed and rejected, when the
        // truth is magi never learned what the fixer did with them.
        fix.failed
          ? numbers([fix.committed ? "committed" : "no commit"])
          : numbers([
              `${addressed.length} addressed`,
              `${rejected.length} declined`,
              fix.committed ? "committed" : "no commit",
            ]),
        fix.failed ? el("p", { class: "card-note", text: `adoption report lost: ${fix.failed}` }) : null,
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

/* One line for the phase rail's label: which seat(s) the current phase is
   still waiting on, or null when nothing is out. Seat identifiers only —
   never an agent id, since a judge or reviewer seat is blind. */
function activeNote(run) {
  const active = run.active && typeof run.active === "object" ? run.active : {};
  const seats = Object.keys(active).sort();
  if (seats.length === 0) return null;
  if (!run.live) return `${plural(seats.length, "seat", "seats")} left mid-answer by a dead process`;
  return seats.length === 1
    ? `${seats[0]} has not answered yet`
    : `${plural(seats.length, "seat", "seats")} have not answered yet (${seats.join(", ")})`;
}

/* Seats still mid-answer, for the panel between "Landing" and "Verdict" —
   the same place in the column as the ask panel's reasoning: while a seat is
   still out, nothing below it that depends on the panel is going to change.

   `run.active` is keyed by seat, not by candidate or judge number, and on
   purpose carries no agent id: a judge or reviewer seat is blind, and the
   identifier alone ("judge-2", "review-1") is what the operator needs to
   answer "which seat is quiet" without this view becoming a second place
   that could leak who is behind it mid-run. `run.live` says whether a daemon
   is actually still asking these seats anything right now, or whether they
   are a leftover from a process that died before it could say so itself —
   see `ActiveSeat`'s Rust docs for why the entry alone never proves that. */
function renderActive(run) {
  const active = run.active && typeof run.active === "object" ? run.active : {};
  const seats = Object.keys(active).sort();
  show($("run-active-panel"), seats.length > 0);
  if (seats.length === 0) return;

  setText($("run-active-count"), String(seats.length));
  const note = $("run-active-note");
  show(note, !run.live);
  if (!run.live) {
    setText(note, "No live daemon claims this run right now — likely left behind by a killed process, not a seat that is actually still working.");
  }

  const list = $("run-active");
  clear(list);
  const now = Date.now();
  for (const key of seats) {
    const a = active[key];
    const startedMs = Date.parse(a.started_at || "");
    const elapsed = Number.isFinite(startedMs) ? Math.max(Math.round((now - startedMs) / 1000), 0) : null;
    const budget = Number(a.timeout_secs) || 0;
    const remaining = elapsed === null ? null : Math.max(budget - elapsed, 0);
    const retry = Number(a.attempt) > 0 ? ` · retry ${a.attempt}` : "";
    list.append(el("li", {},
      el("span", { class: "seat", text: key }),
      el("span", { text: `${a.node || "?"}${retry}` }),
      el("span", {
        text: elapsed === null
          ? "in progress"
          : `${elapsed}s elapsed · ${remaining}s left of ${budget}s`,
      }),
    ));
  }
}

/* Local clock tick, not a fetch: `renderActive` only recomputes the elapsed /
   remaining text from data already in hand, so a run with one seat quiet for
   ten minutes does not sit there showing the number from whenever the change
   stream last had a reason to fire. Nothing here talks to the network. */
function tickActive() {
  if (state.route.name === "run" && state.detail.run) renderActive(state.detail.run);
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

/* A restart hands the address from one process to the next, and the gap is
   meant to be sub-second (see `bind_waiting` server-side) - a fetch landing
   in it is not a fault. Reported as an error it used to read
   `Cannot reach magi: Failed to fetch` with a Retry button that fixed
   nothing: this page already retries on its own by polling health, and
   reconnects the moment the successor is listening. Returns whether it
   handled the failure, so `loadHealth` knows not to also call `fail`.
   `UPGRADE_WAIT_LIMIT_MS` is the one exception - past it, patience stops
   being the right read and a human is told instead. */
function reportUnreachableDuringUpgrade(error) {
  const upgradeInfo = state.health && state.health.upgrade;
  if (!upgradeInfo || !UPGRADE_BUSY_STAGES.has(upgradeInfo.stage)) return false;
  if (upgradeOverdue(upgradeInfo)) {
    fail(`Cannot reach magi: ${error.message}. It was replacing itself with ${upgradeInfo.to || "a new release"} and has not come back in over an hour — check on it by hand.`);
    return true;
  }
  setAttr($("daemon"), "data-state", "upgrading");
  setAttr($("daemon"), "data-owned", null);
  const why = $("loop-why");
  setText(why, "The deck is restarting on the new build. This page reconnects on its own.");
  show(why, true);
  show($("loop-toggle"), false);
  return true;
}

async function loadHealth({ applyRevisions = false } = {}) {
  try {
    state.health = await getJson(API.health);
    /* Through applyLoop rather than a bare render, so a loop that went quiet
       between ticks is confirmed here too: with the stream up, this interval
       is the only thing that asks. */
    if (state.health.loop) applyLoop(state.health.loop);
    else renderLoop();
    /* health.questions_open is the count until /api/questions has answered,
       so the indicator is right on the very first paint. */
    renderAskBar();
    if (applyRevisions) await applyRevisions_(state.health);
    ok();
  } catch (error) {
    if (!reportUnreachableDuringUpgrade(error)) fail(`Cannot reach magi: ${error.message}`);
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
  const talksRev = source.talks_rev;
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
  /* See the `chatsRev` block above: same reasoning, the standing chat's own
     store and its own revision. */
  if (talksRev !== state.rev.talks) {
    state.rev.talks = talksRev;
    jobs.push(loadTalks());
    if (state.route.name === "talk" && state.talkDetail.id) jobs.push(loadTalk(state.talkDetail.id));
  }
  /* Bumped by this process whenever the loop it owns starts, stops, claims or
     finishes, so a phone learns about a tap it did not make. Guarded on the
     field existing: a payload without it must not refetch every tick. */
  if (source.loop_rev !== undefined && source.loop_rev !== state.rev.loop) {
    state.rev.loop = source.loop_rev;
    jobs.push(loadLoop());
  }
  if (jobs.length) {
    const saidBefore = saidCount;
    await Promise.allSettled(jobs);
    /* Only the generic word, and only when nothing better was said: a job in
       this batch may have announced the loop stopping, and overwriting that
       with "Updated." would be the one sentence the operator needed lost. */
    if (saidCount === saidBefore) announce("Updated.");
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
  if (parts[0] === "chat" && parts[1]) return { name: "talk", id: decodeURIComponent(parts[1]) };
  if (parts[0] === "chat") return { name: "talks", id: null };
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
  show($("view-talks"), route.name === "talks");
  show($("view-talk"), route.name === "talk");

  /* The fab is the only entry point into Resume / Fold / Delete, so it must
     not survive a navigation away from the run it belongs to — nor stay
     around to be tapped from another screen. */
  show($("run-actions-fab"), route.name === "run");
  if (route.name !== "run") closeRunActions();

  const section = route.name === "run" ? "runs"
    : route.name === "chat" ? "chats"
    : route.name === "talk" ? "talks"
    : route.name;
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
    /* A chat routes to the last turn's top instead of the page top. Set
       before the renderChat() calls below — including the synchronous one a
       few lines down — so it is already true whether the chat is fetched
       here (loadChat's later renderChat picks it up) or was already cached
       (e.g. startChat() sets state.chatDetail before changing the hash, so
       the render below is the only one that runs). Arriving via a hash
       change to the same chat (e.g. tapping the same conversation again) is
       not a route change here, so it scrolls to top as before and
       openingChat stays false. */
    if (changed) state.openingChat = true;
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

  /* Same rule as the chat block above, for the standing chat's own store. */
  if (route.name === "talk") {
    if (state.talkDetail.id !== route.id) {
      state.talkDetail = { id: route.id, talk: null };
      loadTalk(route.id);
    }
    renderTalk();
  } else if (state.talkDetail.id) {
    state.talkDetail = { id: null, talk: null };
  }

  if (changed) window.scrollTo({ top: 0 });
  /* The operator arrived to answer one specific thing, so the caret goes on
     it rather than on the top of the document. */
  if (changed && route.name === "questions") focusFirstAsk();
  /* Same for a Plan control that navigated here: the caret goes on the idea
     box, not the top of the document. */
  if (changed && route.name === "chats" && state.planFocus) {
    state.planFocus = false;
    requestAnimationFrame(() => $("f-idea").focus());
  }
  /* No revision to poll here (see the "design deliberations" block above),
     so a first arrival is the trigger - same as `loadRepos` never being
     re-fetched on its own either. */
  if (route.name === "chats" && state.drafts === null) loadDrafts();
  renderTitle();
}

/* ---- run actions sheet -------------------------------------------------- */
function openRunActions() {
  const dialog = $("run-actions-sheet");
  if (!dialog.open) dialog.showModal();
}

function closeRunActions() {
  const dialog = $("run-actions-sheet");
  if (dialog.open) dialog.close();
}

/* ---- task edit sheet ----------------------------------------------------
 * Full-text replacement, not append: the operator may want to rewrite the
 * task as much as add to it, so the field opens with the current instruction
 * already in it rather than blank - overwriting is how "add a clause" gets
 * typed, but starting from nothing is how the rest of it gets lost. */
let editingTaskId = null;

function openTaskEdit(task) {
  editingTaskId = task.id;
  $("task-edit-title").value = task.title || "";
  $("task-edit-instruction").value = task.instruction || "";
  show($("task-edit-error"), false);
  setText($("task-edit-error"), "");
  const dialog = $("task-edit-sheet");
  if (!dialog.open) dialog.showModal();
  requestAnimationFrame(() => $("task-edit-title").focus({ preventScroll: true }));
}

function closeTaskEdit() {
  const dialog = $("task-edit-sheet");
  if (dialog.open) dialog.close();
  editingTaskId = null;
}

async function saveTaskEdit() {
  if (!editingTaskId) return;
  const id = editingTaskId;
  const title = $("task-edit-title").value.trim();
  const instruction = $("task-edit-instruction").value;
  if (!title || !instruction.trim()) {
    setText($("task-edit-error"), "Give both a title and an instruction.");
    show($("task-edit-error"), true);
    return;
  }
  const button = $("task-edit-save");
  const label = button.textContent;
  button.disabled = true;
  setText(button, "Saving…");
  try {
    await postJson(API.editTask(id), { title, instruction });
    ok();
    announce(`Task ${shortId(id)} edited.`);
    closeTaskEdit();
    await loadQueue();
  } catch (error) {
    setText($("task-edit-error"), error.message);
    show($("task-edit-error"), true);
  } finally {
    button.disabled = false;
    setText(button, label);
  }
}

/* ---- plan -------------------------------------------------------------- */
/* Planning is the only way work gets in from this UI: an agent interviews
   the operator into a task file, the way `magi plan` would in a terminal.
   So every Plan control - the nav items and the empty-state buttons - lands
   on the planning page and puts the caret in the idea box, even when that
   page is already showing. */
function openPlan() {
  if (location.hash === "#/plan") {
    /* Already there, so no hash change will unwrap a hidden view: the caret
       can go straight in. */
    requestAnimationFrame(() => $("f-idea").focus());
    return;
  }
  /* On the way there the focus has to wait for applyRoute to unhide the
     panel - an element inside a hidden view cannot take focus. The flag
     rides the route change and is cleared once the caret has landed. */
  state.planFocus = true;
  location.hash = "#/plan";
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
  for (const entry of document.querySelectorAll("[data-plan]")) {
    entry.addEventListener("click", openPlan);
  }

  $("run-actions-fab").addEventListener("click", openRunActions);
  $("run-actions-close").addEventListener("click", closeRunActions);
  /* Clicking the backdrop hits the dialog element itself, since nothing else
     is there to catch it — a click on the sheet's own content lands on a
     descendant instead and never reaches this listener. */
  $("run-actions-sheet").addEventListener("click", (event) => {
    if (event.target === event.currentTarget) closeRunActions();
  });
  /* Fires for every close path — the close button, Escape (which the
     dialog's default "cancel" handling turns into a close), and the backdrop
     handler above — so the focus return only has to live in one place. */
  $("run-actions-sheet").addEventListener("close", () => {
    $("run-actions-fab").focus({ preventScroll: true });
  });

  $("task-edit-close").addEventListener("click", closeTaskEdit);
  $("task-edit-save").addEventListener("click", saveTaskEdit);
  $("task-edit-sheet").addEventListener("click", (event) => {
    if (event.target === event.currentTarget) closeTaskEdit();
  });
  $("task-edit-sheet").addEventListener("close", () => {
    editingTaskId = null;
  });

  $("runs-filter-clear").addEventListener("click", clearRunsFilter);

  $("theme-toggle").addEventListener("click", () => {
    const next = THEMES[(THEMES.indexOf(currentTheme()) + 1) % THEMES.length];
    applyTheme(next);
  });

  $("wrap-toggle").addEventListener("click", (event) => {
    state.wrap = !state.wrap;
    event.currentTarget.setAttribute("aria-pressed", String(state.wrap));
    $("run-report").dataset.wrap = state.wrap ? "1" : "0";
  });

  $("chat-draft-raw-toggle").addEventListener("click", () => {
    state.draftRaw = !state.draftRaw;
    applyDraftView();
  });

  $("alert-retry").addEventListener("click", () => {
    ok();
    loadHealth({ applyRevisions: true });
    loadQuestions();
    loadChats();
    if (state.route.name === "run" && state.detail.id) loadRun(state.detail.id);
    if (state.route.name === "chat" && state.chatDetail.id) loadChat(state.chatDetail.id);
    if (state.route.name === "talk" && state.talkDetail.id) loadTalk(state.talkDetail.id);
  });

  $("chat-start-go").addEventListener("click", startChat);
  $("chat-say").addEventListener("submit", sendTurn);
  $("chat-file").addEventListener("click", fileDraft);
  $("chat-derive-go").addEventListener("click", deriveChat);

  $("talk-start-go").addEventListener("click", startTalk);
  $("talk-say").addEventListener("submit", sendTalkTurn);
  $("talk-close-go").addEventListener("click", closeTalk);
  $("talk-reopen-go").addEventListener("click", reopenTalk);
  /* Same accommodation `f-say` gets: Enter is a newline on a phone, Ctrl/Cmd
     with Enter sends. */
  $("f-talk-say").addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      $("talk-say").requestSubmit();
    }
  });

  /* Picking a repository fills the text field beside it, which is what
     actually gets sent - so a checkout outside `[repos] roots` stays
     reachable by typing a path even when the picker has nothing in it. */
  $("chat-start-repo-select").addEventListener("change", (event) => {
    $("chat-start-repo").value = event.target.value;
  });
  $("chat-derive-repo-select").addEventListener("change", (event) => {
    $("chat-derive-repo").value = event.target.value;
  });
  $("chat-start-repo-refresh").addEventListener("click", () => loadRepos(true));
  $("plan-drafts-refresh").addEventListener("click", () => loadDrafts());
  $("plan-draft-detail-close").addEventListener("click", closeDraftDetail);
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
    state.rev.talks = state.health.talks_rev;
    state.rev.loop = state.health.loop_rev;
    if (state.health.loop) state.loop = state.health.loop;
  }
  await Promise.allSettled([
    loadRuns(), loadQueue(), loadQuestions(), loadChats(), loadTalks(), loadRepos(false),
  ]);

  subscribe();
  setInterval(() => {
    if (document.hidden) return;
    loadHealth({ applyRevisions: !state.streamOpen });
  }, HEALTH_MS);
  setInterval(tickActive, 1000);
}

boot();
