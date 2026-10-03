# Phase 1 結果: 縦に一本通す（コア＋UI、Codex のみ）

- 日付: 2026-10-03
- 環境: Phase 0 と同じ（Linux 4コア、Xvfb、Mesa lavapipe、`LP_NUM_THREADS=4`、ウィンドウ 1280×800）
- 結論: **Blongo 単体（Node・Electron なし）で、プロジェクト追加 → スレッド作成 → Codex ターンの実行（ストリーミング、思考、ツール、承認、中断）→ 再起動後の SQLite からの復元まで通った。** t3code が実 Codex（0.156.1）で録った5シナリオのリプレイでもハーネスとコアの両方で同じ結果になる。メモリは Phase 0 から idle PSS +8 MiB、stream PSS +2 MiB で、目標（idle ≤155、stream ≤175 MiB PSS、idle CPU <1%）に収まった。
- 改訂: 独立レビュー（CHANGES REQUIRED）を受けて、リプレイ適合テスト、`thread/resume` の `excludeTurns`、IME の選択位置の修正などを入れた（§4）

## 1. 作ったもの

```
apps/blongo (GPUI UI スレッド)               crates/blongo-core (専用スレッド, current_thread tokio)
  Shell: サイドバー / 見出し / コンポーザー       Orchestrator: コマンド検証 → 1トランザクションでコミット
  Timeline: ブロック単位の仮想化リスト   <────    → イベント配信 → アウトボックスの副作用を実行
  TextInput: 複数行・IME・選択・クリップボード     Codex セッション（スレッドごとに1つ、アイドルで解放）
        │  CoreClient.dispatch(Command)           │  AgentEvent → ドメインイベント
        └──── 型付きの値をチャネルで（シリアライズなし、本文は Arc<str>）
                                                 crates/blongo-store (rusqlite, WAL)
                                                 crates/blongo-harness (codex app-server)
```

| クレート | 内容 |
|---|---|
| `blongo-protocol` | ID（UUID v7 の newtype: Project/Thread/Run/Item/Command）、`Command`（`project.create`, `thread.create/rename/archive`, `message.dispatch`, `run.interrupt`, `runtime_request.respond`）、連番付き `DomainEvent`、`TurnItem`（user_message, assistant_message{streaming}, reasoning, command_execution, file_change, tool_call, approval_request, system_notice, error）、`RunStatus`、スナップショット型 |
| `blongo-store` | SQLite（WAL, `synchronous=NORMAL`, 外部キー有効, page cache 1 MiB）。`events`（追記ログ）、`projects/threads/runs/turn_items`（プロジェクション）、`command_receipts`、`effect_outbox` を **1コミット1トランザクション** で書く。マイグレーション（前進のみ、新しすぎる DB は拒否） |
| `blongo-core` | オーケストレーター（下記） |
| `blongo-harness` | Phase 0 の Codex ハーネスに `thread/resume`（`excludeTurns: true` で履歴は受け取らない）を追加。再起動後も Codex 側の会話を継続し、失敗時は `thread/start` に戻ってその旨を通知アイテムで表示。コマンドの終了コードも渡す。Linux ではエージェントに `PR_SET_PDEATHSIG` を付け、Blongo が `kill -9` されても Codex が残らない |
| `apps/blongo` | t3code 風の画面（下記） |

t3code の不変条件は次の形で守っている。
- **アプリ ID が主**: ID はすべてクライアント生成の UUID v7。Codex のスレッド ID・承認リクエスト ID・call_id は参照としてだけ保存する
- **冪等なコマンド**: 同じ `command_id` の再送はレシートを見て何もしない（`CommandDuplicate`）。テストあり
- **ターンを閉じるのはルート Run の完了だけ**: `Run.parent_run_id` を持ち、スレッドの状態を動かすのは親のない Run だけ（ストアのプロジェクションで強制、テストあり）。Phase 1 はサブエージェントを作らないので子 Run は実際には出てこない

### 永続化の方針

