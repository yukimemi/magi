//! Rendering checks for the web UI, in a real (headless) Chrome.
//!
//! The Rust-side tests of `web.rs` prove the API and the embedded assets are
//! served; none of them can see that a Queue row has lost its title or that two
//! labels sit on top of each other, because that only exists once CSS has laid
//! the page out. This test starts the real router against a seeded temp home,
//! opens the Queue, Runs and Chat pages at a desktop and a phone width, and
//! asserts *structural* invariants (a row has a visible title, nothing spills
//! out of its pane, the sticky header does not cover the heading, the console
//! stays clean) rather than pixels, so it does not need a golden image.
//!
//! No Chrome on the machine: the test says so and passes, unless `CI` is set,
//! where a missing browser is a failure (a check that silently stops running is
//! worse than one that fails). `MAGI_CHROME` names a binary explicitly.
//!
//! The server binds port 0. Never hard-code a port here.

#[path = "web_render/cdp.rs"]
mod cdp;
// This binary uses the home lock and the fixture, not the `e2e!` wrapper.
#[allow(unused_macros, unused_imports)]
mod common;

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::time::Duration;

use magi::queue::{Queue, Source, Task, TaskStatus};
use magi::run::{RunState, RunStatus};
use magi::talk::{self, Talks};
use serde_json::Value;

/// One page under test.
struct Case {
    name: &'static str,
    route: &'static str,
    /// The list pane whose width must hold.
    pane: &'static str,
    /// Every row of the list, whatever section it sits in.
    rows: &'static str,
    /// The page's `<h1>`.
    heading: &'static str,
    /// Selector for one seeded item, given its id; used to know the page has
    /// finished loading.
    link: fn(&str) -> String,
    /// How many of the seeded items (in seed order) the page shows without
    /// anything being expanded. Runs keeps finished ones in a collapsed
    /// section that renders no rows until opened.
    expect: usize,
}

const CASES: [Case; 3] = [
    Case {
        name: "queue",
        route: "#/queue",
        pane: "#view-queue",
        rows: "#queue-sections li.card:not(.skeleton)",
        heading: "queue-h",
        link: |id| format!("a[href=\"#/queue/{id}\"]"),
        expect: 5,
    },
    Case {
        name: "runs",
        route: "#/runs",
        pane: ".runs-main",
        rows: "#view-runs a.card.run-card",
        heading: "runs-h",
        link: |id| format!("a[href=\"#/runs/{id}\"]"),
        expect: 1,
    },
    Case {
        name: "chat",
        route: "#/chat",
        pane: "#view-talks",
        rows: "#talks-list a.card",
        heading: "talks-h",
        link: |id| format!("a[href=\"#/chat/{id}\"]"),
        expect: 3,
    },
];

/// `(width, height, mobile)`.
const VIEWPORTS: [(u32, u32, bool); 3] =
    [(1280, 900, false), (1950, 1000, false), (390, 844, true)];

/// Assertions that are known to fail today because another task is already
/// fixing the layout they describe. Matched as `(page, check)`; `*` is any
/// page. The test reports a hit instead of failing, and says so loudly when an
/// entry no longer fires so it gets removed.
///
/// Keep this to the specific assertion: no page-wide exclusion, no `#[ignore]`.
const KNOWN_FAILURES: &[(&str, &str, &str)] = &[];

/// The measurement, run inside the page. Returns `{ rows, failures: [[check,
/// detail]] }`. `ROWS`, `PANE` and `HEAD` are substituted as JSON strings.
const MEASURE: &str = r##"(() => {
  const ROWS = __ROWS__, PANE = __PANE__, HEAD = __HEAD__;
  const fails = [];
  const shown = (e) => {
    const r = e.getBoundingClientRect(), cs = getComputedStyle(e);
    return r.width > 0 && r.height > 0 && cs.display !== "none" && cs.visibility !== "hidden";
  };
  const hit = (a, b) =>
    a.left < b.right - 1 && b.left < a.right - 1 && a.top < b.bottom - 1 && b.top < a.bottom - 1;
  const pane = document.querySelector(PANE);
  if (!pane) fails.push(["pane-missing", PANE]);
  const pr = pane ? pane.getBoundingClientRect() : null;
  const pr0 = pr;
  if (pane && pane.scrollWidth > pane.clientWidth + 1)
    fails.push(["pane-overflow", `${[...pane.querySelectorAll("*")].filter((e) => e.getBoundingClientRect().right > pr0.right + 1).slice(0, 3).map((e) => e.tagName + "." + e.className + "#" + e.id).join(" | ")} ${PANE} scrollWidth ${pane.scrollWidth} > clientWidth ${pane.clientWidth}`]);
  let rows = 0;
  for (const row of document.querySelectorAll(ROWS)) {
    if (!shown(row)) continue; // a collapsed section's rows have no layout
    rows++;
    const rr = row.getBoundingClientRect();
    const link = row.querySelector("a[href]");
    const label = (row.getAttribute("href") || (link && link.getAttribute("href")) || "row").toString();
    if (pr && (rr.left < pr.left - 1 || rr.right > pr.right + 1))
      fails.push(["row-overflow", `${label}: ${rr.left}..${rr.right} outside ${pr.left}..${pr.right}`]);
    const title = row.querySelector(".card-title");
    if (!title || !shown(title) || !(title.innerText || "").trim()) {
      fails.push(["title-visible", `${label}: no visible title text`]);
      continue;
    }
    const tr = title.getBoundingClientRect();
    // Line 1 of a row is the title on the full row width, never a sliver.
    if (tr.width < rr.width * 0.5)
      fails.push(["title-narrow", `${label}: title ${Math.round(tr.width)}px of a ${Math.round(rr.width)}px row`]);
    for (const other of row.querySelectorAll(".card-top > *, .card-meta > *")) {
      if (other.contains(title) || title.contains(other) || !shown(other)) continue;
      if (hit(tr, other.getBoundingClientRect()))
        fails.push(["title-overlap", `${label}: title overlaps ${other.className || other.tagName}`]);
    }
  }
  window.scrollTo(0, 0);
  const h = document.getElementById(HEAD);
  if (!h || !shown(h)) fails.push(["heading-covered", `${HEAD} is not visible`]);
  else {
    const r = h.getBoundingClientRect();
    const top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    if (!top || !h.contains(top))
      fails.push(["heading-covered", `${HEAD} is covered by ${top ? (top.id || top.className || top.tagName) : "nothing"}`]);
  }
  return { rows, failures: fails };
})()"##;

/// Console errors, uncaught exceptions and browser-logged errors (CSP
/// violations arrive as `Log.entryAdded` with `source: security`).
fn console_problems(events: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for e in events {
        let p = &e["params"];
        match e["method"].as_str().unwrap_or("") {
            "Runtime.exceptionThrown" => out.push(format!(
                "exception: {}",
                p["exceptionDetails"]["exception"]["description"]
                    .as_str()
                    .or(p["exceptionDetails"]["text"].as_str())
                    .unwrap_or("?")
            )),
            "Runtime.consoleAPICalled" if p["type"] == "error" => {
                out.push(format!("console.error: {}", p["args"]));
            }
            "Log.entryAdded" if p["entry"]["level"] == "error" => {
                let entry = &p["entry"];
                let text = entry["text"].as_str().unwrap_or("");
                let url = entry["url"].as_str().unwrap_or("");
                // The only tolerated noise: a browser asking for a favicon.
                if url.contains("favicon") || text.contains("favicon") {
                    continue;
                }
                out.push(format!(
                    "log[{}]: {text} {url}",
                    entry["source"].as_str().unwrap_or("?")
                ));
            }
            _ => {}
        }
    }
    out
}

fn known(page: &str, check: &str) -> Option<&'static str> {
    KNOWN_FAILURES
        .iter()
        .find(|(p, c, _)| (*p == page || *p == "*") && *c == check)
        .map(|(_, _, why)| *why)
}

/// Start the real router on an OS-chosen loopback port and return its URL.
async fn serve(
    home: &std::path::Path,
    queue: Queue,
    talks: Talks,
    runs_root: std::path::PathBuf,
    repo: &std::path::Path,
) -> String {
    serve_with_client_probe(home, queue, talks, runs_root, repo, ClientProbe::None).await
}

enum ClientProbe {
    None,
    Queue,
    Settings,
    Questions,
    Connection,
    Run,
    Upgrade,
}

// Expose the live module's functions without installing a second client that
// could render the same DOM from independent state.
async fn serve_with_client_probe(
    home: &std::path::Path,
    queue: Queue,
    talks: Talks,
    runs_root: std::path::PathBuf,
    repo: &std::path::Path,
    probe: ClientProbe,
) -> String {
    let worktrees = home.join("wt").join("magi");
    std::fs::create_dir_all(&worktrees).unwrap();
    let ui = magi::web::Ui::new(
        queue,
        magi::ask::Questions::at(home.join("questions")),
        talks,
        runs_root,
        home.to_path_buf(),
        repo.to_path_buf(),
    )
    .with_worktrees_root(worktrees);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind loopback");
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let boot = !matches!(probe, ClientProbe::Connection | ClientProbe::Upgrade);
    let exports = match probe {
        ClientProbe::None => None,
        ClientProbe::Queue => Some(
            "setChip, RUN_STATUS, TASK_STATUS, QUESTION_STATUS, TALK_STATUS, loadQueue, openTaskEdit, saveTaskEdit, closeTaskEdit",
        ),
        ClientProbe::Settings => Some("settingsState, renderSettings, loadSettings, saveSettings"),
        ClientProbe::Questions => Some(
            "state, storeReads, loadQuestionOrigins, shortId, renderQuestions, renderAsks, openPanel, closePanel, focusFirstAsk",
        ),
        ClientProbe::Run => Some("state, loadRun"),
        ClientProbe::Upgrade => {
            Some("state, upgrade, renderLoop, renderUpgradeError, loadHealth, ok")
        }
        ClientProbe::Connection => Some(
            "state, loadHealth, loadLoop, loadTalk, tickTalkWaits, loadStats, resumeConnection, onPageShow, unreachable, loadQueue, applyRevisions_, acceptTalkUpdate, renderTalkTasks",
        ),
    };
    let router = ui.router().layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| async move {
            if let Some(exports) = exports.filter(|_| request.uri().path() == "/app.js") {
                use axum::response::IntoResponse;
                let source = include_str!("../assets/ui/app.js");
                let source = source
                    .trim_end()
                    .strip_suffix("boot();")
                    .expect("client boot entry");
                return (
                    [(axum::http::header::CONTENT_TYPE, "text/javascript")],
                    format!(
                        "{source}\nwindow.deck = {{ {exports} }};\n\
                         window.queueReady = {};",
                        if boot { "boot()" } else { "Promise.resolve()" }
                    ),
                )
                    .into_response();
            }
            next.run(request).await
        },
    ));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    base
}

/// Seed the Queue: the first five tasks (in this order) are what the layout
/// test waits for; the rest cover the other sections and row shapes (blocked,
/// done, agent-filed from a chat, solo, attempts, no title).
fn seed_tasks(queue: &Queue, repo: &std::path::Path, run_id: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let long =
        "A task whose title is long enough to wrap onto several lines on a narrow phone screen";
    for (title, status) in [
        ("Running task", TaskStatus::Running),
        ("Queued task", TaskStatus::Queued),
        ("Held task", TaskStatus::Held),
        ("Failed task", TaskStatus::Failed),
        (long, TaskStatus::Queued),
        ("Blocked task", TaskStatus::Blocked),
        ("Done task", TaskStatus::Done),
        ("Solo task from a chat", TaskStatus::Queued),
        ("Task that was tried twice", TaskStatus::Running),
        ("", TaskStatus::Queued),
    ] {
        let source = if title.starts_with("Solo") {
            Source::Agent {
                run: "7dd7aaaa".to_owned(),
                node: magi::queue::CHAT_NODE.to_owned(),
            }
        } else {
            Source::Human
        };
        let mut t = Task::new(
            title.to_owned(),
            format!("Instruction for: {title}"),
            repo.to_path_buf(),
            source,
        );
        t.status = status;
        t.solo = title.starts_with("Solo");
        if title.contains("twice") {
            t.attempts = 2;
        }
        match status {
            // A run that exists, so the page does not chase a missing id.
            TaskStatus::Running => t.runs = vec![run_id.to_owned()],
            TaskStatus::Held => t.hold_reason = Some("held by hand".to_owned()),
            TaskStatus::Failed => t.last_error = Some("the gate failed".to_owned()),
            _ => {}
        }
        queue.put(&mut t).expect("seed task");
        ids.push(t.id.clone());
    }
    ids
}

#[tokio::test]
async fn pages_render_with_visible_titles_and_no_console_errors() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");

    // --- seed ------------------------------------------------------------
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let runs_root = home.join("runs");

    let mut run_ids = Vec::new();
    for (id, status, text) in [
        (
            "20260901-000001-aa01",
            RunStatus::Reviewing,
            "Add a retry budget to the fetch loop",
        ),
        (
            "20260901-000002-aa02",
            RunStatus::Blocked,
            "Rework the settings screen so that every field carries a long and \
             rather descriptive instruction title that has to wrap on a phone",
        ),
    ] {
        let mut run = RunState::new(
            fx.repo.clone(),
            "main".to_owned(),
            "0000000".to_owned(),
            text.to_owned(),
            fx.config.clone(),
        );
        run.id = id.to_owned();
        run.status = status;
        run.save_under(&home).expect("seed run");
        run_ids.push(id.to_owned());
    }

    let task_ids = seed_tasks(&queue, &fx.repo, &run_ids[0]);

    let agent = fx.config.agents[0].id.clone();
    let mut talk_ids = Vec::new();
    for text in [
        "How does the queue pick the next task?",
        "A much longer opening question that keeps going so the conversation \
         title has to be clipped or wrapped rather than pushing the row wide",
        "Short one",
    ] {
        let mut t =
            talk::begin(&talks, &fx.config, fx.repo.clone(), Some(&agent)).expect("seed talk");
        talk::record(&mut t, &talks, text, Vec::new()).expect("seed turn");
        talk_ids.push(t.id.clone());
    }

    // --- serve on an OS-chosen port ---------------------------------------
    let base = serve(&home, queue, talks, runs_root, &fx.repo).await;

    // --- drive ------------------------------------------------------------
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));

    let mut failures: Vec<String> = Vec::new();
    let mut hits: BTreeSet<(String, String)> = BTreeSet::new();

    for case in &CASES {
        let ids = match case.name {
            "queue" => &task_ids,
            "runs" => &run_ids,
            _ => &talk_ids,
        };
        for &(w, h, mobile) in &VIEWPORTS {
            let tag = format!("{} @{w}px", case.name);
            let url = format!("{base}{}", case.route);
            let page = browser
                .open_page(&url, w, h, mobile)
                .await
                .unwrap_or_else(|e| panic!("{tag}: open: {e}"));

            // Ready once every seeded item has its row in the DOM.
            let all = ids
                .iter()
                .take(case.expect)
                .map(|id| format!("!!document.querySelector('{}')", (case.link)(id)))
                .collect::<Vec<_>>()
                .join(" && ");
            if let Err(e) = browser.wait_for(&page, &all, Duration::from_secs(30)).await {
                // What the page showed instead is the first thing a failure needs.
                let seen = browser
                    .eval(
                        &page,
                        "document.querySelector('main').innerText.slice(0, 600)",
                    )
                    .await
                    .unwrap_or_default();
                let console = console_problems(&browser.take_events(&page.session));
                panic!("{tag}: {e}\npage text: {seen}\nconsole: {console:?}");
            }
            // Fonts, then two frames, so layout has settled before measuring.
            browser
                .eval(
                    &page,
                    "document.fonts.ready.then(() => new Promise(r => \
                     requestAnimationFrame(() => requestAnimationFrame(() => r(true)))))",
                )
                .await
                .unwrap_or_else(|e| panic!("{tag}: settle: {e}"));
            // Sections such as Held default to collapsed on a fresh profile and
            // a collapsed row has no layout; open them so every seeded row is
            // measured, not just the ones that happen to be expanded.
            browser
                .eval(
                    &page,
                    "document.querySelectorAll('details.list-section').forEach((d) => { d.open = true; })",
                )
                .await
                .unwrap_or_else(|e| panic!("{tag}: expand: {e}"));
            browser.settle(Duration::from_millis(300)).await.unwrap();

            let script = MEASURE
                .replace("__ROWS__", &Value::from(case.rows).to_string())
                .replace("__PANE__", &Value::from(case.pane).to_string())
                .replace("__HEAD__", &Value::from(case.heading).to_string());
            let out = browser
                .eval(&page, &script)
                .await
                .unwrap_or_else(|e| panic!("{tag}: measure: {e}"));

            let rows = out["rows"].as_u64().unwrap_or(0);
            if (rows as usize) < case.expect {
                failures.push(format!(
                    "{tag}: {rows} visible row(s) rendered, expected {}",
                    case.expect
                ));
            }
            for f in out["failures"].as_array().into_iter().flatten() {
                let check = f[0].as_str().unwrap_or("?");
                let detail = f[1].as_str().unwrap_or("");
                match known(case.name, check) {
                    Some(why) => {
                        eprintln!("KNOWN FAILURE [{tag}] {check} ({why}): {detail}");
                        hits.insert((case.name.to_owned(), check.to_owned()));
                    }
                    None => failures.push(format!("{tag}: {check}: {detail}")),
                }
            }
            browser.settle(Duration::from_millis(200)).await.unwrap();
            for p in console_problems(&browser.take_events(&page.session)) {
                failures.push(format!("{tag}: {p}"));
            }
            browser.close_page(&page).await;
        }
    }

    for (p, c, why) in KNOWN_FAILURES {
        if !hits.contains(&((*p).to_owned(), (*c).to_owned())) {
            eprintln!(
                "NOTE: known failure ({p}, {c}) [{why}] did not fire; \
                 remove it from KNOWN_FAILURES in tests/web_render.rs"
            );
        }
    }

    assert!(
        failures.is_empty(),
        "web UI rendering regressions:\n  {}",
        failures.join("\n  ")
    );
}

/// The Runs search over a history longer than the loaded window: a match that
/// only the server saw is listed as an extra row and must not be counted as
/// hidden, and a run whose text is markup must be shown as text, not parsed.
#[tokio::test]
async fn runs_search_counts_extra_rows_as_shown_and_never_parses_hit_text() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));

    // The page loads the newest RUN_LIMIT (50) runs; the oldest of 51 is
    // outside that window, and so is only reachable through the search.
    const WINDOW: usize = 50;
    let markup = "<img src=x id=pwn>";
    for n in 1..=WINDOW + 1 {
        let text = match n {
            1 => format!("zebrafruit outside the window {markup}"),
            n if n == WINDOW + 1 => "zebrafruit inside the window".to_owned(),
            n => format!("ordinary run {n}"),
        };
        let mut run = RunState::new(
            fx.repo.clone(),
            "main".to_owned(),
            "0000000".to_owned(),
            text,
            fx.config.clone(),
        );
        run.id = format!("20260901-{n:06}-bb{n:02}");
        run.status = RunStatus::Reviewing;
        run.save_under(&home).expect("seed run");
    }

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    let page = browser
        .open_page(&format!("{base}#/runs"), 1280, 900, false)
        .await
        .expect("open");
    let w = Duration::from_secs(30);
    browser
        // The input is in the static HTML, so it exists before the module
        // script has wired its listener; `#runs-search` is un-hidden only by
        // `renderRuns`, which runs after `wire()`.
        .wait_for(
            &page,
            "(() => { const b = document.getElementById('runs-search'); \
             return !!b && !b.hidden; })()",
            w,
        )
        .await
        .expect("search box");
    browser
        .eval(
            &page,
            "(() => { const i = document.getElementById('runs-search-input'); \
             i.value = 'zebrafruit'; i.dispatchEvent(new Event('input', { bubbles: true })); \
             return true; })()",
        )
        .await
        .expect("type");
    browser
        .wait_for(
            &page,
            "(document.getElementById('runs-search-status').textContent || '').includes('match')",
            w,
        )
        .await
        .expect("search answered");

    let out = browser
        .eval(
            &page,
            "({ status: document.getElementById('runs-search-status').textContent, \
               extra: document.querySelectorAll('#runs-search-extra li').length, \
               pwn: document.querySelectorAll('#pwn').length, \
               snippet: [...document.querySelectorAll('.card-snippet')].map((e) => e.textContent).join('\\n') })",
        )
        .await
        .expect("read");
    let status = out["status"].as_str().unwrap_or_default();
    assert!(status.starts_with("2 runs match"), "status: {status}");
    assert!(
        !status.contains("hidden"),
        "extra rows counted as hidden: {status}"
    );
    assert_eq!(
        out["extra"], 1,
        "the out-of-window hit is one extra row: {out}"
    );
    assert_eq!(out["pwn"], 0, "hit text was parsed as markup: {out}");
    assert!(
        out["snippet"].as_str().unwrap_or_default().contains(markup),
        "snippet should show the markup literally: {out}"
    );
    browser.close_page(&page).await;
}

/// A follow-up task shows where it came from: the flow chart opens with a link
/// to the parent task with a visible label, the detail page has a "Follow-up
/// of" block, and the Queue card carries a chip. Both widths.
#[tokio::test]
async fn followup_task_links_back_to_its_parent() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));

    let mut parent = Task::new(
        "Parent task that was merged".to_owned(),
        "Do the parent thing".to_owned(),
        fx.repo.clone(),
        Source::Human,
    );
    parent.status = TaskStatus::Done;
    queue.put(&mut parent).expect("seed parent");
    let mut child = Task::new(
        "Follow-up of the parent".to_owned(),
        "Fix what was left open".to_owned(),
        fx.repo.clone(),
        Source::Agent {
            run: "20260901-000001-aa01".to_owned(),
            node: "followup".to_owned(),
        },
    );
    child.followup = Some(magi::queue::FollowUp {
        run: "20260901-000001-aa01".to_owned(),
        origin_task: Some(parent.id.clone()),
        pr: "https://github.com/example/repo/pull/7".to_owned(),
        findings: vec!["R3-1-1".to_owned(), "R3-1-2".to_owned()],
        generation: 1,
    });
    child.start("missing-flow-run".to_owned());
    queue.put(&mut child).expect("seed child");

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    for &(w, h, mobile) in &VIEWPORTS {
        let page = browser
            .open_page(&format!("{base}#/tasks/{}", child.id), w, h, mobile)
            .await
            .expect("open");
        browser
            .wait_for(
                &page,
                "!!document.querySelector('#task-flow .flow-node')",
                Duration::from_secs(30),
            )
            .await
            .unwrap_or_else(|e| panic!("@{w}px: {e}"));
        let out = browser
            .eval(
                &page,
                "(() => { const n = document.querySelector('#task-flow .flow-node'); \
                   const s = n.querySelector('strong'); const r = s.getBoundingClientRect(); \
                   const b = document.getElementById('task-followup'); \
                   return { tag: n.tagName, href: n.getAttribute('href'), kind: n.dataset.kind, \
                     label: s.textContent, visible: r.width > 0 && r.height > 0, \
                     panelHidden: document.getElementById('task-followup-panel').hidden, \
                     block: b.textContent, \
                     flowTimes: [...document.querySelectorAll('#task-flow .flow-time')].map(t => ({ \
                       tag: t.tagName, text: t.textContent, datetime: t.getAttribute('datetime'), \
                       fits: t.getBoundingClientRect().right <= innerWidth })), \
                     runHref: document.querySelector('#task-flow [data-kind=run]').getAttribute('href'), \
                     contextTime: !!n.querySelector('.flow-time'), \
                     spill: document.documentElement.scrollWidth > document.documentElement.clientWidth }; })()",
            )
            .await
            .expect("read");
        let tag = format!("@{w}px: {out}");
        assert_eq!(out["tag"], "A", "{tag}");
        assert_eq!(out["kind"], "followup", "{tag}");
        assert_eq!(
            out["href"].as_str(),
            Some(format!("#/tasks/{}", parent.id).as_str()),
            "{tag}"
        );
        assert!(
            out["label"]
                .as_str()
                .is_some_and(|l| l.starts_with("Follow-up of ") && l.len() > 13),
            "{tag}"
        );
        assert_eq!(out["visible"], true, "{tag}");
        assert_eq!(out["panelHidden"], false, "{tag}");
        let block = out["block"].as_str().unwrap_or_default();
        assert!(
            block.contains("R3-1-1, R3-1-2") && block.contains("pull/7"),
            "{tag}"
        );
        assert_eq!(out["contextTime"], false, "{tag}");
        assert_eq!(out["runHref"], "#/runs/missing-flow-run", "{tag}");
        let times = out["flowTimes"].as_array().expect("flow timestamps");
        assert_eq!(times.len(), 3, "{tag}");
        assert_eq!(times[0]["tag"], "TIME", "{tag}");
        assert_eq!(times[0]["datetime"], child.created_at.to_string(), "{tag}");
        assert!(
            times[0]["text"].as_str().unwrap().starts_with("Started "),
            "{tag}"
        );
        for time in &times[1..] {
            assert_eq!(time["text"], "Start time not recorded", "{tag}");
            assert!(time["datetime"].is_null(), "{tag}");
        }
        assert!(times.iter().all(|t| t["fits"] == true), "{tag}");
        assert_eq!(out["spill"], false, "{tag}");
        browser.close_page(&page).await;

        let page = browser
            .open_page(&format!("{base}#/queue"), w, h, mobile)
            .await
            .expect("open queue");
        browser
            .wait_for(
                &page,
                "[...document.querySelectorAll('.task-card .tag')].some((e) => e.textContent === 'follow-up' && e.getClientRects().length > 0)",
                Duration::from_secs(30),
            )
            .await
            .unwrap_or_else(|e| panic!("queue @{w}px: {e}"));
        browser.close_page(&page).await;
    }
}

