# Phase 0 結果

- 日付: 2026-10-03
- 対象: Blongo `apps/blongo`（本ブランチ）、zeron `9e1a111`、t3code `8ed276c`
- 環境: Linux（4コア、15GB）、Xvfb、Mesa lavapipe（ソフトウェア Vulkan、`VK_ICD_FILENAMES=lvp_icd.json`、`LP_NUM_THREADS=4`）、ウィンドウ 1280×800
- 負荷: zeron の計測負荷そのもの。Haiku の 52KB の Markdown 返答（`tools/fixtures/resource-stream.jsonl`）を、偽の `claude` CLI（zeron の `scripts/replay-claude.py`）が 40ms 間隔で流す。Blongo も zeron も同じ偽 CLI を自分のハーネスで起動し、本物の stream-json として受け取る
- 計測: 500ms ごとに `/proc` から RSS / PSS / CPU。フェーズは idle（10秒）→ stream（返答完了まで）→ settled（15秒）。Blongo は `tools/profile.py`、zeron は zeron 自身の `scripts/resource-profile.mjs`

## 1. 3者比較

PSS（共有ページを按分した値）が公平な合計値。RSS はプロセスをまたぐ共有ライブラリを二重に数えるので参考値。CPU は 1コア=100%。

| | Blongo（Phase 0） | zeron | t3code（Electron） |
|---|---:|---:|---:|
| プロセス数（エージェント除く） | 1 | 2（UI＋エンジン） | 7（main, renderer, GPU, Node サーバー, network, zygote×2） |
| idle ピーク PSS | **143 MiB** | 222 MiB | 620 MiB |
| stream ピーク PSS | **164 MiB** | 256 MiB | 679 MiB |
| settled ピーク PSS | **162 MiB** | 258 MiB | 742 MiB |
| idle ピーク RSS | 166 MiB | 267 MiB | 1,063 MiB |
| stream ピーク RSS | 187 MiB | 304 MiB | 1,143 MiB |
| idle CPU | **0.3%** | 16.4% | 3.7% |
| stream 中 CPU | 84〜101% | 164% | 18.5%（※） |

- zeron の内訳: UI 188 MiB PSS / エンジン 34 MiB PSS（idle）。UI は idle でも CPU 16% を使い続ける
- ※ t3code は Claude の返答本文をストリーミング表示しない（思考デルタのみ逐次、本文はブロック完了時に一括）。そのため stream 中の CPU は低く、本文 52KB の描画は settled の冒頭にまとめて来る（最大 190%）。詳細は `t3code-baseline.md`
- 生データ: `t3code-baseline.json`、Blongo / zeron は計測出力を `docs/phase0/profiles/` に保存

### この数字の読み方（重要）

1. **ソフトウェア描画の床が大きい。** Blongo の idle 166 MiB RSS のうち、約 60 MiB が `libLLVM`、約 18 MiB が gallium / lavapipe などのソフトウェア Vulkan 実装で、実機の GPU ではこの部分の形が変わる。実機（Metal / ネイティブ Vulkan）での絶対値はまだ測っていない。zeron と Blongo は同じ条件なので**相対比較は有効**だが、目標値（idle 100 MiB 以下）の判定は実機計測で行う
2. **Blongo はまだ機能がほとんどない。** Phase 0 の Blongo はサイドバー、ブロック単位の仮想化タイムライン、Claude ハーネスだけ。zeron との差には「zeron が持つ機能の重さ」が含まれる。機能を足しながらこの差を守れるかが本番で、そのために CI に mem-smoke を入れた
3. **ストリーミングで増える 20 MiB は一度きり。** 同じ返答を10回分（520KB）流しても RSS は 184〜187 MiB で頭打ちになり、本文量に比例しない。グリフアトラスと描画パイプラインのウォームアップと見ている（推定、アロケーション単位では未確認）

## 2. スパイク1: GPUI の土台

| 案 | 結果 |
|---|---|
| 上流 Zed の `gpui` + `gpui_platform`（`badfb8d`） | 描画・ストリーミングとも正常。上記の数値はこれ |
| zeron のフォーク `zeronsh/zui`（`667d0aa`） | ビルドは通るが、Xvfb 上でウィンドウが真っ黒のまま描画されない（stream 中 CPU 0.9%、スクリーンショットは単色）。zeron 本体は同じ環境で描画できているので、zui 側のウィンドウ設定の違いと推定。公平な比較はできなかった |
| `gpui-component` | 試していない。`gpui-pre 0.3.7` 固定で上流と別の GPUI を持ち込むため、Phase 0 の結論（上流固定）と両立しない |

