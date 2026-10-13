# Prebuilt files

- `si-code-index-0.1.0.vsix`: the VS Code extension. Install with
  `code --install-extension si-code-index-0.1.0.vsix`, or in VS Code:
  Extensions, the `...` menu, "Install from VSIX...".
- `si-windows-x64.exe`: the `si` index server for 64-bit Windows, which the
  extension starts. Set `si.serverPath` in VS Code's settings to where you
  put it. On another system, build it with `cargo build --release`.

Built from commit 8e1631f.
