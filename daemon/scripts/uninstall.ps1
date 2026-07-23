# modelstat - remove EVERYTHING (Windows).
#
#   irm https://modelstat.ai/uninstall.ps1 | iex
#
# TOTAL removal. After this runs, the machine is indistinguishable from one that
# never installed modelstat: no binaries, no scheduled task or service, no
# background processes, no device identity, no downloaded models, no MCP wiring,
# no statusline, no Path entries, no cached copies, nothing of ours anywhere.
#
# It removes BOTH eras in one pass: the native Rust install and the retired
# npm/Node one (global packages across every package manager and Node version,
# npx caches, `npx @modelstat/mcp` MCP entries).
#
# NOTHING IS PRESERVED - not even the device identity. Installing again after
# this registers the machine as a BRAND-NEW device on the dashboard. That is the
# intended behaviour; there is no flag to keep the old pairing.
#
# What it deliberately does NOT touch: Node.js and your package managers, other
# MCP servers in the same config files, your own statusline (restored if we ever
# covered it up), and Claude Code's session history (%USERPROFILE%\.claude\
# projects\... - those folders are named after YOUR repo paths, so some contain
# the word "modelstat" without being ours).
#
# Flags: -DryRun (print every change without making it)
#
# A system-wide install (installed with -System) needs an elevated PowerShell;
# the script says so and names exactly what is left if you are not elevated.

[CmdletBinding()]
param([switch]$DryRun)

$ErrorActionPreference = 'Stop'
$HomeDir = if ($env:MODELSTAT_HOME) { $env:MODELSTAT_HOME } else { Join-Path $env:USERPROFILE '.modelstat' }
# Every npm package name the CLI was ever published under, plus the MCP server.
$LegacyPkgs = @('modelstat', '@modelstat/daemon', '@modelstat/mcp')
# Both shipped binaries, and the launcher shapes npm generates for them.
$BinNames = @('modelstat', 'modelstat.exe', 'modelstat.cmd', 'modelstat.ps1',
              'modelstat-summarizer', 'modelstat-summarizer.exe', 'modelstat-summarizer.cmd')
$script:Removed = 0
$script:NeedsAdmin = @()
$script:Unparseable = @()

function Step($m) { Write-Host "`n> $m" -ForegroundColor Cyan }
function Ok($m) { Write-Host "  + $m" -ForegroundColor Green; $script:Removed++ }
function Skip($m) { Write-Host "  . $m" -ForegroundColor DarkGray }
function Warn($m) { Write-Host "  ! $m" -ForegroundColor Yellow }
function Die($m) { Write-Host "  x $m" -ForegroundColor Red; exit 1 }
function Would($m) { Write-Host "    would $m" -ForegroundColor DarkGray }

