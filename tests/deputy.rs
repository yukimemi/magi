//! A conductor's question has a deputy: a seat of its own that reads the
//! owner's free-text reply and answers back, survives a restart by resuming
//! the same conversation, and stops when the question's time runs out.
mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{Fixture, HomeGuard, Judges, fixture, home_lock};
use jiff::Timestamp;
use magi::ask::{Deputy, Question, QuestionStatus, Questions, WaiterKind, Who};
use magi::deputy::{Deputies, MAX_STARTS};
use magi::waiter::Waiter;

const TASK: &str = "20260101-000001-task";

struct Scene {
    fx: Fixture,
    home: PathBuf,
    store: Questions,
    q: Question,
}

fn file(store: &Questions, fx: &Fixture, summary: &str) -> Question {
    let mut q = Question::new(
        TASK.to_owned(),
        magi::conduct::NODE.to_owned(),
        "conduct".to_owned(),
        summary.to_owned(),
        "the conductor's reasoning".to_owned(),
        vec![
            "operator: setup is done".to_owned(),
            "operator: skip it".to_owned(),
        ],
    );
    q.answer_timeout = 3600;
    q.cwd = Some(fx.repo.to_string_lossy().into_owned());
    q.deputy = Some(Deputy::new(magi::deputy::brief(
        TASK,
        "the build needs a manual setup step",
        &q.choices,
        &q.actions,
    )));
    store.put(&mut q).expect("file the question");
    q
}

fn scene(home: HomeGuard) -> Scene {
    let fx = fixture(home, Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let store = Questions::at(home.join("questions"));
    let q = file(&store, &fx, "Is the setup done?");
    Scene { fx, home, store, q }
}

fn deputies(s: &Scene, max: usize) -> Deputies {
    Deputies::new(
        s.store.clone(),
        s.home.clone(),
        Some(s.fx.config.clone()),
        s.fx.repo.clone(),
        max,
        Arc::new(|| false),
    )
}

fn log(s: &Scene) -> Vec<String> {
    std::fs::read_to_string(s.fx.repo.join("deputy.log"))
        .map(|l| l.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

async fn turn(d: &mut Deputies) {
    d.tick(Timestamp::now());
    d.drain().await;
}

common::e2e! {
#[cfg(unix)]
async fn failed_deputy_notices_describe_exit_status_without_debug_values() {
    for (command, status) in [
        ("cat >/dev/null; exit 1", "exit code 1"),
        ("cat >/dev/null; kill -TERM $$", "exit code unavailable"),
    ] {
        for sessions in [false, true] {
            let mut s = scene(home_lock().await);
            s.fx.config.graph.sessions = sessions;
            for agent in &mut s.fx.config.agents {
                agent.command = vec!["sh".into(), "-c".into(), command.into()];
            }
            let mut d = deputies(&s, 1);
            turn(&mut d).await;
            let notices = magi::notices::Notices::at(s.home.join("notifications"));
            let id = magi::notices::id_of(&format!("deputy-turn:{}", s.q.id));
            let notice = notices.get(&id).unwrap();
            assert!(notice.message.contains(&format!("its turn failed ({status})")), "{}", notice.message);
            assert!(!notice.message.contains("Some(") && !notice.message.contains("None"));
            assert_eq!(notice.severity, magi::notices::Severity::Warn);
            let question = s.store.get(&s.q.id).unwrap();
            assert_eq!(question.status, QuestionStatus::Open);
            assert_eq!(question.choices, s.q.choices);
        }
    }
}
}

common::e2e! {
async fn exhausted_deputies_do_not_create_or_increment_notifications() {
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| {
        q.deputy.as_mut().unwrap().starts = MAX_STARTS;
        q.say("are you there?")
    }).unwrap();
    let notices = magi::notices::Notices::at(s.home.join("notifications"));
    let key = format!("deputy:{}", s.q.id);
    let id = magi::notices::id_of(&key);
    let mut d = deputies(&s, 2);
    for _ in 0..3 { turn(&mut d).await; }
    assert!(notices.get(&id).is_err(), "no new warning");
    let old = notices.raise(magi::notices::Notice::warn(&key, "legacy warning")).unwrap();
    let mut restarted = deputies(&s, 2);
    for _ in 0..3 { turn(&mut restarted).await; }
    let after = notices.get(&id).unwrap();
    assert_eq!(after.count, old.count);
    assert_eq!(after.last_at, old.last_at);
    assert!(log(&s).is_empty(), "retry limit still holds");
    let q = s.store.get(&s.q.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Open);
    assert_eq!(q.choices, s.q.choices);
    assert_eq!(q.deputy.unwrap().starts, MAX_STARTS);
    s.store.update(&s.q.id, |q| q.answer(magi::ask::Answer::Choice(q.choices[0].clone()))).unwrap();
    assert_eq!(s.store.get(&s.q.id).unwrap().status, QuestionStatus::Answered);
}
}

common::e2e! {
async fn a_free_text_reply_reaches_the_deputy_and_is_answered_back() {
    let s = scene(home_lock().await);
    let mut d = deputies(&s, 2);
    s.store.update(&s.q.id, |q| q.say("setup done")).unwrap();

    turn(&mut d).await;

    assert_eq!(log(&s).len(), 1, "one deputy turn: {:?}", log(&s));
    let q = s.store.get(&s.q.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Open, "a say never settles a question");
    assert_eq!(q.delivered_turns, q.thread.len(), "the word was read");
    let last = q.thread.last().unwrap();
    assert_eq!(last.who, Who::Agent);
    assert!(last.body.contains("mock deputy reply from deputy-"), "{}", last.body);
    assert!(!q.waiting_on_agent(), "the ball is back with the owner");
    assert!(q.waiter.is_none());
    let dep = q.deputy.as_ref().unwrap();
    assert_eq!(dep.starts, 1);
    let seat = dep.seat.as_ref().expect("the seat is persisted");
    assert_eq!(seat.key, magi::ask::deputy_seat_key(&q.id), "keyed by seat, not by agent");
    assert_eq!(seat.turns, 2, "a handover turn, then the real one");

    // What the deputy was told: the question, the conductor's reasoning, the
    // owner's word.
    let dir = s.store.root().join(format!("{}.deputy", s.q.id));
    let prompt = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".prompt.md"))
        .expect("a prompt artifact");
    let prompt = std::fs::read_to_string(prompt).unwrap();
    assert!(prompt.contains("Is the setup done?"), "{prompt}");
    assert!(prompt.contains("the build needs a manual setup step"), "{prompt}");
    assert!(prompt.contains("setup done"), "{prompt}");
    assert!(prompt.contains("operator: skip it"), "{prompt}");
}
}

common::e2e! {
async fn a_restart_resumes_the_same_deputy_seat() {
    let s = scene(home_lock().await);
    let mut d = deputies(&s, 2);
    turn(&mut d).await;
    let first = s.store.get(&s.q.id).unwrap().deputy.unwrap();
    assert_eq!(log(&s), [format!("{} started", first.seat.as_ref().unwrap().key)]);

    // A new process: nothing in memory, only what is on the question.
    s.store.drop_lease(&s.q.id);
    s.store.update(&s.q.id, |q| q.say("are you there?")).unwrap();
    let mut restarted = deputies(&s, 2);
    turn(&mut restarted).await;

    let after = s.store.get(&s.q.id).unwrap().deputy.unwrap();
    let (a, b) = (first.seat.unwrap(), after.seat.unwrap());
    assert_eq!(b.key, a.key);
    assert_eq!(b.claude_session, a.claude_session, "the same conversation, not a new one");
    assert_eq!(b.turns, 3, "resumed, not handed over again");
    assert_eq!(after.starts, 2);
    assert_eq!(log(&s)[1], format!("{} resumed", a.key));
}
}

