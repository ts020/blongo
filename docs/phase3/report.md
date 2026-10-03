# Phase 3 結果: リモートとサーバーモード

- 日付: 2026-10-03
- 環境: Phase 0〜2 と同じ（Linux 4コア、Xvfb、Mesa lavapipe、`LP_NUM_THREADS=4`、ウィンドウ 1280×800）
- 結論: **`blongo serve`（ヘッドレス、GPUI なし）が動き、アプリはそれを2つ目の環境として扱える。** ペアリングしてから、リモートのスレッドで次のことができる
  - 承認と中断
  - Markdown のストリーミング
  - サーバー側のターミナル

  回線が切れても再接続して、欠けも重複もなく続きを受け取る（resume）。サーバーを再起動したときは新しいスナップショットに切り替わる。どちらも GUI e2e で確かめた（§2.3）
- サーバーのメモリ: idle で PSS 13.5 MiB、フェイク Codex のストリーム中で 16.5 MiB（§2.4）
- ローカルだけで使うときのアプリ: idle PSS 156 MiB、stream PSS 173〜176 MiB。同じ日に Phase 2 のバイナリを測った値より idle で +1.1 MiB 多い。原因はリモートクライアントのコード（+2 MB）が常駐する分で、ヒープは増えていない（§2.4）
- SSH と Tailscale は実物がない環境なので、偽の `ssh` と Tailscale のアドレス指定（環境変数）でだけ確かめた（§3）

## 1. 作ったもの

```
apps/blongo (GPUI)                          crates/blongo-server（新規、bin: blongo-serve）
  Shell ── Vec<Env{Arc<dyn Backend>}>          accept（TCP/WebSocket・Unix ソケット・stdio）
   ├─ env 0: LocalBackend（プロセス内）          conn: ハンドシェイク → 認証 → 読み書き
   └─ env n: RemoteBackend ──WebSocket──▶       hub: 全接続へ配る。seq を振り、リング（再送用）、Outbox
  Sidebar: 環境ヘッダ + 接続状態                 auth: ペアリング、デバイストークン、DPoP 型の証明
  TerminalView: ローカル PTY / サーバーの PTY    pty: サーバー側ターミナル（接続ごとに最大4）
                                              blongo-core（Phase 2 のまま、同じプロセス内）
crates/blongo-client（新規）
  Backend トレイト、LocalBackend（feature local）、RemoteBackend（監督タスク、再接続、resume）
  transport（WebSocket / 長さ前置きストリーム）、target（ws / ssh トンネル / ssh stdio）
  secret（0600 の保存、定数時間比較、Ed25519 の証明）、environments.json、pairing、backoff
crates/blongo-protocol
  client.rs: CoreEvent、ConnectionState、TerminalEvent など（UI とバックエンドの境界の型）
  wire.rs: ワイヤ型、エンコード（MessagePack）、フレーム、交渉、証明メッセージ
```

### Backend の抽象化

- UI は `Backend` トレイト（`dispatch` / `open_thread` / `login` / `install_antigravity` / `import_t3` / `terminal_*`）と `CoreEvent` のチャネルだけを見る。ローカルとリモートを区別するのは `is_remote()` だけで、ターミナルの起動先を選ぶのに使う
- `LocalBackend(CoreClient)` は Phase 2 と同じ経路で、型のある値をチャネルで送るだけ。シリアライズはしない。イベントの本文は `Arc<str>` のまま共有する
- `RemoteBackend` はサーバーのフレームを、プロセス内コアと**同じ `CoreEvent` 値**に戻す。UI はシーケンス番号を直接見ない。重複を捨て、順序を守るのは `blongo-client::remote::StreamState` の役目
- 環境ごとにイベントを汲み上げ、`EnvId` を付けて Shell に渡す。各経路で、1回の起床で溜まった分をまとめて処理する（Phase 2 と同じ）

### プロトコル（Blongo 独自）

