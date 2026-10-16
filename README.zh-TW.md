# Norn

[English](README.md) | **繁體中文**

**在 VS Code 裡，用 Source Insight 的方式瀏覽大型 C 專案。**
Norn 不需要編譯就能為整個原始碼樹建立索引：不需要 compilation database，
也不需要任何設定。它能追蹤透過巨集和函式指標的呼叫，並判斷專案的 build
實際會編譯哪些 `#if` 分支。

![VS Code 裡的 Norn](vscode/media/screenshots/overview.png)

## 為什麼用 Norn

以編譯器為基礎的語言伺服器（clangd、IntelliSense）需要精確知道每個檔案
怎麼編譯。很多實際的 C 專案很難提供這些資訊：用專屬工具鏈建置的韌體、
每顆晶片一份標頭檔的 SDK、只能在某台機器上建置的程式碼。Norn 像人一樣
直接讀原始碼，有 build 描述檔時也會一併讀取。

- **快**：整個 Linux kernel 在一般桌機上不到一分鐘建好索引；存檔後幾十毫秒
  內更新；重新開啟工作區時直接用快取，立即可用。
- **準**：在公開專案上逐一呼叫和 clang 比對（Redis：找到 99.3% 的呼叫、
  精確度 99.8%；STM32F4 HAL：找到 96.5%、精確度 100%）。
- **只在本機執行**：不連網，不收集任何資料。

## 功能

- **Project Symbols**：模糊搜尋工作區裡的所有符號。
- **Relation**：呼叫者、被呼叫者、參考位置、函式指標被指派的位置，可以用樹狀
  或關係圖檢視；每個呼叫都會標出是直接呼叫、透過巨集、透過函式指標，
  還是在判斷不出來的 `#if` 裡。
- **Context**：在程式碼旁邊顯示游標所在名稱的定義。
- **File Symbols**：目前檔案的符號，函式底下列出參數和區域變數。
- **編譯條件**：從 Arm DS-5 的 `.cproject`、Keil、IAR、Visual Studio、
  `compile_commands.json`、PlatformIO、CMake、Makefile 讀取；build 不會編譯的
  程式碼會被排除並變暗。
- **完整的 C 語言伺服器**：移至定義、尋找參考、呼叫階層、顯示巨集值的懸停、
  自動完成、參數提示、語意上色、重新命名、參數名稱提示、摺疊、格式化。
  和微軟的 C/C++ 擴充套件一起使用時，Norn 會成為它的 configuration provider，
  不會重複提供同樣的功能。

詳細說明與截圖請見擴充套件頁面 [vscode/README.md](vscode/README.md)（英文），
使用指南請見 [docs/norn-guide.md](docs/norn-guide.md)。

## 安裝

從 Visual Studio Marketplace 或 Open VSX 安裝
**Norn — Code Navigation & Symbol Index**，或從
[releases](https://github.com/humphrey0925/norn/releases) 下載 `.vsix`。
擴充套件已內含 Windows、Linux、macOS（x64 與 Arm64）的伺服器。開啟一個有 C
原始碼的資料夾即可，不需要其他設定。

## 從原始碼建置

需要 Rust（stable）和 Node.js 20。

    cargo build --release                  # 伺服器：target/release/norn
    cd vscode && npm ci && npm run compile  # 擴充套件

`node release/package.js <平台> <伺服器>` 可以為單一平台打包，例如
`node release/package.js linux-x64 target/release/norn`。正式發佈由 GitHub
Actions 在 Windows、Linux、macOS 上建置與測試，流程見
[docs/releasing.md](docs/releasing.md)。

| 路徑 | |
|---|---|
| `crates/si-core` | 索引本體：解析（tree-sitter）、前置處理器條件、連結、儲存的圖 |
| `crates/si-cli` | `norn` 執行檔：`norn lsp`（語言伺服器）、`norn index`、`norn query` |
| `vscode/` | VS Code 擴充套件 |
| `bench/` | 和 clang 及其他索引工具比較準確度與速度的量測 |

## 參與貢獻

歡迎提出 issue 和 pull request，請見 [CONTRIBUTING.md](CONTRIBUTING.md)。
貢獻需同意[貢獻者授權協議（CLA）](CLA.md)：第一次送 pull request 時，CLA
檢查會請你簽署。

## 授權

Copyright 2026 humphrey0925 and the Norn contributors。以
[Apache License 2.0](LICENSE) 授權。
