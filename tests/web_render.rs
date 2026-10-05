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
const VIEWPORTS: [(u32, u32, bool); 2] = [(1280, 900, false), (390, 844, true)];

/// Assertions that are known to fail today because another task is already
/// fixing the layout they describe. Matched as `(page, check)`; `*` is any
/// page. The test reports a hit instead of failing, and says so loudly when an
/// entry no longer fires so it gets removed.
///
/// Keep this to the specific assertion: no page-wide exclusion, no `#[ignore]`.
const KNOWN_FAILURES: &[(&str, &str, &str)] = &[
    // Queue task-row layout: the row's title is missing or lands under the
    // chips (task 717c / 2e42, the two-line list rows).
    (
        "queue",
        "title-visible",
        "queue task rows, tasks 717c / 2e42",
    ),
    // Same layout: at desktop width the title and time of a row overflow the
    // list pane.
    (
        "queue",
        "pane-overflow",
        "queue task rows, tasks 717c / 2e42",
    ),
];

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

    let mut task_ids = Vec::new();
    for (title, status) in [
        ("Running task", TaskStatus::Running),
        ("Queued task", TaskStatus::Queued),
        ("Held task", TaskStatus::Held),
        ("Failed task", TaskStatus::Failed),
        (
            "A task whose title is long enough to wrap onto several lines on a narrow phone screen",
            TaskStatus::Queued,
        ),
    ] {
        let mut t = Task::new(
            title.to_owned(),
            format!("Instruction for: {title}"),
            fx.repo.clone(),
            Source::Human,
        );
        t.status = status;
        match status {
            // A run that exists, so the page does not chase a missing id.
            TaskStatus::Running => t.runs = vec![run_ids[0].clone()],
            TaskStatus::Held => t.hold_reason = Some("held by hand".to_owned()),
            TaskStatus::Failed => t.last_error = Some("the gate failed".to_owned()),
            _ => {}
        }
        queue.put(&mut t).expect("seed task");
        task_ids.push(t.id.clone());
    }

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
    let worktrees = home.join("wt").join("magi");
    std::fs::create_dir_all(&worktrees).unwrap();
    let ui = magi::web::Ui::new(
        queue,
        magi::ask::Questions::at(home.join("questions")),
        talks,
        runs_root,
        home.clone(),
        fx.repo.clone(),
    )
    .with_worktrees_root(worktrees);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind loopback");
    let base = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, ui.router()).await;
    });

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