#[tokio::test]
async fn urgent_toggle_saves_in_queue_and_detail_and_reports_claim_conflicts() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP urgent toggle: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let mut task = Task::new(
        "Toggle urgent".into(),
        "Keep instruction".into(),
        fx.repo.clone(),
        Source::Human,
    );
    queue.put(&mut task).unwrap();
    let base = serve_with_client_probe(
        &home,
        queue.clone(),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Queue,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    for width in [320, 1280] {
        let page = browser
            .open_page(&format!("{base}#/queue"), width, 900, width < 400)
            .await
            .unwrap();
        browser
            .wait_for(
                &page,
                "!!document.querySelector('.task-urgent-box button')",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        let result = browser.eval(&page, r##"(async () => {
          const check = (ok, msg) => { if (!ok) throw new Error(msg); };
          const wait = async f => { const end = performance.now() + 5000; while (!f()) { if (performance.now() > end) throw new Error('toggle timeout'); await new Promise(r => setTimeout(r, 10)); } };
          const button = () => document.querySelector('.task-urgent-box button');
          const menu = document.querySelector('.task-action-menu'); menu.open = true;
          check(button().textContent === 'Urgent: off', 'initial saved state');
          const r = button().getBoundingClientRect();
          check(r.left >= 0 && r.right <= innerWidth, 'toggle fits viewport');
          const realFetch = window.fetch;
          let release, reached;
          const gate = new Promise(r => release = r), started = new Promise(r => reached = r);
          window.fetch = async (url, options) => { if (url.endsWith('/urgent')) { reached(); await gate; } return realFetch(url, options); };
          try {
            button().click(); await started;
            await deck.loadQueue();
            check(button().disabled, 'pending survives redraw');
            check(button().getAttribute('aria-pressed') === 'false', 'pending keeps saved state');
            release();
            await wait(() => button().getAttribute('aria-pressed') === 'true' && !button().disabled);
          } finally { release(); window.fetch = realFetch; }
          await deck.loadQueue();
          check(!document.querySelector('.tag-urgent').hidden, 'badge after save and reload');
          const id = document.querySelector('.task-card').dataset.taskId;
          location.hash = '#/tasks/' + id;
          await wait(() => document.querySelector('#task-actions-urgent button')?.getAttribute('aria-pressed') === 'true');
          document.querySelector('#task-actions-fab').click();
          const toggle = document.querySelector('#task-actions-urgent button');
          const rect = toggle.getBoundingClientRect(); check(rect.left >= 0 && rect.right <= innerWidth, 'detail toggle fits');
          toggle.click();
          await wait(() => document.querySelector('#task-actions-urgent button')?.getAttribute('aria-pressed') === 'false' && !document.querySelector('#task-actions-urgent button').disabled);
          check(!document.querySelector('#task-status .tag-urgent'), 'detail badge clears');
          return true;
        })()"##).await.unwrap();
        assert_eq!(result, true);
        assert!(!queue.get(&task.id).unwrap().urgent);
        let claim = queue.claim(&task.id).unwrap();
        let result = browser.eval(&page, r##"(async () => {
          document.querySelector('#task-actions-urgent button').click();
          const end = performance.now() + 5000;
          while (document.querySelector('#task-actions-error').hidden) {
            if (performance.now() > end) throw new Error('no refusal');
            await new Promise(r => setTimeout(r, 10));
          }
          const button = document.querySelector('#task-actions-urgent button');
          return button.getAttribute('aria-pressed') === 'false' && document.querySelector('#task-actions-error').textContent.includes('released');
        })()"##).await.unwrap();
        assert_eq!(result, true, "claim refusal preserves saved display");
        drop(claim);
        task.status = TaskStatus::Running;
        queue.put(&mut task).unwrap();
        browser
            .eval(&page, "location.hash = '#/queue'; true")
            .await
            .unwrap();
        browser
            .wait_for(
                &page,
                "document.querySelector('.task-urgent-box button')?.disabled === true",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        task.status = TaskStatus::Queued;
        queue.put(&mut task).unwrap();
        browser.close_page(&page).await;
    }
}

/// `Task::urgent` shows as an URGENT tag on the Queue card and the task page,
/// whatever the priority or status; a non-urgent task never carries one.
#[tokio::test]
async fn urgent_tasks_carry_a_visible_badge() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));

    let mut seeded = Vec::new();
    for (title, status, urgent, priority) in [
        ("Urgent queued task", TaskStatus::Queued, true, 0),
        ("Urgent held task", TaskStatus::Held, true, 0),
        ("Plain boosted task", TaskStatus::Queued, false, 1),
    ] {
        let mut t = Task::new(
            title.to_owned(),
            format!("Instruction for: {title}"),
            fx.repo.clone(),
            Source::Human,
        );
        t.status = status;
        t.urgent = urgent;
        t.priority = priority;
        if status == TaskStatus::Held {
            t.hold_reason = Some("held by hand".to_owned());
        }
        queue.put(&mut t).expect("seed task");
        seeded.push((t.id.clone(), title, urgent));
    }

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    for &(w, h, mobile) in &VIEWPORTS {
        let page = browser
            .open_page(&format!("{base}#/queue"), w, h, mobile)
            .await
            .expect("open queue");
        browser
            .wait_for(
                &page,
                "document.querySelectorAll('.task-card').length >= 3",
                Duration::from_secs(30),
            )
            .await
            .unwrap_or_else(|e| panic!("queue @{w}px: {e}"));
        for (_, title, urgent) in &seeded {
            let out = browser
                .eval(
                    &page,
                    &format!(
                        "(() => {{ const c = [...document.querySelectorAll('.task-card')].find((e) => \
                           e.querySelector('.card-title')?.textContent === {title:?}); \
                           if (!c) return null; \
                           const vis = (e) => !!e && e.getClientRects().length > 0; \
                           const u = c.querySelector('.tag-urgent'); \
                           return {{ urgent: vis(u), text: u ? getComputedStyle(u).textTransform + ':' + u.textContent : '', \
                             chip: vis(c.querySelector('.chip')) }}; }})()"
                    ),
                )
                .await
                .expect("read card");
            let tag = format!("@{w}px {title}: {out}");
            assert!(!out.is_null(), "{tag}");
            assert_eq!(out["urgent"], *urgent, "{tag}");
            if *urgent {
                assert_eq!(out["text"], "uppercase:urgent", "{tag}");
            }
            assert_eq!(out["chip"], true, "{tag}");
        }
        browser.close_page(&page).await;

        for (id, title, urgent) in &seeded {
            let page = browser
                .open_page(&format!("{base}#/tasks/{id}"), w, h, mobile)
                .await
                .expect("open task");
            browser
                .wait_for(
                    &page,
                    "!!document.querySelector('#task-status .chip')",
                    Duration::from_secs(30),
                )
                .await
                .unwrap_or_else(|e| panic!("task @{w}px: {e}"));
            let out = browser
                .eval(
                    &page,
                    "(() => { const u = document.querySelectorAll('#task-status .tag-urgent'); \
                       return { count: u.length, visible: u.length > 0 && u[0].getClientRects().length > 0 }; })()",
                )
                .await
                .expect("read task");
            let tag = format!("task @{w}px {title}: {out}");
            assert_eq!(out["count"], u64::from(*urgent), "{tag}");
            assert_eq!(out["visible"], *urgent, "{tag}");
            browser.close_page(&page).await;
        }
    }
}

#[tokio::test]
async fn blocked_run_header_shows_the_failed_gate_reason() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "CI requires Chrome");
        eprintln!("SKIP web_render: no Chrome/Chromium found");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let mut run = RunState::new(
        fx.repo.clone(),
        "main".into(),
        "abc".into(),
        "Failed gate".into(),
        fx.config.clone(),
    );
    run.status = RunStatus::Blocked;
    run.gate_ran = true;
    run.gate.push(magi::run::CommandOutcome {
        command: "cargo make check".into(),
        code: Some(1),
        output_tail: "failures:\n    web::tests::talks_report_independent_thinking_claims_and_queue_a_second_message\n\ntest result: FAILED. 1813 passed; 1 failed; 0 ignored;".into(),
        duration_ms: 0,
        resource_blocked: false,
    });
    run.save_under(&home).unwrap();
    let base = serve(
        &home,
        Queue::at(home.join("queue")),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.unwrap();
    for (width, height, mobile) in [(1280, 900, false), (390, 844, true)] {
        let page = browser
            .open_page(&format!("{base}#/runs/{}", run.id), width, height, mobile)
            .await
            .unwrap();
        browser
            .wait_for(
                &page,
                "!!document.querySelector('#run-status .run-stop-reason')",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        let result = browser
            .eval(
                &page,
                r#"(() => {
            const reason = document.querySelector('#run-status .run-stop-reason');
            const style = getComputedStyle(reason);
            return {
                text: reason.textContent, title: reason.title,
                previous: reason.previousElementSibling.textContent,
                oneLine: style.whiteSpace === 'nowrap' && style.textOverflow === 'ellipsis',
                visible: reason.getBoundingClientRect().width > 0,
                genericNote: !!document.querySelector('#run-status .card-note')
            };
        })()"#,
            )
            .await
            .unwrap();
        let expected = "Gate failed: cargo make check — web::tests::talks_report_independent_thinking_claims_and_queue_a_second_message (1 failed)";
        assert_eq!(result["text"], expected);
        assert_eq!(result["title"], expected);
        assert!(
            result["previous"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("blocked")
        );
        assert_eq!(result["oneLine"], true);
        assert_eq!(result["visible"], true);
        assert_eq!(result["genericNote"], false);
        browser.close_page(&page).await;
    }
}

/// The run detail: tabs live in the hash and survive a live refresh, the
/// landing panel never prints a null child, the deliberation strip is slim and
/// the list pane has no nested scroller. Checked at both widths, in both themes.
#[tokio::test]
async fn run_detail_tabs_landing_and_strip_hold_at_both_widths_and_themes() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));

    let id = "20260901-000009-cc09";
    let mut run = RunState::new(
        fx.repo.clone(),
        "main".to_owned(),
        "0000000".to_owned(),
        "Merged run with no fix round and no follow-up".to_owned(),
        fx.config.clone(),
    );
    run.id = id.to_owned();
    run.status = RunStatus::Merged;
    run.pr = Some(magi::run::PrRecord {
        url: "https://github.com/example/repo/pull/7".to_owned(),
        number: 7,
        state: "merged".to_owned(),
        checks: "green".to_owned(),
        round: 0,
        rounds: 3,
        red_at_merge: Vec::new(),
    });
    run.event("implement", "candidate A started");
    run.event("implement", "candidate B started");
    run.event("judge", "ranking in");
    run.advice = Some(magi::advise::Advice {
        records: (1..=3)
            .map(|n| {
                magi::advise::AdvisorRecord::failed(n, format!("agent-{n}"), "no proposal".into())
            })
            .collect(),
        synthesis: Some("A blended brief.".to_owned()),
    });
    run.judgements = vec![magi::run::Judgement {
        judge: 1,
        seat: "judge-1".to_owned(),
        agent: "agent-1".to_owned(),
        ranking: vec!['A', 'B'],
        reasons: std::collections::BTreeMap::new(),
        strengths: std::collections::BTreeMap::new(),
        confidence: Some(8),
        order: vec![0, 1],
        failed: None,
        duration_ms: 100,
    }];
    run.tally = Some(magi::run::Tally {
        first_choice: [('A', 1)].into_iter().collect(),
        borda: [('A', 2)].into_iter().collect(),
        winner: 'A',
        unanimous_initial: true,
        deliberated: true,
        changed_votes: 0,
        unanimous_final: true,
        judges: 1,
        present: 1,
        quorum: 1,
        met_quorum: true,
        rankings: 1,
        uncontested: None,
        tie_break: None,
    });
    run.deliberation = vec![
        magi::run::DeliberationRound {
            round: 1,
            turns: vec![
                magi::run::DeliberationTurn {
                    judge: 1,
                    agent: "agent-1".to_owned(),
                    body: "I strongly back candidate **A** <script id=\"injected-delib\"></script> with clean changes.".to_owned(),
                    tentative: Some('A'),
                },
            ],
        },
    ];
    run.save_under(&home).expect("seed run");

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    let w = Duration::from_secs(30);

    for (width, height, mobile) in [(1280u32, 900u32, false), (390, 844, true)] {
        for theme in ["light", "dark"] {
            let tag = format!("run detail @{width}px {theme}");
            let page = browser
                .open_page(&format!("{base}#/runs/{id}"), width, height, mobile)
                .await
                .unwrap_or_else(|e| panic!("{tag}: open: {e}"));
            // The page may still be mid-navigation, with no root element yet.
            browser
                .wait_for(&page, "!!document.documentElement", w)
                .await
                .unwrap_or_else(|e| panic!("{tag}: document never ready: {e}"));
            browser
                .eval(
                    &page,
                    &format!("document.documentElement.dataset.theme = '{theme}'; true"),
                )
                .await
                .unwrap();
            browser
                .wait_for(&page, "!!document.querySelector('#run-land .land-top')", w)
                .await
                .unwrap_or_else(|e| panic!("{tag}: land never rendered: {e}"));

            // No text node anywhere in the landing panel may be a bare
            // null / undefined.
            let bare = browser
                .eval(
                    &page,
                    "(() => { const out = []; const walk = document.createTreeWalker(\
                     document.getElementById('run-land'), NodeFilter.SHOW_TEXT); \
                     while (walk.nextNode()) { const t = walk.currentNode.textContent.trim(); \
                     if (t === 'null' || t === 'undefined') out.push(t); } return out; })()",
                )
                .await
                .unwrap();
            assert_eq!(bare, serde_json::json!([]), "{tag}: stringified child");

            // The deliberation strip is slim.
            let strip = browser
                .eval(
                    &page,
                    "document.getElementById('advise-strip').getBoundingClientRect().height",
                )
                .await
                .unwrap();
            let strip = strip.as_f64().unwrap_or(f64::MAX);
            assert!(strip > 0.0 && strip < 120.0, "{tag}: strip height {strip}");
            assert_eq!(
                browser
                    .eval(
                        &page,
                        "document.querySelectorAll('#view-run svg.advise, #advise-converge').length"
                    )
                    .await
                    .unwrap(),
                0
            );

            // The Report tab shows the report and hides the overview.
            browser
                .eval(
                    &page,
                    &format!("location.hash = '#/runs/{id}/report'; true"),
                )
                .await
                .unwrap();
            browser
                .wait_for(
                    &page,
                    "(() => { const r = document.querySelector('#run-report-cards .rcard'); \
                     return !!r && r.offsetParent !== null; })()",
                    w,
                )
                .await
                .unwrap_or_else(|e| panic!("{tag}: report tab: {e}"));
            // The structured view is what the tab opens on; the raw text is one
            // toggle away and the cards stay inside the list pane.
            let cards = browser
                .eval(
                    &page,
                    "(() => { const box = document.getElementById('run-report-cards'); \
                     const b = box.getBoundingClientRect(); \
                     const sums = [...box.querySelectorAll('.rcard-sum')]; \
                     return { raw_hidden: document.getElementById('run-report').hidden, \
                              n: sums.length, \
                              titles: sums.every((s) => s.querySelector('.rcard-title').textContent.trim() !== ''), \
                              inside: sums.every((s) => s.getBoundingClientRect().right <= b.right + 1) }; })()",
                )
                .await
                .unwrap();
            assert_eq!(cards["raw_hidden"], true, "{tag}: {cards}");
            assert_eq!(cards["titles"], true, "{tag}: {cards}");
            assert_eq!(cards["inside"], true, "{tag}: card spills out: {cards}");
            assert!(cards["n"].as_u64().unwrap_or(0) >= 1, "{tag}: {cards}");

            // The judging section rendered deliberation turns with judge 1.
            let has_judge_turn = browser
                .eval(
                    &page,
                    "(() => { const titles = [...document.querySelectorAll('#run-report-cards .rcard-title')]; \
                     return titles.some((t) => t.textContent.includes('judge 1')); })()",
                )
                .await
                .unwrap();
            assert_eq!(has_judge_turn, true, "{tag}: judging turn title rendered");

            // Raw HTML in deliberation turn bodies must not be executed as DOM elements.
            let injected_script = browser
                .eval(&page, "document.getElementById('injected-delib') !== null")
                .await
                .unwrap();
            assert_eq!(
                injected_script, false,
                "{tag}: raw HTML script must not be in DOM"
            );

            browser
                .eval(
                    &page,
                    "document.getElementById('report-raw-toggle').click(); true",
                )
                .await
                .unwrap();
            assert_eq!(
                browser
                    .eval(
                        &page,
                        "!document.getElementById('run-report').hidden && document.getElementById('run-report-cards').hidden"
                    )
                    .await
                    .unwrap(),
                true,
                "{tag}: the Raw toggle shows the text report"
            );
            browser
                .eval(
                    &page,
                    "document.getElementById('report-raw-toggle').click(); true",
                )
                .await
                .unwrap();
            let problems = console_problems(&browser.take_events(&page.session));
            assert!(problems.is_empty(), "{tag}: console: {problems:?}");
            assert_eq!(
                browser
                    .eval(
                        &page,
                        "document.getElementById('run-tabpanel-overview').hidden"
                    )
                    .await
                    .unwrap(),
                true,
                "{tag}"
            );

            // A live refresh must leave the tab alone.
            browser
                .eval(&page, "fetch('/api/runs').then(() => true)")
                .await
                .unwrap();
            browser.settle(Duration::from_millis(400)).await.unwrap();
            let after = browser
                .eval(
                    &page,
                    "({ hash: location.hash, report: !document.getElementById('run-tabpanel-report').hidden, \
                        sel: document.getElementById('run-tab-report').getAttribute('aria-selected') })",
                )
                .await
                .unwrap();
            assert_eq!(after["hash"], format!("#/runs/{id}/report"), "{tag}");
            assert_eq!(after["report"], true, "{tag}");
            assert_eq!(after["sel"], "true", "{tag}");

            // Timeline: grouped by node.
            browser
                .eval(
                    &page,
                    &format!("location.hash = '#/runs/{id}/timeline'; true"),
                )
                .await
                .unwrap();
            browser
                .wait_for(
                    &page,
                    "document.querySelectorAll('#run-events .tl-group').length === 2",
                    w,
                )
                .await
                .unwrap_or_else(|e| panic!("{tag}: timeline groups: {e}"));
            browser.close_page(&page).await;
        }
    }

    // The list pane: no nested scroller inside it.
    let page = browser
        .open_page(&format!("{base}#/runs"), 1280, 900, false)
        .await
        .expect("open");
    // A merged run is hidden by the default "Active" segment.
    browser
        .wait_for(
            &page,
            "!!document.querySelector('.state-chip[data-key=all]')",
            w,
        )
        .await
        .expect("segmented control");
    browser
        .eval(
            &page,
            "document.querySelector('.state-chip[data-key=all]').click(); true",
        )
        .await
        .unwrap();
    browser
        .wait_for(
            &page,
            "!!document.querySelector('#view-runs a.card.run-card')",
            w,
        )
        .await
        .expect("rows");
    let nested = browser
        .eval(
            &page,
            "(() => [...document.querySelectorAll('#view-runs *')].filter((e) => { \
             const o = getComputedStyle(e).overflowY; \
             return (o === 'auto' || o === 'scroll') && e.scrollHeight > e.clientHeight + 1; \
             }).map((e) => e.className || e.tagName))()",
        )
        .await
        .unwrap();
    assert_eq!(
        nested,
        serde_json::json!([]),
        "nested scroller in the list pane"
    );
    browser.close_page(&page).await;
}

fn seed_candidate(
    label: char,
    agent: &str,
    failed: Option<&str>,
    repo: &std::path::Path,
) -> magi::run::Candidate {
    magi::run::Candidate {
        index: (label as usize) - ('A' as usize),
        label,
        agent: agent.to_owned(),
        branch: format!("magi/seed/{label}"),
        worktree: repo.to_path_buf(),
        summary: String::new(),
        stat: String::new(),
        files: 1,
        commits: 1,
        empty: false,
        failed: failed.map(str::to_owned),
        verified_noop: None,
        duration_ms: 0,
        folded: false,
    }
}

