# Norn 語言伺服器協定

`norn lsp` 是一個語言伺服器（Language Server）：提供任何編輯器都能用的標準 LSP 請求，以及一組自己的
`si/*` 請求，VS Code 擴充套件的各個檢視就是建立在這些請求上。本頁說明這兩者，讓其他前端或工具也能使用。
如何啟動伺服器、它在多次執行之間保留什麼，見[命令列手冊](cli.zh-TW.md#lsp)；不透過 LSP 的查詢見
[`serve`](cli.zh-TW.md#serve) 和 [`live`](cli.zh-TW.md#live)。
[English](lsp-api.md)

- [啟動伺服器](#啟動伺服器)
- [慣例](#慣例)
- [標準 LSP 方法](#標準-lsp-方法)
- [`si/*` 請求](#si-請求)：[`si/search`](#sisearch)、
  [`si/context`](#sicontext)、[`si/relation`](#sirelation)、
  [`si/references`](#sireferences)、[`si/assignments`](#siassignments)、
  [`si/symbols`](#sisymbols)、[`si/place`](#siplace)、
  [`si/locals`](#silocals)、[`si/inactive`](#siinactive)、
  [`si/counterpart`](#sicounterpart)、[`si/conditions`](#siconditions)、
  [`si/setConditions`](#sisetconditions)、[`si/scope`](#siscope)、
  [`si/setScope`](#sisetscope)、[`si/sync`](#sisync)、
  [`si/status`](#sistatus)、[`si/mcp`](#simcp)
- [伺服器送出的通知](#伺服器送出的通知)
- [`.norn/config.json`](#nornconfigjson)
- [其他 VS Code 擴充套件可呼叫的指令](#其他-vs-code-擴充套件可呼叫的指令)

## 啟動伺服器

```
norn lsp [ROOT] [--cache-dir <DIR> | --no-cache] [--rebuild] [--include-alias <FROM=TO>]...
```

伺服器在 stdin 和 stdout 上使用 JSON-RPC 2.0，每則訊息依 LSP 的規定加上 `Content-Length` 標頭。
根目錄是有給的 `ROOT`，否則是 `initialize` 的 `rootUri`（或 `rootPath`），再否則是目前的資料夾。
不讀取 `workspaceFolders`。

`initialize` 會立即回答。之後索引在獨立的執行緒上讀入（或建立）；在索引就緒之前，請求會以上次工作階段
存下的內容回答，或回答空結果。索引就緒時，伺服器送出 [`si/ready`](#siready)。會改變索引的請求
（`si/setConditions`、`si/setScope`、`si/sync`）以及 `textDocument/didSave` 和
`workspace/didChangeWatchedFiles` 通知會等到那時，並依收到的順序處理。

伺服器本身不監看檔案。執行期間，檔案的變動要由 client 告知：編輯器裡存檔時送 `textDocument/didSave`，
其他方式造成的變更、新增或刪除（例如 `git pull`、建置）則送 `workspace/didChangeWatchedFiles`。
VS Code 擴充套件用自己的 file watcher 監看 C 和 C++ 原始檔，兩種通知都會送；其他 client 也應該送，
或在這類變動之後送 [`si/sync`](#sisync)。伺服器沒在執行時發生的變動，下次啟動時會自己找出來。

### `initializationOptions`

| 鍵 | 型別 | |
|---|---|---|
| `alongside` | boolean | 編輯器同時執行 Microsoft C/C++ 擴充套件。這時懸停說明只提供那個擴充套件沒有的資訊（巨集或列舉值的值），伺服器也不提供摺疊、語意上色（semantic tokens）、重新命名和 inlay hint。預設 `false`。 |
| `followBuildConfig` | boolean | `false`：`.norn/config.json` 裡沒有自己設定組的專案，以無編譯條件的方式索引，而不是依照樹裡找到的第一個建置組態。檔案裡寫明的設定組無論如何都有效。預設 `true`。 |

VS Code 擴充套件在已安裝 C/C++ 擴充套件、且其 IntelliSense 引擎不是 `disabled` 時，送出 `true` 的
`alongside`；`followBuildConfig` 則取自它的 `norn.conditions.followBuildConfig` 設定。
兩者都只在 `initialize` 時讀取：要改變它們，請重新啟動伺服器。
（`followBuildConfig: false` 就是命令列 `--no-build-config` 選項的效果。）

### 錯誤

| 代碼 | 時機 |
|---|---|
| `-32601` | 伺服器不回答這個方法（`"<method> is not supported"`） |
| `-32002` | 請求在 `initialize` 之前送到（`"not initialized"`） |

`initialize` 之前的通知，以及伺服器不認得的通知（例如 `initialized`），一律忽略。`si/setConditions`
和 `si/setScope` 存檔失敗時改在結果裡回報（見各該節）。

## 慣例

**路徑與 URI。** `uri` 欄位是 `file://` URI，除了字母、數字和 `/-_.~` 以外的每個位元組都做百分比編碼
（在 Windows 上：`file:///C%3A/src/proj/uart.c`）。`file` 欄位是相對於根目錄的路徑，分隔符號為 `/`。
根目錄以外的文件不會有任何回答。

**行號。** 會出現兩種行號，每個欄位都會註明是哪一種：

| 位置 | 起算 |
|---|---|
| LSP 的 `position` 和 `range`（參數與結果裡） | 0 |
| `line`、`endLine`、`functionLine`、`lines`、`onlyIf[].line`、`[first, last]` 數對 | 1 |
| [`si/references`](#sireferences) 的 `items[].line` | 0（它是 `range` 所在的行） |

**位置。** 針對游標所在名稱的請求，使用一般的 `TextDocumentPositionParams`：

```json
{"textDocument": {"uri": "file:///home/me/proj/src/uart.c"}, "position": {"line": 41, "character": 7}}
```

**被編譯條件關掉的行。** 位置所在的行在目前生效的編譯條件下不會被編譯時，那裡的內容都沒有索引，所以
伺服器不會去查那個名稱：

| 方法 | 回答 |
|---|---|
| `si/context`、`si/relation`、`si/references`、`si/assignments` | `{"inactive": [first, last]}`：位置周圍被關掉的行，從 1 起算 |
| `textDocument/references` | `[]` |
| `textDocument/definition`、`hover`、`documentHighlight`、`declaration`、`typeDefinition`、`implementation`、`prepareRename`、`rename`、`prepareCallHierarchy` | `null` |

**項目。** 大多數 `si/*` 結果都用同一種物件描述符號，下文稱為*項目*：

```json
{"name": "uart_send", "kind": "function", "file": "src/uart.c", "line": 42, "endLine": 57,
 "declaration": false, "uri": "file:///home/me/proj/src/uart.c",
 "range": {"start": {"line": 41, "character": 4}, "end": {"line": 41, "character": 13}},
 "data": {"file": "src/uart.c", "line": 42, "name": "uart_send"}}
```

| 欄位 | |
|---|---|
| `name` | 顯示用的名稱：C++ 成員帶有所屬類別（`DBImpl::Get`） |
| `kind` | `function`、`macro`、`struct`、`union`、`enum`、`enum_const`、`typedef`、`global`、`field`、`class`、`namespace` 或 `file`；區域變數或參數（見下文）為 `local` 或 `parameter` |
| `file`、`line`、`endLine` | 定義（或宣告）所在的位置，從 1 起算 |
| `declaration` | 只是宣告（原型、`extern`）時為 `true` |
| `uri`、`range` | 名稱本身在那裡寫出的位置。在那一行及其下兩行都找不到時，range 是該行開頭的空範圍。 |
| `data` | `file`、`line` 和單純的 `name`：[`si/relation`](#sirelation) 以 `item` 接收、用來再次找到這個符號 |

參數或區域變數不在索引的圖裡，而是由檔案本身的資訊回答。它的項目 `kind` 為 `parameter` 或 `local`，
`endLine` 等於 `line`，`declaration` 為 `false`，另外多兩個欄位：`function`（所屬函式的名稱）和
`type`（型別名稱，或 `null`）。

## 標準 LSP 方法

| 方法 | 回答 |
|---|---|
| `initialize`、`shutdown`、`exit` | `shutdown` 會把索引存起來供下次啟動使用（索引已載入完成時） |
| `textDocument/didOpen`、`didChange`、`didClose` | 完整同步（`change: 1`）。開啟中文件的文字用來讀取行內容；索引則依照已存檔的內容 |
| `textDocument/didSave` | 重新索引該檔案 |
| `workspace/didChangeWatchedFiles` | 重新索引每個改過、新增或刪除的檔案 |
| `textDocument/definition` | 游標所在名稱的定義，以某個編譯會先讀到的檔案裡的為準 |
| `textDocument/declaration` | 原型和其他宣告；沒有的話回答定義 |
| `textDocument/typeDefinition` | 變數、參數或成員：它的型別；函式：它的回傳型別；型別：它自己 |
| `textDocument/implementation` | 原型或函式：它的定義（virtual 方法則是每個 override）；存放函式的成員或變數：存進去的函式 |
| `textDocument/references` | 參考，依照 `context.includeDeclaration`；區域變數只在它的函式內 |
| `textDocument/documentHighlight` | 這個檔案裡該名稱指同一個東西的每處寫法（kind 1，text） |
| `textDocument/hover` | 巨集和列舉值附定義與值；宣告；區域變數與參數。有 `alongside` 時只有值 |
| `textDocument/signatureHelp` | 觸發字元 `(` `,`，重新觸發 `)` |
| `textDocument/completion` | 觸發字元 `.` `>` `"` `<` `/`（成員，以及 `#include` 裡的檔名）；沒有 resolve |
| `textDocument/documentSymbol` | 扁平的 `SymbolInformation[]`，`containerName` 為檔案 |
| `workspace/symbol` | 最多 200 個，比對方式同 [`si/search`](#sisearch) |
| `textDocument/prepareCallHierarchy`、`callHierarchy/incomingCalls`、`callHierarchy/outgoingCalls` | 包括透過巨集和函式指標的呼叫；`fromRanges` 是呼叫所在每一行開頭的空範圍 |
| `textDocument/documentLink` | 每個 `#include` 讀取的檔案 |
| `textDocument/codeAction` | `quickfix`：檔案用到某個名稱卻沒讀到它的宣告時，補上宣告它的標頭檔的 `#include` |
| `textDocument/foldingRange` | 大括號、`#if` 分支、超過一行的註解、連續的 `#include`（有 `alongside` 時不提供） |
| `textDocument/semanticTokens/full` | 有 `alongside` 時不提供。Legend：types 為 `namespace`、`type`、`class`、`enum`、`struct`、`parameter`、`variable`、`property`、`enumMember`、`function`、`macro`；modifiers 為 `declaration`、`static`、`readonly` |
| `textDocument/prepareRename`、`textDocument/rename` | 有 `alongside` 時不提供 |
| `textDocument/inlayHint` | 在呼叫的每個引數前顯示參數名稱（有 `alongside` 時不提供） |

位置清單最多 2000 筆。

## `si/*` 請求

### `si/search`

附分數的模糊符號搜尋：任意順序的片段、字首縮寫、依序出現的字母。`workspace/symbol` 的結果會被編輯器
依自己的規則再篩選一次，這個請求則回傳伺服器自己的排名。

```json
{"query": "uart snd", "limit": 200}
```

`limit` 預設 100。結果是[項目](#慣例)的陣列，最相符的在前，每個附 `score`（數字，越高越好）：

```json
[{"name": "uart_send", "kind": "function", "file": "src/uart.c", "line": 42, "endLine": 57, "declaration": false,
  "uri": "…", "range": {…}, "data": {…}, "score": 812}]
```

在目前生效的條件下沒有任何編譯會讀到的檔案，其中的符號不列入。

### `si/context`

游標所在的名稱定義成什麼：一個項目，給顯示定義的檢視使用。

參數：一個[位置](#慣例)。結果：一個[項目](#慣例)，附 `candidates`，即找到的定義數（項目是其中第一個）；
游標不在索引認得的東西上時為 `null`；或 `{"inactive": [first, last]}`。

```json
{"name": "uart_send", "kind": "function", "file": "src/uart.c", "line": 42, "endLine": 57, "declaration": false,
 "uri": "…", "range": {…}, "data": {…}, "candidates": 1}
```

定義優先於宣告：只有沒有定義時才回傳宣告。在區域變數或參數上，回傳該區域變數的項目，`candidates: 1`。

### `si/relation`

符號的呼叫者或被呼叫者，每一筆都說明呼叫是怎麼發生的。

```json
{"textDocument": {"uri": "…"}, "position": {"line": 41, "character": 7}, "direction": "callers"}
{"item": {"file": "src/uart.c", "line": 42, "name": "uart_send"}, "direction": "callees"}
```

`direction` 為 `"callees"` 表示被呼叫者；其他值（擴充套件送的是 `"callers"`）表示呼叫者。有 `item`
（某個項目的 `data`）時，以該符號為根，不使用位置：樹就是這樣一次展開一層的。沒有 `item` 時，根依序為：

1. 游標所在的函式或巨集（定義優先於原型）；
2. 游標所在的其他符號（變數、型別、成員或常數）：它的呼叫者是使用它的地方；它的被呼叫者，對存放函式的
   成員或變數而言，是存進去的函式；
3. 游標所在的函式。

結果：

```json
{"root": {"name": "uart_send", "kind": "function", …},
 "entries": [{"name": "log_flush", "kind": "function", "file": "src/log.c", "line": 88, …,
              "lines": [95, 102], "viaMacro": "LOG_SEND", "through": null, "pointer": false,
              "certain": true, "onlyIf": [{"line": 90, "text": "#ifdef CONFIG_UART"}]}]}
```

| 項目的欄位 | |
|---|---|
| `lines` | 呼叫所在的行，從 1 起算，在呼叫端函式的檔案裡 |
| `viaMacro` | 呼叫透過哪個巨集寫成，或 `null` |
| `through` | 呼叫經過的函式存放在哪個成員或變數，或 `null` |
| `pointer` | 透過沒有其他名稱的指標呼叫時為 `true` |
| `certain` | 索引無法確定這個呼叫時為 `false`（判斷不了的 `#if`、有多個定義的名稱） |
| `onlyIf` | `lines` 每一行都位在目前生效的條件判斷不了的分支裡時：決定它的那些 `#if`，格式為 `{line, text}`（行從 1 起算）；否則為 `[]` |

沒有根時：`{"root": null, "entries": []}`。在被關掉的行上：`{"inactive": [first, last]}`。

在區域變數或參數上（沒有 `item` 時），`root` 是該區域變數的項目；呼叫者的 `entries` 只有一筆，即它所屬
的函式，`lines` 是用到它的行，`viaMacro` 和 `through` 為 `null`，`pointer` 為 `false`，`certain` 為
`true`，沒有 `onlyIf`。被呼叫者的 `entries` 為空。

來自或經過沒有任何編譯會讀到的檔案的呼叫不列入。根是函式或巨集時，只會被函式、巨集和變數（初始值）
呼叫；其他符號則可能在任何地方被使用。

### `si/references`

游標所在名稱的參考，依檔案分組，每筆附所在行的文字，完全由索引回答：不需要在編輯器裡開啟任何檔案。

參數：一個[位置](#慣例)，可另加 `context.includeDeclaration`（預設 `true`），同 `textDocument/references`。

```json
{"groups": [{"file": "src/uart.c", "uri": "file:///home/me/proj/src/uart.c",
             "items": [{"line": 41, "character": 4, "text": "int uart_send(const char *s)"}]}],
 "total": 1}
```

`line` 和 `character` 是參考的起點，從 0 起算；`text` 是整行，去掉前後空白。每個檔案一組，檔案依路徑
排序，項目依行排序。或 `{"inactive": [first, last]}`。

### `si/assignments`

游標所在的成員或變數裡存進了哪些函式；或者，游標在函式上時，這個函式被存到哪裡：每一筆都是初始值或
指派所在的一行。

參數：一個[位置](#慣例)。

```json
{"target": {"name": "send", "kind": "field", …},
 "by": "member",
 "rows": [{"member": {"name": "send", "kind": "field", …},
           "function": {"name": "uart_send", "kind": "function", …},
           "file": "src/board.c", "line": 12, "uri": "…", "range": {…},
           "inside": "board_ops"}]}
```

| 欄位 | |
|---|---|
| `target` | 這些 rows 所描述的項目，或 `null` |
| `by` | `"member"`：rows 是存進 `target` 的東西。`"function"`：rows 是 `target` 被存到的地方。在沒有存任何東西的成員或變數上，伺服器改用 `"function"`。 |
| `rows[].member`、`rows[].function` | 成員或變數的項目，以及存進去的函式的項目 |
| `rows[].file`、`rows[].line` | 位置，從 1 起算 |
| `rows[].uri`、`rows[].range` | 函式名稱在那一行的位置 |
| `rows[].inside` | 那一行所在的符號名稱（函式、變數的初始值），或 `null` |

或 `{"inactive": [first, last]}`。

### `si/symbols`

專案所有符號中的一頁，給捲動時才逐步取得內容的清單使用。

```json
{"offset": 0, "limit": 200, "order": "name"}
```

| 參數 | |
|---|---|
| `offset` | 這一頁從哪裡開始（預設 0；超過結尾則回傳空頁） |
| `limit` | 幾筆（預設 200，最多 2000） |
| `order` | `"name"`（預設）：依名稱排序，不分大小寫。`"kind"`：函式定義，接著是原型、巨集、struct、union、enum、列舉值、typedef、變數定義、變數宣告、其他；各組內依名稱排序 |

```json
{"total": 48210, "offset": 0,
 "items": [{"name": "uart_init", "kind": "function", "declaration": false, "file": "src/uart.c", "line": 3}]}
```

`total` 是整份清單的長度。項目沒有 `uri` 也沒有欄位：開啟某一筆時，再向 [`si/place`](#siplace) 查詢。
檔案符號以及沒有任何編譯會讀到的檔案裡的符號不列入。在索引改變之前，順序維持不變。

### `si/place`

定義在某一行的符號，名稱確切寫在哪裡：[`si/symbols`](#sisymbols) 這類清單不帶欄位資訊。

```json
{"file": "src/uart.c", "line": 3, "name": "uart_init"}
```

`line` 從 1 起算。結果是 LSP 的 `Location`，位在那一行或其下兩行之一（GNU 風格的定義會把名稱寫在下一行）：

```json
{"uri": "file:///home/me/proj/src/uart.c", "range": {"start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 14}}}
```

### `si/locals`

一個檔案裡每個函式的參數與區域變數。

```json
{"textDocument": {"uri": "file:///home/me/proj/src/uart.c"}}
```

```json
[{"function": "uart_send", "functionLine": 42, "name": "s", "line": 42, "kind": "parameter",
  "type": "char", "uri": "…", "range": {…}}]
```

`functionLine` 和 `line` 從 1 起算；`kind` 為 `parameter` 或 `local`；`type` 是型別名稱或 `null`。
根目錄以外或沒有索引的檔案，回傳 `[]`。

### `si/inactive`

一個檔案裡，目前生效的條件下不會被編譯的行。

```json
{"textDocument": {"uri": "file:///home/me/proj/src/uart.c"}}
```

```json
[[10, 14], [30, 30]]
```

每個數對為 `[first, last]`，從 1 起算，頭尾都包含。

### `si/counterpart`

原始檔的標頭檔，或標頭檔的原始檔：同名（不分大小寫）、副檔名屬於另一類的已索引檔案，同一資料夾裡的
在前，其餘依距離由近到遠。

```json
{"textDocument": {"uri": "file:///home/me/proj/src/uart.c"}}
```

```json
["file:///home/me/proj/src/uart.h", "file:///home/me/proj/include/uart.h"]
```

標頭檔為 `.h`、`.hpp`、`.hh`、`.hxx`、`.inl`；原始檔為 `.c`、`.cpp`、`.cc`、`.cxx`、`.c++`。

### `si/conditions`

專案有哪些編譯條件設定組、哪一組生效、生效的那組逐個巨集展開後是什麼，以及在樹裡找到的建置組態。
參數：`{}`。

```json
{"configurations": {"Debug": {"defines": {"DEBUG": "1"}, "undefines": [], "assumeUndefined": [], "conditionFiles": []}},
 "active": "Debug",
 "automatic": false,
 "available": [{"name": "Release", "source": "fw/.cproject#Release"}],
 "effective": [{"name": "DEBUG", "value": "1", "source": ""}],
 "sourceDefines": true,
 "fromSources": [{"name": "HAVE_UART", "value": "1", "files": 120}],
 "fromSourcesTotal": 1,
 "files": 342,
 "includePaths": ["fw/inc"],
 "headerDirs": ["/home/me/proj/fw/inc"],
 "known": 2131}
```

| 欄位 | |
|---|---|
| `configurations` | 每個設定組，依名稱，格式同[設定組的鍵](#設定組的鍵)（空的或未設定的鍵省略，和檔案裡一樣） |
| `active` | 生效的設定組名稱，或 `null` |
| `automatic` | 設定組是樹自己的建置組態、因為專案沒有指定任何設定組而沿用時為 `true` |
| `available` | 在樹裡找到的建置組態，每個附名稱，以及要沿用它時 `conditionFiles` 該寫的值 |
| `effective` | 生效的設定組逐個巨集展開後的結果：`value` 為 `null` 表示未定義；`source` 是它來自哪個條件檔，`""` 表示來自設定組本身 |
| `sourceDefines` | 原始碼自己的定義是否跨檔案使用（只有設定組寫了 `"sourceDefines": false` 時為 `false`） |
| `fromSources`、`fromSourcesTotal` | 由原始碼確定值的巨集，每個附它在多少個檔案裡已知（最多列出 2000 個），以及總共有幾個 |
| `files` | 索引裡的檔案數 |
| `includePaths` | build 尋找標頭檔的位置，以條件檔中的 `.cproject` 所寫的為限 |
| `headerDirs` | 存放已索引標頭檔的資料夾，絕對路徑（最多 4000 個） |
| `known` | 條件確定了（定義或未定義）的巨集數 |

### `si/setConditions`

把每個設定組以及哪一組生效存進 `.norn/config.json`，並依新條件重新索引。參數是
[檔案中條件部分](#設定組)的全部內容：沒有列出的設定組會被移除。

```json
{"configurations": {"Debug": {"defines": {"DEBUG": "1"}}, "Release": {"conditionFiles": ["fw/.cproject#Release"]}},
 "activeConfiguration": "Debug"}
```

結果與 [`si/sync`](#sisync) 相同，另附 `conditions`，即新的 [`si/conditions`](#siconditions)：

```json
{"include": [], "exclude": [], "config": "/home/me/proj/.norn/config.json",
 "files": 342, "symbols": 9120, "added": 0, "removed": 0, "changed": 0, "parsed": 57, "ms": 410.2,
 "conditions": {…}}
```

無法寫入檔案時：`{"error": "could not save the conditions in <path>: <why>"}`。
參數無法解讀為上述物件時（例如某個欄位型別錯誤）：`{"error": "the conditions are not as they should be: <why>"}`，
且不會存檔。

注意：

- `"configurations": {}` 會存成空的設定組清單：專案從此沒有任何條件，也不會沿用樹裡的建置組態。
- `activeConfiguration` 不在 `configurations` 裡，或沒有給，則沒有任何設定組生效。
- 索引仍在載入時，請求會等待。

### `si/scope`

哪些資料夾被索引，以及這項設定寫在哪裡。參數：`{}`。

```json
{"include": ["src", "include"], "exclude": ["src/tests"], "config": "/home/me/proj/.norn/config.json"}
```

`config` 是設定檔的路徑，不論檔案是否已經存在。

### `si/setScope`

設定哪些資料夾要被索引，存檔，並讓索引跟上。

```json
{"include": ["src"], "exclude": ["src/tests"]}
```

缺少的清單視為空清單；`{"include": [], "exclude": []}` 表示整個樹。結果與 [`si/sync`](#sisync) 相同。
無法寫入檔案時：`{"error": "could not save <path>: <why>"}`。索引仍在載入時，請求會等待。

### `si/sync`

檢查整個樹自索引建立以來改了什麼（在編輯器外編輯、新增或刪除的檔案、手動修改的範圍設定檔），並重新讀取
那些內容。參數：`{}`。

```json
{"include": [], "exclude": [], "config": "/home/me/proj/.norn/config.json",
 "files": 342, "symbols": 9120, "added": 1, "removed": 0, "changed": 2, "parsed": 3, "ms": 35.0}
```

| 欄位 | |
|---|---|
| `include`、`exclude`、`config` | 同 [`si/scope`](#siscope) |
| `files`、`symbols` | 目前索引裡的數量 |
| `added`、`removed`、`changed` | 檔案數 |
| `parsed` | 讀取的檔案數 |
| `ms` | 讀取和連結花費的時間 |

索引仍在載入時，請求會等待。

### `si/status`

索引裡有什麼。參數：`{}`。

```json
{"files": 342, "symbols": 9120, "edges": 30211, "loaded": 340, "parsed": 2, "loading": false,
 "cache": "/home/me/proj/.norn/cache"}
```

| 欄位 | |
|---|---|
| `files`、`symbols`、`edges` | 索引裡的數量 |
| `loaded`、`parsed` | 啟動時：從上次執行的存檔讀回的檔案數，以及解析的檔案數（在 [`si/ready`](#siready) 之前兩者都是 0） |
| `loading` | 索引就緒前為 `true` |
| `cache` | 索引在多次執行之間存放的位置；使用 `--no-cache` 時為 `null` |

### `si/mcp`

一則 [Model Context Protocol](https://modelcontextprotocol.io) 訊息，用這份索引回答：回答內容與
`norn mcp` 相同（見[命令列手冊](cli.zh-TW.md#mcp)），給已經在執行這個伺服器的編輯器，用它把索引的工具
提供給 agent。參數：該則 JSON-RPC 訊息（`initialize`、`tools/list`、`tools/call`、`ping` 或通知）。
結果是對它的 JSON-RPC 回應；通知則為 `null`。

```json
{"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "find_callers", "arguments": {"name": "uart_send"}}}
```

索引還在載入時，`tools/call` 會等它載入完成，其他訊息則不等。

伺服器也會替 agent 的 `norn mcp` 負責它的專案（見[命令列手冊](cli.zh-TW.md#mcp)）：它們用這份索引、
以同樣方式回答，不再自己建索引；編輯器開啟資料夾之前已經負責該專案的 `norn mcp`，會把專案交給它。

## 伺服器送出的通知

伺服器不送出診斷，也不送出 refresh 請求。它會送出兩個自己的通知。

### `si/progress`

建索引的進度，執行期間最多每 150 ms 送一次，結束時再送一次 `done`。第一次建索引和之後的每次重新索引
（同步、新的索引範圍或新的條件）都會送出。

```json
{"phase": "parsing", "done": 1200, "total": 4800}
```

`phase` 為 `parsing`、`linking` 或 `done`。`done` 和 `total` 是已解析的檔案數與總數；連結期間維持在解析
結束時的值。

### `si/ready`

在 `initialize` 之後索引讀入或建立完成時送出一次。在那之前的所有請求都是以上次工作階段存下的內容回答
（或沒有回答），所以用戶端應該重新查詢它要顯示的內容。

```json
{"files": 342, "symbols": 9120, "edges": 30211, "loaded": 340, "parsed": 2}
```

`loaded`：從存檔讀回的檔案數；`parsed`：解析的檔案數。

## `.norn/config.json`

專案的設定，位於根目錄下。（舊版的 `.si` 資料夾在第一次被讀取時會改名為 `.norn`。）伺服器在
`si/setScope` 和 `si/setConditions` 時寫入這個檔案，各自只寫自己的鍵，檔案其餘部分保持原樣；
`norn live` 和 `norn index` 讀的也是同一個檔案。

```json
{
  "include": ["src", "include"],
  "exclude": ["src/tests"],
  "activeConfiguration": "Debug",
  "configurations": {
    "Debug": { "defines": { "DEBUG": "1", "CONFIG_SMP": "" }, "undefines": ["_WIN32"] },
    "Release": { "conditionFiles": ["fw/.cproject#Release"] }
  }
}
```

### 索引範圍

`include` 和 `exclude` 是相對於根目錄的資料夾，`.` 表示根目錄本身。對一個檔案而言，由包含它的最長
資料夾上的規則決定。上層都沒有規則時，`include` 為空則檔案在範圍內（整個樹就是專案），不為空則不在
（只有那些資料夾才是）。因此 `"exclude": ["."]` 會排除所有東西，只留下 `include` 加回來的部分。

### 設定組

`configurations` 把名稱對應到一組[設定組的鍵](#設定組的鍵)；`activeConfiguration` 指定生效的那一組。
檔案裡沒有 `configurations` 鍵時，會沿用樹自己的建置組態（先找 Kconfig build 的
`include/generated/autoconf.h` 或 `.config`，再找其他找得到的建置描述檔），除非 `followBuildConfig`
為 `false`。在還沒有設定組之前寫的檔案，設定直接放在最上層，會被讀成一個名為 `Default` 的設定組。

### 設定組的鍵

| 鍵 | 型別 | |
|---|---|---|
| `defines` | object | build 定義的名稱及其值（單純的 `#define` 為 `""`） |
| `undefines` | string[] | build 沒有定義的名稱 |
| `assumeUndefined` | string[] | 字首（`CONFIG_`），或 `*` 表示所有名稱：這類名稱除非有列出或在原始碼中定義，否則視為未定義 |
| `conditionFiles` | string[] | 要另外讀取的檔案：`#define`/`#undef` 行、Kconfig 的 `.config`；或在 `#` 後面接組態名稱的建置描述檔（`fw/.cproject#Release`、`.vscode/c_cpp_properties.json#Linux`） |
| `sourceDefines` | boolean | `false`：原始碼定義的巨集，在定義它的檔案以外不視為已知 |
| `undecided` | string | 沒有任何東西能確定的巨集上的分支：`"both"`（預設），兩個分支都索引，並標示為只是可能被編譯；`"off"`，把該巨集視為未定義 |
| `notBuilt` | string[] | build 不會編譯的資料夾：照樣索引和搜尋，但某個名稱或 `#include` 可能指那裡也可能指別處時，一律指別處 |
| `includeFirst` | string[] | 每次編譯都在自己的檔案之前先讀的標頭檔（build 的 `-include`） |
| `kbuild` | boolean | 由樹裡的 Kbuild makefile 決定這個組態會建置哪些檔案；它們沒列出的就視為不建置 |

每一項的作用在[使用指南](norn-guide.zh-TW.md#10-編譯條件)和
[`cond.rs`](../crates/si-core/src/cond.rs) 裡有更詳細的說明。

## 其他 VS Code 擴充套件可呼叫的指令

擴充套件註冊了下列會接收引數或回傳值的指令，供其他擴充套件和測試使用
（`vscode.commands.executeCommand(...)`）。只作用於使用者畫面的指令（切換檢視的模式、鎖定檢視）不列入。
沒有回傳值的指令，會在工作完成後才返回。

| 指令 | 引數 | 回傳 |
|---|---|---|
| `norn.open` | `{uri, line, character}`（line 從 0 起算）、`beside: boolean` | 在各檢視跟隨的編輯器群組中，或在它旁邊，開啟該位置 |
| `norn.showRelationAt` | `{textDocument: {uri}, position: {line, character}}` | 聚焦 Relation 檢視，在顯示該文件的可見編輯器中把游標移到那裡，解除檢視鎖定並顯示該位置。沒有編輯器顯示該文件時，不動游標 |
| `norn.relation.target` | 無 | Relation 檢視顯示的位置，格式為 `{textDocument, position}`，或 `undefined` |
| `norn.relation.referencesAt` | `{textDocument: {uri}, position}` | `{groups, message}`：[`si/references`](#sireferences) 的 groups（在被關掉的行上為空）以及檢視顯示的摘要 |
| `norn.relation.showReference` | `{uri, line, character}`（line 從 0 起算） | 在 Context 檢視中以顯示定義的方式顯示該位置，即使檢視已鎖定 |
| `norn.context.shown` | 無 | Context 檢視上次顯示內容的 `{uri, line, lines, colors}`（`line` 從 1 起算，`lines` 為檔案行數，`colors` 為佈景主題依 scope 的 token 顏色），或 `undefined` |
| `norn.fileSymbols.tree` | 無 | File Symbols 檢視列出的內容：`[{label, detail, children}]` |
| `norn.serverInUse` | 無 | 擴充套件啟動的伺服器路徑（或指令名稱） |
| `norn.search.ask` | `text?: string` | 聚焦 Search 檢視並搜尋 `text`。沒有給字串時詢問使用者，並預先帶入游標所在的字 |
| `norn.conditions.switch` | `arg?: string` | 字串是某個 `available` 建置組態的 `source`：在新的設定組中沿用它。是某個設定組的名稱：讓它生效。其他字串：建立同名的空設定組並讓它生效。沒有給字串時顯示 quick pick |
| `norn.conditions.delete` | `name: string` | 移除該設定組 |
| `norn.conditions.define` | `"NAME"` 或 `"NAME=value"` | 在生效的設定組中定義它（並從 `undefines` 移除）。沒有給字串時詢問使用者 |
| `norn.conditions.undefine` | `name: string` | 在生效的設定組中標為未定義。沒有給字串時詢問使用者 |
| `norn.conditions.addFile` | `path: string` | 在生效的設定組中加入一個條件檔。沒有給字串時開啟檔案對話框 |
| `norn.conditions.assume` | `prefix: string` | 加入 `assumeUndefined`。沒有給字串時詢問使用者 |
| `norn.conditions.get` | 無 | 生效的設定組，格式同[設定組的鍵](#設定組的鍵)（`defines`、`undefines`、`assumeUndefined` 和 `conditionFiles` 一定存在），附 `state`，即最近一次的 [`si/conditions`](#siconditions) |
| `norn.scope.include`、`norn.scope.exclude` | 一個資料夾：相對於根目錄的路徑，或 `vscode.Uri` | 包含或排除該資料夾（連同 Scope 檢視中尚未套用的變更）並重新索引 |
| `norn.scope.only` | 同上 | 只索引該資料夾 |
| `norn.scope.pending` | 無 | Scope 檢視有尚未套用的變更時為 `true` |

會改變條件的指令，會把最近一次讀到的每個設定組都送出，只修改生效的那一組；它們沒有動到的鍵（`notBuilt`、
`includeFirst` 等）照原樣送出。
