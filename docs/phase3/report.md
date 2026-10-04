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
  - 再接続したクライアントは `Resume{epoch, last_seq, threads}` を送る。ただし、その epoch の `Shell` を適用し終えているときだけ（`Welcome` と `Shell` の間で切れたら、次は新規として接続する。サーバーも `last_seq == 0` の resume は受けない）。epoch が一致し、`last_seq` がリング内にあれば、それより後の分だけを再送する（resume）。そうでなければ新しいスナップショットを送る
  - クライアントは `seq ≤ last_seq` のメッセージを捨てる（スナップショットは除く）。また、スナップショットがまだ届いていないスレッドのタイムラインイベントは無視する。これで欠けも重複も起きない
- **デルタの合体と背圧**: 接続ごとの `Outbox` に上限がある（既定 4 MiB / 8192 件）
  - 同じ item への連続した TextDelta は1つにまとめ、最後の seq を残す
  - 上限に数えるのは状態を運ぶ Seq だけ。溢れるのも Seq を積んだときだけで、そのときは Seq の溜まり分を捨て（制御メッセージとターミナルは残す）、`Resnapshot` を送ってスナップショットを取り直させる。Pong、ターミナルの出力や失敗の通知を積んでも、黙って状態が落ちることはない（レビュー対応。待っている Pong は新しいもので置き換える）
  - ターミナルの出力は Outbox が半分埋まると読み取りを待つ（背圧）。ターミナルへの入力は 256 チャンクの有界キュー。ハブへのメッセージも 1024 件の有界キュー
  - 60秒間に5回溢れたら、その接続を切る。書き込みのタイムアウトは30秒
  - WebSocket の書き込みバッファは 16 MiB + 64 KiB で頭打ちになる
- **コマンド**: `CommandEnvelope` の id で冪等になる。再接続後、答えをもらっていないコマンド（最大64件、120秒以内）をクライアントが送り直す。サーバー側では重複として扱われ、二重には書き込まれない。オフラインのときに送ったコマンドは、その場で `<name> is not connected` として拒否する
- **生存確認**: 20秒送信がないか、20秒なにも届かなければ Ping（タイマーは1本の `sleep_until`）。45秒なにも届かなければ切断扱いにする。サーバーは75秒なにも届かない接続を閉じる

### 認証

- **ペアリング**: サーバーで1回限りのコードを発行する（`blongo-serve pair`、または `--pair` で起動時に表示）
  - コードは10文字（`XXXXX-XXXXX`、紛らわしい文字を除いた30文字種）、既定の有効期間は10分。サーバーにはコードの SHA-256 と期限だけを保存する
  - 600秒間に10回失敗すると、未使用のコードを全部無効にする
- **長期資格情報**: ペアリングが成功すると、サーバーはデバイス ID と 256 ビットのトークンを発行する。サーバーが保存するのはトークンの SHA-256 とデバイスの公開鍵だけ
  - クライアントは `environments.json`（0600、ディレクトリは 0700、一時ファイル経由で原子的に置き換える）に、server_id / device_id / token / Ed25519 の秘密鍵を保存する
- **DPoP 型の所持証明**: 認証のたびに Ed25519 で署名する
  - 署名対象: `"blongo-proof-v1\0"`、目的（Token / Pair）、server_id、そのセッションの nonce、jti（16B）、公開鍵、iat、秘密（トークンまたはコード）の SHA-256
  - **秘密そのものは送らない**（レビュー対応）。Token の要求は device_id と証明だけ、Pair の要求はデバイス名・公開鍵・証明だけを運ぶ。サーバーは保存してある SHA-256 で署名対象を組み立てて検証する（ペアリングでは未使用のコードを順に試す）
  - 鮮度は接続ごとの nonce で保証する。iat は署名に含めるが、時計のずれで弾かないよう検査はしない。`verify` と `verify_strict` の両方を通し、jti のリプレイキャッシュ（600秒、16384件）で同じ証明の使い回しも拒否する
  - nonce は接続ごとに新しいので、盗聴した証明は別の接続では通らない
  - **中間者には効かない**: 証明は TLS のチャネルに結び付いていない。`--insecure-listen` の平文経路で中継（relay）できる攻撃者は、正規のクライアントの認証をそのまま通して、確立した接続を乗っ取れる。防げるのは盗聴した証明の再利用だけ