/// A run with one candidate shows the compact solo strip in the Verdict panel
/// and no diagram; a run with two candidates keeps the diagram even when one
/// of them failed (the rule is the candidate count, not the viable count).
#[tokio::test]
async fn verdict_panel_is_a_solo_strip_only_for_one_candidate() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));

    let seed = |id: &str, cands: Vec<magi::run::Candidate>| {
        let mut run = RunState::new(
            fx.repo.clone(),
            "main".to_owned(),
            "0000000".to_owned(),
            "verdict panel shape".to_owned(),
            fx.config.clone(),
        );
        run.id = id.to_owned();
        run.status = RunStatus::Merged;
        run.candidates = cands;
        run.save_under(&home).expect("seed run");
    };
    let solo_id = "20260901-000010-cc10";
    let pair_id = "20260901-000011-cc11";
    seed(solo_id, vec![seed_candidate('A', "alpha", None, &fx.repo)]);
    seed(
        pair_id,
        vec![
            seed_candidate('A', "alpha", None, &fx.repo),
            seed_candidate('B', "beta", Some("quota"), &fx.repo),
        ],
    );

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    let w = Duration::from_secs(30);

    for (width, height, mobile) in [(1280u32, 900u32, false), (390, 844, true)] {
        let tag = format!("verdict solo @{width}px");
        let page = browser
            .open_page(&format!("{base}#/runs/{solo_id}"), width, height, mobile)
            .await
            .unwrap_or_else(|e| panic!("{tag}: open: {e}"));
        browser
            .wait_for(
                &page,
                "!!document.querySelector('#verdict-solo .advise-brief')",
                w,
            )
            .await
            .unwrap_or_else(|e| panic!("{tag}: solo strip never rendered: {e}"));
        let got = browser
            .eval(
                &page,
                "(() => { const s = document.getElementById('verdict-solo'); \
                 return { svgs: document.querySelectorAll('#converge svg').length, \
                 shown: !s.hidden, \
                 pill: s.querySelector('.advise-brief').textContent, \
                 chip: s.querySelector('.advise-chip').textContent, \
                 agent: getComputedStyle(s.querySelector('.advise-agent')).display, \
                 height: s.getBoundingClientRect().height, \
                 facts: document.getElementById('tally-facts').hidden }; })()",
            )
            .await
            .unwrap();
        assert_eq!(got["svgs"], 0, "{tag}: diagram drawn for a solo run");
        assert_eq!(got["shown"], true, "{tag}: strip hidden");
        assert_eq!(got["pill"], "solo", "{tag}: pill");
        let chip = got["chip"].as_str().unwrap_or_default();
        assert!(
            chip.contains('A') && chip.contains("alpha"),
            "{tag}: chip {chip}"
        );
        assert_ne!(got["agent"], "none", "{tag}: agent name hidden");
        assert!(
            got["height"].as_f64().unwrap_or(999.0) < 120.0,
            "{tag}: strip tall"
        );
        assert_eq!(got["facts"], true, "{tag}: facts shown");
        browser.close_page(&page).await;

        let tag = format!("verdict pair @{width}px");
        let page = browser
            .open_page(&format!("{base}#/runs/{pair_id}"), width, height, mobile)
            .await
            .unwrap_or_else(|e| panic!("{tag}: open: {e}"));
        browser
            .wait_for(
                &page,
                "document.querySelectorAll('#converge svg').length > 0",
                w,
            )
            .await
            .unwrap_or_else(|e| panic!("{tag}: diagram never rendered: {e}"));
        let hidden = browser
            .eval(&page, "document.getElementById('verdict-solo').hidden")
            .await
            .unwrap();
        assert_eq!(hidden, true, "{tag}: solo strip shown");
        browser.close_page(&page).await;
    }
}

/// In the two-pane layout a Queue row is one big target: clicking a part of it
/// that is not a link or a button opens the task in the right pane. The click
/// is a real mouse event at the row's status chip, so an ancestor that wrongly
/// swallows it (the section's `<details>`) shows up here.
#[tokio::test]
async fn queue_row_click_previews_task_in_split_pane() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));

    let mut run = RunState::new(
        fx.repo.clone(),
        "main".to_owned(),
        "0000000".to_owned(),
        "A run".to_owned(),
        fx.config.clone(),
    );
    run.id = "20260901-000001-aa01".to_owned();
    run.status = RunStatus::Reviewing;
    run.save_under(&home).expect("seed run");
    let ids = seed_tasks(&queue, &fx.repo, &run.id);

    let base = serve_with_client_probe(
        &home,
        queue,
        talks,
        home.join("runs"),
        &fx.repo,
        ClientProbe::Queue,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    let w = Duration::from_secs(30);
    let mut failures: Vec<String> = Vec::new();

    for width in [1280u32, 1950] {
        let tag = format!("queue click @{width}px");
        let page = browser
            .open_page(&format!("{base}#/queue"), width, 900, false)
            .await
            .expect("open");
        let all = ids
            .iter()
            .map(|id| format!("!!document.querySelector('a[href=\"#/queue/{id}\"]')"))
            .collect::<Vec<_>>()
            .join(" && ");
        browser.wait_for(&page, &all, w).await.expect("rows");
        browser
            .eval(&page, "window.queueReady")
            .await
            .expect("live client boot completed");
        let chips = browser
            .eval(
                &page,
                r#"(() => {
                  const failures = [];
                  for (const name of ['RUN_STATUS', 'TASK_STATUS', 'QUESTION_STATUS', 'TALK_STATUS']) {
                    const slot = document.createElement('span');
                    let original;
                    for (const [status, meta] of Object.entries(deck[name])) {
                      deck.setChip(slot, status, deck[name]);
                      original ||= slot.firstChild;
                      const node = slot.firstChild;
                      if (node !== original || node.textContent !== status
                        || node.dataset.status !== status || node.dataset.glyph !== meta.glyph
                        || node.getAttribute('data-flight') !== (meta.flight ? '1' : null)) failures.push(`${name}: ${status}`);
                    }
                  }
                  return failures;
                })()"#,
            )
            .await
            .expect("chip transitions");
        assert_eq!(
            chips,
            serde_json::json!([]),
            "{tag}: stale chip attributes: {chips}"
        );
        browser
            .eval(
                &page,
                "window.queueInput = []; for (const kind of ['mousedown', 'mouseup', 'click']) \
                 document.addEventListener(kind, e => queueInput.push({ kind, \
                 id: e.target.closest('[data-task-id]')?.dataset.taskId, \
                 target: e.target.className, x: e.clientX, y: e.clientY }), true);",
            )
            .await
            .expect("capture queue input");
        browser
            .eval(
                &page,
                "document.querySelectorAll('details.list-section').forEach((d) => { d.open = true; })",
            )
            .await
            .expect("expand");
        browser
            .wait_for(&page, "!!document.querySelector('main[data-split]')", w)
            .await
            .unwrap_or_else(|e| panic!("{tag}: not in the two-pane layout: {e}"));
        browser.settle(Duration::from_millis(300)).await.unwrap();

        // The divider sits on the list/preview seam, is a labelled separator,
        // and resizes by keyboard; a double click restores the default.
        let divider = browser
            .eval(
                &page,
                "(() => { const d = document.getElementById('split-divider'); \
                 const m = document.getElementById('view-queue'); \
                 const r = d.getBoundingClientRect(); const w0 = m.getBoundingClientRect().width; \
                 const key = (k) => d.dispatchEvent(new KeyboardEvent('keydown', { key: k, bubbles: true, cancelable: true })); \
                 key('ArrowRight'); const w1 = m.getBoundingClientRect().width; \
                 d.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); \
                 const w2 = m.getBoundingClientRect().width; \
                 return { visible: r.width > 0 && r.height > 0, role: d.getAttribute('role'), \
                   orient: d.getAttribute('aria-orientation'), tab: d.tabIndex, \
                   gap: Math.abs(r.left - m.getBoundingClientRect().right), w0, w1, w2 }; })()",
            )
            .await
            .unwrap_or_else(|e| panic!("{tag}: divider: {e}"));
        let num = |k: &str| divider[k].as_f64().unwrap_or(f64::NAN);
        if divider["visible"] != true
            || divider["role"] != "separator"
            || divider["orient"] != "vertical"
            || num("tab") != 0.0
            || num("gap") > 2.0
            || (num("w1") - num("w0") - 16.0).abs() > 1.0
            || (num("w2") - num("w0")).abs() > 1.0
        {
            failures.push(format!("{tag}: split divider misbehaves: {divider}"));
        }

        for id in &ids {
            // Bring the row into view, then aim at its status chip: part of
            // the row, but not a link, a button or the title.
            let point = browser
                .eval(
                    &page,
                    &format!(
                        "(async () => {{ const row = document.querySelector('li.card[data-task-id=\"{id}\"]'); \
                         if (!row) return null; row.scrollIntoView({{ block: 'center', behavior: 'instant' }}); \
                         let previous = null; \
                         for (let n = 0; n < 120; n++) {{ \
                           await new Promise(resolve => requestAnimationFrame(resolve)); \
                           const c = row.querySelector('.card-top .chip'); \
                           if (!c) return null; const r = c.getBoundingClientRect(); \
                           const x = r.left + r.width / 2, y = r.top + r.height / 2; \
                           const t = document.elementFromPoint(x, y); \
                           const point = {{ x, y, inRow: !!t && c.contains(t), \
                             hit: t?.closest('[data-task-id]')?.dataset.taskId, \
                             scroll: document.getElementById('view-queue').scrollTop }}; \
                           if (previous && previous.x === x && previous.y === y \
                             && previous.scroll === point.scroll && point.inRow && point.hit === '{id}') {{ \
                             window.queueInput = []; window.pressedChip = c; return {{ ...point, stable: true }}; \
                           }} previous = point; \
                         }} return {{ ...previous, stable: false }}; }})()"
                    ),
                )
                .await
                .unwrap_or_else(|e| panic!("{tag}: locate {id}: {e}"));
            if point.is_null() || point["inRow"] != true || point["stable"] != true {
                failures.push(format!(
                    "{tag}: {id}: no clickable chip inside the row: {point}"
                ));
                continue;
            }
            for kind in ["mousePressed", "mouseReleased"] {
                browser
                    .call(
                        Some(&page.session),
                        "Input.dispatchMouseEvent",
                        serde_json::json!({
                            "type": kind, "x": point["x"], "y": point["y"],
                            "button": "left", "clickCount": 1,
                        }),
                    )
                    .await
                    .expect("mouse");
                if kind == "mousePressed" {
                    // Exercise the ordinary refresh while the button is held,
                    // rather than hoping a stream update hits this short gap.
                    let refresh = browser
                        .eval(
                            &page,
                            "(async () => { await deck.loadQueue(); \
                             const r = pressedChip.getBoundingClientRect(); \
                             return { connected: pressedChip.isConnected, \
                               x: r.left + r.width / 2, y: r.top + r.height / 2, \
                               input: queueInput }; })()",
                        )
                        .await
                        .expect("refresh during press");
                    assert_eq!(
                        refresh["connected"], true,
                        "{tag}: {id}: pressed chip replaced: {refresh}"
                    );
                    assert_eq!(
                        refresh["x"], point["x"],
                        "{tag}: {id}: chip moved: {refresh}"
                    );
                    assert_eq!(
                        refresh["y"], point["y"],
                        "{tag}: {id}: chip moved: {refresh}"
                    );
                }
            }
            let input = browser
                .eval(&page, "queueInput")
                .await
                .expect("input trace");
            for kind in ["mousedown", "mouseup", "click"] {
                assert!(
                    input
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|e| e["kind"] == kind && e["id"] == *id),
                    "{tag}: {id}: missing {kind} on the clicked row: {input}"
                );
            }
            let want = format!("#/tasks/{id}");
            // Identity, not the heading, proves this detail belongs to the
            // clicked row (titles need not be unique in a real queue).
            let shown = format!(
                "location.hash === {want:?} \
                 && !document.getElementById('view-task').hidden \
                 && document.getElementById('split-empty').hidden \
                 && !document.getElementById('task-h').textContent.includes('Loading task') \
                 && !document.getElementById('task-h').textContent.includes('could not be loaded') \
                 && document.getElementById('task-h').textContent.trim() !== '' \
                 && document.getElementById('view-task').dataset.taskId === {id:?}"
            );
            if browser
                .wait_for(&page, &shown, Duration::from_secs(5))
                .await
                .is_err()
            {
                let seen = browser
                    .eval(
                        &page,
                        "({ hash: location.hash, task: document.getElementById('view-task').hidden, \
                           id: document.getElementById('view-task').dataset.taskId, input: queueInput, \
                           empty: document.getElementById('split-empty').hidden, \
                           h: document.getElementById('task-h').textContent })",
                    )
                    .await
                    .unwrap_or_default();
                failures.push(format!(
                    "{tag}: {id}: clicking the row did not open it: {seen}"
                ));
                continue;
            }
        }
        browser.close_page(&page).await;
    }
    assert!(
        failures.is_empty(),
        "queue row clicks:\n  {}",
        failures.join("\n  ")
    );
}

/// The Settings link is a `.rail-link` that only exists from 720px up, and the
/// dock has no sixth item, so a phone reaches Settings through a gear in the
/// header. It must be a real tap target that opens the view, must not crowd
/// the brand or the bell, and must never appear next to the rail's link.
#[tokio::test]
async fn settings_gear_is_reachable_on_a_phone_and_never_doubles_the_rail_link() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    let w = Duration::from_secs(30);

    // Phone: the gear is visible, big enough, clear of its neighbours.
    let page = browser
        .open_page(&format!("{base}#/runs"), 390, 844, true)
        .await
        .expect("open");
    browser
        .wait_for(&page, "!!document.getElementById('settings-btn')", w)
        .await
        .expect("gear in the DOM");
    browser
        .wait_for(
            &page,
            "document.getElementById('runs-count').textContent !== 'Loading…'",
            w,
        )
        .await
        .expect("client ready");
    // Glow assertions require motion regardless of the runner's OS settings.
    // The real press below separately exercises reduced motion, then restores
    // the OS preference after both modes have been checked.
    browser
        .call(
            Some(&page.session),
            "Emulation.setEmulatedMedia",
            serde_json::json!({"features": [{"name": "prefers-reduced-motion", "value": "no-preference"}]}),
        )
        .await
        .expect("enable motion for glow assertions");
    let feedback = browser.eval(&page, r##"(() => {
      const fails = [];
      const host = document.createElement('div');
      host.style.cssText = 'position:fixed;left:20px;top:200px;z-index:999';
      host.innerHTML = '<button class="btn">Dynamic <span>button</span></button><fieldset disabled><button>Disabled</button></fieldset><div inert><button>Inert</button></div><div aria-disabled="true"><button>Unavailable</button></div><div data-press><input></div><button disabled>Disabled</button>';
      document.body.append(host);
      const button = host.firstElementChild;
      const down = (node) => {
        const r = node.getBoundingClientRect();
        node.dispatchEvent(new PointerEvent('pointerdown', {bubbles:true, isPrimary:true, button:0, clientX:r.x+r.width/2, clientY:r.y+r.height/2}));
      };
      const clear = () => document.dispatchEvent(new PointerEvent('pointercancel'));
      down(button.firstElementChild);
      if (!button.hasAttribute('data-pressed')) fails.push('dynamic nested target did not press');
      if (document.querySelectorAll('.press-glow').length !== 1) fails.push('missing glow');
      if (getComputedStyle(document.querySelector('#press-feedback')).pointerEvents !== 'none') fails.push('overlay intercepts input');
      down(button);
      if (document.querySelectorAll('.press-glow').length !== 1) fails.push('glows accumulate');
      clear();
      if (button.hasAttribute('data-pressed') || document.querySelector('.press-glow')) fails.push('cancel did not clear');
      for (const node of host.querySelectorAll('fieldset button, [inert] button, [aria-disabled] button, input, button:disabled')) {
        down(node);
        if (host.querySelector('[data-pressed]') || document.querySelector('.press-glow')) fails.push('unavailable or input target reacted');
        clear();
      }
      down(button);
      document.dispatchEvent(new Event('scroll'));
      if (button.hasAttribute('data-pressed') || document.querySelector('.press-glow')) fails.push('scroll did not clear');
      button.focus();
      if (getComputedStyle(button).outlineStyle === 'none') fails.push('missing focus outline');
      host.setAttribute('data-press', '');
      host.setAttribute('data-pressed', '');
      button.setAttribute('data-pressed', '');
      if (getComputedStyle(host).opacity !== '1') fails.push('nested control also pressed its parent');
      if (getComputedStyle(button).opacity !== '0.78') fails.push('nested control lost feedback');
      host.remove();
      return fails;
    })()"##).await.expect("press feedback");
    assert_eq!(
        feedback,
        serde_json::json!([]),
        "press feedback regressions"
    );

    let pending = browser.eval(&page, r##"(async () => {
      const source = await (await fetch('/app.js')).text();
      let settle;
      const client = new Function('fetch', source.replace(/\nboot\(\);\s*$/, `
        return { request, press(target, age = 0) { pressedControl = target; pressedAt = performance.now() - age; } };
      `))(() => new Promise((resolve, reject) => { settle = { resolve, reject }; }));
      const button = document.createElement('button');
      document.body.append(button);
      const fails = [];
      for (const [method, age, expected] of [['GET', 0, false], ['POST', 601, false], ['POST', 0, true], ['PATCH', 0, true]]) {
        client.press(button, age);
        const promise = client.request('/test', { method });
        if ((button.getAttribute('aria-busy') === 'true') !== expected) fails.push(`${method}/${age}: pending state`);
        settle.resolve(new Response('{}'));
        await promise;
        if (button.hasAttribute('aria-busy')) fails.push(`${method}: pending not cleared`);
      }
      for (const outcome of ['http', 'network']) {
        button.setAttribute('aria-busy', 'false');
        client.press(button);
        const promise = client.request('/test', { method: 'POST' });
        if (outcome === 'network') settle.reject(new Error('offline'));
        else settle.resolve(new Response('{}', { status: 500 }));
        try { await promise; fails.push('failure swallowed'); } catch {}
        if (button.getAttribute('aria-busy') !== 'false') fails.push(`${outcome}: previous state lost`);
      }
      button.setAttribute('aria-busy', 'true');
      client.press(button);
      const promise = client.request('/test', { method: 'POST' });
      settle.resolve(new Response('{}'));
      await promise;
      if (button.getAttribute('aria-busy') !== 'true') fails.push('explicit pending scope cleared');
      button.remove();
      return fails;
    })()"##).await.expect("generic pending feedback");
    assert_eq!(
        pending,
        serde_json::json!([]),
        "pending feedback regressions"
    );

    let geometry = browser
        .eval(
            &page,
            r##"(() => {
              const fails = [];
              const shown = (e) => { const r = e.getBoundingClientRect(), cs = getComputedStyle(e);
                return r.width > 0 && r.height > 0 && cs.display !== "none" && cs.visibility !== "hidden"; };
              const hit = (a, b) => a.left < b.right - 1 && b.left < a.right - 1 && a.top < b.bottom - 1 && b.top < a.bottom - 1;
              const gear = document.getElementById("settings-btn");
              if (!shown(gear)) return ["gear is not visible at 390px"];
              const g = gear.getBoundingClientRect();
              if (g.width < 44 || g.height < 44) fails.push(`gear is ${g.width}x${g.height}, under 44px`);
              if (g.right > innerWidth + 1) fails.push("gear spills past the viewport");
              for (const sel of [".brand", "#bell", "#theme-toggle"]) {
                const e = document.querySelector(sel);
                if (e && shown(e) && hit(g, e.getBoundingClientRect())) fails.push(`gear overlaps ${sel}`);
              }
              const visible = [...document.querySelectorAll('[data-nav="settings"]')].filter(shown).length;
              if (visible !== 1) fails.push(`${visible} visible Settings entries, want 1`);
              return fails;
            })()"##,
        )
        .await
        .expect("measure gear");
    assert_eq!(geometry, serde_json::json!([]), "phone header: {geometry}");

    // Motion-free feedback must survive an active decorative SVG descendant.
    browser
        .call(
            Some(&page.session),
            "Emulation.setEmulatedMedia",
            serde_json::json!({"features": [{"name": "prefers-reduced-motion", "value": "reduce"}]}),
        )
        .await
        .expect("reduce motion");
    // Tap it for real, at its centre.
    let point = browser
        .eval(
            &page,
            "(() => { const r = document.getElementById('settings-btn').getBoundingClientRect(); \
             return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; })()",
        )
        .await
        .expect("locate gear");
    for kind in ["mousePressed", "mouseReleased"] {
        browser
            .call(
                Some(&page.session),
                "Input.dispatchMouseEvent",
                serde_json::json!({
                    "type": kind, "x": point["x"], "y": point["y"],
                    "button": "left", "clickCount": 1,
                }),
            )
            .await
            .expect("mouse");
        if kind == "mousePressed" {
            let pressed = browser
                .eval(
                    &page,
                    r##"(() => {
                      const gear = document.getElementById('settings-btn');
                      return {
                        childActive: !!gear.querySelector('svg:active'),
                        opacity: getComputedStyle(gear).opacity,
                        glow: !!document.querySelector('.press-glow'),
                      };
                    })()"##,
                )
                .await
                .expect("measure reduced-motion press");
            assert_eq!(
                pressed["childActive"], true,
                "SVG must be active: {pressed}"
            );
            assert_eq!(pressed["opacity"], "0.78", "link must react: {pressed}");
            assert_eq!(
                pressed["glow"], false,
                "motion must be suppressed: {pressed}"
            );
        }
    }
    browser
        .call(
            Some(&page.session),
            "Emulation.setEmulatedMedia",
            serde_json::json!({"features": []}),
        )
        .await
        .expect("restore motion preference");
    browser
        .wait_for(
            &page,
            "location.hash === '#/settings' \
             && !document.getElementById('view-settings').hidden \
             && document.getElementById('settings-btn').getAttribute('aria-current') === 'page'",
            w,
        )
        .await
        .unwrap_or_else(|e| panic!("tapping the gear did not open Settings: {e}"));
    let after = browser
        .eval(
            &page,
            r##"(() => {
              window.scrollTo(0, 0);
              const h = document.getElementById("settings-h");
              if (!h) return "no settings heading";
              const r = h.getBoundingClientRect();
              const top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
              if (!top || !h.contains(top)) return "heading covered by " + (top ? (top.id || top.className || top.tagName) : "nothing");
              const others = [...document.querySelectorAll("[data-nav][aria-current]")]
                .filter((e) => e.dataset.nav !== "settings").map((e) => e.dataset.nav);
              return others.length ? "also current: " + others.join(",") : "ok";
            })()"##,
        )
        .await
        .expect("after");
    assert_eq!(after, "ok", "after tapping the gear");

    // Leaving Settings drops the highlight.
    browser
        .eval(&page, "location.hash = '#/runs'")
        .await
        .expect("navigate away");
    browser
        .wait_for(
            &page,
            "!document.getElementById('settings-btn').hasAttribute('aria-current')",
            w,
        )
        .await
        .expect("gear no longer current");
    browser.close_page(&page).await;

    // Desktop: the rail owns Settings and the gear stays out of the way.
    let page = browser
        .open_page(&format!("{base}#/runs"), 1280, 900, false)
        .await
        .expect("open desktop");
    browser
        .wait_for(&page, "!!document.getElementById('settings-btn')", w)
        .await
        .expect("gear in the DOM");
    let visible = browser
        .eval(
            &page,
            r##"[...document.querySelectorAll('[data-nav="settings"]')]
                 .filter((e) => { const r = e.getBoundingClientRect();
                   return r.width > 0 && r.height > 0 && getComputedStyle(e).display !== "none"; })
                 .map((e) => e.id || e.className)"##,
        )
        .await
        .expect("count");
    assert_eq!(
        visible,
        serde_json::json!(["rail-link"]),
        "desktop Settings entries"
    );
    browser.close_page(&page).await;
}

/// Exercise the actual client functions without boot's timers. The test-only
/// harness is appended to the embedded source; shipped assets need no hooks.
async fn client_harness(browser: &mut cdp::Browser, page: &cdp::Page) {
    // `open_page` returns right after `Page.navigate`; app.js looks up
    // elements at top level, so it must not run before the document is parsed.
    browser
        .wait_for(
            page,
            "document.readyState !== 'loading' && !!document.getElementById('notifications-read-all')",
            Duration::from_secs(30),
        )
        .await
        .expect("document parsed before the client harness");
    browser
        .eval(
            page,
            r#"(async () => {
      const source = await (await fetch('/app.js')).text();
      // Tag the final queue URL, after page paths and cursors are constructed.
      window.deck = new Function(source.replace('fetch(url, init)', 'fetch(url.startsWith("/api/queue") ? url + (url.includes("?") ? "&" : "?") + "test_client=1" : url, init)').replace(/\nboot\(\);\s*$/, `
        return { setChip, RUN_STATUS, TASK_STATUS, QUESTION_STATUS, TALK_STATUS, settingsState, renderSettings, loadSettings, saveSettings, linkify, refreshReferences, truncateLabel, state, loadHealth, loadLoop, loadTalk, tickTalkWaits, loadStats, resumeConnection, onPageShow, unreachable, loadQueue, loadRuns, loadTalks, applyRevisions_, storeReads, statsAgentTone, statsBarPlan, statsBarRows, statsScatterPlan, renderStatsReviewerScatter, reviewPassed, voteTag, reviewStamp };
      `))();
      await Promise.all([deck.loadQueue(), deck.loadRuns(), deck.loadTalks()]);
    })()"#,
        )
        .await
        .expect("install client harness");
}

