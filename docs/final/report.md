# Blongo 最終報告（Phase 0〜4）

- 日付: 2026-10-03
- 対象: ブランチ `claude/project-thread-9bd61j`（計測したバイナリは `38b27ef` の release ビルド、fat LTO）
- 比較対象: zeron `9e1a111`（Phase 0 でビルドしたバイナリ、SHA-256 `45ceeb24…`）、t3code `8ed276c`
- 環境: Linux 4 コア・15 GB、Xvfb、Mesa lavapipe（`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`、`LP_NUM_THREADS=4`）、ウィンドウ 1280×800
- 負荷: Phase 0 からずっと同じもの。52KB の Markdown の返答（`tools/fixtures/resource-stream.jsonl`）を、偽のエージェントが 40 ms 間隔で流す。フェーズは idle（10 秒）→ stream（返答が終わるまで）→ settled（15 秒）。500 ms ごとに `/proc` から PSS / RSS / CPU を取る

## 結論

- **t3code（Electron）を Rust + GPUI のネイティブアプリとして作り直した。Node も Electron も使っていない。** 3 つのエージェント（Codex、Claude Code、Antigravity）と汎用 ACP に対応し、ヘッドレスサーバー、リモート環境、diff レビュー、MCP、スケジュール実行まで入っている
- **同じ日・同じ負荷で、zeron よりメモリが少なく、アイドル時の CPU もずっと低い。**
  - idle PSS: Blongo 158 MiB、zeron 210 MiB（25% 少ない）
  - stream PSS: Blongo 177 MiB、zeron 247 MiB（28% 少ない）
  - idle CPU: Blongo 0.3%、zeron 11.6%
  - t3code（Phase 0 の値）と比べると、PSS は約 4 分の 1
- **ヘッドレスサーバー `blongo-serve` は idle PSS 15 MiB。** zeron のエンジン単体（35 MiB）の半分以下
- 実際のエージェントでターンを走らせたことは一度もない。外部サービスはどれも偽物でだけ確かめた（§5）。数値はすべて Linux のソフトウェア描画で取ったもので、macOS の実機では測っていない

## 1. フェーズごとに作ったもの

| フェーズ | 作ったもの |
|---|---|
| 0 土台とベースライン | Cargo ワークスペース。GPUI は上流の `badfb8d` に固定した（zeron のフォーク zui は Xvfb で描画できなかった）。仮想化したタイムライン。3 つのエージェントのハーネスのスパイク（ACP と Codex のプロトコルは serde_json で手書き）。計測スクリプトと 3 者比較。CI の定義（mem-smoke と GPL の混入検査） |
| 1 縦に一本通す | プロセス内のコア（イベントソーシング、SQLite WAL、1 コマンドを 1 トランザクションで書く、冪等なコマンド、再起動後の回復）。Codex のスレッドの作成、ストリーミング、思考、ツール、承認、中断。IME に対応した入力欄。t3code が実際の Codex で録った 5 シナリオのリプレイで適合を確かめた |
| 2 プロバイダーとワークスペース | Claude Code と Antigravity（インストーラ、OAuth）。`ProviderCapabilities`。キューと steer、フォーク、プロバイダーの切り替え（文脈を引き継ぐ）。チェックポイントとロールバック（`refs/blongo/*`）。worktree。PTY ターミナル。tree-sitter のハイライト（8 言語）。Markdown。プランの表示。モデルの選択。t3code のデータの取り込み |
| 3 リモートとサーバー | `blongo-serve`（GPUI を使わない）。`Backend` トレイト（ローカルとリモート）。独自の WebSocket プロトコル（seq、リング、resume）。ペアリング、デバイストークン、Ed25519 の証明。SSH のトンネルと stdio、Tailscale。サーバー側のターミナル。データフォルダのロック |
| 4 機能の厚み | diff とレビューのパネル（ターンごと、スレッド全体、行コメント）。コマンドパレット、when 句つきのキーバインド、設定画面。エージェント向けの MCP サーバー（10 ツール、プロジェクトの外は見えない、スレッドを増やせる数に上限）。ファイルブラウザ、ファジー検索、git 操作。cron によるスケジュール実行。トークン数と費用。GitHub / GitLab の PR 受信箱。署名つきの更新確認。`blongo://` のディープリンク。通知。汎用 ACP。ジョブをオーケストレーターのループの外で動かすようにした |

