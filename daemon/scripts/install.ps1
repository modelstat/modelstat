# modelstat installer for Windows (feature §3).
#
#   irm https://install.modelstat.ai/ps | iex
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
  [switch]$NoBrowser
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

# ── legacy Node install migration ──────────────────────────────────
if ((Test-Path (Join-Path $HomeDir 'bin\modelstat.mjs')) -or (Test-Path (Join-Path $HomeDir 'bin\node_modules'))) {
  Step 'Migrating off the old Node install'
  Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $HomeDir 'bin\modelstat.mjs')
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue (Join-Path $HomeDir 'bin\node_modules')
  Ok 'removed the old npm launcher (your device pairing is untouched)'
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
