# Norn

Source-Insight-style code navigation for C (and C++) in VS Code, backed by
`norn`, an index and language server written in Rust. No compile step and
no compile database needed: it reads the sources, works out which `#if`
branches the project's build compiles, and resolves calls through macros
and function pointers.

- `crates/si-core`: the index (tree-sitter parsing, the preprocessor's
  conditions, linking, the stored graph).
- `crates/si-cli`: the `norn` executable: `norn lsp` (the language server),
  `norn index`, `norn query`.
- `vscode/`: the VS Code extension. See [vscode/README.md](vscode/README.md)
  and the user guide, [docs/norn-guide.md](docs/norn-guide.md) (繁體中文).

## Building

    cargo build --release          # target/release/norn
    cd vscode && npm ci && npm run compile

Releases are built by GitHub Actions for Windows, Linux and macOS
(`.github/workflows/release.yml`); `node release/package.js <platform>
<server>` packages the extension for one platform.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Contributions are made under the
[Contributor License Agreement](CLA.md).

## License

Copyright 2026 humphrey0925 and the Norn contributors. Licensed under the
[Apache License, Version 2.0](LICENSE).