#[tokio::test]
async fn machine_settings_context_and_overrides_fit_phone_and_desktop() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP settings context: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let base = serve_with_client_probe(
        &home,
        Queue::at(home.join("queue")),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Settings,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    for width in [390, 1280] {
        let page = browser
            .open_page(&format!("{base}#/settings"), width, 900, width == 390)
            .await
            .expect("page");
        // Inspect the live client's state, so there is only one settings
        // renderer and one request sequence throughout these interactions.
        browser
            .wait_for(
                &page,
                "!!document.querySelector('#settings-body .set-role') && document.getElementById('settings-repo')?.disabled === false",
                Duration::from_secs(30),
            )
            .await
            .expect("live client settings loaded before checking geometry and states");
        let result = browser.eval(&page, r#"(async () => {
          const s = deck.settingsState;
          const data = structuredClone(s.data);
          if (data.error || !data.roles.length) return ['fixture config did not load'];
          data.repositories = [
            {name: 'owner/same-name', path: '/checkouts/one/' + 'long-repository-name-'.repeat(8)},
            {name: 'owner/same-name', path: '/checkouts/two/same-name'}
          ];
          data.repo = data.repositories[0].path;
          const role = data.roles.find(r => r.count);
          role.source = 'repo'; role.editable = false; role.machine_value = ['machine-agent'];
          role.count.source = 'repo'; role.count.editable = false; role.count.machine_value = 1;
          s.data = data; s.repo = data.repo; s.repositories = data.repositories;
          deck.renderSettings();
          const fails = [];
          const picker = document.getElementById('settings-repo');
          const r = picker.getBoundingClientRect();
          if (r.width <= 0 || r.right > innerWidth + 1) fails.push('selector spills or is hidden');
          if (document.documentElement.scrollWidth > innerWidth + 1) fails.push('horizontal overflow');
          if (document.getElementById('settings-h').textContent !== 'Machine settings') fails.push('heading');
          if (!document.getElementById('settings-sub').textContent.startsWith('As seen from')) fails.push('context');
          if (picker.options.length !== 2 || !picker.options[0].textContent.includes(data.repo)) fails.push('checkout paths');
          const overrides = [...document.querySelectorAll('.set-override')];
          if (overrides.length !== 2 || overrides.some(e => !e.textContent.includes('Repository override') || !e.textContent.includes('shadowed'))) fails.push('override badges');
          if (!overrides[0].textContent.includes('machine-agent') || !overrides[1].textContent.includes('Machine value: 1')) fails.push('shadowed values');
          if (document.getElementById('set-count-' + role.key).disabled !== true) fails.push('override count editable');
          s.loading = true; deck.renderSettings();
          if (!document.getElementById('settings-repo').disabled) fails.push('loading selector enabled');
          s.loading = false; s.busy = true; deck.renderSettings();
          if (!document.getElementById('settings-repo').disabled) fails.push('saving selector enabled');
          s.busy = false; s.data = {...data, error: {message: 'broken selected config'}}; deck.renderSettings();
          if (document.getElementById('settings-repo').disabled || document.getElementById('settings-repo').options.length !== 2) fails.push('cannot recover from config error');
          s.data = null; s.message = {bad: true, text: 'request failed'}; deck.renderSettings();
          if (document.getElementById('settings-repo').disabled || document.getElementById('settings-repo').options.length !== 2) fails.push('cannot recover from request error');
          s.data = data; s.repo = data.repo; s.draft = {judges: ['a']}; deck.renderSettings();
          const confirm = window.confirm, fetch = window.fetch;
          const requests = [];
          try {
            window.confirm = () => false;
            let picker = document.getElementById('settings-repo');
            picker.value = data.repositories[1].path;
            picker.dispatchEvent(new Event('change'));
            if (s.repo !== data.repo || !s.draft.judges || picker.value !== data.repo) fails.push('discard cancellation lost edits');
            window.confirm = () => true;
            window.fetch = async (url, init) => {
              if (!String(url).startsWith('/api/settings')) return fetch(url, init);
              requests.push(String(url));
              return new Response(JSON.stringify({...data, repo: data.repositories[1].path}), {status: 200});
            };
            picker.value = data.repositories[1].path;
            picker.dispatchEvent(new Event('change'));
            while (s.loading) await new Promise(resolve => setTimeout(resolve, 10));
            if (Object.keys(s.draft).length || s.data.repo !== data.repositories[1].path) fails.push('context switch retained edits');
            if (!requests[0]?.includes('repo=' + encodeURIComponent(data.repositories[1].path))) fails.push('GET omitted selected context');
            s.draft = {judges: ['a']};
            await deck.saveSettings();
            if (!requests[1]?.includes('/api/settings/roles?repo=' + encodeURIComponent(data.repositories[1].path))) fails.push('PUT omitted selected context');
            const pending = [];
            window.fetch = (url, init) => String(url).startsWith('/api/settings')
              ? new Promise(resolve => pending.push(resolve)) : fetch(url, init);
            const first = deck.loadSettings(), second = deck.loadSettings();
            pending[1](new Response(JSON.stringify({...data, repo: 'newest'}), {status: 200}));
            await second;
            pending[0](new Response(JSON.stringify({...data, repo: 'stale'}), {status: 200}));
            await first;
            if (s.data.repo !== 'newest') fails.push('stale response replaced selection');
          } finally { window.confirm = confirm; window.fetch = fetch; }
          return fails;
        })()"#).await.expect("settings geometry and states");
        assert_eq!(
            result,
            serde_json::json!([]),
            "settings at {width}px: {result}"
        );
        browser.close_page(&page).await;
    }
}

#[tokio::test]
async fn delta_client_merges_rows_coalesces_and_recovers() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP delta client: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let ids = seed_tasks(&queue, &fx.repo, "run");
    let base = serve(
        &home,
        queue.clone(),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser
        .open_page(&format!("{base}#/queue"), 1280, 900, false)
        .await
        .expect("page");
    browser
        .wait_for(
            &page,
            "!!document.querySelector('#queue-sections li.card')",
            Duration::from_secs(30),
        )
        .await
        .expect("loaded queue");
    client_harness(&mut browser, &page).await;
    let id = &ids[0];
    let unchanged = &ids[1];
    browser
        .eval(
            &page,
            &format!(
                r#"window.sameRow = deck.state.queue.find(t => t.id === {unchanged:?});
      deck.state.rev.queue = 1; performance.clearResourceTimings();"#
            ),
        )
        .await
        .unwrap();
    let mut task = queue.get(id).expect("task");
    task.title = "Updated through a delta".into();
    queue.put(&mut task).unwrap();
    let check = browser.eval(&page, &format!(r#"(async () => {{
      await deck.loadQueue({{rev: 2, delta: {{base: 1, changed: [{id:?}], removed: []}}}});
      return {{same: sameRow === deck.state.queue.find(t => t.id === {unchanged:?}),
        title: deck.state.queue.find(t => t.id === {id:?}).title,
        partial: performance.getEntriesByType('resource').some(r => r.name.includes('/api/queue?ids=') && r.name.includes('test_client=1'))}};
    }})()"#)).await.unwrap();
    assert_eq!(check["same"], true);
    assert_eq!(check["partial"], true);
    assert_eq!(check["title"], "Updated through a delta");

    let result = browser.eval(&page, r#"(async () => {
      const native = window.fetch;
      const requests = [];
      let release;
      window.fetch = async (url, options) => {
        if (String(url).startsWith('/api/queue') && String(url).includes('test_client=1')) {
          requests.push(String(url));
          if (requests.length === 1) await new Promise(resolve => release = resolve);
        }
        return native(url, options);
      };
      const first = deck.loadQueue({rev: 3, delta: {base: 2, changed: [], removed: []}});
      for (let rev = 4; rev <= 20; rev++) deck.loadQueue({rev, delta: {base: rev - 1, changed: [], removed: []}});
      const before = requests.length;
      release();
      await first;
      window.fetch = native;
      const coalesced = {before, count: requests.length, chained: requests[1] === '/api/queue?ids=&test_client=1', rev: deck.state.rev.queue};
      window.fetch = async () => { throw new Error('test read failed'); };
      await deck.loadQueue({rev: 21, delta: {base: 20, changed: [], removed: []}});
      const failedRev = deck.state.rev.queue;
      window.fetch = native;
      performance.clearResourceTimings();
      await deck.loadQueue({rev: 22, delta: {base: 21, changed: [], removed: []}});
      return {coalesced, failedRev, recovered: deck.state.rev.queue === 22,
        whole: performance.getEntriesByType('resource').some(r => r.name.endsWith('/api/queue/page?test_client=1'))};
    })()"#).await.unwrap();
    assert_eq!(
        result["coalesced"],
        serde_json::json!({"before": 1, "count": 2, "chained": true, "rev": 20})
    );
    assert!(result["failedRev"].is_null());
    assert_eq!(result["recovered"], true);
    assert_eq!(result["whole"], true);
    std::fs::remove_file(queue.path_of(id)).unwrap();
    let removed = browser
        .eval(
            &page,
            &format!(
                r#"(async () => {{
      await deck.loadQueue({{rev: 23, delta: {{base: 22, changed: [], removed: [{id:?}]}}}});
      return !deck.state.queue.some(t => t.id === {id:?});
    }})()"#
            ),
        )
        .await
        .unwrap();
    assert_eq!(removed, true);
    browser
        .eval(
            &page,
            r#"(async () => {
      const native = window.fetch;
      window.fetch = async () => { throw new Error('final event read failed'); };
      await deck.loadQueue({rev: 24});
      window.fetch = native;
      performance.clearResourceTimings();
    })()"#,
        )
        .await
        .unwrap();
    browser
        .wait_for(
            &page,
            "deck.state.rev.queue === 24",
            Duration::from_secs(10),
        )
        .await
        .expect("failed final event retries without another notification");
    let retry = browser.eval(&page, "performance.getEntriesByType('resource').some(r => r.name.endsWith('/api/queue/page?test_client=1'))").await.unwrap();
    assert_eq!(retry, true);
    browser.close_page(&page).await;
}

#[tokio::test]
async fn unreachable_banner_waits_out_transient_failures() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP unreachable grace: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    // No boot timers or stream: their successful reads could clear the same
    // banner while this test is driving failures with a controlled clock.
    let base = serve_with_client_probe(
        &home,
        queue,
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Connection,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser
        .open_page(&format!("{base}#/queue"), 1280, 900, false)
        .await
        .expect("page");
    browser
        .wait_for(
            &page,
            "!!window.deck && !!document.getElementById('alert')",
            Duration::from_secs(30),
        )
        .await
        .expect("page loaded");
    let r = browser
        .eval(
            &page,
            r#"(async () => {
      const native = window.fetch, nativeNow = Date.now;
      const onLine = Object.getOwnPropertyDescriptor(Navigator.prototype, 'onLine');
      const shown = () => !document.getElementById('alert').hidden;
      const out = {};
      let clock = nativeNow.call(Date);
      Date.now = () => clock;
      try {
        window.fetch = async () => { throw new TypeError('Failed to fetch'); };
        await deck.loadHealth();
        out.first = shown();
        clock += 5000; await deck.loadHealth();
        out.early = shown();
        clock += 20000; await deck.loadHealth();
        out.persisted = shown();
        Object.defineProperty(Navigator.prototype, 'onLine', { get: () => false, configurable: true });
        await deck.loadHealth();
        out.offlineHides = !shown();
        Object.defineProperty(Navigator.prototype, 'onLine', onLine);
        resetStore: {
          const gen = deck.unreachable;
          gen.since = null; gen.failures = 0;
        }
        await deck.loadQueue();
        out.storeQuiet = !shown();
        await deck.loadLoop({ background: true });
        out.loopQuiet = !shown();
        const detail = deck.state.talkDetail;
        const waits = deck.state.talkWaits;
        try {
          window.fetch = native;
          await deck.loadLoop({ background: true });
          deck.state.talkDetail = { id: 'waiting-chat', talk: null };
          deck.state.talkWaits = new Map(['waiting-chat', 'other-conversation'].map(id =>
            [id, { lastPoll: clock - 10000, waitFrom: clock, generation: 1 }]));
          let polls = 0;
          window.fetch = async () => { polls++; throw new TypeError('Failed to fetch'); };
          /* tickTalkWaits does not await its requests. Drain the promise chain
             before inspecting the banner or replacing the fetch stub. */
          const tick = async () => {
            deck.tickTalkWaits();
            await new Promise(resolve => setTimeout(resolve, 0));
          };
          await tick();
          out.talkFirstQuiet = !shown();
          out.talkPolls = polls;
          out.talkFailures = deck.unreachable.failures;
          await deck.loadHealth();
          clock += 10000;
          await deck.loadHealth();
          await tick();
          out.talkEarlyQuiet = !shown();
          out.parallelFailures = deck.unreachable.failures;
          clock += 10000;
          await tick();
          out.talkPersisted = shown();
          out.talkBannerKind = document.getElementById('alert').getAttribute('data-kind');
          window.fetch = native;
          await deck.loadLoop({ background: true });
          out.loopRecovered = !shown();
          out.loopFailures = deck.unreachable.failures;
          out.loopSince = deck.unreachable.since;
          window.fetch = async () => { throw new TypeError('Failed to fetch'); };
          clock += 10000;
          await tick();
          out.talkFreshGrace = !shown() && deck.unreachable.since === clock;
          await deck.loadTalk('waiting-chat');
          out.talkExplicit = shown();
          const actionError = document.getElementById('alert').textContent;
          window.fetch = native;
          await deck.loadLoop({ background: true });
          out.loopPreservesActionError = shown()
            && document.getElementById('alert').textContent === actionError;
          await deck.loadHealth();
          /* Keep body reads in the promise chain drained by tick. A native
             Response body stream can finish after its setTimeout callback.
             Match the text and header interfaces used by the transport. */
          window.fetch = async () => ({ ok: false, status: 500, headers: new Headers(),
            text: async () => JSON.stringify({ error: 'server failed' }) });
          clock += 10000;
          await tick();
          out.talkHttpImmediate = shown()
            && document.getElementById('alert').textContent.includes('server failed');
          window.fetch = async () => ({ ok: false, status: 404, headers: new Headers(),
            text: async () => '{}' });
          clock += 10000;
          await tick();
          out.talkGone = deck.state.talkDetail.gone === true
            && deck.state.talkWaits.size === 0 && !shown();
        } finally {
          deck.state.talkDetail = detail;
          deck.state.talkWaits = waits;
        }
        window.fetch = async () => { throw new TypeError('Failed to fetch'); };
        await deck.loadStats();
        out.statsExplicit = shown();
        window.fetch = native;
        await deck.loadHealth();
        out.cleared = !shown();
        window.fetch = async () => { throw new TypeError('Failed to fetch'); };
        deck.resumeConnection();
        clock += 1000; await deck.loadHealth();
        clock += 10000; await deck.loadHealth();
        window.fetch = native;
        await deck.loadStats();
        window.fetch = async () => { throw new TypeError('Failed to fetch'); };
        clock += 10000; await deck.loadHealth();
        out.successResets = !shown();
        deck.resumeConnection();
        window.fetch = async () => { throw new TypeError('Failed to fetch'); };
        Object.defineProperty(Navigator.prototype, 'onLine', { get: () => false, configurable: true });
        clock += 60000; await deck.loadHealth(); await deck.loadHealth();
        out.offline = shown();
        Object.defineProperty(Navigator.prototype, 'onLine', onLine);
        /* The pre-resume request fails only after the resume's own request has
           failed and the clock has moved past the grace period, so counting it
           would raise the banner; the generation guard must drop it. */
        let failLate;
        window.fetch = (url) => String(url).includes('/api/health')
          ? (window.fetch = async () => { throw new TypeError('Failed to fetch'); },
             new Promise((_, reject) => { failLate = () => reject(new TypeError('Failed to fetch')); }))
          : native(url);
        const stale = deck.loadHealth();
        deck.resumeConnection();
        await new Promise(r => setTimeout(r, 50));
        out.resumeFailures = deck.unreachable.failures;
        clock += 20000;
        failLate();
        await stale;
        await new Promise(r => setTimeout(r, 50));
        out.staleFailures = deck.unreachable.failures;
        out.stale = shown();
        await deck.loadHealth({ explicit: true });
        out.explicit = shown();
        const u = deck.unreachable, pageshow = (persisted) => {
          const before = u.gen;
          deck.onPageShow({ persisted });
          return u.gen - before;
        };
        window.fetch = async () => { throw new TypeError('Failed to fetch'); };
        await deck.loadHealth();
        out.pageshowAfterFailure = pageshow(false);
        u.since = null; u.failures = 0;
        out.pageshowHealthy = pageshow(false);
        out.pageshowPersisted = pageshow(true);
      } finally {
        window.fetch = native; Date.now = nativeNow;
        Object.defineProperty(Navigator.prototype, 'onLine', onLine);
      }
      return out;
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(r["first"], false);
    assert_eq!(r["early"], false);
    assert_eq!(r["persisted"], true);
    assert_eq!(r["offlineHides"], true);
    assert_eq!(r["storeQuiet"], true);
    assert_eq!(r["loopQuiet"], true);
    assert_eq!(r["talkFirstQuiet"], true);
    // The visible chat loads core + sections; the other wait polls core only.
    assert_eq!(r["talkPolls"], 3);
    assert_eq!(r["talkFailures"], 0);
    assert_eq!(r["talkEarlyQuiet"], true);
    assert_eq!(r["parallelFailures"], 2);
    assert_eq!(r["talkPersisted"], true);
    assert_eq!(r["talkBannerKind"], "unreachable");
    assert_eq!(r["loopRecovered"], true);
    assert_eq!(r["loopFailures"], 0);
    assert!(r["loopSince"].is_null());
    assert_eq!(r["talkFreshGrace"], true);
    assert_eq!(r["talkExplicit"], true);
    assert_eq!(r["loopPreservesActionError"], true);
    assert_eq!(r["talkHttpImmediate"], true);
    assert_eq!(r["talkGone"], true);
    assert_eq!(r["statsExplicit"], true);
    assert_eq!(r["successResets"], true);
    assert_eq!(r["cleared"], true);
    assert_eq!(r["offline"], false);
    assert_eq!(r["resumeFailures"], 1);
    assert_eq!(r["staleFailures"], 1);
    assert_eq!(r["stale"], false);
    assert_eq!(r["explicit"], true);
    assert_eq!(r["pageshowAfterFailure"], 1);
    assert_eq!(r["pageshowHealthy"], 0);
    assert_eq!(r["pageshowPersisted"], 1);
    browser.close_page(&page).await;
}

/// Run explicitly against a read-only snapshot of the operator's home. Only
/// JSON records are copied; neither the browser nor server touches that home.
#[tokio::test]
#[ignore = "manual before/after measurement; requires MAGI_WEB_BENCH_HOME"]
async fn delta_list_benchmark() {
    let source = std::path::PathBuf::from(
        std::env::var_os("MAGI_WEB_BENCH_HOME").expect("benchmark source"),
    );
    let chrome = cdp::find_chrome().expect("benchmark requires Chrome");
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    for store in ["queue", "talks", "questions", "runs"] {
        let target = home.join(store);
        std::fs::create_dir_all(&target).unwrap();
        for entry in std::fs::read_dir(source.join(store)).unwrap().flatten() {
            let (from, to) = if store == "runs" {
                let target = target.join(entry.file_name());
                std::fs::create_dir_all(&target).unwrap();
                (entry.path().join("run.json"), target.join("run.json"))
            } else {
                (entry.path(), target.join(entry.file_name()))
            };
            if from.extension().is_some_and(|ext| ext == "json") && from.is_file() {
                std::fs::copy(from, to).unwrap();
            }
        }
    }
    let base = serve(
        &home,
        Queue::at(home.join("queue")),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser
        .open_page(&format!("{base}#/queue"), 1280, 900, false)
        .await
        .expect("page");
    browser
        .wait_for(
            &page,
            "!!document.querySelector('#queue-sections li.card')",
            Duration::from_secs(30),
        )
        .await
        .expect("queue loaded");
    client_harness(&mut browser, &page).await;
    for kind in ["runs", "queue+runs", "talks"] {
        let script = r#"(async () => {
      const source = await (await fetch('/app.js')).text();
      window.bench = new Function(source.replace(/\nboot\(\);\s*$/, `
        let renderMs = 0;
        for (const [name, render] of [['queue', renderQueue], ['runs', renderRuns], ['talks', renderTalks]]) {
          const timed = () => { const start = performance.now(); render(); renderMs += performance.now() - start; };
          if (name === 'queue') renderQueue = timed;
          if (name === 'runs') renderRuns = timed;
          if (name === 'talks') renderTalks = timed;
        }
        return { state, loadQueue, loadRuns, loadTalks, reset: () => renderMs = 0, time: () => renderMs };
      `))();
      await Promise.all([bench.loadQueue(), bench.loadRuns(), bench.loadTalks()]);
      const ids = Object.fromEntries(['queue', 'runs', 'talks'].map(k => [k,
        (k === 'queue' ? bench.state.queue.find(t => t.status_str === 'running') || bench.state.queue[0] : bench.state[k][0]).id]));
      const report = {};
      for (const kind of ['runs', 'queue+runs', 'talks']) {
        location.hash = kind === 'talks' ? '#/chat' : kind === 'runs' ? '#/runs' : '#/queue';
        await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
        report[kind] = {};
        for (const partial of [false, true]) {
          const samples = [];
          for (let n = 0; n < 25; n++) {
            bench.reset(); performance.clearResourceTimings();
            const change = name => ({rev: n + 1, delta: partial ? {base: bench.state.rev[name], changed: [ids[name]], removed: []} : null});
            if (kind === 'runs') await bench.loadRuns(change('runs'));
            if (kind === 'talks') await bench.loadTalks(change('talks'));
            if (kind === 'queue+runs') await Promise.all([bench.loadQueue(change('queue')), bench.loadRuns({rev: n + 1})]);
            // Resource timings are delivered after the response is consumed.
            await new Promise(resolve => setTimeout(resolve, 0));
            const bytes = performance.getEntriesByType('resource').filter(r => /\/api\/(queue|runs|talks)(\?|$)/.test(r.name)).reduce((sum, r) => sum + r.transferSize, 0);
            samples.push({bytes, renderMs: bench.time()});
          }
          const percentile = (key, p) => samples.map(s => s[key]).sort((a,b) => a-b)[Math.ceil(samples.length*p)-1];
          report[kind][partial ? 'after' : 'before'] = {bytesMedian: percentile('bytes', .5), bytesP95: percentile('bytes', .95), renderMedian: percentile('renderMs', .5), renderP95: percentile('renderMs', .95)};
        }
      }
      report.counts = Object.fromEntries(['queue','runs','talks'].map(k => [k, bench.state[k].length]));
      return report;
    })()"#.replace("['runs', 'queue+runs', 'talks']", &format!("['{kind}']"));
        let results = browser
            .eval(&page, &script)
            .await
            .expect("benchmark scenario");
        eprintln!("DELTA_BENCH {results}");
    }
    browser.close_page(&page).await;
}

