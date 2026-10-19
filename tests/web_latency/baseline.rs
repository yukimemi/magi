// Route bodies from origin/main af7f5e26453e533ba569f8c19dbe22c9cd7eeffb.
// Kept only for the ignored, reproducible latency comparison.
async fn baseline_talk_detail(
    State(ui): State<Arc<Ui>>,
    Path(id): Path<String>,
) -> ApiResult<Json<TalkDetailView>> {
    blocking(move || {
        let id = baseline_resolve_talk(&ui.talks, &id)?;
        let talk = ui.talks.get(&id)?;
        let thinking = ui.is_thinking(&talk.id);
        let tasks = talk::tasks_of(&ui.queue, &talk.id);
        let task_progress = talk_task_progress(&tasks);
        let tasks = tasks.into_iter().map(TaskView::from).collect();
        let cfg = Config::discover(&talk.repo, None).ok().map(|(cfg, _)| cfg);
        let roster = cfg
            .as_ref()
            .map(|cfg| {
                cfg.agents
                    .iter()
                    .map(|a| RosterEntry {
                        id: a.id.clone(),
                        kind: a.kind,
                        runnable: agent::installed(a),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let specs = cfg
            .as_ref()
            .map(|cfg| cfg.talk.personas.clone())
            .unwrap_or_default();
        let personas = persona::catalog(&specs)
            .into_iter()
            .map(|p| PersonaEntry {
                avatar: avatar_url(&ui.home, &talk.id, &p),
                id: p.id,
                name: p.name,
            })
            .collect();
        Ok(Json(TalkDetailView {
            view: TalkView::with_config(talk, thinking, cfg.as_ref(), &ui.home, &ui.references),
            tasks,
            task_progress,
            roster,
            personas,
        }))
    })
    .await
}

fn baseline_resolve_talk(store: &Talks, id: &str) -> ApiResult<String> {
    pick(store.list().into_iter().map(|t| t.id).collect(), id, "talk")
}
