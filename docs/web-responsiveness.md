# Progressive web loading

Conversation transcripts have a `/api/talks/{id}/core` endpoint that avoids
queue history, config discovery and executable probes. Related tasks, roster,
personas and configured display metadata arrive through `/sections`. Related task
rows carry only ID, title and status, never their instruction bodies. The old
conversation detail endpoint remains compatible. Task instructions have a
`/api/queue/{id}/core` endpoint; history and dependency evidence still come from
the full detail endpoint. Core task responses never enable task actions.

Run detail and structured reports render independently. Text reports are read
only for an unsupported/failed structured report or the Raw selection. A failed
background refresh retains the previous content and exposes a local retry.
Failed dependency/detail evidence disables the associated action controls.

Requests are scoped to the current destination. Navigation aborts reads and
invalidates their callbacks, including navigation away and back to the same ID.
Pending requests are coalesced; a background invalidation during a read schedules
one trailing refresh. Lists retain their existing delta/base handling. Questions
use the same serialized list loader; dashboard filter changes cancel older
requests. The event stream starts without waiting for initial HTTP requests.
Unchanged task histories and run timelines retain their DOM nodes.

## Server work

- Exact task, conversation and run IDs use direct filesystem lookup; short IDs
  use filenames and preserve ambiguity rejection. Direct lookup rejects paths.
  The task store's own resolver also has the exact-ID fast path.
- Queue relationship lookups share lightweight relationship records; instruction
  bodies are discarded after parsing. Matching tasks are read fresh. Each lookup checks file
  metadata; only changed records are reparsed. Unix fingerprints include device,
  inode and ctime as well as mtime/size, so atomic replacement with preserved
  mtime is detected. Other platforms reparse rather than assume this guarantee.
  Failed scans/decodes return an error, never an empty dependency result.
- Run detail obtains the matching task once for successor, latest attempt and
  task outcome. This eliminates three independent queue parses.
- A single worker scans revision metadata for all SSE clients and health reads.
  Cold SSE clients wait for that worker instead of starting duplicate scans.
  Health shares revisions and reads questions once for its two counts. Transient
  scan failures preserve the last successful snapshot and are retried. Revision
  tokens include file identity and deletion, including question/notice stores.
  Question stores include lease and panel entries, not just JSON records.
- No config or process-liveness cache was introduced. Mutation handlers retain
  their existing authoritative validation.

## Reproduction and measurement boundaries

```sh
node --test tests/web_progressive.mjs
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib web_history_latency_measurement -- --ignored --nocapture
MAGI_BENCH_RECORDS=4000 RUSTUP_TOOLCHAIN=stable cargo test --locked --lib web_history_latency_measurement -- --ignored --nocapture
```

The ignored measurement builds an isolated history of 1,000 conversations and
1,000 tasks (16 KiB pending history and 22 KiB instructions per record), plus a
conversation containing a real operator turn. It takes 12 batches with one and
four concurrent clients. `tests/web_latency/baseline.rs` preserves the actual
conversation handler and resolver from origin/main
`af7f5e26453e533ba569f8c19dbe22c9cd7eeffb`, replayed against the same fixture and
supporting code. It is a route-body comparison, not a separate baseline server
or a production browser benchmark. The first batch includes cold index setup;
subsequent batches retain the parsed cache. Response sizes include both sections.

`first_ms` measures when transcript data is ready; `complete_ms` waits for the
transcript and secondary response. Neither includes browser layout, HTTP transit
or a paint measurement. The screenshot's agent wait timer is not used. The
JavaScript tests execute shipped loaders with deliberately delayed, failing and
reversed responses; they verify useful rendering before completion rather than
interpreting shell rendering as useful content.

## Observed route-body latency

Development build, 1,000 records in each store, 12 batches per scenario. The
baseline is replayed first; these are warm-filesystem measurements rather than a
cold-disk claim. The changed route's one-client samples include its cold index.

| Route | Clients | First p50 / p95 (ms) | Complete p50 / p95 (ms) | Combined bytes |
| --- | ---: | ---: | ---: | ---: |
| origin/main handler | 1 | 559.44 / 641.36 | 559.44 / 641.36 | 2637 |
| progressive handlers | 1 | 0.33 / 0.77 | 19.34 / 569.22 | 2935 |
| origin/main handler | 4 | 1463.95 / 1997.57 | 1463.95 / 1997.57 | 2637 |
| progressive handlers | 4 | 1.09 / 64.60 | 87.77 / 154.18 | 2935 |

A preceding run on the shared development host measured baseline single-client
p50 at 3,572.85 ms and progressive first-content p50 at 0.69 ms. Host load and
filesystem caching materially affect absolute times; these are diagnostics, not
a production latency guarantee.

The changed handler parsed 1,000 task records for the initial index across the
one-client batches, then zero additional task records across the four-client
batches. The baseline handlers parse the conversation and queue collections on
every request. This demonstrates the cost of cumulative history reads in this
fixture; it does not establish what dominated the reported live CPU sample.

## Remaining costs and follow-up

Metadata reconciliation remains proportional to history size. Queue lookups
serialize their metadata reconciliation and retain relationship projections in memory.
Cold index construction still parses the queue, but no longer blocks the
conversation transcript. Transcript JSON/Markdown itself is not paginated.
Related conversation tasks and roster still share one secondary response.

Task history, run detail relationship evidence, full list initial loads and the
stats aggregate still have monolithic server work. Report/detail routes still
read run state and check liveness separately; lazy text fallback removes one
normal read but does not eliminate the other two. Health still does config,
disk and unreadable-run work. Further section splitting and measured shared run
projections remain follow-up work. There is no claim that every route is O(1).

This environment denies process inspection, so no CPU/profile comparison against
the running web process was collected. Production accumulated-history browser
paint latency, external process counts and multi-client SSE CPU should be
measured on the operator's machine before claiming the original CPU lead is the
dominant cause. Full repository tests and `cargo make check` remain owned by the
parent verification run, as required by the candidate workflow.

For a manual check, throttle a report or conversation sections request, navigate
between two details and back, and verify that the current transcript/instruction
appears while secondary information is still pending. Fail a secondary request,
retry it, and confirm the existing text/draft remains. Change a stats filter twice
with reversed responses. With several connected clients, write, replace and
delete records from another process and verify that updates converge without
enabling actions from a partial task response.

## Validation performed

- `node --test tests/*.mjs`: frontend regression coverage, including nine loader
  race/failure/progressive-rendering cases in `web_progressive.mjs`.
- Targeted library tests: exact-ID ambiguity/path rejection, external replacement
  with preserved mtime and size, deletion/corruption recovery, conversation core
  isolation, bounded related-task payloads, and the `web::tests::delta_` tests.
- `RUSTUP_TOOLCHAIN=stable cargo clippy --locked --lib -- -D warnings` passed.
- The ignored before/after latency comparison passed for one and four clients.
  Full gate and browser/production profiling are not claimed by these checks.