#[tokio::test]
async fn stats_bars_are_stable_per_agent_and_honest_about_sample_size() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP stats bars: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let base = serve(
        &home,
        queue.clone(),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser
        .open_page(&format!("{base}#/stats"), 390, 800, true)
        .await
        .expect("page");
    client_harness(&mut browser, &page).await;
    let out = browser
        .eval(
            &page,
            r#"(() => {
      const r = (agent, num, den) => ({ agent, fraction: `${num}/${den} won`,
        rate: den ? { pct: 100 * num / den, denominator: den } : null });
      const root = document.createElement('div');
      document.body.append(root);
      deck.statsBarRows(root, [r('a', 0, 2), r('sonnet', 56, 719), r('b', 1, 3), r('z', 0, 0), r('c', 9, 10)]);
      const rows = [...root.querySelectorAll('.bar-row')];
      const other = document.createElement('div');
      deck.statsBarRows(other, [r('sonnet', 1, 1), r('x', 5, 9)]);
      const tone = el => el.querySelector('.bar-fill').style.background;
      const sonnet = rows.find(x => x.textContent.includes('sonnet'));
      return {
        order: rows.map(x => x.querySelector('.bar-row-name').firstChild.textContent),
        low: rows.map(x => x.hasAttribute('data-low-n')),
        tags: rows.map(x => !!x.querySelector('.bar-row-tag')),
        tracks: rows.map(x => !!x.querySelector('.bar-track')),
        n719: sonnet.textContent.includes('n=719'),
        stable: tone(sonnet) === tone(other.querySelector('.bar-row')),
        spill: root.scrollWidth <= root.clientWidth + 1,
        n9: deck.statsBarPlan([r('q', 1, 9)])[0].tier,
        n10: deck.statsBarPlan([r('q', 1, 10)])[0].tier,
      };
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out["order"],
        serde_json::json!(["c", "sonnet", "b", "a", "z"])
    );
    assert_eq!(
        out["low"],
        serde_json::json!([false, false, true, true, false])
    );
    assert_eq!(
        out["tags"],
        serde_json::json!([false, false, true, true, false])
    );
    assert_eq!(
        out["tracks"],
        serde_json::json!([true, true, true, true, false])
    );
    assert_eq!(out["n719"], true);
    assert_eq!(out["stable"], true);
    assert_eq!(out["spill"], true);
    assert_eq!(out["n9"], 1);
    assert_eq!(out["n10"], 0);
    browser.close_page(&page).await;
}

#[tokio::test]
async fn stats_scatter_plots_reviewers_on_a_log_axis() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP stats scatter: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let base = serve(
        &home,
        queue.clone(),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser
        .open_page(&format!("{base}#/stats"), 390, 800, true)
        .await
        .expect("page");
    client_harness(&mut browser, &page).await;
    let out = browser
        .eval(
            &page,
            r#"(() => {
      const r = (agent, adopted, submitted) => ({ agent, adopted, submitted });
      const root = document.getElementById('stats-reviewers-scatter');
      const one = deck.statsScatterPlan([r('a', 1, 1)]);
      deck.renderStatsReviewerScatter([r('a', 1, 1)]);
      const hiddenOne = root.hidden;
      const rows = [r('tiny', 3, 3), r('big', 74, 160), r('mid', 20, 40)];
      const plan = deck.statsScatterPlan(rows);
      const by = Object.fromEntries(plan.dots.map(d => [d.agent, d]));
      deck.renderStatsReviewerScatter(rows);
      const ys = plan.dots.map(d => d.labelY).sort((x, y) => x - y);
      return {
        one: one === null, hiddenOne,
        shown: !root.hidden, dots: root.querySelectorAll('.scatter-dot').length,
        logX: (by.big.x - by.mid.x) < (by.mid.x - by.tiny.x) * 2,
        whisker: (by.tiny.yLo - by.tiny.yHi) > (by.big.yLo - by.big.yHi),
        hollow: [by.tiny.low, by.big.low],
        gaps: ys.every((y, i) => i === 0 || y - ys[i - 1] >= 10.99),
        spill: root.scrollWidth <= root.clientWidth + 1,
      };
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(out["one"], true);
    assert_eq!(out["hiddenOne"], true);
    assert_eq!(out["shown"], true);
    assert_eq!(out["dots"], 3);
    assert_eq!(out["logX"], true);
    assert_eq!(out["whisker"], true);
    assert_eq!(out["hollow"], serde_json::json!([true, false]));
    assert_eq!(out["gaps"], true);
    assert_eq!(out["spill"], true);
    browser.close_page(&page).await;
}

#[tokio::test]
async fn chat_persona_select_sits_beside_the_agent_select_without_overlap() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let agent = fx.config.agents[0].id.clone();
    let mut t = talk::begin(&talks, &fx.config, fx.repo.clone(), Some(&agent)).expect("seed talk");
    talk::record(&mut t, &talks, "Which persona suits me?", Vec::new()).expect("seed turn");
    let talk_id = t.id.clone();

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    let w = Duration::from_secs(30);

    for (width, height, mobile) in [(1280u32, 900u32, false), (390, 844, true)] {
        let tag = format!("persona select @{width}px");
        let page = browser
            .open_page(&format!("{base}#/chat/{talk_id}"), width, height, mobile)
            .await
            .unwrap_or_else(|e| panic!("{tag}: open: {e}"));
        browser
            .wait_for(
                &page,
                "(() => { const b = document.getElementById('talk-persona-box'); \
                 return !!b && !b.hidden && document.querySelectorAll('#talk-persona option').length > 1; })()",
                w,
            )
            .await
            .unwrap_or_else(|e| panic!("{tag}: persona select never rendered: {e}"));
        let got = browser
            .eval(
                &page,
                "(() => { const s = document.getElementById('talk-persona'); \
                 const r = s.getBoundingClientRect(); \
                 const a = document.getElementById('talk-agent-box'); \
                 const ar = a.hidden ? null : a.getBoundingClientRect(); \
                 return { value: s.value, options: s.options.length, disabled: s.disabled, \
                 w: r.width, h: r.height, right: r.right, vw: document.documentElement.clientWidth, \
                 overlap: !!ar && ar.left < r.right - 1 && r.left < ar.right - 1 \
                   && ar.top < r.bottom - 1 && r.top < ar.bottom - 1 }; })()",
            )
            .await
            .unwrap_or_else(|e| panic!("{tag}: eval: {e}"));
        assert_eq!(got["value"], "default", "{tag}: {got}");
        assert!(got["options"].as_u64().unwrap_or(0) >= 8, "{tag}: {got}");
        assert_eq!(got["disabled"], false, "{tag}: {got}");
        assert!(
            got["w"].as_f64().unwrap_or(0.0) > 0.0 && got["h"].as_f64().unwrap_or(0.0) > 0.0,
            "{tag}: {got}"
        );
        assert!(
            got["right"].as_f64().unwrap_or(0.0) <= got["vw"].as_f64().unwrap_or(0.0) + 1.0,
            "{tag}: spills out {got}"
        );
        assert_eq!(
            got["overlap"], false,
            "{tag}: overlaps the agent select {got}"
        );
    }
}

// Persona and operator avatars load without overflowing either chat layout.
#[tokio::test]
async fn chat_avatars_load_and_do_not_overlap() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let agent = fx.config.agents[0].id.clone();
    let mut t = talk::begin(&talks, &fx.config, fx.repo.clone(), Some(&agent)).expect("seed talk");
    talk::record(
        &mut t,
        &talks,
        &"A long operator message with wrapping text. ".repeat(12),
        Vec::new(),
    )
    .expect("seed turn");
    t.persona = "rei".to_owned();
    t.turns.push(talk::Turn {
        sender: None,
        who: talk::Who::Agent,
        body: "An answer.".to_owned(),
        at: t.turns[0].at,
        attachments: Vec::new(),
        usage: None,
        breaks: None,
        messages: Vec::new(),
        posted_by: None,
    });
    talks.put(&mut t).expect("seed agent turn");
    talk::queue_once(&mut t, &talks, "An automatic report.", "report").unwrap();
    talk::drain(&mut t, &talks).unwrap();
    let talk_id = t.id.clone();

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    let w = Duration::from_secs(30);

    for (width, height, mobile) in [(1280u32, 900u32, false), (390, 844, true)] {
        let tag = format!("chat avatars @{width}px");
        let page = browser
            .open_page(&format!("{base}#/chat/{talk_id}"), width, height, mobile)
            .await
            .unwrap_or_else(|e| panic!("{tag}: open: {e}"));
        browser
            .wait_for(
                &page,
                "(() => { const a = document.querySelector('#talk-persona-box img'); \
                 const b = document.querySelector('.turn[data-who=agent] .turn-avatar'); \
                 const c = document.querySelector('.turn[data-who=operator] .turn-avatar'); \
                 const d = document.querySelector('.turn[data-who=system] .turn-avatar'); \
                 return [a, b, c, d].every((img) => img && !img.hidden && img.complete && img.naturalWidth > 0); })()",
                w,
            )
            .await
            .unwrap_or_else(|e| panic!("{tag}: avatars never loaded: {e}"));
        // Exercise a configured long name without changing the fixture's config layers.
        browser
            .eval(
                &page,
                "document.querySelector('.turn[data-who=operator] .turn-who').textContent = 'OperatorWithAVeryLongDisplayName'.repeat(6); true",
            )
            .await
            .unwrap();
        for theme in ["light", "dark"] {
            browser
                .eval(
                    &page,
                    &format!("document.documentElement.dataset.theme = '{theme}'; true"),
                )
                .await
                .unwrap();
            let got = browser
                .eval(
                    &page,
                    "(() => { const hit = (a, b) => a.left < b.right - 1 && b.left < a.right - 1 \
                       && a.top < b.bottom - 1 && b.top < a.bottom - 1; \
                     const r = (e) => e.getBoundingClientRect(); \
                     const inside = (a, b) => a.left >= b.left - 1 && a.right <= b.right + 1; \
                     const box = document.getElementById('talk-persona-box'); \
                     const sel = document.getElementById('talk-persona'); \
                     const pa = box.querySelector('img'); \
                     const vw = document.documentElement.clientWidth; \
                     const turns = ['agent', 'operator', 'system'].map((kind) => { \
                       const turn = document.querySelector('.turn[data-who=' + kind + ']'); \
                       const img = turn.querySelector('.turn-avatar'); const who = turn.querySelector('.turn-who'); \
                       return { kind, src: img.getAttribute('src'), width: r(img).width, \
                         overlap: hit(r(img), r(who)), beside: r(who).left >= r(img).right - 1, inTurn: inside(r(img), r(turn)) && inside(r(who), r(turn)), \
                         inView: r(turn).left >= -1 && r(turn).right <= vw + 1, \
                         fits: turn.scrollWidth <= turn.clientWidth + 1 }; \
                     }); \
                     return { src: pa.getAttribute('src'), pw: r(pa).width, turns, \
                       selOverlap: hit(r(pa), r(sel)), inBox: inside(r(pa), r(box)), \
                       inView: r(pa).left >= -1 && r(pa).right <= vw + 1 }; })()",
                )
                .await
                .unwrap_or_else(|e| panic!("{tag} {theme}: eval: {e}"));
            assert!(
                got["src"].as_str().unwrap_or("").contains("persona=rei"),
                "{tag}: {got}"
            );
            assert!(got["pw"].as_f64().unwrap_or(0.0) <= 24.0, "{tag}: {got}");
            assert_eq!(got["selOverlap"], false, "{tag}: {got}");
            for k in ["inBox", "inView"] {
                assert_eq!(got[k], true, "{tag}: {k} {got}");
            }
            for turn in got["turns"].as_array().unwrap() {
                let src = turn["src"].as_str().unwrap();
                assert!(
                    if turn["kind"] == "operator" {
                        src.starts_with("/avatars/operator.svg?v=")
                    } else if turn["kind"] == "system" {
                        src.starts_with("/avatars/magi?v=")
                    } else {
                        src.contains("persona=rei")
                    },
                    "{tag}: {turn}"
                );
                let width = turn["width"].as_f64().unwrap();
                assert!(width > 0.0 && width <= 24.0, "{tag}: {turn}");
                assert_eq!(turn["overlap"], false, "{tag}: {turn}");
                for k in ["beside", "inTurn", "inView", "fits"] {
                    assert_eq!(turn[k], true, "{tag}: {k} {turn}");
                }
            }
        }
        browser.close_page(&page).await;
    }
}

// The Chat selectors are remembered per browser and applied to the next new
// conversation only; the conversation they were changed in keeps its values.
common::e2e! {
async fn chat_remembers_persona_and_implementers_for_new_conversations() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    publish_fixture_roster(&fx).await;
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let agent = fx.config.agents[0].id.clone();
    let mut t = talk::begin(&talks, &fx.config, fx.repo.clone(), Some(&agent)).expect("seed talk");
    talk::record(&mut t, &talks, "Remember my choices", Vec::new()).expect("seed turn");
    let talk_id = t.id.clone();

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    let w = Duration::from_secs(30);
    let page = browser
        .open_page(&format!("{base}#/chat/{talk_id}"), 1280, 900, false)
        .await
        .unwrap_or_else(|e| panic!("open: {e}"));
    browser
        .wait_for(
            &page,
            "(() => { const s = document.getElementById('talk-persona'); \
             return !!s && !s.disabled && s.options.length > 1 \
               && !localStorage.getItem('magi-talk-defaults'); })()",
            w,
        )
        .await
        .unwrap_or_else(|e| {
            panic!("persona select never rendered (or something was stored on load): {e}")
        });

    browser
        .eval(
            &page,
            "(() => { const s = document.getElementById('talk-persona'); \
             s.value = 'rei'; s.dispatchEvent(new Event('change')); return true; })()",
        )
        .await
        .unwrap_or_else(|e| panic!("change persona: {e}"));
    browser
        .wait_for(
            &page,
            "(() => { try { return JSON.parse(localStorage.getItem('magi-talk-defaults')).persona === 'rei'; } \
             catch (_) { return false; } })()",
            w,
        )
        .await
        .unwrap_or_else(|e| panic!("persona was not remembered: {e}"));
    browser
        .wait_for(
            &page,
            "(() => { const s = document.getElementById('talk-implementers'); return !!s && !s.disabled; })()",
            w,
        )
        .await
        .unwrap_or_else(|e| panic!("implementers select never settled: {e}"));
    browser
        .eval(
            &page,
            "(() => { const s = document.getElementById('talk-implementers'); \
             s.value = '3'; s.dispatchEvent(new Event('change')); return true; })()",
        )
        .await
        .unwrap_or_else(|e| panic!("change implementers: {e}"));
    // The config may refuse 3 here; either way the stored value is whatever
    // the server confirmed for this conversation, never a guess.
    browser
        .wait_for(
            &page,
            "(() => { const s = document.getElementById('talk-implementers'); return !!s && !s.disabled; })()",
            w,
        )
        .await
        .unwrap_or_else(|e| panic!("implementers change never settled: {e}"));

    browser
        .eval(
            &page,
            "(() => { document.getElementById('talk-start-go').click(); return true; })()",
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    // A loading shell keeps the previous selector values but disables them.
    // Wait for the new record to render before inspecting its remembered count.
    browser
        .wait_for(
            &page,
            &format!(
                "(() => {{ const s = document.getElementById('talk-persona'); \
                 const n = document.getElementById('talk-implementers'); \
                 return location.hash.startsWith('#/chat/') && !location.hash.endsWith('{talk_id}') \
                   && !!s && !s.disabled && s.value === 'rei' \
                   && !!n && !n.disabled; }})()"
            ),
            w,
        )
        .await
        .unwrap_or_else(|e| panic!("new conversation did not start with the remembered persona: {e}"));

    let stored = browser
        .eval(
            &page,
            "(() => JSON.parse(localStorage.getItem('magi-talk-defaults')))()",
        )
        .await
        .unwrap_or_else(|e| panic!("read stored: {e}"));
    let implementers = stored["implementers"].as_u64();
    if let Some(n) = implementers {
        let got = browser
            .eval(
                &page,
                "(() => document.getElementById('talk-implementers').value)()",
            )
            .await
            .unwrap_or_else(|e| panic!("read implementers: {e}"));
        assert_eq!(
            got,
            n.to_string(),
            "new conversation takes the remembered count: {stored}"
        );
    }

    // The original conversation keeps its own stored values.
    let original = browser
        .eval(
            &page,
            &format!(
                "(async () => {{ const r = await fetch('/api/talks/{talk_id}'); \
                 const t = await r.json(); return {{ persona: t.persona, implementers: t.implementers }}; }})()"
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("read original: {e}"));
    assert_eq!(original["persona"], "rei", "{original}");
}
}

// Regression guard, no browser needed: the fixture's own `[[agents]]` must
// load whatever the operator keeps in their machine layer. `isolated` hands
// the child an empty machine layer; without it this fails on any machine whose
// own `config.toml` declares `[[agents]]`.
common::e2e! {
async fn fixture_roster_loads_without_the_machine_layer() {
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    publish_fixture_roster(&fx).await;
}
}

/// The server discovers config from origin/main, rather than the in-memory
/// fixture used to seed talks. Give both sides the same mock roster.
async fn publish_fixture_roster(fx: &common::Fixture) {
    std::fs::write(
        fx.repo.join("magi.toml"),
        toml::to_string(&fx.config).expect("serialize fixture config"),
    )
    .expect("write fixture config");
    for args in [
        vec!["add", "magi.toml"],
        vec!["commit", "-m", "configure chat test roster"],
        vec!["push", "origin", "main"],
    ] {
        magi::git::git(&fx.repo, &args)
            .await
            .expect("publish fixture config");
    }
    let (discovered, _) =
        magi::config::Config::discover(&fx.repo, None).expect("discover published fixture config");
    assert!(discovered.agents.iter().any(|spec| spec.id == "beta"));
}

// Successful selector changes must be remembered even when their response
// arrives after the operator has opened another conversation.
common::e2e! {
async fn chat_remembers_selector_changes_after_navigation() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    publish_fixture_roster(&fx).await;
    let home = fx.tmp.path().join("magi-home");
    let talks = Talks::at(home.join("talks"));
    let agent = fx.config.agents[0].id.clone();
    let first = talk::begin(&talks, &fx.config, fx.repo.clone(), Some(&agent)).unwrap();
    let second = talk::begin(&talks, &fx.config, fx.repo.clone(), Some(&agent)).unwrap();
    let base = serve(
        &home,
        Queue::at(home.join("queue")),
        talks,
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("launch Chrome");
    let page = browser
        .open_page(&format!("{base}#/chat/{}", first.id), 1280, 900, false)
        .await
        .expect("open chat");
    let w = Duration::from_secs(30);
    for (field, value) in [
        ("agent", fx.config.agents[1].id.as_str()),
        ("persona", "rei"),
        ("implementers", "3"),
    ] {
        browser
            .eval(&page, &format!(
                "location.hash = '#/chat/{}'; true",
                first.id
            ))
            .await
            .expect("return to first chat");
        let ready = browser
            .wait_for(
                &page,
                &format!(
                    "location.hash === '#/chat/{}' \
                     && document.getElementById('talk-meta')?.title.startsWith('{}\\n') \
                     && document.getElementById('talk-agent')?.options.length > 1 \
                     && document.getElementById('talk-persona')?.options.length > 1 \
                     && document.getElementById('talk-{field}')?.disabled === false",
                    first.id, first.id
                ),
                w,
            )
            .await;
        if let Err(error) = ready {
            let snapshot = browser.eval(&page, "({ hash: location.hash, body: document.body?.innerText, agent: document.getElementById('talk-agent')?.outerHTML })").await;
            panic!("selector ready: {error}; {snapshot:?}");
        }
        browser
            .eval(
                &page,
                &format!(
                    r#"(() => {{
                        const originalFetch = window.fetch;
                        window.releaseTalkPreference = null;
                        window.fetch = async (...args) => {{
                            const response = await originalFetch(...args);
                            if (String(args[0]).endsWith('/{}/{field}') && args[1]?.method === 'POST') {{
                                if (!response.ok) throw new Error(await response.text());
                                window.fetch = originalFetch;
                                await new Promise(resolve => {{ window.releaseTalkPreference = resolve; }});
                            }}
                            return response;
                        }};
                        const select = document.getElementById('talk-{field}');
                        select.value = '{value}';
                        select.dispatchEvent(new Event('change'));
                        return true;
                    }})()"#,
                    first.id
                ),
            )
            .await
            .expect("change selector with response held");
        browser
            .wait_for(
                &page,
                "typeof window.releaseTalkPreference === 'function'",
                w,
            )
            .await
            .expect("server accepted change");
        // A hash change and a matching agent can describe the loading shell:
        // cached summaries already contain that agent. Wait for the rendered
        // record's identity so the delayed response really lands on another
        // loaded conversation, regardless of fetch/hashchange timing.
        browser
            .eval(&page, &format!(
                "location.hash = '#/chat/{}'; true",
                second.id
            ))
            .await
            .expect("navigate away");
        browser
            .wait_for(
                &page,
                &format!(
                    "location.hash === '#/chat/{}' \
                     && document.getElementById('talk-meta')?.title.startsWith('{}\\n') \
                     && document.getElementById('talk-agent')?.value === '{}'",
                    second.id, second.id, agent
                ),
                w,
            )
            .await
            .expect("second chat loaded");
        browser
            .eval(&page, "window.releaseTalkPreference(); true")
            .await
            .expect("release successful response");
        browser
            .wait_for(
                &page,
                &format!(
                    "String(JSON.parse(localStorage.getItem('magi-talk-defaults') || '{{}}').{field}) === '{value}'"
                ),
                w,
            )
            .await
            .unwrap_or_else(|e| panic!("{field} was not remembered after navigation: {e}"));
        let current = browser
            .eval(&page, "({ agent: document.getElementById('talk-agent').value, persona: document.getElementById('talk-persona').value, implementers: Number(document.getElementById('talk-implementers').value) })")
            .await
            .expect("read second chat");
        assert_eq!(current["agent"], agent);
        assert_eq!(current["persona"], "default");
        assert_eq!(current["implementers"], 1);
    }
}
}

