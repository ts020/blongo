# Phase 4 結果: 機能の厚み

- 日付: 2026-10-03
- 環境: Phase 0〜3 と同じ（Linux 4コア、Xvfb、Mesa lavapipe、`LP_NUM_THREADS=4`、ウィンドウ 1280×800）
- 結論: **計画にあった 8 項目はすべて、少なくともコアのロジックとテストまで入った。** 1〜5、7、8 は UI までつながっていて、GUI e2e で通しで確かめた（§2.3）。6（追加エージェント）は汎用 ACP プロバイダーを実装し、評価は文書にした
  - diff／レビューパネル: スレッド全体とターンごとの差分をチェックポイントから出す。行は仮想化し、ファイルの本文は 1 ファイルずつ遅延で読む。行コメントは 1 通のメッセージにまとめてエージェントへ送る
  - コマンドパレット、when 句つきのキーバインド（`keybindings.json` で上書き）、`settings.json` に保存する設定画面
  - エージェント向け MCP サーバー: `t3_thread_*`、`delegate_task`、`task_status`、`schedule_task` など 10 ツール。Codex、Claude、ACP の各セッションに渡し、呼び出し元のプロジェクトの外は見えない。エージェントが増やせるスレッドは深さ・同時数とも上限つきで、定期実行はユーザーの承認まで走らない（§5）
  - ファイルブラウザ、`.gitignore` を守るファジー検索、git 操作（状態、ブランチの切替・作成、コミット）
  - スケジュール実行（5 項目の cron、ローカル時刻）と、ターンごとのトークン数・費用の表示
  - GitHub／GitLab の PR 受信箱とレビュー投稿。偽のサーバーでだけ確かめた
  - 更新の確認（ed25519 署名のマニフェスト、SHA-256 を確かめるダウンロード）、`blongo://` ディープリンク、デスクトップ通知
- メモリ（レビュー対応後の最終バイナリ）: idle PSS 155.8 / 157.7 MiB（上限 165）、stream PSS 176.1 / 177.5 MiB（上限 185）、idle CPU 0.42 / 0.31%（§2.4）。大きな diff（200 ファイル × 500 行）を開いた瞬間は 192.5 MiB まで上がり、172.6 MiB に落ち着く。diff の画面を閉じると 168〜172 MiB
- `blongo-serve`: idle PSS 14.9 / stream 17.9 MiB（Phase 3: 13.5 / 16.5）
- 新しいサードパーティのクレートは入れていない。HTTP はシステムの `curl` を使う。ライセンス文の同梱（Phase 3 の持ち越し）は `THIRD_PARTY_LICENSES.txt` で済ませた
- 実エージェント、本物の GitHub／GitLab、本物の更新サーバーには一度もつないでいない。macOS・Windows 固有の部分（URL スキームの登録、通知、MCP の名前付きパイプ）は手順の文書だけ（§3）
- 独立したセキュリティレビューで「要修正」になった 4 件（一時ディレクトリの権限、エージェントによるスレッド増殖と定期実行、リンクからのプロジェクト追加、夏時間の cron）と、指摘された細かい点を直した（§5）

## 1. 作ったもの

```
apps/blongo (GPUI)
  Shell ── View: Chat | Diff | Files | Inbox | Settings（ヘッダーのタブ、Alt+C/D/E/I、Ctrl+,）
   ├─ DiffView（diff.rs）      スレッド／ターンの差分、または PR のパッチ。uniform_list で仮想化
   ├─ FilesView（files.rs）    遅延で開くツリー、ファイルビューア、git バー
   ├─ InboxView（inbox.rs）    PR 受信箱 → DiffView（Patches）→ レビュー投稿
   ├─ SettingsView            タブ式（General / Providers & models / Scheduled runs / Review inbox /
   │                            Updates / Keybindings / Data & environments）
   ├─ Palette（palette.rs）    コマンド一覧とファイル検索（Ctrl+Shift+P / Ctrl+P）
   ├─ keymap.rs               コマンド ID、when 句、keybindings.json
   ├─ query.rs                ワークスペース問い合わせの登録簿（Global）。返事は CoreEvent::Reply
   ├─ notify.rs / deeplink.rs 通知（外部コマンド）、blongo:// と app.sock での受け渡し
   └─ settings.rs             settings.json（0600）
crates/blongo-core
  orchestrator: ジョブをループの外へ（キーごとに直列）、問い合わせ、スケジュール、使用量、承認ポリシー
  mcp.rs + orchestrator/tools.rs: エージェント向け MCP サーバー（Unix ソケット + mcp-bridge）
  cron.rs: 5 項目の cron（ローカル時刻）
crates/blongo-git/src/workspace.rs   スナップショットのツリー、上限つきの diff、ファイル一覧、git status など
crates/blongo-protocol/src/workspace.rs  Query / QueryReply（どの返事にも上限）、unified diff の解析、ファジー採点
crates/blongo-client
  http.rs: システムの curl で HTTPS（設定は stdin、本文は 0600 の一時ファイル）
  forge.rs: GitHub / GitLab の受信箱・PR の差分・レビュー投稿（forge.json、0600）
  update.rs: 署名つきマニフェストの確認とダウンロード
```

### 土台: ループの外のジョブとワークスペース問い合わせ

