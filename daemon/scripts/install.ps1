# modelstat installer for Windows (feature §3).
#
#   irm https://modelstat.ai/install.ps1 | iex
#
# Downloads the two static binaries (collector `modelstat` + summariser engine
# `modelstat-summarizer`) from GitHub Releases, verifies their SHA256, stages
# them under %USERPROFILE%\.modelstat\bin, then runs `modelstat connect` (pair +
# service + MCP wiring). No Node.js, no npm. Every step prints what it does.

[CmdletBinding()]
param(
  [ValidateSet('daemon', 'summarizer')][string]$Component = 'daemon',
  [switch]$System,
  [string]$Version = '',
  [ValidateSet('cloud', 'local', 'self-hosted', '')][string]$Mode = '',
  [string]$Url = '',
  [switch]$Yes,
  [switch]$NoBrowser,
  [switch]$NoAutoUpdate
)

$ErrorActionPreference = 'Stop'
$Repo = 'modelstat/modelstat'
$HomeDir = if ($env:MODELSTAT_HOME) { $env:MODELSTAT_HOME } else { Join-Path $env:USERPROFILE '.modelstat' }

function Step($m) { Write-Host "`n> $m" -ForegroundColor Cyan }
function Ok($m) { Write-Host "  + $m" -ForegroundColor Green }
function Die($m) { Write-Host "  x $m" -ForegroundColor Red; Write-Host "  Help: https://modelstat.ai/install"; exit 1 }

Write-Host "`n  modelstat installer" -ForegroundColor Cyan
Write-Host "  https://modelstat.ai"

# ── target triple ──────────────────────────────────────────────────
$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
  'AMD64' { 'x86_64' }
  'ARM64' { 'aarch64' }
  default { Die "unsupported CPU architecture: $($env:PROCESSOR_ARCHITECTURE)" }
}
$triple = "$arch-pc-windows-msvc"

# ── replace an existing daemon (legacy Node OR a previous install) ──
# Ours registers the same scheduled-task name, so it would replace the old one
# anyway — but end it explicitly so the handover is visible and two daemons never
# race the same home dir. %USERPROFILE%\.modelstat is KEPT (the device identity),
# so the new daemon continues as the SAME device.
if (schtasks /Query /TN 'modelstat' 2>$null) {
  Step 'Found an existing modelstat daemon - replacing it'
  schtasks /End /TN 'modelstat' 2>$null | Out-Null
  Ok "stopped the old daemon (your device pairing in $HomeDir is kept)"
}
if ((Test-Path (Join-Path $HomeDir 'bin\modelstat.mjs')) -or (Test-Path (Join-Path $HomeDir 'bin\node_modules'))) {
  Step 'Migrating off the old Node install'
  Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $HomeDir 'bin\modelstat.mjs')
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue (Join-Path $HomeDir 'bin\node_modules')
  Ok 'removed the old npm launcher (your device pairing is untouched)'
}
# The old daemon also shipped as a GLOBAL npm package (npm i -g modelstat), which
# leaves a stale `modelstat` on PATH that shadows the native binary. Remove it
# best-effort - a no-op if npm is absent or it was never globally installed.
if (Get-Command npm -ErrorAction SilentlyContinue) {
  npm ls -g --depth=0 modelstat 2>$null | Out-Null
  if ($LASTEXITCODE -eq 0) {
    Step 'Removing the old global npm package'
    npm rm -g modelstat 2>$null | Out-Null
    Ok "removed the old global 'modelstat' npm package"
  }
}

# ── resolve version ────────────────────────────────────────────────
Step 'Resolving the latest release'
if (-not $Version) {
  $latest = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'modelstat-installer' }
  $Version = $latest.tag_name
  if (-not $Version) { Die 'couldn''t resolve the latest version — pass -Version X.Y.Z' }
}
$Version = $Version -replace '^v', '' -replace '^daemon-', ''
Ok "version $Version - target $triple"

