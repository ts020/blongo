# t3code デスクトップ (Electron) リソースベースライン

計測日: 2026-10-03 / 生データ: `t3code-baseline.json`

## 対象・バージョン

- t3code `8ed276c` (v0.0.45、`@t3tools/desktop` + `t3` server)
- Electron 44.4.2 (Chromium 152.0.7977.130, 内蔵 Node 24.21.0)。サーバーは Electron を Node として起動 (`ELECTRON_RUN_AS_NODE`) した `apps/server/dist/bin.mjs`
- ビルド用: Node 24.21.0、pnpm 11.10.0、Claude Agent SDK 0.3.276
- **プロキシ計測ではない (実物の Electron アプリ)**

## 方法

- ビルド: `pnpm install` (root / desktop / server / web / scripts に絞る) → `vp run --filter @t3tools/desktop --filter t3 build` (本番ビルド。Vite の dev サーバーは使っていない)。起動方法は `start:desktop` と同じく `electron dist-electron/main.cjs --no-sandbox` (パッケージ前の状態。AppImage ではない)
- 隔離: `env -i` で起動し、`HOME`/`T3CODE_HOME`/`XDG_*` をすべて `/home/claude/t3-profile-home` の下に向けた。認証情報は渡していない。IPv6 が無いとポート探索で落ちるため、`T3CODE_PORT=3799` を指定
- 表示: Xvfb `:97` 上で動かした (WM なし)。ウィンドウは xdotool で 1280x800 にした。GPU プロセスは Mesa GLX/gallium (llvmpipe) を使っており、**ソフトウェアレンダリング**
- サンプリング: 500 ms ごとに `/proc` から読む。VmRSS は status、Pss は smaps_rollup、CPU は stat の utime+stime から取る。Electron main PID の子孫すべてを合計し、プロセス種別ごとの内訳も取った。CPU% は 1 コア = 100%
- idle: 新規プロファイルでオンボーディングを済ませ、小さな git リポジトリ `demo` を追加して空の New thread 画面にした状態。10 s 待ってから 20 s 計測
- stream: Claude プロバイダの `binaryPath` に偽の CLI (`/home/claude/t3-profile-home/fake-claude.py`) を設定した。この偽 CLI は SDK の control_request (initialize など) に success で応答し、`resource-stream.jsonl` (thinking 7 件 + text 437 件、52 KB の markdown) を 40 ms 間隔で再生する。実 CLI の `--include-partial-messages` と同じ形で、stream_event、ブロックごとの assistant スナップショット、result の順に送る。タイトル生成の `claude -p --json-schema` には 1.2 s 後に JSON を返す。UI からは xdotool で 1 ターン送信した。stream 区間は送信から result まで (約 18 s)、settled 区間はその後の 15 s
- 設定は `providers.codex.enabled=false` (Claude だけを使うユーザーを想定。理由は注意点を参照)

## 結果 (合計、プロセスツリー全体)

| 区間 | RSS 平均 / ピーク (MB) | PSS 平均 / ピーク (MB) | CPU% 平均 / ピーク |
|---|---|---|---|
| idle (20 s) | 1057.9 / 1062.9 | 614.5 / 619.6 | 3.7 / 21.8 |
| stream (18 s) | 1126.3 / 1143.3 | 669.2 / 679.1 | 18.5 / 156.1 |
| settled (15 s) | 1158.4 / 1211.0 | 700.7 / 742.1 | 25.7 / 190.0 |

## プロセス種別ごとの内訳 (RSS 平均 MB / PSS 平均 MB / CPU% 平均)

| 種別 | idle | stream | settled |
|---|---|---|---|
| electron main | 206.7 / 121.8 / 0.3 | 206.9 / 121.9 / 0.3 | 208.8 / 122.8 / 0.4 |
| renderer | 206.1 / 137.7 / 0.6 | 239.8 / 166.7 / 7.9 | 277.7 / 204.3 / 16.5 (ピーク 314 MB, 172%) |
| gpu-process | 178.4 / 120.6 / 1.3 | 190.2 / 127.8 / 6.8 | 185.1 / 123.4 / 2.9 |
| server (Node) | 251.3 / 175.0 / 1.5 | 262.8 / 186.5 / 3.0 | 260.6 / 184.2 / 5.9 |
| network utility | 91.7 / 30.8 / 0.0 | 91.8 / 30.8 / 0.2 | 91.8 / 30.7 / 0.1 |
| zygote x2 | 123.6 / 28.5 / 0.0 | 123.6 / 28.5 / 0.0 | 123.6 / 28.5 / 0.0 |
| 偽 provider CLI (python) | - | 11.1 / 7.1 / 0.3 | 10.8 / 6.7 / 0.0 |

## 観察

- **t3code は Claude の本文を逐次描画しない。** ClaudeAdapterV2 が stream_event から拾うのは thinking の delta だけで、本文はブロックが完了したときの `assistant` スナップショットから取る。そのため stream 中の UI には「Working for Ns」と reasoning の 1 行しか出ない (途中のスクリーンショットで確認した)。52 KB の本文は完了時に一度にレンダリングされ、そのスパイク (renderer 約 170%、合計約 190%、RSS +約 90 MB) は settled 区間の頭に入る。
- stream 中の CPU は送信直後のスパイクを除くと合計 10〜14% で安定している。大半は renderer と GPU が Working タイマーなどを再描画している分 (ソフトウェア GL)。サーバーは text delta を捨てるのでほぼ 0〜2%。
- ターン完了の約 8 s 後に、サーバーと renderer に約 100% の短いバーストがもう一度出る (チェックポイントなど、ターン後の処理)。

## 注意点

- ソフトウェアレンダリング (llvmpipe) と Xvfb の組み合わせなので、GPU プロセスの CPU は実 GPU 環境より大きく出ている可能性がある。
- root で `--no-sandbox` を付けて動かした。asar に固めておらず、AppImage でもない。
- 各区間 1 回ずつの計測。500 ms 間隔なので短いスパイクや、500 ms 未満で終わるプロセスの CPU は取りこぼす。
- RSS は Chromium 系プロセス間の共有ページを重複して数えるので、合計値は PSS のほうが妥当。
- 偽 CLI は python で約 10 MB。**実際の Claude Code CLI プロセス (数百 MB 規模) は含まれていない。** ネットワーク遅延も無い。
- 既定設定 (codex 有効) で最初に回した run では、タイトルやブランチ名の生成に既定の codex が使われた。このとき `codex exec` (Node ラッパー + ネイティブ、1 組約 185 MB) が 3 回起動し、openai/plugins の git fetch も走った。この run の値は stream の RSS ピーク 1369.5 MB / PSS 887.7 MB、settled の RSS ピーク 1404.8 MB / PSS 924.0 MB。比較を公平にするため本表からは外し、JSON の `supplementary_run1_codex_enabled_default` に残した。
- ビルドに `libsecret-1-dev` が必要だったので apt で入れた。
- 計測中、同じマシンで cargo ビルドが 2 本動いていた (loadavg 0.4〜1.7)。