/// The per-conversation selectors are chips just above the composer, so a
/// long transcript never has to be scrolled to the top to change one.
#[tokio::test]
async fn chat_selector_chips_stay_reachable_with_a_long_transcript() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };

    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let agent = fx.config.agents[0].id.clone();
    let mut t = talk::begin(&talks, &fx.config, fx.repo.clone(), Some(&agent)).expect("seed talk");
    for i in 0..40 {
        talk::record(&mut t, &talks, &format!("Question number {i}"), Vec::new())
            .expect("seed turn");
    }
    let talk_id = t.id.clone();

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome)
        .await
        .unwrap_or_else(|e| panic!("could not start Chrome at {}: {e}", chrome.display()));
    let w = Duration::from_secs(30);

    for (width, height, mobile) in [(1280u32, 900u32, false), (390, 844, true)] {
        let tag = format!("selector chips @{width}px");
        let page = browser
            .open_page(&format!("{base}#/chat/{talk_id}"), width, height, mobile)
            .await
            .unwrap_or_else(|e| panic!("{tag}: open: {e}"));
        browser
            .wait_for(
                &page,
                "(() => { const b = document.getElementById('talk-persona-box'); \
                 return !!b && !b.hidden && document.querySelectorAll('#talk-persona option').length > 1 \
                 && document.querySelectorAll('#talk-turns > li').length >= 40; })()",
                w,
            )
            .await
            .unwrap_or_else(|e| panic!("{tag}: never rendered: {e}"));
        let got = browser
            .eval(
                &page,
                "(() => { window.scrollTo(0, document.documentElement.scrollHeight); \
                 const vw = document.documentElement.clientWidth, vh = window.innerHeight; \
                 const hit = (r) => ({ x: r.left + r.width / 2, y: r.top + r.height / 2 }); \
                 const rect = (id) => document.getElementById(id).getBoundingClientRect(); \
                 const dock = document.querySelector('.dock'); \
                 const dr = dock && dock.offsetParent !== null ? dock.getBoundingClientRect() : null; \
                 const cross = (a, b) => !!b && a.left < b.right - 1 && b.left < a.right - 1 \
                   && a.top < b.bottom - 1 && b.top < a.bottom - 1; \
                 const out = { inHead: document.querySelectorAll('.view-head select').length, chips: [] }; \
                 for (const id of ['talk-agent-box', 'talk-persona-box', 'talk-implementers-box']) { \
                   const box = document.getElementById(id), r = box.getBoundingClientRect(); \
                   const p = hit(r), top = document.elementFromPoint(p.x, p.y); \
                   out.chips.push({ id, visible: !box.hidden && r.width > 0, h: r.height, \
                     inside: r.top >= 0 && r.bottom <= vh && r.left >= 0 && r.right <= vw, \
                     hit: !!top && box.contains(top), \
                     clash: cross(r, rect('f-talk-say')) || cross(r, rect('talk-send')) || cross(r, dr) }); \
                 } \
                 out.scrolled = window.scrollY > 0 || document.getElementById('talk-turns').scrollTop > 0; \
                 return out; })()",
            )
            .await
            .unwrap_or_else(|e| panic!("{tag}: eval: {e}"));
        assert_eq!(got["inHead"], 0, "{tag}: selector left in the head {got}");
        for c in got["chips"].as_array().expect("chips") {
            // The agent chip is hidden while no agent is installed (a CI
            // runner has none); persona and implementers are always shown.
            if c["id"] == "talk-agent-box" && c["visible"] == false {
                continue;
            }
            assert_eq!(c["visible"], true, "{tag}: {c}");
            assert!(
                c["h"].as_f64().unwrap_or(0.0) >= 44.0,
                "{tag}: too short {c}"
            );
            assert_eq!(c["inside"], true, "{tag}: outside the viewport {c}");
            assert_eq!(c["hit"], true, "{tag}: covered by something else {c}");
            assert_eq!(c["clash"], false, "{tag}: overlaps composer or dock {c}");
        }
    }
}

/// The whole-review stamp is a pure condition over the run record, and the
/// vote tag stamps only a plain approve (承認) or a reject (否決).
#[tokio::test]
async fn review_stamp_condition_and_vote_tags() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI is set but no Chrome/Chromium was found (set MAGI_CHROME)"
        );
        eprintln!("SKIP web_render: no Chrome/Chromium found (set MAGI_CHROME to run it)");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let base = serve(
        &home,
        queue.clone(),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser
        .open_page(&format!("{base}#/runs"), 390, 800, true)
        .await
        .expect("page");
    client_harness(&mut browser, &page).await;
    let out = browser
        .eval(
            &page,
            r#"(() => {
      const rec = (vote, extra) => Object.assign({ reviewer: 1, agent: 'a', vote, findings: [] }, extra || {});
      const round = (extra) => Object.assign({ round: 1, clean: true, blocking: 0, expected: 2,
        verdict: 'approve', reviews: [rec('approve'), rec('approve')], reconsideration: [] }, extra || {});
      const run = (r, extra) => Object.assign({ id: 'x', status: 'ready', reviews: [r] }, extra || {});
      const finding = { id: 'R1-1-1', severity: 'minor', title: 't' };
      const cases = {
        pass: run(round()),
        none: run(round(), { reviews: [] }),
        stalled: run(round(), { status: 'stalled' }),
        failed: run(round({ reviews: [rec('approve'), rec('approve', { failed: 'boom' })] })),
        absent: run(round({ reviews: [rec('approve')] })),
        withFindings: run(round({ verdict: 'approve_with_findings', reviews: [rec('approve'), rec('approve_with_findings')] })),
        reject: run(round({ verdict: 'reject' })),
        revote: run(round({ reconsideration: [{ reviewer: 1, vote: 'approve_with_findings' }] })),
        open: run(round({ clean: false, blocking: 1 })),
        minor: run(round({ reviews: [rec('approve', { findings: [finding] }), rec('approve')] })),
      };
      const passed = {};
      for (const [k, v] of Object.entries(cases)) passed[k] = deck.reviewPassed(v);
      const text = (v) => deck.voteTag(v, '').textContent;
      const stamp = deck.reviewStamp();
      document.body.append(stamp);
      const box = stamp.getBoundingClientRect();
      return {
        passed,
        approve: text('approve'), awf: text('approve_with_findings'), reject: text('reject'), failed: text('failed'),
        stampText: stamp.textContent,
        fits: box.right <= window.innerWidth + 1 && box.left >= -1 && document.documentElement.scrollWidth <= window.innerWidth + 1,
      };
    })()"#,
        )
        .await
        .unwrap();
    for (k, v) in out["passed"].as_object().unwrap() {
        assert_eq!(v, &serde_json::json!(k == "pass"), "reviewPassed {k}");
    }
    assert_eq!(out["approve"], "approve \u{627f}\u{8a8d}");
    assert_eq!(out["awf"], "approve w/ findings");
    assert_eq!(out["reject"], "reject \u{5426}\u{6c7a}");
    assert_eq!(out["failed"], "failed");
    assert_eq!(out["stampText"], "\u{627f}\u{8a8d}REVIEW PASSED");
    assert_eq!(out["fits"], true);
    browser.close_page(&page).await;
}

#[tokio::test]
async fn linkify_builds_anchors_and_keeps_markup_as_text() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP linkify: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let mut target = Task::new(
        "Reference target".into(),
        "Target".into(),
        fx.repo.clone(),
        Source::Human,
    );
    target.id = "20260908-205803-12ba".into();
    target.status = TaskStatus::Held;
    target.hold_reason = Some("dead の修正を待つ".into());
    let mut alphabetic = Task::new(
        "Alphabetic id".into(),
        "Target".into(),
        fx.repo.clone(),
        Source::Human,
    );
    alphabetic.id = "20260908-205804-dead".into();
    queue.put(&mut alphabetic).unwrap();
    target.last_error = Some("See task@12ba and question d678".into());
    queue.put(&mut target).unwrap();
    let talks = Talks::at(home.join("talks"));
    let mut chat = talk::begin(&talks, &fx.config, fx.repo.clone(), None).unwrap();
    talks.remove(&chat.id).unwrap();
    chat.id = "20260908-205806-6f0d".into();
    std::fs::write(
        home.join("talks").join(format!("{}.json", chat.id)),
        serde_json::to_vec(&chat).unwrap(),
    )
    .unwrap();
    talk::record(
        &mut chat,
        &talks,
        "Task 12ba and `12ba`. dead の修正を待つ。Unknown 0000.\n\n```\n12ba\n```",
        Vec::new(),
    )
    .unwrap();
    chat.turns[0].who = talk::Who::Agent;
    chat.turns[0].sender = None;
    chat.turns[0].messages.clear();
    talks.put(&mut chat).unwrap();
    target.source = Source::Agent {
        run: chat.id.clone(),
        node: magi::queue::CHAT_NODE.into(),
    };
    queue.put(&mut target).unwrap();
    let questions = magi::ask::Questions::at(home.join("questions"));
    let mut question = magi::ask::Question::new(
        "run".into(),
        "merge".into(),
        "operator".into(),
        "Recorded approval".into(),
        "detail".into(),
        Vec::new(),
    );
    question.id = "20260908-205805-d678".into();
    question.status = magi::ask::QuestionStatus::Answered;
    questions.put(&mut question).unwrap();
    let base = serve(&home, queue.clone(), talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser
        .open_page(&format!("{base}#/queue"), 1280, 900, false)
        .await
        .expect("page");
    browser
        .wait_for(
            &page,
            "!!document.querySelector('#queue-sections li.card')",
            Duration::from_secs(30),
        )
        .await
        .expect("loaded queue");
    client_harness(&mut browser, &page).await;
    let out = browser
        .eval(
            &page,
            r#"(() => {
      const text = 'Pr: https://github.com/o/r/pull/635 (https://github.com/yukimemi/magi/pull/636). `https://x.test/a`; javascript:alert(1) <b>x</b> ftp://h/';
      const div = document.createElement('div');
      deck.linkify(div, text);
      deck.linkify(div, text, { replace: true });
      deck.linkify(div, text, { replace: true });
      const anchors = [...div.querySelectorAll('a')];
      return {
        hrefs: anchors.map(a => a.getAttribute('href')),
        rel: anchors.map(a => a.rel),
        target: anchors.map(a => a.target),
        bold: div.querySelectorAll('b').length,
        text: div.textContent === text,
      };
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out["hrefs"],
        serde_json::json!([
            "https://github.com/o/r/pull/635",
            "https://github.com/yukimemi/magi/pull/636",
            "https://x.test/a"
        ])
    );
    assert_eq!(
        out["rel"],
        serde_json::json!([
            "noopener noreferrer",
            "noopener noreferrer",
            "noopener noreferrer"
        ])
    );
    assert_eq!(
        out["target"],
        serde_json::json!(["_blank", "_blank", "_blank"])
    );
    assert_eq!(out["bold"], 0);
    assert_eq!(out["text"], true);
    browser
        .eval(
            &page,
            &format!(
                "window.referenceDocument = document; location.hash = '#/chat/{}'; true",
                chat.id
            ),
        )
        .await
        .unwrap();
    browser.wait_for(&page,
        "document.querySelectorAll('.turn[data-who=agent] a[href=\"#/tasks/20260908-205803-12ba\"]').length === 2",
        Duration::from_secs(30)).await.expect("Markdown text and code references");
    let out = browser.eval(&page, r##"(() => {
      const turn = document.querySelector('.turn[data-who=agent]');
      const links = [...turn.querySelectorAll('a')];
      const out = { alphabetic: !!turn.querySelector('a[href="#/tasks/20260908-205804-dead"]'), code: !!turn.querySelector('a code'), blockLinks: turn.querySelectorAll('pre a').length,
        sameTab: links.every(a => !a.target), unknown: links.some(a => a.textContent === '0000') };
      links[0].click();
      return out;
    })()"##).await.unwrap();
    assert_eq!(
        out,
        serde_json::json!({ "alphabetic": true, "code": true, "blockLinks": 0, "sameTab": true, "unknown": false })
    );
    browser.wait_for(&page,
        "location.hash === '#/tasks/20260908-205803-12ba' && !!document.querySelector('#task-note a[href=\"#/tasks/20260908-205804-dead\"]') && !!document.querySelector('#task-error a')",
        Duration::from_secs(30)).await.expect("task note and error references");
    assert_eq!(
        browser
            .eval(&page, "window.referenceDocument === document")
            .await
            .unwrap(),
        true
    );
    // Only candidate-bearing ranges may be resent after their first resolution.
    assert_eq!(
        browser
            .eval(
                &page,
                r##"(async () => {
      const original = window.fetch;
      const plain = 'Reference probe ordinary prose';
      const candidate = 'Reference probe candidate 12ba';
      const counts = { plain: 0, candidate: 0 };
      window.fetch = (url, init) => {
        if (url === '/api/references') {
          for (const item of JSON.parse(init.body)) {
            if (item.text === plain) counts.plain++;
            if (item.text === candidate) counts.candidate++;
          }
        }
        return original(url, init);
      };
      const div = document.createElement('div'); document.body.append(div);
      try {
        deck.linkify(div, plain);
        deck.linkify(div, candidate);
        const settled = async () => {
          for (let i = 0; i < 300; i++) {
            if (div.querySelector('a')) return;
            await new Promise(resolve => setTimeout(resolve, 10));
          }
          throw new Error('reference probe did not resolve');
        };
        await settled();
        // A resolved anchor means the whole batch, including prose, was applied.
        deck.refreshReferences();
        for (let i = 0; i < 300 && counts.candidate < 2; i++) {
          await new Promise(resolve => setTimeout(resolve, 10));
        }
        return counts;
      } finally {
        window.fetch = original;
        div.remove();
      }
    })()"##
            )
            .await
            .unwrap(),
        serde_json::json!({ "plain": 1, "candidate": 2 })
    );
    // A delayed decorator must never overwrite a newer text value or navigate.
    browser.eval(&page, r##"(() => {
      const original = window.fetch;
      window.fetch = (url, init) => {
        if (url !== '/api/references') return original(url, init);
        window.fetch = original;
        return new Promise(resolve => { window.referenceRelease = () => resolve(original(url, init)); });
      };
      const div = document.createElement('div'); div.id = 'reference-race'; document.body.append(div);
      deck.linkify(div, '12ba');
      return true;
    })()"##).await.unwrap();
    browser
        .wait_for(
            &page,
            "typeof window.referenceRelease === 'function'",
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_eq!(
        browser
            .eval(
                &page,
                r##"(async () => {
      const div = document.getElementById('reference-race');
      div.textContent = 'changed';
      window.referenceRelease();
      await new Promise(resolve => setTimeout(resolve, 200));
      return div.textContent === 'changed' && !div.querySelector('a')
        && location.hash === '#/tasks/20260908-205803-12ba';
    })()"##
            )
            .await
            .unwrap(),
        true
    );
    browser.eval(&page, r##"(() => {
      const source = document.querySelector('#task-meta a[href="#/chat/20260908-205806-6f0d"]');
      const approval = document.querySelector('#task-error a[href="#/questions/20260908-205805-d678"]');
      if (!source || !approval) throw new Error('missing source or approval reference');
      approval.click(); return true;
    })()"##).await.unwrap();
    browser.wait_for(&page,
        "location.hash === '#/questions/20260908-205805-d678' && document.activeElement.classList.contains('ask-summary') && document.activeElement.textContent === 'Recorded approval'",
        Duration::from_secs(30)).await.expect("answered question permalink focuses its card");
    browser.eval(&page, r##"(() => {
      const div = document.createElement('div'); div.id = 'reference-live'; document.body.append(div);
      deck.linkify(div, '9abc'); return true;
    })()"##).await.unwrap();
    let mut late = Task::new(
        "Late reference".into(),
        "Target".into(),
        fx.repo.clone(),
        Source::Human,
    );
    late.id = "20260908-205809-9abc".into();
    queue.put(&mut late).unwrap();
    browser
        .eval(&page, "deck.refreshReferences(); true")
        .await
        .unwrap();
    browser
        .wait_for(
            &page,
            "!!document.querySelector('#reference-live a')",
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    std::fs::remove_file(queue.path_of(&late.id)).unwrap();
    browser
        .eval(&page, "deck.refreshReferences(); true")
        .await
        .unwrap();
    browser.wait_for(&page,
        "!document.querySelector('#reference-live a') && document.getElementById('reference-live').textContent === '9abc' && location.hash === '#/questions/20260908-205805-d678'",
        Duration::from_secs(30)).await.expect("store changes invalidate references without navigating");
}

#[tokio::test]
async fn dep_graph_labels_are_truncated_by_display_width() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP truncateLabel: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    seed_tasks(&queue, &fx.repo, "run");
    let base = serve(
        &home,
        queue.clone(),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser
        .open_page(&format!("{base}#/queue"), 1280, 900, false)
        .await
        .expect("page");
    browser
        .wait_for(
            &page,
            "!!document.querySelector('#queue-sections li.card')",
            Duration::from_secs(30),
        )
        .await
        .expect("loaded queue");
    client_harness(&mut browser, &page).await;
    let out = browser
        .eval(
            &page,
            r#"(() => {
      const t = deck.truncateLabel;
      const w = s => [...s].reduce((n, c) => n + (c.codePointAt(0) >= 0x3000 && c.codePointAt(0) <= 0x9fff ? 2 : 1), 0);
      const a22 = 'a'.repeat(22), a23 = 'a'.repeat(23);
      const kana = 'あ'.repeat(30);
      const mixed = 'PR ' + 'レビュー指摘への対応方針を決める';
      const half = 'ｱ'.repeat(30);
      const astral = '𠮷'.repeat(30);
      return {
        a22: t(a22, 22) === a22,
        a23: t(a23, 22) === 'a'.repeat(21) + '…',
        kana: t(kana, 22),
        kanaW: w(t(kana, 22).slice(0, -1)),
        mixedW: w(t(mixed, 22).slice(0, -1)),
        mixedCut: t(mixed, 22).endsWith('…'),
        half: t(half, 22) === 'ｱ'.repeat(21) + '…',
        astral: t(astral, 22),
        short: t('日本語', 22) === '日本語',
      };
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(out["a22"], true);
    assert_eq!(out["a23"], true);
    assert_eq!(out["kana"], "あ".repeat(10) + "…");
    assert!(out["kanaW"].as_i64().unwrap() <= 21);
    assert!(out["mixedW"].as_i64().unwrap() <= 21);
    assert_eq!(out["mixedCut"], true);
    assert_eq!(out["half"], true);
    assert_eq!(out["astral"], "𠮷".repeat(10) + "…");
    assert_eq!(out["short"], true);
}

struct QuestionSeed<'a> {
    id: &'a str,
    run: &'a str,
    node: &'a str,
    seat: &'a str,
    summary: &'a str,
    asked_at: &'a str,
    choices: &'a [&'a str],
}

