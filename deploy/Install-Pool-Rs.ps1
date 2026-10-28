<#
.SYNOPSIS
Installs a build of the port into <Root>\pool-rs\<Version>\ and points the wrapper at it.

.DESCRIPTION
1. Copies pools-shadow.exe and its static files (shadow\static, gateway\static) into
   <Root>\pool-rs\<Version>\, with build.json (its SHA-256 and the commit). A version is never
   replaced: the same version again must be the same file.
2. Writes <Root>\pool-rs\Run-Pool-Rs.vbs and pool-rs.json ("current", "previous", "tailnet").
   Agent Pools' own files are not changed.
3. With -RegisterTasks, registers "<TaskPrefix> - subscriptions", "- inference relay" and
   "- public gateway" as Agent Pools registers its own. A task registered here for the first time
   is left Disabled; Cutover-Pool-Rs.ps1 enables it. An existing one keeps its state.

-Use <Version> points "current" at a version already installed (a rollback) and changes nothing
else. A running command keeps its version until it is started again.

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
  "current: $Use (previous: $($settings.previous)); restart the tasks to run it"
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
  foreach ($s in @(@('shadow', "$repo\shadow\static"), @('gateway', "$repo\gateway\static"))) {
    $to = Join-Path $dir "static\$($s[0])"
    New-Item -ItemType Directory -Force -Path $to | Out-Null
    Copy-Item -Path (Join-Path $s[1] '*') -Destination $to
  }
  $commit = (& git -C $repo rev-parse HEAD 2>$null)
  $dirty = [bool](& git -C $repo status --porcelain 2>$null)
  Write-Json (Join-Path $dir 'build.json') ([ordered]@{ version = $Version; sha256 = $hash; commit = $commit; dirty = $dirty; installedAt = (Get-Date).ToUniversalTime().ToString('o') })
  "installed $Version in $dir"
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'Run-Pool-Rs.vbs') -Destination (Join-Path $homeDir 'Run-Pool-Rs.vbs') -Force
Set-Current $Version
if ($TailnetOrigin -or $TailnetOwner) {
  if (-not ($TailnetOrigin -and $TailnetOwner)) { throw 'pass both -TailnetOrigin and -TailnetOwner, or neither' }
  $settings | Add-Member -Force NoteProperty tailnet ([ordered]@{ origin = $TailnetOrigin; owner = $TailnetOwner })
}
Write-Json $settingsFile $settings
"current: $($settings.current) (previous: $($settings.previous))"

if ($RegisterTasks) {
  $user = "$env:USERDOMAIN\$env:USERNAME"
  $wscript = "$env:SystemRoot\System32\wscript.exe"
  $wrapper = Join-Path $homeDir 'Run-Pool-Rs.vbs'
  foreach ($t in Get-PoolRsTasks $TaskPrefix) {
    $existing = Get-ScheduledTask -TaskName $t.Name -ErrorAction SilentlyContinue
    $action = New-ScheduledTaskAction -Execute $wscript -Argument ('//B //Nologo "' + $wrapper + '" ' + $t.Command) -WorkingDirectory $Root
    $keepalive = New-ScheduledTaskTrigger -Daily -At '00:00'
    $keepalive.Repetition = (New-ScheduledTaskTrigger -Once -At '00:00' -RepetitionInterval (New-TimeSpan -Minutes 5) -RepetitionDuration (New-TimeSpan -Days 1)).Repetition
    $opts = @{ ExecutionTimeLimit = [TimeSpan]::Zero; MultipleInstances = 'IgnoreNew'; DontStopIfGoingOnBatteries = $true; AllowStartIfOnBatteries = $true; StartWhenAvailable = $true }
    if ($t.Restart) { $opts.RestartCount = 999; $opts.RestartInterval = New-TimeSpan -Minutes 1 }
    if ($t.Priority) { $opts.Priority = $t.Priority }
    $settingsSet = New-ScheduledTaskSettingsSet @opts
    $settingsSet.Enabled = if ($existing) { $existing.Settings.Enabled } else { $false }
    Register-ScheduledTask -TaskName $t.Name -Action $action -Trigger @((New-ScheduledTaskTrigger -AtLogOn -User $user), $keepalive) -Settings $settingsSet -Description $t.Description -Force | Out-Null
    "task '$($t.Name)': $(if ($settingsSet.Enabled) { 'enabled' } else { 'disabled' }) ($($t.Command))"
  }
}