- Phase 3 の持ち越しだった「チェックポイント／復元／インポートがオーケストレーターのループの中で動く」を直した。チェックポイント、ロールバックの復元、worktree の作成、アーカイブの片付け、ブランチ切替、コミット、t3code のインポートは、ループの外のジョブとして動く
- ジョブはキー（1 スレッド、またはコア全体）を持つ。キーが使用中のときは到着順に待つので、1 スレッドの中の順序は守られ、ほかのスレッドは止まらない（`core_phase4.rs` のテスト 3）
- 問い合わせ（差分の要約、ファイルごとの差分、ファイル検索、フォルダ一覧、ファイル読み、git の状態・ブランチ・切替・コミット）は、フォルダやチェックポイントのツリーをループで解決してからタスクで実行し、`CoreEvent::Reply` で答える
- リモートではプロトコル v2 の `Query` / `Reply` として流れ、ハブは尋ねた接続にだけ返す（`remote.rs` のテスト 17）
- 返事にはどれも上限がある
  - 差分: 2000 ファイル、ファイルあたり 20,000 行、1 行 2000 バイト
  - git の出力: 8 MiB を超えたらプロセスを止める
  - ファイル一覧: 200,000 件
  - 読み込み: 1 MiB
  - status: 500 件

### 1. diff／レビューパネル

- **出どころ**: チェックポイントのツリー同士の差分。「All changes」はスレッドの最初のチェックポイントから今の作業ツリーまで、「Turn N」はそのターンの前後。ターンのカードにある「Changes」からも開ける
- **大きな差分**:
  - 行は `uniform_list`（22 px 固定）で仮想化
  - 最初に開くのは先頭 12 ファイル。ファイルの本文は 1 つずつ順に取りに行く（キュー、同時に 1 本）
  - 1 ファイルは最初 1500 行まで。残りは「More」行で読み足す
  - 範囲を切り替えたり読み直したりすると世代が進み、古い返事は捨てる
- **コメント**: 行をクリックすると入力欄が開く。「Send to agent」で、ファイル名・行番号・その行の引用・コメントをまとめた 1 通のメッセージ（`Review comments on your changes:`）を送る。実行中なら待ち行列に入る
- PR のレビューでも同じ DiffView を Patches モードで使う（§7）

### 2. コマンドパレット、キーバインド、設定画面

- 詳しくは `docs/phase4/keybindings.md`
- **コマンド**: すべての操作に ID と when 句がある（例: `view.diff` は `threadOpen`）
  - when 句は `!`、`&&`、`||`、括弧が使える
  - キーを押すとシェルの状態で評価し、偽なら `cx.propagate()` で次のバインドに回す
- **上書き**: 設定フォルダの `keybindings.json` で足す／消す（`-command`）／when を変える
  - 間違ったキーや未知のコマンドは、設定画面の Keybindings タブに一覧で出す（GPUI はキーの文字列が不正だとパニックするので、先に `Keystroke::parse` で確かめる）
  - 「Reload」で再読み込みできる
- **パレット**:
  - コマンドモードは名前でファジー検索し、キーを横に出す
  - ファイルモード（Ctrl+P）はバックエンドへの問い合わせ。打鍵中に問い合わせが重ならないよう、飛んでいる問い合わせは 1 本だけにして、次のパターンを 1 つ覚えておく
- **設定**: `settings.json`（0600）に保存する
  - 項目: 承認ポリシー（ask / auto-approve）、テーマ（ダーク／ライト）、既定のプロバイダーとモデル、汎用 ACP のコマンド、通知（Off / ウィンドウが裏のとき / 常に）、起動時の更新確認
  - 設定画面には、スケジュール、受信箱のトークン、キーバインド、データの場所と環境の一覧も置いた
  - 承認ポリシーと既定モデルは、`CoreClient::configure` で実行中のコアに反映する

### 3. エージェント向け MCP サーバー

- 詳しくは `docs/phase4/mcp.md`
- コアが `<data_dir>/mcp/s`（0700 のフォルダ、ソケット 0600）で待ち受ける
- エージェントには `blongo mcp-bridge SOCKET TOKEN_FILE`（stdio とソケットを中継するだけで、GPUI もコアも起動しない）を MCP サーバーとして渡す
  - Codex: `thread/start` の `config.mcp_servers`
  - Claude: `--mcp-config`
  - ACP: `session/new` の `mcpServers`
  - `blongo-serve` の中のエージェントには `blongo-serve mcp-bridge` を渡す
- トークンはセッションごとにランダムに作り、そのスレッドに結びつける。セッションが終わると失効して、ファイルも消える
- ツール: `t3_thread_list` / `read` / `create` / `send` / `wait` / `interrupt`、`delegate_task`（子スレッド、深さ 2、同時 4 つまで、`wait` / `async`）、`task_status`、`list_scheduled_tasks`、`schedule_task`（提案だけ。ユーザーの承認まで無効）。`t3_thread_create` のスレッドも呼び出し元の子になり、同じ上限に入る（§5）
- どのツールも呼び出し元のプロジェクトのスレッドしか扱わない。他のプロジェクトの ID には「unknown thread」と答え、存在も漏らさない
- 待つツールは最大 30 分で返る
- JSON-RPC は手書きの最小実装（`initialize`、`ping`、`tools/list`、`tools/call`）

### 4. ファイルブラウザ、ファジー検索、git 操作

