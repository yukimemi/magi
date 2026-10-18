//! Opt-in end-to-end measurement; run the identical driver against two source
//! revisions with tests/web_latency/compare.py. No production stores are read.
#[path = "web_render/cdp.rs"]
mod cdp;
#[allow(unused_macros, unused_imports)]
mod common;

use magi::{
    ask::{Question, Questions},
    queue::{Queue, Source, Task},
    run::{RunState, RunStatus},
    talk::{self, Talks},
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

// This test process contains the server and CDP driver; Chrome is a separate
// child and excluded. Idle samples do no browser evaluation until the end.
#[cfg(unix)]
fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the supplied correctly sized rusage on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    // SAFETY: the preceding successful getrusage initialized usage.
    let usage = unsafe { usage.assume_init() };
    (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
}

const PROBE: &str = r#"(() => {
  const b = window.latency = {active: 0, last: 0, first: null, done: null, errors: [], sse: 0};
  const fetch0 = window.fetch;
  window.fetch = async (...args) => {
    b.active++;
    let finished = false;
    const finish = () => { if (!finished) { finished = true; b.active--; b.last = performance.now(); } };
    try {
      const response = await fetch0(...args);
      if (!response.ok) b.errors.push(`${args[0]}: ${response.status}`);
      for (const method of ['json', 'text']) {
        const read = response[method].bind(response);
        response[method] = async () => { try { return await read(); } finally { finish(); } };
      }
      return response;
    } catch (e) { b.errors.push(String(e)); finish(); throw e; }
  };
  const ES = window.EventSource;
  window.EventSource = class extends ES {
    constructor(...args) { super(...args); this.addEventListener('open', () => b.sse++); }
  };
  const ready = () => {
    const selector = __SELECTOR__;
    if (selector === '#run-instruction') {
      const panel = document.querySelector('#run-task-panel');
      if (panel) panel.open = true;
    }
    return [...document.querySelectorAll(selector)].some(e => e.getClientRects().length
      && e.textContent.trim() && !e.textContent.includes('Loading'));
  };
  const frame = () => {
    if (b.first === null && ready()) {
      // One further frame provides a paint opportunity for the useful DOM.
      requestAnimationFrame(() => { if (b.first === null && ready()) b.first = performance.now(); });
    }
    if (b.first !== null && b.active === 0 && b.completeFor !== b.last) {
      b.completeFor = b.last;
      b.complete = performance.now();
    }
    if (b.first !== null && b.active === 0 && b.sse && performance.now() - b.last > 200) {
      b.done = {first_ms: b.first, complete_ms: b.complete, errors: b.errors, sse: b.sse};
    }
    if (!b.done) requestAnimationFrame(frame);
  };
  requestAnimationFrame(frame);
})()"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real HTTP/Chrome benchmark; see docs/web-responsiveness.md"]
async fn browser_history_latency() {
    let count: usize = std::env::var("MAGI_BENCH_RECORDS")
        .unwrap_or("1000".into())
        .parse()
        .unwrap();
    let rounds: usize = std::env::var("MAGI_BENCH_ROUNDS")
        .unwrap_or("5".into())
        .parse()
        .unwrap();
    let chrome = cdp::find_chrome().expect("Chrome required for measurement");
    // Fail promptly under a sandbox that prohibits serving HTTP.
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("benchmark requires loopback HTTP permission");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let talks = Talks::at(home.join("talks"));
    let questions = Questions::at(home.join("questions"));
    let mut chat = talk::begin(
        &talks,
        &fx.config,
        fx.repo.clone(),
        Some(&fx.config.agents[0].id),
    )
    .unwrap();
    talk::record(&mut chat, &talks, "Useful conversation transcript", vec![]).unwrap();
    let mut run = RunState::new(
        fx.repo.clone(),
        "main".into(),
        "0000000".into(),
        "Useful run instructions".into(),
        fx.config.clone(),
    );
    run.status = RunStatus::Blocked;
    run.save_under(&home).unwrap();
    let mut task = Task::new(
        "Useful task".into(),
        "Useful task instructions".into(),
        fx.repo.clone(),
        Source::Agent {
            run: chat.id.clone(),
            node: "chat".into(),
        },
    );
    task.runs = vec![run.id.clone()];
    queue.put(&mut task).unwrap();
    let target_task = task.id.clone();
    let talk_id = chat.id.clone();
    for i in 0..count {
        task.id = format!("20261019-{i:06}-history");
        task.instruction = "instruction ".repeat(2048);
        task.runs.clear();
        queue.put(&mut task).unwrap();
        chat.id = format!("20261019-{i:06}-history");
        chat.pending = "history ".repeat(2048);
        std::fs::write(talks.path_of(&chat.id), serde_json::to_vec(&chat).unwrap()).unwrap();
        run.id = format!("20261019-{i:06}-history");
        run.status = RunStatus::Merged;
        run.save_under(&home).unwrap();
        let mut question = Question::new(
            run.id.clone(),
            "review".into(),
            "seat".into(),
            "Useful question".into(),
            "Details".into(),
            vec![],
        );
        questions.put(&mut question).unwrap();
    }
    let task = queue.get(&target_task).unwrap();
    let run_id = &task.runs[0];
    let ui = magi::web::Ui::new(
        queue,
        questions,
        talks,
        home.join("runs"),
        home.clone(),
        fx.repo.clone(),
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let router = ui.router().layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                next.run(request).await
            }
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let routes = [
        (format!("#/chat/{talk_id}"), "#talk-turns .turn-text"),
        (format!("#/tasks/{target_task}"), "#task-instruction"),
        (format!("#/runs/{run_id}"), "#run-instruction"),
        (
            format!("#/runs/{run_id}/report"),
            "#run-report-cards .rcard, #run-report",
        ),
        (
            "#/queue".into(),
            "#queue-sections li.card:not(.skeleton) .card-title",
        ),
        ("#/runs".into(), "#view-runs a.card .card-title"),
        ("#/chat".into(), "#talks-list a.card .card-title"),
        ("#/questions".into(), "#questions-list .ask-summary"),
        ("#/stats".into(), "#stats-kpis"),
    ];
    for clients in [1, 4] {
        // One browser per client avoids hidden-tab rAF throttling.
        let mut browsers = Vec::new();
        for _ in 0..clients {
            browsers.push(cdp::Browser::launch(&chrome).await.expect("Chrome launch"));
        }
        for (route, selector) in &routes {
            for round in 0..rounds {
                let mut pages = Vec::new();
                for browser in &mut browsers {
                    let page = browser
                        .open_page("about:blank", 1280, 900, false)
                        .await
                        .unwrap();
                    browser.call(Some(&page.session), "Page.addScriptToEvaluateOnNewDocument", json!({"source": PROBE.replace("__SELECTOR__", &json!(selector).to_string())})).await.unwrap();
                    pages.push(page);
                }
                // Issue navigation commands without waiting for any page's content.
                for (browser, page) in browsers.iter_mut().zip(&pages) {
                    browser
                        .call(
                            Some(&page.session),
                            "Page.navigate",
                            json!({"url": format!("{base}/{route}")}),
                        )
                        .await
                        .unwrap();
                }
                for (browser, page) in browsers.iter_mut().zip(&pages) {
                    browser
                        .wait_for(page, "!!window.latency?.done", Duration::from_secs(120))
                        .await
                        .unwrap();
                    let result = browser.eval(page, "window.latency.done").await.unwrap();
                    assert_eq!(result["errors"], json!([]), "failed requests: {result}");
                    let events = browser.take_events(&page.session);
                    assert!(
                        !events
                            .iter()
                            .any(|event| event["method"] == "Runtime.exceptionThrown"),
                        "browser exception: {events:?}"
                    );
                    eprintln!(
                        "BROWSER_LATENCY {}",
                        json!({"records_per_store":count,"clients":clients,"route":route,"round":round,"timing":result})
                    );
                }
                for (browser, page) in browsers.iter_mut().zip(&pages) {
                    browser.close_page(page).await;
                }
            }
        }
        let mut pages = Vec::new();
        for browser in &mut browsers {
            let page = browser
                .open_page("about:blank", 1280, 900, false)
                .await
                .unwrap();
            browser.call(Some(&page.session), "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": PROBE.replace("__SELECTOR__", &json!("#questions-list .ask-summary").to_string())})).await.unwrap();
            browser
                .call(
                    Some(&page.session),
                    "Page.navigate",
                    json!({"url": format!("{base}/#/questions")}),
                )
                .await
                .unwrap();
            browser
                .wait_for(&page, "!!window.latency?.done", Duration::from_secs(120))
                .await
                .unwrap();
            pages.push(page);
        }
        // Let initial reads settle; persistent EventSource connections stay open.
        tokio::time::sleep(Duration::from_secs(2)).await;
        #[cfg(unix)]
        let cpu_before = cpu_seconds();
        let before = requests.load(Ordering::Relaxed);
        let start = Instant::now();
        tokio::time::sleep(Duration::from_secs(10)).await;
        #[cfg(unix)]
        eprintln!(
            "SSE_LOAD {}",
            json!({"clients":clients,"wall_s":start.elapsed().as_secs_f64(),"server_driver_cpu_s":cpu_seconds()-cpu_before,"http_requests":requests.load(Ordering::Relaxed)-before})
        );
        for (browser, page) in browsers.iter_mut().zip(&pages) {
            browser.close_page(page).await;
        }
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
