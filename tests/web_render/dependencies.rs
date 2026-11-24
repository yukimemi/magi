use super::{
    ClientProbe, DependencyRead, QuestionSeed, cdp, common, seed_questions, serve_with_client_probe,
};
use magi::queue::{Queue, Source, Task};
use magi::talk::Talks;
use serde_json::json;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(30);
const LOADING: &str = "document.querySelector('#dependencies-message')?.textContent.includes('Loading dependencies') && document.querySelector('#dependencies-graph').hidden";
const GRAPH: &str = "location.hash === '#/dependencies' && !document.querySelector('#view-dependencies')?.hidden && document.querySelectorAll('.dep-node').length === 19";
const QUESTION: &str = "20261121-140452-1111";
const FIRST_QUESTION: &str = "20261121-140453-2222";

async fn key(browser: &mut cdp::Browser, page: &cdp::Page, key: &str, vk: u32) {
    for kind in ["keyDown", "keyUp"] {
        browser
            .call(
                Some(&page.session),
                "Input.dispatchKeyEvent",
                json!({
                    "type": kind, "key": key, "code": key, "windowsVirtualKeyCode": vk,
                    "text": if key == "Enter" && kind == "keyDown" { "\r" } else { "" },
                }),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn dependencies_page_preserves_graph_navigation_and_load_boundaries() {
    let Some(chrome) = cdp::find_chrome() else {
        assert!(std::env::var_os("CI").is_none(), "Chrome required in CI");
        eprintln!("SKIP Dependencies: no Chrome");
        return;
    };
    let guard = common::home_lock().await;
    let fx = common::fixture(guard, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let queue = Queue::at(home.join("queue"));
    let (older_queue, older_gate) = tokio::sync::watch::channel(DependencyRead::Pass);
    let (questions, question_gate) = tokio::sync::watch::channel(DependencyRead::Wait);
    let base = serve_with_client_probe(
        &home,
        queue.clone(),
        Talks::at(home.join("talks")),
        home.join("runs"),
        &fx.repo,
        ClientProbe::Dependencies {
            older_queue: older_gate,
            questions: question_gate,
        },
    )
    .await;
    let mut browser = cdp::Browser::launch(&chrome).await.expect("Chrome");
    let url = format!("{base}#/dependencies");
    let empty = browser.open_page(&url, 390, 844, true).await.unwrap();
    browser.wait_for(&empty, LOADING, WAIT).await.unwrap();
    questions.send_replace(DependencyRead::Pass);
    browser
        .wait_for(&empty, "document.querySelector('#dependencies-message').textContent.includes('No task or question dependencies')", WAIT)
        .await
        .unwrap();
    browser.close_page(&empty).await;

    seed_questions(
        &home,
        &[
            QuestionSeed {
                id: FIRST_QUESTION,
                run: "",
                node: "test",
                seat: "test",
                summary: "Unrelated question comes first",
                asked_at: "2026-11-21T14:04:53Z",
                choices: &["yes"],
            },
            QuestionSeed {
                id: QUESTION,
                run: "",
                node: "test",
                seat: "test",
                summary: "Question dependency",
                asked_at: "2026-11-21T14:04:52Z",
                choices: &["yes"],
            },
        ],
    );
    let add_task = |title: String, dep: &str| {
        let mut task = Task::new(title, "Inspect".into(), fx.repo.clone(), Source::Human);
        task.block(vec![dep.into()], Some("Waiting for input".into()));
        queue.put(&mut task).unwrap();
        task
    };
    for i in 0..10 {
        add_task(format!("Parallel dependency {i}"), QUESTION);
    }
    let mut dep = QUESTION.to_string();
    for i in 0..7 {
        dep = add_task(format!("Chain link {i}"), &dep).id;
    }
    let title = "A long dependency title that wraps across multiple readable lines";
    let mut top = add_task(title.into(), &dep);

    older_queue.send_replace(DependencyRead::Wait);
    let phone = browser.open_page(&url, 390, 844, true).await.unwrap();
    browser
        .wait_for(
            &phone,
            &format!("{LOADING} && document.querySelector('#queue-load-state')?.textContent.includes('Loading older entries') && document.querySelectorAll('#questions-list .ask').length === 2"),
            WAIT,
        )
        .await
        .unwrap();
    older_queue.send_replace(DependencyRead::Fail);
    browser
        .wait_for(&phone, "document.querySelector('#dependencies-message').textContent.includes('Could not load queue') && document.querySelector('#dependencies-graph').hidden", WAIT)
        .await
        .unwrap();
    older_queue.send_replace(DependencyRead::Pass);
    let retry = "document.querySelector('#dependencies-retry').click()";
    browser.eval(&phone, retry).await.unwrap();
    browser.wait_for(&phone, GRAPH, WAIT).await.unwrap();

    browser
        .eval(
            &phone,
            r#"(() => {
        const pane = document.querySelector('#dependencies-graph');
        window.savedLink = pane.querySelector('.dep-node');
        savedLink.focus({ preventScroll: true });
        pane.scrollLeft = 180;
        pane.scrollTop = 90;
    })()"#,
        )
        .await
        .unwrap();
    browser.eval(&phone, "deck.loadQueue()").await.unwrap();
    assert_eq!(
        browser.eval(&phone, "savedLink.isConnected").await.unwrap(),
        true
    );
    questions.send_replace(DependencyRead::Fail);
    browser.eval(&phone, "deck.loadQuestions()").await.unwrap();
    browser
        .wait_for(&phone, "document.querySelector('#dependencies-message').textContent.includes('last successful snapshot') && savedLink.isConnected", WAIT)
        .await
        .unwrap();
    top.title = format!("{title} after refresh");
    queue.put(&mut top).unwrap();
    browser.eval(&phone, "deck.loadQueue()").await.unwrap();
    assert_eq!(
        browser.eval(&phone, "savedLink.isConnected").await.unwrap(),
        true
    );
    questions.send_replace(DependencyRead::Pass);
    browser.eval(&phone, retry).await.unwrap();
    browser
        .wait_for(&phone, "!savedLink.isConnected && document.activeElement.getAttribute('href') === savedLink.getAttribute('href') && document.querySelector('#dependencies-graph').scrollLeft === 180 && document.querySelector('#dependencies-graph').scrollTop === 90", WAIT)
        .await
        .unwrap();

    navigation(&mut browser, &phone, true, &top.id).await;
    browser.close_page(&phone).await;
    let desktop = browser.open_page(&url, 1280, 900, false).await.unwrap();
    navigation(&mut browser, &desktop, false, &top.id).await;
    browser.close_page(&desktop).await;
}

async fn navigation(browser: &mut cdp::Browser, page: &cdp::Page, mobile: bool, task_id: &str) {
    browser.wait_for(page, GRAPH, WAIT).await.unwrap();
    assert_eq!(
        browser.eval(page, r#"(() => {
            const pane = document.querySelector('#dependencies-graph');
            const node = pane.querySelector('.dep-node');
            const title = node.querySelector('.dep-node-title');
            const edge = pane.querySelector('.dep-edge');
            return !document.body.hasAttribute('data-split')
                && !document.querySelector('#view-queue .dep-graph')
                && document.title.includes('Dependencies')
                && document.querySelector('#dependencies-count').textContent === '18 tasks, 1 question · 18 dependencies'
                && node.textContent.includes('blocked')
                && title.offsetHeight > parseFloat(getComputedStyle(title).lineHeight)
                && edge.getPointAtLength(0).y < edge.getPointAtLength(edge.getTotalLength()).y
                && pane.scrollWidth > pane.clientWidth && pane.scrollHeight > pane.clientHeight
                && document.documentElement.scrollWidth === innerWidth;
        })()"#).await.unwrap(),
        true
    );
    let session = Some(page.session.as_str());
    let previous_load = browser.eval(page, "performance.timeOrigin").await.unwrap();
    browser
        .call(session, "Page.reload", json!({}))
        .await
        .unwrap();
    browser
        .wait_for(
            page,
            &format!("performance.timeOrigin !== {previous_load} && {GRAPH}"),
            WAIT,
        )
        .await
        .unwrap();
    browser
        .eval(page, "location.hash = '#/queue'")
        .await
        .unwrap();
    browser
        .wait_for(page, "!document.querySelector('#view-queue').hidden", WAIT)
        .await
        .unwrap();
    let nav = if mobile { ".dock" } else { ".rail" };
    browser
        .eval(
            page,
            &format!("document.querySelector('{nav} [data-nav=queue]').focus()"),
        )
        .await
        .unwrap();
    key(browser, page, "Tab", 9).await;
    assert_eq!(
        browser
            .eval(page, "document.activeElement.dataset.nav")
            .await
            .unwrap(),
        "dependencies"
    );
    key(browser, page, "Enter", 13).await;
    browser.wait_for(page, GRAPH, WAIT).await.unwrap();
    assert_eq!(browser.eval(page, &format!("document.querySelector('{nav} [data-nav=dependencies]').getAttribute('aria-current')")).await.unwrap(), "page");

    browser
        .eval(
            page,
            "document.querySelector('#dependencies-graph').focus()",
        )
        .await
        .unwrap();
    key(browser, page, "Tab", 9).await;
    let task_href = format!("#/tasks/{task_id}");
    assert_eq!(
        browser
            .eval(page, "document.activeElement.getAttribute('href')")
            .await
            .unwrap(),
        task_href
    );
    key(browser, page, "Enter", 13).await;
    let task_detail = format!(
        "location.hash === '{task_href}' && document.querySelector('#view-task').dataset.taskId === '{task_id}' && document.querySelector('#task-h').textContent.includes('after refresh')"
    );
    browser.wait_for(page, &task_detail, WAIT).await.unwrap();
    browser.eval(page, "history.back()").await.unwrap();
    browser.wait_for(page, GRAPH, WAIT).await.unwrap();
    browser.eval(page, "history.forward()").await.unwrap();
    browser.wait_for(page, &task_detail, WAIT).await.unwrap();
    browser.eval(page, "history.back()").await.unwrap();
    browser.wait_for(page, GRAPH, WAIT).await.unwrap();

    let question_href = format!("#/questions/{QUESTION}");
    browser
        .eval(
            page,
            &format!(r#"document.querySelector('.dep-node[href="{question_href}"]').focus()"#),
        )
        .await
        .unwrap();
    key(browser, page, "Enter", 13).await;
    let question_detail = format!(
        "location.hash === '{question_href}' && document.activeElement.closest('.ask')?.dataset.questionId === '{QUESTION}'"
    );
    if let Err(error) = browser.wait_for(page, &question_detail, WAIT).await {
        let actual = browser
            .eval(
                page,
                "[location.hash, document.activeElement.outerHTML.slice(0, 250)]",
            )
            .await;
        panic!("question navigation (mobile={mobile}): {error}; actual: {actual:?}");
    }
    assert_eq!(
        browser
            .eval(
                page,
                "document.querySelector('#questions-list .ask').dataset.questionId"
            )
            .await
            .unwrap(),
        FIRST_QUESTION
    );
    browser.eval(page, "history.back()").await.unwrap();
    browser.wait_for(page, GRAPH, WAIT).await.unwrap();

    let width = if mobile { 320 } else { 720 };
    browser
        .call(
            session,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width, "height": 844, "deviceScaleFactor": 1, "mobile": mobile,
            }),
        )
        .await
        .unwrap();
    browser
        .wait_for(page, &format!("innerWidth === {width}"), WAIT)
        .await
        .unwrap();
    assert_eq!(browser.eval(page, &format!(r#"Array.from(document.querySelectorAll('{nav} a')).every(a => {{
        const r = a.getBoundingClientRect();
        return r.left >= 0 && r.right <= innerWidth && r.height >= 44 && (!{mobile} || r.width >= 44);
    }})"#)).await.unwrap(), true, "navigation at {width}px");
}
