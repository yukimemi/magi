# Run detail layout regression evidence

Verified against the launcher's fetched base `d96fe7d9bc526052f9cb39f8a76c1eda5c27b944`.
The base HTML places Pulse and tabs directly under `#view-run`. The old 1080px
rule makes that element a two-column grid, restored in split panes at 1500px;
only the header and main content span both columns. Pulse and tabs therefore
occupy separate columns, with the main content below the entire expanded Pulse.
The fix removes that obsolete inner grid and keeps the master/detail split.

The supplied image measures **1531 × 1553px**, including roughly 35px of browser
chrome. The reproduction uses a **1531 × 1518px** content viewport and the default
30% master pane. Other geometry cases cover 1499/1500px, 1079/1080px, standalone
1280/1530px, a 480px detail pane, and 320/390px phones. Saved screenshots show
collapsed and expanded Pipeline on desktop, split panes and phones.

These are controlled browser fixtures using the shipped HTML, CSS and rendering
functions, with a live e2e command and retained review-only candidates. They do
not read or control the operator's running task. Boot/network requests and the
breakpoint router are disabled so each mode can be driven independently,
including standalone desktop. The master list uses the shipped renderer too.

Run with the existing optional demo dependency:

```sh
npm ci --prefix tools/demo
MAGI_CHROME=/path/to/chrome MAGI_LAYOUT_SHOTS=docs/run-layout-evidence node tests/web_run_layout.mjs
node --test tests/web_run_progress.mjs tests/web_progressive.mjs
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib run_progress::tests
```

The browser assertions check:

- Pulse, tabs and selected content align in one column; active content follows
  the tabs and remains reachable above the fold without passing Pipeline.
- Expanding Pipeline does not move Running now; changing tabs does not change
  Pulse height, and only the selected panel is visible.
- No page or pane horizontal overflow, including expanded implementation rows.
- One recent event, no invented implementation rows for skipped/review-only
  stages, retained historical candidates with an explanation, and honest
  unrecorded durations for ordinary runs.
- Live updates retain phase elements, disclosure state, keyboard focus and
  page/pane scroll; disclosure preference returns when switching back to a run.
- Reduced-motion styling disables recent-event animation; older records without
  progress still show Running now.

The focused Rust tests cover structured applicability, retained implementation
and selection records in review-only runs, failed gates, waits and unknown
liveness. The JavaScript tests also cover observation ordering and stale action
evidence protection. Full verification and review remain parent-run owned.

Validation result: all 10 browser configurations passed, along with 77 focused
JavaScript tests and all four `run_progress::tests` Rust tests. Tab assertions
use pointer activation and keyboard arrow navigation; split/phone Timeline and
Report screenshots accompany the Overview captures.