- **フレーム**: WebSocket では1バイナリメッセージ、ストリーム（ssh stdio、Unix ソケット）では u32 ビッグエンディアンの長さ前置き。上限はクライアント → サーバーが 1 MiB、サーバー → クライアントが 16 MiB
- **エンコード**: MessagePack（`rmp_serde::to_vec_named`、構造体はマップ）。タグ付きの domain enum と UUID（16バイト）がそのまま往復する
  - サーバーは1フレームに複数のメッセージを入れる（`ServerFrame(Vec<ServerMsg>)`）。書き込み1回あたり 256 KiB まで
- **交渉**: `Hello{versions, caps}` → `Challenge{version, caps, server_id, nonce(32B), epoch}` → `Auth(...)` → `Welcome{device_id, issued?, resumed}` または `Refused{code}`
  - `PROTOCOL_VERSION = 1`。機能フラグは `terminal` / `provider-setup` / `import`
  - 共通のバージョンがなければ `Refused(Version)` を返す。これは恒久エラーで、再試行しない
- **スナップショットと購読**: 接続直後は `Shell`（全プロジェクトと全スレッド）を送る。`Subscribe{threads}` を受けたら、そのスレッドの `Thread` スナップショットを送り、以後そのスレッドのタイムラインのイベントも流す。購読していないスレッドには、サイドバーに要る状態の変化だけを流す
- **シーケンス番号**: ハブは状態を運ぶペイロード（Shell / Thread / Event / TextDelta）すべてに、プロセス内で単調に増える `seq` を振る。最近の分はリングに残す（4096 件 / 2 MiB）
  - 再接続したクライアントは `Resume{epoch, last_seq, threads}` を送る。epoch が一致し、`last_seq` がリング内にあれば、それより後の分だけを再送する（resume）。そうでなければ新しいスナップショットを送る
  - クライアントは `seq ≤ last_seq` のメッセージを捨てる（スナップショットは除く）。また、スナップショットがまだ届いていないスレッドのタイムラインイベントは無視する。これで欠けも重複も起きない
- **デルタの合体と背圧**: 接続ごとの `Outbox` に上限がある（既定 4 MiB / 8192 件）
  - 同じ item への連続した TextDelta は1つにまとめ、最後の seq を残す
  - 溢れたら、状態を運ぶ Seq の溜まり分を捨てる（制御メッセージとターミナルは残す）。そのうえで `Resnapshot` を送り、スナップショットを取り直させる
  - 60秒間に5回溢れたら、その接続を切る。書き込みのタイムアウトは30秒
  - WebSocket の書き込みバッファは 16 MiB + 64 KiB で頭打ちになる
- **コマンド**: `CommandEnvelope` の id で冪等になる。再接続後、答えをもらっていないコマンド（最大64件、120秒以内）をクライアントが送り直す。サーバー側では重複として扱われ、二重には書き込まれない。オフラインのときに送ったコマンドは、その場で `<name> is not connected` として拒否する
- **生存確認**: 20秒送信がなければ Ping。45秒なにも届かなければ切断扱いにする

### 認証

- **ペアリング**: サーバーで1回限りのコードを発行する（`blongo-serve pair`、または `--pair` で起動時に表示）
  - コードは10文字（`XXXXX-XXXXX`、紛らわしい文字を除いた30文字種）、既定の有効期間は10分。サーバーにはコードの SHA-256 と期限だけを保存する
  - 600秒間に10回失敗すると、未使用のコードを全部無効にする
- **長期資格情報**: ペアリングが成功すると、サーバーはデバイス ID と 256 ビットのトークンを発行する。サーバーが保存するのはトークンの SHA-256 とデバイスの公開鍵だけ
  - クライアントは `environments.json`（0600、ディレクトリは 0700、一時ファイル経由で原子的に置き換える）に、server_id / device_id / token / Ed25519 の秘密鍵を保存する
- **DPoP 型の所持証明**: 認証のたびに Ed25519 で署名する
  - 署名対象: `"blongo-proof-v1\0"`、目的（Token / Pair）、server_id、そのセッションの nonce、jti（16B）、公開鍵、iat、秘密（トークンまたはコード）の SHA-256
  - サーバーは iat の窓（古さ300秒まで、未来へのずれ5秒まで）を見て、`verify` と `verify_strict` の両方を通す。さらに jti のリプレイキャッシュ（16384件）で同じ証明の使い回しを拒否する
  - nonce は接続ごとに新しいので、盗聴した証明は別の接続では通らない
