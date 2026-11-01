<#
.SYNOPSIS
Installs a build of the port into <Root>\pool-rs\<Version>\ and points the wrapper at it.

.DESCRIPTION
1. Copies pools-shadow.exe and pools-rs.exe into <Root>\pool-rs\<Version>\, with build.json
   (its SHA-256 and the commit). The stylesheets and static files are built into the program
   (corten::assets::embed!), so there is no static folder. A version is never replaced: the same
   version again must be the same file.
2. Writes pool-rs.json ("current", "previous", "tailnet"). Agent Pools' own files are not changed.
3. Points each of the port's tasks that is registered at the new version's pools-rs.exe (the
   action only). With -RegisterTasks, registers "<TaskPrefix> - subscriptions", "- inference
   relay", "- public gateway", "- cloudflare tunnel" and "- backup" as Agent Pools registers its
   own; a task registered here for the first time is left Disabled (Cutover-Pool-Rs.ps1 enables
   it). An existing task keeps its triggers, settings and state; only its action changes.

-Use <Version> points "current" and the tasks at a version already installed (a rollback).
Either way a running command keeps its version until Restart-Pool-Rs.ps1 starts it again.

The live install is refused without -Live, as the program refuses it.
#>
[CmdletBinding(DefaultParameterSetName = 'Install')]
param(
  [Parameter(Mandatory)][string]$Root,
  [Parameter(ParameterSetName = 'Install', Mandatory)][string]$Exe,
  [Parameter(ParameterSetName = 'Install', Mandatory)][ValidatePattern('^[0-9A-Za-z][0-9A-Za-z._-]*$')][string]$Version,
  [Parameter(ParameterSetName = 'Use', Mandatory)][ValidatePattern('^[0-9A-Za-z][0-9A-Za-z._-]*$')][string]$Use,
  [string]$TaskPrefix = 'Agent Pools (rs)',
  [string]$TailnetOrigin,
  [string]$TailnetOwner,
  [switch]$RegisterTasks,
  [switch]$Live
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
$target = Resolve-PoolRoot $Root $Live.IsPresent
$Root = $target.Root
$repo = Split-Path -Parent $PSScriptRoot
$homeDir = Join-Path $Root 'pool-rs'
$settingsFile = Join-Path $homeDir 'pool-rs.json'
$settings = Read-Json $settingsFile
if (-not $settings) { $settings = [pscustomobject]@{} }

function Set-Current([string]$v) {
  if (-not (Test-Path -LiteralPath (Join-Path $homeDir "$v\pools-shadow.exe"))) { throw "version $v is not installed in $homeDir" }
  $was = $settings.current
  $settings | Add-Member -Force NoteProperty current $v
  if ($was -and $was -ne $v) { $settings | Add-Member -Force NoteProperty previous $was }
}

if ($PSCmdlet.ParameterSetName -eq 'Use') {
  Set-Current $Use
  Write-Json $settingsFile $settings
  foreach ($t in Get-PoolRsTasks $TaskPrefix) { Set-PoolRsTask $Root $Use $t $target.Live -ActionOnly }
  "current: $Use (previous: $($settings.previous)); Restart-Pool-Rs.ps1 runs it"
  return
}

$Exe = (Resolve-Path -LiteralPath $Exe).ProviderPath
$hash = (Get-FileHash -LiteralPath $Exe -Algorithm SHA256).Hash.ToLowerInvariant()
$dir = Join-Path $homeDir $Version
if (Test-Path -LiteralPath $dir) {
  $had = Read-Json (Join-Path $dir 'build.json')
  if (-not $had -or $had.sha256 -ne $hash) { throw "$dir exists with another build; versions are not replaced" }
  "version $Version is already installed (same build)"
} else {
  New-Item -ItemType Directory -Path $dir | Out-Null
  Copy-Item -LiteralPath $Exe -Destination (Join-Path $dir 'pools-shadow.exe')
  # The same program with no console window, for the tasks (built beside it).
  $windowless = Join-Path (Split-Path -Parent $Exe) 'pools-rs.exe'
  if (-not (Test-Path -LiteralPath $windowless)) { throw "$windowless is missing: build both binaries (cargo build -p pools-shadow --release)" }
  Copy-Item -LiteralPath $windowless -Destination (Join-Path $dir 'pools-rs.exe')
  $commit = (& git -C $repo rev-parse HEAD 2>$null)
  $dirty = [bool](& git -C $repo status --porcelain 2>$null)
  $windowlessHash = (Get-FileHash -LiteralPath $windowless -Algorithm SHA256).Hash.ToLowerInvariant()
  Write-Json (Join-Path $dir 'build.json') ([ordered]@{ version = $Version; sha256 = $hash; windowlessSha256 = $windowlessHash; commit = $commit; dirty = $dirty; installedAt = (Get-Date).ToUniversalTime().ToString('o') })
  "installed $Version in $dir"
}
Set-Current $Version
if ($TailnetOrigin -or $TailnetOwner) {
  if (-not ($TailnetOrigin -and $TailnetOwner)) { throw 'pass both -TailnetOrigin and -TailnetOwner, or neither' }
  $settings | Add-Member -Force NoteProperty tailnet ([ordered]@{ origin = $TailnetOrigin; owner = $TailnetOwner })
}
Write-Json $settingsFile $settings
"current: $($settings.current) (previous: $($settings.previous))"

foreach ($t in Get-PoolRsTasks $TaskPrefix) {
  # An existing task gets the new action only (a running instance carries on until restarted).
  $exists = [bool](Get-ScheduledTask -TaskName $t.Name -ErrorAction SilentlyContinue)
  if ($RegisterTasks -and -not $exists) { Set-PoolRsTask $Root $Version $t $target.Live } else { Set-PoolRsTask $Root $Version $t $target.Live -ActionOnly }
}

# The wrapper older versions' tasks started (Run-Pool-Rs.vbs), once no task names it.
$wrapper = Join-Path $homeDir 'Run-Pool-Rs.vbs'
if ((Test-Path -LiteralPath $wrapper) -and -not (Get-ScheduledTask | Where-Object { $_.Actions.Arguments -like "*$wrapper*" })) {
  Remove-Item -LiteralPath $wrapper -ErrorAction SilentlyContinue
  if (Test-Path -LiteralPath $wrapper) { "left $wrapper (still open); the next install removes it" } else { "removed $wrapper (no task starts it)" }
}
