# Releases

One archive per version: `si-<version>-windows-x64.7z`. Extract it over the
last one; the file names inside stay the same apart from the extension's
version.

- `si-code-index-<version>.vsix`: the VS Code extension. Install with
  `code --install-extension si-code-index-<version>.vsix`, or in VS Code:
  Extensions, the `...` menu, "Install from VSIX...".
- `si-windows-x64.exe`: the `si` index server for 64-bit Windows, which the
  extension starts. Set `si.serverPath` in VS Code's settings to where you
  put it. On another system, build it with `cargo build --release`.

Both are from the same commit and belong together: an extension needs the
server it came with.

`make.sh` builds both and packs the archive.