- **比較と失敗**: トークンとコードの比較は `subtle` による定数時間比較。拒否の理由はクライアントに返さず（`Unauthorized` のみ）、応答を 500 ms 遅らせる
- **秘密をログに出さない**: `Debug` 実装でトークンと鍵を伏せ字にする。サーバーのログに出るのはデバイス名と ID だけ。例外は、運用者が手で写すための起動時のペアリングコードの表示（e2e のログでは伏せ字にした）
- **ローカル認証**: Unix ソケット（`<data>/server/blongo.sock`、0600）と `--stdio` は、ファイル権限か SSH で認証済みとみなし、`Auth::Local` を受け付ける。TCP では `Local` を拒否する
- デバイスの一覧と取り消し: `blongo-serve devices` / `revoke ID_OR_NAME`

### TLS とネットワークの方針

- **TLS は内蔵しない**。`wss://` の指定は、理由を書いたメッセージで拒否する
- 待ち受けアドレスの方針
  - 既定: loopback（`127.0.0.1:7878`）
  - Tailscale のアドレス（100.64.0.0/10、fd7a:115c:a1e0::/48）は許可。`--tailscale` で自分の tailnet アドレスに bind する（`$BLONGO_TAILSCALE` か `tailscale ip -4`）
  - それ以外は `--insecure-listen` を付けない限り拒否する
- loopback 以外へ出るときは、Tailscale（WireGuard で暗号化）、SSH のトンネル、SSH の stdio、または前段の TLS 終端（リバースプロキシ）を使う前提。トークンと証明は平文の WebSocket でも盗聴から再利用できない（nonce に結び付いた証明が要る）。ただし、本文の秘匿と改ざん防止は経路側に任せる

### SSH と Tailscale

- 接続先（target）は3種類
  - `ws://host[:port][/path]`: 既定ポート 7878、パス `/ws`。Tailscale のアドレスもこれで指定する
  - `ssh://[user@]host[:p]?port=N`: `ssh -o BatchMode=yes -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -L 127.0.0.1:L:127.0.0.1:N host` でトンネルを張り、ローカルの L に WebSocket で接続する。ssh のプロセスは接続と一緒に終わる（kill_on_drop）
  - `ssh+stdio://[user@]host[?command=...]`: `ssh -T host blongo-serve --stdio` の標準入出力を、長さ前置きのフレームで使う
- `--stdio` は、動いているサーバーがあればその Unix ソケットへ橋渡しする。なければ、その1接続だけのためにサーバーをプロセス内で動かす
- ssh のプログラムは `$BLONGO_SSH` で差し替えられる（テストでは偽の ssh を使う）

### アプリ（UI）

- サイドバー: 環境がローカルだけのときは Phase 2 と同じ見た目で、Phase 2 の e2e の座標もそのまま通る。リモート環境があると、環境ごとに次のものが付く
  - ヘッダー: 状態の点、名前、「+ Project」
  - 状態の行: connected / connecting / offline, retrying in Ns (attempt n) / failed
  - 直近のエラー
- 「+ Environment」: `NAME TARGET [CODE]` と入力する。ペアリングはネットワーク用スレッドで行い、成功したら `environments.json` に保存して接続する。失敗したらフォームに理由を出す
- 再接続のバックオフ: 3s → 4s → 8s → 16s（以後 16s）。30秒健全に接続できていたら最初に戻す。認証の拒否、バージョン不一致、サーバー ID の不一致は恒久エラーとして `failed` にし、再試行しない
- リモートのスレッドでは、ヘッダーの場所の表示に環境名が付く（`devbox · /path`）。モデル一覧は環境ごとに持つ
- **リモートのターミナル**: サーバーの PTY を使う
  - サーバー側はスレッドの作業フォルダで `$SHELL` を起動する。出力は Outbox を通る（背圧あり）
  - クライアント側は、ローカルと同じ alacritty_terminal のエミュレータで出力を描く。入力は専用スレッドからバックエンドへ送る
  - パネルを閉じるか、接続が切れると終わる。サーバーは自分が起動したシェルの PID にだけ SIGHUP を送り、必要なら SIGKILL へ上げる