common::e2e! {
async fn a_seat_that_lost_its_session_starts_over_with_the_whole_context() {
    let s = scene(home_lock().await);
    let mut d = deputies(&s, 2);
    turn(&mut d).await;
    s.store.drop_lease(&s.q.id);
    // Turns recorded but the CLI conversation gone: never assume memory.
    s.store
        .update(&s.q.id, |q| {
            q.deputy.as_mut().unwrap().seat.as_mut().unwrap().turns = 0;
            Ok(())
        })
        .unwrap();
    let mut restarted = deputies(&s, 2);
    turn(&mut restarted).await;
    assert!(log(&s)[1].ends_with("started"), "{:?}", log(&s));
}
}

common::e2e! {
async fn an_unanswered_question_retires_without_a_deputy_and_stays_retired() {
    let s = scene(home_lock().await);
    s.store
        .update(&s.q.id, |q| {
            q.answer_timeout = 60;
            q.asked_at = Timestamp::from_second(Timestamp::now().as_second() - 3600).unwrap();
            Ok(())
        })
        .unwrap();
    let mut d = deputies(&s, 2);
    turn(&mut d).await;
    assert!(log(&s).is_empty(), "no deputy for a question past its deadline");

    // The waiter retires it, exactly as it does any question nobody waits on.
    let mut waiter = Waiter::new(s.store.clone(), s.home.clone(), None);
    waiter.tick(Timestamp::now(), &|| false).await;
    let q = s.store.get(&s.q.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Abandoned);

    turn(&mut d).await;
    assert!(log(&s).is_empty(), "and nothing starts on a retired question");
}
}

common::e2e! {
async fn the_waiter_leaves_a_deputys_question_to_the_deputy() {
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| q.say("setup done")).unwrap();
    let mut waiter = Waiter::new(s.store.clone(), s.home.clone(), None);
    waiter.tick(Timestamp::now(), &|| false).await;
    assert!(log(&s).is_empty());
    assert_eq!(s.store.get(&s.q.id).unwrap().delivered_turns, 0, "nobody resumed the conductor's seat");
}
}

common::e2e! {
async fn a_live_lease_the_cap_and_the_start_bound_all_hold_a_deputy_back() {
    let s = scene(home_lock().await);
    let other = file(&s.store, &s.fx, "A second question?");
    // At most one at once: of two open questions, only one starts.
    let mut one = deputies(&s, 1);
    turn(&mut one).await;
    assert_eq!(log(&s).len(), 1, "{:?}", log(&s));
    // The next tick, with the first done, picks up the other.
    s.store.drop_lease(&s.q.id);
    s.store.drop_lease(&other.id);
    one.tick(Timestamp::now());
    one.drain().await;
    assert_eq!(log(&s).len(), 2, "{:?}", log(&s));

    // A fresh lease means somebody is already on it.
    let mut d = deputies(&s, 2);
    s.store.beat(&s.q.id, WaiterKind::Asker);
    s.store.beat(&other.id, WaiterKind::Deputy);
    turn(&mut d).await;
    assert_eq!(log(&s).len(), 2);

    // And a question whose deputy kept ending is not restarted forever.
    s.store.drop_lease(&s.q.id);
    s.store.drop_lease(&other.id);
    for id in [&s.q.id, &other.id] {
        s.store
            .update(id, |q| {
                q.deputy.as_mut().unwrap().starts = MAX_STARTS;
                Ok(())
            })
            .unwrap();
    }
    let mut again = deputies(&s, 2);
    turn(&mut again).await;
    assert_eq!(log(&s).len(), 2, "bounded");
}
}

common::e2e! {
async fn the_seat_is_persisted_by_a_handover_turn_before_the_long_wait() {
    let s = scene(home_lock().await);
    let mut d = deputies(&s, 2);
    turn(&mut d).await;
    let handovers = std::fs::read_to_string(s.fx.repo.join("handover.log")).unwrap();
    assert_eq!(handovers.lines().count(), 1);
    let seat = s.store.get(&s.q.id).unwrap().deputy.unwrap().seat.unwrap();
    assert!(seat.turns >= 1, "a session exists to resume even if the wait is interrupted");
}
}

common::e2e! {
async fn a_stale_claim_from_a_dead_daemon_does_not_block_a_start() {
    let s = scene(home_lock().await);
    let claim = s.store.root().join(format!("{}.deputy-claim", s.q.id));
    std::fs::write(&claim, "").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(300);
    std::fs::File::options().write(true).open(&claim).unwrap().set_modified(old).unwrap();
    let mut d = deputies(&s, 2);
    turn(&mut d).await;
    assert_eq!(log(&s).len(), 1);
}
}

common::e2e! {
async fn a_spent_deputy_does_not_keep_an_expired_question_open() {
    let s = scene(home_lock().await);
    s.store
        .update(&s.q.id, |q| {
            q.answer_timeout = 60;
            q.say("anyone there?")?;
            // The owner's last word is long past the deadline, and unread.
            q.thread[0].at = Timestamp::from_second(Timestamp::now().as_second() - 3600).unwrap();
            q.asked_at = q.thread[0].at;
            q.deputy.as_mut().unwrap().starts = MAX_STARTS;
            Ok(())
        })
        .unwrap();
    let mut waiter = Waiter::new(s.store.clone(), s.home.clone(), None);
    waiter.tick(Timestamp::now(), &|| false).await;
    assert_eq!(s.store.get(&s.q.id).unwrap().status, QuestionStatus::Abandoned);
}
}

/// An unread say far past the deadline, with no deputy ever started.
fn age_unread(s: &Scene) {
    s.store
        .update(&s.q.id, |q| {
            q.answer_timeout = 60;
            q.say("anyone there?")?;
            q.thread[0].at = Timestamp::from_second(Timestamp::now().as_second() - 3600).unwrap();
            q.asked_at = q.thread[0].at;
            assert_eq!(q.deputy.as_ref().unwrap().starts, 0);
            Ok(())
        })
        .unwrap();
}

common::e2e! {
async fn an_unread_say_cannot_keep_a_question_open_when_no_deputy_can_start() {
    let s = scene(home_lock().await);
    age_unread(&s);
    let mut off = s.fx.config.clone();
    off.daemon.max_deputies = 0;
    let mut waiter = Waiter::new(s.store.clone(), s.home.clone(), Some(off));
    waiter.tick(Timestamp::now(), &|| false).await;
    assert_eq!(s.store.get(&s.q.id).unwrap().status, QuestionStatus::Abandoned);
}
}

common::e2e! {
async fn an_unread_say_cannot_keep_a_question_open_when_the_config_is_unavailable() {
    let s = scene(home_lock().await);
    age_unread(&s);
    let mut waiter = Waiter::new(s.store.clone(), s.home.clone(), None);
    waiter.tick(Timestamp::now(), &|| false).await;
    assert_eq!(s.store.get(&s.q.id).unwrap().status, QuestionStatus::Abandoned);
}
}

common::e2e! {
async fn a_live_deputy_within_its_deadline_keeps_the_question_open() {
    let s = scene(home_lock().await);
    s.store
        .update(&s.q.id, |q| q.say("still here"))
        .unwrap();
    s.store.beat(&s.q.id, WaiterKind::Deputy);
    let mut waiter = Waiter::new(s.store.clone(), s.home.clone(), Some(s.fx.config.clone()));
    waiter.tick(Timestamp::now(), &|| false).await;
    assert_eq!(s.store.get(&s.q.id).unwrap().status, QuestionStatus::Open);
}
}