- **本文は1か所だけ**: ユーザー・アシスタント・思考の本文は `turn_items.body` にだけ入る。イベントログの `item.added` / `item.text_appended` には本文を入れない（`#[serde(skip)]`、テキスト追記は長さだけ記録）。ログは監査と将来の差分同期用で、復元はプロジェクションから読む
- **デルタごとに書かない**: ストリーミング中のテキストは UI にはすぐ `TextDelta` で届け、SQLite へは最大 200 ms ごと、およびアイテム終了・ターン終了・スレッドを開いたときにまとめて書く。52KB の返答（445 デルタ）で `text_appended` はデルタ数の 1/4 未満（テストで確認）。クラッシュ時は最後のフラッシュまでが残る
- **再起動時の回復**: 前回プロセスの `starting/running/waiting` の Run は `interrupted`（error: `process restarted`）にし、ストリーミング中のアイテムを閉じ、保留中の承認を `cancelled`、実行中のツールを `failed` にし、「Interrupted: Blongo exited before this turn finished.」の通知アイテムを足す。アウトボックスに残った副作用はプロセス依存なので再実行せず `dropped` にする。正常終了時は実行中のターンを中断扱いで閉じてから終わる

### コア

- 専用スレッド1本の current_thread tokio。状態を変える処理はすべて1つのタスクで動くので、スレッド単位の直列化は自動的に満たされる
- 購読: 起動時に `Shell` スナップショット、`open_thread` で `Thread` スナップショット。どちらもイベントと同じチャネルで送るので、スナップショットより後のイベントだけが後に届く
- 中断: Codex に `turn/interrupt`（保留中の承認には `cancel`）を送り、10 秒で終わらなければプロセスを止めて中断扱いで閉じる
- セッション: スレッドごとに Codex プロセス1つ。ターン終了から 10 分（`BLONGO_SESSION_IDLE_SECS`）でプロセスを解放し、次のメッセージで `thread/resume` して再開する
- タイマーは必要なときだけ（テキストのフラッシュ、中断の期限、アイドル解放）。アイドル時のコアは何もしない
- セッション → コアのチャネルは有界（256）。コアが詰まったら Codex の stdout 読み取りが止まる。実行済みのアウトボックス行はその場で削除する
- DB を開けない・回復に失敗したときは `CoreEvent::Failed` を送り、ウィンドウにエラーを表示する（空白のままにしない）

### 計画からの変更（アーキテクチャ）

- `blongo-client` クレートと `Backend` / `LocalBackend` トレイトは作っていない。UI は `blongo-core` の `CoreClient`（コマンド送信）とイベントチャネル（スナップショット＋イベント）を直接使う。リモート（Phase 3）を入れるときに、この2つをトレイトに切り出す
- `ProviderCapabilities` もまだない（プロバイダーが Codex だけなので分岐がない）。Claude を足す Phase 2 で入れる
- `blongo-provider-codex` は独立クレートにせず、Codex のプロトコル層は `blongo-harness`、Run への対応付けは `blongo-core` にある
- UI はイベントのシーケンス番号を検査しない。プロセス内チャネルは欠落・順序入れ替えがなく、スナップショットより後のイベントだけが後に届くため。リモートでは必要になる

### UI

- サイドバー: プロジェクトとそのスレッド。状態の点（実行中=青、承認待ち=黄、失敗=赤）。「+ Project」でパス入力欄（Enter で追加、Esc で閉じる）、「+ New」でスレッド作成、ホバーで出る「×」でアーカイブ
- タイムライン: 1行 = 1アイテム、ただしアシスタントの Markdown は 1行 = 1ブロック（Phase 0 の方式）。ユーザーの吹き出し、Markdown（見出し、段落、箇条書き、`**太字**`、`` `コード` ``、フェンス）、折りたためる思考、1行のツール行（クリックで出力を展開）、ファイル変更の要約、承認カード（Approve / Deny）、通知、エラー。実行中は末尾に「Working…」（承認待ちなら「Waiting for approval」）を静的に表示。更新は1フレーム（16 ms）に1回だけ list を splice する
- コンポーザー: 自作の複数行テキスト入力（`EntityInputHandler` で IME、`shape_text` で折り返し、最大 10 行で以降はカーソル位置へスクロール、選択・上下移動・ダブルクリックで単語選択・コピー/カット/ペースト）。Enter で送信、Shift+Enter で改行。実行中は Send が Stop に変わる。コアが送信を拒否したら入力した文が欄に戻る
- 見た目は不透明でフラット。アニメーションもカーソル点滅もなく、アイドル時は再描画しない
- 開いているスレッドのタイムラインだけをメモリに持つ（切り替えると前のものは破棄し、スナップショットを取り直す）

### 設定

- `BLONGO_DATA_DIR`（既定: プラットフォームのデータディレクトリ/blongo）に `blongo.sqlite`
- `BLONGO_CODEX_EXE`（既定: PATH の `codex`。npm のシムなら同梱のネイティブバイナリを直接起動）
- 計測用: `BLONGO_PROFILE_PROMPT` / `BLONGO_PROFILE_START_MS` / `BLONGO_PROFILE_PROJECT`（`tools/profile.py` が使う）。Phase 0 の `BLONGO_REPLAY` と `BLONGO_AGENT=claude` は廃止し、計測も本物の経路（ハーネス → コア → SQLite → タイムライン）を通すようにした

