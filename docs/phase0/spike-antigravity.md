# スパイク4: Antigravity（ACP、`agy_acp_server`）

- 日付: 2026-10-03
- 実装: `crates/blongo-harness/src/acp.rs`（汎用 ACP ＋ Antigravity の補正を `AcpAgent` のデータとして持つ）
- 例: `crates/blongo-harness/examples/antigravity.rs`
- テスト: `src/acp.rs` の単体テスト、`tests/fake_agents.rs::antigravity_*`（`tests/fixtures/fake_acp.py`）
- 参考: zeron `crates/harness/src/acp/{mod.rs,normalize.rs,antigravity_paths.rs}`、t3code `provider/acp/{AntigravityAcpSupport,AntigravityProtocol}.ts`、`provider/antigravityAuthSupport.ts`、`provider/antigravityRelease.ts`

## 結論

ACP の流れ（`initialize` → `authenticate` → `session/new` → `session/prompt` ＋ `session/update` ＋ `session/request_permission` → `session/cancel`）と、Antigravity 固有の補正（stdout の認証 URL、ツール出力の上限、`--uid=`、環境変数）はフェイクで通った。**実バイナリは取得できなかった**。配布元の `dl.google.com` がこの環境のプロキシで拒否される（CONNECT 403）。実機での `initialize` は未確認。

## 実装

- 起動: `agy_acp_server.par --uid=`（Linux。ACP レジストリの起動方法と同じ）
- 環境変数: `PYTHONUNBUFFERED=1`、`AGY_ACP_FORCE_FILE_STORAGE=1`（トークンを OS のキーリングでなくファイルに置く）、`BROWSER=true`（Python の `webbrowser` に実ブラウザを開かせない）。`GEMINI_API_KEY`、`GOOGLE_API_KEY`、`GOOGLE_CLOUD_*` などは子プロセスから外す（API 課金に切り替わるのを防ぐ。t3code と同じ）
- `initialize {protocolVersion:1, clientCapabilities:{fs:false, terminal:false}}`
- `authenticate {methodId:"oauth-personal"}` をセッション前に呼ぶ（エージェントがその方式を広告しているときだけ）。未サインインだとサーバーは stdout に JSON でない行 `Open the following link to authenticate the ACP server: <url>` を出してブラウザを待ち続ける。この行を見たら `AuthRequired { url }` を出して子を終了する（ハングしない）
- `session/new {cwd, mcpServers:[]}` → `SessionStarted(sessionId)`。ACP の `auth_required`（-32000）も `AuthRequired` にする
- `session/prompt` の**応答**の `stopReason` でターンが終わる（`end_turn` 等 → Completed、`cancelled` → Interrupted、`refusal` → Failed）
- `session/update`: `agent_message_chunk` / `agent_thought_chunk` → `TextDelta` / `ReasoningDelta`、`tool_call` → `ToolCall`（＋完了済みなら `ToolResult`）、`tool_call_update` の completed / failed → `ToolResult`
- Antigravity の補正（t3code `normalizeAntigravitySessionUpdate` を移植）: ツールの `title`、`rawInput`、`rawOutput`、`_meta`、`content` について、長い文字列は末尾 8,000 バイトだけ残す。`data:image/…` と画像の `data` / `blob`、`combinedOutput` と重複する `formattedOutput` は捨てる。ノード数 512、文字数 64,000（content は 32,000）の予算をかける。コマンドは `CommandLine` / `command_line` / `commandLine` / `command` のどれでも拾い、出力は `combinedOutput`、終了コードは `exitCode`
- 承認: `session/request_permission` → `ApprovalRequest`。Allow → `allow_once`、AllowForSession → `allow_always`、Deny → `reject_once` の `optionId` を `{outcome:{outcome:"selected",optionId}}` で返す。`toolCallId` が `interaction_` で始まるものは Antigravity 独自の「質問」で、選択肢を detail に並べる（Allow は先頭を選ぶ。暫定）
- 中断: 保留中の承認すべてに `{outcome:"cancelled"}` を返してから `session/cancel` 通知を送り、`session/prompt` の応答を待つ（t3code の `cancelBehavior: "wait-for-prompt"`）
- 実行ファイル: `SessionConfig::executable` → `BLONGO_ANTIGRAVITY_EXECUTABLE` → 管理インストール先 `$XDG_DATA_HOME/blongo/antigravity-acp/current/agy_acp_server.par` → `PATH`

## バイナリの入手