common::e2e! {
async fn deputies_do_not_start_when_disabled() {
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| q.say("hello")).unwrap();
    let mut d = deputies(&s, 0);
    turn(&mut d).await;
    assert!(log(&s).is_empty());
}
}

common::e2e! {
async fn an_unresolvable_deputy_agent_cannot_keep_a_question_open() {
    let s = scene(home_lock().await);
    age_unread(&s);
    let mut bad = s.fx.config.clone();
    bad.agents.clear();
    assert!(!magi::deputy::can_start(Some(&bad), ""));
    let mut waiter = Waiter::new(s.store.clone(), s.home.clone(), Some(bad));
    waiter.tick(Timestamp::now(), &|| false).await;
    assert_eq!(s.store.get(&s.q.id).unwrap().status, QuestionStatus::Abandoned);
}
}

fn file_land(store: &Questions, summary: &str) -> Question {
    let mut q = Question::new(
        "20260101-000001-run".to_owned(),
        magi::land::APPROVAL_NODE.to_owned(),
        "land".to_owned(),
        summary.to_owned(),
        "https://example.test/pull/7 is green".to_owned(),
        vec![magi::land::APPROVE.to_owned(), magi::land::HOLD.to_owned()],
    );
    store.put(&mut q).expect("file the approval");
    q
}

common::e2e! {
async fn a_say_on_a_merge_approval_gets_a_deputy_and_never_the_waiter() {
    let s = scene(home_lock().await);
    let land = file_land(&s.store, "Merge #7?");
    s.store.update(&land.id, |q| q.say("what changed?")).unwrap();
    let mut d = deputies(&s, 2);
    // Only the approval is under test: retire the conductor's question.
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();

    turn(&mut d).await;

    let q = s.store.get(&land.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Open, "a say never settles the approval");
    assert!(q.cwd.is_none(), "a cwd would make it the waiter's");
    assert!(q.answer_timeout > 0);
    let dep = q.deputy.as_ref().expect("a deputy was attached");
    assert_eq!(dep.starts, 1);
    assert!(dep.brief.contains("snapshot"), "{}", dep.brief);
    assert!(dep.brief.contains("could not be read"), "no run record here: {}", dep.brief);
    assert_eq!(q.thread.last().unwrap().who, Who::Agent, "the say was answered");
    assert_eq!(q.delivered_turns, q.thread.len());

    // The waiter never resumes it, with or without a deputy.
    s.store.update(&land.id, |q| q.say("and the tests?")).unwrap();
    let before = log(&s).len();
    let mut waiter = Waiter::new(s.store.clone(), s.home.clone(), None);
    waiter.tick(Timestamp::now(), &|| false).await;
    assert_eq!(log(&s).len(), before);
    assert_eq!(s.store.get(&land.id).unwrap().status, QuestionStatus::Open);
}
}

/// What the mock deputy runs on its real turn: the actual CLI, as the seat
/// would, against this scene's home.
fn script(s: &Scene, lines: &[&str]) {
    let bin = env!("CARGO_BIN_EXE_magi");
    let mut body = format!("export MAGI_HOME='{}'\n", s.home.display());
    body.push_str(": > deputy-action-status\n: > deputy-action-output\n");
    for l in lines {
        body.push_str("if {\n");
        body.push_str(&l.replace("MAGI", &format!("'{bin}'")));
        body.push_str(
            "\n} >> deputy-action-output 2>&1; then\n\
             echo 0 >> deputy-action-status\n\
             else\n\
             echo \"$?\" >> deputy-action-status\n\
             fi\n",
        );
    }
    std::fs::write(s.fx.repo.join("deputy-actions.sh"), body).unwrap();
}

/// Check the nested CLI itself before asserting its persisted effects. The
/// mock continues after expected refusals, but successful-action tests must
/// report the CLI's real failure rather than only a missing answer or task.
fn assert_actions_succeeded(s: &Scene, count: usize) {
    let statuses = std::fs::read_to_string(s.fx.repo.join("deputy-action-status"))
        .expect("the deputy must run its action script");
    let output = std::fs::read_to_string(s.fx.repo.join("deputy-action-output")).unwrap();
    let statuses: Vec<_> = statuses.lines().collect();
    assert_eq!(statuses.len(), count, "not all CLI commands ran: {output}");
    assert!(
        statuses.iter().all(|status| *status == "0"),
        "nested CLI exit statuses {statuses:?}:\n{output}"
    );
}

const FOLLOW_UP: &str = "MAGI task add --hold 'applies after the pull request merges' \
     --title 'Fix R1-1' 'Applies after the pull request merges. R1-1 at src/a.rs:10: the \
     loop skips the last item; fix it and add a test that covers the final element.'";

fn follow_ups(s: &Scene) -> Vec<magi::queue::Task> {
    magi::queue::Queue::at(s.home.join("queue")).list()
}

common::e2e! {
async fn a_clear_merge_with_a_follow_up_request_merges_and_files_the_tasks() {
    let s = scene(home_lock().await);
    let land = file_land(&s.store, "Merge #7?");
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    let say = "マージしていいよ。残りのレビュー指摘はフォローアップタスクとして積んで";
    s.store.update(&land.id, |q| q.say(say)).unwrap();
    script(&s, &[
        FOLLOW_UP,
        "MAGI ask --settle \"$qid\" --choice merge --quote 'マージしていいよ'",
    ]);

    turn(&mut deputies(&s, 2)).await;
    assert_actions_succeeded(&s, 2);

    let q = s.store.get(&land.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Answered, "{:?}", q.thread);
    assert_eq!(q.resolution().as_deref(), Some("merge"));
    let tasks = follow_ups(&s);
    assert_eq!(tasks.len(), 1, "one follow-up filed: {tasks:?}");
    let t = &tasks[0];
    assert_eq!(t.status, magi::queue::TaskStatus::Held, "it must not run before the merge");
    assert!(t.hold_reason.as_deref().unwrap().contains("after the pull request merges"));
    assert_eq!(
        t.source,
        magi::queue::Source::Agent { run: land.run.clone(), node: "deputy".to_owned() }
    );
}
}

common::e2e! {
async fn a_non_verbatim_quote_is_refused_and_the_question_stays_open() {
    let s = scene(home_lock().await);
    let land = file_land(&s.store, "Merge #7?");
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    s.store.update(&land.id, |q| q.say("マージしていいよ")).unwrap();
    script(&s, &[
        "MAGI ask --settle \"$qid\" --choice merge --quote 'ぜひマージしてください'",
    ]);

    turn(&mut deputies(&s, 2)).await;

    let q = s.store.get(&land.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Open, "an invented quote is no basis: {:?}", q.thread);
    assert!(q.answer.is_none());
    assert!(follow_ups(&s).is_empty());
}
}

common::e2e! {
async fn a_settle_note_naming_the_filed_task_lands_on_the_thread() {
    let s = scene(home_lock().await);
    let land = file_land(&s.store, "Merge #7?");
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    let say = "マージしていいよ。残りのレビュー指摘はフォローアップタスクとして積んで";
    s.store.update(&land.id, |q| q.say(say)).unwrap();
    script(&s, &[
        FOLLOW_UP,
        "MAGI ask --settle \"$qid\" --choice merge --quote 'マージしていいよ' \
         --note 'Filed Fix R1-1 (held until `magi task release <id>`).'",
    ]);

    turn(&mut deputies(&s, 2)).await;
    assert_actions_succeeded(&s, 2);

    let q = s.store.get(&land.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Answered, "{:?}", q.thread);
    let turn = q.thread.last().unwrap();
    assert!(turn.body.starts_with("Settled as `merge`"));
    assert!(turn.note.as_deref().unwrap().contains("Fix R1-1"), "{:?}", turn.note);
}
}