- **比較と失敗**: 秘密の比較は `subtle` による定数時間比較。拒否の理由はクライアントに返さず（`Unauthorized` のみ）、応答を 500 ms 遅らせる。サーバーのログには拒否の理由と相手のアドレスを出す。資格情報のファイルを読めないなどの保存側の失敗は `Unavailable`（再試行する）として返す
- **ペアリングコードの文字**: 乱数バイトを棄却法（rejection sampling）で30文字種に写し、偏りをなくした
- **秘密をログに出さない**: `Debug` 実装でトークンと鍵を伏せ字にする。サーバーのログに出るのはデバイス名と ID だけ。例外は、運用者が手で写すための起動時のペアリングコードの表示（e2e のログでは伏せ字にした）
- **ローカル認証**: Unix ソケット（`<data>/server/blongo.sock`、0600）と `--stdio` は、ファイル権限か SSH で認証済みとみなし、`Auth::Local` を受け付ける。TCP では `Local` を拒否する
- デバイスの一覧と取り消し: `blongo-serve devices` / `revoke ID_OR_NAME`
  - `revoke` は動いているサーバーの Unix ソケットに管理者として接続し（`ClientMsg::Revoke`、ローカル認証の接続だけが使える）、そのデバイスの**生きている接続とターミナルをその場で閉じる**。クライアントには `DeviceRevoked` が届き、再接続せずに `failed` になる。サーバーが動いていなければファイルを書き換えるだけ（`--no-socket` で動いているサーバーの接続は、再起動するまで残る）
  - クライアント側の `blongo env remove NAME` は、手元から資格情報を消すだけで、サーバー側では取り消さない（そう表示する）。取り消しはサーバーで `blongo-serve revoke` を使う
- **ペアリング済みのデバイスと未使用のペアリングコードは、サーバーの持ち主と同じ権限を持つ**。プロジェクトのフォルダでエージェントを走らせ、シェル（ターミナル）を開ける。コードを渡すことは、そのマシンのアカウントを渡すことに等しい
- **接続数の上限**: WebSocket（64）と Unix ソケット（16）は別々の枠。認証前の WebSocket 接続は全体で16、同じアドレスから4まで。`Origin` ヘッダーの付いた要求（ブラウザーのページ）は 403 で拒否する

### TLS とネットワークの方針

- **TLS は内蔵しない**。`wss://` の指定は、理由を書いたメッセージで拒否する
- 待ち受けアドレスの方針
  - 既定: loopback（`127.0.0.1:7878`）
  - `--tailscale` で自分の tailnet アドレスに bind する（`$BLONGO_TAILSCALE` か `tailscale ip -4`）。このときだけ Tailscale の範囲（100.64.0.0/10、fd7a:115c:a1e0::/48）を暗号化済みとみなす
  - `--listen` で手で指定した 100.64.0.0/10 のアドレスは、キャリアの CGNAT かもしれないので、ほかのアドレスと同じ扱い（レビュー対応）
  - それ以外は `--insecure-listen` を付けない限り拒否する
- Unix ソケットは umask 0177 の下で bind する（作った瞬間から 0600）。パスが sun_path（108バイト）に収まらなければ、理由を書いて起動を止める
- loopback 以外へ出るときは、Tailscale（WireGuard で暗号化）、SSH のトンネル、SSH の stdio、または前段の TLS 終端（リバースプロキシ）を使う前提。トークンと証明は平文の WebSocket でも盗聴から再利用できない（nonce に結び付いた証明が要る）。ただし、本文の秘匿と改ざん防止は経路側に任せる

### SSH と Tailscale

