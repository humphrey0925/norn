# Norn

**English** | [繁體中文](README.zh-TW.md)

**Source Insight-style code navigation for large C codebases, in VS Code.**
Norn indexes a source tree without building it: no compilation database and
no configuration. It follows calls through macros and function pointers, and it
works out which `#if` branches the project's build compiles.

![Norn in VS Code](vscode/media/screenshots/overview.png)

## Why Norn

Language servers built on a compiler (clangd, IntelliSense) need to know exactly
how each file is compiled. For a lot of real C that is hard to give them:
firmware built with a proprietary toolchain, SDKs with a header per chip,
code that only builds on one machine. Norn reads the sources the way a person
would, and the build's own description where there is one.

- **Fast**: the whole Linux kernel indexes in under a minute on a desktop PC;
  saving a file updates the index in tens of milliseconds; a workspace reopens
  instantly from its cache.
- **Accurate**: measured call by call against clang on public projects
  (Redis: 99.3% of calls found, 99.8% precision; STM32F4 HAL: 96.5% found,
  100% precision).
- **Local**: no network connections, no data collected.

## What it does

- **Project Symbols**: fuzzy search over every symbol in the workspace.
- **Relation**: callers, callees, references and function-pointer
  assignments, as a tree or a graph, each call marked as direct, through a
  macro, through a function pointer, or under an undecided `#if`.
- **Context**: the definition of the name under the cursor, beside your code.
- **File Symbols**: the current file's symbols, with each function's
  parameters and locals.
- **Compile conditions**: read from Arm DS-5 `.cproject`, Keil, IAR, Visual
  Studio, `compile_commands.json`, PlatformIO, CMake and Makefiles; code the
  build does not compile is left out and dimmed.
- **A full C language server**: definitions, references, call hierarchy,
  hover with macro values, completion, signature help, semantic highlighting,
  rename, inlay hints, folding and formatting. Alongside Microsoft's C/C++
  extension it acts as its configuration provider instead of duplicating it.

See the extension's page, [vscode/README.md](vscode/README.md), for the
details and screenshots, and the user guide, [docs/norn-guide.md](docs/norn-guide.md)
(in Traditional Chinese).

## Install

Install **Norn — Code Navigation & Symbol Index** from the Visual Studio
Marketplace or Open VSX, or a `.vsix` from the
[releases](https://github.com/humphrey0925/norn/releases). The server is
included for Windows, Linux and macOS (x64 and Arm64). Open a folder with C
sources; nothing else to set up.

## Building from source

Requires Rust (stable) and Node.js 20.

    cargo build --release                  # the server: target/release/norn
    cd vscode && npm ci && npm run compile  # the extension

`node release/package.js <platform> <server>` packages the extension for one
platform, for example `node release/package.js linux-x64 target/release/norn`.
Releases are built and tested on Windows, Linux and macOS by GitHub Actions;
see [docs/releasing.md](docs/releasing.md).

| Path | |
|---|---|
| `crates/si-core` | The index: parsing (tree-sitter), preprocessor conditions, linking, the stored graph |
| `crates/si-cli` | The `norn` executable: `norn lsp` (the language server), `norn index`, `norn query` |
| `vscode/` | The VS Code extension |
| `bench/` | Accuracy and speed measurements against clang and other indexers |

## Contributing

Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).
Contributions are accepted under the [Contributor License Agreement](CLA.md),
which the CLA check asks you to sign on your first pull request.

## License

Copyright 2026 humphrey0925 and the Norn contributors. Licensed under the
[Apache License, Version 2.0](LICENSE).