- ネットワーク用のランタイム（`blongo-net`、current_thread）は、リモート環境を使うときに初めて起動する。ローカルだけならスレッドは増えない（32本のまま）
- CLI
  - `blongo serve ...`: 同じディレクトリの `blongo-serve` を exec する。UI のプロセスはサーバーにならない
  - `blongo env add NAME TARGET [CODE]` / `env list` / `env remove NAME`
  - 設定の場所は `$BLONGO_CONFIG_DIR`（既定は OS の設定ディレクトリの `blongo`）

### Phase 2 からの持ち越し

- **worktree の削除の追加ガード**: 次の3条件を満たさない限り消さない（`blongo_git::owned_worktree_top`）
  - 解決済み（canonicalize 済み）の top が `<data_dir>/worktrees` の厳密に内側にある
  - git common dir がプロジェクトと一致する
  - linked worktree である（main checkout ではない）

  `..` やシンボリックリンクで外を指させても通らない。テストを1本追加した
- **データディレクトリのロック**: 同じ DB を2つのコア（アプリとサーバーなど）が同時に開かないよう、`<db>.lock` を `File::try_lock` で取る。取れなければ `CoreEvent::Failed` で理由を出す
- チェックポイント／復元／インポートをオーケストレーターのループの外へ出す件と、ライセンス文の同梱は §3 を参照

### Antigravity の実バイナリ（コーディネーターの確認、c02ff1f）

別の作業で、実際の Antigravity を確かめた（このフェーズの作業ではない。コミット c02ff1f）。

- ダウンロードした実際の 1.2.1 の zip は、固定してある SHA-512 と一致した。1.3.0 の SHA-512 も記録した
- 実バイナリで `initialize` まで通った。ターンを走らせるにはサインインが要る
- ACP のハーネスは、サインインの URL を stderr から読むようになった（約3秒で `auth_required` と URL 全体が出る。`acp::login` の中でも、セッションの途中でも同じ）
- Linux で IPv6 がない環境では、`--enforce_kernel_ipv6_support=false` を付けて起動する
- 実エージェントは idle で約 300 MiB RSS
- このコミットを含めた状態で、ワークスペース全体のビルド・テスト・clippy が通ることを確かめた（§2.1）

## 2. 検証

### 2.1 テストと静的検査

`cargo test --workspace` は **198 本**（Phase 2: 136 本）で、すべてオフラインで動く。実エージェントのターンは走らせていない。環境変数のトークンで何かを認証することもしていない。

| バイナリ | 本数 | Phase 3 で増えた主なもの |
|---|---:|---|
| blongo（アプリ） | 16 | |
| blongo-protocol 単体 | 15 | ワイヤの往復（全 item 種別と本文、スナップショット、デルタ、クライアントメッセージ）、ID が 16 バイトになること、**2万回の変異ファズ**（パニックしない）、上限超過の拒否、フレームの分割・再構成・上限、証明メッセージがすべての入力に結び付くこと、バージョン・機能の交渉、タイムラインの振り分け |
| blongo-client 単体 | 12 | バックオフの段と30秒でのリセット、environments.json（0600、Debug で伏せ字）、**StreamState**（再送の重複を捨てて順序を守る、新しい epoch で保持中のタイムラインを捨てる、他スレッドと Resnapshot、本文が往復する）、証明（目的・サーバー・nonce・秘密・時刻・鍵のどれが変わっても失敗）、0600 で原子的な書き込み、定数時間比較、target の解析 |
| blongo-server 単体 | 8 | ペアリング（1回限り、トークンが鍵に結び付く）、**期限切れのコードと古い証明**、総当たりでコードが全部無効になる、ID が変わらない、待ち受けの方針、Outbox（合体と最後の seq、溢れたら状態を捨てて制御は残す、drain と close） |
| **remote（e2e、新規）** | 11 | 下記 |
| blongo-git | 13 | worktree 削除のガード（main checkout、外側、ルート自身、`..`、別リポジトリ） |
| blongo-core 単体 / core_codex / core_phase2 / t3_import | 4 / 10 / 27 / 3 | |
| harness 単体 / 各リプレイ / fake_agents / antigravity_install | 25 / 27 / 11 / 3 | c02ff1f の分を含む |
| store | 13 | |

