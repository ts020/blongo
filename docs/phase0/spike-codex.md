# スパイク2: Codex（`codex app-server`）

- 日付: 2026-10-03
- 実装: `crates/blongo-harness/src/codex.rs`（JSON-RPC は `jsonrpc.rs`、子プロセスは `process.rs`）
- 例: `crates/blongo-harness/examples/codex.rs`
- テスト: `src/codex.rs` の単体テスト、`tests/fake_agents.rs::codex_tool_approval_and_interrupt`（`tests/fixtures/fake_codex.py`）
- 参考: zeron `crates/harness/src/{codex/mod.rs,jsonrpc.rs}`、t3code `CodexAdapterV2.ts`

## 結論

`codex app-server` と JSON-RPC 2.0（改行区切り）で話す実装は手書きの serde_json で十分。実バイナリ 0.160.0 に対して、ログインなしで `initialize` → `account/read` → `thread/start` → `turn/start` まで通った。ターン本体（デルタ、承認、完了）はネットワークが塞がれているためフェイクで確認した。npm の `codex` は Node のシムなので、同梱のネイティブバイナリを直接起動すると **約 48 MiB 減る**。

## 実装

- 手順: `initialize`（`capabilities.experimentalApi: true`）→ `initialized` 通知 → `account/read`（未ログインなら `AuthRequired` を出すが続行）→ `thread/start {cwd, approvalPolicy, sandbox}` → `SessionStarted(thread.id)`
- プロンプト: `turn/start {threadId, input:[{type:"text"}], approvalPolicy, cwd, summary:"auto"}`。ターン中のプロンプトは手元のキューに積み、`turn/completed` 後に次のターンとして送る
- 通知 → `AgentEvent`:
  - `item/agentMessage/delta` → `TextDelta`（デルタが来なかった `agentMessage` は `item/completed` の本文で補う）
  - `item/reasoning/summaryTextDelta` / `textDelta` → `ReasoningDelta`
  - `item/started` / `item/completed` の `commandExecution`, `fileChange`, `mcpToolCall`, `webSearch` → `ToolCall` / `ToolResult`
  - `turn/completed` の `turn.status`（completed / interrupted / failed）→ `TurnCompleted`
  - `error` は `willRetry: false` のときだけ `Error`（再接続中の "Reconnecting... n/5" は出さない）
  - 別スレッド（サブエージェント）の通知は無視
- 承認: `item/commandExecution/requestApproval`, `item/fileChange/requestApproval` → `ApprovalRequest`。返事は `{decision: accept | acceptForSession | decline}`。`serverRequest/resolved` で保留を消す
- 中断: 保留中の承認には `cancel`（スキーマ上「拒否してターンも中断」）を返し、`turn/interrupt {threadId, turnId}` を送る。turn id がまだ無ければ `turn/started` か `turn/start` の応答を待って送る
- それ以外のサーバー要求（`item/tool/requestUserInput`, `item/permissions/requestApproval`, `mcpServer/elicitation/request`, `item/tool/call` など）は -32601 で断る（エージェントを止めないため）
- JSON-RPC 層: リーダータスクも共有 pending マップも持たない。ドライバが stdout を直接読み、応答は id で自分の要求と突き合わせる。セットアップ中だけ `call()` で応答を待ち、その間の通知は小さなバックログに退避する
- 実行ファイル: `SessionConfig::executable` → `BLONGO_CODEX_EXECUTABLE` → `PATH`。見つかったのが npm シム（`…/@openai/codex/bin/codex.js`）なら `node_modules/@openai/codex-<platform>/vendor/<triple>/bin/codex` を探して直接起動する（`CodexOptions::prefer_native`）。シムが付ける `CODEX_MANAGED_BY_NPM=1` は同じく付ける

## 実バイナリでの検証（codex-cli 0.160.0、`npm i -g @openai/codex`、`CODEX_HOME` は空の一時ディレクトリ）

