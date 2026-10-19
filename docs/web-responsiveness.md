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

## ブラウザー計測結果（実機）

固定 SHA: baseline `af7f5e26453e533ba569f8c19dbe22c9cd7eeffb`、candidate はこの
ブランチの HEAD。各ストア 200 件、2 ラウンド（4 クライアントは n=8）、headless
Chrome、デバッグビルド、単一開発ホスト。値は p50 (ms)。

| 画面 | クライアント | first base → cand | complete base → cand |
| --- | ---: | ---: | ---: |
| 会話詳細 | 1 | 12353 → 324 | 12353 → 3574 |
| タスク詳細 | 1 | 842 → 104 | 16663 → 2680 |
| Run 詳細 | 1 | 1258 → 108 | 4480 → 2406 |
| Run レポート | 1 | 563 → 100 | 2923 → 3039 |
| 会話詳細 | 4 | 7915 → 670 | 12902 → 10174 |
| タスク詳細 | 4 | 3332 → 446 | 6860 → 7578 |
| Run 詳細 | 4 | 3168 → 788 | 8021 → 7085 |
| Run レポート | 4 | 3379 → 802 | 8619 → 8579 |

接続を保った 10 秒間のサーバー＋ドライバー CPU（SSE）: 1 クライアント
1.32 s → 0.18 s、4 クライアント 8.74 s → 0.56 s。

1,000 件では baseline の会話詳細が first 58 s / complete 115 s となり、ドライバーの
120 s 待機を超えて後続画面が完走しなかったため、全画面の比較は 200 件で行った
（1,000 件の candidate 側は未取得）。

残る制約: 4 クライアントでは完了時間は baseline と同程度で、改善していない。
この初回計測時点では、一覧（Queue / Chat / Questions / Stats）の first は
4 クライアントで 3.7–5 s と遅かった。下記のレビュー対応で Queue / Chat の
ページ単位の表示と Stats の独立したキュー集計を追加した。このブラウザー値は
追加修正前の値であり、追加修正後のブラウザー計測結果ではない。
Chrome 4 プロセスの同時実行によるホスト CPU 競合を含むデバッグビルドの値である。

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

タスク履歴、Run の関係情報、Stats の履歴集計には一括処理が残る。
Queue / Chat 一覧はページ単位で表示し、Stats のキュー集計は独立して表示する。 Report/detail routes still
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


## レビュー第1ラウンドの追加修正

Queue / Chat の初回取得は16件ずつ表示する。カーソルはファイル名のスナップショットを
参照し、後続ページで全ディレクトリを再列挙しない。スナップショットは最大16個、
有効期間120秒で、期限切れ・追い出しは明示的なエラーと一覧の再試行になる。
本文や状態をこのスナップショットに保持せず、各ページで実ファイルを読む。
列挙中の挿入は後続の更新で反映し、削除や破損があってもカーソルの進行を止めない。
更新中は既存の行を保持し、完全な列挙が成功してから消えた行を除去する。
途中失敗でも取得済みの行を表示し、節単位で再試行できる。

会話一覧は本文の先頭240文字と全ターンの時刻を転送する。Markdown 変換と
リポジトリ設定探索を省き、未読数は正確に維持する。コンテキスト使用量の分母は
詳細を開いてから取得する。通常の詳細・変更 API は全文の応答を維持する。
Queue の従来の全件 API でも、依存関係のために同じキューを読み直す処理を除去した。
ページ内の依存関係は一つの共有 Inventory で必要な依存先だけを読み取る。

Stats はキュー集計と履歴集計を別々に要求する。キューは履歴の完了・成功を待たずに
表示し、片方の失敗でも他方の結果を保持する。キュー集計は既存の共有インデックスを
利用し、毎回のメタデータ照合で外部プロセスの更新を検出する。置換・削除・破損した
レコードの以前の状態を集計に使わない。変更のないレコードは再解析しない。
キューだけの更新通知では Run 履歴を再集計せず、キュー集計だけを更新する。
health の通知 revision はリクエスト時点で取得し、共有監視の次の tick を待たない。

追加した回帰テストは、1,000件の履歴、ページ外の依存先、削除後のカーソル進行、
カーソルの期限切れ、外部書換・破損後の集計、一覧の途中失敗と再試行、履歴集計が
失敗してもキューを先に表示する動作を検証する。詳細の安全性・移動後の応答破棄を
検証する既存テストのアサーションも現在のローダーに合わせて更新した。

再現コマンド:

```sh
node --test tests/web_progressive.mjs tests/web_list_progressive.mjs
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib web_list_latency_measurement -- --ignored --nocapture
```

このラウンドの環境はループバックの bind を `Operation not permitted` で拒否する。
そのため追加修正後の HTTP / Chrome 計測は取得していない。追加の計測は
`tests/web_latency/lists.rs` で、origin/main
`4436002a35dda29db2b12072554b769a1d138cba` の Queue 処理を再現し、全件版の
会話・Stats ハンドラーとページ版・独立したキュー集計を比較する。
各ストア1,000件、タスク指示24 KiB・会話の保留本文16 KiB、既定では各条件3バッチ、
1 / 4 クライアントで初回データと全件取得を測る。JSON のシリアライズを含むが、
HTTP・DOM・描画時間は含まない。既存のブラウザー計測を置き換えるものではない。

