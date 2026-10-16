# Contributing to Norn

Norn is licensed under the [Apache License 2.0](LICENSE). Contributions are
accepted under the [Norn Contributor License Agreement](CLA.md): the first
time you open a pull request, the CLA check asks you to sign it by
commenting on the pull request with

> I have read the CLA Document and I hereby sign the CLA

Once is enough for every later pull request.

Norn 採用 Apache License 2.0 授權。第一次送 pull request 時，CLA 檢查會請你在
PR 下留言上面那一句，表示同意 [CLA](CLA.md)：你保有自己貢獻的著作權，並授權本專案以
Apache-2.0 散布，也授權相關專利。簽一次之後的 PR 都不需要再簽。

## Building

- The server: `cargo build --release` (Rust stable). It builds on Windows,
  Linux and macOS; the binary is `target/release/norn`.
- The extension: in `vscode/`, `npm ci` and `npm run compile`.

## Testing

- `cargo test` runs the index's own tests; they need nothing else.
- `cd vscode && npm test` runs the extension in VS Code against a few files
  of binutils (`corpus/c-only`, not in the repository). CI fetches them with
  `release/fetch-test-corpus.sh`; run it once locally too.
- On Linux the VS Code tests need a display: `xvfb-run -a npm test`.

Before a pull request: `cargo clippy --all-targets` without warnings, and
the tests above passing.