common::e2e! {
async fn a_release_merge_settle_can_say_the_follow_ups_were_not_queued() {
    let s = scene(home_lock().await);
    let land = file_land(&s.store, "Merge #7?");
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    s.store.update(&land.id, |q| q.say("マージしていいよ。フォローアップも積んで")).unwrap();
    // A deputy without authority to file: it settles and says so.
    script(&s, &[
        "MAGI ask --settle \"$qid\" --choice merge --quote 'マージしていいよ' \
         --note 'Follow-ups were NOT queued; file them yourself.'",
    ]);

    turn(&mut deputies(&s, 2)).await;
    assert_actions_succeeded(&s, 1);

    assert!(follow_ups(&s).is_empty());
    let q = s.store.get(&land.id).unwrap();
    assert!(q.thread.last().unwrap().note.as_deref().unwrap().contains("NOT queued"));
}
}

common::e2e! {
async fn an_operator_held_task_refuses_a_verbatim_merge_settle() {
    let s = scene(home_lock().await);
    let land = file_land(&s.store, "Merge #7?");
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    s.store.update(&land.id, |q| q.say("マージしていいよ")).unwrap();
    let queue = magi::queue::Queue::at(s.home.join("queue"));
    let mut task = magi::queue::Task::new(
        "held by the operator".to_owned(),
        "instruction".to_owned(),
        s.fx.repo.clone(),
        magi::queue::Source::Human,
    );
    task.runs.push(land.run.clone());
    task.hold_manual(Some("operator says wait".to_owned()));
    queue.put(&mut task).unwrap();
    script(&s, &[
        "MAGI ask --settle \"$qid\" --choice merge --quote 'マージしていいよ'",
    ]);

    turn(&mut deputies(&s, 2)).await;

    let q = s.store.get(&land.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Open, "{:?}", q.thread);
    assert!(q.answer.is_none());
}
}

common::e2e! {
async fn a_follow_up_request_alone_files_the_tasks_without_merging() {
    let s = scene(home_lock().await);
    let land = file_land(&s.store, "Merge #7?");
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    s.store.update(&land.id, |q| q.say("残りのレビュー指摘はフォローアップタスクとして積んで")).unwrap();
    script(&s, &[FOLLOW_UP]);

    turn(&mut deputies(&s, 2)).await;
    assert_actions_succeeded(&s, 1);

    let q = s.store.get(&land.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Open, "no merge was asked for");
    let tasks = follow_ups(&s);
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    assert_eq!(tasks[0].status, magi::queue::TaskStatus::Held);
}
}

#[test]
fn a_merge_approvals_deadline_never_moves_on_a_reply() {
    let mut land = Question::new(
        "run".to_owned(),
        magi::land::APPROVAL_NODE.to_owned(),
        "land".to_owned(),
        "Merge?".to_owned(),
        String::new(),
        vec!["merge".to_owned(), "hold".to_owned()],
    );
    land.answer_timeout = 1000;
    let base = land.asked_at.as_second();
    land.say("why?").unwrap();
    land.thread[0].at = Timestamp::from_second(base + 900).unwrap();
    assert_eq!(magi::deputy::deadline(&land, 5), base + 1000);

    let mut c = land.clone();
    c.node = magi::conduct::NODE.to_owned();
    assert_eq!(
        magi::deputy::deadline(&c, 5),
        base + 900 + 1000,
        "a conductor question still re-arms"
    );
    assert_eq!(magi::deputy::kind_of(&land), Some(magi::deputy::Kind::Land));
    c.node = "implement".to_owned();
    // No cwd and choices on offer: the fallback serves it.
    assert_eq!(magi::deputy::kind_of(&c), Some(magi::deputy::Kind::Generic));
    // Asked from inside a run (cwd recorded): the asker's, never a deputy's.
    c.cwd = Some("/somewhere".to_owned());
    assert_eq!(magi::deputy::kind_of(&c), None);
}

fn file_release(store: &Questions, seat: &str, choices: &[&str]) -> Question {
    let mut q = Question::new(
        String::new(),
        magi::bump::NOTICE_NODE.to_owned(),
        seat.to_owned(),
        "Release PR o/r#7 is stuck: what now?".to_owned(),
        "Pull request: https://example.test/pull/7".to_owned(),
        choices.iter().map(|c| (*c).to_owned()).collect(),
    );
    store.put(&mut q).expect("file the release question");
    q
}

common::e2e! {
async fn a_say_on_a_release_watch_question_gets_a_deputy_without_a_cwd() {
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    let rel = file_release(&s.store, "release-watch", &["rerun again", "hold", "leave it"]);
    let plain = file_release(&s.store, "bump", &[]);
    s.store.update(&rel.id, |q| q.say("what is failing?")).unwrap();
    s.store.update(&plain.id, |q| q.say("hello")).unwrap();
    let mut d = deputies(&s, 2);

    turn(&mut d).await;

    let q = s.store.get(&rel.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Open, "a say never settles it");
    assert!(q.cwd.is_none(), "a cwd would make it the waiter's");
    assert!(q.answer_timeout > 0);
    let dep = q.deputy.as_ref().expect("a deputy was attached");
    assert_eq!(dep.starts, 1);
    assert!(dep.brief.contains("does NOT close"), "{}", dep.brief);
    assert_eq!(q.thread.last().unwrap().who, Who::Agent, "the say was answered");
    assert_eq!(log(&s).len(), 1, "only the watcher's question costs a deputy");
    assert!(s.store.get(&plain.id).unwrap().deputy.is_none());
}
}

#[test]
fn a_release_questions_deadline_never_moves_on_a_reply() {
    let mut q = Question::new(
        String::new(),
        magi::bump::NOTICE_NODE.to_owned(),
        "release-watch".to_owned(),
        "stuck?".to_owned(),
        String::new(),
        vec!["hold".to_owned()],
    );
    q.answer_timeout = 1000;
    let base = q.asked_at.as_second();
    q.say("why?").unwrap();
    q.thread[0].at = Timestamp::from_second(base + 900).unwrap();
    assert_eq!(magi::deputy::deadline(&q, 5), base + 1000);
    assert_eq!(magi::deputy::kind_of(&q), Some(magi::deputy::Kind::Release));
}

/// One question as production files it: no `cwd`, the node, seat and choices
/// its filer uses. Every node that files a question with nobody asking belongs
/// here, so a new kind cannot ship without a listener: add its constant and
/// its shape, or `kind_of` returning `None` fails this test.
fn unasked_questions() -> Vec<Question> {
    let q = |node: &str, seat: &str, choices: &[&str]| {
        Question::new(
            TASK.to_owned(),
            node.to_owned(),
            seat.to_owned(),
            "s".to_owned(),
            String::new(),
            choices.iter().map(|c| (*c).to_owned()).collect(),
        )
    };
    vec![
        {
            let mut conduct = q(magi::conduct::NODE, "conduct", &["a", "b"]);
            conduct
                .actions
                .insert("a".into(), magi::ask::ChoiceAction::Requeue);
            conduct
                .actions
                .insert("b".into(), magi::ask::ChoiceAction::Hold);
            conduct
        },
        q(magi::land::APPROVAL_NODE, "land", &["merge", "hold"]),
        q(
            magi::bump::NOTICE_NODE,
            "release-watch",
            &["hold", "leave it"],
        ),
        q(magi::triage::NODE, "triage", &["resume", "keep", "discard"]),
        q(
            magi::triage::DEPS_NODE,
            "triage",
            &["release", "discard", "detach"],
        ),
        q(
            magi::github_text::ASK_NODE,
            magi::github_text::ASK_SEAT,
            &["use fallback", "use my text"],
        ),
        q(
            magi::reconcile::NODE,
            magi::reconcile::SEAT,
            &["push local", "keep remote"],
        ),
    ]
}