- **ツリー**: フォルダを開いたときに中身を問い合わせる（遅延）。`.gitignore` を守り、作業フォルダの外へ出るシンボリックリンクは読まない
- **ビューア**: 512 KiB まで表示する。バイナリは `(binary file)` とだけ出す
- **ファジー検索**: 名前の先頭や単語の頭、連続した一致を優先する（`blongo-protocol::workspace` の採点）。t3code（24k ファイル）でも、一覧を作ってから最大 200 件を返す（§2.4）
- **git バー**:
  - ブランチのメニュー（切替・新規作成。重なり順のため `deferred` + `occlude` のオーバーレイ）
  - コミットメッセージの入力と「Commit all」、変更ファイルの一覧、Refresh
  - コミットはスレッドのキー、切替はコア全体のキーを持つジョブとして動く
  - スレッドが実行中なら断る。切替は、同じフォルダで動いているスレッドがあるときも断る

### 5. スケジュール実行と使用量

- **スケジュール**: 5 項目の cron（ローカル時刻）を SQLite に保存する（マイグレーション 3）
  - コアのタイマーが、決めたスレッドへ送るか、毎回新しいスレッドを作る
  - Blongo が止まっている間に過ぎた時刻は、起動時に 1 回だけ走らせる
  - 設定画面の Scheduled runs タブで作成・一時停止・今すぐ実行・削除ができる。MCP の `schedule_task` で作られたものは「proposed by an agent」と出て、「Approve」を押すまで走らない
  - 時刻は壁時計の分を暦の上で進めて探すので、23 時間・25 時間の日（夏時間の切り替え）でも飛ばしたり二重に走ったりしない（§5）
- **使用量**: ターンごとのトークン数（入力・キャッシュ・出力）と費用を、run に保存する
  - Codex は `tokenUsage`、Claude は result フレーム（費用つき）、ACP は `usage_update` から取る
  - スレッドのヘッダーに合計（例: `321 tokens`、費用があれば `· $0.12`）を出す

### 6. 追加エージェントの評価と汎用 ACP

- 評価は `docs/phase4/agents.md`。Grok と ACP ネイティブのものは汎用 ACP で動く見込み。OpenCode は汎用 ACP で動くがターン終了の判定が弱い。Pi は専用ドライバが要る。Cursor は汎用 ACP の範囲で案内する
- **汎用 ACP プロバイダー**（`ProviderKind::Acp`）:
  - 設定画面でコマンドを指定する（例: `opencode acp`）。環境変数 `BLONGO_ACP_EXE` / `BLONGO_ACP_ARGS` があればそちらが優先
  - 中身は Antigravity と同じ ACP ドライバで、MCP サーバーも渡す
  - 認証はエージェント自身のコマンドに任せ、Blongo から `authenticate` は呼ばない

### 7. PR 受信箱

- `forge.json`（0600）に、GitHub／GitLab のトークンと API の URL を、設定画面の Review inbox タブから保存する。環境変数のトークンは読まない
- **受信箱**: 自分にレビュー依頼が来ている PR／MR を取る
  - GitHub: search API の `review-requested:@me`
  - GitLab: `reviewer_id`（自分の ID）
- **PR を選ぶと**、ファイルごとのパッチを DiffView に出す
- **レビュー投稿**: 新しい側の行へのコメントは 1 つのレビューとして送る
  - GitHub: `POST /pulls/N/reviews`（`event: COMMENT`、`side: RIGHT`）
  - GitLab: diff の位置つき discussion
  - 消えた側の行へのコメントは、引用つきでサマリーに入れる
- HTTP は `curl` 経由（§1 の http.rs）。https だけを許し、テスト用にループバックの http だけ通す。トークンは設定ファイル（stdin）で渡すので、コマンドライン（`ps`）には出ない
- テストはどれもループバックの偽サーバー（`forge_update.rs`、e2e の `tools/fixtures/fake_forge.py`）

### 8. 更新の確認、ディープリンク、通知

- **更新**（`docs/phase4/updates.md`）:
  - ed25519 で署名したマニフェストを、埋め込みの公開鍵で確かめる。URL とハッシュも署名の中から取る
  - 新しければダウンロードし、SHA-256 が合わなければ消す
  - インストールはしない
  - リリース用の鍵はまだ無いので、既定では無効（`BLONGO_UPDATE_URL` / `BLONGO_UPDATE_KEY` で有効にする）
- **ディープリンク**（`docs/phase4/deep-links.md`）:
  - リンク: `blongo://thread/<id>`、`project?path=…`、`settings`、`inbox`。どれも画面の移動だけで、無いフォルダをプロジェクトに足すときだけ確認を出す（§5）
  - 2 つ目のプロセスは `<data_dir>/app.sock`（0600）で、動いている Blongo へリンクを渡してすぐ終わる（e2e で 26 ms）
  - Linux では `blongo register-url-handler` で `.desktop` と `xdg-mime` を登録する。GPUI の `on_open_urls` も同じ経路に流す
- **通知**（`docs/phase4/notifications.md`）:
  - ターンの終了と承認待ちで出す。既定は「ウィンドウが裏にあるときだけ」
  - `notify-send`、なければ `gdbus`（macOS は `osascript`）を起動する。常駐の D-Bus ライブラリは入れていない
  - `BLONGO_NOTIFY_CMD` で差し替えられる

