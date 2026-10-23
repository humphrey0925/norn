# The Norn language server protocol

`norn lsp` is a Language Server: the standard LSP requests any editor can
use, and a set of `si/*` requests of its own that the VS Code extension's
views are built on. This page describes both, so that another front end or
tool can use them. How to start the server and what it keeps between runs
is in [the command line manual](cli.md#lsp); the queries without LSP are in
[`serve`](cli.md#serve) and [`live`](cli.md#live).

- [Starting the server](#starting-the-server)
- [Conventions](#conventions)
- [Standard LSP methods](#standard-lsp-methods)
- [`si/*` requests](#si-requests): [`si/search`](#sisearch),
  [`si/context`](#sicontext), [`si/relation`](#sirelation),
  [`si/references`](#sireferences), [`si/assignments`](#siassignments),
  [`si/symbols`](#sisymbols), [`si/place`](#siplace),
  [`si/locals`](#silocals), [`si/inactive`](#siinactive),
  [`si/counterpart`](#sicounterpart), [`si/conditions`](#siconditions),
  [`si/setConditions`](#sisetconditions), [`si/scope`](#siscope),
  [`si/setScope`](#sisetscope), [`si/sync`](#sisync),
  [`si/status`](#sistatus), [`si/mcp`](#simcp)
- [Notifications from the server](#notifications-from-the-server)
- [`.norn/config.json`](#nornconfigjson)
- [Commands other VS Code extensions can call](#commands-other-vs-code-extensions-can-call)

## Starting the server

```
norn lsp [ROOT] [--cache-dir <DIR> | --no-cache] [--rebuild] [--include-alias <FROM=TO>]...
```

The server speaks JSON-RPC 2.0 on stdin and stdout, each message framed with
a `Content-Length` header as LSP has it. The root is `ROOT` when it is
given, else the `rootUri` (or `rootPath`) of `initialize`, else the current
folder. `workspaceFolders` is not read.

`initialize` answers at once. The index is then read (or made) on a thread
of its own; until it is there, requests are answered from what the last
session saved, or with nothing. When it is there the server sends
[`si/ready`](#siready). Requests that would change the index
(`si/setConditions`, `si/setScope`, `si/sync`) and the notifications
`textDocument/didSave` and `workspace/didChangeWatchedFiles` wait until
then, and are handled in the order they came.

The server does not watch the tree itself. While it runs, it learns of
changes from the client: `textDocument/didSave` for a file saved in the
editor, and `workspace/didChangeWatchedFiles` for files changed, added or
deleted otherwise (`git pull`, a build). The VS Code extension sends both,
from its own file watcher on the C and C++ sources; another client should
too, or send [`si/sync`](#sisync) after such changes. What changed while no
server ran is found when the next one starts.

### `initializationOptions`

| Key | Type | |
|---|---|---|
| `alongside` | boolean | The editor also runs the Microsoft C/C++ extension. Hover then gives only what that one does not (the value of a macro or enumerator), and the server does not offer folding, semantic tokens, rename or inlay hints. Default `false`. |
| `followBuildConfig` | boolean | `false`: a project with no configurations of its own in `.norn/config.json` is indexed with no compile conditions, instead of following the first build configuration found in the tree. Sets written in the file hold either way. Default `true`. |

The VS Code extension sends `alongside` as `true` when the C/C++ extension
is installed and its IntelliSense engine is not `disabled`, and
`followBuildConfig` from its `norn.conditions.followBuildConfig` setting.
Both are read only at `initialize`: to change them, start the server again.
(`followBuildConfig: false` is what the `--no-build-config` option of the
command line does.)

### Errors

| Code | When |
|---|---|
| `-32601` | The method is not one the server answers (`"<method> is not supported"`) |
| `-32002` | A request came before `initialize` (`"not initialized"`) |

Notifications before `initialize`, and notifications the server does not
know (`initialized`, say), are ignored. `si/setConditions` and `si/setScope`
report a failure to save in the result instead (see there).

## Conventions

**Paths and URIs.** `uri` fields are `file://` URIs, with every byte other
than letters, digits and `/-_.~` percent-encoded (on Windows:
`file:///C%3A/src/proj/uart.c`). `file` fields are paths relative to the
root, with `/`. A document outside the root has no answers.

**Lines.** Two kinds of line number appear, and each field says which:

| Where | Counted from |
|---|---|
| LSP `position` and `range` (in params and results) | 0 |
| `line`, `endLine`, `functionLine`, `lines`, `onlyIf[].line`, `[first, last]` pairs | 1 |
| `items[].line` of [`si/references`](#sireferences) | 0 (it is the line of the `range`) |

**Positions.** Requests about the name under the cursor take the usual
`TextDocumentPositionParams`:

```json
{"textDocument": {"uri": "file:///home/me/proj/src/uart.c"}, "position": {"line": 41, "character": 7}}
```

**Lines the conditions switch off.** When the position is on a line the
compile conditions in force do not compile, nothing there is indexed, so
the server does not look the name up:

| Method | Answer |
|---|---|
| `si/context`, `si/relation`, `si/references`, `si/assignments` | `{"inactive": [first, last]}`: the switched-off lines around the position, from 1 |
| `textDocument/references` | `[]` |
| `textDocument/definition`, `hover`, `documentHighlight`, `declaration`, `typeDefinition`, `implementation`, `prepareRename`, `rename`, `prepareCallHierarchy` | `null` |

**Entries.** Most `si/*` results describe a symbol with the same object,
called an *entry* below:

```json
{"name": "uart_send", "kind": "function", "file": "src/uart.c", "line": 42, "endLine": 57,
 "declaration": false, "uri": "file:///home/me/proj/src/uart.c",
 "range": {"start": {"line": 41, "character": 4}, "end": {"line": 41, "character": 13}},
 "data": {"file": "src/uart.c", "line": 42, "name": "uart_send"}}
```

| Field | |
|---|---|
| `name` | The name as shown: a C++ member with its classes (`DBImpl::Get`) |
| `kind` | `function`, `macro`, `struct`, `union`, `enum`, `enum_const`, `typedef`, `global`, `field`, `class`, `namespace` or `file`; for a local variable or parameter (see below) `local` or `parameter` |
| `file`, `line`, `endLine` | Where it is defined (or declared), from 1 |
| `declaration` | `true` for a declaration only (a prototype, an `extern`) |
| `uri`, `range` | The name itself as written there. When it cannot be found on the line or the two below it, the range is empty at the start of the line. |
| `data` | `file`, `line` and the plain `name`: what [`si/relation`](#sirelation) takes as `item` to find the symbol again |

A parameter or local variable is not part of the index's graph but is
answered from the file's own facts. Its entry has `kind` `parameter` or
`local`, `endLine` equal to `line`, `declaration` `false`, and two more
fields: `function` (the name of its function) and `type` (its type's name,
or `null`).

## Standard LSP methods

| Method | Answers |
|---|---|
| `initialize`, `shutdown`, `exit` | `shutdown` saves the index for the next start (when it has finished loading) |
| `textDocument/didOpen`, `didChange`, `didClose` | Full sync (`change: 1`). The text of open documents is used to read lines; the index follows what is saved |
| `textDocument/didSave` | Indexes that file again |
| `workspace/didChangeWatchedFiles` | Indexes each changed, added or deleted file again |
| `textDocument/definition` | Definitions of the name under the cursor, those in files some compilation reads first |
| `textDocument/declaration` | Prototypes and other declarations; the definition when there are none |
| `textDocument/typeDefinition` | Of a variable, parameter or member, its type; of a function, what it returns; of a type, itself |
| `textDocument/implementation` | Of a prototype or function, its definitions (of a virtual method, every override); of a member or variable that holds functions, the functions stored in it |
| `textDocument/references` | References, honouring `context.includeDeclaration`; for a local, within its function |
| `textDocument/documentHighlight` | Each spelling of the name in this file where it means the same (kind 1, text) |
| `textDocument/hover` | Macros and enumerators with their definition and value; declarations; locals and parameters. With `alongside`, only the value |
| `textDocument/signatureHelp` | Trigger characters `(` `,`, retrigger `)` |
| `textDocument/completion` | Trigger characters `.` `>` `"` `<` `/` (members, and file names in `#include`); no resolve |
| `textDocument/documentSymbol` | Flat `SymbolInformation[]`, `containerName` the file |
| `workspace/symbol` | Up to 200, matched as [`si/search`](#sisearch) matches |
| `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls` | Calls through macros and function pointers included; `fromRanges` are empty ranges at the start of each line of a call |
| `textDocument/documentLink` | The file each `#include` reads |
| `textDocument/codeAction` | `quickfix`: the `#include` of the header that declares a name the file reads no declaration of |
| `textDocument/foldingRange` | Braces, `#if` branches, comments of more than a line, runs of `#include` (not with `alongside`) |
| `textDocument/semanticTokens/full` | Not with `alongside`. Legend: types `namespace`, `type`, `class`, `enum`, `struct`, `parameter`, `variable`, `property`, `enumMember`, `function`, `macro`; modifiers `declaration`, `static`, `readonly` |
| `textDocument/prepareRename`, `textDocument/rename` | Not with `alongside` |
| `textDocument/inlayHint` | The parameter name before each argument of a call (not with `alongside`) |

Lists of places are cut at 2000.

## `si/*` requests

### `si/search`

Fuzzy symbol search with scores: fragments in any order, initials, letters
in order. Unlike `workspace/symbol`, which an editor filters again by its
own rules, this returns the server's ranking.

```json
{"query": "uart snd", "limit": 200}
```

`limit` defaults to 100. The result is an array of [entries](#conventions),
best first, each with a `score` (a number; higher is better):

```json
[{"name": "uart_send", "kind": "function", "file": "src/uart.c", "line": 42, "endLine": 57, "declaration": false,
  "uri": "…", "range": {…}, "data": {…}, "score": 812}]
```

Symbols in files no compilation reads under the conditions in force are
left out.

### `si/context`

What the name under the cursor is defined as: one entry, for the view that
shows a definition.

Params: a [position](#conventions). Result: an [entry](#conventions) with
`candidates`, the number of definitions found (the entry is the first of
them); `null` when the cursor is on nothing the index knows; or
`{"inactive": [first, last]}`.

```json
{"name": "uart_send", "kind": "function", "file": "src/uart.c", "line": 42, "endLine": 57, "declaration": false,
 "uri": "…", "range": {…}, "data": {…}, "candidates": 1}
```

Definitions come before declarations: a declaration is returned only when
there is no definition. On a local or parameter, the local's entry with
`candidates: 1`.

### `si/relation`

The callers or callees of a symbol, each saying how the call comes about.

```json
{"textDocument": {"uri": "…"}, "position": {"line": 41, "character": 7}, "direction": "callers"}
{"item": {"file": "src/uart.c", "line": 42, "name": "uart_send"}, "direction": "callees"}
```

`direction` is `"callees"` for callees; anything else (the extension sends
`"callers"`) means callers. With `item` (the `data` of an entry), that
symbol is the root, and the position is not used: this is how a tree is
expanded one level at a time. Without it, the root is, in order:

1. a function or macro under the cursor (a definition before a prototype);
2. any other symbol under the cursor (a variable, type, member or
   constant): its callers are the places that use it, and its callees, for a
   member or variable that holds functions, the functions stored in it;
3. the function the cursor is in.

Result:

```json
{"root": {"name": "uart_send", "kind": "function", …},
 "entries": [{"name": "log_flush", "kind": "function", "file": "src/log.c", "line": 88, …,
              "lines": [95, 102], "viaMacro": "LOG_SEND", "through": null, "pointer": false,
              "certain": true, "onlyIf": [{"line": 90, "text": "#ifdef CONFIG_UART"}]}]}
```

| Field of an entry | |
|---|---|
| `lines` | The lines of the calls, from 1, in the calling function's file |
| `viaMacro` | The macro the call is written through, or `null` |
| `through` | The member or variable holding the function the call goes through, or `null` |
| `pointer` | `true` for a call through a pointer not otherwise named |
| `certain` | `false` when the index cannot settle a call (an undecided `#if`, a name with several definitions) |
| `onlyIf` | When every one of `lines` is in a branch the conditions in force cannot decide: the `#if`s that decide it, as `{line, text}` (line from 1); else `[]` |

With no root: `{"root": null, "entries": []}`. On a line switched off:
`{"inactive": [first, last]}`.

On a local or parameter (without `item`), `root` is the local's entry, and
for callers `entries` has one entry, its function, with `lines` the lines
it is used on, `viaMacro` and `through` `null`, `pointer` `false`,
`certain` `true` and no `onlyIf`. For callees `entries` is empty.

Calls from or through files that no compilation reads are left out. A root
that is a function or macro is called only from functions, macros and
variables (initializers); anything else may be used from anywhere.

### `si/references`

The references of the name under the cursor, by file, each with the text of
its line, from the index alone: no file has to be opened in the editor.

Params: a [position](#conventions), with an optional
`context.includeDeclaration` (default `true`), as `textDocument/references`.

```json
{"groups": [{"file": "src/uart.c", "uri": "file:///home/me/proj/src/uart.c",
             "items": [{"line": 41, "character": 4, "text": "int uart_send(const char *s)"}]}],
 "total": 1}
```

`line` and `character` are the start of the reference, from 0; `text` is
the whole line, trimmed. There is one group per file, the files in path
order and the items in line order. Or `{"inactive": [first, last]}`.

### `si/assignments`

Where functions are stored in the member or variable under the cursor; or,
on a function, where that function is stored: each a line of an
initializer or an assignment.

Params: a [position](#conventions).

```json
{"target": {"name": "send", "kind": "field", …},
 "by": "member",
 "rows": [{"member": {"name": "send", "kind": "field", …},
           "function": {"name": "uart_send", "kind": "function", …},
           "file": "src/board.c", "line": 12, "uri": "…", "range": {…},
           "inside": "board_ops"}]}
```

| Field | |
|---|---|
| `target` | The entry the rows are about, or `null` |
| `by` | `"member"`: the rows are what is stored in `target`. `"function"`: the rows are where `target` is stored. On a member or variable nothing is stored in, the server falls back to `"function"`. |
| `rows[].member`, `rows[].function` | Entries for the member or variable, and the function stored in it |
| `rows[].file`, `rows[].line` | Where, from 1 |
| `rows[].uri`, `rows[].range` | The function's name on that line |
| `rows[].inside` | The name of the symbol the line is in (a function, a variable's initializer), or `null` |

Or `{"inactive": [first, last]}`.

### `si/symbols`

A page of every symbol of the project, for a list that is fetched as it is
scrolled.

```json
{"offset": 0, "limit": 200, "order": "name"}
```

| Param | |
|---|---|
| `offset` | Where the page starts (default 0; past the end gives an empty page) |
| `limit` | How many (default 200, at most 2000) |
| `order` | `"name"` (the default): by name without regard to case. `"kind"`: function definitions, then prototypes, macros, structs, unions, enums, enumerators, typedefs, variable definitions, variable declarations, the rest; each by name |

```json
{"total": 48210, "offset": 0,
 "items": [{"name": "uart_init", "kind": "function", "declaration": false, "file": "src/uart.c", "line": 3}]}
```

`total` is the length of the whole list. Items have no `uri` or column: ask
[`si/place`](#siplace) for those when one is opened. File symbols and
symbols in files no compilation reads are not listed. The order is stable
until the index changes.

### `si/place`

Where exactly the name of a symbol defined on a line is written: a listing
like [`si/symbols`](#sisymbols) carries no columns.

```json
{"file": "src/uart.c", "line": 3, "name": "uart_init"}
```

`line` is from 1. The result is an LSP `Location`, on that line or one of
the two below it (a GNU-style definition has the name on the next line):

```json
{"uri": "file:///home/me/proj/src/uart.c", "range": {"start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 14}}}
```

### `si/locals`

The parameters and local variables of each function of a file.

```json
{"textDocument": {"uri": "file:///home/me/proj/src/uart.c"}}
```

```json
[{"function": "uart_send", "functionLine": 42, "name": "s", "line": 42, "kind": "parameter",
  "type": "char", "uri": "…", "range": {…}}]
```

`functionLine` and `line` are from 1; `kind` is `parameter` or `local`;
`type` is the type's name or `null`. A file outside the root, or not
indexed, gives `[]`.

### `si/inactive`

The lines of a file that the conditions in force do not compile.

```json
{"textDocument": {"uri": "file:///home/me/proj/src/uart.c"}}
```

```json
[[10, 14], [30, 30]]
```

Each pair is `[first, last]`, from 1, both included.

### `si/counterpart`

The header of a source file, or the source of a header: indexed files of the
same name (without regard to case) with the other kind of suffix, those in
the same folder first, then the nearest.

```json
{"textDocument": {"uri": "file:///home/me/proj/src/uart.c"}}
```

```json
["file:///home/me/proj/src/uart.h", "file:///home/me/proj/include/uart.h"]
```

Headers are `.h`, `.hpp`, `.hh`, `.hxx`, `.inl`; sources `.c`, `.cpp`,
`.cc`, `.cxx`, `.c++`.

### `si/conditions`

The sets of compile conditions the project has, which is in force, what that
one comes to macro by macro, and the build configurations found in the tree.
Params: `{}`.

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

| Field | |
|---|---|
| `configurations` | Every set, by name, as [Settings](#settings) (keys left out when empty or unset, as in the file) |
| `active` | The name of the set in force, or `null` |
| `automatic` | `true` when the sets are the tree's own build configurations, followed because the project names none |
| `available` | Build configurations found in the tree, each as a name and the `conditionFiles` entry that would follow it |
| `effective` | What the set in force comes to, macro by macro: `value` is `null` for not defined; `source` is the condition file it is from, or `""` for the set itself |
| `sourceDefines` | Whether what the sources define themselves is used across files (`false` only when the set says `"sourceDefines": false`) |
| `fromSources`, `fromSourcesTotal` | Macros whose value the sources settle, each with the number of files it is known in (at most 2000 listed), and how many there are |
| `files` | Files in the index |
| `includePaths` | Where the build looks for headers, as far as a `.cproject` among the condition files says |
| `headerDirs` | The folders that hold indexed headers, absolute (at most 4000) |
| `known` | How many macros the conditions settle, one way or the other |

### `si/setConditions`

Saves every set and which is in force into `.norn/config.json`, and indexes
again under it. The params are the whole of the
[conditions part of the file](#configurations): sets left out are removed.

```json
{"configurations": {"Debug": {"defines": {"DEBUG": "1"}}, "Release": {"conditionFiles": ["fw/.cproject#Release"]}},
 "activeConfiguration": "Debug"}
```

The result is that of [`si/sync`](#sisync), with `conditions`, the new
[`si/conditions`](#siconditions):

```json
{"include": [], "exclude": [], "config": "/home/me/proj/.norn/config.json",
 "files": 342, "symbols": 9120, "added": 0, "removed": 0, "changed": 0, "parsed": 57, "ms": 410.2,
 "conditions": {…}}
```

When the file cannot be written: `{"error": "could not save the conditions in <path>: <why>"}`.
When the params do not read as that object (a field of the wrong type,
say): `{"error": "the conditions are not as they should be: <why>"}`, and
nothing is saved.

Notes:

- `"configurations": {}` saves an empty list of sets: the project then has
  no conditions, and does not follow the tree's build configurations
  either.
- An `activeConfiguration` that is not among `configurations`, or none,
  leaves no set in force.
- The request waits while the index is still loading.

### `si/scope`

Which folders are indexed, and where that is written. Params: `{}`.

```json
{"include": ["src", "include"], "exclude": ["src/tests"], "config": "/home/me/proj/.norn/config.json"}
```

`config` is the path of the settings file, whether it exists yet or not.

### `si/setScope`

Sets which folders are indexed, saves it, and brings the index in line.

```json
{"include": ["src"], "exclude": ["src/tests"]}
```

A missing list is taken as empty; `{"include": [], "exclude": []}` is the
whole tree. The result is that of [`si/sync`](#sisync). When the file
cannot be written: `{"error": "could not save <path>: <why>"}`. The request
waits while the index is still loading.

### `si/sync`

Looks at the whole tree for what changed since the index was made (files
edited, added or deleted outside the editor, a scope file edited by hand)
and reads that. Params: `{}`.

```json
{"include": [], "exclude": [], "config": "/home/me/proj/.norn/config.json",
 "files": 342, "symbols": 9120, "added": 1, "removed": 0, "changed": 2, "parsed": 3, "ms": 35.0}
```

| Field | |
|---|---|
| `include`, `exclude`, `config` | As [`si/scope`](#siscope) |
| `files`, `symbols` | In the index now |
| `added`, `removed`, `changed` | Files |
| `parsed` | Files read |
| `ms` | Time taken to read and link |

The request waits while the index is still loading.

### `si/status`

What the index holds. Params: `{}`.

```json
{"files": 342, "symbols": 9120, "edges": 30211, "loaded": 340, "parsed": 2, "loading": false,
 "cache": "/home/me/proj/.norn/cache"}
```

| Field | |
|---|---|
| `files`, `symbols`, `edges` | In the index |
| `loaded`, `parsed` | At start: files read back from the last run's save, and files parsed (both 0 until [`si/ready`](#siready)) |
| `loading` | `true` until the index is there |
| `cache` | Where the index is kept between runs, or `null` with `--no-cache` |

### `si/mcp`

One message of the [Model Context Protocol](https://modelcontextprotocol.io),
answered from this index: what `norn mcp` answers (see
[the command line](cli.md#mcp)), for an editor that serves the index's tools
to agents from the server it already runs. Params: the message, as JSON-RPC
(`initialize`, `tools/list`, `tools/call`, `ping`, or a notification). The
result is the answer to it, as JSON-RPC, or `null` for a notification.

```json
{"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "find_callers", "arguments": {"name": "uart_send"}}}
```

A `tools/call` waits while the index is still loading; the rest do not.

## Notifications from the server

The server sends no diagnostics and no refresh requests. It sends two
notifications of its own.

### `si/progress`

How far indexing has got, sent at most every 150 ms while it runs, and once
more with `done` when it is over. It is sent for the first index and for
any indexing after it (a sync, a new scope or new conditions).

```json
{"phase": "parsing", "done": 1200, "total": 4800}
```

`phase` is `parsing`, `linking` or `done`. `done` and `total` are files
parsed out of how many; while linking they stay as parsing left them.

### `si/ready`

Sent once, when the index has been read or made after `initialize`.
Everything asked before then was answered from what the last session saved
(or not at all), so a client should ask again for what it shows.

```json
{"files": 342, "symbols": 9120, "edges": 30211, "loaded": 340, "parsed": 2}
```

`loaded`: files read back from the save; `parsed`: files parsed.

## `.norn/config.json`

The project's settings, under the root. (A `.si` folder from before is
renamed to `.norn` when it is first read.) The server writes it on
`si/setScope` and `si/setConditions`, each writing only its own keys and
leaving the rest of the file as it is; `norn live` and `norn index` read
the same file.

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

### Scope

`include` and `exclude` are folders relative to the root, `.` being the
root itself. For a file, the rule on its longest enclosing folder decides.
With no rule above it, a file is in when `include` is empty (the whole tree
is the project) and out when it is not (only those folders are).
`"exclude": ["."]` therefore leaves everything out but what `include`
brings back.

### Configurations

`configurations` maps a name to a set of [Settings](#settings);
`activeConfiguration` names the one in force. When the file has no
`configurations` key, the tree's own build configurations are followed (a
Kconfig build's `include/generated/autoconf.h` or `.config` first, then the
build descriptions found), unless `followBuildConfig` is `false`. A file
written before there were configurations, with the settings at the top
level, is read as one set named `Default`.

### Settings

| Key | Type | |
|---|---|---|
| `defines` | object | Names the build defines, with their values (`""` for a bare `#define`) |
| `undefines` | string[] | Names the build does not define |
| `assumeUndefined` | string[] | Prefixes (`CONFIG_`), or `*` for every name: such a name is not defined unless listed or defined in the sources |
| `conditionFiles` | string[] | Files to read more from: `#define`/`#undef` lines, a Kconfig `.config`; or a build description with the configuration after `#` (`fw/.cproject#Release`, `.vscode/c_cpp_properties.json#Linux`) |
| `sourceDefines` | boolean | `false`: macros the sources define are not taken as known outside the file that defines them |
| `undecided` | string | A branch on a macro nothing settles: `"both"` (the default), both branches indexed and marked as only possibly compiled; `"off"`, the macro taken as not defined |
| `notBuilt` | string[] | Folders the build does not compile: indexed and searched, but where a name or `#include` may mean what is there or elsewhere, it means elsewhere |
| `includeFirst` | string[] | Headers every compilation reads before its own file (a build's `-include`) |
| `kbuild` | boolean | The tree's Kbuild makefiles say which files the configuration builds; those they do not are not built |

What each one does is told at more length in the
[user guide](norn-guide.md#10-compile-conditions) and in
[`cond.rs`](../crates/si-core/src/cond.rs).

## Commands other VS Code extensions can call

The extension registers these commands, which take arguments or return
values, for other extensions and for tests
(`vscode.commands.executeCommand(...)`). Commands that only act on what the
user sees (switching a view's mode, locking it) are left out. Where a
command returns nothing, it returns once the work is done.

| Command | Arguments | Returns |
|---|---|---|
| `norn.open` | `{uri, line, character}` (line from 0), `beside: boolean` | Opens the place, in the editor group the views follow or beside it |
| `norn.showRelationAt` | `{textDocument: {uri}, position: {line, character}}` | Focuses the Relation view, moves the cursor there in a visible editor of that document, unlocks the view and shows that place. Does nothing to the cursor when no editor shows the document |
| `norn.relation.target` | none | The position the Relation view shows, as `{textDocument, position}`, or `undefined` |
| `norn.relation.referencesAt` | `{textDocument: {uri}, position}` | `{groups, message}`: the groups of [`si/references`](#sireferences) (empty on a switched-off line) and the summary the view shows |
| `norn.relation.showReference` | `{uri, line, character}` (line from 0) | Shows that place in the Context view, as a definition is shown, even when the view is locked |
| `norn.context.shown` | none | `{uri, line, lines, colors}` of what the Context view last showed (`line` from 1, `lines` the file's line count, `colors` the theme's token colors by scope), or `undefined` |
| `norn.fileSymbols.tree` | none | What the File Symbols view lists: `[{label, detail, children}]` |
| `norn.serverInUse` | none | The path (or command name) of the server the extension starts |
| `norn.search.ask` | `text?: string` | Focuses the Search view and searches for `text`. Without a string, asks for it, offering the word under the cursor |
| `norn.conditions.switch` | `arg?: string` | A string that is the `source` of an `available` build configuration: follows it in a new set. The name of a set: puts it in force. Any other string: a new empty set of that name, in force. Without a string, a quick pick |
| `norn.conditions.delete` | `name: string` | Removes that set |
| `norn.conditions.define` | `"NAME"` or `"NAME=value"` | Defines it in the set in force (and takes it off `undefines`). Without a string, asks |
| `norn.conditions.undefine` | `name: string` | Marks it not defined in the set in force. Without a string, asks |
| `norn.conditions.addFile` | `path: string` | Adds a condition file to the set in force. Without a string, a file dialog |
| `norn.conditions.assume` | `prefix: string` | Adds to `assumeUndefined`. Without a string, asks |
| `norn.conditions.get` | none | The set in force as [Settings](#settings) (`defines`, `undefines`, `assumeUndefined` and `conditionFiles` always there), with `state`, the last [`si/conditions`](#siconditions) |
| `norn.scope.include`, `norn.scope.exclude` | a folder: a path relative to the root, or a `vscode.Uri` | Includes or excludes that folder (with any pending change in the Scope view) and indexes |
| `norn.scope.only` | the same | Indexes only that folder |
| `norn.scope.pending` | none | `true` when the Scope view has changes not applied yet |

The commands that change conditions send every set as last read, with the
change made to the one in force; keys they do not touch (`notBuilt`,
`includeFirst` and the like) are sent as they were.
