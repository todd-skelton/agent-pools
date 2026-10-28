# Shared by Install-Pool-Rs.ps1, Restart-Pool-Rs.ps1 and Cutover-Pool-Rs.ps1.
$script:LiveRoot = 'D:\Users\ToddS\AgentPools'

# The install at $Root, refused as Install::open refuses it: the live install only with -Live,
# -Live only on the live install, anything else only as a rehearsal outside the live install.
function Resolve-PoolRoot([string]$Root, [bool]$Live) {
  $r = (Resolve-Path -LiteralPath $Root).ProviderPath.TrimEnd('\')
  $l = (Resolve-Path -LiteralPath $script:LiveRoot -ErrorAction SilentlyContinue).ProviderPath
  if ($l) { $l = $l.TrimEnd('\') }
  $isLive = $l -and ($r -ieq $l)
  if ($isLive -and -not $Live) { throw "$r is the live install: pass -Live (cutover only)" }
  if (-not $isLive -and $Live) { throw "-Live names the live install, and $r is not it" }
  if (-not $isLive) {
    if (-not (Test-Path -LiteralPath (Join-Path $r 'state\rehearsal.json'))) { throw "$r is not a rehearsal (state\rehearsal.json is missing)" }
    if ($l -and (($r + '\').StartsWith($l + '\', [StringComparison]::OrdinalIgnoreCase) -or ($l + '\').StartsWith($r + '\', [StringComparison]::OrdinalIgnoreCase))) { throw "$r overlaps the live install" }
  }
  [pscustomobject]@{ Root = $r; Live = [bool]$isLive }
}

function Read-Json([string]$Path) {
  if (-not (Test-Path -LiteralPath $Path)) { return $null }
  (Get-Content -LiteralPath $Path -Raw) | ConvertFrom-Json
}

# UTF-8 without a byte-order mark.
function Write-Json([string]$Path, $Value) {
  [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 6) + "`n", [Text.UTF8Encoding]::new($false))
}

# The port's tasks, as Agent Pools registers its own (Register-Pool-Tasks.ps1, Set-Inference-Relay.ps1,
# Set-Public-Gateway.ps1): each starts <Root>\pool-rs\<version>\pools-rs.exe (no console window, so no
# wscript.exe wrapper) with its command. Keepalive: logon plus every 5 minutes, IgnoreNew, no time
# limit; the backup runs daily at 03:30 and at logon. Old is the Agent Pools task each replaces.
function Get-PoolRsTasks([string]$Prefix) {
  @(
    [pscustomobject]@{ Key = 'service';   Name = "$Prefix - subscriptions";    Command = 'guardian';        Live = $true;  Keepalive = $true;  Restart = $false; Priority = $null; Old = 'Agent Pools - subscriptions';     Description = 'Agent Pools on Corten: the guardian, the service and the proxy.' }
    [pscustomobject]@{ Key = 'inference'; Name = "$Prefix - inference relay";  Command = 'serve-inference'; Live = $true;  Keepalive = $true;  Restart = $true;  Priority = $null; Old = 'Agent Pools - inference relay';   Description = 'Agent Pools on Corten: the inference relay.' }
    [pscustomobject]@{ Key = 'gateway';   Name = "$Prefix - public gateway";   Command = 'serve-gateway';   Live = $true;  Keepalive = $true;  Restart = $true;  Priority = 4;     Old = 'Agent Pools - public gateway';    Description = 'Agent Pools on Corten: the public gateway.' }
    [pscustomobject]@{ Key = 'tunnel';    Name = "$Prefix - cloudflare tunnel"; Command = 'tunnel';          Live = $false; Keepalive = $true;  Restart = $true;  Priority = 4;     Old = 'Agent Pools - cloudflare tunnel'; Description = 'Agent Pools on Corten: cloudflared for the public gateway hostname.' }
    [pscustomobject]@{ Key = 'backup';    Name = "$Prefix - backup";            Command = 'backup';          Live = $false; Keepalive = $false; Restart = $false; Priority = 7;     Old = 'Agent Pools - backup';            Description = 'Agent Pools on Corten: the DPAPI-protected backup of the credentials and state.' }
  )
}

# The task action for $Task on $Version of the install at $Root.
function New-PoolRsAction([string]$Root, [string]$Version, $Task, [bool]$Live) {
  $exe = Join-Path $Root "pool-rs\$Version\pools-rs.exe"
  if (-not (Test-Path -LiteralPath $exe)) { throw "$exe is not installed" }
  $arguments = $Task.Command + ' "' + $Root + '"'
  if ($Task.Live -and $Live) { $arguments += ' --live' }
  New-ScheduledTaskAction -Execute $exe -Argument $arguments -WorkingDirectory $Root
}

# Registers $Task (disabled when new; an existing task keeps its state) or, with -ActionOnly,
# points an existing one at $Version and changes nothing else.
function Set-PoolRsTask([string]$Root, [string]$Version, $Task, [bool]$Live, [switch]$ActionOnly) {
  $action = New-PoolRsAction $Root $Version $Task $Live
  $existing = Get-ScheduledTask -TaskName $Task.Name -ErrorAction SilentlyContinue
  if ($ActionOnly) {
    if (-not $existing) { return }
    Set-ScheduledTask -TaskName $Task.Name -Action $action | Out-Null
    return "task '$($Task.Name)' -> $Version"
  }
  $user = "$env:USERDOMAIN\$env:USERNAME"
  $triggers = @(New-ScheduledTaskTrigger -AtLogOn -User $user)
  if ($Task.Keepalive) {
    $keepalive = New-ScheduledTaskTrigger -Daily -At '00:00'
    $keepalive.Repetition = (New-ScheduledTaskTrigger -Once -At '00:00' -RepetitionInterval (New-TimeSpan -Minutes 5) -RepetitionDuration (New-TimeSpan -Days 1)).Repetition
    $triggers += $keepalive
    $opts = @{ ExecutionTimeLimit = [TimeSpan]::Zero; MultipleInstances = 'IgnoreNew'; DontStopIfGoingOnBatteries = $true; AllowStartIfOnBatteries = $true; StartWhenAvailable = $true }
  } else {
    $triggers = @((New-ScheduledTaskTrigger -Daily -At '03:30'), $triggers[0])
    $opts = @{ ExecutionTimeLimit = (New-TimeSpan -Minutes 10); MultipleInstances = 'IgnoreNew'; StartWhenAvailable = $true }
  }
  if ($Task.Restart) { $opts.RestartCount = 999; $opts.RestartInterval = New-TimeSpan -Minutes 1 }
  if ($Task.Priority) { $opts.Priority = $Task.Priority }
  $settingsSet = New-ScheduledTaskSettingsSet @opts
  $settingsSet.Enabled = if ($existing) { $existing.Settings.Enabled } else { $false }
  Register-ScheduledTask -TaskName $Task.Name -Action $action -Trigger $triggers -Settings $settingsSet -Description $Task.Description -Force | Out-Null
  "task '$($Task.Name)': $(if ($settingsSet.Enabled) { 'enabled' } else { 'disabled' }) ($($Task.Command) on $Version)"
}

# The port's processes running $Command for $Root (pools-rs.exe from the tasks; pools-shadow.exe
# by hand or from an older version's wrapper).
function Get-PoolRsProcess([string]$Root, [string]$Command) {
  # The task quotes the root; the guardian's child (Rust's argument quoting) only when it must.
  $want = '(^|\s)' + [regex]::Escape($Command) + '\s+"?' + [regex]::Escape($Root) + '"?(\s|$)'
  Get-CimInstance Win32_Process -Filter "Name='pools-shadow.exe' OR Name='pools-rs.exe'" | Where-Object { $_.CommandLine -and $_.CommandLine -imatch $want }
}

# Agent Pools' node processes running $Root\$Script.
function Get-NodeProcess([string]$Root, [string]$Script) {
  $path = Join-Path $Root $Script
  Get-CimInstance Win32_Process -Filter "Name='node.exe'" | Where-Object { $_.CommandLine -and $_.CommandLine.IndexOf($path, [StringComparison]::OrdinalIgnoreCase) -ge 0 }
}

# The process listening on 127.0.0.1:$Port, from netstat (Get-NetTCPConnection took 27 s here).
function Get-PortOwner([int]$Port) {
  foreach ($line in (netstat -ano -p tcp)) {
    if ($line -match ('^\s*TCP\s+127\.0\.0\.1:' + $Port + '\s+\S+\s+LISTENING\s+(\d+)\s*$')) { return Get-CimInstance Win32_Process -Filter "ProcessId=$($Matches[1])" }
  }
}

function Wait-Until([scriptblock]$Test, [int]$Seconds, [string]$What) {
  for ($i = 0; $i -lt $Seconds; $i++) { if (& $Test) { return }; Start-Sleep 1 }
  throw "timed out after $Seconds s waiting for $What"
}
