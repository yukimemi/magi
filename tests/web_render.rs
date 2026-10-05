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
    tokio::spawn(async move {
        let _ = axum::serve(listener, ui.router()).await;
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

    let base = serve(&home, queue, talks, home.join("runs"), &fx.repo).await;
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

        for id in &ids {
            // Bring the row into view, then aim at its status chip: part of
            // the row, but not a link, a button or the title.
            let point = browser
                .eval(
                    &page,
                    &format!(
                        "(() => {{ const row = document.querySelector('li.card[data-task-id=\"{id}\"]'); \
                         if (!row) return null; row.scrollIntoView({{ block: 'center' }}); \
                         const c = row.querySelector('.card-top .chip, .card-top > span'); \
                         if (!c) return null; const r = c.getBoundingClientRect(); \
                         const x = r.left + r.width / 2, y = r.top + r.height / 2; \
                         const t = document.elementFromPoint(x, y); \
                         return {{ x, y, inRow: !!t && row.contains(t), title: row.querySelector('.card-title').innerText }}; }})()"
                    ),
                )
                .await
                .unwrap_or_else(|e| panic!("{tag}: locate {id}: {e}"));
            if point.is_null() || point["inRow"] != true {
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
            }
            let want = format!("#/tasks/{id}");
            // The previous task's heading stays until this one's detail
            // arrives, so wait for this task's own title when it has one.
            let title = point["title"].as_str().unwrap_or_default();
            let own = if title.is_empty() || title.starts_with("Instruction for") {
                "true".to_owned()
            } else {
                format!(
                    "document.getElementById('task-h').textContent.includes({})",
                    Value::from(title)
                )
            };
            let shown = format!(
                "location.hash === {want:?} \
                 && !document.getElementById('view-task').hidden \
                 && document.getElementById('split-empty').hidden \
                 && !document.getElementById('task-h').textContent.includes('Loading task') \
                 && !document.getElementById('task-h').textContent.includes('could not be loaded') \
                 && document.getElementById('task-h').textContent.trim() !== '' \
                 && {own}"
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