$IsAdmin = ([Security.Principal.WindowsPrincipal] `
  [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

Write-Host "`n  modelstat - removing everything" -ForegroundColor Cyan
if ($DryRun) { Write-Host '  dry run - nothing will be changed' -ForegroundColor DarkGray }
else { Write-Host '  this deletes your device identity and downloaded models too' -ForegroundColor DarkGray }

# The single choke point for deletions, so -DryRun can never lie about the real run.
function Remove-Target($path, $label) {
  if (-not (Test-Path -LiteralPath $path)) { return $false }
  if ($DryRun) { Would "delete: $path"; if ($label) { Ok $label }; return $true }
  try {
    Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction Stop
    if ($label) { Ok $label }
    return $true
  } catch {
    Warn "needs elevation: $path"
    $script:NeedsAdmin += $path
    return $false
  }
}
function Invoke-Step($exe, $arguments) {
  if ($DryRun) { Would "run: $exe $($arguments -join ' ')"; return }
  & $exe @arguments 2>$null | Out-Null
}

# ───────────────────────────────────────────────────────────────────
# 1. Scheduled tasks + Windows services, both components
# ───────────────────────────────────────────────────────────────────
# The per-user install registers a Scheduled Task per component; a -System
# install registers a real Windows service under the same names.
Step 'Background services'
$svcHit = $false
foreach ($unit in @('modelstat', 'modelstat-summarizer')) {
  schtasks /Query /TN $unit 2>$null | Out-Null
  if ($LASTEXITCODE -eq 0) {
    Invoke-Step 'schtasks' @('/End', '/TN', $unit)
    Invoke-Step 'schtasks' @('/Delete', '/TN', $unit, '/F')
    Ok "removed scheduled task $unit"; $svcHit = $true
  }
  $svc = Get-Service -Name $unit -ErrorAction SilentlyContinue
  if ($svc) {
    $svcHit = $true
    if ($IsAdmin) {
      Invoke-Step 'sc.exe' @('stop', $unit)
      Invoke-Step 'sc.exe' @('delete', $unit)
      Ok "removed Windows service $unit"
    } else {
      Warn "needs elevation: Windows service '$unit'"
      $script:NeedsAdmin += "service:$unit"
    }
  }
}
if (-not $svcHit) { Skip 'no services installed' }

# ───────────────────────────────────────────────────────────────────
# 2. Any surviving process
# ───────────────────────────────────────────────────────────────────
# Matched on our own binary and bundle names so nothing unrelated is touched.
Step 'Running processes'
$procs = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {
  $_.CommandLine -and ($_.CommandLine -match 'modelstat\.mjs|\\\.modelstat\\bin\\modelstat|modelstat-summarizer')
})
if ($procs.Count -gt 0) {
  foreach ($p in $procs) {
    if ($DryRun) { Would "stop pid $($p.ProcessId)" }
    else { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
  }
  Ok "stopped $($procs.Count) modelstat process(es)"
} else { Skip 'nothing running' }

# ───────────────────────────────────────────────────────────────────
# 3. Global npm-era packages - ask, then sweep
# ───────────────────────────────────────────────────────────────────
# Asking a package manager is NOT enough on its own: pnpm resolves `-g` against
# the CURRENT store version, so a package installed by an older pnpm is invisible
# to a newer one - it reports "no global packages" and removes nothing while the
# command still works. nvm/fnm/volta have the same shape, one global root per
# Node version. So we ask first, then sweep every root on disk.
Step 'Global packages - pass A: ask each package manager'
$pkgHit = $false
foreach ($pkg in $LegacyPkgs) {
  if (Get-Command npm -ErrorAction SilentlyContinue) {
    npm ls -g --depth=0 $pkg 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) { Invoke-Step 'npm' @('rm', '-g', $pkg); Ok "npm: removed $pkg"; $pkgHit = $true }
  }
  if (Get-Command pnpm -ErrorAction SilentlyContinue) {
    if ((pnpm ls -g --depth 0 2>$null | Out-String) -match [regex]::Escape($pkg)) {
      Invoke-Step 'pnpm' @('remove', '-g', $pkg); Ok "pnpm: removed $pkg"; $pkgHit = $true
    }
  }
  # yarn v1 is the only yarn with a global registry; berry exits nonzero here.
  if (Get-Command yarn -ErrorAction SilentlyContinue) {
    if ((yarn global list 2>$null | Out-String) -match [regex]::Escape("$pkg@")) {
      Invoke-Step 'yarn' @('global', 'remove', $pkg); Ok "yarn: removed $pkg"; $pkgHit = $true
    }
  }
  if (Get-Command bun -ErrorAction SilentlyContinue) {
    if ((bun pm ls -g 2>$null | Out-String) -match [regex]::Escape("$pkg@")) {
      Invoke-Step 'bun' @('remove', '-g', $pkg); Ok "bun: removed $pkg"; $pkgHit = $true
    }
  }
}
if (Get-Command volta -ErrorAction SilentlyContinue) {
  if ((volta list 2>$null | Out-String) -match 'modelstat') {
    Invoke-Step 'volta' @('uninstall', 'modelstat'); Ok 'volta: removed modelstat'; $pkgHit = $true
  }
}
if (-not $pkgHit) { Skip 'no manager reported a global install' }

