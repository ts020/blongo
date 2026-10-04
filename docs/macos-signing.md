# macOS の署名と公証

リリースの `.dmg` に入る `Blongo.app` は、GitHub の `release` 環境に次の Secrets があると
Developer ID で署名され、Apple の公証（notarization）を通ります。そうすると、
ダウンロードしてそのまま警告なしで開けます。Secrets が無い間は ad-hoc 署名で、
初回起動時に「システム設定 → プライバシーとセキュリティ → このまま開く」が
必要です。

処理は `packaging/macos/bundle.sh`、呼び出しは `.github/workflows/release.yml`
の `macos-dmg` ジョブです。Secrets を読めるのはこのジョブだけで、タグのリリースの
ときにだけ動きます。

## 公開リポジトリでも安全な理由

- Secrets は暗号化して保存され、リポジトリからは読めず、ログでは伏せ字になります。
- フォークからの PR のワークフローには Secrets が渡りません。
- Secrets はリポジトリ全体ではなく `release` 環境に置き、その環境は**オーナーの
  承認がないと動かない**ようにします。ワークフローを書き換えられても、承認しない
  限り Secrets には届きません。
- 漏れた場合は、証明書を developer.apple.com で失効させ、App 用パスワードを
  account.apple.com で削除すれば無効になります。

## 必要なもの

Apple Developer Program（有料）のメンバーシップ。

## 手順

1. **Developer ID Application 証明書を作る**
   Mac の Xcode → Settings → Accounts → Apple ID を選ぶ → Manage Certificates
   → 左下の「+」→「Developer ID Application」。
   （Xcode が無い場合は developer.apple.com → Certificates → 「+」→
   Developer ID Application。CSR はキーチェーンアクセスの
   「証明書アシスタント → 認証局に証明書を要求」で作ります。）
2. **.p12 に書き出す**
   キーチェーンアクセス → ログイン → 自分の証明書 →
   「Developer ID Application: 名前 (チームID)」を右クリック →
   書き出す → `.p12`、パスワードを付ける。
3. **base64 にする**
   `base64 -i DeveloperID.p12 | pbcopy`
4. **App 用パスワードを作る**
   https://account.apple.com → サインインとセキュリティ → App 用パスワード。
5. **チーム ID を確認する**
   https://developer.apple.com/account → メンバーシップの詳細（10 文字）。
6. **`release` 環境を作る**
   リポジトリ → Settings → Environments → New environment → 名前 `release`。
   - Deployment protection rules: 「Required reviewers」にチェックを入れて自分を追加し、
     「Prevent self-review」は外したままにする（一人で運用するため）。
   - Deployment branches and tags: 「Selected branches and tags」→ Add rule →
     Tag、パターン `v*`。
7. **`release` 環境に Secrets を登録する**
   同じ画面の Environment secrets → Add environment secret
   （リポジトリ全体の Secrets には入れないでください）:

   | 名前 | 中身 |
   | --- | --- |
   | `MACOS_CERTIFICATE_P12_BASE64` | 手順 3 の文字列 |
   | `MACOS_CERTIFICATE_PASSWORD` | 手順 2 のパスワード |
   | `APPLE_ID` | Apple ID のメールアドレス |
   | `APPLE_TEAM_ID` | 手順 5 のチーム ID |
   | `APPLE_APP_PASSWORD` | 手順 4 の App 用パスワード |

## リリースのとき

タグを作ると、各 OS のビルドの後に `macos-dmg` ジョブが承認待ちになります。
Actions のその実行を開き「Review deployments」→ `release` にチェック →
「Approve and deploy」を押すと、署名・公証した `.dmg` がリリースに付きます
（GitHub のスマホアプリからも承認できます）。

次のタグから署名・公証されたアプリになります。証明書だけ入れて `APPLE_ID`
を入れないと、署名はされますが公証はされません（警告は残ります）。
