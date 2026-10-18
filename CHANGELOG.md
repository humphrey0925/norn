# Changelog

## Unreleased

**C++ and the Linux kernel more accurate**

### C++

- Implicit conversions of arguments (a `std::string` where a `Slice` is taken, format strings through `LogInfo` or `strprintf`), `T x(a);` and `T{...}` making a `T`, `return s;` calling the copy or move constructor, and overloads in several files narrowed by the number of arguments.
- gtest and Boost.Test: `ASSERT_*`/`EXPECT_*`, `TEST_F` fixtures and their bases, `ASSERT_THAT`'s `operator()`, `name_invoker`.
- Parsing: a macro alone in a class or function body, `extern "C" {` split across `#ifdef __cplusplus`, a header whose C++ part defines a class.
- Against clang: fmt calls found 75.0% → 89.7% (certain 62.6% → 79.6%), leveldb 86.8% → 95.9% (68.2% → 87.8%), bitcoin 86.1% → 93.9% (78.5% → 89.6%); certain precision up a little on each. The first opening after the upgrade builds the index again.

### C, and the Linux kernel

- A Kconfig configuration of Linux says which architecture it is for. The other architectures' folders, and `tools/` and `scripts/` (programs the host runs), are taken as not built: still indexed and searched, but where a name or an `#include` could mean what is there or what the build compiles, it means what the build compiles. The setting is `"notBuilt": [...]`, for any project to use.
- With it, what Kbuild and the compiler it names define for C: `__KERNEL__`, the compiler's own macros (`__clang__` and its version, or `__GNUC__`), the architecture's (`__x86_64__`); `__ASSEMBLY__` and `__CHECKER__` not defined.
- `__ASSEMBLER__` is never defined in C: the many headers inside `#ifndef __ASSEMBLER__` are no longer undecided.
- A macro defined in a source file is not taken as a meaning of a name in another source file that does not include it (`kfree` from `lib/decompress_unxz.c`).
- Told its architecture, the kernel's compilations are each read through (they were too many to read before), and what they compile of a file is asked once per file, not once per call.
- A call inside a function a macro makes is there whenever the function is: that the macro may not be defined (the kernel's `TRACE_EVENT`, read again and again under other definitions) no longer makes the call uncertain.
- Macros of folders the build does not compile are not seen from those it does (`READ_ONCE` from `samples/bpf`).
- In a macro's body, a name its own file defined just before it means that definition: the kernel's `trace_events.h` redefines its macros stage after stage, and its `do_trace_event_raw_event_*` functions were never made.
- A macro defined further down the file a use is in is not there yet at the use (the kernel's `mutex.h` makes `class_mutex_constructor`, then a macro of that name): the functions its guards make were read as declarations, and every `guard(mutex)` call missed.
- A call through a macro that wraps the function of its own name (`#define error(...) (error(__VA_ARGS__), const_error())`) reaches that function for certain.
- Where every compilation was read, every definition of a macro met is expanded once, and a name first met under another choice of definition is read under it (the kernel's `TRACE_EVENT`, then each stage of `DECLARE_EVENT_CLASS`).
- On the Linux kernel (x86_64 defconfig, against clang), with the build's configuration in the tree: calls found and certain 69.9% → 94.5%, all calls found 90.3% → 97.9%, definitions found 92.8% → 99.6%, precision of certain calls 98.7% → 99.2%; indexing takes longer (90 s → about 145 s on the machine measured) and more memory (5.0 → 6.9 GB at most). Without a configuration: certain 59.7% → 64.2%. git's certain calls 99.4% → 99.6%. Without a configuration, little changes (certain 59.7% → 59.9%). Redis, git, binutils, STM32F4 HAL and nrfx unchanged.

## 0.23.2

**On the Visual Studio Marketplace, with complete third-party notices**

- Third-party notices are complete: each release has `THIRD_PARTY_NOTICES-<platform>.txt` beside the server executables (0.23.0 and 0.23.1 shipped them without); and the notices now include the Rust standard library, musl (in the static Linux servers), and the MIT license of tree-sitter-cpp, whose crate ships none.

## 0.23.1

**Renamed norn-c for the Marketplace**

- The extension's identifier is now `humphrey0925.norn-c`: the Marketplace already has another extension named `norn`, which kept 0.23.0 off it. The name shown, the commands, the settings (`norn.*`) and the `.norn` folder are unchanged. If 0.23.0 was installed from a `.vsix`, Norn says so, so that the old one can be uninstalled and the folder is not indexed twice.
- The user guide is in English too ([docs/norn-guide.md](docs/norn-guide.md)); the Traditional Chinese one is now [docs/norn-guide.zh-TW.md](docs/norn-guide.zh-TW.md). Both describe the present version.
- For the C/C++ extension the setting is now `"C_Cpp.default.configurationProvider": "humphrey0925.norn-c"`: a folder whose settings name `humphrey0925.norn` needs it changed.
- The changelog is in English.

## 0.23.0

**Kconfig projects with nothing to set, an index reused across folders, a manual for the command line**

- The `norn` command line has a manual ([docs/cli.md](docs/cli.md), [Traditional Chinese](docs/cli.zh-TW.md)), and `--help` describes every argument. Given a folder with no index, or a tree with no C sources, it now says so and exits with 2 instead of crashing.
- With nothing set, the configuration a Kconfig build (Linux, U-Boot, Buildroot, BusyBox) leaves is followed: `include/generated/autoconf.h` or `.config`, in the tree or in a build folder (`make O=build`). Every `CONFIG_` name it does not set is taken as not set.
- Opening a folder inside or around one already indexed takes that index's facts for every file read the same way, instead of parsing it again.
- **Fixed**: a request arriving while the saved index was still being read (opening a header, say) made the next request after the index was ready crash the server, and VS Code restarted it until it gave up.
- **Fixed**: until the cursor moved after a workspace was opened, Context said "Nothing the index knows here".
- Hover shows a function's documentation comment also when a blank line separates it from the definition (GNU style).
- With `NORN_TRACE` set, a server crash is written to the trace file too.

## 0.22.0

**Editing C in full without the C/C++ extension**

Without the C/C++ extension, or with its IntelliSense turned off (`C_Cpp.intelliSenseEngine: disabled`), Norn provides the following; with IntelliSense on, it leaves them to it.

- Hover: functions, variables and types show their declaration as written and the documentation comment above it (the header prototype's when the definition has none); parameters and locals show their declaration.
- Signature help: on `(` and `,`, the declaration with the current parameter marked.
- Completion: members after `.` and `->` (down `a.b->`, through typedefs); the parameters and locals of the function; C keywords; `#include` paths.
- Highlighting by what a name is: macro, function, type, global, parameter, local, member, enumerator; static and declarations marked as well.
- Folding: braces, each branch of an `#if`, comments of several lines, runs of `#include`.
- Rename: every spelling of the same thing, from the index; a static function or variable only in its own file, and macros and global names also in `#if` branches the conditions in force switch off.
- Parameter names before the arguments of a call (inlay hints; not when the argument is already that name). Turn off `editor.inlayHints.enabled` to hide them.
- Formatting with clang-format when it is installed: the project's `.clang-format`, or else the Visual Studio style the C/C++ extension uses.

**Always there**

- Where a name is used that no header read declares, the light bulb offers "Add #include", written the way the project's other files write it.
- `#include` lines are links and F12 opens the file; the other uses of a name in the file are highlighted; Alt+O switches between header and source (without the C/C++ extension).
- Lines the conditions in force do not compile are dimmed (when the C/C++ extension does not).

**Decided more accurately**

- The compiler is recognised from the build description (GCC, Arm Compiler 5 and 6, IAR and others), and the macros it defines decide `#if`.
- Calls written in a macro's argument that the expansion drops are not counted as calls.

## 0.21.1

**More macro values worked out**

- Worked out with the project's actual conditions and build files, so that a macro depending on the configuration (`CHIP_B_EN = BOARD_KIND == BOARD_KIND_B`) has a value too.
- When a type is itself a macro (`#define U32 unsigned long`), casts such as `(U32)0x10` are dropped correctly; a cast to a type of known width cuts the value to that width.
- For a macro defined in the headers of several chips, the definition read by the compilation units that read this file is chosen; failing that, the nearest folder.
- Where a variable or member is used and there is no single value, what can be worked out is, and the rest is left as an expression, such as `(void *)(0x400800 + ((U32)(gRing.Used[2] & 3) << 4))`. Expansion follows the C preprocessor's rules, `##` included.
- When there is no value, it says why: which name has several definitions, which macro has the wrong number of arguments.
- Alongside the C/C++ extension, IntelliSense shows the definition and its expansion, and Norn adds only a line with the value.

**What the conditions in force do not read is left out**

A header no compilation unit reads under the conditions in force (one included only from an `#if SOC == 2` that is certainly false, say) has its symbols left out of Project Symbols, symbol search, completion, Relation and the call hierarchy; Go to Definition and macro values use it only when there is nothing else. Under another set of conditions that reads it, they come back.

**Decided as IntelliSense decides**

When the conditions come from the build's own description (a DS-5 `.cproject`, the C/C++ extension's `c_cpp_properties.json`), a name it does not define and the sources do not define is taken as not defined, as the compiler and IntelliSense take it: `#ifdef _FEATURE_X_` takes the `#else` instead of being undecided. The macros a compiler defines itself (`__CC_ARM`, `__GNUC__`, `_WIN32` and the like) stay undecided.

**Headers chosen by compiler or chip no longer interfere**

When the branches of one `#if` each include a header (CMSIS choosing `cmsis_gcc.h` or `cmsis_iccarm.h` by compiler), the macros of one branch no longer apply to the other's header. STM32F4 HAL (with CMSIS) joins the projects checked against clang: definitions right went from 93.73% to 99.55%, calls found from 91.44% to 96.54%. git, Redis and binutils are unchanged.

**Faster completion**

Completion looks names up by prefix in a sorted list, and lets VS Code filter a complete result itself instead of asking again at every key.

## 0.21.0

**Macro values**

Hover over a macro or an enum constant to see its definition and its value: beside `#define BUF_Q3 BUF_Q(3)`, `= 4108 (0x100C)`, as for an enum constant.

- The macros it uses are expanded in turn, and worked out by the rules that decide `#if`.
- Casts such as `(U32)` are dropped; enum constants are counted as C counts them (one more than the one before, when no value is written).
- On a call of a macro with parameters (`BUF_Q(3)`), the arguments there are put in.
- For a macro with several conditional definitions, each is listed with its own value, marked when it holds only if an undecided `#if` is true.
- Where `sizeof`, a variable or a name with no definition is used, only the definition is shown.

**Completion**

As you type a name, the functions, macros, types and variables of Norn's index that match are offered beside the C/C++ extension's suggestions. Nothing needs building and no include path needs setting.

**Calls marked certain more accurately**

Most calls of macros reached through layers of macros are no longer marked uncertain; what is left depends on whether the build defines a switch, which the sources really cannot tell.

Among the cases fixed:

- The definitions in a header's own `#else`, when the header is included under an undecided `#if`, were taken as uncertain.
- Include guards not recognised: `#if !defined(G)`, something after the `#endif`, an `#include` before the guard; and the guard after a header the build generates and the project does not have.
- When an include could be one of several files of the same name, the folder the project's other files include from most is chosen, as an `-I` path would.
- A call and the macro's definition in the same undecided `#if`, or a call and the include of the header in the same undecided `#if` (a whole `.c` inside `#if FEATURE_EN`), are now certain.
- A call in a header is certain when no compilation unit reading it says the macro does not exist, instead of becoming uncertain everywhere because a few units cannot tell.

Against clang's view of the real builds, certain calls went from 99.23% to 99.29% on git and from 95.45% to 95.49% on binutils, Redis unchanged; the precision of certain calls stays above 99.6%. Indexing time and memory are unchanged.

**Other**

- Project Symbols lists prototypes and declarations apart from definitions.
- Go to Definition (context menu and F12) uses Norn's answer: on a variable, its declaration; on a function, its definition; on the definition, its prototype. Ctrl+F12 and Shift+F12 are Implementations and References.
- Fixed an occasional problem: two indexes built at once in the same process could disturb each other's condition results.

## 0.20.0

**C++**

C++ files (`.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh` and the like) are indexed too, with the C files. Classes, methods, namespaces, templates, overrides, constructors, operator overloads, lambdas, `auto` and structured bindings are found, and members are shown with their class. Projects of C alone are unaffected: what only C++ needs is kept only when there are C++ symbols.

Against clang's view of the real builds:

| Project | Calls found | Certain | Certain and right |
|---|---|---|---|
| fmt | 74.96% | 61.06% | 96.44% |
| leveldb | 86.78% | 67.86% | 93.09% |
| bitcoin | 86.17% | 78.42% | 95.44% |

C++ is still less accurate than C, mostly missing constructors called through an implicit conversion and the chains of `operator<<` of test frameworks.

**Faster indexing**

- The first index of a large project is several times faster. Every compilation unit decided the `#if` of the same headers again, expanding macros that refer to each other again and again; a result is now reused while the macros its condition uses stay as they were.
- The Linux kernel about 5% faster (58 s to 55 s).
- Other C projects: speed, memory, index size and accuracy unchanged.

**`#if` worked out by C99's rules**

- Octal (`010` is 8; it was read as 10), hexadecimal, binary, the `u`/`l`/`ll` suffixes, and numbers out of range becoming unsigned.
- Character constants (`'A'`, `'\n'`, `'\x41'`), every operator at its precedence, `?:`, comparisons of signed with unsigned (`-1 > 0u` is true), short-circuit evaluation.
- Macros with parameters (`GIT_GNUC_PREREQ(4, 5)`) are worked out with their arguments.
- What C leaves undefined (division by zero, signed overflow, shifts out of range) and multi-character constants stay undecided.

## 0.19.12

- **References from Relation show in Context**: click a reference and Context shows the code around that line while the editor stays where it is. Double-click or Enter opens it; Ctrl+click opens it in the editor group beside.
- **Faster References**: the list comes straight from Norn's index (`si/references`), with each line's text. It used to go through every reference provider VS Code has, which with the C/C++ extension also searched the whole folder, and each file had to be opened in an editor to get its line's text.
- With the cursor in code the conditions switch off, References says why that code is not compiled, as the other views do.

## 0.19.11

- **Third-party notices**: `THIRD_PARTY_NOTICES.txt`, in the release and in the extension, covers all the software of others Norn contains: the 71 Rust crates the server links, the 10 npm packages the extension bundles, and the codicons icon font of the views (CC-BY-4.0). Each with its name, version, license, source and the authors' license text.
- Otherwise the same as 0.19.10.

## 0.19.10

**The kernel more accurate again**

Against clang's view of the real build (x86_64 defconfig), on the kernel:

| | 0.19.9 | 0.19.10 |
|---|---|---|
| Definitions found | 88.30% | **91.99%** |
| Calls found | 84.62% | **90.62%** |
| Certain and right | 96.59% | **98.62%** |

Redis, binutils and git the same or a little better; the kernel's indexing time and memory no worse.

**What changed**

- **Loop macros**: `for_each_online_cpu(i)`, `list_for_each_entry(...)` and the like, which expand to a `for`, confused the parser and lost much of the function after them (`schedule()` in `kernel/sched/core.c`, say). They are now read as loops.
- **`SYSCALL_DEFINEx(...)`**: the body after the macro belongs to the right function (`__do_sys_xxx`), and the wrappers `__x64_sys_xxx → __se_sys_xxx → __do_sys_xxx` are linked.
- **Macros with several definitions**: each definition is read at least once, none skipped for coming later (the `trace_xxx()` that tracepoints' `DEFINE_EVENT` makes, say).
- **Stubs and the real definition**: the real definition is now listed too when
  - the declaration and a stub are in the same header (`percpu_counter.h`);
  - the stub is in a header only some configurations include (`rcutiny.h`);
  - the stub is made by a macro (`pci_read_config_*` in `pci.h`).
- **Include paths**: of several architecture headers of the same name, the copies under `tools/` for other programs are no longer chosen (they linked `msleep` to the small C library there).

**Note**: the first opening after the upgrade builds the index again.

## 0.19.9

**Functions made by macros linked more accurately (checked on the Linux kernel)**

Against clang's view of the real build (x86_64 defconfig), on the kernel:

| | 0.19.8 | 0.19.9 |
|---|---|---|
| Definitions found | 85.83% | **88.30%** |
| Calls found | 81.82% | **84.62%** |
| Certain and right | 96.11% | **96.59%** |

**What changed**

- **Tracepoints**: functions such as `trace_xxx()`, made through several layers of `TRACE_EVENT`, are found and their calls linked. The kernel reads the same header several times under different `TRACE_EVENT` definitions, and Norn used to drop the one that makes `trace_xxx()`.
- **Macros like `guard(mutex)(m)`**: a macro that expands to a function's name, with the call's parentheses outside it, is a call (to `class_mutex_constructor`, say).
- **Header stubs and the real definition**: when the real definition is under an `#if` too (`_dev_info` under `#ifdef CONFIG_PRINTK`), both are listed. Every `dev_info()` used to lead to the stub alone.

**Fixed**

- When the extension restarted without the window reloading (after an update, say), the lock buttons of Relation and Context could still show "locked" when they were not. They now always start unlocked, as the buttons say.

## 0.19.8

**Closer to what is really compiled (checked on the Linux kernel)**

Against clang's view of the real build (x86_64 defconfig), on the kernel:

| | 0.19.7 | 0.19.8 |
|---|---|---|
| Calls found | 79.22% | **81.82%** |
| Found and certain | 56.10% | **59.03%** |
| Certain and right | 94.35% | **96.11%** |

**What changed**

- A declaration under `#ifdef CONFIG_X` in a header with a stub under `#else`: when the condition cannot be decided, the real definition in the `.c` is listed beside the stub. For functions and macros alike.
- Other programs' headers are no longer taken as the project's own. The kernel's `tools/` are separate user-space programs, and were mistaken for it, linking calls such as `kfree` wrongly.
- A `static` function in a header is seen only by the files that include it.
- A macro holds only where it is seen:
  - one defined in a `.c` only in that file;
  - one in a header only in the files that include it, after its definition.

**Note**: the first opening after the upgrade builds the index again.

## 0.19.7

- **Fixed**: a macro whose value is an expression was not worked out. `#define ENABLE_SELF_TEST (0 && ENABLE_OPT && ...)` now comes to 0 by the rules of `#if`, the code under it is taken as not compiled, and it no longer shows in Relation.
- **Fixed**: macro values longer than 120 characters were not kept; any length is now kept whole.
- **New setting** `"undecided": "off"` (optional, off by default): every macro with no known value is taken as not defined. By default a branch that cannot be decided is kept both ways.

## 0.19.6

- **New**: a call inside an `#if` the conditions cannot decide is marked in orange in Relation, "under #if (OPTION_A)", naming the `#if` it depends on. Set that macro in Compile Conditions to settle it.
- **New**: the line above a function says how many callers depend on an undecided `#if`: "3 callers (1 only if an undecided #if)".
- **Note**: the first opening after the upgrade builds the index again.

## 0.19.5

- **Fixed**: with the cursor in code an `#if` switches off, the name under it was looked up where it is compiled, as if the switched-off code had calls. Now:
  - Relation and Context say "Not compiled: line N switched off by the compile conditions in force";
  - Go to Definition and Find References find nothing.
- **Note**: the first opening after the upgrade builds the index again.

## 0.19.4

- **Fixed**: the icons of Project Symbols, Relation and Context were missing. Since the packaging changed in 0.19.0 the icon font was left out of the package, in 0.19.0 to 0.19.3.
- Two checks before a release: the package's contents are checked, and the unpacked package is run in VS Code. A package missing a file it needs is not released.

## 0.19.3

- **Faster start**: starting from the saved index no longer works out the macro sets again or reads every source file. The kernel, from start to a complete index: 8.7–9.2 s to 5.6–6.2 s.
- **Fixed**: on Windows, reading a file updates its last access time, which VS Code reported as a change, so unchanged files were parsed again (458 on first opening git). Files whose content has not changed are no longer read again.
- **Fixed**: files other than .c/.h brought in only by `#include` (`.def`, say) were not added or removed in a sync.

## 0.19.2

- **New** command **Norn: Search Symbols…**: search by fragments of a name or its initials, with the name under the cursor typed.
- **Fixed**: setting descriptions still had the old name "si".