fn seed_questions(home: &std::path::Path, seeds: &[QuestionSeed<'_>]) {
    let store = magi::ask::Questions::at(home.join("questions"));
    for seed in seeds {
        let mut q = magi::ask::Question::new(
            seed.run.to_owned(),
            seed.node.to_owned(),
            seed.seat.to_owned(),
            seed.summary.to_owned(),
            String::new(),
            seed.choices.iter().map(|c| (*c).to_owned()).collect(),
        );
        q.id = seed.id.to_owned();
        q.asked_at = seed.asked_at.parse().expect("valid question timestamp");
        store.put_executable(&mut q).expect("seed question");
    }
}

/// Persisted question IDs stay visible in each treatment and survive the same
/// keyed reconciliation used by SSE, without taking focus from an answer.
#[tokio::test]
async fn question_identity_disclosure_copy_and_live_updates() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "CI requires Chrome");
        eprintln!("SKIP question identity: no Chrome/Chromium found");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    seed_questions(
        &home,
        &[
            QuestionSeed {
                id: "20261012-230524-5d55",
                run: "20261012-230500-aaaa",
                node: magi::github_text::ASK_NODE,
                seat: magi::github_text::ASK_SEAT,
                summary: "Choose the posting text",
                asked_at: "2026-10-12T23:05:26Z",
                choices: &[
                    magi::github_text::USE_FALLBACK,
                    magi::github_text::POST_ORIGINAL,
                ],
            },
            QuestionSeed {
                id: "20261012-230525-bbbb",
                run: "20261012-230500-aaaa",
                node: "implement",
                seat: "impl-A",
                summary: "Choose the posting text",
                asked_at: "2026-10-12T23:05:25Z",
                choices: &[],
            },
            QuestionSeed {
                id: "20261012-230526-cccc",
                run: "20261012-230500-aaaa",
                node: magi::land::APPROVAL_NODE,
                seat: "land",
                summary: "Choose the posting text",
                asked_at: "2026-10-12T23:05:24Z",
                choices: &[magi::land::APPROVE, magi::land::HOLD],
            },
        ],
    );
    let base = serve_with_client_probe(
        &home,
        Queue::at(home.join("queue")),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Questions,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.unwrap();
    for (width, height, mobile) in [(1280, 900, false), (390, 844, true)] {
        let page = browser
            .open_page(&format!("{base}#/questions"), width, height, mobile)
            .await
            .unwrap();
        browser
            .wait_for(
                &page,
                "!!window.deck && Array.isArray(deck.state.questions) && deck.state.questions.length === 3",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        let ids = [
            "20261012-230524-5d55",
            "simple",
            "20261012-230525-abcd",
            "ending-",
            "",
        ];
        let shorts = browser
            .eval(
                &page,
                &format!("{}.map(deck.shortId)", serde_json::to_string(&ids).unwrap()),
            )
            .await
            .unwrap();
        let expected: Vec<_> = ids.iter().map(|id| magi::ask::short_id(id)).collect();
        assert_eq!(shorts, serde_json::json!(expected));
        let out = browser.eval(&page, r##"(async () => {
          const check = (ok, why) => { if (!ok) throw new Error(why); };
          await window.queueReady;
          while (deck.storeReads.get('questions')) await deck.storeReads.get('questions').promise;
          await deck.loadQuestionOrigins();
          await new Promise(r => requestAnimationFrame(r));
          const q = deck.state.questions.find(item => item.id === '20261012-230524-5d55');
          const normal = deck.state.questions.find(item => item.id === '20261012-230525-bbbb');
          const merge = deck.state.questions.find(item => item.id === '20261012-230526-cccc');
          check(Boolean(q && normal && merge), 'seeded questions missing');
          const rows = [...document.querySelectorAll('#questions-list .ask')];
          check(rows.length === 3, 'question cards missing');
          check(rows.map(r => r.querySelector('summary').textContent).join(',') === '#5d55,#bbbb,#cccc', 'wrong short IDs');
          const row = rows[0], identity = row.querySelector('.question-identity');
          identity.querySelector('summary').click();
          check(identity.open, 'tap did not expand');
          check(identity.querySelector('code').textContent === q.id, 'wrong full ID');
          check(getComputedStyle(identity.querySelector('code')).userSelect === 'text', 'manual copy unavailable');
          const tap = identity.querySelector('summary').getBoundingClientRect();
          check(tap.height >= 44, 'small tap target');
          deck.focusFirstAsk();
          await new Promise(r => requestAnimationFrame(r));
          check(document.activeElement === row.querySelector('.ask-summary'), 'initial question focus missing');
          // A queued route focus must not take focus back from a new answer.
          deck.focusFirstAsk();
          const draft = rows[1].refs.text; draft.value = 'Keep this draft'; draft.focus();
          deck.state.questions = deck.state.questions.map(q => ({ ...q })); deck.renderQuestions();
          check(row === document.querySelector('#questions-list .ask'), 'row replaced');
          check(row.querySelector('.question-identity') === identity && identity.open, 'disclosure reset');
          check(document.activeElement === draft && draft.value === 'Keep this draft', 'draft or focus lost');
          let copied;
          Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async id => { copied = id; } } });
          const copy = identity.querySelector('button'); copy.click();
          await new Promise(r => requestAnimationFrame(r));
          check(copied === q.id && identity.querySelector('[role=status]').textContent === 'Copied full ID.', 'copy failed');
          const realExecCommand = document.execCommand;
          const textareas = document.querySelectorAll('textarea').length;
          draft.setSelectionRange(1, 4, 'backward');
          const beforeScroll = [scrollX, scrollY], beforeRect = identity.getBoundingClientRect();
          const beforeViewport = [visualViewport.width, visualViewport.height];
          let legacyCalls = 0;
          document.execCommand = command => {
            legacyCalls++;
            const field = document.activeElement;
            check(command === 'copy' && field.tagName === 'TEXTAREA' && field.value === q.id, 'wrong fallback content');
            check(field.readOnly && field.inputMode === 'none', 'fallback can open keyboard');
            check(field.selectionStart === 0 && field.selectionEnd === q.id.length, 'fallback selection incomplete');
            check(getComputedStyle(field).position === 'fixed' && getComputedStyle(field).opacity === '0', 'fallback visible');
            return true;
          };
          navigator.clipboard.writeText = async () => { throw new Error('denied'); };
          copy.click(); await new Promise(r => setTimeout(r, 0));
          check(legacyCalls === 1 && identity.querySelector('[role=status]').textContent === 'Copied full ID.', 'rejection fallback failed');
          Object.defineProperty(navigator, 'clipboard', { configurable: true, value: undefined });
          copy.click();
          check(legacyCalls === 2, 'missing API fallback was deferred');
          await new Promise(r => setTimeout(r, 0));
          check(identity.querySelector('[role=status]').textContent === 'Copied full ID.', 'missing API fallback failed');
          check(document.activeElement === draft, 'fallback lost draft focus');
          check(draft.selectionStart === 1 && draft.selectionEnd === 4 && draft.selectionDirection === 'backward', 'fallback lost draft selection');
          check(document.querySelectorAll('textarea').length === textareas, 'fallback leaked textarea');
          check(scrollX === beforeScroll[0] && scrollY === beforeScroll[1], 'fallback scrolled');
          const afterRect = identity.getBoundingClientRect();
          check(afterRect.x === beforeRect.x && afterRect.y === beforeRect.y && afterRect.width === beforeRect.width && afterRect.height === beforeRect.height, 'fallback changed layout');
          check(visualViewport.width === beforeViewport[0] && visualViewport.height === beforeViewport[1], 'fallback changed viewport');
          for (const throws of [false, true]) {
            document.execCommand = () => { if (throws) throw new Error('blocked'); return false; };
            copy.click(); await new Promise(r => setTimeout(r, 0));
            check(identity.querySelector('[role=status]').textContent.includes('Select the full ID'), 'copy failure hidden');
            check(window.getSelection().toString() === q.id, 'failed copy did not select full ID');
            check(document.querySelectorAll('textarea').length === textareas, 'failed fallback leaked textarea');
          }
          document.execCommand = realExecCommand;
          const full = identity.querySelector('code').getBoundingClientRect();
          check(full.right <= innerWidth && full.left >= 0, 'full ID overflow');
          let request;
          const realFetch = window.fetch;
          window.fetch = async (url, options) => {
            if (url !== '/api/questions/' + q.id + '/answer') return realFetch(url, options);
            request = { url, body: JSON.parse(options.body) };
            return new Response(JSON.stringify({ ...q, status: 'answered', answer: request.body }), { status: 200, headers: { 'Content-Type': 'application/json' } });
          };
          row.refs.choices.querySelector('button').click();
          try {
            const deadline = performance.now() + 5000;
            while (row.dataset.state !== 'answered' && performance.now() < deadline) {
              await new Promise(r => setTimeout(r, 10));
            }
          } finally { window.fetch = realFetch; }
          check(request.url.endsWith('/' + q.id + '/answer'), 'answer used short or run ID');
          check(request.body.choice === 'use fallback' && row.dataset.state === 'answered', 'answer behavior changed');
          deck.renderAsks({ id: q.run });
          check(document.querySelectorAll('#run-asks .question-identity').length === 3, 'compact IDs missing');
          deck.openPanel(merge);
          const dialogId = document.querySelector('#panel-full-identity .question-identity');
          check(dialogId.querySelector('summary').textContent === '#cccc', 'approval panel ID missing');
          dialogId.querySelector('summary').click();
          check(dialogId.open && dialogId.querySelector('code').textContent === merge.id, 'panel full ID missing');
          check(dialogId.querySelector('code').getBoundingClientRect().right <= innerWidth, 'panel ID overflow');
          // Leave the modal open for the trusted fallback click/tap below.
          return true;
        })()"##).await.unwrap();
        assert_eq!(out, true, "question identity @{width}px");
        // Exercise the native legacy command with a trusted click/tap, then
        // read back the clipboard instead of trusting the status message.
        for selector in [
            "#panel-full-identity .question-identity",
            "#questions-list .question-identity",
        ] {
            let point = browser
            .eval(
                &page,
                r#"(async () => {
          Object.defineProperty(navigator, 'clipboard', { configurable: true, value: undefined });
          const identity = document.querySelector('__IDENTITY__');
          identity.open = true;
          // Keep the feedback size stable while measuring fallback side effects.
          identity.querySelector('[role=status]').textContent = 'Copied full ID.';
          const button = identity.querySelector('button');
          if (identity.closest('dialog')) window.scrollTo({ top: 0, left: 0, behavior: 'instant' });
          else button.scrollIntoView({ block: 'center', behavior: 'instant' });
          await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
          button.focus({ preventScroll: true });
          window.copyProbe = { selector: '__IDENTITY__', id: identity.dataset.questionId, x: scrollX, y: scrollY,
            height: visualViewport.height, count: document.querySelectorAll('textarea').length };
          const rect = button.getBoundingClientRect();
          return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
        })()"#.replace("__IDENTITY__", selector).as_str(),
            )
            .await
            .unwrap();
            if mobile {
                browser
                    .call(
                        Some(&page.session),
                        "Emulation.setTouchEmulationEnabled",
                        serde_json::json!({ "enabled": true }),
                    )
                    .await
                    .unwrap();
                for kind in ["touchStart", "touchEnd"] {
                    let points = if kind == "touchStart" {
                        serde_json::json!([{ "x": point["x"], "y": point["y"] }])
                    } else {
                        serde_json::json!([])
                    };
                    browser
                        .call(
                            Some(&page.session),
                            "Input.dispatchTouchEvent",
                            serde_json::json!({ "type": kind, "touchPoints": points }),
                        )
                        .await
                        .unwrap();
                }
            } else {
                for kind in ["mousePressed", "mouseReleased"] {
                    browser
                        .call(
                            Some(&page.session),
                            "Input.dispatchMouseEvent",
                            serde_json::json!({ "type": kind, "x": point["x"], "y": point["y"],
                        "button": "left", "clickCount": 1 }),
                        )
                        .await
                        .unwrap();
                }
            }
            browser.wait_for(&page,
            &format!("document.querySelector('{selector} [role=status]').textContent === 'Copied full ID.'"),
            Duration::from_secs(5)).await.unwrap();
            browser
            .call(
                None,
                "Browser.grantPermissions",
                serde_json::json!({
                    "origin": base, "permissions": ["clipboardReadWrite", "clipboardSanitizedWrite"]
                }),
            )
            .await
            .unwrap();
            let native_copy = browser
                .eval(
                    &page,
                    r#"(async () => {
          delete navigator.clipboard;
          const probe = window.copyProbe;
          const copied = await navigator.clipboard.readText();
          delete window.copyProbe;
          const actual = { copied, x: scrollX, y: scrollY, height: visualViewport.height,
            count: document.querySelectorAll('textarea').length,
            focused: document.activeElement === document.querySelector(probe.selector + ' button') };
          if (copied !== probe.id || actual.x !== probe.x || actual.y !== probe.y
              || actual.height !== probe.height || actual.count !== probe.count || !actual.focused) {
            throw new Error('native fallback state: ' + JSON.stringify({ probe, actual }));
          }
          return true;
        })()"#,
                )
                .await
                .unwrap();
            assert_eq!(native_copy, true, "native fallback click/tap @{width}px");
            browser.eval(&page, "deck.closePanel()").await.unwrap();
        }
        browser.close_page(&page).await;
    }
}

#[tokio::test]
async fn queue_compact_controls_and_edit_save_fit_and_preserve_drafts() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP queue controls: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let ids = seed_tasks(&queue, &fx.repo, "run");
    let original = queue.get(&ids[1]).unwrap();
    let base = serve_with_client_probe(
        &home,
        queue.clone(),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Queue,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    for width in [320, 390, 1280, 1950] {
        let page = browser
            .open_page(&format!("{base}#/queue"), width, 900, width < 400)
            .await
            .expect("page");
        browser
            .wait_for(
                &page,
                "!!document.querySelector('.task-card-actions')",
                Duration::from_secs(30),
            )
            .await
            .expect("queue loaded");
        let result = browser.eval(&page, &format!(r##"(async () => {{
          const id = {id};
          let card = document.querySelector(`#queue-sections li.card[data-task-id="${{id}}"]`);
          const assert = (ok, message) => {{ if (!ok) throw new Error(message); }};
          // Clicks start async handlers; observe their result rather than
          // racing the POST and refresh against a fixed sleep.
          const taskCard = () => document.querySelector(`#queue-sections li.card[data-task-id="${{id}}"]`);
          const clickUntil = (button, ready) => new Promise((resolve, reject) => {{
            const check = () => {{ if (ready()) {{ observer.disconnect(); resolve(); }} }};
            const observer = new MutationObserver(check);
            observer.observe(document.body, {{subtree: true, childList: true, attributes: true, characterData: true}});
            try {{ button.click(); check(); }} catch (error) {{ observer.disconnect(); reject(error); }}
          }});
          const visible = e => e.getBoundingClientRect().width > 0 && e.getBoundingClientRect().height > 0;
          const fits = e => {{ const r = e.getBoundingClientRect(), p = card.getBoundingClientRect(); return r.left >= p.left && r.right <= p.right + 1; }};
          assert(!card.querySelector('select'), 'no inline implementers');
          const controls = card.querySelector('.task-card-actions');
          assert([...controls.children].every(visible), 'controls visible on unselected card');
          assert([...controls.children].every(fits), 'controls fit card');
          const step = card.querySelectorAll('.btn-step')[1];
          await clickUntil(step, () => taskCard()?.querySelector('.task-priority-value')?.textContent === 'Priority 1');
          await deck.loadQueue();
          assert(card.querySelector('.task-priority-value').textContent === 'Priority 1', 'priority changes directly');
          const menu = card.querySelector('.task-action-menu');
          menu.querySelector('summary').click();
          card.querySelector('.task-hold-box button').click();
          const reason = card.querySelector('.task-hold-box input');
          reason.value = 'waiting on review'; reason.dispatchEvent(new Event('input'));
          reason.focus();
          await deck.loadQueue();
          assert(card.querySelector('.task-hold-box input') === reason && reason.value === 'waiting on review', 'poll preserves reason node');
          assert(document.activeElement === reason, 'poll preserves focus');
          reason.dispatchEvent(new KeyboardEvent('keydown', {{key: 'Escape', bubbles: true}}));
          assert(!menu.open, 'escape closes menu');
          menu.querySelector('summary').click();
          assert(menu.open && reason.value === 'waiting on review', 'reopening preserves confirmation');
          document.querySelector('#queue-h').click();
          assert(!menu.open, 'outside click closes menu');
          menu.querySelector('summary').click();
          await clickUntil(card.querySelectorAll('.task-hold-box button')[1],
            () => taskCard()?.querySelector('.chip')?.textContent.toLowerCase().includes('held'));
          await deck.loadQueue();
          card = document.querySelector(`#queue-sections li.card[data-task-id="${{id}}"]`);
          assert(card.querySelector('.chip').textContent.toLowerCase().includes('held'), 'hold applied');
          await clickUntil(card.querySelector('.task-hold-box button'),
            () => taskCard()?.querySelector('.chip')?.textContent.toLowerCase().includes('queued'));
          await deck.loadQueue();
          card = document.querySelector(`#queue-sections li.card[data-task-id="${{id}}"]`);
          const task = (await (await fetch(`/api/queue/${{id}}`)).json());
          deck.openTaskEdit(task);
          const field = document.querySelector('#task-edit-implementers');
          assert(field.value === '', 'repository default retained');
          document.querySelector('#task-edit-title').value = 'Revised task';
          field.value = '1';
          await deck.loadQueue();
          assert(field.value === '1', 'poll preserves edit draft');
          const dialog = document.querySelector('#task-edit-sheet');
          for (const e of dialog.querySelectorAll('input, textarea, select, #task-edit-save')) {{
            const r = e.getBoundingClientRect(), p = dialog.getBoundingClientRect();
            assert(r.left >= p.left && r.right <= p.right + 1, 'edit field fits sheet');
          }}
          const originalFetch = window.fetch;
          const calls = [];
          // Reference decoration also POSTs while refreshed cards render.
          // Observe and refuse only this task's writes, preserving those reads.
          const isTaskWrite = (url, options) => options?.method === 'POST'
            && url.startsWith(`/api/queue/${{id}}/`);
          window.fetch = async (url, options) => {{
            if (isTaskWrite(url, options)) {{
              calls.push(url);
              return new Response(JSON.stringify({{error: 'Refused'}}), {{status: 409}});
            }}
            return originalFetch(url, options);
          }};
          await deck.saveTaskEdit();
          assert(calls.length === 1 && calls[0].endsWith('/edit'), 'refused edit stops implementers');
          assert(dialog.open && field.value === '1', 'failure retains draft');
          window.fetch = async (url, options) => {{
            if (options?.method === 'POST' && url.endsWith('/implementers')) return new Response(JSON.stringify({{error: 'Count refused'}}), {{status: 409}});
            return originalFetch(url, options);
          }};
          await deck.saveTaskEdit();
          assert(dialog.open && document.querySelector('#task-edit-error').textContent.includes('Title and instruction saved'), 'partial save explained');
          calls.length = 0;
          window.fetch = async (url, options) => {{
            if (isTaskWrite(url, options)) calls.push(url);
            return originalFetch(url, options);
          }};
          await deck.saveTaskEdit();
          window.fetch = originalFetch;
          assert(calls.length === 1 && calls[0].endsWith('/implementers'), 'retry only unsaved field');
          assert(!dialog.open, 'success closes edit');
          const saved = (await (await fetch(`/api/queue/${{id}}`)).json());
          assert(saved.solo && saved.title === 'Revised task', 'combined save persisted');
          card = document.querySelector(`#queue-sections li.card[data-task-id="${{id}}"]`);
          card.querySelector('.task-action-menu').open = true;
          card.querySelector('.task-done-box button').click();
          assert(card.querySelector('.task-done-box .stakes-confirm'), 'done asks for confirmation');
          await clickUntil(card.querySelectorAll('.task-done-box button')[1],
            () => taskCard()?.querySelector('.chip')?.textContent.toLowerCase().includes('done'));
          await deck.loadQueue();
          const done = await (await fetch(`/api/queue/${{id}}`)).json();
          assert(done.status_str === 'done', 'done keeps task record');
          card = document.querySelector(`#queue-sections li.card[data-task-id="${{id}}"]`);
          card.querySelector('.task-action-menu').open = true;
          card.querySelector('.task-delete-box button').click();
          assert(card.querySelector('.task-delete-box .stakes-confirm'), 'delete asks for confirmation');
          await clickUntil(card.querySelectorAll('.task-delete-box button')[1], () => !taskCard());
          assert((await fetch(`/api/queue/${{id}}`)).status === 404, 'delete removes record');
          return true;
        }})()"##, id = Value::from(ids[1].as_str()))).await.expect("queue controls and saves");
        assert_eq!(result, true, "queue controls at {width}px");
        // Each viewport starts with the same editable repository-default task.
        let mut task = original.clone();
        queue.put(&mut task).unwrap();
        browser.close_page(&page).await;
    }
}

#[tokio::test]
async fn magi_icon_settings_preview_save_reload_and_reset_on_phone() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP MAGI icon settings: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let base = serve(
        &home,
        Queue::at(home.join("queue")),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.unwrap();
    let page = browser
        .open_page(&format!("{base}#/settings"), 390, 844, true)
        .await
        .unwrap();
    let timeout = Duration::from_secs(30);
    browser
        .wait_for(
            &page,
            "!!document.querySelector('.magi-icon-settings input')",
            timeout,
        )
        .await
        .unwrap();
    let result = browser.eval(&page, r#"(async () => {
      const panel = () => document.querySelector('.magi-icon-settings');
      const original = (await (await fetch('/api/magi-icon')).json()).avatar;
      const canvas = document.createElement('canvas'); canvas.width = 24; canvas.height = 24;
      canvas.getContext('2d').fillRect(0, 0, 24, 24);
      const blob = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
      const input = panel().querySelector('input');
      const transfer = new DataTransfer(); transfer.items.add(new File([blob], 'phone.png', {type:'image/png'}));
      input.files = transfer.files; input.dispatchEvent(new Event('change'));
      const until = async f => { for(let i=0;i<200;i++) { if(f()) return; await new Promise(r=>setTimeout(r,25)); } throw new Error('UI timeout'); };
      await until(() => panel().querySelector('img').src.startsWith('data:'));
      if ((await (await fetch('/api/magi-icon')).json()).avatar !== original) throw new Error('preview saved before Save');
      panel().querySelector('button').click();
      await until(() => panel().textContent.includes('MAGI icon saved.'));
      const saved = (await (await fetch('/api/magi-icon')).json()).avatar;
      if (saved === original || !panel().querySelector('img').getAttribute('src').includes(saved)) throw new Error('saved preview did not refresh');
      const rect = panel().getBoundingClientRect();
      if (rect.right > document.documentElement.clientWidth + 1 || panel().scrollWidth > panel().clientWidth + 1) throw new Error('phone overflow');
      return saved;
    })()"#).await.unwrap();
    browser.close_page(&page).await;
    let page = browser
        .open_page(&format!("{base}#/settings"), 390, 844, true)
        .await
        .unwrap();
    browser
        .wait_for(
            &page,
            &format!(
                "document.querySelector('.magi-icon-preview')?.getAttribute('src') === {}",
                result
            ),
            timeout,
        )
        .await
        .unwrap();
    browser
        .eval(
            &page,
            "document.querySelectorAll('.magi-icon-settings button')[1].click(); true",
        )
        .await
        .unwrap();
    browser.wait_for(&page, "document.querySelector('.magi-icon-settings').textContent.includes('Built-in MAGI icon restored.')", timeout).await.unwrap();
    assert!(!home.join("magi-icon.data").exists());
    browser.close_page(&page).await;
}

#[tokio::test]
async fn avatar_settings_preview_isolation_reload_and_reset_on_phone() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP avatar settings: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let base = serve(
        &home,
        Queue::at(home.join("queue")),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.unwrap();
    let page = browser
        .open_page(&format!("{base}#/settings"), 390, 844, true)
        .await
        .unwrap();
    let timeout = Duration::from_secs(30);
    browser
        .wait_for(
            &page,
            "!!document.querySelector('.avatar-settings input')",
            timeout,
        )
        .await
        .unwrap();
    let result = browser.eval(&page, r#"(async () => {
      const panel = () => document.querySelector('.avatar-settings');
      const original = (await (await fetch('/api/avatar-settings')).json()).operator;
      const canvas = document.createElement('canvas'); canvas.width = 24; canvas.height = 24;
      canvas.getContext('2d').fillRect(0, 0, 24, 24);
      const blob = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
      const input = panel().querySelector('input');
      const transfer = new DataTransfer(); transfer.items.add(new File([blob], 'phone.png', {type:'image/png'}));
      input.files = transfer.files; input.dispatchEvent(new Event('change'));
      const until = async f => { for(let i=0;i<200;i++) { if(f()) return; await new Promise(r=>setTimeout(r,25)); } throw new Error('UI timeout'); };
      await until(() => panel().querySelector('img').src.startsWith('data:'));
      if ((await (await fetch('/api/avatar-settings')).json()).operator !== original) throw new Error('preview saved before Save');
      panel().querySelector('button').click();
      await until(() => panel().textContent.includes('Avatar saved.'));
      const saved = (await (await fetch('/api/avatar-settings')).json()).operator;
      if (saved === original || !panel().querySelector('img').getAttribute('src').includes(saved)) throw new Error('saved preview did not refresh');
      const rect = panel().getBoundingClientRect();
      if (rect.right > document.documentElement.clientWidth + 1 || panel().scrollWidth > panel().clientWidth + 1) throw new Error('phone overflow');
      const select = panel().querySelector('select');
      select.value = 'persona:rei'; select.dispatchEvent(new Event('change'));
      if (!panel().querySelector('button').disabled) throw new Error('operator draft leaked');
      const personaInput = panel().querySelector('input');
      personaInput.files = transfer.files; personaInput.dispatchEvent(new Event('change'));
      await until(() => panel().querySelector('img').src.startsWith('data:'));
      panel().querySelector('button').click();
      await until(() => panel().textContent.includes('Avatar saved.'));
      const catalog = await (await fetch('/api/avatar-settings')).json();
      if (catalog.operator !== saved) throw new Error('persona save changed operator');
      const rei = catalog.personas.find(p => p.id === 'rei');
      const asuka = catalog.personas.find(p => p.id === 'asuka');
      if (!rei || !asuka || rei.avatar === asuka.avatar) throw new Error('persona isolation');
      panel().querySelectorAll('button')[1].click();
      await until(() => panel().textContent.includes('Built-in avatar restored.'));
      const reset = await (await fetch('/api/avatar-settings')).json();
      if (reset.personas.find(p => p.id === 'rei').avatar === rei.avatar) throw new Error('reset URL unchanged');
      const sel = panel().querySelector('select');
      sel.value = 'operator'; sel.dispatchEvent(new Event('change'));
      const invalid = new DataTransfer(); invalid.items.add(new File(['bad'], 'bad.png', {type:'image/png'}));
      const invalidInput = panel().querySelector('input'); invalidInput.files = invalid.files; invalidInput.dispatchEvent(new Event('change'));
      await until(() => !panel().querySelector('input').disabled);
      if (!panel().querySelector('button').disabled) throw new Error('invalid image accepted');
      if ((await (await fetch('/api/avatar-settings')).json()).operator !== saved) throw new Error('invalid image corrupted save');
      for (const button of panel().querySelectorAll('button')) if (button.getBoundingClientRect().height < 44) throw new Error('small touch target');
      return saved;
    })()"#).await.unwrap();
    browser.close_page(&page).await;
    let page = browser
        .open_page(&format!("{base}#/settings"), 390, 844, true)
        .await
        .unwrap();
    browser
        .wait_for(
            &page,
            &format!(
                "document.querySelector('.avatar-settings .magi-icon-preview')?.getAttribute('src') === {}",
                result
            ),
            timeout,
        )
        .await
        .unwrap();
    browser
        .eval(
            &page,
            "document.querySelectorAll('.avatar-settings button')[1].click(); true",
        )
        .await
        .unwrap();
    browser.wait_for(&page, "document.querySelector('.avatar-settings').textContent.includes('Built-in avatar restored.')", timeout).await.unwrap();
    assert!(!home.join("avatars/operator.data").exists());
    browser.close_page(&page).await;
}

#[tokio::test]
async fn chat_notification_display_hides_machine_context_on_phone_and_desktop() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP notification display: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let mut conversation = talk::begin(&talks, &fx.config, fx.repo.clone(), None).unwrap();
    talk::record(&mut conversation, &talks, "Report the result", vec![]).unwrap();
    let mut task = Task::new(
        "Notification example".into(),
        "Change".into(),
        fx.repo.clone(),
        Source::Agent {
            run: conversation.id.clone(),
            node: magi::queue::CHAT_NODE.into(),
        },
    );
    task.start("example-run".into());
    task.succeed();
    queue.put(&mut task).unwrap();
    magi::chat_notify::sweep(&queue, &talks, &home);
    talk::drain(&mut conversation, &talks).unwrap();
    let watch = magi::prompt::chat_watch_notice(
        "watch-123",
        "exit status 7",
        "<script>untrusted output</script>",
    );
    talk::queue_once(&mut conversation, &talks, &watch, "old-watch").unwrap();
    talk::drain(&mut conversation, &talks).unwrap();
    talk::record_reply(
        &mut conversation,
        &talks,
        "Explain that",
        vec![],
        Some(talk::ReplyTarget {
            turn: 1,
            message: 0,
        }),
    )
    .unwrap();
    talk::queue_once(&mut conversation, &talks, &watch, "queued-watch").unwrap();
    let id = conversation.id;
    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("launch browser");
    for (width, height, mobile) in [(1280, 900, false), (390, 844, true)] {
        let page = browser
            .open_page(&format!("{base}#/chat/{id}"), width, height, mobile)
            .await
            .unwrap();
        browser
            .wait_for(
                &page,
                "document.querySelectorAll('.notification-facts').length === 3",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        let result = browser.eval(&page, r##"(() => {
          const assert = (ok, message) => { if (!ok) throw new Error(message); };
          const text = document.querySelector('#talk-turns').innerText + document.querySelector('#talk-pending').innerText;
          assert(!/automatic report|not a message from the operator|Tell the operator|Do not file new tasks/.test(text), 'machine context hidden');
          const report = document.querySelector('#talk-message-1-0');
          assert(report.dataset.who === 'system', 'MAGI bubble');
          assert(report.innerText.includes('MAGI') && report.innerText.includes('done'), 'sender and status');
          assert(report.querySelector('a[href^="#/tasks/"]'), 'task link');
          const output = document.querySelector('#talk-message-2-0 details');
          assert(output && !output.open && !output.querySelector('script'), 'output collapsed and escaped');
          output.open = true;
          assert(output.innerText.includes('<script>untrusted output</script>'), 'literal output');
          report.querySelector('button').click();
          const excerpt = document.querySelector('#talk-reply-preview').innerText;
          assert(excerpt.includes('MAGI') && !excerpt.includes('automatic report'), 'composer excerpt');
          for (const card of document.querySelectorAll('.notification-facts')) {
            const r = card.getBoundingClientRect();
            assert(r.left >= 0 && r.right <= innerWidth + 1 && card.scrollWidth <= card.clientWidth + 1, 'facts fit viewport');
          }
          return true;
        })()"##).await.expect("notification display assertions");
        assert_eq!(result, true, "notifications at {width}px");
        browser.close_page(&page).await;
    }
}

