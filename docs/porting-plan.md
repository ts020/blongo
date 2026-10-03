# Blongo: t3code → Rust + GPUI 移植プラン

- 作成日: 2026-10-03
- 対象: [pingdotgg/t3code](https://github.com/pingdotgg/t3code) `8ed276c246b624631e7d39241ebfd22d8314cb68`（2026-10-02 時点の main）
- GPUI: [zed-industries/zed](https://github.com/zed-industries/zed) `crates/gpui`（調査時点の main `badfb8d`）
- ゴール: t3code を GPUI ネイティブアプリとして作り直し、「世界一高速で低メモリなエージェントオーケストレーター」にする

---

## 0. 結論（先に要点）

1. **t3code の本体は「Node サーバー（オーケストレーター）＋薄いクライアント」**。重さの源は Electron（Chromium レンダラー + Node 本体 + 子プロセスの Node サーバー）で、ロジックの中心はイベントソーシングの `orchestration-v2`。
2. **t3code とのワイヤ互換は持たない（2026-10-03 決定）**。既存の t3 サーバー、Web、モバイルとは接続しない。
   - Phase 1 から Rust コア（イベントストア＋オーケストレーター＋プロバイダーアダプター）と GPUI UI を同時に縦に通す。最初から Node も Electron も使わない。
   - Effect RPC の再現が不要になるので、ワイヤ形式は Blongo 側で自由に最適化できる。
   - クライアントは `Backend` トレイトの裏で「プロセス内コア（LocalBackend）」と「`blongo serve`（RemoteBackend、Phase 3）」を差し替えられる設計にする。
3. **ドメインモデルはそのまま借りる**（Project → AppThread → Run → ExecutionNode、TurnItem、コマンド／イベント／プロジェクション／アウトボックス）。t3code が何度も作り直して到達した形なので、再発明しない。
4. **初期の対応エージェントは Codex、Claude Code、Antigravity の3つ（2026-10-03 決定）**。それ以外（Grok, OpenCode, Pi, Cursor, ACP レジストリ等）は後で検討する。Codex は公式 Rust クレートがあり容易。Antigravity は ACP なので `agent-client-protocol` クレートで話せるが、独自の出力補正と認証がある。Claude は JS SDK を介さず CLI の stream-json を直接話す実装が必要。
5. **比較対象は zeron**（同ジャンルの GPUI 実装）。Blongo は zeron より明確に軽いことを目標にし、アイドル RSS 100 MiB 以下（zeron 206 MiB）を狙う。差を生むのは CRDT を持たないこと、プロセス内でシリアライズしないこと、すりガラス等の GPU 演出を持たないこと、範囲を絞ること（§0.5）。zeron は MIT なのでハーネス実装は参考・流用できる。
6. **ライセンス注意**: GPUI 本体は Apache-2.0 だが、Zed の `editor` / `terminal` / `markdown` / `ui` / `acp_thread` などは **GPL-3.0**。Blongo は MIT なので、これらはコピーも依存もしない。参考に読むだけにする。

---

## 0.5 比較対象 zeron と Blongo の存在意義

[zeronsh/zeron](https://github.com/zeronsh/zeron)（`9e1a111`, 2026-10-02, MIT）は同ジャンルの GPUI 実装で、Claude Code / Codex / ACP（Antigravity を含む）/ OpenCode / Pi / Cursor を Rust で動かしている。規模は Rust 約39万行（ui 18.4万、engine 7.9万、harness 4.9万）。**Blongo は zeron より明確に軽くなければ存在意義がない**、を前提にする（2026-10-03 takanori）。

### zeron の実測値（zeron 自身のドキュメントから）

| 指標 | 値 | 出典 |
|---|---|---|
| UI＋エンジン合計のアイドル RSS（Linux ソフトウェア Vulkan、release） | 206 MiB | `docs/performance-resource-usage.md` |
| 同ストリーミング中ピーク RSS | 235 MiB | 同上 |
| アイドル CPU / ストリーミング CPU | 7.5% / 159%（1コア=100%） | 同上 |
| macOS アイドル時の physical footprint | 中央値 約160 MiB、最大 約383 MiB | `docs/performance-idle-presence.md` |
| エンジン単体アイドル | 32 MB | `docs/memory-plan.md` |
| ストリーミング中の保持量（修正前） | 生テキストの約11.6倍 | 同上 |
| 修正前の利用実態 | 450〜600 MB、重用で 1 GB 超 | 同上 |

### zeron が重くなっている理由（読み取った範囲）

1. **CRDT（Loro）がデータモデルの中心**: 全メッセージ本文が LoroText で、oplog・ミラー層・UI 側コピーと多重に持つ。同期を使わないローカルでも同じ構造
2. **プロセス内でもシリアライズする**: 「境界を正直に保つ」ためにインメモリ二重管でも RPC を通し、watch フレームを約4コピー作っていた（後に差分化で 110 分の1 に改善）。UI はフレームごとにトランスクリプト全体を複製している（zeron 自身の既知課題）
3. **見た目の GPU コスト**: すりガラス（backdrop blur）、エッジフェード、常時アニメーション。Metal の描画投入だけで footprint が一時的に最大約210 MiB 跳ねることを zeron 自身が計測している
4. **機能の広さ**: マルチデバイス同期、Cloudflare エッジ、WorkOS 認証、プレゼンス、音声認識モデル、プレビュー、モバイル、6種のハーネス
5. **後付けの対策**: mimalloc、glibc の arena 対策（`malloc_trim` を毎分）、画像 LRU、ドキュメント LRU は、問題が出てから入れている

### Blongo が軽くなるための設計（zeron との差分）

| 項目 | zeron | Blongo |
|---|---|---|
| 永続化 | Loro CRDT ドキュメント＋SQLite スナップショット | 追記型イベントログ（SQLite）。本文は1回だけ保存し、CRDT は持たない |
| ストリーミング中の本文 | LoroText＋ミラー＋UI コピー | 追記バッファ1本を `Arc` で UI と共有。目標は生テキストの1.5倍以内 |
| UI とエンジンの境界 | プロセス内でもシリアライズ | プロセス内は型付きイベントをチャネルで渡すだけ。シリアライズはリモート時のみ |
| UI の状態 | フレームごとにトランスクリプト全体を複製 | エントリ単位の `Arc`。変わった行だけ通知 |
| 見た目 | すりガラス、エッジフェード、常時アニメーション | 不透明でフラット。アニメーションは状態遷移時だけで、アイドル時の描画投入はゼロ |
| 範囲 | 同期、エッジ、認証、音声、モバイルなど | ローカル単体、エージェント3種 |
| メモリ規律 | 問題発生後に LRU・mimalloc・trim | 初日から上限付きキャッシュ、アロケータ設定、スレッド数の上限 |

### 目標値（zeron と同じ負荷・同じ環境で比較）

zeron の計測負荷（Haiku の出力 52KB の Markdown を 40ms 間隔で流す、1280×800 窓）をそのまま使い、同じマシンで zeron と並べて測る。

| 指標 | zeron | Blongo 目標 |
|---|---|---|
| アイドル RSS（UI＋エンジン） | 206 MiB | **100 MiB 以下** |
| ストリーミング中ピーク RSS | 235 MiB | **120 MiB 以下** |
| エンジン単体アイドル | 32 MB | **15 MB 以下** |
| ストリーミング保持量 | 生テキストの3倍以内を目標に改善中 | **1.5倍以内** |
| アイドル CPU | 7.5% | **ほぼ0%**（描画もタイマーも止まる） |

目標値は計画段階の数字で、まだ測っていない。Phase 0 で両者のベースラインを取ってから確定する。

### zeron から借りるもの

zeron は MIT なので、著作権表示を残せばコードを流用できる。
- **ハーネス実装**: `crates/harness/src/claude`（stream-json 直結、約3.5k行）、`codex`（app-server、約3.9k行）、`acp`（Antigravity 対応を含む、約9.4k行）。Claude と Antigravity のスパイクは、ここを読むところから始める
- **調査メモ**: `docs/research/{gpui,harness,acp,mugen-pretext}.md`、`docs/memory-plan.md`、`docs/performance-*.md`
- **タイムラインの技法**: Markdown ブロック単位の行、行高さのメモ化（行 ID・本文長・幅をキー）、末尾追従のスプリング、ストリーミング末尾だけの再解析
- **gpui フォークの修正**: zeron は Zed の gpui をフォーク（`zeronsh/zui`）して、GPU メモリの上限、`ImageSource::evict`（画像アトラスのリーク修正）、インスタンスプールの縮小を入れている。上流の gpui をそのまま使うと同じリークを踏む可能性があるので、Phase 0 で「上流 gpui＋必要な修正だけ当てる」か「zui を使う」かを決める

借りないもの: Loro / 同期 / エッジ / WorkOS / 音声 / すりガラス系の描画。

---

## 1. t3code の全体像

### 1.1 構成と規模（.ts/.tsx、テスト込み）

| 場所 | 規模 | 役割 | 移植での扱い |
|---|---|---|---|
| `apps/server` | 約51.9万行（非テスト約24.9万） | WebSocket/HTTP サーバー。オーケストレーター、プロバイダー、git、PTY、MCP | **Rust コアとして移植（中心）** |
| `apps/web` | 約34万行（非テスト約23.4万） | React 19 クライアント（デスクトップのレンダラーも兼ねる） | **GPUI で作り直し** |
| `packages/client-runtime` | 約7.3万行（非テスト約3.4万） | 接続管理、RPC、プロジェクション reducer、履歴ページング | **Rust で作り直し（クライアント状態層）** |
| `packages/contracts` | 約3.2万行 | Effect Schema によるワイヤ型（`orchestrationV2.ts` 3.3k, `rpc.ts` 1.9k） | **serde 型として移植** |
| `packages/effect-acp` | 約2.8万行（生成1.9万） | Agent Client Protocol クライアント | `agent-client-protocol` クレートで置換 |
| `packages/effect-codex-app-server` | 約6.3万行（生成5.7万） | Codex app-server JSON-RPC | 上流 `codex-app-server-protocol` で置換 |
| `packages/shared` | 約3.3万行 | DPoP、ワーカー、ログ、テーマ等の雑多な共有物 | 必要な部分だけ |
| `packages/ssh`, `packages/tailscale` | 約0.5万行 | SSH 環境、tailscale serve | Phase 3 以降 |
| `apps/desktop` | 約8万行 | Electron メインプロセス（サーバー同梱、IPC、更新、プレビューブラウザ、スクショ） | GPUI アプリ本体に吸収 / 一部は対象外 |
| `apps/mobile` | 約15万行 | Expo/React Native | **対象外**（ワイヤ互換なしと決定済み） |
| `infra/relay` | 約2.7万行 | T3 Connect（Cloudflare Workers の制御プレーン） | **対象外**（クライアント側の接続部分のみ将来検討） |
| `native/*` | 小 | Rust 製の resource-monitor、Hyprland/KDE スクショ、C 製 browser-secret、libghostty-vt ヘッダ | resource-monitor はライブラリとして取り込み可 |

### 1.2 ランタイム構造（現行）

```
Electron main ──spawn──▶ Node: apps/server ──spawn──▶ codex app-server / claude CLI / grok / opencode / pi ...
     │                        ▲   │ SQLite (statev2.sqlite, WAL)
     └─ Chromium renderer ────┘   │ node-pty, git CLI, gh/glab, MCP server (/mcp)
        (apps/web, React)  WebSocket (Effect RPC JSON) + HTTP
```

デスクトップでも UI とサーバーは WebSocket 越しに話している。リモート（LAN, Tailscale, T3 Connect, SSH）と同じ経路を使うためで、t3code が「Remote ready」を譲れない原則にしている理由でもある。

### 1.3 オーケストレーション v2（移植の心臓部）

v1 は削除済みで、`apps/server/src/orchestration-v2/`（非テスト約7.9万行、うちアダプター約4.5万行）が現行。設計文書は `docs/orchestration-v2/*.md`。

**データモデル**
- `Project` → `AppThread`（会話）→ `Run`（ユーザーから見た1ターン）→ `RunAttempt` → `ExecutionNode` ツリー（root, tool, approval, subagent, plan, checkpoint scope）
- 並行して `ProviderSession`（生きているプロセス）、`ProviderThread` / `ProviderTurn`（プロバイダー側ハンドル）、`RuntimeRequest`（承認・ユーザー入力要求）、`Plan`、`Subagent`、`Checkpoint`、`ContextTransfer` / `ContextHandoff`（フォーク、プロバイダー切替、マージバック）
- 画面に出る単位は `TurnItem`: `user_message`, `assistant_message`, `reasoning`, `proposed_plan`, `todo_list`, `user_input_request`, `approval_request`, `file_change`, `command_execution`, `file_search`, `web_search`, `dynamic_tool`, `checkpoint`, `subagent`, `compaction`, `handoff`, `fork`, `error` など
- Run の状態: `preparing | queued | starting | running | waiting | completed | interrupted | failed | cancelled | rolled_back`

**不変条件（そのまま引き継ぐ）**
- アプリ ID が主、プロバイダー ID は参照。変換はアダプターの境界で行う
- プロバイダーのイベントを別プロバイダー風に書き換えない
- ターンを完了させるのはルート Run の完了だけ。子（サブエージェント）の完了で親は閉じない
- ロールバックはアプリ側の Run 数で表す
- 振る舞いはプロバイダー名ではなく `ProviderCapabilities`（バージョン付き）で分岐する

**書き込みパス**
1. `dispatch(command)` はスレッドごとの直列実行器（`KeyedSerialExecutor`）で処理
2. `CommandPolicy` が意図を解決（I/O なしで決定）
3. `EventSink.commitCommand` が **1トランザクションで** ドメインイベント、プロジェクション更新、コマンドレシート、アウトボックス行を書く
4. 確定したシーケンス番号を返す。同じ `commandId` の再送はレシートを返すだけで副作用を再実行しない
5. `EffectWorker` がアウトボックスをリースして副作用（`provider-turn.start/steer/interrupt`, `runtime-request.respond`, `checkpoint.capture`, `thread-title.generate` など）を実行。プロセス依存の副作用はクラッシュ後に破棄、リプレイ安全なものは再試行

**永続化（SQLite, WAL）**
- `orchestration_events`（統合イベントログ: sequence, stream_id, stream_version, event_type, command_id, causation/correlation, payload_json …）
- `orchestration_command_receipts`
- `orchestration_v2_projection_{threads, runs, run_attempts, nodes, turn_items, messages, plans, runtime_requests, subagents, provider_sessions, provider_threads, provider_turns, checkpoints, ...}`
- `orchestration_v2_effect_outbox`、`projection_projects`、`scheduled_tasks`、`auth_*` など
- マイグレーション 001〜056 のうち v1 関連は Blongo では不要

### 1.4 プロバイダーアダプター

共通インターフェース `ProviderAdapterV2Shape`（`orchestration-v2/ProviderAdapter.ts`）:
- インスタンス: `getCapabilities()`, `planSelectionTransition()`, `openSession()`
- セッション: `events: Stream<Event>`, `ensureThread`, `resumeThread`, `startTurn`, `steerTurn`, `interruptTurn`, `respondToRuntimeRequest`, `readThreadSnapshot`, `rollbackThread`, `forkThread`（任意で `compactThread`, `injectHistory` など）
- イベント: `provider_session/thread/turn.updated`, `node.updated`, `message.updated`, `turn_item.updated`, `runtime_request.updated`, `plan.updated`, `subagent.updated`, `turn.terminal`

| プロバイダー | t3code の実装 | 通信方式 | Rust での方針 | 難易度 |
|---|---|---|---|---|
| Codex | `CodexAdapterV2.ts` 6.3k | `codex app-server` と stdio JSON-RPC | 上流 `codex-rs/app-server-protocol` を git 依存で使う | 低 |
| Antigravity | `AntigravityAdapterV2.ts` + 汎用 `AcpAdapterV2.ts` 7.9k、`provider/acp/AntigravityAcpSupport.ts`, `AntigravityProtocol.ts`, `Drivers/AntigravityDriver.ts` | ACP（stdio JSON-RPC）。stdout の補正と session update の正規化が独自に入る。バイナリはリリースから zip をダウンロードして展開、認証は OAuth（`oauth-personal`） | serde_json で手書きした ACP 層（Phase 0 で `agent-client-protocol` クレートは不採用と決定。107 クレートと独自ランタイムを持ち込むため）＋ Antigravity 固有の補正層。v2 alpha の `session/fork|resume` 対応状況を要確認 | 中 |
| （後で検討）ACP 系の Grok / Devin / レジストリ | `AcpAdapterV2.ts` の薄いラッパー | ACP | Antigravity で作った ACP 層を再利用 | 低 |
| （後で検討）Pi | `PiAdapterV2.ts` 3k | `pi --mode rpc` の行区切り JSON | 自前実装 | 低 |
| Claude | `ClaudeAdapterV2.ts` 7.7k | JS の `@anthropic-ai/claude-agent-sdk` がプロセス内で `claude` CLI を stream-json で起動 | CLI の stream-json 入出力を直接話す実装。権限プロンプト、セッション再開、履歴読み込みを自前で | 中〜高 |
| （後で検討）OpenCode 1.x / 2.x | 3.8k / 4.2k | `opencode serve` に HTTP + SSE | reqwest + SSE。スキーマは手書き | 中 |
| （後で検討）Cursor | `CursorAdapterV2.ts` 2.7k | JS の `@cursor/sdk` がプロセス内で動く（CLI プロトコルなし） | Node サイドカーで SDK を包むか、当面非対応 | 高 |

t3code はプロバイダーのトランスクリプトを録って再生するリプレイテスト（`orchestration-v2/testkit/`）を持っている。**このフィクスチャを Rust 実装の適合テストとして再利用する**のが品質担保の近道。

### 1.5 通信（ワイヤ）

- WebSocket `/ws`: Effect RPC（`effect/unstable/rpc`）の JSON フレーミング。JSON-RPC 2.0 ではない。約200メソッド、`stream: true` のものは ACK ベースのストリーム
- ミューテーションは基本 `orchestration.dispatchCommand` 一本に集約
- 購読: `subscribeShell`（サイドバー用の軽量一覧）、`subscribeThread`（`snapshot` → `event`(sequence付き) → `synchronized`）
- HTTP: 境界付きスナップショット、履歴ページ、diff、アセット、認証（ペアリング、WS チケット、DPoP）
- 性能規約（`docs/internals/performance-regressions.md`）: 初回表示は直近75行・約1MiB、再接続のキャッチアップは最大128イベント／1MiB で超えたらスナップショット、コマンド出力やツール結果本体はワイヤに乗せず diff エンドポイントで遅延取得

### 1.6 クライアント（apps/web + client-runtime）

- React 19 + React Compiler、TanStack Router、Effect Atom + zustand、Tailwind v4、base-ui、LegendList（仮想化）
- Markdown: react-markdown + GFM + 独自ディレクティブ、ストリーミング用のインクリメンタルパーサー
- ハイライト: Shiki（Oniguruma WASM）、diff: `@pierre/diffs`（ワーカープール）
- ターミナル: **libghostty-vt を WASM 化**して Canvas2D で自前描画
- コンポーザー: TipTap 3 / ProseMirror（メンションチップ、スラッシュコマンド、IME 対応、画像・ファイル添付）
- 巨大ファイル: `ChatView.tsx` 1.1万行、`ChatComposer.tsx` 7.5k、`MessagesTimeline.tsx` 5.3k、`Sidebar.tsx` 5.3k、settings 4.2万行、PR レビュー 1.7万行
- タイムライン処理の流れ: サーバーイベント → プロジェクション reducer（`client-runtime/state/orchestrationV2Projection.ts`）→ `TimelineEntry`（`session-logic.ts`）→ 行（`MessagesTimeline.logic.ts`）。ストリーミングは差分ではなく TurnItem 全体の置き換えで、テキストだけの更新は行構造を作り直さない

---

## 2. Blongo の目標アーキテクチャ

### 2.1 プロセス構成

```
blongo (単一バイナリ)
├─ UI スレッド: GPUI (Metal / Vulkan(Blade) / DirectX)
│    └─ blongo-app: ビュー群 + クライアント状態 (ClientStore)
│          │  Backend トレイト
│          ├─ LocalBackend  ──(チャネル)──▶ blongo-core (同一プロセス, 専用 tokio ランタイム)
│          └─ RemoteBackend ──(WebSocket, Blongo独自)─▶ `blongo serve` (Phase 3)
└─ blongo-core
     ├─ event store (rusqlite, WAL) / orchestrator / projections / outbox worker
     ├─ providers: codex, claude, antigravity (acp)  ※初期対応はこの3つ
     ├─ pty (portable-pty), git (git CLI → 将来 gix), workspace search
     └─ mcp server (エージェント向け)            ──spawn──▶ 各エージェント CLI
`blongo serve` = 同じ blongo-core を UI なしで起動し WebSocket で公開（リモート用）
```

- ローカル利用では **WebSocket もシリアライズも通さない**。コアの購読は `async_channel` 経由で型付きイベントのまま UI に届く
- UI とコアの境界は「コマンド送信」と「購読（スナップショット＋シーケンス付きイベント）」だけにする。t3code のワイヤ契約と同じ形なので、リモートでも同じ Backend トレイトで扱える
- GPUI は独自のエグゼキューター（foreground/background）を持つ。コアは専用の小さな tokio ランタイム（ワーカー2〜4スレッド）で動かし、UI とはチャネルでつなぐ。これでヘッドレスの `blongo serve` もそのまま成立する

### 2.2 Cargo ワークスペース（案）

| クレート | 内容 | 主な依存 |
|---|---|---|
| `blongo-protocol` | ドメイン型（ID, Command, DomainEvent, TurnItem, Projection, Capabilities）。serde `#[serde(tag = "type")]` | serde, serde_json, uuid, time |
| `blongo-store` | SQLite イベントストア、プロジェクション、レシート、アウトボックス、マイグレーション | rusqlite (bundled), r2d2 か専用ライタースレッド |
| `blongo-core` | オーケストレーター（スレッド単位直列化、CommandPolicy、EventSink、EffectWorker、Run 実行、チェックポイント、フォーク） | tokio, blongo-store |
| `blongo-provider` | アダプタートレイトと共通部品（イベント結合、テキストデルタの合体、プロセス監督） | tokio::process |
| `blongo-provider-codex` / `-claude` / `-acp`（Antigravity） | 各アダプター。ACP 層は Antigravity 固有の補正と汎用部分を分けておき、将来の ACP エージェント追加に流用する | codex-app-server-protocol, agent-client-protocol |
| `blongo-pty` | PTY とヘッドレス端末状態 | portable-pty, alacritty_terminal |
| `blongo-git` | worktree、ステータス、隠し ref によるチェックポイント、diff | git CLI（後で gix） |
| `blongo-mcp` | エージェントに公開する MCP サーバー（thread/queue/project ツール） | rmcp |
| `blongo-client` | クライアント状態（接続監督、shell/thread プロジェクション reducer、履歴ページング、キャッシュ） | blongo-protocol |
| `blongo-ui` | GPUI コンポーネント（ボタン、リスト、ポップオーバー、テキスト入力、markdown、コードブロック、diff、端末ビュー） | gpui |
| `blongo-app` | バイナリ。ウィンドウ、ルーティング、キーバインド、設定 | 上記すべて |

### 2.3 GPUI 周りの技術選択

- **GPUI のバージョン**: crates.io の `gpui` 0.2.2 は 2025-10 で古い。Zed の git リビジョンを固定して使う（`gpui` + `gpui_platform`）。リビジョン更新は月1回程度、専用 PR で行う
- **コンポーネント**: `gpui-component` 0.7.0（longbridge, Apache-2.0）は入力欄、仮想リスト、Markdown、tree-sitter ハイライト、Dock などを持つが、`gpui-pre =0.3.7`（Zed の特定スナップショット）に固定されている。Phase 0 のスパイクで「gpui-component に合わせて GPUI を固定する」か「最新 GPUI に自前コンポーネント」かを決める。**推奨は初期は gpui-component を採用**し、性能上のボトルネックになった部品だけ自前に置き換える
- **Zed の GPL クレートは使わない**: `editor`, `terminal`, `terminal_view`, `markdown`, `ui`, `acp_thread`, `agent_servers` は GPL-3.0。設計を読んで学ぶのは可、コードの流用は不可
- **タイムライン**: GPUI の `list`（`ListState`、可変高さ、末尾追従）を使う。`uniform_list` は使えない（行高さが可変）
- **Markdown**: `pulldown-cmark`（MIT）で解析し、GPUI 要素ツリーを自前生成。ストリーミング中は「最後に閉じたトップレベルブロック以降だけ再解析」（t3code の `markdown-incremental.ts` と同じ方針）
- **シンタックスハイライト**: `tree-sitter` + 各言語文法（MIT）を background executor で。テーマは VS Code テーマ JSON から変換
- **ターミナル**: `alacritty_terminal`（Apache-2.0、Zed が GPUI 上で実績あり）で VT 状態を持ち、セル描画を GPUI で自作。libghostty-vt（t3code が採用）は C ABI なので将来の選択肢として残す
- **テキスト入力 / IME**: GPUI の `EntityInputHandler`（`examples/input.rs`）が IME の土台。メンションチップ付きのリッチなコンポーザーは自作が必要で、最初はプレーンテキスト＋添付から始める
- **アクセシビリティ**: GPUI は accesskit を取り込み始めているが Web 版ほどではない。既知のギャップとして扱う

### 2.4 低メモリ・高速化の設計原則

1. **真実は SQLite、メモリはワーキングセットだけ**。開いているスレッドと実行中スレッドのプロジェクションだけを保持し、閉じたスレッドは一定時間（t3code は5分）後に破棄
2. **ワイヤ／チャネルに本体を流さない**。コマンド出力やツール結果本体は ID 参照にして、表示時に遅延取得（t3code の性能規約をそのまま採用）
3. **ストリーミングはフレーム単位で合体**。デルタは UI へ流す前にフレーム（約8ms）単位でまとめ、`cx.notify()` は1フレーム1回
4. **テキストは `Arc<str>` / `SharedString` で共有**し、ストリーミング中のメッセージは追記用バッファ（必要ならロープ）で保持
5. **整形済みテキストのキャッシュ**。GPUI の `ShapedLine` / `WrappedLine` をメッセージ ID と幅でキャッシュし、スクロールでの再シェイピングを避ける
6. **重い処理は background executor**: Markdown 解析、ハイライト、diff 計算、ファイル検索
7. **アイドル時はゼロ**。ポーリングしない。タイマーは必要時だけ
8. **計測を CI に入れる**: 起動時間、アイドル RSS、1万行スレッドのスクロール fps、ストリーミング中の CPU を t3code と同じマシンで比較

メモリの目標値は §0.5 のとおり zeron を基準にし、Phase 0 で t3code と zeron の実測ベースラインを取ってから確定する。注意点として、エージェント CLI（`claude`, `codex` など）自体のメモリは Blongo からは削れないので、**計測は「Blongo 本体」と「エージェントプロセス」を分けて報告**する。

---

## 3. フェーズ計画

各フェーズは「動くもの」で終わる。期間は書かず、マイルストーンと完了条件で管理する。

### Phase 0: 土台とベースライン（2026-10-03 完了、結果は `docs/phase0/results.md`）

> 結果の要点: 同じ負荷で idle PSS は Blongo 143 MiB / zeron 222 MiB / t3code 620 MiB（Linux ソフトウェア Vulkan）。GPUI は上流 `badfb8d` に固定。ACP と Codex の公式クレートは採用せず serde_json で手書き。Antigravity の実バイナリ確認と、実機（macOS）での計測は Phase 1 に持ち越し。


- 実測ベースライン: **zeron**（主な比較対象）と t3code の両方について、起動時間、アイドル RSS（全プロセス合計）、長いスレッドのスクロール、ストリーミング中の CPU と RSS、ストリーミング保持量を測る。負荷は zeron の計測負荷に揃える
- メモリ計測を CI に入れる（zeron の mem-smoke 相当。閾値は §0.5 の目標値）
- Cargo ワークスペース作成、CI（fmt, clippy, test、macOS / Linux / Windows ビルド）
- GPUI リビジョンを固定し、ウィンドウ＋サイドバー＋仮想リストの最小アプリ
- **スパイク1**: GPUI の土台を決める。上流 gpui か zeron の zui フォークか、gpui-component を使うか自前か（入力、リスト、Markdown、画像の解放の4点で、メモリと描画コストを測って判断）
- **スパイク2**: Codex（`codex app-server`）を Rust から起動し、1ターン往復＋承認を通す。Phase 1 の最初のプロバイダーになる
- **スパイク3**: `claude` CLI を stream-json で直接起動し、1ターン往復＋権限要求を通す（zeron の `harness/src/claude` を参考に）
- **スパイク4**: Antigravity を `agent-client-protocol` クレートで起動し、t3code の補正（`AntigravityProtocol.ts`）を当てて1ターン往復を通す（zeron の `harness/src/acp` も参考に）

完了条件: ベースラインの数値表、空の GPUI アプリがビルドできる CI、4つのスパイクの結論。

### Phase 1: 縦に一本通す（コア＋UI、Codex のみ）

Node なし・Electron なしで、1プロバイダーのスレッドが最後まで動く最小構成を作る。

- `blongo-protocol`: ドメイン型（ID、Command、DomainEvent、TurnItem、Projection、Capabilities）。t3code の `orchestrationV2.ts` を参考に、必要なものだけを最初から Rust の型として設計する（互換は取らないので名前や形は簡素化してよい）
- `blongo-store`: イベントログ、プロジェクション、コマンドレシート、アウトボックス（t3code の v2 テーブルを簡素化して採用）
- `blongo-core`: スレッド単位の直列化、CommandPolicy、1トランザクションのコミット、EffectWorker、Run 実行と完了処理、承認、中断
- `blongo-provider-codex`: 最初のプロバイダー
- `blongo-client` と `LocalBackend`: コアの購読（スナップショット＋シーケンス付きイベント）を UI 用の状態に反映
- 画面: サイドバー（プロジェクト、スレッド一覧、状態表示）、タイムライン（メッセージ、思考、ツール実行のまとめ、承認）、コンポーザー（プレーンテキスト、送信・中断）
- t3code のリプレイフィクスチャ（Codex 分）を取り込み、適合テストにする

完了条件: Blongo 単体で Codex のスレッドを作成・実行・承認・中断でき、アプリを再起動しても SQLite から復元できる。Phase 0 のベースラインと並べた RSS・描画性能の数値がある。

### Phase 2: プロバイダーとワークスペース機能

- プロバイダー追加: **Claude → Antigravity** の順。各プロバイダーのリプレイ適合テストつき。Antigravity はバイナリのダウンロード・展開と OAuth ログインも含む
- キュー／steer、フォーク、プロバイダー切替（ContextHandoff）
- チェックポイント（git の隠し ref）とロールバック、worktree 作成
- PTY とターミナルビュー
- Markdown とコードブロックのハイライト、プラン表示、モデル選択
- t3code の DB からのインポート（`statev2.sqlite` を読み取り専用で取り込む一方向の移行ツール。継続的な互換ではない）

完了条件: Codex / Claude Code / Antigravity のスレッドが完走し、チェックポイントからロールバックできる。

### Phase 3: リモートとサーバーモード

- `blongo serve`（ヘッドレス）と `RemoteBackend`。プロトコルは Blongo 独自（候補: WebSocket 上の長さ前置きバイナリ。型は `blongo-protocol` を共有し、スキーマのバージョンと機能フラグで交渉）
- 認証（ペアリング、bearer、DPoP）、Tailscale、SSH 環境
- 複数環境を同時に扱う接続レジストリと再接続（3s, 4s, 8s, 16s のバックオフ、30秒健全でリセット。t3code の方針を踏襲）

### Phase 4: 機能の厚み

優先度順:
1. diff / レビューパネル（ターン diff、全体 diff、コメント）
2. コマンドパレット、キーバインド（when 句）、設定画面
3. エージェント向け MCP サーバー（`t3_thread_*`、`delegate_task`、サブエージェント）
4. ファイルブラウザ・ファジー検索、ブランチ／git 操作
5. スケジュール実行、使用量表示
6. 追加エージェントの検討（Grok などの ACP 系、OpenCode、Pi、Cursor）
7. PR 受信箱・レビュー（GitHub / GitLab ほか）
8. 自動更新、ディープリンク、通知

### 当面の対象外

ブラウザプレビュー（GPUI に webview がない。必要なら wry 埋め込みか CDP で外部ブラウザ）、デバイスストリーム、モバイルアプリ、T3 Connect のリレー本体、スクリーンショット機能、マーケティングサイト。

---

## 4. リスクと対策

| リスク | 影響 | 対策 |
|---|---|---|
| zeron より軽くならない | 存在意義がなくなる | 機能を足すたびに CI のメモリ計測で zeron 基準の閾値を守る。超えたら機能より先に直す |
| 上流 gpui の画像アトラス等のリーク | 長時間利用で RSS が増え続ける | zeron の zui の修正を確認し、必要な分だけ当てる。8時間の連続利用テストで横ばいを確認 |
| GPUI が pre-1.0 で破壊的変更が多い | 追従コスト | リビジョン固定、更新は専用 PR、GPUI 依存を `blongo-ui` に閉じ込める |
| gpui-component が古い GPUI スナップショットに固定 | 最新 GPUI の改善を取り込めない | Phase 0 で判断。採用しても部品単位で抜けるよう薄いラッパー越しに使う |
| コア先行のため最初に動くものが出るまでが長い | 体感できる成果が遅れる | Phase 1 を Codex 1本・最小画面に絞り、縦に一本通すことを最優先にする |
| Claude SDK 相当の機能（セッション履歴、スキル、使用量制限）の再実装 | Claude 対応が遅れる | Phase 0 でスパイク。足りなければ一時的に Node サイドカー |
| Antigravity の ACP 実装に独自の癖がある（stdout 補正、update 正規化） | 汎用 ACP クレートだけでは動かない | t3code の `AntigravityProtocol.ts` を読み、補正層を独立モジュールにする。Phase 0 で起動〜1ターンを確認 |
| オーケストレーターの不変条件の取りこぼし | 二重実行、ターンが閉じない等 | リプレイ適合テスト、`docs/orchestration-v2` の不変条件をテスト化 |
| Zed の GPL コードの混入 | ライセンス違反 | 依存チェック（cargo-deny で GPL を禁止）を CI に入れる |
| リッチなコンポーザー、Markdown 選択コピー、アクセシビリティ | Web 版より体験が劣る | 段階的に。最初はプレーンテキストと基本選択に絞る |

---

## 5. 決めてほしいこと（推奨つき）

1. ~~t3code とのワイヤ互換~~ → **互換なしに決定（2026-10-03）**
2. ~~対応エージェント~~ → **Codex、Claude Code、Antigravity の3つに決定（2026-10-03）**。Cursor 等は後で検討
3. **gpui-component の採用**: Phase 0 のスパイク結果で決める（初期採用を推奨）
4. **対応 OS の優先順位**: macOS → Linux → Windows を推奨（GPUI の成熟度順）

---

## 6. 最初の一歩（次のスレッドで着手できる単位）

1. Cargo ワークスペースと CI、GPUI 固定リビジョンで空ウィンドウ（Phase 0）
2. zeron と t3code のベースライン計測スクリプトと結果表（Phase 0）
3. Rust から `codex app-server` を起動して1ターン往復するプロトタイプ（スパイク2）
4. `claude` CLI stream-json 直結のプロトタイプ（スパイク3）
5. Antigravity を ACP で1ターン往復するプロトタイプ（スパイク4）

## 付録: 参照すべき t3code のファイル

- 設計: `docs/orchestration-v2/README.md`, `core-graph-and-data-model.md`, `provider-capability-system.md`, `docs/internals/overview.md`, `connection-runtime.md`, `performance-regressions.md`, `terminal-runtime.md`
- 契約: `packages/contracts/src/orchestrationV2.ts`, `rpc.ts`, `auth.ts`, `environmentHttp.ts`
- コア: `apps/server/src/orchestration-v2/Orchestrator.ts`, `EventSink`, `ProjectionStore.ts`, `EffectOutbox.ts`, `EffectWorker.ts`, `ProviderAdapter.ts`, `runtimeLayer.ts`
- アダプター: `apps/server/src/orchestration-v2/Adapters/*`
- クライアント状態: `packages/client-runtime/src/state/orchestrationV2Projection.ts`, `connection/supervisor.ts`, `rpc/session.ts`
- タイムライン: `apps/web/src/session-logic.ts`, `apps/web/src/components/chat/MessagesTimeline.logic.ts`, `apps/web/src/markdown-incremental.ts`