`crates/blongo-server/tests/remote.rs`（実際の `blongo serve` とフェイク Codex を、WebSocket 越しにクライアントから動かす）:

1. `turn_with_approval_and_interrupt_over_the_wire`: ペアリング → プロジェクトとスレッド → 承認つきのターンを完走 → 中断
2. `bad_credentials_replayed_proofs_and_expired_codes_are_refused`: 次のものがすべて `Unauthorized` になる
   - 違うトークン
   - 別の鍵の証明
   - **同じ証明の再送（jti のリプレイ）**
   - 使用済みのコード
   - **期限切れのコード**
   - 別サーバーの資格情報（WrongServer）
3. `resumes_after_a_dropped_link_without_gaps_or_duplicates`: ストリーム中にプロキシで回線を切り、つなぎ直す。resume され、受け取った本文に欠けも重複もない
4. `a_restarted_server_sends_fresh_snapshots`: epoch が変わるのでスナップショットに切り替わる
5. `a_slow_reader_is_resnapshotted_with_bounded_memory`: 1 KiB × 4000 のデルタを、受信バッファを絞った遅いクライアントへ流す
   - Outbox のピークは上限（256 KiB）+ 1件以内に収まる
   - Resnapshot の後、スナップショットと続くデルタで全文がそろう
6. `resume_points_outside_the_ring_get_snapshots`
7. `ssh_tunnel_and_ssh_stdio_with_a_fake_ssh`: 偽の ssh（python）で `-L` のトンネルと stdio を確かめる。引数もログで検査する
8. `stdio_without_a_running_server_serves_in_process`
9. `unix_socket_clients_authenticate_by_file_permissions`
10. `server_side_terminal_runs_in_the_thread_folder`
11. `tailscale_listen_uses_the_tailnet_address_and_refuses_others`

静的検査:
- `cargo clippy --workspace --all-targets -- -D warnings` と `cargo fmt --all --check` は通る（c02ff1f を含めて）
- `cargo deny check licenses` も通る（licenses ok）。cargo-deny はスクラッチにインストールし、終わってから消した
- 新しい依存はすべて寛容なライセンス: tokio-tungstenite（default-features なし、`handshake` のみ）、futures-util、rmp-serde、serde_bytes、ed25519-dalek、subtle、sha2、getrandom、base64、libc。axum や hyper のような HTTP フレームワークは使っていない

### 2.2 完了条件

- **`blongo serve` と RemoteBackend**: サーバーは GPUI をリンクしない別バイナリ（5.6 MB）。アプリはそれを2つ目の環境として扱う（e2e と GUI e2e）
- **独自プロトコル**: バージョンと機能フラグの交渉、seq つきのスナップショットと購読、欠けも重複もない resume、デルタの合体、上限つきのバッファ、遅いクライアントの再スナップショット（テスト 3〜6）
- **認証**: ペアリング、bearer、DPoP 型の証明、0600、定数時間比較（テスト 2 と単体テスト）
- **SSH と Tailscale**: 偽物で確かめた（テスト 7、11）
- **接続レジストリ**: 複数の環境、バックオフ、状態の表示（GUI e2e）
- **リモートのターミナル**: 実装した（テスト 10、GUI e2e の場面8）

### 2.3 GUI のエンドツーエンド（`tools/e2e-gui-remote.sh`、Xvfb + lavapipe + xdotool）

アプリとローカルの `blongo-serve` の間に TCP の中継（`tools/fixtures/tcp_proxy.py`）を置き、それをネットワークの代わりにした。release ビルドで全場面が通る。スクリーンショットは `docs/phase3/screenshots/` に14枚ある。