### Phase 3 からの持ち越し

- `clean_stale_tunnel_dirs` を起動時に別スレッドで呼ぶ（自分の UID が持つ古い `blongo-ssh-*` だけ）
- ライセンス文: `tools/gen_third_party_licenses.py` が Cargo.lock から、blongo と blongo-serve にリンクされる 630 クレートのライセンスファイルを集めて `THIRD_PARTY_LICENSES.txt` に書く
  - Linux、macOS、Windows のターゲットが対象。オフラインで動く
  - ファイルを同梱していないクレートには、SPDX 表記と標準の MIT／Zlib の本文を添える
- サーバー側の細かい修正（5c94255）:
  - `bind_unix` でプロセス全体の umask を変えない
  - ハンドシェイクと参加の間に取り消されたデバイスを断る
  - ターミナル入力を捨てたらクライアントに知らせる
  - PTY の読み取りは 100 ms のタイムアウトではなく、起床用パイプつきの poll で待つ
  - Outbox の読み手は条件変数で待つ

## 2. 検証

### 2.1 テストと静的検査

`cargo test --workspace` は **263 本**と ignored 1 本（夏時間テストが子プロセスで走らせる本体）。Phase 3: 206 本、レビュー前: 252 本。最初のレビュー対応のあと 3 回、再レビュー対応のあと 2 回続けて全部通した。すべてオフラインで動き、実エージェントのターンは走らせていない。環境変数のトークンで何かを認証することもしていない。

| バイナリ | 本数 | Phase 4 で増えた主なもの |
|---|---:|---|
| blongo（アプリ） | 28 | キーマップ（既定値が正しい、when 句の解析と評価、ユーザーファイルの追加・削除・誤りの報告、表示）、ファジー採点、コメントのメッセージ、設定の往復、通知コマンド、ディープリンク（解析、**2 つ目のプロセスからの受け渡し**、desktop entry） |
| blongo-client 単体 / forge_update（新規） | 22 / 3 | トークンファイル、バージョン比較、**署名の検証**（鍵違い・改ざん）。偽の HTTP サーバーで GitHub（受信箱、ファイル、レビュー）、GitLab（受信箱、変更、discussion）、更新確認とダウンロード（**ハッシュ違いと鍵違いを拒否**） |
| blongo-core 単体 | 15（+ ignored 1） | cron（解析、ローカル時刻の次回、UTC 変換）、MCP（プロトコルの往復と未知のトークン、待ちの上限）、検索の順位と上限 |
| **core_phase4（新規）** | 9 | 下記 |
| core_codex / core_phase2 / t3_import | 10 / 27 / 3 | |
| blongo-git | 16 | スナップショットの差分（リネーム、上限）、`.gitignore` を守る一覧、porcelain の解析 |
| harness 単体 / acp・claude・codex のリプレイ / fake_agents / antigravity_install | 27 / 8・10・9 / 11 / 3 | Claude の `--mcp-config` |
| blongo-protocol | 19 | hunk と行番号、行の上限、ファジー採点、Query / Reply の往復 |
| blongo-server 単体 / remote | 11 / 17 | **`queries_over_the_wire_and_agents_use_the_real_mcp_bridge`**: リモートの問い合わせと、サーバー内のエージェントが本物の `blongo-serve mcp-bridge` で MCP を使う |
| store | 15 | 使用量・親スレッド・スケジュールの往復、2 接続の交互書き込み |

`crates/blongo-core/tests/core_phase4.rs`（本物のコアとフェイク Codex、本物の git リポジトリ）:

1. `turn_and_thread_diffs_from_checkpoints`: 2 ターンの後、ターンごとの差分とスレッド全体の差分が合う
2. `files_search_and_git_operations`: 一覧、検索、読み込み、status、ブランチの作成・切替、コミット
3. `work_for_one_thread_keeps_its_order_while_checkpoints_run_off_the_loop`
4. `usage_is_recorded_per_turn`
5. `auto_approve_answers_without_waiting`
6. `schedules_fire_persist_and_catch_up_after_a_restart`
7. `mcp_tools_see_only_their_project_and_delegate_to_children`: 他のプロジェクトのスレッドは見えない（list に出ない、read は unknown thread）。`delegate_task` が子スレッドを作って結果を返す。async モードと `task_status`（親以外は読めない）
8. `agents_cannot_escape_the_spawn_limits`（レビュー対応）: 上限を破ろうとする攻撃のテスト。§5 を参照
9. `agents_cannot_wake_more_threads_than_they_could_start`（再レビュー対応）: `t3_thread_send` で動いていないスレッドを起こす攻撃のテスト。§5 を参照

静的検査:

- `cargo clippy --workspace --all-targets -- -D warnings` と `cargo fmt --all --check` は通る
- `cargo deny check licenses` も通る（licenses ok）。cargo-deny はスクラッチにインストールし、終わってから消した
- 新しいサードパーティのクレートは無い。Cargo.lock の差分は、既存のクレート（dirs、serde、serde_json、libc、getrandom）を別のワークスペースのクレートからも使うようにした分だけ

### 2.2 完了条件

