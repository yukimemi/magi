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
