# Phase 2 結果: プロバイダーとワークスペース機能

- 日付: 2026-10-03
- 環境: Phase 0・1 と同じ（Linux 4コア、Xvfb、Mesa lavapipe、`LP_NUM_THREADS=4`、ウィンドウ 1280×800）
- 結論: **Codex・Claude Code・Antigravity の3プロバイダーのスレッドが、フェイクエージェントと t3code の録音リプレイで最後まで動く。チェックポイントからロールバックすると作業ツリーが戻る。** あわせて次の機能も入れた: キュー／steer、フォーク、プロバイダー切替（文脈の引き継ぎ）、worktree、PTY ターミナル、コードハイライト、プラン表示、モデル選択、t3code からのインポート。メモリは idle PSS 155 MiB、stream PSS 170〜173 MiB、idle CPU 0.2〜0.3% で、目標（≤160 / ≤180 MiB、<1%）に収まった（§2.4）
- 実バイナリでは確認していない（モデル接続と認証がない環境で、`dl.google.com` も塞がれている）。とくに **Antigravity の実バイナリ（ダウンロード・展開・OAuth・ACP の会話）は未確認**。ローカルの HTTP サーバーとフェイク ACP エージェントでだけ試した（§3）

## 1. 作ったもの

```
apps/blongo (GPUI)                                 crates/blongo-core (専用スレッド)
  Sidebar（独立エンティティ・cached ビュー）           Orchestrator: prepare（非同期: worktree 作成・チェックポイント復元）
  Timeline: Markdown（pulldown-cmark）               → decide（検証）→ 1トランザクションでコミット → 後処理
            + コードハイライト（tree-sitter、遅延）     キュー（スレッドごと）、steer、フォーク、切替、ロールバック
            + プラン、ターン操作（Fork / Undo）         t3_import（statev2.sqlite のコピーから読む）
  Composer: プロバイダー／モデル選択、Queue / Steer   crates/blongo-harness: codex / claude / acp、Antigravity インストーラ
  TerminalView: portable-pty + alacritty_terminal     crates/blongo-git（新規）: チェックポイント（隠し ref）、worktree
```

### プロバイダーの抽象化

- `blongo-protocol` に `ProviderKind`（`codex` / `claude-code` / `antigravity`）と `ProviderCapabilities` を入れた。コアは能力を見て分岐し、プロバイダー名では分岐しない（t3code の不変条件）

| 能力 | Codex | Claude Code | Antigravity |
|---|---|---|---|
| steer | ネイティブ（`turn/steer`） | ネイティブ（`"priority":"now"` のユーザーメッセージ） | 取り消して送り直す（同じ Run の中） |
| 再開 | `thread/resume` | `--resume` | `session/load`（再生される履歴は捨てる） |
| ネイティブのフォーク | `thread/fork`（lastTurnId） | `--resume X --fork-session --resume-session-at=U` | なし → 文脈の引き継ぎ |
| ネイティブのロールバック | `thread/revert`（実行中のセッションにそのまま） | 次の起動で `--resume-session-at` | なし → 文脈の引き継ぎ |
| モデル | ターンごと（`model/list` で一覧） | プロセス固定（`initialize` の応答で一覧） | プロセス固定（`session/new` の一覧、`session/set_model`） |
| プラン | `turn/plan/updated` | TodoWrite | `plan` 更新 |
| アプリからのサインイン | なし | なし | あり（`authenticate`、URL をブラウザで開く） |

- ハーネスの入口は `blongo_harness::start(provider, StartOptions)` の1つにした。コマンドに `Command::Steer` と `Command::Rewind` を足した
- Claude Code: stream-json を直接話す既存のハーネスに次を足した
  - steer
  - 再開・フォーク・巻き戻しのフラグ
  - プロバイダーのターン ID（最後の assistant フレームの uuid）
  - モデル一覧
  - TodoWrite をプランとして扱う
- Antigravity: ACP ハーネスに `session/load`、steer、プラン、モデル選択、ログインを足した。ログイン（`acp::login`）は URL を通知し、ブラウザでの完了を期限つきで待つ
- **インストーラ**（`antigravity_install.rs`）: 1.2.1 のアーカイブを、zeron と同じ SHA-512 で固定した。手順は次のとおり
  1. curl のサブプロセスで取得する（`--proto =https,http`、サイズ上限、低速なら打ち切り）
  2. SHA-512 を検証する
  3. zip を安全に展開する（`enclosed_name`、リンク禁止、サイズ上限）
  4. 一時ディレクトリから rename する
  5. `current` シンボリックリンクを原子的に張り替える

  取得元のオリジンは固定で、それ以外の URL は拒否する

