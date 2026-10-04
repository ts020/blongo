# スパイク3: Claude Code（stream-json 直結）

- 日付: 2026-10-03
- 実装: `crates/blongo-harness/src/claude.rs`（共通部は `process.rs`, `lib.rs`）
- 例: `crates/blongo-harness/examples/claude.rs`
- テスト: `src/claude.rs` の単体テスト、`tests/fake_agents.rs::claude_tool_approval_and_interrupt`（`tests/fixtures/fake_claude.py`）
- 参考: zeron `crates/harness/src/claude/{mod,wire,normalize}.rs`（MIT、流用箇所はソースに明記）

## 結論

`claude` CLI を Node の SDK なしで直接起動し、stream-json の入出力で 1 ターン往復、ツール承認、中断まで通せる。Node サイドカーは要らない。ハーネス側の RSS は約 3 MiB で、10 MB 流しても増えない。メモリの大半はエージェント CLI 自身（実測 約 225〜245 MiB）。

## 実装

- 起動: `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio --permission-mode default [--model M]`
- stdin: プロンプトごとに `{"type":"user","message":{...}}` を1行。承認の返事は `control_response`、中断は `control_request {subtype:"interrupt"}`
- stdout → `AgentEvent`:
  - `system/init` → `SessionStarted`（同じ id は1回だけ）
  - `stream_event` の `text_delta` / `thinking_delta` → `TextDelta` / `ReasoningDelta`
  - `assistant` の `tool_use` → `ToolCall`、`user` の `tool_result` → `ToolResult`（出力は 64 KiB で切る）
  - `control_request` `can_use_tool` → `ApprovalRequest`。Allow は `updatedInput` に元の入力を返す。AllowForSession は CLI の `permission_suggestions` を `updatedPermissions` で返す。Deny は `behavior:"deny"`
  - `result` → `TurnCompleted`（中断後は `Interrupted`、失敗時は `Error` を先に出す。`/login` 系の文言は `AuthRequired`）
- `parent_tool_use_id` 付きのフレーム（サブエージェント）は今回は捨てる
- タスク構成: ドライバ1つ + stdin 書き込み1つ + stderr 読み捨て1つ（すべて tokio タスク、OS スレッドなし）。イベントは容量 64 の有界チャネルなので、UI が遅いと stdout の読み取りが止まり、メモリは溜まらない
- 子プロセスは独自のプロセスグループで起動し、終了時は SIGTERM → 3 秒 → SIGKILL をグループに送る
- 実行ファイル: `SessionConfig::executable` → `BLONGO_CLAUDE_EXECUTABLE` → `PATH`

## 検証

| 項目 | 相手 | 結果 |
|---|---|---|
| `claude --version` | 実バイナリ `/opt/node22/bin/claude` | `2.1.288 (Claude Code)` |
| フラグの受理 | 実バイナリ | `--permission-mode default` はヘルプの選択肢（acceptEdits, auto, bypassPermissions, manual, dontAsk, plan）に無いが受理される（隠しエイリアス）。`bogus` は弾かれる。`--permission-prompt-tool` はヘルプに無いが受理される |
| control プロトコル | 実バイナリ（`env -i`、空の HOME） | `control_request {subtype:"initialize"}` に `control_response` success（コマンド一覧つき）が返る。モデル呼び出しは発生しない |
| ストリーミング | zeron の `replay-claude.py` + `tools/fixtures/resource-stream.jsonl` | 437 デルタ、51,769 バイト、17.9 秒で `TurnCompleted(completed)` |
| 承認 allow / deny、中断 | `fake_claude.py`（自作） | `can_use_tool` → `ApprovalRequest` → 返答 → `tool_result` → `result`。中断は `interrupt` → `result error_during_execution` → `Interrupted` |
| 子プロセスの異常終了 | `/bin/true` | `Error` + `TurnCompleted(Failed)` |

**注意（意図しない実行）**: 未認証の挙動を見るつもりで `env -i PATH=… HOME=<空ディレクトリ>` で例を実バイナリに対して1回実行したところ、認証が通り、"Say hello." の1ターンが完走した（`SessionStarted` → `TextDelta` 3つ → `TurnCompleted(completed)`、約2秒）。環境変数と HOME を空にしても、この環境では何らかの形で認証情報が供給される。以後は実行していない。結果として、実 CLI での1ターン往復（ツールなし）は確認できたことになる。

確認できていないこと:
- 実 CLI での `can_use_tool`（モデルにツールを使わせる必要があるため、意図して実行しなかった）。フィールド名は zeron が 2.1.228 で実機確認したものと Agent SDK に合わせている
- AllowForSession の `updatedPermissions` の形が実 CLI で受理されるか
- `AskUserQuestion`、`--resume`、画像入力、サブエージェント、steer（`priority`）

## メモリ（RSS）

| 計測 | 値 |
|---|---|
| ハーネス例（release）リプレイ中のピーク VmRSS | **2,908 KiB** |
| 同、負荷試験（`ZERON_REPLAY_REPEAT=200`、遅延0、87,400 デルタ / 10.35 MB / 7.4 秒）のピーク | **3,024 KiB** |
| リプレイ用 python（参考） | 9.9 MiB |
| 実 `claude` 2.1.288（1ターン後） | 225,356 KiB（ピーク 245,156 KiB） |

計測方法: 例の中で 100 ms ごとに `/proc/self/status` と子プロセスツリーの `/proc/<pid>/status` の VmRSS を読む（`examples/common/mod.rs`）。

## 未解決

- サブエージェントのイベントを捨てている。Phase 1 で `parent_tool_use_id` 付きのイベントをどう出すか決める
- 中断後の `control_cancel_request` で承認を取り消す処理はあるが、UI 側に「取り消された」を伝えるイベントがない
- 実 CLI の RSS（約 230 MiB）は Blongo からは削れない。複数スレッド同時実行時のエージェント数の上限を Phase 1 で決める