## 2. 最終のメモリと CPU

Blongo と blongo-serve は今日の最終バイナリで 2 回ずつ測った（`docs/final/profiles/`）。zeron も同じ日に 1 回測り直した。t3code は測り直していないので、Phase 0 の値を載せている。単位は MiB。CPU は 1 コア = 100%。

| | Blongo アプリ | blongo-serve | zeron（UI＋エンジン） | t3code（Phase 0 の値） |
|---|---:|---:|---:|---:|
| 計測 | 今日、2 回 | 今日、2 回 | 今日、1 回 | Phase 0 |
| プロセス数（エージェントを除く） | 1 | 1 | 2 | 7 |
| **idle ピーク PSS** | **157.8 / 157.7** | **14.9 / 15.0** | 210.2（175.2＋35.0） | 620 |
| **stream ピーク PSS** | **177.1 / 177.0** | **17.9 / 18.0** | 247.0（208.0＋39.0） | 679 |
| settled ピーク PSS | 171.7 / 171.1 | 17.9 / 18.0 | 248.3 | 742 |
| **idle CPU** | **0.31 / 0.31%** | 0.0 / 0.0% | 11.6% | 3.7% |
| **idle ピーク RSS** | **180.8 / 180.6** | 16.8 / 16.9 | 253.5 | 1,063 |
| stream ピーク RSS | 200.1 / 200.0 | 19.9 / 20.0 | 292.9 | 1,143 |
| stream 中の CPU | 124 / 122% | 0.6 / 0.7% | 172% | 18.5%（※1） |

- PSS は、共有しているページをプロセスの数で按分した値で、合計を比べるときに公平になる。RSS は参考値
- blongo-serve は描画をしないので、アプリとは役割が違う。ヘッドレスのクライアント（`examples/drive.rs`）が 444 個のデルタ（52,622 バイト）を受け取って最後まで終わった
- 偽のエージェント（python）は約 11 MiB RSS で、どの列にも含めていない
- zeron は Phase 0（idle 222 / stream 256 MiB、CPU 16.4%）より少し低く出た。バイナリは同じもの（SHA-256 が一致）。zeron の計測スクリプトはウィンドウを待つ時間が 2 秒に固定されていて、今日はウィンドウが出るまでに約 5 秒かかり、2 回続けて失敗した。そこで、ウィンドウが出るまで最大 60 秒待つように変えた写しをスクラッチに置いて使った（それ以外は zeron の `scripts/resource-profile.mjs` と同じ）。負荷は replay モードで、実際の claude は起動していない
- ※1 t3code は Claude の返答の本文をストリーミングで描画しない。本文はブロックが終わったときにまとめて描くので、stream 中の CPU が低い（`docs/phase0/t3code-baseline.md`）
- 実際の GPU では、ソフトウェア描画の分（libLLVM 約 60 MiB、gallium 約 18 MiB）が別の形になる。計画の目標（idle 100 MiB 以下）は、macOS の実機で判定する必要があり、まだしていない

## 3. フェーズごとのメモリの推移（Blongo アプリ、PSS）

| | 機能 | idle PSS | stream PSS | idle CPU |
|---|---|---:|---:|---:|
| Phase 0 | サイドバー、タイムライン、Claude ハーネスだけ | 143 | 164 | 0.3% |
| Phase 1 | コア、SQLite、Codex、入力欄 | 151 | 166 | 0.31〜0.42% |
| Phase 2 | 3 プロバイダー、ターミナル、ハイライト、チェックポイント | 155 | 172〜173 | 0.31% |
| Phase 3 | リモートのクライアント（ローカルだけで使った場合） | 156 | 173〜176 | 0.31% |
| Phase 4 | diff、MCP、パレット、git、受信箱、更新、ディープリンク | 156〜158 | 176〜178 | 0.31〜0.42% |
| **最終** | Phase 4 のレビュー対応の後 | **158** | **177** | **0.31%** |

- Phase 0 から最終までで、idle は +15 MiB、stream は +13 MiB。その内訳は次のとおり
  - Phase 1 の +8 MiB は、ほとんどがスワップチェーンの画像。起動時に何フレームか描くので、3 枚とも常駐するようになった
  - そのあとは、ほとんどが常駐するコードの増加。バイナリは 25 MB から 37.5 MB に増えた
