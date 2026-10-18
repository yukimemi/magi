// Route-body measurement without TCP/Chrome. Legacy queue logic below is from
// origin/main 4436002a35dda29db2b12072554b769a1d138cba. Unpaged talks_list and
// stats_get with runs_only=false retain that revision's whole-response work.
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
                                        &talks_list(
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