- 接続先（target）は3種類
  - `ws://host[:port][/path]`: 既定ポート 7878、パス `/ws`。Tailscale のアドレスもこれで指定する
  - `ssh://[user@]host[:p]?port=N`: `ssh -o BatchMode=yes -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o StreamLocalBindMask=0177 -L <dir>/t.sock:127.0.0.1:N -- host` でトンネルを張り、ローカル側は**自分専用の Unix ソケット**（0700 の一時ディレクトリ内）に WebSocket で接続する。TCP ポートを空けてから ssh に渡す間の取り合いがない。ssh のプロセスは接続と一緒に終わり、ディレクトリも消す
  - `ssh+stdio://[user@]host[?command=...]`: `ssh -T -- host blongo-serve --stdio` の標準入出力を、長さ前置きのフレームで使う
  - **引数の注入を防ぐ**: host の前に必ず `--` を置く。さらに user や host が `-` で始まるもの、空白・制御文字・余分な `@` を含むものは解析の段階で拒否する
- `--stdio` は、動いているサーバーがあればその Unix ソケットへ橋渡しする。なければ、その1接続だけのためにサーバーをプロセス内で動かす（ソケットは作らない。データディレクトリのロックがあるので、2本目の `--stdio` は1本目が終わるまで失敗する）。複数のクライアントで使うなら、`blongo-serve` をデーモンとして常駐させる
- ssh のプログラムは `$BLONGO_SSH` で差し替えられる（テストでは偽の ssh を使う）

### アプリ（UI）

- サイドバー: 環境がローカルだけのときは Phase 2 と同じ見た目で、Phase 2 の e2e の座標もそのまま通る。リモート環境があると、環境ごとに次のものが付く
  - ヘッダー: 状態の点、名前、「+ Project」
  - 状態の行: connected / connecting / offline, retrying in Ns (attempt n) / failed
  - 直近のエラー
- 「+ Environment」: `NAME TARGET [CODE]` と入力する（ウィンドウ内の入力なので、コードはプロセスの引数に出ない）。ペアリングはネットワーク用スレッドで行い、成功したら `environments.json` に保存して接続する。失敗したらフォームに理由を出す
- 再接続のバックオフ: 3s → 4s → 8s → 16s（以後 16s）。30秒健全に接続できていたら最初に戻す。認証の拒否、バージョン不一致、サーバー ID の不一致は恒久エラーとして `failed` にし、再試行しない
- リモートのスレッドでは、ヘッダーの場所の表示に環境名が付く（`devbox · /path`）。モデル一覧は環境ごとに持つ
- **リモートのターミナル**: サーバーの PTY を使う
  - サーバー側はスレッドの作業フォルダで `$SHELL` を起動する。出力は Outbox を通る（背圧あり）
  - クライアント側は、ローカルと同じ alacritty_terminal のエミュレータで出力を描く。入力は専用スレッドからバックエンドへ送る
  - パネルを閉じるか、接続が切れると終わる。サーバーは自分が起動したシェルの PID にだけ SIGHUP を送り、必要なら SIGKILL へ上げる
- ネットワーク用のランタイム（`blongo-net`、current_thread）は、リモート環境を使うときに初めて起動する。ローカルだけならスレッドは増えない（32本のまま）
- CLI
  - `blongo serve ...`: 同じディレクトリの `blongo-serve` を exec する。UI のプロセスはサーバーにならない
  - `blongo env add NAME TARGET`（コードは標準入力から読む。端末ならプロンプトを出す。引数で渡すと `ps` やシェルの履歴に残るため。後方互換で引数も受け付ける）/ `env list` / `env remove NAME`
  - `environments.json` の読み書きは `environments.lock` の排他ロックの下で行う。アプリと CLI が同時に書いても項目が消えない
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

`cargo test --workspace` は **206 本**（レビュー前 198 本、Phase 2: 136 本）。レビュー対応の後、3回続けて全部通したで、すべてオフラインで動く。実エージェントのターンは走らせていない。環境変数のトークンで何かを認証することもしていない。