/// Chat navigation remains independent of consultation and answer controls.
#[tokio::test]
async fn question_chat_link_survives_consultation_and_settlement() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "CI requires Chrome");
        eprintln!("SKIP question chat link: no Chrome/Chromium found");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let talks = Talks::at(home.join("talks"));
    let mut chat = talk::begin(
        &talks,
        &fx.config,
        fx.repo.clone(),
        Some(&fx.config.agents[0].id),
    )
    .unwrap();
    talk::record(&mut chat, &talks, "Question origin chat", Vec::new()).unwrap();
    let queue = Queue::at(home.join("queue"));
    let mut task = Task::new(
        "Question origin task".into(),
        "Instruction".into(),
        fx.repo.clone(),
        Source::Agent {
            run: chat.id.clone(),
            node: magi::queue::CHAT_NODE.into(),
        },
    );
    task.status = TaskStatus::Running;
    task.runs = vec!["run".into()];
    queue.put(&mut task).unwrap();
    seed_questions(
        &home,
        &[QuestionSeed {
            id: "question-chat-link",
            run: "run",
            node: "implement",
            seat: "impl-A",
            summary: "Choose a backend",
            asked_at: "2026-10-12T23:05:24Z",
            choices: &[],
        }],
    );
    let base = serve_with_client_probe(
        &home,
        queue,
        talks,
        home.join("runs"),
        &fx.repo,
        ClientProbe::Questions,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.unwrap();
    for (width, height, mobile) in [(1280, 900, false), (390, 844, true), (320, 640, true)] {
        let page = browser
            .open_page(&format!("{base}#/questions"), width, height, mobile)
            .await
            .unwrap();
        browser
            .wait_for(
                &page,
                "!!window.deck && Array.isArray(deck.state.questions) && deck.state.questions.length === 1",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        browser
            .eval(&page, &format!("window.originChat = {:?}", chat.id))
            .await
            .unwrap();
        let out = browser.eval(&page, r##"(async () => {
          await window.queueReady;
          while (deck.storeReads.get('questions')) await deck.storeReads.get('questions').promise;
          await deck.loadQuestionOrigins();
          const check = (ok, why) => { if (!ok) throw new Error(why); };
          const q = deck.state.questions.find(item => item.id === 'question-chat-link');
          check(q && q.origin_chat === originChat && q.can_consult_chat, 'origin chat not loaded');
          const render = (patch = {}) => {
            Object.assign(q, patch); deck.state.questions = [{ ...q }]; deck.renderQuestions();
          };
          render();
          const row = document.querySelector('#questions-list .ask');
          const { consultBtn, chatLink, text, sayText } = row.refs;
          check(chatLink.tagName === 'A' && !chatLink.hidden, 'missing native chat link');
          check(!consultBtn.hidden && !consultBtn.disabled, 'consultation unavailable');
          text.value = 'Keep this answer'; sayText.value = 'Keep this reply'; text.focus();
          render({ consult: { talk: originChat } });
          check(consultBtn.disabled && consultBtn.textContent === 'Asked the chat agent', 'consultation state lost');
          check(!chatLink.hidden && chatLink.getAttribute('href') === '#/chat/' + encodeURIComponent(originChat), 'consulted chat unavailable');
          check(text.value === 'Keep this answer' && sayText.value === 'Keep this reply' && document.activeElement === text, 'draft or focus lost');
          check(!row.refs.send.disabled, 'answering disabled by consultation');
          const box = row.refs.consultBox.getBoundingClientRect();
          for (const control of [consultBtn, chatLink]) {
            const rect = control.getBoundingClientRect();
            check(rect.height >= 44, 'small tap target');
            check(rect.left >= box.left && rect.right <= box.right + 1 && rect.right <= innerWidth, 'action overflows');
          }
          const buttonRect = consultBtn.getBoundingClientRect(), linkRect = chatLink.getBoundingClientRect();
          check(linkRect.left >= buttonRect.right || linkRect.top >= buttonRect.bottom, 'actions overlap');
          render({ status: 'answered', can_consult_chat: false });
          check(consultBtn.hidden && !chatLink.hidden, 'settled chat unavailable');
          render({ origin_chat_closed: true });
          check(chatLink.textContent === 'Open chat (closed)', 'closed chat not labelled');
          render({ status: 'abandoned' });
          check(!chatLink.hidden, 'abandoned chat unavailable');
          render({ origin_chat: null });
          check(row.refs.consultBox.hidden && chatLink.hidden && !chatLink.hasAttribute('href'), 'stale chat link retained');
          render({ origin_chat: 'chat /?#日本語' });
          check(decodeURIComponent(chatLink.hash.split('/')[2]) === q.origin_chat, 'chat ID encoding lost');
          render({ origin_chat: originChat });
          chatLink.click();
          return true;
        })()"##).await.unwrap();
        assert_eq!(out, true, "question chat link @{width}px");
        browser
            .wait_for(
                &page,
                "!document.querySelector('#view-talk').hidden && document.querySelector('#talk-h').textContent === 'Question origin chat'",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        browser.close_page(&page).await;
    }
}

#[tokio::test]
async fn upgrade_failures_remain_readable_and_retryable_on_phone() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP upgrade UI: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let base = serve_with_client_probe(
        &home,
        Queue::at(home.join("queue")),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Upgrade,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.unwrap();
    let page = browser.open_page(&base, 390, 844, true).await.unwrap();
    let result = browser.eval(&page, r##"(async () => {
      while (!window.deck) await new Promise(r => setTimeout(r, 20));
      const assert = (ok, why) => { if (!ok) throw new Error(why); };
      const health = {version:'0.1.0', loop:{}, daemon:{}, update:{available:true, to:'v0.2.0'}, upgrade:{stage:'done', to:'v0.1.0'}};
      deck.state.health = health;
      deck.renderLoop();
      const button = document.getElementById('loop-upgrade');
      const box = document.getElementById('upgrade-error');
      const text = document.getElementById('upgrade-error-text');
      const attempt = async () => { await deck.upgrade(); await deck.upgrade(); };
      const reason = '<img src=x onerror=alert(1)> ' + 'GitHub API rate limit exceeded '.repeat(20);
      let calls = 0, release;
      window.fetch = async () => { calls++; return await new Promise(r => { release = r; }); };
      await deck.upgrade();
      const pending = deck.upgrade();
      await Promise.resolve();
      deck.renderLoop();
      assert(button.disabled, 'redraw preserves sending state');
      await deck.upgrade();
      assert(calls === 1, 'no duplicate POST while sending');
      release(new Response(JSON.stringify({error:reason,retry_after_secs:120,retry_at:2000000000}), {status:500}));
      await pending;
      assert(!box.hidden && text.textContent.includes(reason), 'full JSON reason visible');
      assert(!box.querySelector('img') && text.textContent.includes('Retry after'), 'safe text and retry timing');
      window.fetch = async () => new Response(JSON.stringify(health));
      await deck.loadHealth();
      deck.ok(); deck.renderLoop();
      assert(!box.hidden, 'normal health polling retains failure over previous done');
      const rect = box.getBoundingClientRect();
      assert(rect.width <= 390 && box.scrollWidth <= box.clientWidth && text.clientHeight > 100, 'phone wraps full message');
      document.getElementById('upgrade-error-dismiss').click();
      deck.renderLoop();
      assert(box.hidden, 'explicit dismissal survives redraw');
      for (const [reply, expected] of [
        [() => new Response('Proxy unavailable', {status:502, headers:{'Retry-After':'60'}}), '502'],
        [() => { throw new Error('Network disconnected'); }, 'Network disconnected']
      ]) {
        window.fetch = async () => reply();
        await attempt();
        deck.ok(); deck.renderLoop();
        assert(!box.hidden && text.textContent.includes(expected), 'HTTP/network failure retained');
        assert(!button.disabled, 'retry enabled after failure');
      }
      deck.state.health = health;
      // Health carries the pre-progress failure independently of old done.
      health.upgrade_request_error = {error:'Retained release check failure', occurred_at:'now'};
      // A successfully accepted retry clears local and stale server failures.
      window.fetch = async () => new Response(JSON.stringify({to:'v0.2.0', detail:'Accepted'}), {status:202});
      await attempt(); deck.renderLoop();
      assert(box.hidden, 'accepted retry clears stale errors');
      health.upgrade_request_error = {error:'New request failure', occurred_at:'later'};
      deck.renderLoop();
      assert(!box.hidden && text.textContent.includes('New request failure'), 'health restores request failure');
      health.upgrade = {stage:'downloading', to:'v0.2.0'};
      deck.renderLoop();
      assert(box.hidden, 'active progress wins over response loss or stale failure');
      return true;
    })()"##).await.expect("upgrade UI behavior");
    assert_eq!(result, true);
    browser.close_page(&page).await;
}

/// Task progress uses the detail snapshot, refreshes on queue changes, and
/// remains actionable without expanding the transcript or task panel first.
#[tokio::test]
async fn chat_task_progress_refreshes_and_survives_operation_races() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP chat task progress: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let mut chat = talk::begin(&talks, &fx.config, fx.repo.clone(), None).expect("chat");
    let mut tasks: Vec<_> = (0..7)
        .map(|i| {
            let mut task = Task::new(
                format!("Task {i}"),
                "instruction".into(),
                fx.repo.clone(),
                Source::Agent {
                    run: chat.id.clone(),
                    node: "chat".into(),
                },
            );
            queue.put(&mut task).expect("task");
            task
        })
        .collect();
    let base = serve_with_client_probe(
        &home,
        queue.clone(),
        talks.clone(),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Connection,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    for (width, height, mobile) in [(1280, 900, false), (390, 844, true)] {
        for (task, status) in tasks.iter_mut().zip([
            TaskStatus::Done,
            TaskStatus::Queued,
            TaskStatus::Running,
            TaskStatus::Held,
            TaskStatus::Failed,
            TaskStatus::Blocked,
            TaskStatus::Parked,
        ]) {
            task.status = status;
            queue.put(task).expect("status");
        }
        let page = browser
            .open_page(&format!("{base}#/chat/{}", chat.id), width, height, mobile)
            .await
            .expect("page");
        browser
            .wait_for(&page, "!!window.deck", Duration::from_secs(30))
            .await
            .expect("client");
        browser
            .eval(
                &page,
                &format!(
                    r#"(async () => {{
            deck.state.route = {{ name: 'talk', id: {id:?} }};
            deck.state.talkDetail = {{ id: {id:?}, talk: null }};
            document.getElementById('view-talk').hidden = false;
            await deck.loadTalk({id:?});
            await deck.loadHealth({{ applyRevisions: true }});
        }})()"#,
                    id = chat.id
                ),
            )
            .await
            .expect("load chat");
        let got = browser.eval(&page, r#"(() => {
            const panel = document.getElementById('talk-tasks-panel');
            const status = document.getElementById('talk-task-progress');
            const button = document.getElementById('talk-task-progress-open');
            const out = { text: status.textContent, folded: !panel.open };
            const r = status.getBoundingClientRect();
            out.fits = r.width > 0 && r.left >= 0 && r.right <= innerWidth
                && status.scrollWidth <= status.clientWidth;
            button.click();
            out.open = panel.open;
            out.focused = document.activeElement === document.querySelector('#talk-tasks a');
            const first = document.querySelector('#talk-tasks a').getBoundingClientRect();
            out.reachable = first.top >= 0 && first.bottom <= innerHeight;
            out.links = [...document.querySelectorAll('#talk-tasks a')].map(a => a.getAttribute('href'));
            out.first = document.querySelector('#talk-tasks a').textContent;
            out.summary = document.getElementById('talk-tasks-count').textContent;
            return out;
        })()"#).await.expect("measure progress");
        assert!(
            got["text"].as_str().unwrap().contains("6 of 7 unfinished"),
            "{got}"
        );
        for key in ["folded", "fits", "open", "focused", "reachable"] {
            assert_eq!(got[key], true, "@{width} {key}: {got}");
        }
        assert!(
            got["summary"].as_str().unwrap().contains("1 blocked"),
            "{got}"
        );
        assert!(
            !got["first"].as_str().unwrap().contains("Task 0"),
            "unfinished first: {got}"
        );
        let links = got["links"].as_array().unwrap();
        assert_eq!(links.len(), tasks.len());
        for task in &tasks {
            assert!(links.contains(&serde_json::json!(format!("#/tasks/{}", task.id))));
        }
        for task in &mut tasks {
            task.status = TaskStatus::Done;
            queue.put(task).expect("done");
        }
        // Queue changes alone must update the visible chat.
        browser
            .eval(&page, "deck.loadHealth({ applyRevisions: true })")
            .await
            .expect("queue refresh");
        browser
            .wait_for(
                &page,
                "(async () => { await deck.loadHealth({ applyRevisions: true }); return document.getElementById('talk-task-progress').textContent.includes('all 7 complete'); })()",
                Duration::from_secs(10),
            )
            .await
            .expect("shared monitor observes completed tasks");
        let got = browser
            .eval(
                &page,
                "document.getElementById('talk-task-progress').textContent",
            )
            .await
            .expect("completed text");
        assert!(
            got.as_str()
                .unwrap()
                .contains("Tasks filed from this chat: all 7 complete"),
            "{got}"
        );
        assert!(
            got.as_str().unwrap().contains("Other unresolved matters"),
            "{got}"
        );
        // Both revisions in one event still fetch the detail only once.
        tasks[0].status = TaskStatus::Queued;
        queue.put(&mut tasks[0]).expect("requeue");
        talk::record(&mut chat, &talks, "Another topic", Vec::new()).expect("chat revision");
        let got = browser
            .eval(
                &page,
                r#"(async () => {
            const native = window.fetch;
            let details = 0;
            window.fetch = (...args) => {
                if (String(args[0]).endsWith('/api/talks/' + deck.state.talkDetail.id + '/core')) details++;
                return native(...args);
            };
            try {
                const deadline = performance.now() + 10000;
                do {
                    const health = await (await native('/api/health')).json();
                    if (health.queue_rev !== deck.state.rev.queue && health.talks_rev !== deck.state.rev.talks) {
                        await deck.applyRevisions_(health);
                        break;
                    }
                    await new Promise(resolve => setTimeout(resolve, 50));
                } while (performance.now() < deadline);
            } finally { window.fetch = native; }
            return { details, text: document.getElementById('talk-task-progress').textContent };
        })()"#,
            )
            .await
            .expect("requeue refresh");
        assert_eq!(got["details"], 1, "{got}");
        assert!(
            got["text"].as_str().unwrap().contains("1 of 7 unfinished"),
            "{got}"
        );
        let got = browser.eval(&page, r#"(async () => {
            const native = window.fetch, id = deck.state.talkDetail.id;
            const snapshot = structuredClone(deck.state.talkDetail.talk);
            const sections = structuredClone(deck.state.talkDetail.sections);
            const pending = [];
            const response = body => ({ ok: true, status: 200, headers: new Headers(), json: async () => body });
            const out = {};
            window.fetch = () => new Promise(resolve => pending.push(resolve));
            try {
                const old = deck.loadTalk(id), fresh = deck.loadTalk(id);
                out.coalesced = pending.length === 2;
                pending.shift()(response(snapshot));
                pending.shift()(response(sections));
                await Promise.all([old, fresh]);
                deck.acceptTalkUpdate({ id, status: 'closed' });
                out.preserved = deck.state.talkDetail.talk.task_progress.total === 7
                    && deck.state.talkDetail.talk.tasks.length === 7;
                // A newer operation cancels both sections of the older read.
                const stale = pending.splice(0);
                deck.acceptTalkUpdate({ id, status: 'open' });
                const latest = deck.loadTalk(id);
                pending.shift()(response(snapshot));
                pending.shift()(response(sections));
                await latest;
                stale[0]({ ok: false, status: 404, headers: new Headers(), text: async () => '{}' });
                stale[1](response({ ...sections, tasks: [], task_progress: { state: 'none', total: 0, done: 0, counts: {}, unfinished_ids: [] } }));
                await new Promise(resolve => setTimeout(resolve, 0));
                out.stale404Ignored = !deck.state.talkDetail.gone && !!deck.state.talkDetail.talk
                    && deck.state.talkDetail.talk.tasks.length === 7;
                deck.renderTalkTasks({ id });
                out.unknown = document.getElementById('talk-task-progress-text').textContent.includes('not loaded yet');
                deck.renderTalkTasks({ id, tasks: [], task_progress: { state: 'none', total: 0, done: 0, counts: {}, unfinished_ids: [] } });
                out.empty = document.getElementById('talk-task-progress-text').textContent === 'No tasks filed from this chat.'
                    && document.getElementById('talk-tasks-panel').hidden;
            } finally { window.fetch = native; }
            return out;
        })()"#).await.expect("operation and read races");
        for key in [
            "coalesced",
            "preserved",
            "stale404Ignored",
            "unknown",
            "empty",
        ] {
            assert_eq!(got[key], true, "@{width} {key}: {got}");
        }
        browser.close_page(&page).await;
    }
}

#[tokio::test]
async fn run_refresh_keeps_question_focus_and_blocks_stale_actions() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP run refresh focus: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let mut run = RunState::new(
        fx.repo.clone(),
        "main".into(),
        "0000000".into(),
        "Check focus".into(),
        fx.config.clone(),
    );
    run.save_under(&home).unwrap();
    let questions = magi::ask::Questions::at(home.join("questions"));
    let mut q = magi::ask::Question::new(
        run.id.clone(),
        "review".into(),
        "seat".into(),
        "Keep typing".into(),
        "Details".into(),
        vec![],
    );
    questions.put(&mut q).unwrap();
    let base = serve_with_client_probe(
        &home,
        Queue::at(home.join("queue")),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Run,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser
        .open_page(&format!("{base}#/runs/{}", run.id), 1280, 900, false)
        .await
        .unwrap();
    browser.wait_for(&page,
        "window.deck && deck.state.detail.evidenceError === false && !!document.querySelector('#run-asks textarea')",
        Duration::from_secs(30)).await.unwrap();
    let out = browser.eval(&page, r#"(async () => {
      const box = document.querySelector('#run-asks textarea');
      const panel = document.querySelector('#run-ask-panel');
      box.value = 'An unfinished answer'; box.focus(); box.setSelectionRange(3, 8);
      const originalFetch = window.fetch;
      let release;
      const barrier = new Promise(r => release = r);
      let fail = false;
      window.fetch = async (...args) => {
        if (String(args[0]).match(/\/api\/runs\/[^/]+(?:\/core)?$/)) {
          await barrier;
          if (fail) throw new Error('deliberate failure');
        }
        return originalFetch(...args);
      };
      const refresh = deck.loadRun(deck.state.detail.id, {background: true});
      const button = [...document.querySelectorAll('#run-asks button')].find(b => b.textContent === 'Send answer');
      let clicked = 0;
      button.addEventListener('click', () => clicked++);
      button.click();
      const pending = document.activeElement === box && !panel.hidden
        && box.value === 'An unfinished answer' && clicked === 0
        && button.getAttribute('aria-disabled') === 'true';
      release(); await refresh;
      const success = document.activeElement === box && box.selectionStart === 3 && box.selectionEnd === 8;
      // A failed secondary refresh must leave the same editable DOM present.
      fail = true;
      await deck.loadRun(deck.state.detail.id, {background: true});
      button.click();
      const failure = document.activeElement === box && !panel.hidden
        && box.value === 'An unfinished answer' && clicked === 0;
      return {pending, success, failure};
    })()"#).await.unwrap();
    assert_eq!(
        out,
        serde_json::json!({
            "pending": true,
            "success": true,
            "failure": true,
        })
    );
    browser.close_page(&page).await;
}

/// The evidence-ready transition must render the sections gated on it even
/// when the evidence payload equals what core already put on screen.
#[tokio::test]
async fn run_landing_renders_when_core_precedes_identical_evidence() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP run core-before-evidence: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let mut merged = RunState::new(
        fx.repo.clone(),
        "main".into(),
        "0000000".into(),
        "Merged run with identical core and evidence payloads".into(),
        fx.config.clone(),
    );
    merged.id = "20260901-000009-cc09".into();
    merged.status = RunStatus::Merged;
    merged.pr = Some(magi::run::PrRecord {
        url: "https://github.com/example/repo/pull/7".into(),
        number: 7,
        state: "merged".into(),
        checks: "green".into(),
        round: 0,
        rounds: 3,
        red_at_merge: Vec::new(),
    });
    merged.save_under(&home).unwrap();
    let base = serve_with_client_probe(
        &home,
        Queue::at(home.join("queue")),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Run,
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let page = browser.open_page(&base, 1280, 900, false).await.unwrap();
    browser
        .wait_for(&page, "!!window.deck", Duration::from_secs(30))
        .await
        .unwrap();
    let out = browser.eval(&page, r#"(async () => {
      const originalFetch = window.fetch;
      let markCoreReady;
      const coreReady = new Promise(r => markCoreReady = r);
      let releaseEvidence;
      const evidenceBarrier = new Promise(r => releaseEvidence = r);
      window.fetch = async (...args) => {
        const url = String(args[0]);
        if (url.endsWith('/api/runs/20260901-000009-cc09/core')) {
          const res = await originalFetch(...args);
          const body = await res.json();
          return {
            ok: res.ok,
            status: res.status,
            headers: res.headers,
            json: () => {
              Promise.resolve().then(() => Promise.resolve()).then(markCoreReady);
              return Promise.resolve(body);
            },
          };
        }
        if (url.endsWith('/api/runs/20260901-000009-cc09')) {
          await evidenceBarrier;
        }
        return originalFetch(...args);
      };
      const loadMerged = deck.loadRun('20260901-000009-cc09');
      await coreReady;
      const coreBeforeEvidence = deck.state.detail.core !== null
        && deck.state.detail.evidence === null
        && deck.state.detail.evidenceError === true
        && !document.querySelector('#run-land .land-top');
      releaseEvidence();
      await loadMerged;
      const identicalPayloads = JSON.stringify(deck.state.detail.core) === JSON.stringify(deck.state.detail.evidence);
      const landRendered = deck.state.detail.evidenceError === false
        && !!document.querySelector('#run-land .land-top');
      window.fetch = originalFetch;
      return {coreBeforeEvidence, identicalPayloads, landRendered};
    })()"#).await.unwrap();
    assert_eq!(
        out,
        serde_json::json!({
            "coreBeforeEvidence": true,
            "identicalPayloads": true,
            "landRendered": true,
        })
    );
    browser.close_page(&page).await;
}
