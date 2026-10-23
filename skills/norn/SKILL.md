---
name: norn
description: "C/C++ code: find definitions, callers, callees and uses with Norn, not grep. Set the build configuration first."
---

# Navigating C and C++ with Norn

Norn has indexed this project: definitions, references and the call graph,
worked out without a build, under the `#if` branches a build configuration
compiles. Its MCP tools answer in text, with paths relative to the project.

Grep finds where a name is written. It does not find:

- calls a macro writes (`SEND(x)` that expands to `uart_send(x)`);
- calls through a function pointer kept in a struct member or a variable
  (`dev->ops->write(...)`, `handlers[i](...)`);
- which of several definitions of a name a given call reaches;
- whether a line is compiled at all, in this board's or this target's build.

So for any question about a C/C++ symbol, ask Norn first. And read and
search C/C++ files through Norn too: `read_file` rather than your own file
reader, `search_code` rather than grep. Code in a switched-off `#if` branch
looks like any other code in a plain read or grep; these mark it (`✗`), or
leave it out. Plain tools are for everything else: build files, other
languages, documentation.

## 1. Set the context first

The build configuration decides what the code is: which `#if` branches are
compiled, so which definitions exist, which calls happen, which lines run.
Answers under the wrong configuration look right and are wrong.

1. Call `list_configurations`. It says which configuration is in force, the
   project's configurations, those found in its build files (Keil, IAR,
   Eclipse, Visual Studio, CMake, Makefile, Kconfig, PlatformIO...), and what
   the one in force defines.
2. Work out which one the task is about: a board, chip, target, product or
   build type the user names, or that the files you are working on belong
   to (a `boards/stm32f4/` folder, a `#ifdef CONFIG_X` around the code in
   question). If unsure and it matters, ask the user.
3. If it is not the one in force, call `switch_configuration` with its name.
   It is the project's setting, shared with the user's editor and other
   agents: say which one you chose and why.
4. With no configuration, a branch nothing settles is indexed both ways.
   Results under it are marked `[maybe not compiled: ...]`; do not take them
   as certain.

## 2. Then find, don't grep

| To find | Use |
|---|---|
| A symbol whose exact name you do not know | `search_symbols` (fragments in any order, initials) |
| Where a name is defined and declared | `find_definition`; with the `file` and `line` where it is used, the definition that place sees |
| Its code | `read_symbol`: the definition with its comment, not the whole file |
| A C/C++ file, or some of its lines | `read_file` (`from`, `to`; `hide_inactive` folds what is not compiled) |
| Text in C/C++ code: a string, a log message, a register name, a pattern | `search_code` (`regex`, `in` a folder): only what is compiled; `include_inactive` for the rest |
| Who calls a function | `find_callers` |
| What a function calls | `find_callees` |
| Every use of a symbol (reads, writes, stores of a function) | `find_references` |
| What a function reaches, or what reaches it, a few levels deep | `call_tree` (`direction`: `callees` or `callers`) |
| Whether a line, or which lines of a file, are compiled | `compiled_lines` |

Typical paths:

- **Understand a function**: `find_definition` → `read_symbol` →
  `find_callees` → `read_symbol` on the callees that matter.
- **Impact of a change**: `find_callers` (and `call_tree` with
  `direction: callers`) before changing a function's behaviour or signature.
  Every caller listed, including those through macros and function pointers,
  is one to check.
- **Where a callback is called from**: `find_references` on the function
  shows where it is stored (`stored in member ...`); `find_callers` shows the
  calls through that member.
- **Before editing inside `#if`**: `compiled_lines` with the file and line,
  or look at the marks `read_file` gives. Do not edit, or reason from, a
  branch the configuration switches off, unless the task is about that
  other configuration (then switch to it).

## 3. Reading the answers

- `— via macro X`: the call is written by macro `X` where it is used.
- `— through member send, which can hold it`: a call through a function
  pointer; the function listed is one that is stored there somewhere. Check
  `find_references` to see where it is stored, if it matters which.
- `— through a pointer that can hold it`: through a local pointer variable.
- `[uncertain]`: the index cannot settle the call: several definitions could
  be meant.
- `[maybe not compiled: under the #if at line N]`: the line is under an
  `#if` the configuration in force does not settle. Choose a configuration
  that settles it, if the task depends on it.
- `[in a file the build does not compile]`: no compilation of the build
  reads that file.
- In `read_file`, `search_code` and `read_symbol`, a line marked `✗` is not
  compiled under the configuration in force: the code is there, but this
  build does not have it. `?` marks a line under an `#if` the configuration
  does not settle.
- A name with several definitions (one per board, a `static` function in
  several files) answers for each; pass `file` to choose one.
- `Listed N of M`: pass a larger `limit`, or narrow with `file`.

## 4. When Norn cannot help

- A tool says no symbol is indexed by that name: check the spelling with
  `search_symbols`; then it may be generated code, assembly, or outside the
  project's folders: `search_code` for its text.
- A tool says Norn does not know which project to index: the agent started
  it outside the project. Tell the user to give the project's folder in the
  agent's MCP settings (`norn mcp /path/to/project`), or run `norn setup`
  again from a newer Norn.
- The Norn tools are not available at all: say so to the user; do not
  silently fall back to grep for call relations, since it misses those
  through macros and function pointers.