| バイナリ | 本数 | Phase 3 で増えた主なもの |
|---|---:|---|
| blongo（アプリ） | 16 | |
| blongo-protocol 単体 | 15 | ワイヤの往復（全 item 種別と本文、スナップショット、デルタ、クライアントメッセージ）、ID が 16 バイトになること、**2万回の変異ファズ**（パニックしない）、上限超過の拒否、フレームの分割・再構成・上限、証明メッセージがすべての入力に結び付くこと、バージョン・機能の交渉、タイムラインの振り分け |
| blongo-client 単体 | 13 | バックオフの段と30秒でのリセット、environments.json（0600、Debug で伏せ字）、**StreamState**（再送の重複を捨てて順序を守る、新しい epoch で保持中のタイムラインを捨てる、他スレッドと Resnapshot、本文が往復する）、証明（目的・サーバー・nonce・秘密・時刻・鍵のどれが変わっても失敗）、0600 で原子的な書き込み、定数時間比較、target の解析 |
| blongo-server 単体 | 10 | ペアリング（1回限り、トークンが鍵に結び付く）、**期限切れのコードと古い証明**、総当たりでコードが全部無効になる、ID が変わらない、待ち受けの方針、Outbox（合体と最後の seq、溢れたら状態を捨てて制御は残す、drain と close） |
| **remote（e2e、新規）** | 16 | 下記 |
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
12. `revoking_a_device_ends_its_live_connection_and_terminals`（レビュー対応）: 接続中のデバイスを `blongo-serve revoke` で取り消す → クライアントは再試行せず `failed`、サーバーのシェルの PID が消える、同じ資格情報はもう通らない
13. `revoke_needs_the_local_socket`: ネットワーク越しの `Revoke` は拒否される
14. `browser_origins_are_refused_and_unauthenticated_connections_are_capped`: `Origin` 付きは 403、同じアドレスからの認証前接続は4本まで
15. `silent_connections_are_closed`: Ping を送らない接続はサーバーが閉じる
16. `a_link_lost_before_the_shell_starts_fresh`: `Welcome` の直後に切れたクライアントの `last_seq == 0` の resume は断られ、`Shell` が届く

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
- **TLS は内蔵しない**（§1）。loopback 以外で `--insecure-listen` を使うと、本文は平文のまま流れる。盗聴した証明は再利用できないが、盗み見は防げない。経路の途中で中継できる攻撃者（MITM）には、認証をそのまま通されて接続を乗っ取られる（証明はチャネルに結び付いていない）
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

## 5. レビュー対応（review fixes）

独立したセキュリティレビューの指摘（CHANGES REQUIRED）への対応。+1.1 MiB は既定のビルドに残す（コーディネーターの判断）。

### 必須（BLOCKING）

1. **SSH の引数注入**: `ssh` の host の前に必ず `--` を置く（トンネル・stdio とも）。user / host が `-` で始まるもの、空白・制御文字・余分な `@` を含むものは target の解析で拒否する（`target.rs` の単体テストと、偽 ssh の引数ログで検査）
2. **取り消しで生きている接続が切れない**: 接続ごとに device_id を持たせた。`blongo-serve revoke` は動いているサーバーの Unix ソケットへ管理メッセージを送り、ハブがそのデバイスの接続に `DeviceRevoked` を送って閉じる。接続が閉じるとそのターミナルも終わる。クライアントは恒久エラーにして再試行しない。`blongo env remove` はサーバー側を取り消さないことを、CLI の表示と本書に書いた（e2e テスト 12、13）
3. **状態以外のメッセージで状態が黙って落ちる**: Outbox の上限には Seq だけを数え、溢れるのも Seq を積んだときだけにした（Resnapshot が必ず続く）。Pong は合体する。ターミナルの出力を大量に挟んでも Seq が全部順に残る単体テストを追加
4. **Shell を受け取っていない epoch の resume**: クライアントは `have_shell`（`Shell` を適用したら立て、新規の `Welcome` と `Resnapshot` で下ろす）が立っているときだけ resume する。サーバーも `last_seq == 0` の resume を断る（単体テストと e2e テスト 16）
5. **データディレクトリのロックでテストが不安定**: std はファイルを O_CLOEXEC で開くので、子プロセスがロックを持ち続けることはない（コメントで明記）。直前に止めたコアがロックを放すのを待つため、`try_lock` を 2 秒まで 25 ms 間隔で再試行する。`core_phase2` の全体を25回、`codex_rollback_after_restart_reverts_on_resume` を単独で20回回して失敗なし。`cargo test --workspace` も3回続けて全部通った

