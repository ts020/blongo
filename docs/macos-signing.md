# macOS の署名と公証

リリースの `.dmg` に入る `Blongo.app` は、リポジトリに次の Secrets があると
Developer ID で署名され、Apple の公証（notarization）を通ります。そうすると、
ダウンロードしてそのまま警告なしで開けます。Secrets が無い間は ad-hoc 署名で、
初回起動時に「システム設定 → プライバシーとセキュリティ → このまま開く」が
必要です。

処理は `packaging/macos/bundle.sh`、呼び出しは `.github/workflows/release.yml`
の「Package (macOS app)」です。Secrets はタグの push のときだけ使います。

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
6. **GitHub に Secrets を登録する**
   リポジトリ → Settings → Secrets and variables → Actions → New repository secret:

   | 名前 | 中身 |
   | --- | --- |
   | `MACOS_CERTIFICATE_P12_BASE64` | 手順 3 の文字列 |
   | `MACOS_CERTIFICATE_PASSWORD` | 手順 2 のパスワード |
   | `APPLE_ID` | Apple ID のメールアドレス |
   | `APPLE_TEAM_ID` | 手順 5 のチーム ID |
   | `APPLE_APP_PASSWORD` | 手順 4 の App 用パスワード |

次のタグから署名・公証されたアプリになります。証明書だけ入れて `APPLE_ID`
を入れないと、署名はされますが公証はされません（警告は残ります）。