| # | 場面 | 確認したこと |
|---|---|---|
| 1 | ローカルにプロジェクト | Phase 2 と同じ見た目（r01） |
| 2 | 「+ Environment」で `devbox ws://127.0.0.1:PORT CODE` | ペアリングされ、環境ヘッダーが connected になる（r02, r03）。`environments.json` が 0600 で、コードは保存されていないことをスクリプトで検査 |
| 3 | 環境の「+ Project」でサーバー側のフォルダ | リモートのスレッドができ、ヘッダーに `devbox · /path` が出る（r04） |
| 4 | 承認 | 承認のカードが出て、Approve で完走する（r05, r06） |
| 5 | 中断 | 実行中の表示 → Stop で中断される（r07, r08） |
| 6 | Markdown のストリーム中に中継を kill → 再起動 | `offline, retrying in 3s (attempt 1)` と表示される（r09）。再接続後に resume し（サーバーのログ `resumed after seq 161, replaying 28`）、本文が最後までそろう（r10）。スクリプトで検査 |
| 7 | サーバーを止めて再起動 | `the server stopped` の通知と offline の表示（r11）。再起動後は `cannot resume ... sending snapshots` でスナップショットに切り替わり、タイムラインが元どおりになる（r12）。続けて送ったターンがサーバーの DB に入る（r13）。スクリプトで検査 |
| 8 | ターミナル | サーバーの PTY でスレッドのフォルダが開く（r14）。シェルの親が blongo-serve であることと、パネルを閉じたらシェルが止まることをスクリプトで検査 |

Phase 2 の `tools/e2e-gui.sh`（15場面）も、このフェーズの release ビルドでそのまま通った。この e2e は独自の設定ディレクトリを使うようにした。Xvfb はこちらで起動したものを PID で止めた。

### 2.4 メモリと CPU（`tools/profile.py` / `tools/profile_serve.py`、Phase 0〜2 と同じ負荷）

負荷は同じで、52KB の返答をフェイク Codex が 40 ms 間隔で流す。release ビルド（fat LTO）で計測した。同じ日に Phase 2 の最終コミット（79261d9）のバイナリも測って並べた。結果は `docs/phase3/profiles/` にある。

**ローカルだけのアプリ**（リモート環境なし）:

| | Phase 2（記録） | Phase 2（同じ日） | **Phase 3** |
|---|---:|---:|---:|
| idle ピーク PSS | 155 MiB | 154.8 / 155.0 | **156.1 / 156.1** |
| stream ピーク PSS | 170〜173 MiB | 170.5 / 174.2 | **173.2 / 176.2** |
| settled ピーク PSS | 168〜171 MiB | 170.7 / 170.8 | 170.6 / 173.6 |
| idle CPU | 0.21〜0.31% | 0.31 / 0.42% | **0.31 / 0.31%** |
| stream 中 CPU | 121〜124% | 126% | 126〜127% |
| スレッド数 | 32 | 32 | 32 |

idle の +1.1 MiB は、マッピングごとの PSS で見るとすべて実行ファイルのマッピングで増えている（20.0 → 21.1 MiB）。匿名メモリとヒープは同じ。

- バイナリは 32.3 → 34.8 MB になった。リモートクライアント（tungstenite、MessagePack のデコード、Ed25519 など）の分
- リモートの経路を参照しないようにした診断ビルドでは、idle が 155.2〜155.4 MiB に戻った
- つまり、使わないコードでもページの一部が常駐する分。`MADV_RANDOM` で実行ファイルのフォールトアラウンドを止める案も試したが、変わらなかったので入れていない
- stream の差は回ごとのばらつき（同じ日の Phase 2 で 170.5〜174.2）と同程度

**リモート環境を1つ接続したアプリ**（`blongo-phase3-with-remote.json`。ローカルのストリームはそのまま、サーバーとは接続を保つだけ）:

