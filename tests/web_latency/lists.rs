async fn legacy_talks_for_measurement(
    State(ui): State<Arc<Ui>>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<TalkView>>> {
    blocking(move || {
        let mut configs: HashMap<PathBuf, Option<Config>> = HashMap::new();
        let talks = q.ids.as_ref().map_or_else(
            || ui.talks.list(),
            |ids| {
                ids.split(',')
                    .filter(|id| plain_record_id(id))
                    .filter_map(|id| ui.talks.get(id).ok())
                    .collect()
            },
        );
        let references = if talks.iter().any(id_links::has_references) {
            ui.references.get(
                &ui.runs,
                &ui.queue,
                &ui.questions,
                &ui.talks,
                talks.iter().any(id_links::has_findings),
            )
        } else {
            Arc::new(id_links::IdIndex::default())
        };
        Ok(Json(
            talks
                .into_iter()
                .filter(|talk| q.contains(&talk.id))
                .map(|talk| {
                    let thinking = ui.is_thinking(&talk.id);
                    let cfg = configs
                        .entry(talk.repo.clone())
                        .or_insert_with(|| Config::discover(&talk.repo, None).ok().map(|(c, _)| c));
                    TalkView::with_references(talk, thinking, cfg.as_ref(), &ui.home, &references)
                })
                .collect(),
        ))
    })
    .await
}

// Route-body measurement without TCP/Chrome. Legacy queue logic below is from
// origin/main 4436002a35dda29db2b12072554b769a1d138cba. The legacy talks
// helper preserves whole-response work from before summaries were bounded.
async fn legacy_queue_for_measurement(ui: Arc<Ui>) -> Vec<TaskView> {
    blocking(move || {
        let tasks = ui.queue.list();
        let inv = crate::blockers::Inventory::load(&ui.queue, &ui.questions);
        Ok(tasks
            .into_iter()
            .map(|task| TaskView::with_inventory(task, &inv))
            .collect())
    })
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "route-body list/dashboard measurement; see docs/web-responsiveness.md"]
async fn web_list_latency_measurement() {
    use std::time::Instant;
    let home = tempfile::tempdir().unwrap();
    let ui = delta_test_ui(home.path());
    let count = std::env::var("MAGI_BENCH_RECORDS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1000);
    let seed = seed_talk_at(&ui.talks, "seed", "open");
    let mut talk = ui.talks.get(&seed).unwrap();
    std::fs::remove_file(ui.talks.path_of(&seed)).unwrap();
    let mut run = RunState::new(
        PathBuf::from("/repo/magi"),
        "main".into(),
        "0000000".into(),
        "instruction ".repeat(2048),
        Config::default(),
    );
    run.status = crate::run::RunStatus::Merged;
    for i in 0..count {
        let id = format!("20261019-{i:06}-history");
        let mut task = Task::new(
            "Useful task".into(),
            "instruction ".repeat(2048),
            PathBuf::from("/repo/magi"),
            Source::Human,
        );
        task.id = id.clone();
        ui.queue.put(&mut task).unwrap();
        talk.id = id.clone();
        talk.pending = "history ".repeat(2048);
        std::fs::write(ui.talks.path_of(&id), serde_json::to_vec(&talk).unwrap()).unwrap();
        run.id = id;
        run.save_under(home.path()).unwrap();
    }
    let rounds = std::env::var("MAGI_BENCH_ROUNDS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(3);
    for route in ["queue", "talks", "stats"] {
        for baseline in [true, false] {
            for clients in [1, 4] {
                let mut samples = Vec::new();
                for _ in 0..rounds {
                    let mut jobs = Vec::new();
                    for _ in 0..clients {
                        let ui = ui.clone();
                        jobs.push(tokio::spawn(async move {
                            let start = Instant::now();
                            let mut first = None;
                            let mut bytes = 0;
                            if baseline {
                                bytes = match route {
                                    "queue" => {
                                        serde_json::to_vec(&legacy_queue_for_measurement(ui).await)
                                            .unwrap()
                                            .len()
                                    }
                                    "talks" => serde_json::to_vec(
                                        &legacy_talks_for_measurement(
                                            State(ui),
                                            Query(ListQuery {
                                                limit: None,
                                                ids: None,
                                            }),
                                        )
                                        .await
                                        .unwrap()
                                        .0,
                                    )
                                    .unwrap()
                                    .len(),
                                    _ => serde_json::to_vec(
                                        &stats_get(State(ui), Query(StatsQuery::default()))
                                            .await
                                            .unwrap()
                                            .0,
                                    )
                                    .unwrap()
                                    .len(),
                                };
                            } else if route == "stats" {
                                let (queue, history) = tokio::join!(
                                    async {
                                        let response =
                                            stats_queue(State(ui.clone())).await.unwrap();
                                        let size = serde_json::to_vec(&response.0).unwrap().len();
                                        (start.elapsed().as_secs_f64() * 1000.0, size)
                                    },
                                    stats_get(
                                        State(ui.clone()),
                                        Query(StatsQuery {
                                            runs_only: true,
                                            ..Default::default()
                                        })
                                    )
                                );
                                first = Some(queue.0);
                                bytes = queue.1
                                    + serde_json::to_vec(&history.unwrap().0).unwrap().len();
                            } else {
                                let mut cursor = None;
                                loop {
                                    let (size, next) = if route == "queue" {
                                        let page = queue_page(
                                            State(ui.clone()),
                                            Query(PageQuery { before: cursor }),
                                        )
                                        .await
                                        .unwrap()
                                        .0;
                                        (serde_json::to_vec(&page).unwrap().len(), page.next)
                                    } else {
                                        let page = talks_page(
                                            State(ui.clone()),
                                            Query(PageQuery { before: cursor }),
                                            header::HeaderMap::new(),
                                        )
                                        .await
                                        .unwrap()
                                        .0;
                                        (serde_json::to_vec(&page).unwrap().len(), page.next)
                                    };
                                    bytes += size;
                                    first.get_or_insert(start.elapsed().as_secs_f64() * 1000.0);
                                    cursor = next;
                                    if cursor.is_none() {
                                        break;
                                    }
                                }
                            }
                            let total = start.elapsed().as_secs_f64() * 1000.0;
                            (first.unwrap_or(total), total, bytes)
                        }));
                    }
                    for job in jobs {
                        samples.push(job.await.unwrap());
                    }
                }
                let mut first: Vec<_> = samples.iter().map(|s| s.0).collect();
                let mut total: Vec<_> = samples.iter().map(|s| s.1).collect();
                first.sort_by(f64::total_cmp);
                total.sort_by(f64::total_cmp);
                println!(
                    "LIST_LATENCY route={route} baseline={baseline} records={count} clients={clients} samples={} first_p50_ms={:.2} complete_p50_ms={:.2} bytes={}",
                    samples.len(),
                    first[first.len() / 2],
                    total[total.len() / 2],
                    samples[0].2
                );
            }
        }
    }
}

#[tokio::test]
async fn large_home_health_and_talks_bound_request_work() {
    let home = tempfile::tempdir().unwrap();
    let ui = delta_test_ui(home.path());
    for i in 0..925 {
        write_run_repo(
            &ui.runs,
            &format!("20261020-{i:06}-run"),
            RunStatus::Ready,
            "/repo",
        );
    }
    let seed = seed_talk_at(&ui.talks, "seed", "open");
    let mut talk = ui.talks.get(&seed).unwrap();
    std::fs::remove_file(ui.talks.path_of(&seed)).unwrap();
    let mut turn: talk::Turn = serde_json::from_value(
        serde_json::json!({ "who": "agent", "body": "reply", "at": talk.updated_at }),
    )
    .unwrap();
    turn.who = talk::Who::Agent;
    turn.body = "transcript ".repeat(410);
    talk.turns = vec![turn; 100];
    for i in 0..200 {
        talk.id = format!("20261020-{i:06}-talk");
        std::fs::write(
            ui.talks.path_of(&talk.id),
            serde_json::to_vec(&talk).unwrap(),
        )
        .unwrap();
    }
    // Model a disk measurement still in progress. Even cold requests must
    // return without joining it or starting another recursive traversal.
    ui.monitor.disk_cache.lock().unwrap().loading = true;
    let before = health(State(ui.clone())).await.unwrap().0;
    assert_eq!(before.runs_unreadable, 0);
    assert!(before.disk.is_none());
    assert_eq!(ui.monitor.health_index.lock().unwrap().run_parses, 925);
    let rows = talks_list(
        State(ui.clone()),
        Query(ListQuery {
            ids: None,
            limit: None,
        }),
        header::HeaderMap::new(),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(rows.len(), STORE_PAGE_SIZE);
    assert_eq!(ui.monitor.talk_index.lock().unwrap().len(), STORE_PAGE_SIZE);
    assert!(serde_json::to_vec(&rows).unwrap().len() < 100_000);
    assert!(
        rows.iter()
            .all(|row| row.turns.len() <= 2 && row.turn_count == 100 && row.unread_count == 100)
    );
    // A repeat must not deserialize a single unchanged run, nor more talks
    // than the page window, regardless of transcript/history volume.
    let _ = health(State(ui.clone())).await.unwrap();
    let _ = talks_list(
        State(ui.clone()),
        Query(ListQuery {
            ids: None,
            limit: None,
        }),
        header::HeaderMap::new(),
    )
    .await
    .unwrap();
    assert_eq!(ui.monitor.health_index.lock().unwrap().run_parses, 925);
    assert_eq!(ui.monitor.talk_index.lock().unwrap().len(), STORE_PAGE_SIZE);
    assert_eq!(
        ui.monitor
            .talk_parses
            .load(std::sync::atomic::Ordering::Relaxed),
        STORE_PAGE_SIZE
    );
    let path = ui.runs.join("20261020-000000-run/run.json");
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, "broken").unwrap();
    assert_eq!(
        health(State(ui.clone())).await.unwrap().0.runs_unreadable,
        1
    );
    std::fs::write(&path, original).unwrap();
    assert_eq!(
        health(State(ui.clone())).await.unwrap().0.runs_unreadable,
        0
    );
    assert_eq!(ui.monitor.health_index.lock().unwrap().run_parses, 927);
    // Delta/search batches contain up to 50 IDs, independently of the
    // smaller initial page window. Never silently drop the tail of a batch.
    let ids = (0..50)
        .map(|i| format!("20261020-{i:06}-talk"))
        .collect::<Vec<_>>()
        .join(",");
    let rows = talks_list(
        State(ui.clone()),
        Query(ListQuery {
            ids: Some(ids),
            limit: None,
        }),
        header::HeaderMap::new(),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(rows.len(), 50);
}

#[tokio::test]
async fn talk_summary_unread_counts_and_external_changes_are_exact() {
    let home = tempfile::tempdir().unwrap();
    let ui = delta_test_ui(home.path());
    let id = seed_talk_at(&ui.talks, "20261020-000000-talk", "open");
    let mut talk = ui.talks.get(&id).unwrap();
    let mut turn: talk::Turn = serde_json::from_value(
        serde_json::json!({ "who": "agent", "body": "reply", "at": talk.updated_at }),
    )
    .unwrap();
    turn.who = talk::Who::Agent;
    turn.at = talk.updated_at;
    talk.turns.push(turn.clone());
    ui.talks.put(&mut talk).unwrap();
    let rows = talk_rows(&ui, vec![id.clone()], &header::HeaderMap::new());
    assert_eq!(rows[0].unread_count, 1);
    let mut headers = header::HeaderMap::new();
    headers.insert(
        "x-talk-reads",
        serde_json::json!({id.clone(): turn.at})
            .to_string()
            .parse()
            .unwrap(),
    );
    assert_eq!(
        talk_rows(&ui, vec![id.clone()], &headers)[0].unread_count,
        0
    );
    turn.at = turn
        .at
        .checked_add(jiff::SignedDuration::from_secs(1))
        .unwrap();
    talk.turns.push(turn);
    ui.talks.put(&mut talk).unwrap();
    assert_eq!(
        talk_rows(&ui, vec![id.clone()], &headers)[0].unread_count,
        1
    );
    std::fs::write(ui.talks.path_of(&id), "broken").unwrap();
    assert!(talk_rows(&ui, vec![id.clone()], &headers).is_empty());
    std::fs::remove_file(ui.talks.path_of(&id)).unwrap();
    assert!(talk_rows(&ui, vec![id], &headers).is_empty());
}

#[tokio::test]
async fn conversation_core_does_not_queue_behind_bulk_readers() {
    let home = tempfile::tempdir().unwrap();
    let ui = delta_test_ui(home.path());
    let id = seed_talk_at(&ui.talks, "20261020-000000-talk", "open");
    let _all_bulk_slots = ui
        .monitor
        .bulk_slots
        .clone()
        .acquire_many_owned(2)
        .await
        .unwrap();
    let stalled = tokio::spawn(talks_page(
        State(ui.clone()),
        Query(PageQuery::default()),
        header::HeaderMap::new(),
    ));
    tokio::task::yield_now().await;
    assert!(!stalled.is_finished());
    let _ = tokio::time::timeout(Duration::from_secs(2), talk_core(State(ui), Path(id)))
        .await
        .unwrap()
        .unwrap();
    stalled.abort();
    assert!(stalled.await.err().unwrap().is_cancelled());
}