### 会話の機能（コア）

- **キュー**:
  - 実行中に送ったメッセージは `Queued` の Run になり、前の Run が終わると順に始まる（中断・失敗の後も続く）
  - ユーザーメッセージは、その Run が始まるときにタイムラインの末尾へ移す（`ItemUpdated` の ordinal）
  - 始まる前なら `run.cancel` で取り消せる
  - 再起動時に残っていたキューは「Not sent: …」という通知にして閉じる
- **steer**: 実行中の Run に入力を足す（新しい Run は作らない）。Antigravity は取り消し → 送り直しを1つの Run の中で行う
- **フォーク**:
  - 会話（Run とアイテム）を新しいスレッドにコピーする
  - ネイティブにフォークできるプロバイダーはそれを使う（ロールバックの巻き戻しが保留中でも、残すターンを名指しするのでネイティブのまま）。できなければ、次のプロンプトに会話の書き起こし（上限 24k 文字）を付ける
  - フォーク先は元のスレッドの作業ディレクトリを共有する（worktree なら同じ worktree）
- **プロバイダー切替**: 履歴があれば、文脈の引き継ぎ（同じ書き起こし）と通知を入れる。モデルだけを変えた場合の扱いはプロバイダーで違う
  - ターンごとに変えられるプロバイダー: 次のターンから新しいモデル
  - モデルがプロセス固定のプロバイダー: セッションを作り直して再開
- **チェックポイント**:
  - 各 Run の開始直前に、作業ツリーを丸ごとコミットする（一時インデックス `GIT_INDEX_FILE` ＋ `commit-tree`）。保持には `refs/blongo/checkpoints/<thread>/<run>` を使う
  - ユーザーのインデックス、HEAD、ブランチには触らない
  - git リポジトリでなければ取らない
- **ロールバック**:
  - 指定した Run 以降を `RolledBack` にして、タイムラインから隠す
  - 作業ツリーをその Run のチェックポイントへ戻す（`git restore` → `git clean -fd` → `git reset`）
  - プロバイダー側は能力に応じて巻き戻す
    - Codex: 実行中のセッションに `thread/revert`
    - Claude: 次の起動で `--resume-session-at`
    - Antigravity: 文脈の引き継ぎ
  - 実行中のスレッドでは拒否する
- **worktree**: `thread.create` に `worktree: true` を付けると、`git worktree add -b blongo/<12桁> <data_dir>/worktrees/<thread> HEAD` を実行する
- **t3code のインポート**:
  - `statev2.sqlite`（と `-wal`）を一時ディレクトリにコピーしてから開く。元のファイルは開かない
  - 取り込む対象: プロジェクト（削除済みを除く）、スレッド（アーカイブ状態、プロバイダー、文脈の引き継ぎ待ち）、Run、アイテム（todo_list はプランにする）
  - ID は UUID v5 で決まるので、二度目のインポートでは何も起きない
  - 入口は2つ: サイドバーの「Import from t3code…」と `blongo import-t3 [PATH]`

### UI

- **サイドバー**: 独立したエンティティにして、`cached` ビューで描く（Phase 1 の持ち越し）。ストリーミング中に通知されるのはタイムラインだけなので、サイドバーの要素ツリーは毎フレーム作り直さない。表示は次のとおり
  - worktree（⑂）とフォーク（↳）の印
  - プロバイダーの略号
  - 「+ Worktree」
- **コンポーザー**:
  - プロバイダーとモデルのピッカー。Antigravity の行には Install と Sign in がある
  - 実行中は Enter でキュー、Ctrl+Enter で steer（ボタンもある）
  - キュー中のメッセージは × つきの帯で表示する
- **タイムライン**:
  - キュー中・取り消し・ロールバック済みの Run のアイテムは隠す
  - ユーザーメッセージにホバーすると「Fork from here」「Undo from here」が出る
  - プランはチェックリストで表示する（完了数、進行中、取り消し線）
- **Markdown**:
  - ストリーミング中のブロック分割は Phase 1 の方式のまま
  - 確定したブロックのインラインは pulldown-cmark で解析する（太字、斜体、コード、リンク、取り消し線、箇条書き・番号付き、タスクリスト）
  - 重なったスタイルは区間に分けて合成する