Step 'Global packages - pass B: sweep every global root on disk'
$diskHit = $false
$nmRoots = @()
if (Get-Command npm -ErrorAction SilentlyContinue) {
  $r = (npm root -g 2>$null | Out-String).Trim(); if ($r) { $nmRoots += $r }
}
foreach ($base in @($env:APPDATA, $env:LOCALAPPDATA, $env:USERPROFILE) | Where-Object { $_ }) {
  $nmRoots += @(
    (Join-Path $base 'npm\node_modules'),
    (Join-Path $base 'pnpm\global'),
    (Join-Path $base '.bun\install\global\node_modules'),
    (Join-Path $base 'Yarn\Data\global\node_modules'),
    (Join-Path $base 'Volta\tools\image\packages'),
    (Join-Path $base 'nvm')
  )
}
# Expand one level of version/store directories (pnpm global\5, nvm\v20.11.0, ...).
$expanded = @()
foreach ($r in $nmRoots | Where-Object { Test-Path -LiteralPath $_ }) {
  $expanded += $r
  $expanded += (Get-ChildItem -LiteralPath $r -Directory -ErrorAction SilentlyContinue |
                ForEach-Object { @($_.FullName, (Join-Path $_.FullName 'node_modules')) })
}
foreach ($nm in ($expanded | Sort-Object -Unique | Where-Object { Test-Path -LiteralPath $_ })) {
  foreach ($pkg in @('modelstat', '@modelstat')) {
    $p = Join-Path $nm $pkg
    if (Test-Path -LiteralPath $p) { if (Remove-Target $p "removed $p") { $diskHit = $true } }
  }
  # pnpm keeps the real files in a content-addressed store beside node_modules.
  $store = Join-Path $nm '.pnpm'
  if (Test-Path -LiteralPath $store) {
    foreach ($d in Get-ChildItem -LiteralPath $store -Directory -ErrorAction SilentlyContinue |
                   Where-Object { $_.Name -match '^@?modelstat' }) {
      if (Remove-Target $d.FullName "removed $($d.FullName)") { $diskHit = $true }
    }
  }
}
if (-not $diskHit) { Skip 'no leftover package directories' }

# ───────────────────────────────────────────────────────────────────
# 4. npx / dlx caches
# ───────────────────────────────────────────────────────────────────
# `npx modelstat@latest` never installed globally - it cached a full copy that
# `npx modelstat` would keep re-running.
Step 'npx / dlx caches'
$cacheHit = $false
$cacheRoots = @()
if ($env:USERPROFILE) {
  $cacheRoots += (Join-Path $env:USERPROFILE '.npm\_npx')
  $cacheRoots += (Join-Path $env:USERPROFILE '.bun\install\cache')
}
if ($env:LOCALAPPDATA) {
  $cacheRoots += (Join-Path $env:LOCALAPPDATA 'npm-cache\_npx')
  $cacheRoots += (Join-Path $env:LOCALAPPDATA 'pnpm-cache\dlx')
  $cacheRoots += (Join-Path $env:LOCALAPPDATA 'pnpm\dlx')
}
foreach ($root in $cacheRoots | Where-Object { Test-Path -LiteralPath $_ }) {
  foreach ($d in Get-ChildItem -LiteralPath $root -Directory -ErrorAction SilentlyContinue) {
    $nm = Join-Path $d.FullName 'node_modules'
    if ((Test-Path (Join-Path $nm 'modelstat')) -or (Test-Path (Join-Path $nm '@modelstat')) -or
        ($d.Name -match '^@?modelstat')) {
      if (Remove-Target $d.FullName) { $cacheHit = $true }
    }
  }
}
if ($cacheHit) { Ok 'cleared cached copies' } else { Skip 'no cached copies' }

