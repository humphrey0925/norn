# Norn 安裝與使用指南

適用版本：Norn 0.21.0（VS Code 擴充套件，Windows x64）

Norn 讓你在大型 C 和 C++ 專案裡立刻查到每個函式和變數在哪定義、誰呼叫它、它又呼叫誰。它不需要先把專案編譯過，開啟資料夾就自動建立索引，連透過巨集和函式指標的呼叫也追得到。

![Norn 的整體畫面](images/overview.png)

## 目錄

1. [系統需求](#1-系統需求)
2. [安裝](#2-安裝)
3. [第一次使用](#3-第一次使用)
4. [畫面導覽](#4-畫面導覽)
5. [搜尋符號](#5-搜尋符號)
6. [本檔符號](#6-本檔符號)
7. [Context：看定義](#7-context看定義)
8. [Relation：看呼叫關係](#8-relation看呼叫關係)
9. [索引範圍](#9-索引範圍)
10. [編譯條件](#10-編譯條件)
11. [與 C/C++ 擴充套件（IntelliSense）連動](#11-與-cc-擴充套件intellisense連動)
12. [保持索引最新](#12-保持索引最新)
13. [設定項目](#13-設定項目)
14. [指令一覽](#14-指令一覽)
15. [常見問題](#15-常見問題)
16. [已知限制](#16-已知限制)

---

## 1. 系統需求

| 項目 | 需求 |
|---|---|
| 作業系統 | Windows 64 位元 |
| VS Code | 1.85 以上 |
| 支援的語言 | C（`.c`、`.h`）、C++（`.cpp`、`.cc`、`.cxx`、`.hpp`、`.hh` 等） |
| 記憶體 | 一般專案數百 MB 以內；Linux kernel 規模（6 萬個檔案）建立索引時約需 6 GB |
| 其他軟體 | 不需要。索引程式已內建在擴充套件裡，不必安裝編譯器或 clang |

Norn 不連網，也不會把任何資料傳出去。

## 2. 安裝

### 2.1 取得安裝檔

下載 `norn-0.20.0-windows-x64.7z` 並解壓縮，裡面有三個檔案：

| 檔案 | 用途 |
|---|---|
| `norn-0.20.0.vsix` | VS Code 擴充套件，安裝這個就夠了 |
| `norn-windows-x64.exe` | 同一個索引程式的命令列版本，給腳本或其他工具用；擴充套件不需要它 |
| `README.md` | 簡短說明 |

### 2.2 安裝擴充套件

擇一即可。

**從 VS Code 介面安裝**

1. 開啟 VS Code，按左側活動列的「擴充功能」（或 `Ctrl+Shift+X`）。
2. 按擴充功能面板右上角的「…」。
3. 選「從 VSIX 安裝…」（Install from VSIX…）。
4. 選擇 `norn-0.20.0.vsix`。

**從命令列安裝**

```
code --install-extension norn-0.20.0.vsix
```

### 2.3 確認安裝成功

在擴充功能面板搜尋 `norn`，點進去應該會看到下面的頁面，版本與你安裝的相同、來源是 VSIX（下圖拍攝時是 0.19.2）：

![安裝後的擴充套件頁面](images/install-extension-page.png)

### 2.4 從舊版 SI Code Index 升級

Norn 在 0.18.1 以前叫做 SI Code Index。因為名稱和識別碼都換了，VS Code 會把 Norn 當成另一個擴充套件：

1. **先移除舊的 SI Code Index**，再安裝 Norn。兩個同時裝著會各自建立一份索引；Norn 偵測到時會提醒一次。
2. 其餘自動處理，第一次開啟工作區時：
   - 工作區的 `.si` 資料夾會改名為 `.norn`（設定與索引快取都保留）。
   - 你原本寫的 `si.*` 設定值會抄到 `norn.*`。

## 3. 第一次使用

1. 用 VS Code 開啟含有 C 原始碼的**資料夾**（檔案 → 開啟資料夾）。
2. 開啟任何一個 `.c` 或 `.h` 檔，Norn 就會啟動。
3. 第一次會跳出詢問，二選一：
   - **Index Everything**：索引整個資料夾。
   - **Choose Folders First**：先在「Index Scope」勾選要索引的資料夾，再按套用。專案很大、只關心其中一部分時選這個。
4. 左下角狀態列顯示進度（`Norn parsing 420 / 726 files`），完成後變成檔案數與符號數：

![狀態列](images/status-bar.png)

你的選擇會存在工作區的 `.norn/config.json`，之後隨時可以在「Index Scope」修改。

索引會存在 `.norn/cache/`，下次開啟同一個資料夾時直接讀回，只重新處理有變動的檔案。參考時間：

| 專案 | 第一次索引 | 再次開啟到可以查詢 |
|---|---|---|
| Redis（39 萬行） | 約 0.3 秒 | 0.03 秒 |
| binutils（214 萬行） | 約 2.4 秒 | 0.06 秒 |
| Linux kernel（3,775 萬行） | 約 54 秒 | 約 3 秒 |

> 如果有安裝微軟的 C/C++ 擴充套件，它會另外問一次是否讓 Norn 設定 IntelliSense，建議按允許。詳見[第 11 節](#11-與-cc-擴充套件intellisense連動)。

## 4. 畫面導覽

![畫面各區域](images/overview-numbered.png)

| 編號 | 名稱 | 用途 |
|---|---|---|
| 1 | Norn 圖示 | 活動列上的入口，點它顯示左側的 Norn 面板 |
| 2 | Project Symbols | 搜尋整個專案的符號 |
| 3 | File Symbols | 目前這個檔案裡的符號，依種類分組 |
| 4 | Index Scope | 勾選哪些資料夾要索引 |
| 5 | Compile Conditions | 編譯條件（哪些巨集有定義），決定 `#if` 分支是否納入 |
| 6 | Relation | 游標所在符號的呼叫者、被呼叫者、參考 |
| 7 | Context | 游標所在符號的定義 |
| 8 | 狀態列 | 檔案數與符號數；點一下會和磁碟同步 |

Relation 和 Context 在下方面板的「Norn」分頁裡。**它們會跟著游標走**：游標移到哪個名稱上，兩個視窗就顯示那個名稱的資料，不需要另外按鍵。

每個檢視都可以拖到你習慣的位置。想恢復原狀時，執行指令「Norn: Reset View Layout」。

## 5. 搜尋符號

在 **Project Symbols** 的輸入框打字，結果即時更新。

![搜尋符號](images/search.png)

三種輸入方式都可以，而且不分大小寫：

| 輸入 | 找得到 | 說明 |
|---|---|---|
| `cache init` | `bfd_cache_init`、`init_section_cache` | 多個片段，順序不拘 |
| `bci` | `bfd_cache_init` | 各段的字首 |
| `bfdcinit` | `bfd_cache_init` | 依序出現的字母 |

操作：

- **點一下**結果：在編輯器開啟定義。
- **Ctrl + 點一下**：在旁邊的編輯器群組開啟，原本的檔案不動。
- **Enter**：開啟第一筆結果。
- 右上的 **Name / Kind**：依名稱排序，或把函式、巨集、型別、變數分開排。
- 輸入框清空時，列出整個專案的所有符號，可以直接捲動瀏覽。

也可以執行指令「**Norn: Search Symbols…**」，它會預先帶入游標所在的字。

## 6. 本檔符號

**File Symbols** 列出目前檔案裡的巨集、函式、型別、變數，每一項後面是行號。點一下就跳過去。

![本檔符號](images/file-symbols.png)

展開一個函式可以看到它的**參數與區域變數**。不想列出區域變數的話，把設定 `norn.fileSymbols.showLocals` 關掉。

## 7. Context：看定義

把游標放在任何一個名稱上，**Context** 就顯示它的定義，你原本看的檔案不會跳走。

- 顯示的是**定義所在的整個檔案**，停在定義那一行；可以上下捲動閱讀，但不能編輯。
- 標題列會寫出這是什麼（`function`、`macro`、`member`、`local variable`…）以及位置。
- 在 Context 裡**Ctrl + 點一下**某個名稱，會繼續跳到那個名稱的定義；滑鼠停在名稱上會顯示說明。
- 右上角的**鎖頭**：鎖住後 Context 不再跟著游標變動，適合對照著看。再按一次解鎖。

下圖右半邊是 Context：游標在 `abfd->iovec->bread` 的 `bread` 上，Context 顯示這個結構成員的宣告。

![Relation 與 Context](images/relation-calls.png)

## 8. Relation：看呼叫關係

**Relation** 上方有四個分頁，游標所在的符號是查詢對象。

### 8.1 Calls：它呼叫了誰

列出這個函式裡呼叫的所有函式，可以逐層展開。

對**函式指標**也有效。上一張圖裡游標在 `abfd->iovec->bread (...)`，這是透過結構成員的間接呼叫；Relation 顯示「bread holds 4」，列出程式裡實際被存進這個成員的四個函式。

### 8.2 Called by：誰呼叫了它

![Called by](images/relation-called-by.png)

每一列是一個呼叫者，後面的數字是發生呼叫的行號，點行號直接跳到那一行。展開一個呼叫者可以繼續往上追它的呼叫者。

每個呼叫都會標示是怎麼來的：直接寫名字、經由巨集展開、經由函式指標，或是不確定（例如位在沒有設定編譯條件的 `#if` 分支裡）。

**不只函式能查。** 變數、巨集、結構成員、列舉值、區域變數都可以，這時列出的是使用它的地方：

![區域變數的使用位置](images/relation-local.png)

### 8.3 References：所有出現的地方

![References](images/relation-references.png)

依檔案分組，列出每一處出現的程式碼。上圖是列舉值 `bfd_error_system_call` 在 38 個檔案裡的 90 處參考。清單直接取自 Norn 的索引，不需要另外掃描檔案。

**單擊**一筆參考，Context 視窗會顯示它前後的程式碼，編輯器停在原處，方便一筆一筆瀏覽；**雙擊**或按 **Enter** 才會在編輯器開啟，**Ctrl＋點擊**則在旁邊的編輯器群組開啟。

### 8.4 Assignments：函式指標被指派的地方

![Assignments](images/relation-assignments.png)

游標在一個會存放函式的結構成員或變數上時，這裡列出**哪些函式在哪裡被存進去**。反過來把游標放在函式上，會列出它被存進了哪些成員或變數。

### 8.5 樹狀與關係圖

右上角的 **Tree / Graph** 切換顯示方式。

![關係圖](images/relation-graph.png)

關係圖裡：

- 節點旁的 **＋ / −** 展開或收合下一層。
- 連線下方的數字是行號，點了會跳過去。
- **虛線**是透過函式指標的呼叫。
- 圖比視窗大時用捲軸移動。

### 8.6 鎖定與同步

- **鎖頭**：鎖住後 Relation 停在目前的符號，游標移動不會改變它。
- **同步（靶心圖示）**：在鎖定狀態下，把 Relation 更新成游標現在所在的符號，但維持鎖定。適合「先鎖住看一個函式，偶爾換一個」的用法。

### 8.7 函式上方的摘要

每個函式定義的上方有一行灰色小字，寫出呼叫者與被呼叫者的數量，點它會在 Relation 開啟對應的清單。

![函式上方的摘要](images/codelens.png)

不需要的話把設定 `norn.codeLens.enabled` 關掉。

## 9. 索引範圍

**Index Scope** 列出工作區的資料夾，每個前面有核取方塊。勾選的才會被索引。

![索引範圍](images/scope.png)

1. 勾選或取消資料夾（可以展開到子資料夾）。
2. 變更後上方會出現「Not applied yet」，這時還沒有生效。
3. 按工具列的**打勾**套用並重新索引；按旁邊的**還原箭頭**放棄這次修改。

工具列的其他按鈕：全部勾選、全部取消、和磁碟同步。在檔案總管對資料夾按右鍵，也有「Norn: Index This Folder / Exclude This Folder / Index Only This Folder」。

設定存在 `.norn/config.json`，可以放進版本控制讓同事共用；`.norn/cache/` 是索引快取，不需要也不應該放進版本控制（Norn 會自動產生忽略它的 `.gitignore`）。

## 10. 編譯條件

C 程式裡大量的 `#if` / `#ifdef` 讓同一份原始碼在不同設定下編譯出不同的內容。Norn 需要知道哪些巨集有定義，才能把沒有被編譯的分支排除在索引之外。

### 10.1 通常不需要設定

Norn 會**自動採用專案裡找得到的建置設定**，往下找三層資料夾：

| 來源 | 檔案 |
|---|---|
| DS-5 / Arm Development Studio / Eclipse CDT | `.cproject` |
| Keil µVision | `.uvprojx`、`.uvproj` |
| IAR Embedded Workbench | `.ewp` |
| Visual Studio | `.vcxproj` |
| VS Code C/C++ 擴充套件 | `.vscode/c_cpp_properties.json` |
| 編譯資料庫 | `compile_commands.json`、`compile_flags.txt` |
| PlatformIO | `platformio.ini` |
| Make / CMake | `Makefile`、`*.mk`、`CMakeLists.txt` |

專案檔裡的每個建置組態（Debug、Release、各個 target）會成為一組設定，第一組自動生效。

完全找不到建置設定時，Norn 仍會盡量判斷：原始碼自己定義的巨集照樣生效；判斷不出來的分支兩邊都保留，相關的呼叫標示為不確定。

確定被關掉的分支（例如 `#if 0`，或條件中的巨集已確定為 0 或未定義）不會被索引。游標放在這些行上時，Relation 和 Context 會顯示「Not compiled: line N switched off by the compile conditions in force」，跳到定義、找參考也不會有結果；因為那一行的名稱在目前的條件下不代表任何東西。（0.19.5 起。之前的版本會用游標下的名稱去查有編譯到的地方，看起來像被關掉的程式碼也有關係。）

判斷不出來的分支裡的呼叫仍會出現在 Relation，但會加上橘色標記「under #if (OPTION_A)」，指出是哪一個 `#if` 決定它會不會被編譯；函式上方的摘要也會註明，例如「3 callers (1 only if an undecided #if)」。看到這個標記，代表 Norn 不知道那個巨集的值：在 Compile Conditions 裡定義或取消定義它（例如 `OPTION_A=0`），那個分支就會確定下來，呼叫隨之出現或消失。（0.19.6 起。）

> Makefile 和 CMakeLists.txt 只會讀取「寫明的」定義：不在任何 `ifdef` / `if()` 裡、值不是變數算出來的 `-D`。Norn 不會執行它們，所以讀到的會比實際建置少。有專案檔或 `compile_commands.json` 時會優先採用。

### 10.2 查看目前生效的條件

**Compile Conditions** 檢視列出目前這組設定的每一個巨集、它的值，以及是誰說的：

![編譯條件](images/conditions.png)

- 最上面一列是**目前生效的設定組**（圖中是 `Debug`）。
- **Set here**：你在 Norn 裡手動設定的，優先權最高。
- **Debug (.cproject)**：從專案檔讀來的。
- **From the sources**：原始碼自己定義、而且確定生效的巨集。

### 10.3 切換設定組

點**狀態列上的設定名稱**，或 Compile Conditions 最上面那一列：

![切換設定組](images/conditions-switch.png)

可以切到另一組、建立新的一組、複製目前這組再修改，或刪除。切換後 Norn 會依新的條件重新索引。

### 10.4 手動調整

在 Compile Conditions 標題列按 **＋**，或用下列指令：

| 指令 | 作用 |
|---|---|
| Norn: Define a Macro for Conditions… | 定義一個巨集，例如 `ARCH_SIZE=64` 或只寫 `USE_NET` |
| Norn: Mark a Macro as Not Defined… | 指定某個巨集沒有定義 |
| Norn: Read Conditions from a File… | 從一個全是 `#define` 的標頭檔，或 Kconfig 的 `.config` 讀入 |
| Norn: Assume Names Are Not Defined… | 把某個字首（例如 `CONFIG_`）或所有沒被定義的名稱都當成未定義 |

對某一列按右鍵可以移除，或覆寫從專案檔讀來的值。

所有設定存在 `.norn/config.json`。直接用文字編輯器改這個檔也可以，存檔後 Norn 會自動套用。

## 11. 與 C/C++ 擴充套件（IntelliSense）連動

如果同時安裝了微軟的 C/C++ 擴充套件（`ms-vscode.cpptools`），Norn 會把**目前生效的巨集定義和 include 路徑**即時提供給它，IntelliSense 的灰階顯示（未編譯的 `#if` 區塊）和跳轉就會和 Norn 一致。

1. 第一次使用時，C/C++ 擴充套件會問是否讓 Norn 設定這個資料夾的 IntelliSense，按**允許**。
2. 之後在 Norn 新增巨集、切換設定組，或修改 `.norn/config.json`、`.cproject`，IntelliSense 都會立刻跟著更新，不需要其他操作。

如果當初按了不允許，可以在工作區的 `settings.json` 加上：

```json
"C_Cpp.default.configurationProvider": "humphrey0925.norn"
```

include 路徑包含建置設定裡寫的路徑，以及所有含有標頭檔的已索引資料夾。這個機制不會寫入任何檔案。

> 另有一個舊的做法：把設定 `norn.conditions.updateCppProperties` 打開，Norn 會在 `.vscode/c_cpp_properties.json` 維護一組名為「Norn」的設定，你再到 C/C++ 擴充套件裡手動選它。一般用上面的方式即可。

### 巨集的值與自動完成

- **巨集的值**：游標停在巨集或 enum 常數上，會顯示它的定義和算出來的值，例如 `#define BUF_Q3 BUF_Q(3)` 旁邊顯示 `= 4108 (0x100C)`。Norn 會沿著它用到的其他巨集一路展開，用判斷 `#if` 的同一套規則計算；`(U32)` 這類型別轉換會略過，enum 常數會照 C 的規則數下去。帶參數的巨集停在呼叫處（例如 `BUF_Q(3)`）時，會代入那裡的引數。同一個巨集有好幾份條件定義時，每一份各自列出自己的值。用到 `sizeof`、變數或找不到定義的名字時，只顯示定義、不顯示值。
- **自動完成**：輸入名稱時，Norn 會從索引裡找出符合的函式、巨集、型別、變數，和 C/C++ 擴充套件的建議一起列出。不需要先編譯，也不需要設定 include 路徑。

## 12. 保持索引最新

| 情況 | Norn 的處理 |
|---|---|
| 在 VS Code 裡存檔 | 自動更新（實測 Redis 約 20 毫秒、binutils 約 90 毫秒） |
| 在 VS Code 外修改、新增、刪除檔案（例如 `git pull`） | 自動偵測並更新 |
| 修改 `.norn/config.json` 或專案的建置設定檔 | 自動套用 |
| 覺得索引和實際不一致 | 點狀態列的「Norn … files」或執行「Norn: Sync Index with Disk」 |
| 想整個重來 | 執行「Norn: Discard the Saved Index and Rebuild」 |

大型專案重新開啟時，Norn 會先用上次存下的結果讓你馬上可以查詢，同時在背景檢查檔案有沒有變動；這段時間狀態列會顯示轉圈圖示。

## 13. 設定項目

在 VS Code 設定裡搜尋 `norn`。

| 設定 | 預設 | 說明 |
|---|---|---|
| `norn.serverPath` | 空白 | 留空使用內建的索引程式。只有要改用自己建置的版本時才填完整路徑 |
| `norn.cache.enabled` | 開 | 把索引存在磁碟，下次開啟只處理有變動的檔案 |
| `norn.cache.directory` | 空白 | 索引存放位置。留空為工作區的 `.norn/cache` |
| `norn.codeLens.enabled` | 開 | 在每個函式上方顯示呼叫者與被呼叫者數量 |
| `norn.fileSymbols.showLocals` | 開 | 在 File Symbols 的函式底下列出參數與區域變數 |
| `norn.conditions.updateCppProperties` | 關 | 在 `c_cpp_properties.json` 維護一組「Norn」設定（見第 11 節的說明） |
| `norn.includeAliases` | 空白 | 建置時由範本產生的標頭檔對應，格式 `產生的檔名=範本檔名`，例如 `bfd.h=bfd-in2.h` |

## 14. 指令一覽

按 `Ctrl+Shift+P` 開啟命令選擇區，輸入 `Norn` 可以看到全部。常用的：

| 指令 | 作用 |
|---|---|
| Norn: Search Symbols… | 搜尋符號，預先帶入游標所在的字 |
| Norn: Show Callers / Show Calls / Show References | 在 Relation 顯示游標所在符號的呼叫者／被呼叫者／參考 |
| Norn: Show Function Pointer Assignments | 顯示函式指標的指派位置 |
| Norn: Show Relation as a Tree / as a Graph | 切換樹狀與關係圖 |
| Norn: Lock Relation / Lock Context | 鎖定，不再跟著游標 |
| Norn: Sync Index with Disk | 和磁碟同步 |
| Norn: Restart Index | 重新啟動索引程式，只重新處理有變動的檔案 |
| Norn: Discard the Saved Index and Rebuild | 丟掉存檔，從頭建立索引 |
| Norn: Switch Compile Configuration… | 切換編譯條件的設定組 |
| Norn: Open the Scope File | 開啟 `.norn/config.json` |
| Norn: Reset View Layout | 把所有 Norn 檢視放回預設位置 |

Norn 沒有預設快速鍵。需要的話在「鍵盤快速鍵」（`Ctrl+K Ctrl+S`）搜尋 `Norn` 自行指定。

Norn 同時也是標準的語言伺服器，所以 VS Code 內建的操作一樣有效：`F12` 移至定義、`Shift+F12` 尋找所有參考、`Ctrl+T` 依名稱找符號、`Ctrl+Shift+O` 檔案內符號、呼叫階層。

## 15. 常見問題

**狀態列顯示「Norn not running」**

- 訊息若提到內建伺服器不見了（built-in server is missing）：防毒軟體可能把擴充套件裡的 `norn.exe` 移除了。重新安裝擴充套件，並把它加入防毒軟體的例外；或把壓縮檔裡的 `norn-windows-x64.exe` 放到固定位置，在 `norn.serverPath` 填入完整路徑。
- 檢查 `norn.serverPath` 是否填了一個已不存在的路徑；清空就會使用內建的。

**同時出現兩套檢視，或索引跑了兩次**

舊的 SI Code Index 還裝著。到擴充功能面板把它移除。

**儲存編譯條件時出現「存取被拒」**

0.18.0 以後已處理：`.norn/config.json` 被標成唯讀（版本控制系統常這樣做）或隱藏時，Norn 會自行解除唯讀後寫入。如果仍然出現，訊息會附上檔案路徑，請確認你對那個檔案有寫入權限。

**某個函式的呼叫者少了，或多了不該有的**

多半和編譯條件有關：

- 少了：該呼叫可能位在目前設定下不會編譯的 `#if` 分支。到 Compile Conditions 確認生效的是不是你要的那一組。
- 多了：沒有建置設定時，判斷不出來的分支兩邊都會保留。提供建置設定（見第 10 節）或手動定義相關巨集。

**IntelliSense 的結果和 Norn 不一致**

確認 C/C++ 擴充套件有採用 Norn 當設定提供者（見第 11 節）。

**索引佔太多記憶體或太慢**

在 Index Scope 取消不需要的資料夾（第三方程式庫、測試資料、其他平台的程式碼）。

**想把設定分享給同事**

把 `.norn/config.json` 放進版本控制即可，裡面有索引範圍和編譯條件。

**索引看起來不對，想完全重建**

執行「Norn: Discard the Saved Index and Rebuild」。

## 16. 已知限制

- C++ 的準確度比 C 低：透過隱式轉換呼叫的建構子、測試框架裡串接的 `operator<<` 常會找不到或標成不確定。
- 內建的索引程式只有 Windows 64 位元版本。其他系統需要自行從原始碼建置，再用 `norn.serverPath` 指定。
- 沒有建置設定、原始碼本身也判斷不出來的 `#if` 分支，會兩邊都保留並把相關呼叫標示為不確定。
- Makefile 與 CMakeLists.txt 只讀取寫明的 `-D`，讀到的定義比實際建置少。
- Keil、IAR、Visual Studio、PlatformIO 專案檔的讀取目前只用範例檔測試過，遇到讀不出來的專案請回報。
- Linux kernel 規模的專案，建立索引時約需 6 GB 記憶體，之後常駐約 4–5 GB。

---

本文的截圖取自 GNU binutils 的 `bfd/` 與 `include/` 目錄（726 個檔案、約 7.6 萬個符號）。
