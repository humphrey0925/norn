# Releases

Norn was called SI (`si-code-index`) up to 0.18.1. Uninstall that
extension when installing this one; a workspace's `.si` folder and
`si.*` settings are taken over as `.norn` and `norn.*` the first time.

One archive per version: `norn-<version>-windows-x64.7z`. Extract it over the
last one; the file names inside stay the same apart from the extension's
version.

- `norn-<version>.vsix`: the VS Code extension, with the index
  server for 64-bit Windows inside it. Install with
  `code --install-extension norn-<version>.vsix`, or in VS Code:
  Extensions, the `...` menu, "Install from VSIX...". Nothing else to set.
- `norn-windows-x64.exe`: the same `norn` server on its own, for the command
  line and for other tools (`norn index`, `norn query`, `norn lsp`). The
  extension does not need it. If `norn.serverPath` is set in VS Code it is
  used in place of the built-in server; leave it empty to use the one
  inside the extension.

On another system, build the server with `cargo build --release` and point
`norn.serverPath` at it.

`make.sh` builds both and packs the archive.
