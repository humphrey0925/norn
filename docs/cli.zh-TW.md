# `norn` 命令列手冊

`norn` 是 VS Code 擴充套件在背後執行的索引程式，也可以單獨使用：不用編譯就能替
C 原始碼建索引，查詢符號和呼叫關係，提供給腳本、編輯器和 AI agent 使用，也能把找到的全部列出來。
[English](cli.md)

- [安裝](#安裝)
- [五個指令上手](#五個指令上手)
- [指令](#指令)：[`index`](#index)、[`query`](#query)、[`serve`](#serve)、
  [`live`](#live)、[`lsp`](#lsp)、[`mcp`](#mcp)、[`setup`](#setup)、[`symbols`](#symbols)、[`calls`](#calls)、
  [`sample`](#sample)、[`bench-query`](#bench-query)、[`bench-incr`](#bench-incr)
- [結果的格式](#結果的格式)
- [專案設定](#專案設定)
- [結束碼與錯誤](#結束碼與錯誤)

## 安裝

每個 [Release](https://github.com/humphrey0925/norn/releases) 都附有各平台的執行檔，
可用 `SHA256SUMS` 驗證：

| 平台 | 檔案 |
|---|---|
| Windows x64 / Arm64 | `norn-win32-x64.exe`、`norn-win32-arm64.exe` |
| Linux x64 / Arm64（靜態連結，任何發行版） | `norn-linux-x64`、`norn-linux-arm64` |
| macOS Intel / Apple 晶片 | `norn-darwin-x64`、`norn-darwin-arm64` |

每個執行檔旁邊都有一份 `THIRD_PARTY_NOTICES-<平台>.txt`，列出它內含的他人軟體及其授權；轉發執行檔時請一起附上。

改名為 `norn`（Windows 為 `norn.exe`），放到 `PATH` 裡；Linux 和 macOS 要加上執行權限
（`chmod +x norn`）。macOS 的執行檔沒有簽章，第一次執行時到「系統設定 → 隱私權與安全性」
允許，或執行 `xattr -d com.apple.quarantine norn`。

也可以自己建置：在 checkout 裡執行 `cargo build --release`，得到
`target/release/norn`（見 [Building from source](../README.md#building-from-source)）。

`norn --version` 顯示目前的版本。

## 五個指令上手

```sh
norn index ~/src/myproject --out ~/idx/myproject     # 建索引
norn query ~/idx/myproject find uart_send            # 定義和宣告在哪裡
norn query ~/idx/myproject callers uart_send         # 誰呼叫它
norn query ~/idx/myproject search "uart snd"         # 用片段找名稱
norn live ~/src/myproject                            # 隨原始碼更新的查詢伺服器
```

不需要先設定：`norn` 會從原始碼樹裡的 build 檔案判斷哪些 `#if` 分支會被編譯
（見[專案設定](#專案設定)）。

## 指令

每個指令都接受的選項：

| 選項 | |
|---|---|
| `--threads <N>` | 工作執行緒數；`0`（預設）用全部核心 |
| `--system-parser-alloc` | 讓解析器用系統的記憶體配置器，而不是 mimalloc（量測用） |
| `--no-build-config` | `.norn/config.json` 沒有指定設定時，不套用任何編譯條件，而不是自動採用專案裡找到的第一組建置設定 |
| `-h`、`--help` | 這個指令的選項 |
| `-V`、`--version` | 版本 |

### `index`

```
norn index <ROOT> [--out <DIR>] [--stores <LIST>] [--include-alias <FROM=TO>]... [--stride <K>] [--dump-binds]
```

從零替 `ROOT` 建索引，寫到 `--out`（預設 `out`）的 `graph.si`。完成後印出一個 JSON 物件：
檔案數、行數、符號數、邊數、已知的條件數、找到和找不到的 `#include`、每個步驟的時間（`ms`）、
記憶體（`mem`）。

| 選項 | |
|---|---|
| `--out <DIR>` | 索引寫到哪裡（預設 `out`） |
| `--stores <LIST>` | 逗號分隔：`csr`（預設，其他指令讀的就是這個）、`csr-packed`、`sqlite-naive`、`sqlite-tuned`，或 `none`（建索引但不寫檔）。其他幾種是為了比較儲存方式而保留的。 |
| `--include-alias <FROM=TO>` | `#include` 名為 `FROM` 的檔案時改讀 `TO`，用於 build 從範本產生的標頭檔。可重複。 |
| `--stride <K>` | 每 K 個檔案只索引一個（量測時間如何隨規模成長用） |
| `--dump-binds` | 另外寫出 `binds.tsv`：每個函式被存進哪個成員或變數 |

`index` 不保留也不沿用快取；要在多次執行之間保持索引最新，請用 [`live`](#live)。

### `query`

```
norn query <DIR> <OP> <NAME> [--depth <N>]
```

對 `DIR`（`index --out` 寫出的資料夾）裡的索引查詢一次，印出一個 JSON 物件。

| `OP` | 回答 |
|---|---|
| `find` | `NAME` 的每個定義和宣告 |
| `search` | 最多 50 個符合輸入的符號：任意順序的片段（`uart snd`）、字首縮寫（`us`）、依序出現的字母；最相符的在前，附 `score` |
| `callers` | 每個呼叫 `NAME` 的地方：呼叫它的函式和呼叫所在的行，包括透過巨集、以及透過存在成員或變數裡的函式指標的呼叫。最多列出 1,000,000 筆；`count` 是全部的數量，有省略時會帶 `"truncated": true` |
| `callees` | `NAME` 呼叫的每個函式，方式同上 |
| `refs` | `NAME` 的每個參考，呼叫和其他用法都算 |
| `tree` | 從 `NAME` 沿呼叫往下 `--depth` 層（預設 3）能到達幾個函式，包含它自己 |

有多個定義的名稱（例如每個設定各一個）會對每個定義都回答。

### `serve`

```
norn serve <DIR>
```

和 `query` 相同的查詢，對 `DIR` 裡的索引：stdin 每行一個 JSON 請求，stdout 每行一個 JSON
回答，直到 stdin 關閉。索引只載入一次，每個回答只要幾微秒。

```json
{"op": "callers", "name": "uart_send"}
{"op": "search", "name": "uart snd"}
{"op": "tree", "name": "main", "depth": 2}
{"op": "definition", "file": "src/main.c", "line": 3, "name": "uart_send"}
```

`callers` 也可以帶 `limit`，例如 `{"op": "callers", "name": "printk", "limit": 1000}`，
列出比上限 1,000,000 更少的筆數。

`definition` 回答那個檔案那一行寫的名稱指的是什麼（路徑相對於建索引的根目錄，第一行是 1），
和編輯器的「移至定義」一樣：有多個定義時，回答那個檔案看得到的那一個。

### `live`

```
norn live <ROOT> [--cache-dir <DIR> | --no-cache] [--rebuild] [--include-alias <FROM=TO>]...
```

直接對原始碼的 `serve`，並隨原始碼保持最新。就緒時先印出一行：

```json
{"ready": true, "files": 3, "symbols": 11, "ms": 2.97, "loaded": 0, "extracted": 3, "reused": 0, "link_ms": 0.0, "cache": "…/.norn/cache"}
```

（`loaded`：從快取讀回的檔案；`extracted`：重新讀過的檔案；`reused`：沿用自這個資料夾裡面或外面
另一份索引的檔案。）之後回答 `serve` 的所有請求，另外還有：

| 請求 | 作用 |
|---|---|
| `{"op": "update", "files": ["src/uart.c"]}` | 這些檔案改過、新增或刪除後，重新讀它們 |
| `{"op": "sync"}` | 檢查整個樹自上次以來改了什麼，重新讀那些 |
| `{"op": "scope", "include": ["src"], "exclude": ["src/tests"]}` | 設定專案包含哪些資料夾（存在 `.norn/config.json`），並讓索引跟上 |

每個請求都回答改了什麼（`changed`、`added`、`removed`、`extracted`）和花了多久。

索引在多次執行之間存在根目錄下的 `.norn/cache`，下一次 `live`（或編輯器）會從那裡開始。

| 選項 | |
|---|---|
| `--cache-dir <DIR>` | 改存在這裡 |
| `--no-cache` | 每次都從零建索引，什麼都不保留 |
| `--rebuild` | 丟掉保留的索引，從零建立，再保留新的 |

### `lsp`

```
norn lsp [ROOT] [--cache-dir <DIR> | --no-cache] [--rebuild] [--include-alias <FROM=TO>]...
```

透過 stdin/stdout 的語言伺服器（Language Server），給 VS Code 以外的編輯器用（Neovim、Emacs、
Helix、Sublime Text、Zed…）。根目錄是編輯器開啟的工作區，除非另外給 `ROOT`。它提供「移至定義」
（以及宣告、型別定義、實作）、「尋找所有參考」、呼叫階層、工作區和檔案符號、懸停說明、參數提示、
自動完成、游標所在名稱的醒目提示、`#include` 的連結、快速修正、摺疊、語意上色、重新命名和
inlay hint，並像 `live` 一樣保持索引最新。VS Code 擴充套件各個視窗所用的 `si/*` 自訂請求，說明在[語言伺服器協定](lsp-api.zh-TW.md)。

Neovim（0.11）：

```lua
vim.lsp.config('norn', { cmd = { 'norn', 'lsp' }, filetypes = { 'c' }, root_markers = { '.git', '.norn' } })
vim.lsp.enable('norn')
```

Helix（`languages.toml`）：

```toml
[language-server.norn]
command = "norn"
args = ["lsp"]

[[language]]
name = "c"
language-servers = ["norn"]
```

### `mcp`

```
norn mcp [ROOT] [--cache-dir <DIR> | --no-cache] [--rebuild] [--include-alias <FROM=TO>]...
```

怎麼設定 agent、agent 該怎麼用：見[給 AI agent 用的 Norn](agents.zh-TW.md)。

透過 stdin/stdout 的 [Model Context Protocol](https://modelcontextprotocol.io) 伺服器，給 AI agent
用：Claude Code、VS Code 的 Copilot、Cursor，以及其他支援 MCP 伺服器的工具。專案資料夾的決定順序：
有給 `ROOT` 就用它；否則用工具告知的已開啟資料夾（MCP roots）；再否則用目前所在的資料夾，但如果那是
家目錄或檔案系統的根目錄（桌面 app 通常在這裡啟動 server），就不建索引，每個工具都會說明怎麼指定專案。
它會立刻回應，同時在背景建索引；第一次呼叫工具時會等索引建好。

一個專案只有一份索引，不管有幾個 agent 在用。某個專案的第一個 `norn mcp` 負責建索引，之後為同一個
專案啟動的（另一個 agent、另一個工作階段）都直接用它的索引回答，不再自己建；它結束時，其中一個會接手，
從磁碟上保留的索引載入。VS Code 的語言伺服器一開啟該資料夾，就會接手這個專案，agent 改用編輯器的索引
回答。哪個程序負責哪個專案記在 `~/.norn/run`（一個鎖，以及負責者在哪裡回答：只在本機，而且只回答知道
那裡寫的密語的程序）。

它像 VS Code
擴充套件一樣監看檔案：agent 改了哪個原始檔，下次回答前就只重讀那個檔；建置設定（`.norn/config.json`、
Keil、IAR、Eclipse、Visual Studio 專案、`compile_commands.json`…）改了，或新增、刪除了資料夾，才重新
檢查整棵樹。沒有任何變動時，什麼都不讀。索引也像 `live` 一樣保留到下次執行。

工具以文字回答，讓 agent 閱讀，路徑都相對於根目錄：

| 工具 | 回答 |
|---|---|
| `search_symbols` | 名稱符合部分字串的符號：片段順序不拘、字首縮寫、依序出現的字母 |
| `find_definition` | 名稱在哪裡定義和宣告；給了使用該名稱的檔案和行號時，只回答那個位置看到的定義 |
| `read_symbol` | 一個定義的原始碼，連同上方的註解，附行號 |
| `find_callers` | 一個函式的每個呼叫，並標出哪些經由巨集、經由存放它的成員或變數、經由指標，以及哪些不確定 |
| `find_callees` | 一個函式發出的每個呼叫，包括經由成員或變數呼叫時，那個成員或變數可能存放的函式 |
| `find_references` | 一個符號的每個使用處 |
| `call_tree` | 一個函式往下呼叫到哪些函式，或有哪些函式呼叫到它，幾層深 |
| `compiled_lines` | 一個檔案的哪些行在建置設定下會編譯，或某一行是否會編譯 |
| `read_file` | 讀一個檔案或其中幾行，附行號，並在設定組不編譯的區段起點和終點各加一行標示 |
| `search_code` | 搜尋 C/C++ 檔案的文字或 regular expression，只看設定組會編譯的部分（`include_inactive` 才列出其餘的） |
| `list_configurations` | 有哪些設定組、目前生效的是哪個，以及它定義了什麼 |
| `switch_configuration` | 讓另一個設定組生效（這是專案的設定，和編輯器共用），並重建索引 |

清單最多列出 100 筆，除非 agent 要求更多（`limit`），並會說明總共有幾筆。一個名稱有多個定義時，
每個定義都會回答；用 `file` 可以只選其中一個。

在 VS Code 裡什麼都不用設定：擴充套件會把同樣的工具提供給 VS Code 管理 MCP 伺服器的 agent（Copilot
的 agent 模式），由它已經在執行的語言伺服器回答。所以 agent 用的就是編輯器手上的那份索引，跟編輯器一樣
保持最新，不會多建第二份索引（需要 VS Code 1.101 以上；`norn.mcp.enabled` 可以關掉）。

Claude Code、Codex、OpenCode：用 [`norn setup`](#setup) 一次設定好。其他工具：指令是 `norn`，參數是 `mcp`；
如果工具不是在專案資料夾裡啟動它，也不會告知開啟的資料夾，就再加上專案資料夾。

### `setup`

```
norn setup [claude | codex | opencode]... [--remove | --print | --refresh]
```

把 [`mcp`](#mcp) 的工具提供給這台電腦上的 agent，不必逐一手動設定：

1. 把這個程式放到 `~/.norn/bin/norn`（Windows 為 `norn.exe`），這是不會變動的路徑；那裡已經有同版或更新的版本時就保留它。
2. 把指定的 agent（沒指定就是這台電腦上找得到的每一個）設定成以 `norn mcp` 啟動它。專案就是 agent 當下工作的那個資料夾。
3. 把 skill [`norn`](../skills/norn/SKILL.md) 放到 agent 讀取 skill 的地方：`~/.claude/skills/norn/`（Claude Code）和 `~/.agents/skills/norn/`（Codex、OpenCode、VS Code 的 Copilot）。這份 skill 告訴 agent 先設定好設定組，再用這些工具，不要用 grep。使用者自己同名的 skill 不會被動到。

| Agent | 寫在哪裡 |
|---|---|
| Claude Code（終端機、VS Code、桌面 app） | 用 Claude Code 自己的指令 `claude mcp add --scope user`；`PATH` 上沒有 `claude` 時，照印出來的指令手動執行 |
| Codex（命令列、VS Code、ChatGPT 桌面 app 的 Codex） | `~/.codex/config.toml`（`$CODEX_HOME`）的 `[mcp_servers.norn]` |
| OpenCode | `~/.config/opencode/opencode.json` 的 `"mcp": { "norn": … }` |

只寫入 Norn 自己的那一項：每個檔案的其他內容（包括註解）都維持原樣。已經在執行的 agent，重新啟動後就有這些工具。

| 選項 | |
|---|---|
| `--remove` | 從每個 agent 的設定中移除 Norn，以及它的 skill |
| `--print` | 只印出每個 agent 的設定會加入什麼 |
| `--refresh` | 只在 `~/.norn/bin` 的副本和 skill 已經存在時，把它們更新到最新。VS Code 擴充套件每次啟動都會做，所以擴充套件更新後 agent 也會跟著更新 |

在 VS Code 裡：**Norn: Connect AI Agents** 做的是同一件事。Copilot 的 agent 模式不需要這些（見 [`mcp`](#mcp)）。

### `symbols`

```
norn symbols <DIR>
```

列出 `DIR` 索引裡的每個符號，每行一個，以 tab 分隔：`檔案  行  種類  decl|def  名稱`。

```
uart.c	3	function	def	uart_init
uart.h	1	function	decl	uart_init
```

### `calls`

```
norn calls <DIR>
```

列出 `DIR` 索引裡函式之間的每個呼叫，每行一個，以 tab 分隔：呼叫所在的檔案和行；呼叫者的名稱、
檔案、行；被呼叫者的名稱、檔案、行；`certain` 或 `ambiguous`；`def` 或 `decl`（被呼叫者的定義
是否在索引裡）；呼叫經過的巨集、成員或變數（沒有為 `-`，沒有其他名稱的指標為 `*`）；`direct`
或 `indirect`。

```
uart.c	4	uart_send	uart.c	4	uart_init	uart.c	3	certain	def	-	direct
```

`ambiguous` 表示索引無法確定的呼叫：位在設定判斷不了的 `#if` 裡，或呼叫的名稱有多個定義。

### `sample`

```
norn sample <DIR> [--n <N>] [--seed <S>]
```

從 `DIR` 的索引隨機抽出 `N`（預設 200）個函式定義的名稱，輸出成 JSON 陣列；同樣的 seed 抽出
同樣的名稱。量測時用來挑選要查詢的對象。

### `bench-query`

```
norn bench-query <DIR> [--n <N>] [--seed <S>]
```

對 `index` 在 `DIR` 寫出的每種儲存方式，在隨機抽出的 `N`（預設 2000）個函式上，量測行程內
`find`、`callers`、`callees`、`refs` 的時間：中位數、第 99 百分位和最差值，單位為奈秒。

### `bench-incr`

```
norn bench-incr <ROOT> [--n <N>] [--seed <S>]
```

替 `ROOT` 建索引，然後重複 `N` 次（預設 30）：隨機挑一個檔案重新讀取，再把整個索引重新連結，
就像編輯之後一樣，並回報每個部分花的時間。

## 結果的格式

結果裡的一個符號：

```json
{"name": "uart_send", "kind": "function", "file": "uart.c", "line": 4, "decl": false}
```

`kind` 是 `function`、`macro`、`struct`、`union`、`enum`、`enum_const`、`typedef`、`global`、
`field`、`file` 其中之一；`decl` 為 `true` 表示宣告（原型、`extern`），`false` 表示定義。
路徑相對於建索引的根目錄，分隔符號為 `/`。

`callers`、`callees`、`refs` 的每一筆另外有：

| 欄位 | |
|---|---|
| `at_line` | 呼叫或參考所在的行 |
| `certain` | 索引無法確定時為 `false`（判斷不了的 `#if`、有多個定義的名稱） |
| `via_macro` | 呼叫寫在哪個巨集裡 |
| `through` | 呼叫經過的函式指標存在哪個成員或變數 |

## 專案設定

`norn` 和擴充套件讀同一份設定：根目錄下的 `.norn/config.json`，包含專案涵蓋哪些資料夾，以及
編譯條件。沒有設定時，它依照原始碼樹自己的 build 檔案：Arm DS-5 / Eclipse CDT `.cproject`、
Keil、IAR、Visual Studio、`compile_commands.json`、`compile_flags.txt`、`platformio.ini`、
`c_cpp_properties.json`、Makefile 和 `CMakeLists.txt`，以及 Kconfig build 的
`include/generated/autoconf.h` 或 `.config`。可以寫哪些設定，見
[使用指南](norn-guide.zh-TW.md#10-編譯條件)和 [`cond.rs`](../crates/si-core/src/cond.rs)。

## 結束碼與錯誤

指令正常完成時結束碼為 0。給的資料夾裡沒有索引，或原始碼樹裡沒有 C/C++ 原始檔時，會在 stderr
說明並以 2 結束。`serve` 或 `live` 看不懂的請求會回答一個 `error` 欄位
（`{"error": "unknown op …"}`），伺服器繼續執行。