## 2. 検証

### テスト（`cargo test --workspace`: 61 本、すべてオフライン）

| 対象 | 本数 | 主な内容 |
|---|---:|---|
| `blongo-protocol` | 4 | ID の順序とパース、コマンド・イベントの serde 往復、本文がペイロードに出ないこと |
| `blongo-store` | 10 | 1トランザクション（途中で失敗したらイベント・レシート・アウトボックス・プロジェクションのどれも残らない）、冪等な再送、外部キー、ストリーミング本文の追記、子 Run がスレッドを動かさないこと、WAL とファイル DB の再オープン、マイグレーション（差分適用・再実行・新しすぎる DB の拒否・失敗時のロールバック）、実行済みの副作用の削除 |
| `blongo-core` | 2 + 9 | フェイク Codex との結合: 承認 → 許可、拒否、実行中の二重送信の拒否、中断、再送の無視、正常終了 → 再起動で同一のスナップショット、`thread/resume`／クラッシュ（`abort`）→ 回復／不正コマンドの拒否／Codex が起動できないときの失敗／ストリーミングのまとめ書き／DB を開けないときの `Failed`。**実 Codex の録音で**: 承認ターン（アイテムの並び、終了コード 0、承認は Approved）、開始直後とコマンド実行中の中断（Run は interrupted、ツールは failed）、Blongo を再起動して `thread/resume`（同じ Codex スレッド、`excludeTurns`、前の答えを覚えている返答） |
| `blongo-harness` | 19 + 6 + 5 | Phase 0 のフェイク3種（Codex のフェイクは `excludeTurns` のない resume を拒否する）、`PR_SET_PDEATHSIG` で生成スレッドが終わるとエージェントも死ぬこと、**実 Codex の録音5本**（下記） |
| `apps/blongo` | 6 | Markdown のブロック分割とインライン、タイムラインの行管理、UTF-16 ⇔ UTF-8 の変換と IME の選択位置（ASCII の後ろに日本語を変換中、サロゲートペア） |

#### t3code の録音によるリプレイ適合テスト

t3code が実 `codex app-server` 0.156.1 と話した記録（`apps/server/src/orchestration-v2/testkit/fixtures/*/codex_transcript.ndjson`、MIT）から5本を `crates/blongo-harness/tests/fixtures/t3code/` に取り込んだ（モデル名などは伏せ字に置換、`THIRD_PARTY_NOTICES.md` に帰属表示）。`replay_codex.py` が録音のサーバー側を演じ、クライアントが送るべきフレーム（`expect_outbound`）が来るまで待ってから続きを流す。Blongo が送った実際のフレームはログに残し、テスト側で録音と比べる。

| シナリオ | 確認すること |
|---|---|
| `simple` | 返答テキスト `fixture simple ok`、ターン完了、`turn/start` のスレッド ID と入力文が録音と一致 |
| `tool_call_read_only_on_request` | 承認要求（コマンドと理由）→ Allow の応答が録音（`{"decision":"accept"}`、id 0）と一致、思考の要約、コマンドの開始・終了（終了コード 0）、コメントと最終回答 |
| `turn_interrupt` | `turn/interrupt` のスレッド ID・ターン ID が録音と一致、ターンは Interrupted |
| `turn_interrupt_mid_tool` | コマンド実行中の中断。コマンドは完了しないままターンが Interrupted。t3code だけが送る `thread/backgroundTerminals/terminate` は「送らない」と記録 |
| `provider_thread_resume` | 2プロセス目で `thread/resume` に同じスレッド ID と `excludeTurns: true`、録音どおり前のターンを覚えた返答 |

録音にないのに Blongo が送るフレームは `account/read`（サインイン確認）だけで、テストはそれ以外が出ないことも確かめる。ハーネス単体で5本、コア経由（SQLite のアイテムと Run の状態まで）で4本（simple は resume の1プロセス目で兼ねる）を通す。

### GUI のエンドツーエンド（Xvfb + lavapipe + xdotool、フェイク Codex）

`DISPLAY=:93 tools/e2e-gui.sh target/release/blongo docs/phase1/screenshots` で再現できる。