- Phase 4 で機能が大きく増えたが、idle の増え方は +1.5 MiB に収まった。ネットワーク用のランタイムは、使うまで起動しない
- 重い画面を開いたときのピークは別に測った（Phase 4）。10 万行の diff を開くと 192.5 MiB で、落ち着くと 172.6 MiB。24k ファイルのリポジトリでファイル一覧と検索を出すと 185 MiB
- blongo-serve: Phase 3 は 13.5 / 16.5 MiB、Phase 4 と最終は 14.9 / 17.9 MiB。増えたのは MCP と、ワークスペースへの問い合わせのコードの分

## 4. テスト

- `cargo test --workspace`（最終コミットで 1 回）: **263 本が通り、失敗 0、ignored 1**。ignored の 1 本は、夏時間のテストが子プロセスで動かす本体
- テストの数の推移: Phase 1 は 61 本、Phase 2 は 156 本、Phase 3 は 206 本、Phase 4 は 263 本。テストはすべてオフラインで動く
- GUI のエンドツーエンド（Xvfb + xdotool）は 3 本あり、Phase 4 の最後にどれも通った
  - `tools/e2e-gui.sh`: 15 場面
  - `tools/e2e-gui-remote.sh`: 8 場面
  - `tools/e2e-gui-phase4.sh`: 11 場面
- clippy（`-D warnings`）、fmt、`cargo deny check licenses` は Phase 4 で通っている
- CI（`.github/workflows/ci.yml`）は、push していないので GitHub 上では一度も走っていない

## 5. 実物で確かめたことと、偽物だけで確かめたこと

| 対象 | 実物で確かめたこと | 偽物・録音だけで確かめたこと |
|---|---|---|
| Codex | `initialize`、`account/read`、`model/list`、`thread/start`（Phase 0）。**実際のターンは走らせていない** | フェイクの app-server。t3code が実際の Codex 0.156.1 で録った 5 シナリオのリプレイ |
| Claude Code | 起動フラグと `initialize`（Phase 0。用意されていた認証で、ツールなしの 1 ターンが意図せず 1 回だけ完了した）。**Blongo からターンを走らせたことはない** | フェイクの stream-json。t3code の録音のリプレイ |
| Antigravity | 実際の 1.2.1 の zip の SHA-512 と、**`initialize` まで**（サインイン URL の検出を含む）。ターンにはサインインが要るので走らせていない | ダウンロードと展開（ローカルの HTTP サーバー）、OAuth、ACP の会話（フェイク）。ACP のリプレイは Grok の録音 |
| SSH / Tailscale | なし | 偽の `ssh`、環境変数で指定したアドレス |
| GitHub / GitLab | なし | 偽のサーバー（`tools/fixtures/fake_forge.py`） |
| 更新サーバー | なし | 偽の HTTP サーバー。署名とハッシュの検証 |
| git、PTY、SQLite、MCP のブリッジ | 本物（テストと e2e） | — |
| macOS / Windows | なし。URL スキームの登録、通知、名前付きパイプは手順の文書だけ | — |

## 6. 既知のギャップ（全フェーズの報告から、重複を除いてまとめたもの）

### 確かめていないこと

- 実際のエージェントのターン（どのプロバイダーも）。Codex 0.156.1 より新しい CLI での差。Antigravity 固有の出力補正を実データで見ること
- 本物の SSH（known_hosts、鍵のエージェント）と `tailscale ip`。本物の GitHub / GitLab の API（ページ送り、権限エラー、GitLab の版による `position` の違い）
- macOS の実機での計測と、macOS / Windows での動作全般。日本語 IME を実機で使うこと

### プラットフォーム

