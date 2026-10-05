//! The daemon waiter: a question keeps something waiting on it after the
//! `magi ask` that filed it is gone, and the owner's word still reaches the
//! same seat's conversation.
mod common;

use std::path::PathBuf;

use common::{Fixture, HomeGuard, Judges, fixture, home_lock};
use jiff::Timestamp;
use magi::agent::SeatState;
use magi::ask::{Answer, Question, QuestionStatus, Questions, WaiterKind, Who};
use magi::notices::Notices;
use magi::queue::{Queue, Source, Task, TaskStatus};
use magi::run::{RunState, RunStatus};
use magi::waiter::Waiter;

struct Scene {
    fx: Fixture,
    home: PathBuf,
    store: Questions,
    waiter: Waiter,
    q: Question,
}

/// A run with one seat that has taken a turn, and a question that seat filed
/// from its worktree. `turns` is the seat's turn count: 0 means nothing to
/// resume.
fn scene(home: HomeGuard, turns: usize) -> Scene {
    let fx = fixture(home, Judges::Unanimous, false);
    let home = fx.tmp.path().join("magi-home");
    let mut state = RunState::new(
        fx.repo.clone(),
        "main".to_owned(),
        "0".repeat(40),
        "task".to_owned(),
        fx.config.clone(),
    );
    state.status = RunStatus::Implementing;
    let mut seat = SeatState::new("impl-A", "alpha", 7);
    seat.turns = turns;
    state.seats.insert("impl-A".to_owned(), seat);
    state.save_under(&home).expect("save the run");

    let store = Questions::at(home.join("questions"));
    let mut q = Question::new(
        state.id.clone(),
        "implement".to_owned(),
        "impl-A".to_owned(),
        "Which storage backend?".to_owned(),
        "SQLite or Redis".to_owned(),
        Vec::new(),
    );
    q.answer_timeout = 3600;
    q.cwd = Some(fx.repo.to_string_lossy().into_owned());
    store.put(&mut q).expect("file the question");

    let waiter = Waiter::new(store.clone(), home.clone(), None);
    Scene {
        fx,
        home,
        store,
        waiter,
        q,
    }
}

fn resumes(s: &Scene) -> usize {
    std::fs::read_to_string(s.fx.repo.join("resumes.log"))
        .map(|l| l.lines().count())
        .unwrap_or(0)
}

async fn tick(s: &mut Scene) {
    s.waiter.tick(Timestamp::now(), &|| false).await;
}

common::e2e! {
async fn a_say_after_the_asker_is_gone_resumes_the_same_seat() {
    let mut s = scene(home_lock().await, 1);
    // No lease at all: the `magi ask` that filed this is long dead.
    s.store.update(&s.q.id, |q| q.say("Why not Postgres?")).unwrap();

    tick(&mut s).await;

    assert_eq!(resumes(&s), 1, "the seat was resumed exactly once");
    let q = s.store.get(&s.q.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Open, "a say never settles a question");
    assert_eq!(q.delivered_turns, q.thread.len(), "and it is recorded as delivered");
    let last = q.thread.last().unwrap();
    assert_eq!(last.who, Who::Agent, "the agent's prose reply is kept as its turn");
    assert!(last.body.contains("mock reply from impl-A"), "{}", last.body);
    assert!(!q.waiting_on_agent(), "the ball is back with the owner");
    assert!(q.waiter.is_none());

    // What the agent was sent: the question, the thread, the owner's words.
    let dir = s.store.root().join(format!("{}.delivery", s.q.id));
    let prompt = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".prompt.md"))
        .expect("a prompt artifact");
    let prompt = std::fs::read_to_string(prompt).unwrap();
    assert!(prompt.contains("Which storage backend?"), "{prompt}");
    assert!(prompt.contains("Why not Postgres?"), "{prompt}");

    // Idempotent: nothing new to hand over, so no second turn.
    tick(&mut s).await;
    assert_eq!(resumes(&s), 1);
}
}

common::e2e! {
async fn a_live_asker_means_no_second_agent() {
    let mut s = scene(home_lock().await, 1);
    s.store.update(&s.q.id, |q| q.say("Why not Postgres?")).unwrap();
    // The asker beat a moment ago: it is still blocked in `magi ask`.
    s.store.beat(&s.q.id, WaiterKind::Asker);

    tick(&mut s).await;

    assert_eq!(resumes(&s), 0, "nothing was started while the asker is alive");
    let q = s.store.get(&s.q.id).unwrap();
    assert!(q.waiting_on_agent(), "the say is still waiting for the asker to read it");
    assert_eq!(q.delivered_turns, 0);
}
}

