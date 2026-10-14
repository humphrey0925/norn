# SI Code Index

Source-Insight-style views for C in VS Code, backed by the `si` index.

- **專案符號**: fuzzy search over every symbol. Fragments in any order
  (`init cache`), initials (`bci`), or letters in order. Ctrl+click opens
  beside.
- **本檔符號**: the symbols of the file in front of you, by kind.
- **Context**: the definition of whatever the cursor is on, without leaving
  where you are.
- **Relation**: callers, callees or references of the function at the cursor,
  expandable level by level. Each call says how it comes about: by name,
  through a macro, through a function pointer, or uncertain.
- A line above each function with its number of callers and callees
  (`si.codeLens.enabled`).

- **Index Scope**: the folders of the workspace with a checkbox each;
  checked folders are indexed. The choice is kept in `.si/config.json` in
  the workspace and is there the next time. The sync button brings the
  index in line with the disk whenever you want it.

- **Context** shows the definition of the name under the cursor in its
  file: the whole file, at the definition's line, to scroll through and
  read but not to edit, coloured as the theme colours code. Hovering a
  name there shows what the editor's hover would (from every language
  extension installed, or the definition from the index when none
  answers); Ctrl+click goes to its definition, a click marks its other
  occurrences, and a line number opens that line in the editor.
- **Parameters and local variables** are listed under their function in
  File Symbols (`si.fileSymbols.showLocals`), and the cursor on one gives
  its declaration in Context and by Go to Definition, its uses by Find
  References, and in Relation the function it belongs to with the lines
  it is used on. They are a function's own: Project Symbols does not list
  them.
- **Lock and sync**: Relation and Context follow the cursor until locked.
  A locked view stays on its symbol while you read elsewhere; its target
  button moves it to what the cursor is on now, still locked.
- **Compile Conditions**: what the build defines, so that `#if` branches
  it does not compile are left out. A project can have several sets, as a
  build has configurations (Debug, Release, one per board), with one in
  force: it is named in the status bar, and a click there switches. The
  view lists what the set in force comes to, macro by macro with its
  value, under what says so.
  - A set can *follow* a build configuration found in the workspace: a
    DS-5 / Arm Development Studio / Eclipse CDT `.cproject`, or the
    `c_cpp_properties.json` of VS Code's C/C++ extension, or a
    `compile_commands.json` (what every command in it defines). Its
    defines are then read from there, and Sync picks up changes made there.
  - Nothing has to be set up for that: while the project has no sets of
    its own, the build configurations found in it are its sets, the first
    one in force (the status bar says which, and a click switches).
  - Macros can also be set by hand (they have the last word), read from a
    header of `#define` lines or a Kconfig `.config`, and names with a
    prefix, or all names, can be assumed not defined unless the build or
    the sources define them.
  - The set in force can be passed on to VS Code's C/C++ extension
    (`si.conditions.updateCppProperties`, or "Write Compile Conditions to
    the C/C++ Extension" in the view's menu): a configuration named `SI`
    in `.vscode/c_cpp_properties.json` gets its defines and, from a
    `.cproject`, its include paths, and is rewritten when the set changes
    or another is switched to. Select `SI` in the C/C++ extension once.
  - What the sources define themselves counts as well, without being
    set: a `#define` settles the name for the rest of its file, and a
    name is known in a file when every source file that file is compiled
    as part of certainly includes a definition of it, all to one value (a
    switch in a configuration header everything includes). "From the
    sources" lists those, with the number of files each is known in, and
    can be turned off there; anything set by hand overrules them.
  - Kept in `.si/config.json` with the scope.

Go to definition, find references, workspace symbols and call hierarchy use
the same index.

With the editor split, the views follow the editor that last had focus and
results open in its group; the lock button on Relation and Context stops them
following.

## Setup

The packaged extension for Windows x64 has the `si` server inside it:
install the `.vsix` and open a folder. Elsewhere, build `si`
(`cargo build --release` at the repository root) and point `si.serverPath`
at the executable, or put it on `PATH`; a path set there is always used in
place of the built-in server. The index is built when a folder with C
files is opened, kept in `.si/cache`, and updated when files are saved.

`si.includeAliases` tells the index which template a generated header comes
from, for example `bfd.h=bfd-in2.h`.

## Development

    npm install
    npm run compile
    npm test        # runs the extension in a real VS Code on a scratch copy
