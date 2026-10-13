# Releases

One archive per version: `si-<version>-windows-x64.7z`. Extract it over the
last one; the file names inside stay the same apart from the extension's
version.

- `si-code-index-<version>.vsix`: the VS Code extension, with the index
  server for 64-bit Windows inside it. Install with
  `code --install-extension si-code-index-<version>.vsix`, or in VS Code:
  Extensions, the `...` menu, "Install from VSIX...". Nothing else to set.
- `si-windows-x64.exe`: the same `si` server on its own, for the command
  line and for other tools (`si index`, `si query`, `si lsp`). The
  extension does not need it. If `si.serverPath` is set in VS Code it is
  used in place of the built-in server; leave it empty to use the one
  inside the extension.

On another system, build the server with `cargo build --release` and point
`si.serverPath` at it.

`make.sh` builds both and packs the archive.
