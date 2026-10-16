# Norn

Source-Insight-style views for C in VS Code, backed by the `norn` index.

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
  (`norn.codeLens.enabled`).

- **Index Scope**: the folders of the workspace with a checkbox each;
  checked folders are indexed. The choice is kept in `.norn/config.json` in
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
  File Symbols (`norn.fileSymbols.showLocals`), and the cursor on one gives
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
    `c_cpp_properties.json` of VS Code's C/C++ extension, a
    `compile_commands.json` (what every command in it defines), a Keil
    µVision project (`.uvprojx`), an IAR one (`.ewp`), a Visual Studio
    one (`.vcxproj`), `platformio.ini`, `compile_flags.txt`, or a
    Makefile or `CMakeLists.txt`. They are looked for in the workspace
    folder and up to three folders down. Its defines are then read from
    there, and Sync picks up changes made there.
  - Of a Makefile or `CMakeLists.txt` only what is said outright is
    taken: a `-D` outside every `ifdef`/`if ()`, with a value that is
    written, not computed. Neither is run. That is less than the build
    defines; a project file or a compilation database says more, and is
    preferred when there is one.
  - Nothing has to be set up for that: while the project has no sets of
    its own, the build configurations found in it are its sets, the first
    one in force (the status bar says which, and a click switches).
  - Macros can also be set by hand (they have the last word), read from a
    header of `#define` lines or a Kconfig `.config`, and names with a
    prefix, or all names, can be assumed not defined unless the build or
    the sources define them.
  - VS Code's C/C++ extension is given the set in force as it changes:
    Norn is a *configuration provider* for it, so IntelliSense takes each
    file's defines (and the folders that hold headers) from Norn, and is
    told the moment a macro is added, a set is switched, or
    `.norn/config.json` or the build's own description is edited. The C/C++
    extension asks once whether Norn may configure the folder (or set
    `"C_Cpp.default.configurationProvider": "humphrey0925.norn"`).
    Nothing is written for this.
  - Without that, the set in force can be passed on to VS Code's C/C++ extension
    (`norn.conditions.updateCppProperties`, or "Write Compile Conditions to
    the C/C++ Extension" in the view's menu): a configuration named `Norn`
    in `.vscode/c_cpp_properties.json` gets its defines and, from a
    `.cproject`, its include paths, and is rewritten when the set changes
    or another is switched to. Select `Norn` in the C/C++ extension once.
  - What the sources define themselves counts as well, without being
    set: a `#define` settles the name for the rest of its file, and a
    name is known in a file when every source file that file is compiled
    as part of certainly includes a definition of it, all to one value (a
    switch in a configuration header everything includes). "From the
    sources" lists those, with the number of files each is known in, and
    can be turned off there; anything set by hand overrules them.
  - Kept in `.norn/config.json` with the scope.

Go to definition, find references, workspace symbols and call hierarchy use
the same index.

With the editor split, the views follow the editor that last had focus and
results open in its group; the lock button on Relation and Context stops them
following.

## Setup

The extension has the `norn` server inside it, built for the system it is
installed on: Windows (x64, Arm64), Linux (x64, Arm64; static, any
distribution) and macOS (Intel, Apple silicon). Install it and open a
folder. To use a server of your own instead, build `norn`
(`cargo build --release` at the repository root) and point
`norn.serverPath` at the executable; a path set there is always used in
place of the built-in server. The index is built when a folder with C
files is opened, kept in `.norn/cache`, and updated when files are saved.

`norn.includeAliases` tells the index which template a generated header comes
from, for example `bfd.h=bfd-in2.h`.

## Development

    npm install
    npm run compile
    npm test        # runs the extension in a real VS Code on a scratch copy

## License

Apache License 2.0; see `LICENSE` and `NOTICE`. What Norn includes of
others' work is listed with its licenses in `THIRD_PARTY_NOTICES.txt`.
