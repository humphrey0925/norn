# The `norn` command line

`norn` is the indexer the VS Code extension runs, and a tool of its own: it
indexes a C tree without a build, answers queries about symbols and calls,
serves them to scripts, editors and AI agents, and lists everything it found.
[繁體中文版](cli.zh-TW.md)

- [Installing](#installing)
- [In five commands](#in-five-commands)
- [Commands](#commands): [`index`](#index), [`query`](#query),
  [`serve`](#serve), [`live`](#live), [`lsp`](#lsp), [`mcp`](#mcp), [`symbols`](#symbols),
  [`calls`](#calls), [`sample`](#sample), [`bench-query`](#bench-query),
  [`bench-incr`](#bench-incr)
- [What a result looks like](#what-a-result-looks-like)
- [The project's settings](#the-projects-settings)
- [Exit status and errors](#exit-status-and-errors)

## Installing

Each [release](https://github.com/humphrey0925/norn/releases) has the
executable for every platform beside the extension, checked by `SHA256SUMS`:

| Platform | File |
|---|---|
| Windows x64 / Arm64 | `norn-win32-x64.exe`, `norn-win32-arm64.exe` |
| Linux x64 / Arm64 (static, any distribution) | `norn-linux-x64`, `norn-linux-arm64` |
| macOS Intel / Apple silicon | `norn-darwin-x64`, `norn-darwin-arm64` |

Beside each is `THIRD_PARTY_NOTICES-<platform>.txt`: the licenses of the
software of others built into it, to keep with it if you pass it on.

Rename it to `norn` (`norn.exe` on Windows), put it on your `PATH`, and on
Linux and macOS make it executable (`chmod +x norn`). The macOS executables
are not signed: the first time, allow it under System Settings → Privacy &
Security, or run `xattr -d com.apple.quarantine norn`.

To build it instead: `cargo build --release` in a checkout gives
`target/release/norn` (see [Building from source](../README.md#building-from-source)).

`norn --version` says which one you have.

## In five commands

```sh
norn index ~/src/myproject --out ~/idx/myproject     # index the tree
norn query ~/idx/myproject find uart_send            # where it is defined and declared
norn query ~/idx/myproject callers uart_send         # who calls it
norn query ~/idx/myproject search "uart snd"         # find a name by fragments
norn live ~/src/myproject                            # a query server kept up to date
```

Nothing has to be set up first: `norn` works out from the tree's own build
files which `#if` branches are compiled (see
[The project's settings](#the-projects-settings)).

## Commands

Options every command takes:

| Option | |
|---|---|
| `--threads <N>` | Worker threads; `0` (the default) uses every core |
| `--system-parser-alloc` | Let the parser use the system heap instead of mimalloc (for measurements) |
| `--no-build-config` | Where `.norn/config.json` names no configuration, index with no compile conditions instead of following the first build configuration found in the tree |
| `-h`, `--help` | The command's options |
| `-V`, `--version` | The version |

### `index`

```
norn index <ROOT> [--out <DIR>] [--stores <LIST>] [--include-alias <FROM=TO>]... [--stride <K>] [--dump-binds]
```

Indexes the tree at `ROOT` from nothing and writes the index to `--out`
(default `out`) as `graph.si`. Prints what it found and how long each step
took, as one JSON object (files, lines, symbols, edges, conditions known,
includes found and missing, time per step in `ms`, memory in `mem`).

| Option | |
|---|---|
| `--out <DIR>` | Where to write the index (default `out`) |
| `--stores <LIST>` | Comma-separated: `csr` (the default, what the other commands read), `csr-packed`, `sqlite-naive`, `sqlite-tuned`, or `none` (index, write nothing). The others are kept to compare storage layouts. |
| `--include-alias <FROM=TO>` | An `#include` of a file named `FROM` reads `TO` instead, for a header the build generates from a template. Repeatable. |
| `--stride <K>` | Index only every K-th file (for measuring how time grows with size) |
| `--dump-binds` | Also write `binds.tsv`: where each function is stored in a member or variable |

`index` does not keep or reuse a cache: for an index kept up to date between
runs, use [`live`](#live).

### `query`

```
norn query <DIR> <OP> <NAME> [--depth <N>]
```

One query against the index in `DIR` (what `index --out` wrote). Prints one
JSON object.

| `OP` | Answers |
|---|---|
| `find` | Every definition and declaration of `NAME` |
| `search` | Up to 50 symbols matching what is typed: fragments in any order (`uart snd`), initials (`us`), letters in order; best first, with a `score` |
| `callers` | Every call of `NAME`: the calling function and the line of the call, including calls through macros and through function pointers stored in members or variables. At most 1,000,000 are listed; `count` is of all of them, and `"truncated": true` says some were left out |
| `callees` | Every call `NAME` makes, the same way |
| `refs` | Every reference to `NAME`, calls and the rest |
| `tree` | How many functions `NAME` reaches through calls, `--depth` levels down (default 3), itself included |

A name with several definitions (one per configuration, say) answers for all
of them.

### `serve`

```
norn serve <DIR>
```

The queries of `query`, over the index in `DIR`, one JSON request per line on
stdin and one JSON answer per line on stdout, for as long as stdin is open.
The index is loaded once, so each answer takes microseconds.

```json
{"op": "callers", "name": "uart_send"}
{"op": "search", "name": "uart snd"}
{"op": "tree", "name": "main", "depth": 2}
{"op": "definition", "file": "src/main.c", "line": 3, "name": "uart_send"}
```

`callers` takes a `limit` too, `{"op": "callers", "name": "printk", "limit": 1000}`,
for fewer than the 1,000,000 it lists at most.

`definition` answers what the name written at that line of that file refers
to (the path relative to the indexed root, the first line being 1), the way
an editor's *Go to Definition* does: the definition that file sees, where
there are several.

### `live`

```
norn live <ROOT> [--cache-dir <DIR> | --no-cache] [--rebuild] [--include-alias <FROM=TO>]...
```

`serve` over the sources themselves, kept up to date. It first prints one
line when it is ready:

```json
{"ready": true, "files": 3, "symbols": 11, "ms": 2.97, "loaded": 0, "extracted": 3, "reused": 0, "link_ms": 0.0, "cache": "…/.norn/cache"}
```

(`loaded`: files whose facts came from the cache; `extracted`: files read;
`reused`: files whose facts were taken from the index of a folder inside or
around this one.) Then it answers every request `serve` does, and these:

| Request | Does |
|---|---|
| `{"op": "update", "files": ["src/uart.c"]}` | Reads these files again, after they changed, were added or deleted |
| `{"op": "sync"}` | Looks at the whole tree for what changed since, and reads that |
| `{"op": "scope", "include": ["src"], "exclude": ["src/tests"]}` | Sets which folders the project is (kept in `.norn/config.json`) and brings the index in line |

Each answers with what changed (`changed`, `added`, `removed`, `extracted`)
and how long it took.

The index is kept between runs in `.norn/cache` under the root, so the next
`live` (or the editor) starts from it.

| Option | |
|---|---|
| `--cache-dir <DIR>` | Keep it here instead |
| `--no-cache` | Index from nothing every time and keep nothing |
| `--rebuild` | Throw away what is kept, index from nothing, and keep that |

### `lsp`

```
norn lsp [ROOT] [--cache-dir <DIR> | --no-cache] [--rebuild] [--include-alias <FROM=TO>]...
```

A Language Server on stdin/stdout, for an editor other than VS Code (Neovim,
Emacs, Helix, Sublime Text, Zed…). The root is the workspace the editor
opens, unless `ROOT` is given. It answers *Go to Definition* (and
declaration, type definition, implementation), *Find References*, the call
hierarchy, workspace and document symbols, hover, signature help,
completion, highlights of the name under the cursor, links on `#include`s,
quick fixes, folding, semantic highlighting, rename and inlay hints, and
keeps the index as `live` does. Its own `si/*` requests, which the VS Code
extension's views are built on, are described in
[the language server protocol](lsp-api.md).

Neovim (0.11):

```lua
vim.lsp.config('norn', { cmd = { 'norn', 'lsp' }, filetypes = { 'c' }, root_markers = { '.git', '.norn' } })
vim.lsp.enable('norn')
```

Helix (`languages.toml`):

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

A [Model Context Protocol](https://modelcontextprotocol.io) server on
stdin/stdout, for AI agents: Claude Code, VS Code's Copilot, Cursor and the
others that take MCP servers. The project is `ROOT` if given; else the
folder the client says it has open (MCP roots), where it says; else the
current folder, unless that is the home folder or the root of the file
system, where a desktop app starts a server: then nothing is indexed, and
each tool says how to name the project. It answers at once and indexes
meanwhile; the first tool called waits for the index.

One project has one index, however many ask of it. The first `norn mcp` of
a project makes it, and the others started for the same project (another
agent, another session) are answered from it rather than make their own;
if it ends, one of them takes over, from the index kept on disk. VS Code's
language server, once it opens the folder, takes the project over, and the
agents are answered from the editor's index. Who holds which project is
kept in `~/.norn/run` (a lock, and where the holder answers: on this
machine only, to a process that knows the word written there).

It watches the tree as the VS Code extension does: a
source file the agent edits is read again before the next answer, and only
it; a changed build configuration (`.norn/config.json`, a Keil, IAR,
Eclipse or Visual Studio project, `compile_commands.json`…) or a folder
added or removed has the whole tree looked at again. With nothing changed,
nothing is read. The index is kept between runs as `live` keeps it.

Its tools answer in text, for the agent to read, with paths relative to the
root:

| Tool | Answers |
|---|---|
| `search_symbols` | Symbols matching part of a name: fragments in any order, initials, letters in order |
| `find_definition` | Where a name is defined and declared; given the file and line it is used on, the definition that place sees |
| `read_symbol` | The source of a definition, with the comment above it, numbered |
| `find_callers` | Every call of a function, saying which come through a macro, a member or variable holding it, or a pointer, and which are uncertain |
| `find_callees` | Every call a function makes, including the functions a member or variable it calls through can hold |
| `find_references` | Every use of a symbol |
| `call_tree` | What a function reaches through calls, or what reaches it, a few levels deep |
| `compiled_lines` | Which lines of a file the build configuration compiles, or whether one line is |

Lists stop at 100 results unless the agent asks for more (`limit`), and say
how many there are. A name with several definitions answers for each; `file`
chooses one.

In VS Code there is nothing to set up: the extension offers the same tools
to the agents VS Code serves MCP servers to (Copilot's agent mode), from the
language server it already runs, so an agent is answered from the index the
editor has, kept up to date as the editor keeps it, with no second index
(VS Code 1.101 or later; `norn.mcp.enabled` turns it off).

Claude Code, in the project's folder:

```sh
claude mcp add norn -- norn mcp "$PWD"
```

Any other client: the command `norn`, with the arguments `mcp` and the
project's folder.

### `symbols`

```
norn symbols <DIR>
```

Every symbol in the index in `DIR`, one per line, tab-separated:
`file  line  kind  decl|def  name`.

```
uart.c	3	function	def	uart_init
uart.h	1	function	decl	uart_init
```

### `calls`

```
norn calls <DIR>
```

Every call between functions in the index in `DIR`, one per line,
tab-separated: the file and line of the call; the calling function's name,
file and line; the called function's name, file and line; `certain` or
`ambiguous`; `def` or `decl` (whether the called function's definition is in
the index); the macro, member or variable the call goes through (`-` for
none, `*` for a pointer not otherwise named); `direct` or `indirect`.

```
uart.c	4	uart_send	uart.c	4	uart_init	uart.c	3	certain	def	-	direct
```

`ambiguous` marks a call the index cannot settle: one under an `#if` the
configuration does not decide, or of a name with several definitions.

### `sample`

```
norn sample <DIR> [--n <N>] [--seed <S>]
```

The names of `N` (default 200) function definitions drawn at random from the
index in `DIR`, as a JSON array; the same seed gives the same names. For
choosing what to query in a measurement.

### `bench-query`

```
norn bench-query <DIR> [--n <N>] [--seed <S>]
```

How long `find`, `callers`, `callees` and `refs` take inside the process, on
`N` (default 2000) functions drawn at random, for each store `index` wrote
into `DIR`: median, 99th percentile and worst, in nanoseconds.

### `bench-incr`

```
norn bench-incr <ROOT> [--n <N>] [--seed <S>]
```

Indexes `ROOT`, then `N` times (default 30) reads one file drawn at random
again and links the whole index again, as after an edit, and reports how
long each part takes.

## What a result looks like

A symbol in a result:

```json
{"name": "uart_send", "kind": "function", "file": "uart.c", "line": 4, "decl": false}
```

`kind` is one of `function`, `macro`, `struct`, `union`, `enum`,
`enum_const`, `typedef`, `global`, `field`, `file`; `decl` is `true` for a
declaration (a prototype, an `extern`), `false` for the definition. Paths are
relative to the indexed root, with `/`.

`callers`, `callees` and `refs` add to each:

| Field | |
|---|---|
| `at_line` | The line of the call or reference |
| `certain` | `false` when the index cannot settle it (an undecided `#if`, a name with several definitions) |
| `via_macro` | The macro the call is written through |
| `through` | The member or variable holding the function pointer the call goes through |

## The project's settings

`norn` reads the same settings as the extension, from `.norn/config.json`
under the root: which folders are the project, and the compile conditions.
With none, it follows the tree's own build files: Arm DS-5 / Eclipse CDT
`.cproject`, Keil, IAR, Visual Studio, `compile_commands.json`,
`compile_flags.txt`, `platformio.ini`, `c_cpp_properties.json`, Makefiles
and `CMakeLists.txt`, and a Kconfig build's `include/generated/autoconf.h`
or `.config`. What can be written there is described in the
[user guide](norn-guide.md#10-compile-conditions) and in
[`cond.rs`](../crates/si-core/src/cond.rs).

## Exit status and errors

A command that finishes exits with 0. Given a folder with no index in it, or
a tree with no C or C++ sources, it says so on stderr and exits with 2. A request `serve` or `live` cannot read is answered with an `error`
field (`{"error": "unknown op …"}`) and the server goes on.