#[test]
fn every_question_filed_without_an_asker_has_a_deputy_kind() {
    for q in unasked_questions() {
        assert!(
            magi::deputy::kind_of(&q).is_some(),
            "node `{}` files questions that nobody listens to",
            q.node
        );
    }
}

#[test]
fn the_fallback_follows_the_cwd_and_the_choices_not_the_node_name() {
    use magi::deputy::{Kind, kind_of};
    let mut by_name = unasked_questions().pop().unwrap();
    assert_eq!(by_name.node, "review");
    assert_eq!(kind_of(&by_name), Some(Kind::Generic));
    // A reviewer's own `magi ask` from node `review` records its cwd: it has an
    // asker and the waiter, so no deputy.
    by_name.cwd = Some("/somewhere".to_owned());
    assert_eq!(kind_of(&by_name), None);
    // An unknown future node, no cwd, choices on offer: still served.
    let mut future = by_name.clone();
    future.cwd = None;
    future.node = "some-future-node".to_owned();
    assert_eq!(kind_of(&future), Some(Kind::Generic));
    // No choices: nothing to settle.
    future.choices.clear();
    assert_eq!(kind_of(&future), None);
    // A conductor question with a cwd is still served.
    let mut c = unasked_questions().remove(0);
    c.cwd = Some("/somewhere".to_owned());
    assert_eq!(kind_of(&c), Some(Kind::Conduct));
    // The bump notice (no choices, seat `bump`) stays unserved.
    let mut notice = unasked_questions().remove(2);
    notice.seat = "bump".to_owned();
    notice.choices.clear();
    assert_eq!(kind_of(&notice), None);
}

#[test]
fn only_the_conductors_question_is_ever_given_a_cwd_or_a_fixed_clock() {
    use magi::deputy::{Kind, fixed_clock, kind_of};
    for q in unasked_questions() {
        let kind = kind_of(&q).unwrap();
        // Triage and fallback questions run from their last activity, like a
        // conductor's; the waiter would otherwise take them as its own.
        // The posting gate's question is a deliberate exception: the graph
        // retires it at `asked_at + answer_timeout`, so a reply must not move
        // that deadline.
        if q.node == magi::github_text::ASK_NODE {
            assert!(fixed_clock(&q));
            assert!(q.cwd.is_none());
        } else if matches!(kind, Kind::Triage | Kind::Generic) {
            assert!(!fixed_clock(&q), "{}", q.node);
            assert!(q.cwd.is_none());
        }
    }
}

fn settle_ready(mut q: Question, said: &str) -> Question {
    let key = magi::ask::deputy_seat_key(&q.id);
    let mut d = Deputy::new("brief".to_owned());
    d.seat = Some(magi::agent::SeatState::new(&key, "stub", 1));
    q.deputy = Some(d);
    q.say(said).unwrap();
    q
}

#[test]
fn a_destructive_choice_settles_only_on_an_unhedged_quote() {
    let triage = unasked_questions().remove(3);
    let key = magi::ask::deputy_seat_key(&triage.id);
    for (said, quote, ok) in [
        ("discard it", "discard it", true),
        ("maybe discard it", "discard it", false),
        ("discard it if CI is red", "discard it", false),
        ("discard it. actually wait", "discard it", false),
    ] {
        let mut q = settle_ready(triage.clone(), said);
        let r = q.settle_by_deputy(&key, "discard", quote, None);
        assert_eq!(r.is_ok(), ok, "{said:?}: {r:?}");
    }
    // The task is released, not deleted: no mechanical hedge gate.
    let mut q = settle_ready(triage.clone(), "maybe release it");
    q.settle_by_deputy(&key, "resume", "maybe release it", None)
        .unwrap();
    // A divergence answer drops commits either way.
    let div = unasked_questions().pop().unwrap();
    let mut q = settle_ready(div.clone(), "keep the remote, I guess?");
    assert!(
        q.settle_by_deputy(&key, "keep remote", "keep the remote", None)
            .is_err()
    );
    assert!(magi::deputy::destructive(&div, "push local"));
}

common::e2e! {
async fn a_say_on_a_triage_or_unknown_question_gets_a_deputy_without_a_cwd() {
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    let mut ids = Vec::new();
    for mut q in unasked_questions().into_iter().filter(|q| {
        matches!(q.node.as_str(), "triage" | "triage-deps" | "review")
    }) {
        q.answer_timeout = 3600;
        s.store.put(&mut q).unwrap();
        s.store.update(&q.id, |r| r.say("what happens if I pick the last one?")).unwrap();
        ids.push(q.id.clone());
    }
    let mut d = deputies(&s, 3);

    turn(&mut d).await;

    for id in &ids {
        let q = s.store.get(id).unwrap();
        assert_eq!(q.status, QuestionStatus::Open, "a say never settles it");
        assert!(q.cwd.is_none(), "a cwd would make `{}` the waiter's", q.node);
        let dep = q.deputy.as_ref().expect("a deputy was attached");
        assert_eq!(dep.starts, 1);
        assert!(!dep.brief.is_empty());
        assert_eq!(q.thread.last().unwrap().who, Who::Agent, "the say was answered");
    }
    assert_eq!(log(&s).len(), 3);
}
}

#[test]
fn generic_brief_offers_read_only_investigation_only_for_a_run_id() {
    let home = std::path::Path::new("/tmp/magi-home-for-test");
    let mk = |run: &str| {
        Question::new(
            run.to_owned(),
            magi::github_text::ASK_NODE.to_owned(),
            "posting-gate".to_owned(),
            "title withheld".to_owned(),
            "detail".to_owned(),
            vec!["use fallback".to_owned()],
        )
    };
    let brief = |q: &Question| magi::deputy::generic_brief(q, &q.actions, home);

    let with = brief(&mk("20261011-120000-7756"));
    assert!(with.contains("20261011-120000-7756"));
    assert!(with.contains("magi show 20261011-120000-7756"));
    let artifacts = home
        .join("runs")
        .join("20261011-120000-7756")
        .join("artifacts");
    assert!(with.contains(&artifacts.display().to_string()));
    assert!(with.contains("read-only"));
    assert!(with.contains("not yours to guess"));
    assert!(with.contains("`use fallback`"));

    for run in ["", "a1b2c3"] {
        let without = brief(&mk(run));
        assert!(!without.contains("magi show"), "run {run:?}");
        assert!(!without.contains("read-only"), "run {run:?}");
        assert!(!without.contains("artifacts"), "run {run:?}");
        assert!(without.contains("You know only what the question itself says"));
    }
}

common::e2e! {
async fn deputy_non_hold_owner_choices_remain_independent_of_consultation_destination() {
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    for choice in [magi::land::REPAIR, magi::merge_decision::WITHDRAW, "reimplement:primary"] {
        let mut q = file_land(&s.store, "Owner decision");
        q.choices.push(choice.into());
        q.consult = Some(magi::ask::ChatConsult { talk: "different-standing-conversation".into(), at: Timestamp::now() });
        q.say(choice).unwrap();
        s.store.put(&mut q).unwrap();
        let command = format!("MAGI ask --settle \"$qid\" --choice '{choice}' --quote '{choice}'");
        script(&s, &[&command]);
        turn(&mut deputies(&s, 2)).await;
        assert_actions_succeeded(&s, 1);
        let answered = s.store.get(&q.id).unwrap();
        assert_eq!(answered.resolution().as_deref(), Some(choice));
        assert_eq!(answered.consult, q.consult);
        assert_eq!(answered.thread.iter().filter(|t| t.who == Who::Agent && t.body.starts_with("Settled as")).count(), 1);
    }
}
}