- **diff／レビューパネル**: ターンごと・スレッド全体、仮想化、ファイル単位の遅延読み込みと行の上限、コメントをエージェントへ（テスト 1、GUI e2e の場面 3、大きな diff のプロファイル）
- **パレット、キーバインド、設定**: when 句、ユーザーファイルでの上書き、設定の保存（単体テスト、GUI e2e の場面 2・4・5）
- **MCP サーバー**: `t3_thread_*` と `delegate_task`、Codex／Claude／ACP への配線、プロジェクトへの制限、文書（テスト 7、remote のテスト 17、`docs/phase4/mcp.md`）
- **ファイルブラウザ、検索、git**: 切替・作成・コミット・status（テスト 2、GUI e2e の場面 7・8、t3code でのプロファイル）
- **スケジュール、使用量**: テスト 4・6、GUI e2e の場面 1b・6
- **追加エージェント**: 評価の文書と汎用 ACP（`docs/phase4/agents.md`）
- **PR 受信箱**: 偽のサーバーでだけ（forge_update、GUI e2e の場面 9）
- **更新・ディープリンク・通知**: 更新は確認とダウンロードまで（forge_update）。ディープリンクと通知は GUI e2e の場面 1・3・10

### 2.3 GUI のエンドツーエンド（`tools/e2e-gui-phase4.sh`、Xvfb + lavapipe + xdotool）

スクリプトは自前の Xvfb、フェイク Codex、偽の GitHub（`tools/fixtures/fake_forge.py`）を使い、通知は `BLONGO_NOTIFY_CMD` でファイルに記録する。`keybindings.json` で Alt+D を外し、Alt+G（when `threadOpen && !view.settings`）と Alt+K（パレット）を足した状態で動かす。スクリーンショットは `docs/phase4/screenshots/`。最後まで通った（`done`）。

| # | 場面 | 確認したこと |
|---|---|---|
| 1 | README.md を書き換えるターン | 完了の通知が記録される（p4-01） |
| 1b | 使用量 | `usage` のターンで runs.usage が入り、ヘッダーに `321 tokens`（p4-01b） |
| 2 | キーの上書き | Alt+D は効かず、Alt+G で Changes が開く（p4-02） |
| 3 | 行コメント | 「Turn 1」でターンの差分（p4-03b）、All changes に戻って追加行にコメント（p4-03）、Send to agent で `Review comments on your changes: README.md:1 …` がスレッドに入る（DB で検査）。次のターンの承認待ちの通知（p4-04） |
| 4 | パレット | Alt+K で開き、`sett` → Enter で設定が開く（p4-05, p4-06） |
| 5 | 設定の保存 | Light にすると `settings.json` に `"theme": "light"`、Dark に戻す（p4-07） |
| 6 | スケジュール | `* * * * *` でこのスレッドへ。次の分に走り、`last_run_at` とメッセージが入る（p4-08, p4-09） |
| 7 | ファイル検索とブラウザ | Ctrl+P で `main` → Enter。`.gitignore` の対象は出ない（p4-10, p4-11） |
| 8 | ブランチ | `feature-x` を作って切り替わる（git で検査、p4-12） |
| 9 | PR 受信箱 | acme/widgets#7 の差分、行コメントとサマリー、Submit review。偽サーバーのログで、パス、Bearer トークン、commit_id、`side: RIGHT`、本文を検査（p4-13〜16） |
| 10 | ディープリンク | 未知のフォルダの `blongo://project` で確認が出て、プロジェクトは増えない。Cancel でも増えない（p4-17a）。既知のプロジェクトのリンクを 2 回送っても、スレッドは増えない（p4-17b）。`blongo://settings` → 25 ms で終わり、動いている側で設定が開く（p4-17c） |
| 11 | 終了 | エージェントのプロセスが残らず、`app.sock` も消える |

Phase 2・3 の `tools/e2e-gui.sh`（15 場面）と `tools/e2e-gui-remote.sh`（8 場面）も、最終のデバッグビルドで再実行して最後まで通った。

### 2.4 メモリと CPU（`tools/profile.py`、Phase 0〜3 と同じ負荷）

release ビルド（fat LTO）で、レビュー対応後の最終バイナリを 2 回測った（レビュー前は 157.8 / 157.5、177.2 / 176.0）。結果は `docs/phase4/profiles/`。データフォルダは短いパス（`/home/claude/p4`）に置いた。長いパスだと MCP のソケットが作れず、MCP サーバーが無効のまま測ることになるため（§3）。

| | Phase 3（記録） | **Phase 4** | 上限 |
|---|---:|---:|---:|
| idle ピーク PSS | 156.1 / 156.1 | **155.8 / 157.7** | 165 |
| stream ピーク PSS | 173.2 / 176.2 | **176.1 / 177.5** | 185 |
| settled ピーク PSS | 170.6 / 173.6 | 169.6 / 175.7 | |
| idle CPU | 0.31 / 0.31% | **0.42 / 0.31%** | 1% |
| stream 中 CPU | 126〜127% | 122〜125% | |
| スレッド数 | 32 | 33（`blongo-links`） | |

- idle の +1.5 MiB の内訳:
  - 常駐するコードが増えた分（バイナリは 37.2 MB）
  - ディープリンクの待ち受けスレッド（スタック）
  - MCP の待ち受け
- ネットワーク用のランタイムは、受信箱か更新確認を使うまで起動しない

**機能ごとの計測**（`--view` / `--prompt` で同じ道具を使う）:

| 場面 | ピーク PSS | 落ち着いた値 | 備考 |
|---|---:|---:|---|
| diff パネル、小さな差分（1 ファイル） | 165.4 | 165.4 | `blongo-phase4-diff-small.json` |
| diff パネル、大きな差分（200 ファイル × 500 行 = 10 万行） | 192.5 | 172.6 | 開いた直後に先頭 12 ファイルを読む分。約 10 秒で戻る（tokio の blocking プールのスレッドが消える時間と一致）。`diff-big` |
| ファイルブラウザ、t3code（24k ファイル）+ 検索 `index` | 185.2 | 179.5 | 一覧と検索の結果を持つ分。`files-t3code` |
| 大きな差分を開いて 10 秒後にチャットへ戻る（レビュー対応） | 180.5 / 184.6 | 168.1 / 171.6 | 閉じると DiffView ごと捨て、glibc に `malloc_trim` で返す。開いたままの 172.6 より 1〜4.5 MiB 少ない。idle（約 156）との差の残りは、tokio の blocking スレッドなど diff 以外のもの。`diff-big-closed-run1/2` |

**`blongo-serve`**（`tools/profile_serve.py`、`blongo-serve.json`）: idle PSS 14.9 MiB、stream 17.9 MiB（Phase 3: 13.5 / 16.5）。増えたのは MCP サーバーとワークスペース問い合わせのコードの分。

計測のあと、t3code の作業ツリーは手を付けていないことを確かめた（status は clean、`refs/blongo` も無い）。

## 3. 既知のギャップ・リスク

- **実物での確認がない**: GitHub／GitLab の API と更新サーバーは偽物でだけ確かめた。本物の API とはページ送りや権限エラーの形が違うかもしれない
  - GitLab の discussion の位置指定（`position`）は、版によって必要な項目が違う
  - 実エージェントのターンも、どのプロバイダーでも走らせていない
- **MCP サーバー**:
  - Unix ソケットなので、Windows では無効
  - データフォルダのパスが長いと、ソケットのパスが上限（約 108 バイト）を超えて無効になる。理由はログに出すが UI には出ない。`$XDG_RUNTIME_DIR` へ逃がす処理が要る
  - トークンのフォルダは一覧できないようにしたが、同じユーザーのプロセスは権限を戻せるので、意図的に探せば他のセッションのトークンを読める。分けるには別ユーザーかサンドボックスが要る（§5）
- **更新はインストールしない**: ダウンロードと検証まで。リリース用の鍵と配布の仕組み（macOS の署名・公証、Windows のインストーラ）が無いので、既定では無効
- **macOS／Windows**:
  - URL スキームの登録は手順の文書だけ。Windows では 2 つ目のプロセスからの受け渡しも未実装（2 つ目のウィンドウになる）
  - Windows の通知も未実装
- **汎用 ACP は 1 枠だけ**: コマンドの変更は次回の起動から効く。Grok 系は `session/prompt` が返らないことがあり（zeron の検証）、そのときは Stop で止めるしかない。終了検知の補強は未実装（`agents.md`）
- **大きな diff のピーク**: 10 万行の差分を開いた直後は PSS 192.5 MiB（落ち着くと 172.6）。先頭 12 ファイルを自動で開く分なので、数を減らすか、画面に入ったファイルだけ読むようにすれば下がる
- **プロジェクトのフォルダが親リポジトリで無視されている場合**: たとえば、別のリポジトリの `target/` の中に置いたフォルダ。チェックポイントの `git add` が失敗し、そのターンにはチェックポイントが無い（通知は出し、ターンは続く）
- **PR レビュー**: 消えた側の行へのコメントは、行コメントではなくサマリーに入る。新しい側だけの単純化
- **設定画面**:
  - Xvfb ではスクロールの操作を再現できなかったので、タブ式にした
  - ライトテーマは主要な色だけ。コードハイライトの配色はダーク用のまま
- **ファイルビューア**: ハイライトなしで、512 KiB まで
- **e2e の座標**: e2e は座標でクリックするので、レイアウトを変えたら更新が要る

## 4. 計画からの変更

- **HTTP**: クレート（reqwest／hyper＋rustls）ではなく、システムの `curl` を起動する
  - TLS スタックを入れずに済み、バイナリと常駐メモリを増やさない
  - 証明書ストアとプロキシの設定は OS のものを使う
  - トークンは stdin の設定で渡す
- **通知**: D-Bus のクレート（zbus）ではなく、`notify-send` ／ `gdbus` を起動する。常駐スレッドを増やさないため
- **ジョブをループの外へ**: Phase 3 の報告で Phase 4 に回すと書いた分。問い合わせの土台として最初に入れた
- **設定画面の形**: スクロールする 1 ページではなく、タブ式にした
- **自動更新**: 確認とダウンロードまでにした。インストールは配布の仕組みと一緒に作る
- **追加エージェント**: 専用ドライバは作らず、汎用 ACP と評価の文書にした（OpenCode の HTTP+SSE、Pi の JSONL は Phase 5 以降の候補）

## 5. レビュー対応（review fixes）

独立したセキュリティ重視のレビューで「要修正」になった。指摘ごとの対応は次のとおり。

### 要修正（4 件）

