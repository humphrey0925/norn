# Prebuilt files

- `si-code-index-0.2.0.vsix`: the VS Code extension. Install with
  `code --install-extension si-code-index-0.2.0.vsix`, or in VS Code:
  Extensions, the `...` menu, "Install from VSIX...".
- `si-windows-x64.exe`: the `si` index server for 64-bit Windows, which the
  extension starts. Set `si.serverPath` in VS Code's settings to where you
  put it. On another system, build it with `cargo build --release`.

The server is built from commit 8e1631f; the extension is version 0.2.0.