common::e2e! {
async fn an_unanswered_question_expires_at_its_timeout_without_starting_anyone() {
    let mut s = scene(home_lock().await, 1);
    s.store
        .update(&s.q.id, |q| {
            q.answer_timeout = 60;
            q.asked_at = Timestamp::from_second(Timestamp::now().as_second() - 3600).unwrap();
            Ok(())
        })
        .unwrap();

    tick(&mut s).await;

    let q = s.store.get(&s.q.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Abandoned);
    assert!(q.detail.contains("no answer within 60s of asking"), "{}", q.detail);
    assert_eq!(resumes(&s), 0);
}
}

common::e2e! {
async fn a_seat_with_no_session_is_never_replaced_by_a_fresh_agent() {
    let mut s = scene(home_lock().await, 0);
    s.store.update(&s.q.id, |q| q.say("Why not Postgres?")).unwrap();

    tick(&mut s).await;

    assert_eq!(resumes(&s), 0, "no fresh consultant was started");
    let q = s.store.get(&s.q.id).unwrap();
    assert_eq!(q.status, QuestionStatus::Open, "the record is kept");
    assert_eq!(q.thread.len(), 1, "and nothing was invented for the agent");
    assert_eq!(q.delivered_turns, 0);
    let notices = Notices::at(s.home.join("notifications")).list();
    assert_eq!(notices.len(), 1, "the owner is told why: {notices:?}");
    assert!(notices[0].message.contains("no session to resume"), "{}", notices[0].message);

    // The same input is not retried every tick, nor does it re-notify.
    tick(&mut s).await;
    assert_eq!(Notices::at(s.home.join("notifications")).list()[0].count, 1);
}
}

common::e2e! {
async fn an_answer_the_asker_never_read_is_delivered_by_the_daemon() {
    let mut s = scene(home_lock().await, 1);
    s.store
        .update(&s.q.id, |q| q.answer(Answer::Text("SQLite".to_owned())))
        .unwrap();

    tick(&mut s).await;

    assert_eq!(resumes(&s), 1);
    let q = s.store.get(&s.q.id).unwrap();
    assert!(q.answer_delivered);
    assert_eq!(q.status, QuestionStatus::Answered);
}
}

common::e2e! {
async fn an_applied_action_is_never_delivered_after_a_claim_collision() {
    let mut s = scene(home_lock().await, 1);
    let queue = Queue::at(s.home.join("queue"));
    let mut task = Task::new(
        "t".to_owned(),
        "t".to_owned(),
        s.fx.repo.clone(),
        Source::Human,
    );
    task.start(s.q.run.clone());
    queue.put(&mut task).unwrap();
    s.store
        .update(&s.q.id, |q| {
            q.choices = vec!["again".to_owned()];
            q.actions
                .insert("again".to_owned(), magi::ask::ChoiceAction::Requeue);
            q.answer(Answer::Choice("again".to_owned()))
        })
        .unwrap();

    // An in-flight attempt holds the task's claim while the waiter looks: the
    // task reads Running, so the word would be delivered - but not claimless.
    let attempt = queue.claim(&task.id).unwrap();
    tick(&mut s).await;
    assert_eq!(resumes(&s), 0, "no delivery without the task claim");
    assert!(!s.store.root().join(format!("{}.delivery", s.q.id)).exists());
    assert!(s.store.read_lease(&s.q.id).is_none());
    assert!(s.store.get(&s.q.id).unwrap().waiter.is_none());

    // The attempt ends held and releases; the daemon applies the requeue
    // (exactly once, through `actions_applied`) and a new competition starts.
    let mut t = queue.get(&task.id).unwrap();
    t.hold_machine(Some("blocked".to_owned()));
    queue.put(&mut t).unwrap();
    drop(attempt);
    let mut t = queue.get(&task.id).unwrap();
    t.release();
    t.mark_action_applied(&s.q.id);
    queue.put(&mut t).unwrap();
    let mut t = queue.get(&task.id).unwrap();
    t.start("20260101-000000-new1".to_owned());
    queue.put(&mut t).unwrap();

    tick(&mut s).await;
    assert_eq!(resumes(&s), 0, "an applied action is a stop condition");
    let t = queue.get(&task.id).unwrap();
    assert_eq!(t.status, TaskStatus::Running);
    assert_eq!(t.actions_applied, vec![s.q.id.clone()], "applied once");
    assert!(!s.store.get(&s.q.id).unwrap().answer_delivered);
}
}