1. **一時ディレクトリの権限を変えていた**（`http.rs` → `secret::write_private` → `private_dir`）
   - 原因: PR レビューの本文を `temp_dir()` に書くとき、親（`/tmp`）を 0700 に chmod していた。root で走るテストが `/tmp` を壊し（コーディネーターが 1777 に戻した）、一般ユーザーでは EPERM で POST が全部失敗していた
   - 修正:
     - 本文は一時ファイルに書かず、curl の設定（stdin）の `data-raw` で渡す。テキスト（JSON）だけを許し、NUL などの制御文字と 8 MiB 超は拒否する
     - `private_dir` は、無いフォルダを 0700 で作る。既存のフォルダは、自分の持ち物で共有（sticky）でないときだけ 0700 に締める。共有・他人のフォルダは chmod せずにエラーにする
     - `write_private` は既存の親フォルダに触らない
     - ダウンロード先のファイルは `create_new` + 0600 で作る
     - curl は `-q` を最初に付け、`~/.curlrc` を読まない
   - テスト:
     - `existing_and_shared_directories_keep_their_mode`: 0755 と 1777 の自前のフォルダが書き込み後も同じモード。sticky には `private_dir` がエラー
     - `config_quotes_values_and_never_takes_headers_with_newlines`: 本文が設定の中で正しく引用される
     - forge_update: 偽サーバーで本物の curl の POST 本文が届く
     - `blongo-client` の単体テストと forge_update を `setpriv --reuid=65534`（nobody）で走らせて全部通り、`/tmp` は 1777 のまま
2. **エージェントが委任の上限をすり抜けられた**（`orchestrator/tools.rs`）
   - 修正:
     - `t3_thread_create` のスレッドも呼び出し元の子にし、`delegate_task` と同じ `check_spawn`（深さ 2、子 4 つ、プロジェクトあたり 8 つ）を通す
     - 同時数には、実行中やジョブ中の子に加えて、まだ列にいる作成（`ThreadCreate`）と子へのメッセージを数える
     - `schedule_task` は提案だけにした。`proposed_by` つきの無効な定期実行を作り（マイグレーション 4）、ユーザーが設定画面で「Approve」（オン）を押して初めて走る
     - 提案は 15 分以上の間隔だけ（`Cron::min_gap` で次の 500 回を確かめる）、承認待ちは 1 プロジェクト 10 件まで
     - 定期実行のターンの承認ポリシーは全体の設定のままで、作ったエージェントより緩くならない
   - テスト `agents_cannot_escape_the_spawn_limits`（本物のコア、フェイク Codex、本物のブリッジ）:
     1. `t3_thread_create` の入れ子で深さを破ろうとすると、孫が「limited to 2 levels」で断られ、スレッドは 2 つしか増えない。孫からの `delegate_task` も断られる
     2. post-checkout フックで 4 秒かかるブランチ切替（グローバルなジョブ）が列を止めている間に、`delegate_task` を 6 本同時に呼ぶ。4 本だけ通り、2 本は断られる。通った分の返事は、切替のあとにしか来ない。キューを数えない版ではこのテストが落ちることを、一時的に壊して確かめた
     3. `* * * * *` の提案は拒否される。`*/30` の提案は無効・次回なし・`proposed_by` つきで作られる。ユーザーがオンにすると有効になり、`proposed_by` が消える
     4. スレッドをアーカイブすると、トークンファイルが消える。古いトークンでソケットにつないでも、何も返らない
3. **`blongo://project` リンクが確認なしにプロジェクトとスレッドを作っていた**（`shell.rs`）
   - 修正: 既知のプロジェクトなら最新のスレッドを選ぶだけ。未知のフォルダなら確認（パスと注意書き、Cancel / Add project）を出し、追加はユーザーが押したときだけ。リンクでスレッドは作らない
   - 他のリンクも見直した: `thread` / `settings` は移動だけ。`inbox` は開くと設定済みフォージへの読み取りが走る（`deep-links.md` に明記）
   - GUI e2e の場面 10 で確かめた（DB の件数で検査）
4. **cron が夏時間で飛ばす・二重に走る**（`cron.rs`）
   - 原因: 1 日を 24 時間として進めていた
   - 修正:
     - 壁時計の分を暦の上で進めて合う時刻を探し、`mktime` で時刻に直す（`tm_isdst` を -1 / 0 / 1 で試し、`after` より後で最も早いもの）
     - 存在しない時刻（02:30）は `mktime` の正規化どおり 03:30 に走る
     - 繰り返す 1 時間は、最初の 1 回だけ走る
     - 再起動などで時計が戻ったときは、前回の実行の壁時計以下の分を飛ばす（`next_run`）
   - テスト: `daylight_saving_changes_neither_skip_nor_repeat` が自分自身を `TZ=America/New_York` の子プロセスで走らせる（他のテストの時刻帯を変えないため）。確かめたこと:
     - 2026-03-09 の月曜 0 時が飛ばない
     - 02:30 は 03:30 に走る
     - 毎時は 01:00 の次が 03:00
     - 2026-11-01 の 01:30 は 1 回だけ
     - 毎分は 01:59 EDT の次が 02:00 EST
     - 繰り返す 1 時間の中で再起動しても二重に走らない

### 再レビューで残った 1 件（要修正）

