# デスクトップ通知（Phase 4）

コード: `apps/blongo/src/notify.rs`、`Shell::notify`。

## いつ出すか

- 実行（ターン）が終わったとき: 「Blongo: a run finished」/「Completed — スレッド名」（失敗・停止も同様）
- エージェントが承認を待ち始めたとき: 「Blongo: approval needed」/「スレッド名: 承認の内容」

設定画面の General → Notifications で選べます: Off / In background（既定: ウィンドウが前面にないときだけ）/ Always。前面かどうかは GPUI のウィンドウのアクティブ状態で判断します。

## 出し方

常駐するものは何もありません。通知のたびに小さなプロセスを1つ起動し、終了を短命のスレッドで回収します。

| OS | 方法 |
|---|---|
| Linux | `notify-send --app-name=Blongo -- TITLE BODY`（libnotify）。無ければ `gdbus call --session --dest=org.freedesktop.Notifications … Notify` で D-Bus の通知サービスを直接呼ぶ |
| macOS | `osascript -e 'display notification "BODY" with title "TITLE"'`（実装済み・未検証。AppleScript の文字列としてエスケープ） |
| Windows | 未実装 |
| どこでも | `BLONGO_NOTIFY_CMD` を設定すると、そのコマンドを `TITLE BODY` の2引数で起動（テストと e2e はこれで通知を記録） |

D-Bus のライブラリ（zbus など）を入れなかったのは、常駐スレッドとメモリを増やさないためです。通知は数分に1回程度なので、プロセス起動のコストは問題になりません。

## Windows でやるなら

トースト通知（`Windows.UI.Notifications`）は、アプリの AppUserModelID とスタートメニューのショートカットが必要です。インストーラでショートカットに AUMID を付け、`windows` クレートの `ToastNotificationManager` を使うのが筋です。インストーラが無い今の段階では入れていません。