**結論: 上流 gpui を `badfb8d` に固定して進める。** zui の修正のうち必要なもの（`ImageSource::evict` による画像アトラスの解放、GPU 一時バッファの上限）は、画像やブラーを扱う段階で個別に取り込む。Blongo はすりガラス等を使わないので、zui の描画系の追加はそもそも不要。Zed の GPL クレートは使っていない（`deny.toml` で CI 検査）。

## 3. スパイク2〜4: ハーネス

詳細は `spike-claude.md`、`spike-codex.md`、`spike-antigravity.md`。

| エージェント | 実バイナリで確認できたこと | フェイクで確認したこと | ハーネス自身の RSS |
|---|---|---|---|
| Claude Code（2.1.288） | 起動フラグ、`control_request initialize`、1ターンの実応答（※） | ツール承認（許可・拒否）、中断、52KB リプレイ | 約 2.9 MiB |
| Codex（0.160.0） | `initialize`、`account/read`、`model/list`、`thread/start`。`turn/start` は受理されるがモデル接続がプロキシで 403 | 差分ストリーム、承認、中断 | 約 3.0 MiB |
| Antigravity | ACP レジストリで 1.3.0 を確認。**配布元 `dl.google.com` がこの環境のプロキシで拒否され、実バイナリは未確認** | `initialize`〜`session/prompt`、承認、中断、サインイン URL 検出 | 約 3.0 MiB |

- エージェント側のメモリ（実測）: 実 `claude` 約 220〜240 MiB、`codex app-server`（ネイティブ）約 130〜143 MiB。npm の Node ラッパー経由だと Codex は +48 MiB なので、ハーネスはネイティブバイナリを直接起動する。Antigravity の実体は 1.88GB の Python バンドルで、3者で最も重い可能性が高い（未計測）
- 依存の判断: `agent-client-protocol` クレートは採用しない（107 クレート、独自のスレッドプールと非同期ランタイムを持ち込み、`serde_json` の `preserve_order` を全体に強制する）。ACP は serde_json で手書き。`codex-app-server-protocol` も codex リポジトリ内の多数のクレートに依存するため採用せず、インストール済み CLI の `generate-json-schema` との照合で追従する。**計画の「ACP は公式クレートで」は撤回**
- ※ Claude の実応答: 認証なしの挙動を見るつもりで実 CLI を1回起動したところ、この実行環境に用意されていた認証で「Say hello.」の1ターンが実際に完了した（ツールなし、約2秒）。以降は実行していない

## 4. 作ったもの

- Cargo ワークスペース（`blongo-protocol`、`blongo-harness`、`apps/blongo`）、Rust 1.98.1 固定
- `apps/blongo`: サイドバー＋ブロック単位の仮想化タイムライン（Markdown ブロック1つ＝1行、末尾だけ再計測、デルタは1フレーム単位で合体）、プロセス内コア（専用スレッドの current-thread tokio、UI とはシリアライズなしのチャネル）、mimalloc
- `crates/blongo-harness`: Claude（stream-json）、Codex（app-server JSON-RPC）、Antigravity（ACP＋固有補正）。テスト 24 本（オフライン）
- `tools/profile.py`: zeron 互換フェーズの計測スクリプト。`BLONGO_MAX_RSS_MIB` で上限判定
- CI（`.github/workflows/ci.yml`）: fmt / clippy / test（Linux・macOS・Windows）、GPL 混入検査（cargo-deny）、mem-smoke（Linux、ソフトウェア Vulkan、上限 120 MiB）。**GitHub に push できていないため、CI はまだ一度も走っていない**

## 5. Phase 1 への申し送り

- 目標値の判定は実機（macOS Metal）で取り直す。Linux ソフトウェア描画は CI の回帰検知用
- zeron の UI は idle でも CPU 16% を使う。Blongo は 0.3%。アイドル時に描画を止める設計は維持する
- Antigravity の実機確認は、`dl.google.com` に届く環境（ユーザーの手元など）で行う
- Codex は実ターンの確認が未了（認証とモデル接続が必要）