- `PR_SET_PDEATHSIG` は Linux だけ。macOS と Windows では、Blongo を強制終了するとエージェントが残ることがある
- Windows では次のものがない（ビルドとテストは CI で通るが、これらはコンパイル時に外すか、「このプラットフォームでは未対応」のエラーを返す）
  - MCP サーバー（Unix ソケットを使うため）。テストも Unix だけで動かす
  - `blongo-serve` のローカルソケット。既定で作らない。そのため `--stdio` は動いているサーバーへ中継せず、毎回そのプロセスでサーバーを動かす。`blongo-serve revoke` は動いているサーバーの接続を切れない
  - SSH トンネル（`ssh://`）の接続先。手元の口に Unix ソケットを使うため。`ssh+stdio://` は使える
  - ターミナル（ローカルも、サーバー側も）。PTY の読み取りを Unix の `poll` で書いているため。ConPTY への対応はまだ
  - 通知
  - ディープリンクを 2 つ目のプロセスから受け渡すこと
  - ファイルの権限（0600 / 0700）。Windows ではユーザーごとの一時フォルダとプロファイルの ACL に頼っている
  - エージェントの子プロセスをまとめて止めること（プロセスグループがない）。テストの偽エージェントは `.cmd` 経由で python を起動するので、止めるのは `cmd.exe` で、python は標準入力が閉じたときに自分で終わる
- ターミナルの終了処理は Unix のシグナルを前提にしている
- 更新はダウンロードと検証まで。インストールはしない。リリース用の鍵、署名・公証、インストーラがないので、既定では無効
- 依存クレートのライセンス文は `THIRD_PARTY_LICENSES.txt` に同梱した。配布の仕組みそのものはまだない

### セキュリティ・ネットワーク

- サーバーは TLS を内蔵していない。loopback 以外で `--insecure-listen` を使うと、本文は平文で流れる。中継できる攻撃者には接続を乗っ取られる
- MCP のトークンは、同じユーザーのプロセスからは守り切れない（ゆるい境界）
- データフォルダのパスが長いと、MCP のソケットのパスが上限（約 108 バイト）を超えて無効になる。理由はログには出るが、UI には出ない
- `--no-socket` で動いているサーバーは、`revoke` しても、すでにつながっている接続を閉じない

### 機能の制限

- タイムライン
  - テキストを選択・コピーできない
  - 開いたスレッドのアイテムを一度に全部読む（長いスレッドのページングがない）
- 入力欄: 単語単位の移動、Undo、添付がない
- ターミナル
  - 選択・コピー、IME、マウス入力の転送がない
  - スレッドを切り替えると閉じる
- ロールバック
  - 「その Run 以降をすべて」だけ
  - redo の UI がない（`refs/blongo/pre-rollback` から手で戻す）
  - pre-rollback の ref は自動では消えない
- フォークと切り替えでは、書き起こし（24k 文字まで）で文脈を引き継ぐ。ツールの出力は要約だけ渡す
- 汎用 ACP は 1 枠だけ。Grok 系で `session/prompt` が返らないときは、Stop で止めるしかない
- PR レビューで、消えた側の行へのコメントはサマリーに入る
- ハイライトは 8 言語だけで、配色はダーク用だけ。ライトテーマは主な色だけ。ファイルビューアはハイライトなしで、512 KiB まで
- その他
  - プロジェクトを追加するときはパスを入力する
  - Antigravity のサインインはブラウザを開くだけ
  - t3code を取り込み直しても、新しいターンを足すだけ
  - 親のリポジトリで無視されているフォルダでは、チェックポイントを取れない

### 性能

- stream 中の CPU は約 122〜127%。約 6 割は、ソフトウェアのラスタライザー（llvmpipe）が使っている
- 大きな diff を開いた直後のピークは 192.5 MiB。閉じても、開く前の約 156 MiB までは戻らず、168〜172 MiB になる
- リモートのクライアントのコードが常駐するので、ローカルだけで使うときも +1.1 MiB になる
- サーバーの PTY の読み取りは 100 ms の poll のまま。リングは 4096 件 / 2 MiB で、長く切れていたクライアントはスナップショットから戻る

### テストの保守

- GUI の e2e は座標でクリックするので、レイアウトを変えたら直す必要がある

## 7. 計測の再現

```
Xvfb :77 -screen 0 1920x1080x24 -nolisten tcp &   # PID で止める
export DISPLAY=:77 VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json LP_NUM_THREADS=4
cargo build --release -p blongo -p blongo-server --bins --examples
python3 tools/profile.py target/release/blongo OUT_DIR
python3 tools/profile_serve.py target/release/blongo-serve target/release/examples/drive OUT_DIR
```

出力先は短いパスにすること。パスが長いと MCP のソケットが作れず、MCP が無効のまま計測することになる。