| 確認 | スクリーンショット（`docs/phase1/screenshots/`） |
|---|---|
| 空の状態 → パス入力でプロジェクト追加（スレッドも自動作成） | `01-empty.png`, `02-project-and-thread.png` |
| 複数行入力（Shift+Enter） | `03-composer-multiline.png` |
| ストリーミング中（思考、Markdown、Working 行、Stop ボタン、サイドバーの青い点）→ 完了（ファイル変更行、コードブロック） | `04-streaming.png`, `05-streamed.png` |
| 承認カード → Approve → コマンド成功、出力の展開 | `06-approval-pending.png`, `07-approved.png`, `08-tool-output.png` |
| 実行中 → Stop → 「Turn interrupted.」 | `09-running.png`, `10-interrupted.png` |
| Ctrl+Q で終了 → 再起動で全スレッドとタイムラインが復元 | `11-restored.png`, `12-restored-markdown.png` |
| ターン中に `kill -9` → 再起動で中断扱いに回復（部分テキストが残る） | `13-crash-recovered.png` |

`kill -9` の直後に、そのプロセスが起動したフェイク Codex が残っていないことも確かめる（改訂で追加）。

日本語の直接入力は xdotool がこの環境のロケールで送れず未確認（IME の経路は `EntityInputHandler` の実装どおり。実機で要確認）。

### メモリと CPU（`tools/profile.py`、Phase 0 と同じ負荷・同じフェーズ）

52KB の返答をフェイク Codex が 40 ms 間隔で流し、ハーネス → コア（SQLite へのまとめ書きを含む）→ タイムラインの実経路で表示する。release ビルド、2回計測（`docs/phase1/profiles/`、レビュー修正後に取り直し）。

| | Phase 0 | **Phase 1** | 目標 |
|---|---:|---:|---:|
| idle ピーク PSS | 143 MiB | **151 MiB**（150.8 / 151.3） | ≤155 |
| stream ピーク PSS | 164 MiB | **166 MiB**（165.6 / 166.5） | ≤175 |
| settled ピーク PSS | 162 MiB | 158〜163 MiB | — |
| idle ピーク RSS | 166 MiB | 174 MiB | — |
| stream ピーク RSS | 187 MiB | 189 MiB | — |
| idle CPU | 0.3% | **0.31〜0.42%** | <1% |
| settled CPU | 0.3% | 0.41〜0.61% | — |
| stream 中 CPU | 101% | 112〜115% | — |
| フェイク Codex（python、別プロセス） | — | 10.5 MiB RSS | — |

idle の +8 MiB の内訳（`/proc/PID/smaps` で Phase 0 のバイナリと並べて確認）:
- **約 8 MiB はスワップチェーン画像**。lavapipe のウィンドウサイズの RGBA 画像（各 4000 KiB）が3枚あり、Phase 0 の idle は起動後に1フレームしか描かないので1枚しか触っていなかった。Phase 1 は起動時にスナップショット受信などで数フレーム描くため3枚とも常駐する。マウスを一度動かせば Phase 0 でも同じになる（ホバー後に揃えた比較では Phase 0 145 / Phase 1 150 MiB PSS、thin LTO 時点）。stream では両方とも3枚触っているので、stream の差（+2 MiB）が実質の増分に近い
- 残りはコード（SQLite、コア）。リリースビルドを thin LTO から fat LTO・codegen-units=1 に変えて、バイナリ 31→25 MB、idle PSS を 155〜158 → 151 MiB に下げた（release ビルドは約 7 分に伸びる）
- テキストのグレースケール描画（サブピクセル無効）も試したが差が出なかったので入れていない
- 改訂中に分かった計測上の注意2つ。(1) `VK_ICD_FILENAMES` を指定しないと Vulkan ローダーが入っている全 ICD（nouveau, radeon, intel など）を読み込み PSS が約 +13〜14 MiB になる。これまでの数値はすべて lavapipe 指定なので、`tools/profile.py` が既定で指定するようにした。(2) SQLite をコアのスレッドで開くと、そのスレッド用の mimalloc ヒープが別に触られて idle PSS が約 +2.5 MiB 増えた。DB は呼び出し側のスレッドで開き、コアのスレッドへ渡す形に戻した

CI の mem-smoke の上限は 120 → 200 MiB RSS に直した。ソフトウェア Vulkan では libLLVM（約 60 MiB）と gallium（約 18 MiB）が乗るので 120 は Phase 0 の時点で超えていた。200 は回帰検知用の天井で、製品の目標値（idle 100 MiB 以下）は実機 GPU で判定する。