### 任意（NON-BLOCKING）で対応したもの

- サーバーのアイドルタイムアウト（75秒。クライアントは送信か受信が20秒途絶えたら Ping）
- 接続の枠を TCP（64）と Unix ソケット（16）に分けた。認証前の接続は全体16、アドレスごとに4
- `Origin` ヘッダー付きの WebSocket 要求を 403 で拒否
- 認証失敗のログに相手のアドレスを出す
- ペアリングコードの文字を棄却法で偏りなく選ぶ
- ペアリングコードとトークンをワイヤに乗せない（SHA-256 を署名対象に入れた証明だけ）
- `--insecure-listen` での MITM について記述を直した（証明はチャネルに結び付いていない）
- 時計のずれ: iat の検査をやめ、鮮度は nonce に任せる（リプレイキャッシュは600秒）
- `AuthError::Storage` は `Unavailable`（再試行する）として返す
- 100.64.0.0/10 を暗号化済みとみなすのは `--tailscale` のときだけ
- SSH トンネルのローカル側を、0700 の一時ディレクトリ内の Unix ソケットにした（TCP ポートの取り合いをなくした）
- ハブへのチャネル（1024）、ターミナルごとの入力（256）を有界にした。状態以外の Outbox メッセージは上限に数えず、Pong を合体する（ターミナルの出力は背圧で抑える）
- ペアリング済みのデバイスとペアリングコードが持ち主と同じ権限を持つことを、本書と `blongo-serve pair` の表示に書いた
- `blongo env add` はコードを標準入力（端末ならプロンプト）から読む
- `--stdio` で動いているサーバーがないときの挙動と、複数クライアントなら `blongo-serve` を常駐させることを記述
- クライアントの監視ループ: 5秒刻みのティックをやめ、次の Ping か沈黙の期限のどちらか早い方への `sleep_until` 1本にした。Ping は最後に受信した時刻も見る
- ハブの `shell_requests` カウンターをやめ、`Shell` のスナップショットは待っている接続に直接送る
- `environments.json` の読み書きを `environments.lock` の排他ロックで囲んだ（アプリと CLI）
- Unix ソケットのパス長（sun_path）の検査と、bind の前後の umask 0177
- `THIRD_PARTY_NOTICES.md` に、Phase 3 で増えた推移的な依存（fiat-crypto、sha1、httparse、data-encoding、utf-8、rmp、ed25519、signature、der / spki / pkcs8 / const-oid / base64ct、socket2 など）とライセンスの表を足した

### 見送ったもの（ギャップ）

- **サーバーの PTY の読み取りは 100 ms の poll のまま**。閉じる指示（フラグ）を見るための待ちで、出力があればすぐ起きる。完全にイベント駆動にするには自己パイプか eventfd を足す必要があり、今回は見送った。Outbox が半分以上埋まっているときの待ち（`wait_room_blocking`）も 5 ms 間隔の確認のまま
- `--no-socket` で動いているサーバーは、`blongo-serve revoke` でファイルを書き換えても、生きている接続を閉じない（再起動が要る。CLI がそう表示する）。devices.json の更新時刻を見て検証し直す方式は入れていない

### 再検証

- `cargo test --workspace` 206 本を3回連続で通過。`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all --check`、`cargo deny check licenses`（licenses ok。スクラッチにインストールして後で消した）
- クライアントとハブの振る舞いが変わったので、`tools/e2e-gui-remote.sh` をリリースビルドでもう一度通した（場面1〜8すべて成功、resume は「resumed after seq 165, replaying 22」）。スクリーンショットとログ（ペアリングコードは伏せ字）を差し替えた
- メモリの計測はやり直していない。変わったのは接続処理と認証で、待機時の常駐量に関わるコードではないため