| 要求 | 実際の応答 |
|---|---|
| `initialize` | `{"userAgent":"blongo/0.160.0 (Ubuntu 24.4.0; x86_64) linux (blongo; 0.0.1)","codexHome":"…","platformFamily":"unix","platformOs":"linux"}`。続けて通知 `configWarning`（bubblewrap が PATH に無いので同梱版を使う）と `remoteControl/status/changed {status:"disabled"}` |
| `account/read {}` | `{"account":null,"requiresOpenaiAuth":true,"workspaceRouting":null}` → `AuthRequired` を出す |
| `model/list {}` | ログインなしで成功。既定は `gpt-6.1-sol`（他に `gpt-6-astra` など、`supportedReasoningEfforts` に low〜max と ultra） |
| `thread/start {cwd, approvalPolicy:"on-request", sandbox}` | ログインなしで成功。`thread.id`、`model:"gpt-6.1-sol"`、`sandbox:{type:"readOnly",networkAccess:false}` など。通知 `thread/started` |
| `turn/start` | 応答 `turn.status:"inProgress"` → `thread/status/changed` → `turn/started` → `item/started`/`item/completed`（`userMessage`）→ `error {willRetry:true,"Reconnecting... 2/5"…}`（`wss://api.openai.com/v1/responses` へのプロキシ CONNECT が 403）→ `warning`（HTTPS へフォールバック）。90 秒待っても `turn/completed` は来なかった |

プロトコルの観察:
- 通知には `"jsonrpc"` が付かず、`emittedAtMs` が付く。パーサは `jsonrpc` が無いことを許容している
- 0.160 のスキーマ（`codex app-server generate-json-schema`）には `turn/failed` / `turn/aborted` が無い。zeron は古い版のために両方を扱っている
- `thread/start` 直後に codex が git を起動して何かを取得している（`git-remote-http` が子に見える）。プラグインの同期と思われる

確認できていないこと: 実バイナリでのデルタ、ツール実行、承認要求、`turn/interrupt`（いずれもフェイクでのみ確認）。

## フェイクでの検証（`fake_codex.py`）

実スキーマの名前と形（通知に `jsonrpc` なし、`emittedAtMs` あり）に合わせた。1ターン目で `commandExecution` の承認 → acceptForSession → 出力 → `turn/completed`、2ターン目で decline、3ターン目でデルタ連続中に `turn/interrupt` → `interrupted`、4ターン目で承認待ち中の中断（`cancel`）を通している。

## メモリ（RSS）

| 計測 | 値 |
|---|---|
| ハーネス例（release）の VmRSS | **約 2.9〜3.1 MiB**（全計測で最大 3,084 KiB） |
| ネイティブ codex、`initialize` 直後 | 96,988 KiB |
| ネイティブ codex、`thread/start` 後 / 5 秒アイドル後 | 131,216 KiB / 133,352 KiB |
| ネイティブ codex、ネットワーク失敗中のターン | 143,444 KiB |
| npm シム経由（node + codex）、5 秒アイドル後 | node 48,980 KiB + codex 132,984 KiB = 181,964 KiB |
| エージェントツリーのピーク（git 子プロセス込み） | ネイティブ 166,008 KiB / シム経由 212,004 KiB |

## 依存クレートの評価: `codex-app-server-protocol`（openai/codex の git）

採用しない。このクレートは openai/codex ワークスペースの中にあり、`codex-protocol`, `codex-rollout`, `codex-secrets`, `codex-shell-command`, `codex-history`, `rmcp`, `zstd`, `inventory` などに依存する（`codex-rs/app-server-protocol/Cargo.toml` を確認）。git 依存にするとリポジトリ全体の取得と、それらのビルドが付いてくる。実際に使う型は十数個なので手書きで足りる。代わりに、インストール済みの CLI から `codex app-server generate-json-schema`（または `generate-ts`）でスキーマを出し、Phase 1 でフェイクと実装の名前がずれていないかを確かめるテストに使う。

## 未解決

- 再試行中（`willRetry: true`）は UI に何も出ない。状態表示用のイベントが要るかもしれない
- サブエージェントのスレッド、`requestUserInput`、権限昇格（`item/permissions/requestApproval`）は未対応で、断っている
- バージョン固定の方針（実験的 API なので、対応バージョンの範囲を決める）
- 未ログイン時に `thread/start` は通ってしまうので、本当のエラーはターン中のネットワークエラーとして来る。`account/read` の結果だけで UI のサインイン導線を出すか決める