#[test]
fn every_question_choice_has_a_mapped_execution_effect() {
    let mut all = unasked_questions();
    let mut release = all[2].clone();
    release.choices.extend(
        [
            magi::release_watch::RERUN_AGAIN,
            magi::release_watch::RETRY,
            magi::land::APPROVE,
        ]
        .map(str::to_owned),
    );
    all.push(release);
    let mut posting = all[5].clone();
    posting.choices.extend(
        [
            magi::github_text::POST_ORIGINAL,
            magi::github_text::USE_REWRITE,
        ]
        .map(str::to_owned),
    );
    all.push(posting);
    for q in all {
        for label in &q.choices {
            assert!(
                magi::choice_effect::describe(&q, label).is_some(),
                "{} / {} offers an unmapped choice: {label}",
                q.node,
                q.seat
            );
        }
    }
    let mut q = unasked_questions().remove(0);
    q.choices.push("operator: edit the run record".into());
    assert!(magi::choice_effect::describe(&q, q.choices.last().unwrap()).is_none());
}

fn maintenance_question(s: &Scene, action: magi::ask::ChoiceAction) -> Question {
    let mut q = Question::new(
        TASK.into(),
        magi::conduct::NODE.into(),
        "conduct".into(),
        "Maintenance".into(),
        String::new(),
        vec!["daemon: execute".into()],
    );
    q.actions.insert(q.choices[0].clone(), action);
    s.store.put_executable(&mut q).unwrap();
    q
}

common::e2e! {
async fn maintenance_release_is_bound_to_the_hold_and_applies_once_after_settle() {
    use magi::queue::{Queue, Source, Task, TaskStatus};
    use magi::ask::{Answer, ChoiceAction};
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    let queue = Queue::at(s.home.join("queue"));
    let mut task = Task::new("held".into(), "work".into(), s.fx.repo.clone(), Source::Human);
    task.hold_machine(Some("waiting for a repair".into()));
    queue.put(&mut task).unwrap();
    let action = ChoiceAction::ReleaseTask { task: task.id.clone(),
        held_at: task.held_at.unwrap(), reason: task.hold_reason.clone() };
    let mut q = maintenance_question(&s, action.clone());
    q.say("daemon: execute").unwrap();
    s.store.put(&mut q).unwrap();
    script(&s, &["MAGI ask --settle \"$qid\" --choice 'daemon: execute' --quote 'daemon: execute'"]);
    turn(&mut deputies(&s, 2)).await;
    assert_actions_succeeded(&s, 1);
    assert_eq!(s.store.get(&q.id).unwrap().status, QuestionStatus::Answered);
    assert_eq!(queue.get(&task.id).unwrap().status, TaskStatus::Held,
        "a deputy records the answer and applies nothing");
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    let released = queue.get(&task.id).unwrap();
    assert_eq!(released.status, TaskStatus::Queued);
    assert!(released.action_applied(&q.id));
    // Reopening the stores simulates a daemon restart. A fresh manual hold
    // cannot be released by an old settled question.
    task = released;
    task.hold_machine(Some("another hold".into()));
    queue.put(&mut task).unwrap();
    magi::daemon::apply_choice_actions(&Queue::at(s.home.join("queue")),
        &Questions::at(s.home.join("questions")), &s.home).await;
    assert_eq!(queue.get(&task.id).unwrap().hold_reason.as_deref(), Some("another hold"));
    // A different question bound to the original hold must refuse as well.
    let mut stale = maintenance_question(&s, action);
    stale.answer(Answer::Choice(stale.choices[0].clone())).unwrap();
    s.store.put(&mut stale).unwrap();
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    assert_eq!(queue.get(&task.id).unwrap().status, TaskStatus::Held);
    assert!(!queue.get(&task.id).unwrap().action_applied(&stale.id));
    // Already released by hand is a refusal, never a second resumption.
    let action = ChoiceAction::ReleaseTask { task: task.id.clone(),
        held_at: task.held_at.unwrap(), reason: task.hold_reason.clone() };
    let mut late = maintenance_question(&s, action);
    late.answer(Answer::Choice(late.choices[0].clone())).unwrap();
    s.store.put(&mut late).unwrap();
    task.release_operator("manual", None);
    queue.put(&mut task).unwrap();
    let generation = task.resumption.as_ref().unwrap().generation;
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    assert_eq!(queue.get(&task.id).unwrap().resumption.unwrap().generation, generation);
    task.hold_manual(Some("operator parked".into()));
    queue.put(&mut task).unwrap();
    let mut manual = maintenance_question(&s, ChoiceAction::ReleaseTask { task: task.id.clone(),
        held_at: task.held_at.unwrap(), reason: task.hold_reason.clone() });
    manual.say("daemon: execute").unwrap();
    s.store.put(&mut manual).unwrap();
    turn(&mut deputies(&s, 2)).await;
    assert_eq!(s.store.get(&manual.id).unwrap().status, QuestionStatus::Open,
        "a deputy cannot settle a typed release of an operator-held target");
    assert!(queue.get(&task.id).unwrap().operator_held());
}
}

common::e2e! {
async fn maintenance_ownerless_resume_refuses_work_on_another_run() {
    use magi::{ask::{Answer, ChoiceAction}, run::{RunState, RunStatus}};
    let s = scene(home_lock().await);
    let queue = magi::queue::Queue::at(s.home.join("queue"));
    let mut run = RunState::new(s.fx.repo.clone(), "main".into(), "base".into(),
        "work".into(), s.fx.config.clone());
    run.status = RunStatus::Stalled;
    run.driver_exited = true;
    run.save_under(&s.home).unwrap();
    let other = "20261019-000001-bbbb";
    std::fs::write(s.home.join("daemon.json"), serde_json::to_vec(&serde_json::json!({
        "updated_at": Timestamp::now(),
        "current": [{"task": TASK, "run": other}],
    })).unwrap()).unwrap();
    assert_eq!(magi::daemon::current_work(&s.home, Timestamp::now())[0].run, other);
    let mut q = maintenance_question(&s, ChoiceAction::ResumeRun { run: run.id.clone() });
    q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
    s.store.put(&mut q).unwrap();
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    let receipt = s.home.join("choice-actions").join(format!("{}.json", q.id));
    let body = std::fs::read_to_string(&receipt).unwrap();
    assert!(body.contains("the loop is running run"), "{body}");
    assert_eq!(serde_json::to_value(RunState::load_under(&run.id, &s.home).unwrap()).unwrap(),
        serde_json::to_value(&run).unwrap());
    assert!(queue.list().is_empty());
    // A refused choice cannot start later when the daemon becomes idle.
    std::fs::remove_file(s.home.join("daemon.json")).unwrap();
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    assert_eq!(std::fs::read_to_string(receipt).unwrap(), body);
    assert_eq!(serde_json::to_value(RunState::load_under(&run.id, &s.home).unwrap()).unwrap(),
        serde_json::to_value(&run).unwrap());
}
}