- ACP レジストリ `https://raw.githubusercontent.com/agentclientprotocol/registry/main/antigravity-acp/agent.json`（取得できた）の現行版は **1.3.0**。linux-x86_64 は `https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-1.3.0-linux-x86_64.zip`、`cmd: ./agy_acp_server.par`、`args: ["--uid="]`
- zeron は 1.2.1 を SHA-512 で固定、t3code は 1.1.1 を SHA-256 とサイズで固定している。t3code の記録では linux-x86_64 の zip が 682 MB、展開後の `agy_acp_server.par` が 1.88 GB、別に `localharness_external` が 129 MB（t3code は `ANTIGRAVITY_HARNESS_PATH` でその場所を渡している）
- `release_from_manifest()` はマニフェストから自分のプラットフォームの URL を選び、`https://dl.google.com/` 以外の配布元を拒否する（単体テストあり）
- ダウンロードと展開は**実装していない**。`dl.google.com` へは `curl -I` でもプロキシが 403 を返す（`/__agentproxy/status` の記録でも CONNECT 拒否）ため検証できず、HTTP クライアントと zip 展開のクレートを足す判断もできないため。Phase 1 でハッシュ固定のダウンロード（zeron の `archive_install.rs` が参考になる）を入れる

## 検証

| 項目 | 相手 | 結果 |
|---|---|---|
| レジストリのマニフェスト取得 | 実ネットワーク | 取得できた（1.3.0） |
| バイナリのダウンロード | `dl.google.com` | **不可**（プロキシで CONNECT 403） |
| 実バイナリの `initialize` | — | **未確認** |
| 1ターン往復、ツール、承認 allow_always / reject、中断 | `fake_acp.py`（自作） | 通過。stdout の JSON でない行も無視できる |
| 未サインイン | `fake_acp.py`（`FAKE_ACP_SIGNED_OUT=1`） | `AuthRequired { url: "https://accounts.google.com/…" }` を出し、セッションが終わる |
| 巨大なツール出力（200 KB の `blob`、重複 `formattedOutput`） | フェイクと単体テスト | 上限内に切り詰められる |

## メモリ（RSS）

- ハーネス例（release、フェイク相手）の VmRSS: **約 3.0 MiB**（最大 VmHWM 3,468 KiB）
- 実エージェントの RSS: 計測できていない（バイナリを取得できないため）。1.88 GB の Python `.par` なので、ほかの2つより重い可能性が高い。Phase 1 で最優先で計測する

## 依存クレートの評価: `agent-client-protocol` 2.2.0

試しにビルドした（scratch クレート、ツールチェイン 1.98.1）。問題なくビルドできるが、**採用しない**。

- 依存ツリーが 107 クレート。release、`-j 1` のコールドビルドで 2 分 17 秒（他のビルドと並行中の 4 コア機）
- `async-io`、`async-process`、`blocking`（独自のスレッドプール）、`futures-concurrency`、`schemars`、derive マクロを引き込む。tokio の上で自分のスレッドを増やさない、という方針と合わない
- 内部の `agent-client-protocol-schema =1.9.1` が serde_json の `preserve_order` と `raw_value` を有効にする。feature の統合でワークスペース全体の `serde_json::Map` が IndexMap になる
- 型だけの `agent-client-protocol-schema` 1.10.2（`default-features = false`）は 39 クレート、43 秒。それでも `serde_with`、`strum`、`derive_more`、`preserve_order` が付く
- 実際に使うメソッドは 6 つ、通知の種類も数個で、手書きは数百行で済む。ACP の型が増えてきたら、schema クレートだけを型定義として入れ直すことを再検討する

移植プラン（§スパイク4）は「`agent-client-protocol` クレートで起動」としていたが、上の理由で手書きに変える。

## 未解決

- 実バイナリでの検証すべて（`initialize` の応答、`authMethods` の実際の id、`session/update` の実際の形、RSS）。ネットワークが通る環境で最初にやる
- ダウンロードと展開、ハッシュ固定、版の更新の仕組み
- `GEMINI_HOME` の分離（t3code はインスタンスごとにプロファイルを分けている）と、サインイン専用の起動経路（今はセッション起動時に URL を出して終わるだけ）
- `ANTIGRAVITY_HARNESS_PATH`（1.1.1 では必要だった）が 1.3.0 でも要るか
- `interaction_` の質問を承認と同じイベントで出している。専用のイベントが要る
- モデル選択（`session/set_model` か `session/set_config_option`）は未実装
