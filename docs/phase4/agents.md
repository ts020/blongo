# 追加エージェントの評価（Phase 4）

Blongo が今話せるのは Codex（app-server）、Claude Code（stream-json）、Antigravity（ACP）の3つと、Phase 4 で足した**汎用 ACP エージェント**（`ProviderKind::Acp`）です。この文書では Grok 系の ACP、OpenCode、Pi、Cursor について、つなぎ方と、汎用 ACP で済むか専用ドライバが要るかをまとめます。

材料にしたのは、各エージェントの公開ドキュメントと、zeron（MIT）のハーネス実装（`crates/harness/src/{acp,opencode,pi,cursor}`）にあるコメントと検証メモです。zeron はこの4つを実機で検証済みで、その結論をそのまま参考にしました。**この文書を書くために実際のエージェントを動かしたり認証したりはしていません。**

## 結論

| エージェント | つなぎ方 | Blongo での扱い | 推奨 |
|---|---|---|---|
| Grok（`grok agent stdio`） | ACP（そもそも ACP 前提で作られている） | 汎用 ACP でそのまま動く見込み | 汎用 ACP で使う。終了検知の補強だけ入れる |
| Devin / Hermes など ACP ネイティブ | ACP | 同上 | 同上 |
| OpenCode | `opencode acp` もあるが、本命は `opencode serve` の HTTP + SSE | 汎用 ACP で動くが、ターン終了の判定が不安定 | まず汎用 ACP。品質が要るなら専用ドライバ（HTTP+SSE） |
| Pi | 独自の JSONL RPC（JSON-RPC ではない） | 汎用 ACP では動かない | 専用ドライバが必要（Phase 5 以降） |
| Cursor | ACP もあるが情報が落ちる。本命は `@cursor/sdk`（Node 内のランタイム） | 汎用 ACP で最低限は動く | 汎用 ACP を案内。SDK 経由は Node が要るので Blongo の軽さと合わない |

## 汎用 ACP エージェント（実装済み）

- 設定画面の「Providers & models → ACP agent command」に、ACP モードで起動するコマンドを1行で書きます（例: `opencode acp`、`grok agent stdio`）。`settings.json` の `acp.executable` と `acp.args` に保存され、**次回起動から**有効になります（コアの実行ファイルは起動時に固定のため）。
- 環境変数 `BLONGO_ACP_EXE` / `BLONGO_ACP_ARGS` が設定されていれば、そちらが優先です。
- プロバイダの選択肢に「ACP agent」が出ます（Alt+4、またはコマンドパレット）。中身は Antigravity と同じ ACP ドライバ（`blongo-harness/src/acp.rs`）で、`session/new` に Blongo の MCP サーバー（`mcpServers`）も渡します。
- 認証はそのエージェント自身のコマンドで済ませてもらいます（Blongo は ACP の `authenticate` を自動では呼びません。環境のトークンを勝手に使わないため）。
- モデル一覧は ACP の `session/new` が返す `models` をそのまま使います。既定モデルを設定画面で指定できます。

## エージェントごとのメモ

### Grok（ACP）

`grok agent stdio` は最初から ACP で作られており、Blongo の汎用 ACP でそのまま話せる見込みです。zeron の検証では、`session/prompt` の RPC 応答が返らないまま止まることがあり、zeron は「`session/update` の最後のメッセージ + プロセスの状態」をターン終了の正としています。Blongo の ACP ドライバは `session/prompt` の応答を終了とみなすので、これに当たると「実行中」のまま残ります。停止ボタン（割り込み → タイムアウトで強制終了）で回復はできますが、Grok を正式に案内する前に、終了検知の補強（一定時間 `session/update` が止まり、かつ最後の更新が完了を示していたら終了とみなす）を入れるべきです。

### OpenCode

`opencode acp` で汎用 ACP として動きます。ただし zeron によれば、OpenCode 自身の ACP 層は「購読後に最初に見た `session.status{idle}`」でプロンプトを完了扱いにし、送ったターンと対応づけていません。`session.error` も無視します。そのため「まだ作業中なのに完了」「黙って止まる」が起きます。サブエージェントや思考の表示も ACP には流れません。

品質を求めるなら、デスクトップ版と同じ `opencode serve`（HTTP Basic 認証つき、SSE で全セッションのイベント）を話す専用ドライバが必要です。サーバーの世代（1.x の `/session/*` と 2.x の `/api/*`）を起動時に見分ける必要があり、ドライバとしては Codex 並みの大きさになります。Blongo の `http.rs` は curl を使う単発リクエスト向けなので、SSE を読むには別の小さな HTTP/1.1 クライアント（tokio の TcpStream で十分）を足すことになります。

### Pi

Pi は JSONL の独自 RPC で、JSON-RPC でも ACP でもありません。ターンの終わりは `agent_end` ではなく `agent_settled` で、さらに古い版では `prompt` の受理直後に `get_state` を挟む「ACK バリア」が要る、といった癖があります（zeron の `pi/PROTOCOL.md`）。汎用 ACP では動かないので、専用ドライバ（行指向の JSON をやり取りする小さなもの）が必要です。MCP は Pi 側の拡張の仕組みで渡す必要があり、Blongo の MCP サーバーをそのまま渡す方法も別途調べる必要があります。

### Cursor

Cursor には ACP の入口もありますが、zeron の検証ではサブエージェントの内容が境界で落ちるなど情報が欠けます。Cursor 自身が統合先に勧めているのは `@cursor/sdk` で、これは Node プロセス内で動くランタイム（Cursor のバックエンドと独自プロトコルで話す）なので、Rust から直接話せる配線がありません。zeron は小さな Node のシムを挟んでいます。Node ランタイム（数十 MiB）を常駐させるのは Blongo の「軽さ」の目標と合わないので、Blongo では**汎用 ACP で使える範囲を案内する**にとどめます。SDK の認証（`CURSOR_API_KEY`）は `cursor-agent login` とは別物で、ここでも Blongo は環境のトークンを使いません。

## 残り（Phase 5 以降の候補）

1. ACP のターン終了検知の補強（Grok 向け）。フェイク ACP エージェントで「`session/prompt` が返らない」を再現するテストから作る。
2. OpenCode の HTTP + SSE ドライバ（任意。汎用 ACP で足りない人向け）。
3. Pi の JSONL ドライバ。
4. ACP エージェントを複数登録できるようにする（今は「ACP agent」1枠だけ）。設定を `acp: [{name, command}]` の配列にし、`ProviderKind::Acp` に名前を持たせる。