common::e2e! {
async fn maintenance_resume_refuses_a_live_driver_and_abandon_refuses_unknown() {
    use magi::{ask::{Answer, ChoiceAction}, run::{RunState, RunStatus}};
    let s = scene(home_lock().await);
    let queue = magi::queue::Queue::at(s.home.join("queue"));
    let mut run = RunState::new(s.fx.repo.clone(), "main".into(), "base".into(),
        "work".into(), s.fx.config.clone());
    run.status = RunStatus::Stalled;
    run.driver_pid = Some(std::process::id());
    run.driver_started_at = magi::proc::process_started_at(std::process::id());
    assert!(run.driver_started_at.is_some(), "the test requires process identity");
    run.driver_exited = false;
    run.save_under(&s.home).unwrap();
    let mut q = maintenance_question(&s, ChoiceAction::ResumeRun { run: run.id.clone() });
    q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
    s.store.put(&mut q).unwrap();
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    let receipt = std::fs::read_to_string(s.home.join("choice-actions").join(format!("{}.json", q.id))).unwrap();
    assert!(receipt.contains("live driver"), "{receipt}");
    assert_eq!(RunState::load_under(&run.id, &s.home).unwrap().status, RunStatus::Stalled);
    run.driver_pid = None;
    run.driver_started_at = None;
    run.save_under(&s.home).unwrap();
    let mut q = maintenance_question(&s, ChoiceAction::AbandonRun { run: run.id.clone() });
    q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
    s.store.put(&mut q).unwrap();
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    assert_eq!(RunState::load_under(&run.id, &s.home).unwrap().status, RunStatus::Stalled);
    // Explicit stopped-driver evidence is accepted by the shared judge.
    run.driver_exited = true;
    run.save_under(&s.home).unwrap();
    let mut q = maintenance_question(&s, ChoiceAction::AbandonRun { run: run.id.clone() });
    q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
    s.store.put(&mut q).unwrap();
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    assert_eq!(RunState::load_under(&run.id, &s.home).unwrap().status, RunStatus::Failed);
}
}

common::e2e! {
#[cfg(unix)]
async fn maintenance_reruns_are_recorded_before_forge_calls_and_survive_restart() {
    use magi::ask::{Answer, ChoiceAction};
    use std::os::unix::fs::PermissionsExt;
    let s = scene(home_lock().await);
    let bin = s.fx.tmp.path().join("choice-forge");
    std::fs::create_dir_all(&bin).unwrap();
    let snapshot = s.fx.tmp.path().join("choice-pr.json");
    let log = s.fx.tmp.path().join("choice-reruns.log");
    let gh = bin.join("gh");
    std::fs::write(&gh, r#"#!/bin/sh
case "$1 $2" in
  'pr view') cat "$CHOICE_PR" ;;
  'run rerun')
    test -s "$CHOICE_HOME/choice-actions/reruns.json" || exit 19
    grep -q "$3" "$CHOICE_HOME/choice-actions/reruns.json" || exit 20
    printf '%s\n' "$3" >> "$CHOICE_LOG" ;;
  *) exit 17 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    // SAFETY: e2e! isolates this test in a child; no other test shares PATH.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("CHOICE_PR", &snapshot);
        std::env::set_var("CHOICE_HOME", &s.home);
        std::env::set_var("CHOICE_LOG", &log);
    }
    let write_snapshot = |head: &str| {
        std::fs::write(&snapshot, serde_json::to_vec(&serde_json::json!({
            "url": "https://github.com/o/r/pull/7", "number": 7, "state": "OPEN",
            "headRefOid": head, "statusCheckRollup": [{
                "__typename": "CheckRun", "name": "CI", "status": "COMPLETED",
                "conclusion": "FAILURE", "detailsUrl": "https://github.com/o/r/actions/runs/42/job/1"
            }]
        })).unwrap()).unwrap();
    };
    write_snapshot("head");
    let action = ChoiceAction::RerunChecks { repo: s.fx.repo.clone(),
        pr: "https://github.com/o/r/pull/7".into(), head: "head".into(), runs: vec!["42".into()] };
    let queue = magi::queue::Queue::at(s.home.join("queue"));
    let mut q = maintenance_question(&s, action.clone());
    q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
    s.store.put(&mut q).unwrap();
    assert!(!log.exists(), "recording an answer applies nothing");
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "42\n");
    let restarted = Questions::at(s.home.join("questions"));
    magi::daemon::apply_choice_actions(&queue, &restarted, &s.home).await;
    // A fresh explicit owner choice can authorize one more bounded rerun.
    let mut duplicate = maintenance_question(&s, action.clone());
    duplicate.answer(Answer::Choice(duplicate.choices[0].clone())).unwrap();
    s.store.put(&mut duplicate).unwrap();
    magi::daemon::apply_choice_actions(&queue, &restarted, &s.home).await;
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "42\n42\n");
    write_snapshot("different-head");
    let mut stale = maintenance_question(&s, action.clone());
    stale.answer(Answer::Choice(stale.choices[0].clone())).unwrap();
    s.store.put(&mut stale).unwrap();
    magi::daemon::apply_choice_actions(&queue, &restarted, &s.home).await;
    let receipt = std::fs::read_to_string(s.home.join("choice-actions").join(format!("{}.json", stale.id))).unwrap();
    assert!(receipt.contains("head or lifecycle changed"), "{receipt}");
    // A failed durable write prevents an external call.
    write_snapshot("head");
    let snapshot_body = std::fs::read_to_string(&snapshot).unwrap().replace("/runs/42/", "/runs/43/");
    std::fs::write(&snapshot, snapshot_body).unwrap();
    let mut unsaved_action = action;
    if let ChoiceAction::RerunChecks { runs, .. } = &mut unsaved_action { runs[0] = "43".into(); }
    let mut unsaved = maintenance_question(&s, unsaved_action);
    unsaved.answer(Answer::Choice(unsaved.choices[0].clone())).unwrap();
    s.store.put(&mut unsaved).unwrap();
    std::fs::create_dir(s.home.join("choice-actions").join(format!("{}.tmp", unsaved.id))).unwrap();
    magi::daemon::apply_choice_actions(&queue, &restarted, &s.home).await;
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "42\n42\n");
}
}

#[test]
fn maintenance_choice_without_a_queue_owner_is_never_delivered_to_an_asker() {
    let mut q = unasked_questions().remove(0);
    q.cwd = Some("repo".into());
    q.actions.insert(
        q.choices[0].clone(),
        magi::ask::ChoiceAction::ResumeRun {
            run: "named".into(),
        },
    );
    q.answer(magi::ask::Answer::Choice(q.choices[0].clone()))
        .unwrap();
    assert_eq!(
        magi::waiter::decide_owned(&q, None, false, 3600, Timestamp::now(), false),
        magi::waiter::Action::Idle
    );
}

fn conductor_maintenance_question(
    s: &Scene,
    queue: &magi::queue::Queue,
    task: &magi::queue::Task,
    action: magi::ask::ChoiceAction,
) -> Question {
    magi::conduct::apply(
        queue,
        &s.store,
        &magi::conduct::Verdict {
            decisions: vec![magi::conduct::Decision {
                id: task.id.clone(),
                question: Some("Maintenance".into()),
                choices: vec!["daemon: execute".into()],
                actions: std::collections::BTreeMap::from([("daemon: execute".into(), action)]),
                ..Default::default()
            }],
        },
    )
    .unwrap();
    s.store
        .list()
        .into_iter()
        .find(|q| q.run == task.id && q.node == magi::conduct::NODE)
        .unwrap()
}