- **`t3_thread_send` で上限をすり抜けられた**
  - 原因: 上限で数えるのは「動いている」スレッドだけだった。そのため、動いていない子を `t3_thread_create {}` で何個も作り、あとから `t3_thread_send` で 1 つずつ起こせば、何個でも同時に動かせた。終わった `delegate_task` の子も同じように起こせた
  - 修正（`orchestrator/tools.rs` の `check_wake`）:
    - 動いていないスレッドへの送信は「エージェントがもう 1 つ動き出す」とみなし、作るときと同じ上限で確かめる
    - エージェントが作ったスレッドは、その親の同時 4 つとプロジェクトの 8 つに数える
    - ユーザーのスレッドは、送り手の 4 つとプロジェクトの 8 つに数える。その run は `agent_runs` に残し、待っている間も動いている間も数える
    - もう動いているスレッドへの送信は、その作業のあとに並ぶだけなので通す
    - 1 つのスレッドが作れる子は、動いていなくても（アーカイブしていないもの）16 まで（`check_spawn`）
  - テスト `agents_cannot_wake_more_threads_than_they_could_start`:
    1. 子を 16 個作ると、17 個目は断られる
    2. 動いていない子 10 個に順に `sleep` を送ると、4 つだけ動き、6 つは断られる。動いている子へのメッセージは並ぶだけ
    3. 別の親が 4 つ起こしてプロジェクトが 8 つになると、ユーザーのスレッドも起こせない（何も届かない）
    4. 終わった `delegate_task` の子 6 つを順に起こすと、4 つだけ動く
    - `check_wake` を一時的に外すと、このテストが落ちることを確かめた
- 細かい指摘 3 件も直した:
  - 確認が出ている間に届いた別のフォルダの `blongo://project` リンクは無視して、通知だけ出す。「Add project」を押す直前にパスがすり替わらない
  - curl の設定に入れる本文の上限（8 MiB）は、エスケープ後の長さで数える。curl は設定の 1 行を 10 MiB までしか読まない。テスト: 5 MiB の `"` は拒否、5 MiB の `a` は通る
  - `mcp.md` に、トークンは**ゆるい境界**だと明記した。同じユーザーのプロセスは `/proc/*/cmdline` からトークンファイルのパスを読めるため

### 細かい指摘

| 指摘 | 対応 |
|---|---|
| 更新: リリース鍵があっても `BLONGO_UPDATE_KEY` で差し替えられる | `RELEASE_KEY` があればそれだけを使う（`trusted_key` のテスト）。マニフェストに `platform` と `expires` を足し、署名の中で確かめる（他の OS 用と期限切れを拒否） |
| curl が `~/.curlrc` を読む | `-q` を最初に付けた |
| 16 MiB を超えるリモートの返事が黙って捨てられ、クライアントが待ち続ける | エンコード後 8 MiB（フレームの半分）を超える返事は「too large」のエラーに置き換える（`answers_too_large_for_a_frame_become_errors`）。Outbox は返事の実サイズで数えるので、大きな返事は単独のフレームで送られる |
| ジョブだけのスレッドへの `t3_thread_wait` が返らない | ジョブが終わって何も残っていなければ、待っている呼び出しに答える（`on_job_done`） |
| MCP の接続ごとの上限 | 同時のツール呼び出しは 8 つまで（`one_connection_runs_a_bounded_number_of_calls`）。返事の列は 32 件の有限、ブリッジは全体で 64 本まで |
| MCP のトークンを他のセッションが読める | トークンファイル名を乱数にし、フォルダを 0300（一覧不可）にした。同じユーザーなら権限を戻せるので、完全には防げない（§3） |
| スケジュールのタイマーが毎分起きる | 次の予定まで眠る。サスペンドや時計の変更に備えて、最長 15 分で見直す |
| ACP の使用量の数えすぎ（セッションの合計を run ごとに保存して足していた） | ターン開始時の合計との差を、そのターンの分として記録する（`usage_totals_become_per_turn_shares`） |
| `read_file` が FIFO で止まる | 普通のファイル以外は拒否し、`O_NONBLOCK` で開く（FIFO とフォルダのテスト） |
| when 句の入れ子の深さ | 512 文字まで、`(` と `!` の入れ子は 32 段まで |
| desktop entry の Exec のエスケープ | `"` `` ` `` `$` `\` と `%` を規則どおりに書き、制御文字を含むパスは登録しない（テストつき） |
| 環境変数名の食い違い（`BLONGO_ACP_EXECUTABLE` と `BLONGO_ACP_EXE`） | 文書どおり `BLONGO_ACP_EXE` に統一した（ハーネスの定数をコアも使う） |
| diff の画面を閉じても読み込んだパッチが残る | 書きかけのコメントがなければ、閉じたときに DiffView ごと捨て、glibc では `malloc_trim` を呼ぶ。閉じたあとは 168〜172 MiB（§2.4） |
| cron の日の欄の `*/N` | Vixie cron と同じく、`*` で始まる日の欄は「制限なし」として扱う（曜日と OR にならない） |

### 残したもの（§3 にも記載）

- MCP のトークンは、同じユーザーのプロセスからは守り切れない（`/proc/*/cmdline` でパスが見える。ゆるい境界）。継承した fd で渡す方法は、エージェント（Codex、Claude など）が MCP サーバーのプロセスを自分で起動するため使えなかった
- diff を閉じたあとの常駐量は、開く前（約 156 MiB）までは戻らない（残りは diff 以外のスレッドやヒープ）

