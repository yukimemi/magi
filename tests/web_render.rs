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
    }
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
      window.deck = new Function(source.replace('queue: "/api/queue"', 'queue: "/api/queue?test_client=1"').replace(/\nboot\(\);\s*$/, `
        return { linkify, state, loadHealth, loadLoop, loadStats, resumeConnection, onPageShow, unreachable, loadQueue, loadRuns, loadTalks, applyRevisions_, storeReads, statsAgentTone, statsBarPlan, statsBarRows, statsScatterPlan, renderStatsReviewerScatter, reviewPassed, voteTag, reviewStamp };
      `))();
      await Promise.all([deck.loadQueue(), deck.loadRuns(), deck.loadTalks()]);
    })()"#,
        )
        .await
        .expect("install client harness");
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
        partial: performance.getEntriesByType('resource').some(r => r.name.includes('/api/queue?test_client=1&ids='))}};
    }})()"#)).await.unwrap();
    assert_eq!(check["same"], true);
    assert_eq!(check["partial"], true);
    assert_eq!(check["title"], "Updated through a delta");

    let result = browser.eval(&page, r#"(async () => {
      const native = window.fetch;
      const requests = [];
      let release;
      window.fetch = async (url, options) => {
        if (String(url).includes('/api/queue?test_client=1')) {
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
      const coalesced = {before, count: requests.length, fallback: requests[1] === '/api/queue?test_client=1', rev: deck.state.rev.queue};
      window.fetch = async () => { throw new Error('test read failed'); };
      await deck.loadQueue({rev: 21, delta: {base: 20, changed: [], removed: []}});
      const failedRev = deck.state.rev.queue;
      window.fetch = native;
      performance.clearResourceTimings();
      await deck.loadQueue({rev: 22, delta: {base: 21, changed: [], removed: []}});
      return {coalesced, failedRev, recovered: deck.state.rev.queue === 22,
        whole: performance.getEntriesByType('resource').some(r => r.name.endsWith('/api/queue?test_client=1'))};
    })()"#).await.unwrap();
    assert_eq!(
        result["coalesced"],
        serde_json::json!({"before": 1, "count": 2, "fallback": true, "rev": 20})
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
    let retry = browser.eval(&page, "performance.getEntriesByType('resource').some(r => r.name.endsWith('/api/queue?test_client=1'))").await.unwrap();
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
    let base = serve(
        &home,
        queue,
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
            "!!document.getElementById('alert')",
            Duration::from_secs(30),
        )
        .await
        .expect("page loaded");
    client_harness(&mut browser, &page).await;
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
}