## 3. 既知のギャップ・リスク

- **実 Codex での生のターンは未確認**（この環境はモデル接続がプロキシで塞がれていて認証もない）。代わりに t3code が実 Codex 0.156.1 で録ったセッションのリプレイで、送受信するフレームの形を確かめた。0.156.1 より新しい CLI での差は未確認
- Antigravity の実バイナリでの確認は Phase 0 からの持ち越しのまま（Phase 2 で Antigravity を足すときに行う）
- キュー／steer なし: 実行中のスレッドへの送信は拒否する（UI は Stop だけ出す）。Phase 2 の範囲
- スナップショットは開いたスレッドの全アイテムを一度に読む。長いスレッドのページング（t3code の履歴ページング相当）は Phase 2 で入れる
- コンポーザー: 10 行を超える入力はカーソル追従のスクロールのみでスクロールバーなし。単語単位の移動、Undo、添付なし。日本語 IME は変換中の選択位置の計算をテストで確かめたが、実機の IME では未確認
- タイムライン: テキストの選択・コピー不可。Markdown はブロック分割と太字・インラインコード・箇条書きだけ（表、リンク、ハイライトなし）。ツール出力はプレビュー（4 KiB）のみ保存
- サイドバーは Shell の再描画のたびに組み立て直している（キャッシュしたビューに分けていない）。ストリーミング中の CPU（112〜115%、Phase 0 は 101%）の一部はこれと思われる。Phase 2 でサイドバーを独立したエンティティにして `cached` で描く
- プロジェクト追加はパス入力のみ（ネイティブのフォルダ選択ダイアログは未使用）。プロジェクトの削除・スレッド名変更の UI なし（コマンドはある）
- 承認の「このセッションでは常に許可」（Codex の `acceptForSession`）は UI に出していない
- 親が死んだらエージェントも死ぬ仕組み（`PR_SET_PDEATHSIG`）は Linux だけ。macOS / Windows では Blongo が強制終了されると Codex が残りうる（stdin が閉じるので通常は自分で終わる）。また対象は Codex 本体だけで、Codex が起動したツールのプロセスは Codex 自身の後始末に任せる
- 計画との差: `blongo-client` / `Backend` トレイト / `ProviderCapabilities` は未作成、UI はシーケンス番号を検査しない（§1「計画からの変更」）
- 計測は Linux のソフトウェア描画のみ。macOS 実機の数値は未計測（Phase 0 からの持ち越し）
- 正常終了時、コアの停止（Codex の終了待ち）が終わるまで最大数秒 UI スレッドが待つ（5 秒で打ち切り、残ったプロセスグループは SIGKILL）
- CI はまだ一度も GitHub 上で走っていない（push していない）

## 4. レビュー後の修正（2026-10-03）

独立レビューの指摘（CHANGES REQUIRED）に対して入れたもの。

- **必須**: t3code のリプレイ録音5本による適合テスト（§2）／`thread/resume` に `excludeTurns: true`（なしだと Codex が全履歴を1行で返し、行サイズ上限や待ち時間で黙って `thread/start` に落ちうる）／IME が返す選択位置を「変換中の文字列の中の UTF-16 位置」として変換し、境界に丸める（これまでは全文に対して変換していたため、"ab" の後に「にほ」を変換中だと文字の途中を指し、Home/End で落ちた）。ほかのバッファ切り出しも全部、文字境界に丸めてから行う
- Linux でエージェントに `PR_SET_PDEATHSIG`、停止の待ちが打ち切られたらプロセスグループごと SIGKILL、e2e で `kill -9` 後の残存プロセスを検査
- コアが送信を拒否したらコンポーザーに文を戻す
- コマンドの終了コードを `ToolResult` → `CommandExecution.exit_code` まで渡す（UI に `exit 0` などが出る）
- 中断の要求中に Codex が終了したら Failed ではなく Interrupted にする
- セッションのないスレッドの実行時状態（`ThreadRt`）を Run 終了時に捨てる（これまで残り続けた）
- DB を開けない・回復に失敗したら、ウィンドウにエラーを表示する
- タイムライン: 終わったユーザー発言・思考の本文はコアの `Arc<str>` を共有し、二重に持たない
- セッション → コアのチャネルを有界にし、実行済みのアウトボックス行はその場で削除する
- `THIRD_PARTY_NOTICES.md` に t3code の録音（MIT）と GPUI（Apache-2.0、`input.rs` の元になった `examples/input.rs`）を追記し、Apache-2.0 の本文を `licenses/` に置いた
