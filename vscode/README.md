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

Go to definition, find references, workspace symbols and call hierarchy use
the same index.

With the editor split, the views follow the editor that last had focus and
results open in its group; the lock button on Relation and Context stops them
following.

## Setup

Build `si` (`cargo build --release` at the repository root) and point
`si.serverPath` at the executable, or put it on `PATH`. The index is built in
memory when a folder with C files is opened and updated when files are saved.

`si.includeAliases` tells the index which template a generated header comes
from, for example `bfd.h=bfd-in2.h`.

## Development

    npm install
    npm run compile
    npm test        # runs the extension in a real VS Code on a scratch copy
