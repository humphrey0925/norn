# 發佈與上架流程

## 一次性設定（需要你本人操作）

這些步驟牽涉帳號和權杖，只能由 repo 擁有者完成。

### 1. Visual Studio Marketplace 發行者

1. 用 Microsoft 帳號登入 <https://marketplace.visualstudio.com/manage>，建立
   ID 為 `humphrey0925` 的 publisher（必須和 `vscode/package.json` 的
   `publisher` 相同）。
2. 到 <https://dev.azure.com> → User settings → Personal access tokens →
   New Token：
   - Organization：**All accessible organizations**
   - Scopes：Custom defined → **Marketplace → Manage**
   - 期限：最長一年，到期前要換新。
3. 在 GitHub repo → Settings → Secrets and variables → Actions，新增 secret
   `VSCE_PAT`，值為上一步的 token。

### 2. Open VSX（選用）

VSCodium、Cursor、Gitpod 等編輯器是從 Open VSX 安裝擴充套件，不是從微軟的
Marketplace。要支援它們：

1. 用 GitHub 帳號登入 <https://open-vsx.org>，簽署 Eclipse 的 Publisher
   Agreement。
2. 在 Settings → Access Tokens 產生 token，存成 GitHub secret `OVSX_PAT`。
3. 第一次發佈前執行一次 `npx ovsx create-namespace humphrey0925 -p <token>`。

沒有設 `OVSX_PAT` 時，發佈流程會跳過 Open VSX。

### 3. 上架前的人工核准

GitHub repo → Settings → Environments → New environment，名稱填
`marketplace`，勾選 **Required reviewers** 並加入自己。之後每次發佈都會停在
「上架」這一步，等你在 Actions 頁面按下核准才會真的上架。

### 4. 分支保護（建議）

Settings → Branches → `master`：要求 PR、要求 CI（`ubuntu-24.04`、
`windows-2025`）和 `CLA` 通過。CLA 機器人會把簽署紀錄寫進 `cla-signatures`
分支，這個分支不要加保護。

## 每次發佈

1. 更新版本號，三處要一致（CI 會檢查）：
   - `vscode/package.json` 的 `version`
   - `crates/si-cli/Cargo.toml` 和 `crates/si-core/Cargo.toml` 的 `version`
   - `CHANGELOG.md` 新增 `## <版本>` 段落（會成為 GitHub Release 的說明）
2. 提交後打 tag 並推上去：

       git tag v0.23.0
       git push origin master v0.23.0

3. `Release` workflow 會自動進行：
   1. 建置六個平台的伺服器：win32-x64、win32-arm64、linux-x64、linux-arm64、
      darwin-x64、darwin-arm64。Linux 版是靜態連結，任何發行版都能跑。
   2. 每個平台各打包一個 vsix，伺服器放在裡面。
   3. 在 Windows、Linux、macOS 上各自安裝 vsix，跑完整的 VS Code 測試。
   4. 建立 GitHub Release，附上所有 vsix、獨立的伺服器執行檔和 `SHA256SUMS`。
   5. 等你核准後，上架到 Marketplace 和 Open VSX。

   VS Code 會自動安裝符合使用者平台的那一份 vsix。

只想建置和測試、不上架時：Actions → Release → Run workflow，不勾選
`publish`。

## 本機打包

- 單一平台：`node release/package.js <平台> <伺服器執行檔> [輸出目錄]`，例如
  `node release/package.js linux-x64 target/x86_64-unknown-linux-musl/release/norn dist`。
- 舊的 Windows 打包腳本：`release/make.sh`，產生 `out/norn-<版本>-windows-x64.7z`（不提交進 repo，發佈檔一律放在 GitHub Release）。

## CI 用量

repo 是私人的時候，GitHub Actions 每月有免費分鐘數。macOS 的分鐘數按 10 倍計算、
Windows 按 2 倍計算，所以：

- 每次 push 和 PR 只跑 Linux 和 Windows；
- macOS 每週跑一次，也可以手動觸發；
- 發佈時一定會跑全部三個系統。

repo 改成公開後就沒有分鐘數限制，可以把 `ci.yml` 的 macOS 改成每次都跑。
