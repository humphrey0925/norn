# site/install.ps1 as `irm ... | iex` runs it, under the PowerShell named
# (pwsh, or Windows PowerShell 5.1: powershell), in a home folder of its
# own, with this build's norn.exe (target/release) served as a release
# would be: installed, checked and set up for Codex and OpenCode; and
# refused, with nothing changed, when the checksum does not match.
#
#   pwsh -File release/test-install.ps1 -Shell powershell
param([string]$Shell = "pwsh")
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
$asset = "norn-win32-$arch.exe"
$work = Join-Path ([IO.Path]::GetTempPath()) ("norn-install-" + [Guid]::NewGuid())
$release = Join-Path $work "release"
$home_ = Join-Path $work "home"
New-Item -ItemType Directory -Force -Path $release, (Join-Path $home_ ".codex"), (Join-Path $home_ ".config\opencode") | Out-Null
Copy-Item target\release\norn.exe (Join-Path $release $asset)
$hash = (Get-FileHash -Algorithm SHA256 (Join-Path $release $asset)).Hash.ToLower()
Set-Content -NoNewline -Path (Join-Path $release "SHA256SUMS") -Value "$hash  $asset`n"
$port = Get-Random -Minimum 20000 -Maximum 40000
$server = Start-Process python -ArgumentList "-m", "http.server", $port, "--bind", "127.0.0.1", "--directory", $release -PassThru -WindowStyle Hidden
function Fail($why) { throw "test-install: $why" }

try {
  foreach ($i in 1..50) {
    try { Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$port/SHA256SUMS" | Out-Null; break } catch { Start-Sleep -Milliseconds 200 }
  }
  $env:USERPROFILE = $home_
  $env:NORN_DOWNLOAD_URL = "http://127.0.0.1:$port/"
  $script = (Resolve-Path site\install.ps1).Path
  function Install {
    & $Shell -NoProfile -ExecutionPolicy Bypass -Command "Get-Content -Raw '$script' | Invoke-Expression" | Out-Host
    return $LASTEXITCODE
  }

  if ((Install) -ne 0) { Fail "install.ps1 failed under $Shell" }
  $exe = Join-Path $home_ ".norn\bin\norn.exe"
  if ((& $exe --version) -ne (& target\release\norn.exe --version)) { Fail "the installed norn.exe is not this build's" }
  if (-not (Select-String -Quiet -Pattern '^\[mcp_servers\.norn\]' (Join-Path $home_ ".codex\config.toml"))) { Fail "no [mcp_servers.norn] in Codex's config" }
  if (-not (Select-String -Quiet -SimpleMatch '"norn"' (Join-Path $home_ ".config\opencode\opencode.json"))) { Fail "no norn in OpenCode's config" }
  if (-not (Test-Path (Join-Path $home_ ".agents\skills\norn\SKILL.md"))) { Fail "no skill in ~/.agents/skills/norn" }

  $before = (Get-FileHash $exe).Hash
  Set-Content -NoNewline -Path (Join-Path $release "SHA256SUMS") -Value ("0" * 64 + "  $asset`n")
  if ((Install) -eq 0) { Fail "installed although the checksum did not match" }
  if ((Get-FileHash $exe).Hash -ne $before) { Fail "the norn.exe there was changed although the checksum did not match" }
  Write-Host "test-install: install.ps1 under $Shell installs, sets up, and refuses a checksum that does not match"
} finally {
  Stop-Process -Id $server.Id -ErrorAction SilentlyContinue
  Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
