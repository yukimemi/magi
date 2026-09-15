# Add `omp` (oh-my-pi) as a built-in agent kind

magi drives subscription CLIs, and `omp` is one — it carries the operator's own
plan, holds DeepSeek in its built-in catalog, and exposes a whole tool loop.
Today it can only be reached through `kind = "command"` plus a wrapper script
outside this repository, which means every operator who wants it has to
reinvent the same JSON unwrapping. Make it a first-class [`AgentKind`] the way
`codex` and `opencode` are.

## The mechanics, as verified by hand

These were established by running `omp` v18.1.19 on this machine, not read from
documentation. Treat them as the spec; if a future `omp` changes one, the tests
added here are the alarm.

### Command line

- One-shot turn: `omp -p --mode=json --auto-approve --model <model>`
- Follow-up turn: `omp -p --mode=json --auto-approve --model <model> --resume <id>`
- `-p` / `--print` is non-interactive: process the prompt, print, exit.
- The prompt is read from **stdin**. It may also be given as an argv argument,
  but `magi`'s judging prompts exceed the Windows argv limit, so stdin is the
  only delivery that works for every node — `Delivery::Stdin`, like `codex`.
- `--auto-approve` is required. Without it an unattended seat stops at the
  first tool call and blocks until the node timeout kills it. Note that `omp`
  gates reads and writes together behind it, so this adapter has the same
  character as `opencode`: read-only-ness rests on the prompt plus the fact
  that judge worktrees are deleted after the tally and reviewer worktrees are
  reset to the commit under review every round. Say so in a comment, exactly as
  the opencode arm does, so nobody later mistakes it for an oversight.

### Session id

`--mode=json` writes a JSONL event stream. The **first** line is:

```json
{"type":"session","version":3,"id":"01a09fe9-…","timestamp":"…","cwd":"…"}
```

That `id` is the resume token. Capture it from the first line carrying
`"type":"session"` and hand it to `captured_session`, the same field `opencode`
and `agy` use — magi cannot mint this one up front, so `has_session` must keep
answering `false` until a turn has reported an id.

### The answer

The answer is the **last non-empty assistant text block** in the stream. Three
carriers have to be walked, because the shape of the turn decides which one is
present:

| event | carries |
|---|---|
| `agent_end` | `messages` — the whole thread |
| `turn_end` | `message` — the last assistant message |
| `message_end` | `message` — any message, assistant ones included |

Two traps, both hit during the hand verification:

1. **`agent_end` is not always emitted.** A turn that ends on a tool call
   (`stopReason: "toolUse"`) ends the run with **no `agent_end` line at all**.
   The first naive extractor looked only for `agent_end` and silently threw
   away three complete reviews. The final answer routinely arrives in
   `message_end` / `turn_end` instead.
2. **Earlier assistant text is not the answer.** The model narrates its tool
   loop, sometimes with a single `.`, so taking the first assistant text block
   hands the caller a progress note. Take the last non-empty one.

### Non-ASCII is load-bearing

Seats on this machine are configured to write prose in Japanese
(`[graph] language`), and a stream containing Japanese must survive intact: an
earlier wrapper in PowerShell damaged every non-ASCII character because it
passed the bytes through the shell's string pipeline. magi reads the child's
stdout as bytes and lossily converts at the end (`String::from_utf8_lossy`), so
the Rust side is fine — but the test added below must use a non-ASCII answer so
a future regression in the pipe cannot pass unnoticed.

## Mechanical constraints, exactly

1. `AgentKind` gains exactly one variant, `Omp`, serialised as `"omp"`, with
   `program()` returning `Some("omp")`.
2. `AgentSpec::delivery()` gives `Omp` the same default as `Claude`, `Codex`
   and `Command`: `Delivery::Stdin`.
3. `has_session` treats `Omp` like `Opencode`/`Antigravity`/`Codex`: it answers
   `true` only when `captured_session` is `Some`.
4. `build_command` builds the argv listed above, with the same
   "resuming ⇒ `--resume <captured id>`" structure the other arms use, and
   emits the prompt through the existing `Delivery::Stdin` path.
5. `extract` gains an `Omp` arm implementing the table above: session id from
   the first `"type":"session"` line, text from the last non-empty assistant
   text block across `agent_end` / `turn_end` / `message_end`.
6. `Config::autodetected` gains an `(AgentKind::Omp, "omp", None)` entry so a
   machine with `omp` on `PATH` gets it in the derived roster, in the same
   style as the entries already there.
7. `missing_programs` / any exhaustiveness the compiler reports must be
   satisfied without weakening the checks for the existing kinds.

## Tests to add (in `src/agent.rs`, in the existing style)

- `omp` is sandboxed-by-prompt like opencode, not by the CLI: assert the argv
  contains `--auto-approve` and that `--dangerously-bypass-approvals-and-sandbox`
  appears nowhere in the built command.
- The prompt reaches stdin when resuming as well as on the first turn.
- `--resume <id>` is emitted only when `has_session` says there is an id, and
  the id is the captured one.
- An extraction fixture that ends on **`message_end`** with no `agent_end` line
  still yields the answer — this is the regression that cost three reviews.
- An extraction fixture whose last assistant text is a tool-loop narration and
  whose final answer came earlier is **not** what the current fixtures assume:
  assert the last non-empty block wins.
- A non-ASCII answer survives the extraction byte-for-byte.

## Documentation

`README.md`'s conversation-continuity table and the per-CLI table in
`AGENTS.md` are the two places that list session mechanics per CLI. Both must
gain an `omp` row (`-p --mode=json` reports `id` on its first line |
`--resume <id>`), and the `AGENTS.md` row must carry the trap: *`agent_end` is
absent when a turn ends on a tool call; the answer is the last non-empty
assistant text block, and earlier ones narrate the tool loop.*

## Out of scope

- Removing `kind = "command"`: it stays the escape hatch for anything else.
- Any HTTP/API path: magi drives CLIs, and `omp` is a CLI.
