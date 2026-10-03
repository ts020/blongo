# キーバインドとコマンド（Phase 4）

コード: `apps/blongo/src/keymap.rs`（コマンド一覧、when 句、ユーザーファイル）、`apps/blongo/src/palette.rs`。

すべての操作に ID があり（`thread.new`、`view.diff` など）、コマンドパレット（Ctrl+Shift+P）に同じ一覧が出ます。キーは「コマンド ID + when 句」を持つアクションに結びつき、キーが押されたときに when 句をシェルの状態で評価します。偽ならそのキーは次のバインドに回ります（GPUI の `propagate`）。

## 既定のキー

| キー | コマンド | when |
|---|---|---|
| Ctrl+Shift+P | palette.commands | |
| Ctrl+P | palette.files（ファイル検索） | threadOpen |
| Ctrl+N | thread.new | |
| Alt+F | thread.fork | threadOpen |
| Alt+Z | thread.undo | threadOpen |
| Ctrl+` | terminal.toggle | threadOpen |
| Alt+C / Alt+D / Alt+E | view.chat / view.diff / view.files | threadOpen |
| Alt+I | view.inbox | |
| Ctrl+, | view.settings | |
| Alt+1〜4 | provider.codex / claude-code / antigravity / acp | |
| Alt+M | model.next | threadOpen |

キーなしのコマンド: `thread.stop`、`theme.toggle`、`approval.toggle`、`git.refresh`、`keybindings.open`。

## ユーザーファイル

設定フォルダ（`$BLONGO_CONFIG_DIR`、既定は `~/.config/blongo`）の `keybindings.json`:

```json
[
  { "key": "ctrl-k", "command": "palette.commands" },
  { "key": "alt-g", "command": "view.diff", "when": "threadOpen && !busy" },
  { "key": "alt-d", "command": "-view.diff" },
  { "command": "-thread.fork" }
]
```

- `-コマンド` は既定のバインドを外します（`key` を書けばそのキーだけ、書かなければ全部）。
- ユーザーのバインドは既定より優先されます。
- キーの書き方は GPUI と同じ（`ctrl-`、`alt-`、`shift-`、`cmd-`、`secondary-` = Linux/Windows では Ctrl・macOS では Cmd。空白区切りで連続キー）。
- when 句: 名前を `!`、`&&`、`||`、括弧で組み合わせます。知らない名前は偽。名前: `threadOpen`、`busy`、`remote`、`terminalOpen`、`paletteOpen`、`view.chat`、`view.diff`、`view.files`、`view.inbox`、`view.settings`。
- 読めない行（知らないコマンド、壊れたキー、when 句の文法エラー）は飛ばし、理由を通知欄と設定画面の Keybindings に出します。ファイル全体が JSON として壊れていれば、既定だけで動きます。
- 設定画面の Keybindings → Reload で再読み込みします（再起動不要）。