残る制約として、全件取得時には各レコードの解析が必要で、タスク一覧は引き続き
指示本文を含む。初期のファイル名列挙とキュー索引のメタデータ照合は履歴件数に比例する。
Stats の履歴集計や health の設定・ディスク・未読質問の処理は引き続き独立した最適化候補である。
全テストと最終ゲートは親の検証に委ねる。


最終のハンドラー修正に対する計測結果（ミリ秒）:

| 対象 | クライアント | 従来の初回 / 完了 | 修正後の初回 / 完了 |
| --- | ---: | ---: | ---: |
| Queue | 1 | 3785.77 / 3785.77 | 91.43 / 5111.40 |
| Queue | 4 | 11406.40 / 11406.40 | 196.14 / 11542.07 |
| Chat | 1 | 1653.56 / 1653.56 | 10.02 / 343.00 |
| Chat | 4 | 3193.19 / 3193.19 | 17.34 / 431.64 |
| Stats | 1 | 1731.20 / 1731.20 | 496.62 / 1247.98 |
| Stats | 4 | 1988.20 / 1988.20 | 65.46 / 1524.86 |

この最終計測は `MAGI_BENCH_RECORDS=1000 MAGI_BENCH_ROUNDS=1` で上記の
計測テストを実行したもの。各条件1バッチなので単一クライアントは1標本、
4クライアントは4標本の上側中央値（ソート後の `len / 2` 番目）であり、
安定した分位点や本番保証ではない。ファイル作成後に従来版を先に実行したため
ファイルシステムは温まっている。Stats の修正後1クライアントには初回の
キュー索引構築を含み、4クライアント時は構築済みである。

Chat の合計転送量は約17.3 MBから約0.32 MBになった。Queue は本文を維持するため
約50.3 MBのままで、全件取得は1クライアントで遅く、4クライアントでもほぼ同じだった。
Queue では初回表示を先に返し、二重のキュー解析とページごとの全件ファイル名走査は
除去したが、全文と Markdown の全件処理やページの往復コストまでは除去していない。
Stats の初回表示は独立したキュー集計が利用可能になった時点、完了は履歴集計も
揃った時点である。冷えた索引の構築時間をゼロとは扱わない。

検証結果は JavaScript 全77件成功、対象を完全なテスト名で指定した Rust 全8件成功、
`cargo fmt -- --check` 成功。全テストと最終ゲートは実行していない。
一覧を対象にした Rust の部分実行では21件成功、HTTP のループバックを必要とする
9件はすべて bind の権限拒否で失敗した。この9件を成功扱いにはしていない。

## レビュー第2ラウンドの追加修正

一覧の取得中に新しい通知が届いても、現在のカーソルを先頭に戻さず、最後まで進める。
取得中の連続した差分通知は変更・削除 ID をまとめ、列挙の成功後に反映する。
同じ通知を質問とキューの更新経路から重複して受けても、まとめた差分を失わない。
差分の基準が飛んだ場合や明示的な全件更新では、現在の一覧を最後まで表示してから
新しいスナップショットを取得する。未完了の列挙を新しい revision として扱わない。

質問画面は `/api/questions/core` で本文と回答に必要な情報を先に表示し、
`/api/questions/origins` で出所と相談可否を独立して補完する。後者が失敗しても
本文・回答入力を保持し、出所の節だけにエラーと再試行を表示する。出所の応答は
取得世代と質問レコードの指紋を照合し、古い応答や取得中に変化した質問に対して
相談操作を有効にしない。空の質問一覧では出所の追加要求も履歴の参照も行わない。

出所の解決は、質問が指すタスクと必要な来歴、その会話だけを直接取得する。
Run からタスクを探す場合と旧形式の来歴は既存の共有索引で照合し、同じ要求内の
タスクや会話は再利用する。質問本文は索引の初回構築を待たない。質問への回答・
追記・相談の応答にも同じ参照処理を使い、全会話や全タスク本文の独立した走査を除去した。
相談の適格性は既存の `consult` の判定を維持し、実行時にも最新の質問を確認する。

再現可能な回帰テスト:

```sh
node --test tests/web_list_progressive.mjs tests/web_questions_progressive.mjs
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib -- --exact web::tests::question_core_and_empty_origins_do_not_touch_unavailable_histories web::tests::question_origin_lookup_matches_legacy_with_large_and_changing_histories
```

前者はページごとの連続更新・重複通知・差分の欠落、出所の遅延と失敗、応答の逆転、
質問変更時の相談操作の抑止を出荷ローダーで検証する。後者は履歴ディレクトリが
利用不能でも本文を取得できること、空の質問一覧では履歴に触れないこと、
1,000件の履歴で旧方式と同じ出所を解決すること、来歴の旧形式・親の削除・
会話の削除や質問の終了を検証する。共有索引の初回構築・メタデータ照合と、
参照先の会話本文の読み込みは出所の節に残る。この追加修正後のブラウザー実測値は
取得しておらず、第1ラウンドの計測表を更新後の値として流用していない。