| | ローカルだけ | リモート接続あり | 差 |
|---|---:|---:|---:|
| idle ピーク PSS | 156.1 | 158.1 | +2.0 MiB |
| stream ピーク PSS | 173〜176 | 179.9 | +4〜7 MiB |
| settled ピーク PSS | 171〜174 | 176.0 | +2〜5 MiB |
| idle CPU | 0.31% | 0.42% | |
| スレッド数 | 32 | 33（`blongo-net`） | +1 |

**`blongo serve`**（`blongo-serve.json`。ヘッドレスのクライアント `examples/drive.rs` が接続し、idle の後にストリームを受け取る）:

| | idle | stream | settled |
|---|---:|---:|---:|
| ピーク PSS | **13.5 MiB** | **16.5 MiB** | 16.6 MiB |
| ピーク RSS | 15.4 MiB | 18.6 MiB | 18.6 MiB |
| CPU | 0.0% | 0.68% | 0.0% |
| スレッド数 | 3 | 2 | 2 |

フェイク Codex（python、別プロセス）は 10.7 MiB RSS。クライアントは 444 個のデルタ（52,622 バイト）を受け取って完走した。サーバーのストリーム中の CPU は 1% 未満で、UI の描画がないぶん、アプリ（126%）とは桁が違う。

## 3. 既知のギャップ・リスク

- **SSH と Tailscale は実物で未確認**: ssh / sshd / tailscale がない環境。次の範囲でしか確かめていない
  - SSH: 偽の ssh（`-L` を python で中継し、それ以外は `sh -c` で実行）。引数と、トンネルや stdio の動作
  - Tailscale: `$BLONGO_TAILSCALE` によるアドレス指定と、待ち受けの方針

  実際の ssh の known_hosts や鍵のエージェント、`tailscale ip` の出力形式の違いには当たっていない
- **TLS は内蔵しない**（§1）。loopback 以外で `--insecure-listen` を使うと、本文は平文のまま流れる。資格情報は再利用されないが、盗み見は防げない
- **チェックポイント／復元／インポートは、まだオーケストレーターのループの中で動く**。重いリポジトリでチェックポイントを取っている間、ほかのスレッドのイベントが待たされる。スレッドごとに直列化してループの外へ出す作業は Phase 4 に回す。リモートではこの待ちが、そのまま全クライアントへの配信の遅れになる
- **ライセンス文の同梱**: `THIRD_PARTY_NOTICES.md` に新しい依存を書き足した。ただし、バイナリ配布に必要な各クレートのライセンス文をまとめる仕組み（`cargo about` など）はまだない。配布を始める前に入れる
- **ローカルのアプリの常駐量が +1.1 MiB**: リモートクライアントのコードの分（§2.4）。機能フラグで外せるようにする手もあるが、既定のビルドに要る機能なので入れていない
- リモートのスレッドでのロールバック確認は、同じフォルダを使うスレッドをパスの文字列で比べる。サーバー側のシンボリックリンクは解決しない
- サーバーのリングは 4096 件 / 2 MiB。長く切れていたクライアントは、resume ではなくスナップショットで戻る（正しいが、そのぶん転送量が増える）
- t3 インポートとプロバイダーのログインは、リモートでもプロトコル上は通る。ただし UI のインポートボタンはローカル環境だけを対象にしている
- 状態表示の「retrying in Ns」は再試行までの秒数を数え下げない（イベントを受けた時点の値のまま）
- 実エージェントでは、どのプロバイダーもターンを走らせていない。Antigravity の実バイナリは `initialize` までの確認（§1、c02ff1f）

## 4. 計画からの変更

- `blongo-client` クレートと `Backend` トレイトをこのフェーズで入れた（Phase 1 の報告で、Phase 3 に回すと書いていたもの）。UI とバックエンドの境界の型（`CoreEvent` など）は `blongo-core` から `blongo-protocol::client` へ移し、`blongo-core` からは再エクスポートしている
- サーバーは `blongo` のサブコマンドではなく、別バイナリの `blongo-serve` にした。GPUI をリンクしないため。`blongo serve` はそれを exec する
- HTTP フレームワークは使わず、tokio-tungstenite の素の WebSocket だけにした
