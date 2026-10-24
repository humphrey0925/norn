# Norn for AI agents, on Windows:
#
#   irm https://norn-c.com/install.ps1 | iex
#
# Downloads norn.exe for this machine from the latest release on GitHub,
# checks it against the release's SHA256SUMS, puts it at
# %USERPROFILE%\.norn\bin\norn.exe, and runs `norn setup`: Claude Code,
# Codex and OpenCode found on this machine get Norn's tools and its skill.
#
#   $env:NORN_VERSION = "0.26.0"   a release other than the latest
#   $env:NORN_NO_SETUP = "1"       put the executable there, set no agent up
#   $env:NORN_DOWNLOAD_URL = "..."  a folder of a release's files other than
#                                  GitHub's (a mirror), SHA256SUMS among them
#
# (All in a block of its own: with `iex`, settings made at the top level
# would stay in the shell it was run from.)
& {
  $ErrorActionPreference = "Stop"
  $ProgressPreference = "SilentlyContinue"
  # Windows PowerShell 5.1 on an older .NET offers no TLS 1.2 unless told to.
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  $repo = "https://github.com/humphrey0925/norn/releases"
  $arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64" -or $env:PROCESSOR_ARCHITEW6432 -eq "ARM64") { "arm64" } else { "x64" }
  $base = if ($env:NORN_DOWNLOAD_URL) { $env:NORN_DOWNLOAD_URL.TrimEnd('/') } elseif ($env:NORN_VERSION) { "$repo/download/v$($env:NORN_VERSION.TrimStart('v'))" } else { "$repo/latest/download" }
  $asset = "norn-win32-$arch.exe"

  $dir = Join-Path $env:USERPROFILE ".norn\bin"
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  $exe = Join-Path $dir "norn.exe"
  $tmp = Join-Path $dir ".norn-download.exe"

  Write-Host "norn: downloading $asset from $base"
  Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $tmp
  $sums = (Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS").Content
  if ($sums -is [byte[]]) { $sums = [Text.Encoding]::UTF8.GetString($sums) }
  $want = ($sums -split "`n" | Where-Object { ($_ -split "\s+")[1] -eq $asset } | ForEach-Object { ($_ -split "\s+")[0] }) | Select-Object -First 1
  $got = (Get-FileHash -Algorithm SHA256 $tmp).Hash.ToLower()
  if (-not $want -or $want -ne $got) {
    Remove-Item -Force $tmp
    throw "norn: $asset does not match the release's SHA256SUMS; nothing installed"
  }

  # (A running copy can be renamed away, not written over.)
  if (Test-Path $exe) {
    Remove-Item -Force "$exe.old" -ErrorAction SilentlyContinue
    Move-Item -Force $exe "$exe.old"
  }
  Move-Item -Force $tmp $exe
  Remove-Item -Force "$exe.old" -ErrorAction SilentlyContinue
  Write-Host "norn: $(& $exe --version) is at $exe"

  if ($env:NORN_NO_SETUP) { return }
  & $exe setup --help *> $null
  if ($LASTEXITCODE -eq 0) {
    & $exe setup
  } else {
    Write-Host "norn: this release has no ``norn setup`` yet; set the agents up by hand: https://github.com/humphrey0925/norn/blob/master/docs/agents.md"
  }
}