- **コードハイライト**:
  - tree-sitter を使う。対応言語は Rust、Python、JavaScript、TypeScript、Bash、JSON、Go、C
  - 遅延は二重にしている
    - 言語ごとのクエリのコンパイルは、その言語が最初に必要になったとき
    - ブロックごとのハイライトは、初めて画面に出たときにバックグラウンドスレッドで1回だけ。それまでは色なしで描く
  - ストリーミング中の末尾ブロックはハイライトしない。64 KiB を超えるブロックも色なし
- **ターミナル**:
  - portable-pty で `$SHELL`（`BLONGO_TERMINAL_SHELL`）を、スレッドの作業ディレクトリ（worktree ならそこ）で起動する。解釈は alacritty_terminal
  - 読み取りスレッド → エミュレータ → UI への通知は1フレームに1回まで
  - スクロールバックは 1000 行に制限する
  - 対応: 256 色・truecolor、カーソル、マウスホイールでのスクロール、xterm のキー列（アプリケーションカーソルモードを含む）
  - 閉じると、自分が起動したシェルの PID にだけ SIGHUP を送る。0.5 秒で終わらなければ SIGKILL。回収は UI スレッドの外で行う
- **キーボード**:

  | キー | 動作 |
  |---|---|
  | Ctrl+N | 新規スレッド |
  | Alt+F | フォーク |
  | Alt+Z | 直前のターンを取り消し |
  | Ctrl+` | ターミナル |
  | Alt+1/2/3 | プロバイダー |
  | Alt+M | 次のモデル |

### 設定（環境変数、追加分）

- `BLONGO_CLAUDE_EXE`、`BLONGO_ANTIGRAVITY_EXE`: 実行ファイル
- `BLONGO_T3_DB`: インポート元。既定は `~/.t3/userdata/statev2.sqlite` で、コピーしてから読む
- `BLONGO_TERMINAL_SHELL`: ターミナルのシェル
- 計測用:
  - `BLONGO_PROFILE_TERMINAL=1`: 計測中にターミナルを開く
  - `BLONGO_NO_HIGHLIGHT=1`: コードハイライトを止める（A/B 計測用）

### ライセンス

Zed の GPL クレート（editor、terminal、markdown、ui など）は使っていない。依存に入っている Zed のクレートは gpui 系（Apache-2.0）だけ。追加した依存（pulldown-cmark、tree-sitter とその文法、portable-pty、alacritty_terminal、sha2、zip）はすべて MIT か Apache-2.0。`cargo deny check licenses` は `licenses ok`。

## 2. 検証

### 2.1 テストと静的検査

`cargo test --workspace` は **136 本**（Phase 1: 61 本）で、すべてオフラインで動く。実エージェントのターンは一度も走らせていない。

| バイナリ | 本数 | 内容 |
|---|---:|---|
| blongo（アプリ） | 14 | Markdown（pulldown-cmark、入れ子のスタイル、言語つきフェンス、遅延ハイライト）、8言語のハイライト、キー → バイト列、256 色、スクロールバックの上限 |
| blongo-core 単体 | 4 | t3 インポートの時刻・ID など |
| core_codex | 10 | Phase 1 の Codex の経路（実行中の送信は、キュー＋取り消しのテストに変えた） |
| core_phase2 | 21 | 下記 |
| t3_import | 2 | t3code のマイグレーションと同じ列を持つ合成 DB。元ファイルのバイト列が変わらないこと、隣にファイルが増えないこと |
| blongo-git | 5 | チェックポイント（インデックスと HEAD が変わらない）、復元、worktree |
| harness 単体 | 23 | |
| codex_replay | 9 | Phase 1 の5本 ＋ steer、ロールバック、ネイティブフォーク、プラン |
| claude_replay | 10 | t3code の Claude 録音10本 |
| acp_replay | 8 | t3code の ACP 録音7本 ＋ モデル選択 |
| antigravity_install | 3 | ローカル HTTP サーバーでの正常系（検証・展開・`current`）、異常系（ダイジェスト違い、別オリジン、エントリなし、404）、パストラバーサル |
| fake_agents | 9 | 3プロバイダーの承認・中断、Antigravity の再開（履歴を再生しない）、ログイン（ブラウザ待ち、期限切れ） |
| protocol / store | 5 / 13 | |

core_phase2（21本）の内訳:
- Claude Code: 完走、モデル一覧
- Antigravity: 完走とモデル切替
- キュー: 順序、キュー中のクラッシュ
- steer: 3本
- フォーク: Codex、Claude、Antigravity、ロールバック後の Claude（ネイティブのまま）
- プロバイダー切替での文脈の引き継ぎ
- ロールバック: Codex の実行中ロールバックとファイル復元、再起動後のロールバック、Claude のロールバックの引数、実行中のロールバックの拒否
- worktree
- プラン: 3本
- ログイン
- インストーラのオリジン拒否

静的検査:
- `cargo clippy --workspace --all-targets -- -D warnings` と `cargo fmt --all --check` は通る
- `cargo deny check licenses` も通る。cargo-deny はスクラッチの `CARGO_TARGET_DIR` でビルドし、終わってから消した
- テストが `/tmp` に残していた作業ディレクトリ（Phase 1 からのもの）は、テストの終わりに消すようにした

#### リプレイ適合テスト（t3code の録音）

t3code が実 CLI で録ったセッション（commit `8ed276c`）を使う。プロバイダーごとの再生スクリプト（`replay_codex.py` / `replay_claude.py` / `replay_acp.py`）が相手側として録音を再生し、Blongo が送るフレームを録音と突き合わせる。録音中のモデル名などは中立な文字列に置き換えた。

- Claude Code（10本）: simple、承認の許可と拒否（`toolUseID` つき）、中断（ツール実行中を含む）、steer、キュー、ロールバックの引数、ネイティブフォークの引数、`is_error`
- ACP（8本）: simple＋モデル一覧、複数ターン、権限、取り消し、steer（取り消し → 送り直し）、プラン、プロンプトのエラー、`session/set_model`。**録音は t3code の ACP エージェント（Grok）のもので、Antigravity 自身の録音ではない**（t3code に Antigravity の録音はない）
- Codex（9本）: Phase 1 の5本に、`turn/steer`、`thread/revert`、`thread/fork`（別プロセス）、`turn/plan/updated` の4本を足した

### 2.2 完了条件

- **3プロバイダーのスレッドが完走する**: 次の3つすべてで、Codex / Claude Code / Antigravity のスレッドが最後まで動く
  - core_phase2（コア経由、フェイク）
  - 各リプレイ（ハーネス、録音）
  - GUI e2e（§2.3）
- **チェックポイントからロールバックできる**: core_phase2 で、ファイルを書いたターンをロールバックするとファイルが元に戻る（Codex では実行中のセッションに revert し、再起動後もできる）。GUI e2e でも、書いたファイルがロールバックで消えることを確かめた

### 2.3 GUI のエンドツーエンド（`tools/e2e-gui.sh`、Xvfb + lavapipe + xdotool）

フェイクの Codex / Claude / ACP エージェントを使い、Phase 1 の6場面に9場面を足した。release ビルドで全場面が通る。スクリーンショットは `docs/phase2/screenshots/` に29枚ある。

| # | 場面 | 確認したこと |
|---|---|---|
| 1〜6 | Phase 1 と同じ（プロジェクト追加、Markdown、承認、中断、再起動での復元、kill -9 後の回復と孤児なし） | 通過 |
| 7 | Claude Code のスレッドで Python のコードブロック | ハイライトされる（14） |
| 8 | 実行中にキュー → steer | キューの帯（15）、steer の応答（16）、キューの続きが走る（17） |
| 9 | ファイルを書くターン → Alt+Z → 確認バーで Undo | Alt+Z だけでは何も変わらない。確認バーに同じフォルダの3スレッドが出る（18b）。Undo で `notes.txt` が消え、置き換え前のファイルが `refs/blongo/pre-rollback/…` に残る。スクリプトで検査（18, 18b, 19） |
| 10 | Alt+F でフォーク | フォークしたスレッドで続きを話せる（20） |
| 11 | Antigravity のスレッド | プラン（21）、Rust のハイライト（22）、プロバイダーのピッカー（23）、モデル切替（24） |
| 12 | Antigravity → Codex に切替 | 通知と、書き起こしを受け取った応答（25） |
| 13 | 「+ Worktree」 | worktree のディレクトリができる。スクリプトで検査（26） |
| 14 | ターミナル | プロンプト、色、worktree のブランチ名（27）。閉じた後にシェルのプロセスが残らない。スクリプトで検査 |
| 15 | t3code のインポート | 取り込んだプロジェクトとスレッドが出る（28）。元の DB の SHA-1 が変わらない。スクリプトで検査 |

最後に、Blongo の終了後にフェイクエージェントが1つも残っていないことを確かめる。

### 2.4 メモリと CPU（`tools/profile.py`、Phase 0・1 と同じ負荷）

負荷は Phase 0・1 と同じで、52KB の返答をフェイク Codex が 40 ms 間隔で流す。この返答には Rust のコードブロックが 80 個あるので、ハイライトの経路も通る。release ビルド（fat LTO）で2回計測した（`docs/phase2/profiles/`）。

| | Phase 1 | **Phase 2** | 目標 |
|---|---:|---:|---:|
| idle ピーク PSS | 151 MiB | **155 MiB**（154.9 / 155.0） | ≤160 |
| stream ピーク PSS | 166 MiB | **170〜173 MiB**（170.3 / 173.0） | ≤180 |
| settled ピーク PSS | 158〜163 MiB | 168〜171 MiB | — |
| idle ピーク RSS | 174 MiB | 178 MiB | — |
| stream ピーク RSS | 189 MiB | 193〜196 MiB | — |
| idle CPU | 0.31〜0.42% | **0.21〜0.31%** | <1% |
| settled CPU | 0.41〜0.61% | 0.34% | — |
| stream 中 CPU | 112〜115% | 121〜124% | — |
| フェイク Codex（python、別プロセス） | 10.5 MiB RSS | 10.7 MiB RSS | — |

**ターミナルを開いたとき**（`BLONGO_PROFILE_TERMINAL=1`。プロンプトを送るのと同時に bash のターミナルを開く。`blongo-phase2-terminal.json`）:

| | ターミナルなし | ターミナルあり |
|---|---:|---:|
| stream ピーク PSS | 170〜173 MiB | **178.0 MiB** |
| settled ピーク PSS | 168〜171 MiB | 176.5 MiB |
| settled CPU | 0.34% | 0.27% |

子プロセスの bash は別に約 7.6 MiB RSS。

増分の内訳:
- **idle +4 MiB**: 主にコード。バイナリが 25 → 32 MB に増えた（tree-sitter の8文法の構文表、alacritty_terminal、pulldown-cmark、portable-pty）。文法の表は使うまで触らないので、増えた分の一部しか常駐していない
- **stream +4〜7 MiB**: ハイライトを止めた A/B（`BLONGO_NO_HIGHLIGHT=1`、`blongo-phase2-no-highlight.json`）では stream 167.3 MiB。つまり内訳は次のとおり
  - 約 3〜5 MiB: ハイライト（Rust のクエリのコンパイル、色付きテキストのレイアウト）
  - 残り約 1 MiB: pulldown-cmark などのコード
- **ターミナル +5〜8 MiB**: エミュレータのグリッド、読み取りスレッドのバッファ（64 KiB）、PTY。スクロールバックは 1000 行で頭打ちになる（最大で 1000 × 列数 × 24 バイト程度）

stream 中の CPU は Phase 1 の 112〜115% から 121〜124% に上がった。サイドバーは cached ビューになり、ストリーミング中は再描画しない。
- ハイライトの有無では変わらなかった（122%）ので、ハイライトが原因ではない
- 残る候補は、確定ブロックを pulldown-cmark で解析し直す処理と、スタイル区間が増えたテキストの描画。この環境には perf がなく、内訳は特定できていない（§3）
- idle と settled の CPU は Phase 1 より下がった

## 3. 既知のギャップ・リスク

- **実エージェントでの確認なし**: Codex・Claude Code・Antigravity のどれも、本物の CLI でターンを走らせていない（禁止事項であり、認証もない）。フレームの形は t3code の録音とフェイクで確かめた
- **Antigravity の実バイナリは未確認**: `dl.google.com` が塞がれているため、次の範囲でしか試していない
  - ダウンロード、SHA-512、展開、`current` の張り替え: ローカル HTTP サーバー
  - OAuth（`authenticate` と URL）: フェイク ACP エージェント

  1.2.1 のアーカイブの中身（エントリのパス）は zeron の記述からの推定。Antigravity 固有の出力補正（t3code の `AntigravityProtocol.ts`）も、実データでは確認していない
- ACP のリプレイは Grok エージェントの録音で、Antigravity の録音ではない
- フォークと切替での文脈の引き継ぎは書き起こしで行う（上限 24k 文字、古い方から落とす）。ツールの出力は要約しか渡さない
- アーカイブすると、未コミットの変更も未追跡・無視ファイルもなく、他の生きているスレッド（フォークなど）が使っていない worktree は消す（ブランチ `blongo/<id>` は残る）。それ以外は残し、理由を通知する。手で消す UI はない
- チェックポイントの ref は、スレッドをアーカイブすると消す（フォークのコピーした Run がまだ使う ref は残す）。pre-rollback の ref は消さない（`git update-ref -d` で手で消す）。64 MiB を超える未追跡ファイルがある（または合計 256 MiB を超える）フォルダではチェックポイントを取らず、スレッドに通知する
- ロールバックは「その Run 以降をすべて」で、途中の1ターンだけを外すことはできない。そのスレッドか、同じフォルダの他のスレッドが実行中なら拒否する
- ロールバックのやり直し（redo）の UI はない。置き換えたファイルは `refs/blongo/pre-rollback/<thread>/<run>` にあり、通知に出す `git restore` で戻す
- ターミナル:
  - テキスト選択・コピー、IME、アプリへのマウス入力の転送、ベル、タイトル変更はない
  - セルの幅は、フォントの 'm' の送り幅から計算する
  - スレッドを切り替えると閉じる（スレッドごとに1つで、裏では保持しない）
- コードハイライトは8言語だけで、テーマは固定
- モデルの一覧は、プロバイダーが一度起動してから出る（起動前は「Default」だけ）
- Antigravity のサインインはブラウザを `open_url` で開くだけ。ヘッドレス環境向けの代替（コードの貼り付け）はない
- 太字は、この環境の Xvfb のフォントでは太く見えない（スタイルは付いている）
- stream 中の CPU は 124〜128% で、Phase 1（112〜115%）より高いまま。末尾ブロックの再解析は原因ではなかった。内訳は §5.3
- チェックポイント、復元、t3code のインポートは、まだオーケストレーターのループの上で動く（その間、他のスレッドのイベント処理が待つ）。未追跡ファイルの上限で、チェックポイントの最悪の時間は抑えた
- 依存クレートのライセンス本文は同梱していない。`THIRD_PARTY_NOTICES.md` に一覧とライセンス名だけを載せている。配布前に cargo-about などで本文を生成する
- t3code の再インポートは、新しいターンと項目を足すだけ。取り込み済みの Run の状態（実行中 → 完了など）は更新しない
- Windows / macOS: ターミナルの終了処理は Unix のシグナルが前提。計測は Linux のソフトウェア描画だけ
- CI はまだ GitHub 上で走らせていない（push していない）

## 4. 計画からの変更

- PTY とターミナルはアプリ（UI プロセス）側に置いた。計画ではコア側の機能。Phase 3 のサーバーモードでコアへ移す
- ACP の録音は、Antigravity ではなく t3code の別の ACP エージェントのもの
- フォーク先は作業ディレクトリを元のスレッドと共有する。フォークのたびに worktree を作ることはしない
- Phase 1 の持ち越しは Phase 2 の最初のコミット（244f21d）で直した
  - `stop_sessions` の順序
  - PDEATHSIG がスレッドに依存することの注記
  - 回収済みのグループを kill しないこと

  サイドバーの cached 化とストリーミング中の CPU の再確認は、§1 と §2.4 のとおり

## 5. レビュー対応（review fixes）

独立レビューの指摘（CHANGES REQUIRED）に対応した。コミットは 87bd7e3〜3c68da1。

### 5.1 ブロッキング

| # | 指摘 | 対応 | テスト |
|---|---|---|---|
| 1 | あるスレッドのロールバックが、他のスレッドと共有しているフォルダを書き換える | 作業フォルダを正規化して比べ、同じフォルダか入れ子のフォルダで動く他のスレッドを数える。1つでも Run かキューがあれば拒否する。アイドルなスレッドだけなら、その数を `ThreadRollback.acknowledged_sharers` で確認しない限り拒否する（「N other threads work in this folder…」）。UI の確認バーは、そのスレッドの名前を並べる | `rollback_of_a_shared_folder_needs_every_thread_idle_and_a_confirmation`、e2e の場面9（3つのスレッドの名前が出る、18b） |
| 2 | ロールバックが取り消せず、キー1つで走る | 復元の前に今のファイルを `refs/blongo/pre-rollback/<thread>/<run>` に保存する。通知にその ref と、戻すための `git restore --source=<ref> --worktree -- .` を出す。Alt+Z と「Undo from here」は確認バーを出すだけで、「Undo」を押すまで何も変えない。redo の UI はない（§3） | `rollback_keeps_the_replaced_files`（ref に置き換え前のファイルがあり、git で戻せる）、e2e の場面9（Alt+Z だけではファイルが残る、Undo 後に ref の中に notes.txt がある） |
| 3 | ターンが終わった後に届いた steer が、見えないターンを始める | ハーネスは steer をターンに変えない。届かなかった steer は `AgentEvent::SteerNotDelivered` で返し、コアがキューの先頭の Run に移して通知を出す（Codex、Claude Code、ACP のすべて） | `steer_after_the_turn_ended_is_returned_not_run`（3プロバイダー）、`steer_during_an_interrupt_is_returned`、`steer_that_misses_the_turn_is_queued`、各ハーネスの単体テスト |

### 5.2 ノンブロッキング

| # | 指摘 | 対応 |
|---|---|---|
| 4 | チェックポイント、復元、インポートがループの上で動く。未追跡ファイルに上限がない | 未追跡ファイルに上限を付けた（1ファイル 64 MiB、合計 256 MiB。超えるとチェックポイントを取らず、スレッドに通知）。ループの外に出す作業はしていない（§3） |
| 5 | Claude の巻き戻しで、最後に残すターンに ID がないと `keep_through` が None になる | 残すターンのうち、ID を持つ最新のものまで残す。どれも ID を持たなければ引き継ぎ（Handoff）にする。テスト `claude_rollback_past_a_turn_without_an_id_keeps_the_newest_named_one` |
| 6 | `git reset -- .` で、一部だけステージした変更が失われる | チェックポイントに、ユーザーのインデックスのツリーを2つ目の親コミット（"blongo index"）として保存する。復元では `git restore --source <それ> --staged -- .` でインデックスを戻す。競合中のインデックスは保存できないので、そのときは従来どおり HEAD に合わせる。テスト `restore_keeps_what_was_staged` |
| 7 | インストーラー | curl に `--proto-redir =https` を付けた。https の配布元では `--proto =https` にした。コアは、インストール中の2回目の要求を無視する |
| 8 | t3code のインポート | 一時ディレクトリは 0700 で排他的に作り、終わったら消す。t3code が動いている（`-wal` がある）ときは、読み取り専用の接続と SQLite のオンラインバックアップ API で一貫したスナップショットを取る。`-wal` がないときに開くと `-wal` と `-shm` ができてしまうので、ファイルをコピーして整合性を検査する。必須の列が NULL の行は数えて飛ばす。取り込み済みのスレッドにも、新しいターンと項目を後ろに足す（Blongo で実行中のスレッドは次回に回す）。足したスレッドは、次のメッセージで会話を引き継ぎ直す。テスト `reimport_adds_new_turns_and_skips_unreadable_rows` |
| 9 | ターミナル | PTY への書き込み（キー入力とエミュレーターの応答）は、チャネル経由で書き込みスレッドが行う。UI スレッドとエミュレーターのロックの中では書かない。読み取りスレッドは、自分で複製したマスターの fd を 100 ms のタイムアウトで poll する。パネルを閉じると、裏のジョブが PTY を開いたままでも止まる。テストでは、カーソル位置の問い合わせへの応答がチャネルに積まれることを確かめた。e2e の場面14も通る |
| 10 | ストリーミングの CPU | 末尾ブロックは `LiveTail` で描く。段落の完成した行は1回だけ解析し、書きかけの行はそのまま表示する。コードは完成した行の文字列を保持する。コードブロックは行ごとの `SharedString` を1回だけ作り、フレームごとには複製するだけにした。計測は §5.3 |
| 11 | 後片付けとライセンス | アーカイブで ref を消し、変更のない worktree を消す。Claude には `--model=<値>` の形で渡す。ライセンス本文の同梱は未対応（§3） |

### 5.3 再計測（release、2回、`docs/phase2/profiles/blongo-phase2-review-run{1,2}.json`）

| | Phase 2（レビュー前） | **レビュー後** | 目標 |
|---|---:|---:|---:|
| idle ピーク PSS | 155 MiB | **155.0〜155.1 MiB** | ≤160 |
| stream ピーク PSS | 170〜173 MiB | **172.1〜172.6 MiB** | ≤180 |
| settled ピーク PSS | 168〜171 MiB | 171.0〜172.9 MiB | — |
| idle CPU | 0.21〜0.31% | **0.31%** | <1% |
| settled CPU | 0.34% | 0.27〜0.68% | — |
| stream 中 CPU | 121〜124% | 124〜128% | — |

stream 中の CPU は下がらなかった。負荷のコードブロックは確定してから描くので、末尾の解析はもともと小さかった。3回目の計測で、stream 中にスレッドごとの CPU を 8 秒取った。

| スレッド | CPU |
|---|---:|
| UI（メイン） | 45% |
| llvmpipe-0〜3（Mesa のソフトウェア描画） | 各 19〜20%（計 78%） |
| WSI swapchain | 2% |
| blongo-core | 0.2% |

stream 中の CPU の約 6 割は、この環境のソフトウェアラスタライザーが使っている。コア（SQLite、イベント処理）はほぼ使っていない。残りの UI スレッドの 45% は、レイアウトと描画命令の生成。Phase 1 との差は、その中の色付きテキストとコードブロックの描画とみているが、プロファイラがないので切り分けていない。

### 5.4 確認したこと

- `cargo test --workspace`: 149 件すべて通過
- `cargo clippy --workspace --all-targets`: 警告なし
- `cargo fmt --check`: 差分なし
- `cargo deny check licenses`: licenses ok（依存の追加はなく、rusqlite の `backup` 機能を有効にしただけ）
- GUI e2e（release）: 全15場面が通過。ロールバック（場面9、確認と pre-rollback の ref をスクリプトで検査）、steer（場面8）、ターミナル（場面14）を含む。スクリーンショットは29枚に更新した（18b を追加）

### 5.5 再レビューの対応

| # | 指摘 | 対応 | テスト |
|---|---|---|---|
| 1（ブロッキング） | アーカイブで、無視ファイル（`.env` など）ごと worktree を消す。フォークが使っている worktree も消す | `git status --porcelain --ignored` が空のときだけ消す。他の生きているスレッドの作業フォルダ（正規化）が worktree の中にあれば消さない。消さなかったときは `CoreEvent::Notice` で理由を出す（UI は通知欄に表示） | `archive_keeps_a_worktree_with_ignored_files`、`archive_keeps_a_worktree_a_fork_still_uses_and_its_checkpoints`、`worktrees_with_ignored_files_are_kept` |
| 2 | アーカイブで、pre-rollback の ref と、フォークが参照するチェックポイントの ref を消す | 生きているスレッドの Run が参照するコミットの ref は残す（後でフォークもアーカイブしたときに、先にアーカイブしたスレッドの分も掃除する）。pre-rollback の ref は消さない | `thread_refs_still_used_are_kept`、上のフォークのテスト、`rollback_keeps_the_replaced_files` |
| 3 | `git write-tree` が実際のインデックスを書き換える | インデックスを一時ファイルにコピーし、`GIT_INDEX_FILE` でそれを使う | `capture_leaves_the_users_index_file_alone` |
| 4 | 復元の途中失敗や、復元後の decide / commit の失敗で、pre-rollback の ref が伝わらない | 拒否の理由に ref と `git restore` のコマンドを付ける | （失敗を起こすテストはない） |
| 5 | セッション解放後に届いた `SteerNotDelivered` と、`steer` の送信エラーで、文が失われる | どちらもキューに戻す。重複の報告は、その文がすでに自分の Run の先頭なら無視する | `steer_that_misses_the_turn_is_queued`（既存）。解放と競合する場合のテストはない |
| 6 | `acknowledged_sharers` が数だけ | スレッド ID の集合を送り、集合で比べる | `rollback_of_a_shared_folder_needs_every_thread_idle_and_a_confirmation`（別の集合では拒否） |

`cargo test --workspace` は 154 件すべて通過。clippy と fmt も問題なし。GUI e2e は全15場面が通過した（ディスクの都合で debug ビルド）。

### 5.6 3回目のレビューの対応

- **ブロッキング**: worktree を消す前の確認は `git status --porcelain --ignored --untracked-files=all --ignore-submodules=none` にした。ユーザーの設定（`status.showUntrackedFiles=no`、`submodule.*.ignore`）で未追跡のファイルやサブモジュールの変更が隠れることはない。テストは `user_status_settings_cannot_hide_files_from_the_worktree_check` と `ignored_submodule_changes_keep_the_worktree`
- 他の安全確認も見直した。チェックポイントの `add -A`、未追跡ファイルの上限（`ls-files --others --exclude-standard`）、復元の `clean -fd` は、どれも `status.*` の設定に左右されない
- 入れ子のプロジェクトでは、worktree の最上位（`git rev-parse --show-toplevel`）を確かめてから消す。他のスレッドが使っているかの判定も最上位で行う
- 確認と削除の間に、Blongo の外のプロセス（ユーザーのシェルやエディター）がフォルダに書いた分は失われうる。この点をドキュメントコメントに書いた

`cargo test --workspace` は 156 件すべて通過。clippy と fmt も問題なし。

