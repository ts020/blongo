# `blongo://` リンク（Phase 4）

コード: `apps/blongo/src/deeplink.rs`、`apps/blongo/src/main.rs`、`Shell::open_link`。

## リンクの形

| リンク | 動き |
|---|---|
| `blongo://thread/<uuid>` | ローカルのスレッドを開く（無ければ通知欄に「No thread … here」） |
| `blongo://project?path=/abs/path` | 既にあるプロジェクトなら、その最新のスレッドを選ぶだけ（スレッドは作らない）。無いフォルダなら「プロジェクトとして追加しますか」と確認を出し、ユーザーが「Add project」を押したときだけ追加する |
| `blongo://settings` | 設定画面を開く |
| `blongo://inbox` | レビュー受信箱を開く |

リンクは**画面の移動だけ**です。プロンプトを送る、承認する、コマンドを走らせる、スレッドを作る、といった操作はリンクからはできません（ブラウザのページから勝手に作業させられないため）。状態を変えるのは、無いフォルダをプロジェクトに追加する場合だけで、これはユーザーの確認を経ます。

リンクごとの見直し（レビュー対応）:

- `thread`: 選ぶだけ。無ければ通知欄に出すだけ
- `project`: 上のとおり。既知なら選ぶだけ、未知なら確認つき
- `settings`: 画面を開くだけ
- `inbox`: 画面を開くだけ。ただし開くと、設定済みのフォージ（GitHub／GitLab）へ受信箱の読み込み（GET）が走る。ユーザー自身が設定したトークンとサーバーへの読み取りだけで、書き込みはしないパスは絶対パスのみ、4096 バイトまで、NUL を含むものは拒否します。

## 受け渡し

OS は `blongo <url>` を起動します。

1. 新しいプロセスは `<data_dir>/app.sock`（Unix ソケット、0600、0700 のフォルダ内）につなぎ、URL を1行書いてすぐ終了します（e2e で約 10 ms）。
2. つながらなければ（動いているインスタンスがない）、そのまま普通に起動し、最初のスナップショットが届いてからリンクを開きます。
3. 動いているインスタンスはソケットで受けた行を UI スレッドに渡して開きます。クラッシュで残ったソケットは、起動時に「誰も応答しない」ことを確かめてから消します。終了時に削除します。

macOS では OS が `open_urls` イベントで渡してくるので、`App::on_open_urls` でも同じ経路に流しています。

## OS ごとの登録

### Linux（実装済み）

```
blongo register-url-handler
```

`~/.local/share/applications/blongo-url-handler.desktop` に次のエントリを書き、`xdg-mime default blongo-url-handler.desktop x-scheme-handler/blongo` を実行します（`xdg-mime` が無ければ、そのコマンドを案内します）。

```
[Desktop Entry]
Type=Application
Name=Blongo
Exec="/path/to/blongo" %u
Terminal=false
NoDisplay=true
MimeType=x-scheme-handler/blongo;
```

`Exec` のパスは desktop entry の規則どおりに書きます。引用符の中の `"`、`` ` ``、`$`、`\` にはバックスラッシュを付け、`%` は `%%` にし、そのうえで値全体の `\` を `\\` にします。制御文字を含むパスは登録しません。

### macOS（未実装・手順のみ）

アプリバンドルの `Info.plist` に次を入れます。リンクは GPUI の `on_open_urls` に届き、上の経路で開かれます。バンドルを作る仕組み（署名・公証を含む）は Phase 4 では作っていません。

```xml
<key>CFBundleURLTypes</key>
<array>
  <dict>
    <key>CFBundleURLName</key><string>Blongo link</string>
    <key>CFBundleURLSchemes</key><array><string>blongo</string></array>
  </dict>
</array>
```

### Windows（未実装・手順のみ）

インストーラでレジストリに登録します。

```
HKEY_CURRENT_USER\Software\Classes\blongo
    (既定) = "URL:Blongo link"
    URL Protocol = ""
HKEY_CURRENT_USER\Software\Classes\blongo\shell\open\command
    (既定) = "C:\path\to\blongo.exe" "%1"
```

Windows では `app.sock` の受け渡し（Unix ソケット）が無いので、2つ目のプロセスはそのまま2つ目のウィンドウとして起動してしまいます。名前付きパイプでの受け渡しが必要です（未実装）。
