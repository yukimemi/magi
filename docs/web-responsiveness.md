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
  ディレクトリ全体の走査失敗はエラーとして扱う。読み取れない個別レコードは
  無関係な照会から隔離する。既知の関連レコードが破損した場合は、直前の関連を
  保持して対象の再読み取りを失敗させ、不正な「関連なし」へ変換しない。
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

## 実際の HTTP・ブラウザー比較（レビュー後に追加）

旧ハンドラーを新しい supporting code 上で実行した下表の数値は、ルート本体の
診断値に限定される。実際の origin/main と変更版の UI 比較、描画時間、SSE の
負荷改善を証明する数値としては使用しない。

コミット後、HTTP 待受と Chrome 起動が許可されたホストで実行する。
`origin/main` は手動で更新した追跡参照を指定する。スクリプトは両参照を最初に
SHA に固定し、一時ソースアーカイブから実際の router と埋め込み UI をそれぞれ
ビルドする。同一の計測ドライバーと CDP クライアントだけをコピーし、アプリ本体や
supporting code は各コミットのものを使う。作業ツリーやブランチは作成・変更しない。
共有 `CARGO_TARGET_DIR` をそのまま使用する。

```sh
python3 tests/web_latency/compare.py --baseline origin/main --candidate HEAD \
  --records 1000 --rounds 5 --output /tmp/magi-browser-comparison
```

`tests/web_browser_latency.rs` は独立した履歴を作成する。各ストアに 1,000 件の
タスク、会話、Run、質問を置き、タスク指示は約 24 KiB、会話履歴は約 16 KiB。
本物の HTTP サーバーと未改変の UI を起動し、会話・タスク・Run 詳細・レポート・
三種類の一覧・質問・ダッシュボードを新規ナビゲーションで測定する。
クライアント数は 1 と 4。非表示タブの描画抑制を避けるため、各クライアントは
独立した Chrome プロファイルとブラウザープロセスを使用する。

`first_ms` は navigation start から、画面固有の有用な DOM が可視となり、
次の animation frame を迎えるまでの時間。見出しだけのスケルトンや Loading 表示は
数えない。Run 詳細では指示パネルを開き、本文が可視となった時点を測る。
画素の提示を直接測る指標ではない。
`complete_ms` は有用な表示と初期 fetch 本文の読み込みがすべて完了した後の
animation frame の時刻。同期的な描画処理が終わるまでを含める。
200 ms の静穏期間は完了判定にだけ使い、数値には加算しない。SSE 接続確立も確認する。
各試行の失敗応答は記録して失敗させ、成功した計測として扱わない。

一覧が揃った後、接続を保った 10 秒間の CPU 時間と HTTP リクエスト数も記録する。
CPU はサーバーと CDP ドライバーを含むテストプロセスの user + system 時間で、
Chrome 子プロセスは含まない。この区間はブラウザーの評価を行わない。
静止した蓄積履歴に対する比較であり、連続書き込み中の負荷を表すものではない。
各版のログ、各試行の JSONL、固定 SHA を保存し、p50/p95 を出力する。
最初の試行にはコールドなインデックスを含み、後続試行は同一サーバーを再利用する。

このレビュー修正環境ではループバック bind が `Operation not permitted` で拒否され、
インストール済み Chrome の headless 起動も終了コード -6 で終了した。
実際の両版の UI 時間および SSE CPU 数値は未取得であり、R1-1-2 の計測結果の要件は
未完了。上記コマンドは許可されたホストで実行する必要がある。画面キャプチャの
エージェント待機タイマーをページ読み込み時間として扱っていない。

バックグラウンド Run 更新では直前の完成した表示を保持し、回答入力はそのまま
編集できる。クリックと submit の capture guard、および aria-disabled によって、
新しい core と関係情報が一致するまで操作を保留する。失敗時にも入力 DOM を維持し、
局所的な再試行を表示する。同一内容の取得成功ではアクション DOM を再構築しない。

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

## レビュー修正時の確認

- `node --test tests/web_progressive.mjs`: 16 件成功。
- 破損タスクの隔離、既知の関連タスクの破損と修復、会話 core/sections の隔離を
  対象とする Rust テスト 3 件成功。
- `RUSTUP_TOOLCHAIN=stable cargo check --locked --test web_browser_latency --test web_render` 成功。
- 同じ二つの integration target を対象とする Clippy（`-D warnings`）成功。
- 実ブラウザーのフォーカス確認はコード追加・コンパイルまで。計測スクリプトは
  ループバック待受の権限チェックで停止した。実行成功や比較数値は主張しない。
- 全体テストと最終ゲートは親の検証で実施する。