# ───────────────────────────────────────────────────────────────────
# 5. Command launchers on disk
# ───────────────────────────────────────────────────────────────────
# Both binary names in every shape npm generates, everywhere a package manager
# or the installer could have put them - including bin dirs of Node versions that
# are not active right now (invisible to Path, but back the moment you switch).
Step 'Command launchers'
$shimHit = $false
$binDirs = @()
$binDirs += ($env:Path -split ';' | Where-Object { $_ })
$binDirs += (Join-Path $HomeDir 'bin')
foreach ($base in @($env:APPDATA, $env:LOCALAPPDATA, $env:USERPROFILE) | Where-Object { $_ }) {
  $binDirs += @(
    (Join-Path $base 'npm'), (Join-Path $base 'pnpm'),
    (Join-Path $base '.bun\bin'), (Join-Path $base 'Yarn\bin'),
    (Join-Path $base 'Volta\bin')
  )
}
foreach ($d in ($binDirs | Sort-Object -Unique | Where-Object { Test-Path -LiteralPath $_ })) {
  foreach ($name in $BinNames) {
    $f = Join-Path $d $name
    if (Test-Path -LiteralPath $f -PathType Leaf) {
      if (Remove-Target $f "removed $f") { $shimHit = $true }
    }
  }
}
if (-not $shimHit) { Skip 'no launchers found' }

# ───────────────────────────────────────────────────────────────────
# 6. MCP wiring, statusline and the Claude Code plugin
# ───────────────────────────────────────────────────────────────────
function Edit-JsonConfig($path, $mode) {
  if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return }
  try { $cfg = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json }
  catch {
    # JSONC with comments (VS Code, Zed) or a hand-broken file - never guess.
    if ((Get-Content -LiteralPath $path -Raw) -match 'modelstat') { $script:Unparseable += $path }
    return
  }
  $changes = @()
  if ($mode -eq 'mcp') {
    # One shape per tool family: mcpServers (Claude, Cursor, Windsurf, Gemini),
    # servers (VS Code), context_servers (Zed) - plus Claude Code's per-project
    # copies. Every modelstat entry goes, npx-era and native alike.
    $buckets = @()
    foreach ($k in @('mcpServers', 'servers', 'context_servers')) {
      if ($cfg.PSObject.Properties[$k]) { $buckets += , $cfg.$k }
    }
    if ($cfg.PSObject.Properties['projects']) {
      foreach ($p in $cfg.projects.PSObject.Properties) {
        if ($p.Value -and $p.Value.PSObject.Properties['mcpServers']) { $buckets += , $p.Value.mcpServers }
      }
    }
    foreach ($bucket in $buckets) {
      if (-not $bucket) { continue }
      foreach ($name in @($bucket.PSObject.Properties.Name)) {
        if ($name -match 'modelstat') { $bucket.PSObject.Properties.Remove($name); $changes += $name }
      }
    }
  } elseif ($mode -eq 'claude') {
    # Statusline: ours goes whatever form it is in. A statusline of yours that we
    # had composed over is stashed here and comes back.
    $cmd = if ($cfg.statusLine) { [string]$cfg.statusLine.command } else { '' }
    if ($cmd -match 'modelstat') {
      if ($cfg._modelstatPrevStatusLine) {
        $cfg.statusLine = $cfg._modelstatPrevStatusLine; $changes += 'statusLine (restored yours)'
      } else { $cfg.PSObject.Properties.Remove('statusLine'); $changes += 'statusLine' }
    }
    if ($cfg.PSObject.Properties['_modelstatPrevStatusLine']) {
      $cfg.PSObject.Properties.Remove('_modelstatPrevStatusLine'); $changes += '_modelstatPrevStatusLine'
    }
    # Claude Code plugin registration (the /stat command + bundled MCP server).
    foreach ($k in @('extraKnownMarketplaces', 'enabledPlugins', 'disabledPlugins')) {
      if ($cfg.PSObject.Properties[$k] -and $cfg.$k) {
        foreach ($name in @($cfg.$k.PSObject.Properties.Name)) {
          if ($name -match 'modelstat') { $cfg.$k.PSObject.Properties.Remove($name); $changes += "$k.$name" }
        }
        if (-not $cfg.$k.PSObject.Properties.Name) { $cfg.PSObject.Properties.Remove($k) }
      }
    }
  } elseif ($mode -eq 'topkeys') {
    foreach ($name in @($cfg.PSObject.Properties.Name)) {
      if ($name -match 'modelstat') { $cfg.PSObject.Properties.Remove($name); $changes += $name }
    }
  }
  if ($changes.Count -eq 0) { return }
  if (-not $DryRun) {
    # Atomic write, no .bak litter left behind.
    $tmp = "$path.modelstat-tmp"
    ($cfg | ConvertTo-Json -Depth 100) | Set-Content -LiteralPath $tmp -Encoding UTF8
    Move-Item -LiteralPath $tmp -Destination $path -Force
  }
  Ok "$path - removed $($changes -join ', ')"
}

