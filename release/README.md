# Prebuilt files

- `si-code-index-0.8.0.vsix`: the VS Code extension. Install with
  `code --install-extension si-code-index-0.8.0.vsix`, or in VS Code:
  Extensions, the `...` menu, "Install from VSIX...".
- `si-windows-x64.exe`: the `si` index server for 64-bit Windows, which the
  extension starts. Set `si.serverPath` in VS Code's settings to where you
  put it. On another system, build it with `cargo build --release`.

Both are from the same commit; the extension is version 0.8.0 and needs this server (older ones do not keep the index).
