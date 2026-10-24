# 給 AI agent 用的 Norn

AI coding agent（Copilot 的 agent 模式、Claude Code、Codex、OpenCode）靠文字搜尋找程式碼。在 C
裡，這會漏掉最重要的部分：巨集寫出來的呼叫、經由 struct 成員裡函式指標的呼叫，以及 build 實際編譯哪些
`#if` 分支。Norn 把它的索引當成工具交給 agent，並附上一份 skill，告訴 agent 先設定好設定組，再用這些工具
查詢，不要用 grep。
[English](agents.md)

- [安裝設定](#安裝設定)
- [agent 該怎麼用](#agent-該怎麼用)
- [工具一覽](#工具一覽)
- [設定組](#設定組)
- [怎麼讀回答](#怎麼讀回答)
- [運作方式](#運作方式)
- [疑難排解](#疑難排解)

## 安裝設定

| Agent | 要做什麼 |
|---|---|
| VS Code 裡 Copilot 的 agent 模式 | 不用做任何事：擴充套件會提供工具（需要 VS Code 1.101 以上）。要連 skill 一起裝，執行一次 **Norn: Connect AI Agents** |
| Claude Code（終端機、VS Code、桌面 app） | 在 VS Code 執行 **Norn: Connect AI Agents**，或在終端機執行 `norn setup` |
| Codex（命令列、VS Code、ChatGPT 桌面 app 的 Codex） | 同上 |
| OpenCode | 同上 |
| 其他支援 Model Context Protocol 的工具 | 指令設成 `norn mcp`（見[下方](#其他工具)） |

**Norn: Connect AI Agents**（VS Code 裡按 `Ctrl+Shift+P`）和 `norn setup` 做的是同一件事。VS Code 也會主動提示：
啟動一會兒後，如果發現電腦上有 Claude Code、Codex 或 OpenCode 但還沒接上 Norn，會詢問一次（**Connect** / **Not Now**），
之後不會再為同一個 agent 詢問。沒有 VS Code 時，一行指令就會從最新的
release 安裝 `norn`、比對 release 的 `SHA256SUMS`，再執行 `norn setup`：

```sh
# Linux、macOS
curl -fsSL https://norn-c.com/install.sh | sh
```

```powershell
# Windows（PowerShell）
irm https://norn-c.com/install.ps1 | iex
```

（`NORN_VERSION=0.26.0` 可指定其他版本；`NORN_NO_SETUP=1` 則不設定任何 agent；`NORN_DOWNLOAD_URL` 可改從 release 檔案的鏡像下載，而不是 GitHub。）也可以自己從
[releases](https://github.com/humphrey0925/norn/releases) 下載執行檔（見[命令列手冊](cli.zh-TW.md#安裝)），再執行 `norn setup`。

`[已驗證]` `install.sh` 在 Linux 上實測過：能下載並比對 checksum，checksum 不符時不會安裝。`[未測]` `install.ps1`：這裡沒有 PowerShell 可以測。

它會對這台電腦上找得到的每個 agent 做這些事：

1. 把 server 放到 `~/.norn/bin/norn`，這個路徑不會變動。VS Code 擴充套件每次啟動，都會把這份副本更新成擴充套件的版本。
2. 把 Norn 加進 agent 的 MCP server，以 `norn mcp` 啟動。只寫入 Norn 自己那一項，每個檔案的其他內容（包括註解）
   維持原樣：

   | Agent | 寫在哪裡 |
   |---|---|
   | Claude Code | 用 Claude Code 自己的指令 `claude mcp add --scope user` |
   | Codex | `~/.codex/config.toml` 的 `[mcp_servers.norn]` |
   | OpenCode | `~/.config/opencode/opencode.json` 的 `"mcp": { "norn": … }` |

3. 把名為 `norn` 的 skill 放到 agent 讀取 skill 的地方：`~/.claude/skills/norn/`（Claude Code）和
   `~/.agents/skills/norn/`（Codex、OpenCode、Copilot）。

已經在執行的 agent，重新啟動後才會有這些工具。確認方式：

| Agent | 怎麼確認 |
|---|---|
| Claude Code | `claude mcp list` 顯示 `norn … ✔ Connected`；`/skills` 列出 `norn` |
| Codex | `codex mcp list` 顯示 `norn` 為 enabled |
| OpenCode | `opencode mcp list` 顯示 `norn ✓ connected` |
| Copilot | 對話框的工具按鈕裡列出 Norn 的工具 |

全部移除：`norn setup --remove`。只想看會寫入什麼、不實際寫入：`norn setup --print`。

### 其他工具

支援 Model Context Protocol 的工具都可以自己啟動 server：指令是 `norn`，參數是 `mcp`。如果工具不是在專案資料夾裡啟動
server，也不會告訴 server 它開啟的資料夾（MCP `roots`），就在後面加上專案資料夾：`norn mcp /path/to/project`。
Skill 就是 [`skills/norn/SKILL.md`](../skills/norn/SKILL.md) 這個檔案；工具不支援 skill 的話，把它當成給 agent 的指示。

## agent 該怎麼用

以下是 skill 告訴 agent 的重點：

1. **先設定好情境。** 呼叫 `list_configurations`，看目前生效的是哪個設定組、還有哪些可選。任務講的是另一個
   板子、target 或 build 時，用 `switch_configuration` 切過去，並告訴使用者。設定組錯了，查到的結果看起來對，其實是錯的。
2. **再查詢，不要 grep。** 用 `search_symbols` 或 `find_definition` 找名稱；用 `read_symbol` 看程式碼，不要讀整個檔案；
   用 `find_callers`、`find_callees`、`call_tree`、`find_references` 看它怎麼被使用。
3. **讀檔和搜尋 C/C++ 也透過 Norn**：用 `read_file` 取代一般讀檔，用 `search_code` 取代 grep。一般讀檔時，被關掉的 `#if`
   分支看起來和其他程式碼一樣；這兩個工具會標出這段的起點和終點，或直接不列出。
4. **修改 `#if` 裡的程式碼之前**，先看這些標記或用 `compiled_lines` 確認。設定組沒有編譯的分支不要改，除非任務講的就是那個設定組。
5. **其他東西才用一般工具**：build 檔、其他語言、文件。

好用的問法：

- 「STM32F4 板子上誰呼叫 `uart_send`？用 Norn 查。」
- 「改 `flash_write` 的參數之前，列出所有呼叫它的地方，包括經由函式指標的。」
- 「`CMD_RESET` 訊息最後是哪個函式處理的？從 `dispatch` 往下追。」
- 「release build 會編譯 `spi.c` 裡 `#ifdef USE_DMA` 底下的程式碼嗎？」

## 工具一覽

| 工具 | 回答 |
|---|---|
| `list_configurations` | 有哪些設定組、目前生效的是哪個，以及它定義了什麼 |
| `switch_configuration` | 讓另一個設定組生效（這是專案的設定，和編輯器共用），並重建索引 |
| `search_symbols` | 名稱符合部分字串的符號：片段順序不拘、字首縮寫、依序出現的字母 |
| `find_definition` | 名稱在哪裡定義和宣告；給了使用該名稱的檔案和行號時，只回答那個位置看到的定義 |
| `read_symbol` | 一個定義的原始碼，連同上方的註解，附行號與標記 |
| `read_file` | 讀一個檔案或其中幾行，附行號，並在設定組不編譯的區段起點和終點各加一行標示；`hide_inactive` 會把不編譯的區段摺成那一行 |
| `search_code` | 像 grep 一樣搜尋 C/C++ 檔案的文字或 regular expression，但只看設定組會編譯的部分（`include_inactive` 才列出其餘的，並加標記）；每筆結果附上所在的函式 |
| `find_callers` | 一個函式的每個呼叫，包括經由巨集和函式指標的 |
| `find_callees` | 一個函式發出的每個呼叫，包括經由成員或變數呼叫時，那個成員或變數可能存放的函式 |
| `find_references` | 一個符號的每個使用處 |
| `call_tree` | 一個函式往下呼叫到哪些函式，或有哪些函式呼叫到它，幾層深 |
| `compiled_lines` | 一個檔案的哪些行在設定組下會編譯，或某一行是否會編譯 |

清單最多列出 100 筆，除非 agent 要求更多（`limit`），並會說明總共有幾筆。一個名稱有多個定義時，每個定義都會回答；
用 `file` 可以只選其中一個。路徑都相對於專案資料夾。

## 設定組

設定組就是 build 定義了什麼：哪個板子、晶片或 target，Debug 還是 Release。它決定哪些 `#if` 分支會編譯，
也就決定了工具看得到哪些定義、呼叫和程式行。Norn 會從專案自己的 build 檔找出設定組（Keil、IAR、Eclipse/DS-5、
Visual Studio、CMake、Makefile、Kconfig、PlatformIO、`compile_commands.json`），專案也可以在 `.norn/config.json`
定義自己的設定組（見[使用指南](norn-guide.zh-TW.md#10-編譯條件)）。

`switch_configuration` 改的是專案的設定，和 VS Code 的 **Norn: Switch Compile Configuration…** 一樣：從此編輯器和這個專案的
每個 agent 看到的都是同一個設定組。Skill 會要求 agent 說明它選了哪個、為什麼。

沒有設定組時，沒有被設定的分支兩邊都會建索引，裡面的呼叫會標成 `[maybe not compiled: …]`。例如 `BOARD_A`
只有其中一個設定組有定義：

```c
int main(void)
{
#ifdef BOARD_A
    return uart_a();
#else
    return uart_b();
#endif
}
```

```
read_file a.c                （BoardA 生效中）
     5  #ifdef BOARD_A
     6      return uart_a();
     7  #else
        >>> not compiled under BoardA (line 8)
     8      return uart_b();
        <<< end of not compiled (line 8)
     9  #endif

search_code uart_b
a.c:2   int uart_b(void) { return 2; }  [in uart_b]
（另有 1 筆在 BoardA 不編譯的地方：用 include_inactive 才會列出）
```

```
find_callees main            （沒有生效的設定組）
  a.c:6  uart_a (function, a.c:1) [maybe not compiled: under the #if at line 5]
  a.c:8  uart_b (function, a.c:2) [maybe not compiled: under the #if at line 7]

switch_configuration BoardA
find_callees main
  a.c:6  uart_a (function, a.c:1)
```

## 怎麼讀回答

| 標記 | 意思 |
|---|---|
| `— via macro X` | 這個呼叫是使用處的巨集 `X` 寫出來的 |
| `— through member send, which can hold it` | 經由 struct 成員裡的函式指標呼叫；列出的函式是某處曾存進這個成員的 |
| `— through variable handler, which can hold it` | 同上，經由變數 |
| `— through a pointer that can hold it` | 經由區域指標變數 |
| `[uncertain]` | 可能指向多個定義中的任一個 |
| `[maybe not compiled: under the #if at line N]` | 目前的設定組決定不了那個 `#if` |
| `[in a file the build does not compile]` | build 沒有任何編譯單元會讀這個檔案 |
| `>>> not compiled under X (lines a–b)` … `<<< end of not compiled`（`read_file`、`read_symbol`） | 中間的程式碼存在，但目前的設定組不會編譯 |
| `>>> maybe not compiled: under the #if at line N` … `<<< end` | 中間的程式碼在目前設定組決定不了的 `#if` 裡 |
| 結果前的 `✗` / `?`（`search_code`） | 同上，針對單一行 |
| `Listed N of M` | 用 `limit` 要更多，或用 `file` 縮小範圍 |

## 運作方式

- **一個專案一份索引。** 一個專案的第一個 Norn（agent 的 `norn mcp` 或 VS Code 的語言伺服器）負責建索引，其他的直接用它
  回答，不再自己建。VS Code 的語言伺服器開啟資料夾時會接手專案，所以 agent 用的是編輯器那份索引。負責的程序結束時，
  另一個會從磁碟上的索引（`.norn/cache`）接手。誰負責哪個專案記在 `~/.norn/run`，其他程序只能在本機、用那裡記的密語連線。
- **專案是哪個資料夾。** 有指定就用指定的；否則用工具告知的已開啟資料夾；再否則用 server 啟動時所在的資料夾，但絕不用
  家目錄或檔案系統的根目錄（桌面 app 通常在這裡啟動 server）。
- **保持最新。** 跟編輯器一樣監看檔案：原始檔改了，下次回答前就重讀那個檔；build 設定改了，才重新檢查整棵樹。
- **不外傳。** 全部在你的電腦上執行。Server 只和啟動它的 agent，以及同一台電腦上的其他 Norn 程序溝通。

## 疑難排解

| 狀況 | 怎麼處理 |
|---|---|
| Agent 沒列出 Norn 的工具 | 執行 `norn setup` 後重新啟動 agent，再用該 agent 的 MCP 清單確認（見[上方](#安裝設定)） |
| 出現「Norn does not know which project to index」 | Agent 在專案外啟動了 server：在它的 MCP 設定裡加上資料夾（`norn mcp /path/to/project`） |
| 第一次回答很慢 | 正在建索引；之後的回答會立刻出來，重新啟動也會直接讀 `.norn/cache` |
| 結果和要查的板子或 build 對不上 | 生效的是另一個設定組：先 `list_configurations`，再 `switch_configuration` |
| Agent 還是用 grep 找呼叫者 | 確認它有這個 skill（Claude Code 用 `/skills`，或 `ls ~/.agents/skills ~/.claude/skills`），並要求它用 Norn |
| 同一個專案出現第二份索引 | Agent 的 `norn` 和編輯器的版本不同：開一次 VS Code（它會更新 `~/.norn/bin`），或用較新的 `norn` 再執行一次 `norn setup` |
| VS Code 或 ChatGPT 桌面 app 裡的 Codex 找不到專案 | 在 `~/.codex/config.toml` 的 `[mcp_servers.norn]` 裡，把專案資料夾加進 `args` |

### 驗證狀態

- `[已驗證]` Claude Code 2.1.309、Codex CLI 0.164.0、OpenCode 1.19.4：`norn setup` 寫入的設定能讓它們啟動 `norn mcp`、呼叫工具並收到回答，也都會載入
  skill。測試時模型是用本機替身代替的，所以只驗證了接線，沒有驗證真模型會不會主動選用工具。
- `[已驗證]` VS Code（Linux、Windows）：擴充套件提供的 MCP server 能回答 `initialize`、`tools/list`、`tools/call`。
- `[已驗證]` VSCodium 1.140（VS Code 的開源建置）：機器上有 Claude Code、Codex、OpenCode 時，啟動後不久會跳出提示；按 **Connect** 會寫好三個 agent 的設定和兩個 skill 資料夾。
- `[未測]` Copilot 是否會載入 `~/.agents/skills/norn/`：依 VS Code 文件應該會，但沒有實測。
- `[未測]` VS Code 的 Codex 擴充套件和 ChatGPT 桌面 app 的 Codex：不確定它們在哪個資料夾啟動 server。