Step 'MCP entries in your AI tools'
$mcpFiles = @()
if ($env:USERPROFILE) {
  $mcpFiles += @('.claude.json', '.cursor\mcp.json', '.codeium\windsurf\mcp_config.json',
                 '.gemini\settings.json') | ForEach-Object { Join-Path $env:USERPROFILE $_ }
}
if ($env:APPDATA) {
  $mcpFiles += @('Claude\claude_desktop_config.json', 'Code\User\mcp.json',
                 'Code - Insiders\User\mcp.json', 'VSCodium\User\mcp.json',
                 'Zed\settings.json') | ForEach-Object { Join-Path $env:APPDATA $_ }
}
$mcpFiles | ForEach-Object { Edit-JsonConfig $_ 'mcp' }

if (Get-Command claude -ErrorAction SilentlyContinue) {
  if ((claude mcp list 2>$null | Out-String) -match 'modelstat') {
    Invoke-Step 'claude' @('mcp', 'remove', 'modelstat', '-s', 'user')
    Ok 'claude: removed the modelstat MCP entry'
  }
}

# Codex stores MCP servers as a TOML table - keep the file byte-for-byte except
# our block.
$codex = Join-Path $env:USERPROFILE '.codex\config.toml'
if (Test-Path -LiteralPath $codex -PathType Leaf) {
  $inBlock = $false; $found = $false; $kept = @()
  foreach ($line in Get-Content -LiteralPath $codex) {
    if ($line -match '^\[mcp_servers\.modelstat\]') { $inBlock = $true; $found = $true; continue }
    elseif ($line -match '^\[') { $inBlock = $false }
    if (-not $inBlock) { $kept += $line }
  }
  if ($found) {
    if ($DryRun) { Would "edit: $codex (drop [mcp_servers.modelstat])" }
    else { $kept | Set-Content -LiteralPath $codex -Encoding UTF8 }
    Ok "$codex - removed [mcp_servers.modelstat]"
  }
}

# NOTE: this touches settings.json and the plugin registry ONLY. It never goes
# near .claude\projects\ - those folders are named after your repo paths, so some
# contain "modelstat" while being your own session history.
Step 'Claude Code statusline + plugin'
$claudeDir = if ($env:CLAUDE_CONFIG_DIR) { $env:CLAUDE_CONFIG_DIR } else { Join-Path $env:USERPROFILE '.claude' }
Edit-JsonConfig (Join-Path $claudeDir 'settings.json') 'claude'
Edit-JsonConfig (Join-Path $claudeDir 'plugins\known_marketplaces.json') 'topkeys'
$markets = Join-Path $claudeDir 'plugins\marketplaces'
if (Test-Path -LiteralPath $markets) {
  foreach ($d in Get-ChildItem -LiteralPath $markets -Directory -ErrorAction SilentlyContinue |
                 Where-Object { $_.Name -match 'modelstat' }) {
    Remove-Target $d.FullName "removed plugin marketplace $($d.FullName)" | Out-Null
  }
}
foreach ($b in @('settings.json.modelstat.bak', 'settings.json.modelstat-legacy.bak')) {
  Remove-Target (Join-Path $claudeDir $b) "removed leftover backup $b" | Out-Null
}

