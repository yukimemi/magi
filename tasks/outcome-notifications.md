# Outcome notification investigation

Read-only inspection of the queue, questions, conductor artifacts and chat delivery
code showed two independent causes of repeated automatic turns:

- The task's final recorded run settled pass 1 before the later startup failures.
  The later task error still described a preflight source-selection refusal.
  A saved conductor output explicitly requested requeue after an earlier recovery
  approval. Requeue calls release, which clears `held_at`; a subsequent startup
  failure can hold the task without implementation progress. Timestamp-based
  delivery therefore treated that unresolved outcome as new.
- Conductor questions block a held task. Applying the answer restores/reaffirms
  a hold, and `note_held` previously replaced its timestamp when arriving from
  Blocked. Dependency keys also included regenerated explanation prose.

The originating talk retained 38 accepted delivery keys for this task: 33 Held
and five Blocked keys. These were accepted draft deliveries, not duplicate UI
rows. Its automatic draft draining therefore could start repeated assistant
turns. The final Held keys continued after the last settled run/pass.

The saved artifacts do not prove every intervening conductor decision. In
particular, paraphrasing a reason while a task stays Held does not itself reset
`held_at`. The last persisted delivery recorded a Held timestamp, while the
operator's answers and last settled run still described the unresolved recovery.

The current base includes `f817610` (selected public-head recovery and bounded
startup retries) and `c577963` (legacy repair choices resume their existing
owners). This change leaves candidate selection and those authorization paths
intact. Real queue, question, run and chat files were only read.

Delivery identity now uses producer stage, settled run/pass and dependency sets.
A release freezes an unresolved legacy hold before a no-progress recovery can
mint a different run. Completed deliveries form a task-lock-protected union;
the talk's existing atomic draft/key write still reconciles a crash between
stores. Notification bookkeeping is excluded from conductor decision input. New
conductor questions also retain their independent decision context, preventing
an answered question from being regenerated solely by rewording it. Open
questions are still reused and never cleared by deduplication.

Focused regressions use temporary stores and synthetic task/run/question ids.
They check draft growth and automatic-turn eligibility, including returning to
an earlier cause after blocking, stale writes, persistence, new decisions,
new run failures and completion after a hold. Full verification and pre-PR
review remain the parent run's responsibility.