common::e2e! {
async fn maintenance_conductor_release_restores_its_original_hold_once() {
    use magi::{ask::{Answer, ChoiceAction}, queue::{Queue, Source, Task, TaskStatus}};
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    let queue = Queue::at(s.home.join("queue"));
    let mut task = Task::new("held".into(), "work".into(), s.fx.repo.clone(), Source::Human);
    task.hold_machine(Some("waiting for repair".into()));
    queue.put(&mut task).unwrap();
    let action = ChoiceAction::ReleaseTask { task: task.id.clone(),
        held_at: task.held_at.unwrap(), reason: task.hold_reason.clone() };
    let mut q = conductor_maintenance_question(&s, &queue, &task, action);
    let blocked = queue.get(&task.id).unwrap();
    assert_eq!(blocked.status, TaskStatus::Blocked);
    assert_eq!(blocked.blocked_from, Some(TaskStatus::Held));
    assert_eq!(blocked.blocked_by, vec![q.id.clone()]);
    assert_eq!(blocked.held_at, task.held_at);
    q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
    s.store.put(&mut q).unwrap();
    magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
    let released = queue.get(&task.id).unwrap();
    assert_eq!(released.status, TaskStatus::Queued);
    assert!(released.blocked_by.is_empty());
    assert!(released.action_applied(&q.id));
    assert_eq!(released.resumption.as_ref().unwrap().cleared_reason, task.hold_reason);
    magi::daemon::apply_choice_actions(&Queue::at(s.home.join("queue")),
        &Questions::at(s.home.join("questions")), &s.home).await;
    assert_eq!(queue.get(&task.id).unwrap().resumption.as_ref().unwrap().generation,
        released.resumption.as_ref().unwrap().generation);
}
}

common::e2e! {
async fn maintenance_resume_preserves_operator_decisions_after_deputy_settle() {
    use magi::{ask::ChoiceAction, queue::{Queue, Source, Task, TaskStatus}, run::{RunState, RunStatus}};
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    let queue = Queue::at(s.home.join("queue"));
    for manual_hold in [true, false] {
        let mut run = RunState::new(s.fx.repo.clone(), "main".into(), "base".into(),
            "work".into(), s.fx.config.clone());
        run.status = RunStatus::Stalled;
        run.driver_exited = true;
        run.save_under(&s.home).unwrap();
        let mut task = Task::new("resume".into(), "work".into(), s.fx.repo.clone(), Source::Human);
        task.runs.push(run.id.clone());
        task.hold_machine(Some("waiting for resume".into()));
        queue.put(&mut task).unwrap();
        let mut q = conductor_maintenance_question(&s, &queue, &task,
            ChoiceAction::ResumeRun { run: run.id.clone() });
        q.say("daemon: execute").unwrap();
        s.store.put(&mut q).unwrap();
        script(&s, &["MAGI ask --settle \"$qid\" --choice 'daemon: execute' --quote 'daemon: execute'"]);
        turn(&mut deputies(&s, 2)).await;
        assert_actions_succeeded(&s, 1);
        assert_eq!(s.store.get(&q.id).unwrap().status, QuestionStatus::Answered);
        task = queue.get(&task.id).unwrap();
        assert_eq!(task.status, TaskStatus::Blocked, "settling applies nothing");
        if manual_hold {
            task.hold_manual(Some("operator parked after settle".into()));
        } else {
            task.release_operator("manual", None);
        }
        assert!(task.operator_decided_at.unwrap() > q.asked_at);
        queue.put(&mut task).unwrap();
        let run_before = std::fs::read(s.home.join("runs").join(&run.id).join("run.json")).unwrap();
        for _ in 0..2 {
            magi::daemon::apply_choice_actions(&Queue::at(s.home.join("queue")),
                &Questions::at(s.home.join("questions")), &s.home).await;
            let after = queue.get(&task.id).unwrap();
            assert_eq!(after.status, task.status);
            assert_eq!(after.hold_reason, task.hold_reason);
            assert_eq!(after.operator_decided_at, task.operator_decided_at);
            assert_eq!(after.resumption.as_ref().map(|r| r.generation),
                task.resumption.as_ref().map(|r| r.generation));
            assert!(after.resume_override.is_none());
            assert_eq!(std::fs::read(s.home.join("runs").join(&run.id).join("run.json")).unwrap(), run_before);
        }
        let receipt = std::fs::read_to_string(s.home.join("choice-actions").join(format!("{}.json", q.id))).unwrap();
        assert!(receipt.contains("superseded by an operator hold or newer operator decision"), "{receipt}");
    }
}
}

common::e2e! {
async fn maintenance_named_resume_never_replaces_an_exhausted_run() {
    use magi::{ask::{Answer, ChoiceAction}, queue::{Queue, Source, Task, TaskStatus},
        run::{ReviewRound, RunState, RunStatus}};
    let s = scene(home_lock().await);
    s.store.update(&s.q.id, |q| { q.abandon("not under test"); Ok(()) }).unwrap();
    let queue = Queue::at(s.home.join("queue"));
    let mut config = s.fx.config.clone();
    config.disk.min_free_bytes = 0;
    config.disk.auto_fold = false;
    config.disk.cache_limit_bytes = 0;
    config.graph.review_rounds = 1;
    let config_path = s.fx.tmp.path().join("resume.toml");
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let opts = magi::daemon::Opts { repo: s.fx.repo.clone(), config: Some(config_path),
        worktrees_root: Some(s.fx.tmp.path().join("worktrees")), ..Default::default() };
    for exhausted_before_answer in [true, false] {
        let mut task = Task::new("resume existing work".into(), "work".into(), s.fx.repo.clone(), Source::Human);
        let mut run = RunState::new(s.fx.repo.clone(), "main".into(), "base".into(),
            task.instruction.clone(), config.clone());
        run.status = RunStatus::Blocked;
        run.driver_exited = true;
        run.pr_publication_pending = false;
        let round = ReviewRound { round: 1, head: "base".into(), verified_head: None,
            verified_at: None, reviews: Vec::new(), e2e: Vec::new(), verify_retried: false,
            e2e_deferred: false, e2e_defer_reason: None, fix: None, blocking: 0,
            answered: 1, expected: 1, clean: false, progressed: true, vote_split: false,
            reconsideration: Vec::new(), verdict: None };
        if exhausted_before_answer { run.reviews.push(round.clone()); }
        run.save_under(&s.home).unwrap();
        task.runs.push(run.id.clone());
        task.hold_machine(Some("waiting for resume".into()));
        queue.put(&mut task).unwrap();
        assert!(task.review_of.is_none() && task.replacement.is_none() && task.pr_recovery.is_none());
        let mut q = conductor_maintenance_question(&s, &queue, &task,
            ChoiceAction::ResumeRun { run: run.id.clone() });
        q.answer(Answer::Choice(q.choices[0].clone())).unwrap();
        s.store.put(&mut q).unwrap();
        magi::daemon::apply_choice_actions(&queue, &s.store, &s.home).await;
        task = queue.get(&task.id).unwrap();
        assert_eq!(task.status, TaskStatus::Queued);
        assert_eq!(task.resume_override.as_ref().unwrap().pinned_run.as_deref(), Some(run.id.as_str()));
        // Also protect a pin that was accepted while budget remained but is
        // no longer resumable by the time the daemon dispatches the attempt.
        if !exhausted_before_answer {
            run.reviews.push(round);
            run.save_under(&s.home).unwrap();
        }
        let path = s.home.join("runs").join(&run.id).join("run.json");
        let before = std::fs::read(&path).unwrap();
        let runs_before = std::fs::read_dir(s.home.join("runs")).unwrap()
            .map(|entry| entry.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>();
        let claim = queue.claim(&task.id).unwrap();
        magi::daemon::run_claimed(&opts, &queue, &mut task).await;
        drop(claim);
        let stored = queue.get(&task.id).unwrap();
        assert_eq!(stored.status, TaskStatus::Held);
        assert!(stored.hold_reason.as_deref().unwrap().contains("pinned run cannot resume"), "{:?}", stored.hold_reason);
        assert_eq!(stored.runs, vec![run.id.clone()]);
        assert_eq!(stored.attempts, 0, "no new competition or graph execution");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let runs_after = std::fs::read_dir(s.home.join("runs")).unwrap()
            .map(|entry| entry.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(runs_after, runs_before, "no replacement run was minted");
    }
}
}