# ───────────────────────────────────────────────────────────────────
# 7. Path entries
# ───────────────────────────────────────────────────────────────────
# Windows has no dotfiles: the Path environment variable IS the Path.
# SetEnvironmentVariable is the one API that writes it without setx's
# 1024-character truncation and that broadcasts the change to running shells.
Step 'Path environment variable'
$pathHit = $false
foreach ($scope in @('User', 'Machine')) {
  if ($scope -eq 'Machine' -and -not $IsAdmin) {
    $mp = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    if ($mp -and (($mp -split ';') | Where-Object { $_ -match 'modelstat' })) {
      Warn 'needs elevation: machine-wide Path entry'
      $script:NeedsAdmin += 'path:Machine'
    }
    continue
  }
  $val = [Environment]::GetEnvironmentVariable('Path', $scope)
  if (-not $val) { continue }
  $entries = $val -split ';' | Where-Object { $_ }
  $stale = @($entries | Where-Object { $_ -match 'modelstat' })
  if ($stale.Count -eq 0) { continue }
  foreach ($s in $stale) { Write-Host "    $s" -ForegroundColor DarkGray }
  if ($DryRun) { Would "rewrite the $scope Path without those entries" }
  else {
    $newPath = ($entries | Where-Object { $stale -notcontains $_ }) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newPath, $scope)
  }
  Ok "removed $($stale.Count) $scope Path entry(ies)"
  $pathHit = $true
}
if (-not $pathHit) { Skip 'no Path entries' }

# ───────────────────────────────────────────────────────────────────
# 8. The home directory - identity, models, logs, binaries, everything
# ───────────────────────────────────────────────────────────────────
Step 'Data directory'
$homes = @($HomeDir, (Join-Path $env:USERPROFILE '.modelstat')) | Sort-Object -Unique
$homeHit = $false
foreach ($d in $homes) {
  if (Test-Path -LiteralPath $d) {
    Remove-Target $d "deleted $d (identity, models, logs, binaries)" | Out-Null
    $homeHit = $true
  }
}
if (-not $homeHit) { Skip 'nothing to delete' }

# ── summary ────────────────────────────────────────────────────────
Write-Host ''
if ($script:Unparseable.Count -gt 0) {
  Warn 'these files mention modelstat but could not be parsed automatically -'
  Warn 'remove the "modelstat" entry by hand (they are likely JSON with comments):'
  $script:Unparseable | ForEach-Object { Write-Host "    $_" }
  Write-Host ''
}
if ($script:NeedsAdmin.Count -gt 0) {
  Warn 'a system-wide install needs an elevated PowerShell. Re-run this script as'
  Warn 'Administrator to finish removing:'
  $script:NeedsAdmin | ForEach-Object { Write-Host "    $_" }
  Write-Host ''
}
if ($script:Removed -eq 0) {
  Write-Host '  Nothing found - this machine is already clean.' -ForegroundColor Cyan
} else {
  Write-Host "  Done - modelstat is gone ($($script:Removed) item(s) removed)." -ForegroundColor Cyan
}
Write-Host ''
Write-Host '  Open a NEW terminal, then confirm:' -ForegroundColor DarkGray
Write-Host '    Get-Command modelstat    # expect an error (not found)' -ForegroundColor DarkGray
Write-Host "    Test-Path `"$HomeDir`"   # expect False" -ForegroundColor DarkGray
Write-Host ''
Write-Host '  Your account and history on modelstat.ai are server-side and untouched -' -ForegroundColor DarkGray
Write-Host '  delete those from the dashboard if you want them gone too.' -ForegroundColor DarkGray
Write-Host ''