# Auto-update policy (feature §13). Default ON — it is how users receive fixes.
# OFF only when asked (-NoAutoUpdate), or when this release is marked a
# PRE-RELEASE on GitHub: you deliberately installed a test build, so the daemon
# must not update itself off the very thing you're testing. (Detected via
# GitHub's prerelease flag, NOT a version suffix — the version stays clean semver.)
if (-not $NoAutoUpdate) {
  try {
    $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/tags/daemon-$Version" -Headers @{ 'User-Agent' = 'modelstat-installer' }
    if ($rel.prerelease) {
      $NoAutoUpdate = $true
      Write-Host '  (pre-release - auto-update will be disabled so it stays on this build)'
    }
  } catch {
    # Can't tell -> leave the default (on). Not fatal.
  }
}

# ── download + verify + extract ────────────────────────────────────
$base = "https://github.com/$Repo/releases/download/daemon-$Version"
$archive = "modelstat-$Version-$triple.zip"
$tmp = Join-Path $env:TEMP "modelstat-install-$PID"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
  Step "Downloading $archive"
  Invoke-WebRequest "$base/$archive" -OutFile (Join-Path $tmp $archive) -UseBasicParsing
  Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS') -UseBasicParsing

  Step 'Verifying checksum'
  $expected = (Select-String -Path (Join-Path $tmp 'SHA256SUMS') -Pattern ([regex]::Escape($archive)) | Select-Object -First 1).Line.Split(' ')[0]
  if (-not $expected) { Die "no checksum for $archive in SHA256SUMS" }
  $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $archive)).Hash.ToLower()
  if ($expected.ToLower() -ne $actual) { Die "checksum mismatch — refusing to install (expected $expected, got $actual)" }
  Ok 'sha256 verified'

  Expand-Archive -Path (Join-Path $tmp $archive) -DestinationPath $tmp -Force

  # ── stage + hand off ─────────────────────────────────────────────
  Step "Installing to $HomeDir\bin"
  if ($System) { $env:MODELSTAT_HOME = Join-Path $env:ProgramData 'modelstat' }
  & (Join-Path $tmp 'modelstat.exe') _setup-runtime
  if ($LASTEXITCODE -ne 0) { Die 'staging failed' }
  $binHome = if ($env:MODELSTAT_HOME) { $env:MODELSTAT_HOME } else { $HomeDir }
  Ok "staged $binHome\bin\modelstat.exe"

  # Apply the auto-update policy BEFORE the daemon starts, so it never acts on a
  # release verdict for a build the server doesn't know about.
  if ($NoAutoUpdate) {
    Step 'Disabling auto-update for this build'
    & (Join-Path $binHome 'bin\modelstat.exe') autoupdate off 2>$null | Out-Null
    Ok 'auto-update off - re-enable any time with: modelstat autoupdate on'
  } else {
    # GA install (not a pre-release, no -NoAutoUpdate): ensure auto-update is ON. A
    # pre-release installed earlier turns it OFF and that preference persists, so
    # without this a tester moving to a GA build would silently stay off.
    & (Join-Path $binHome 'bin\modelstat.exe') autoupdate on 2>$null | Out-Null
  }

  $fwd = @()
  if ($Mode) { $fwd += '--mode'; $fwd += $Mode }
  if ($Url) { $fwd += '--url'; $fwd += $Url }
  if ($Yes) { $fwd += '--yes' }
  if ($NoBrowser) { $fwd += '--no-browser' }
  if ($System) { $fwd += '--system' }

  if ($Component -eq 'summarizer') {
    Step 'Configuring the summariser engine'
    & (Join-Path $binHome 'bin\modelstat-summarizer.exe') setup @fwd
  } else {
    Step 'Pairing this device'
    & (Join-Path $binHome 'bin\modelstat.exe') connect @fwd
  }
}
finally {
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
}
