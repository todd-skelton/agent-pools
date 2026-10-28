<#
.SYNOPSIS
One cutover step from Agent Pools' Node processes to the port's, or back with -Undo.

.DESCRIPTION
Each step hands one job from the Node task to the port's task (Install-Pool-Rs.ps1 -RegisterTasks
registers them, disabled). The Node task is disabled, not deleted, so -Undo is the same step
backwards.

  gateway    "<OldPrefix> - public gateway" -> "<TaskPrefix> - public gateway" (publicGateway.port, 8330).
             The Node gateway has no graceful stop: streams through it are cut, so pick an idle time.
  inference  "<OldPrefix> - inference relay" -> "<TaskPrefix> - inference relay" (inferenceRelayPort, 8319).
             The same: Codex's turns in flight through the relay are cut.
  service    "<OldPrefix> - subscriptions" -> "<TaskPrefix> - subscriptions" (supervisorPort 8318,
             desktopRelayPort 8000). state\guardian-stop.request stops the running guardian, which
             stops its service gracefully and leaves the proxy running; the next service adopts the
             same proxy process from state\process-proxy.json. With -Tailnet, tailscale serve's
             :8443 moves from the tailnet proxy (8327) to the service, and "<OldPrefix> - tailnet
             dashboard proxy" is disabled.

The live install is refused without -Live. -WhatIf says what would be done and does nothing.
#>
[CmdletBinding(SupportsShouldProcess)]
param(
  [Parameter(Mandatory)][string]$Root,
  [Parameter(Mandatory)][ValidateSet('gateway', 'inference', 'service')][string]$Step,
  [switch]$Undo,
  [switch]$Tailnet,
  [string]$TaskPrefix = 'Agent Pools (rs)',
  [string]$OldPrefix = 'Agent Pools',
  [switch]$Live
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
$target = Resolve-PoolRoot $Root $Live.IsPresent
$Root = $target.Root
$install = Read-Json (Join-Path $Root 'state\install.json')
$new = Get-PoolRsTasks $TaskPrefix | Where-Object Key -eq $Step
if (-not (Get-ScheduledTask -TaskName $new.Name -ErrorAction SilentlyContinue)) { throw "task '$($new.Name)' is not registered: run Install-Pool-Rs.ps1 -RegisterTasks first" }

switch ($Step) {
  'gateway' { $oldTask = "$OldPrefix - public gateway"; $script = 'public-gateway-service.mjs'; $port = if ($install.publicGateway.port) { [int]$install.publicGateway.port } else { 8330 } }
  'inference' { $oldTask = "$OldPrefix - inference relay"; $script = 'inference-service.mjs'; $port = if ($install.inferenceRelayPort) { [int]$install.inferenceRelayPort } else { 8319 } }
  'service' { $oldTask = "$OldPrefix - subscriptions"; $script = 'pool-guardian.mjs'; $port = if ($install.supervisorPort) { [int]$install.supervisorPort } else { 8318 } }
}
$tailnetTask = "$OldPrefix - tailnet dashboard proxy"
$serviceCommand = 'serve-service'

function Say([string]$s) { "$((Get-Date).ToUniversalTime().ToString('HH:mm:ss'))Z $s" }

function Set-TaskEnabled([string]$name, [bool]$on) {
  if (-not (Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue)) { Say "no task '$name'"; return }
  if ($PSCmdlet.ShouldProcess($name, $(if ($on) { 'enable' } else { 'disable' }))) {
    if ($on) { Enable-ScheduledTask -TaskName $name | Out-Null } else { Disable-ScheduledTask -TaskName $name | Out-Null }
    Say "$(if ($on) { 'enabled' } else { 'disabled' }) '$name'"
  }
}

function Start-Task([string]$name) {
  if ($PSCmdlet.ShouldProcess($name, 'start')) { Start-ScheduledTask -TaskName $name; Say "started '$name'" }
}

function Stop-Processes($procs, [string]$what) {
  foreach ($p in @($procs | Where-Object { $_ })) {
    if ($PSCmdlet.ShouldProcess("$($p.ProcessId) $($p.CommandLine)", 'stop')) { Stop-Process -Id $p.ProcessId -Force; Say "stopped $what $($p.ProcessId)" }
  }
}

# The process listening on $port, as "node <script>" or "pools-shadow <command>".
function Owner-Name {
  $o = Get-PortOwner $port
  if (-not $o) { return 'nobody' }
  if ($o.Name -eq 'node.exe') { return "node $([IO.Path]::GetFileName(($o.CommandLine -split '"' | Where-Object { $_ -like '*.mjs' } | Select-Object -First 1)))" }
  if ($o.Name -eq 'pools-shadow.exe' -and $o.CommandLine -match '^"[^"]+"\s+(\S+)') { return "pools-shadow $($Matches[1]) pid $($o.ProcessId)" }
  return "$($o.Name) pid $($o.ProcessId)"
}

function Health {
  switch ($Step) {
    'gateway' { & curl.exe -s -o NUL -m 5 -w '%{http_code}' -H "Host: $($install.publicGateway.hostname)" "http://127.0.0.1:$port/" }
    'inference' { & curl.exe -s -k -o NUL -m 5 -w '%{http_code}' "https://localhost:$port/health" }
    'service' { try { $h = Invoke-RestMethod "http://127.0.0.1:$port/api/health" -TimeoutSec 3; "$($h.service) pid $($h.pid)" } catch { 'down' } }
  }
}

function Proxy-Record { $r = Read-Json (Join-Path $Root 'state\process-proxy.json'); if ($r) { [int]$r.pid } else { 0 } }

function Service-Proxy { try { @((Invoke-RestMethod "http://127.0.0.1:$port/api/status" -TimeoutSec 10).processes | Where-Object name -eq 'proxy')[0].pid } catch { $null } }

# Stops whatever holds the service through the stop file, as pool-guardian.mjs and the port's guardian both read it.
function Stop-Guardian([scriptblock]$Running) {
  $stop = Join-Path $Root 'state\guardian-stop.request'
  if (-not (& $Running)) { Say 'no guardian running'; return }
  if ($PSCmdlet.ShouldProcess($stop, 'write the stop file')) {
    New-Item -ItemType File -Force -Path $stop | Out-Null
    Say 'wrote state\guardian-stop.request'
    Wait-Until { -not (& $Running) -and -not (Get-PortOwner $port) } 90 'the guardian and its service to stop'
    Say "guardian stopped; port $port free; stop file left: $(Test-Path -LiteralPath $stop)"
  }
}

Say "$Step $(if ($Undo) { 'back to Node' } else { 'to the port' }) on $Root; port $port held by $(Owner-Name); health $(Health)"
$proxyBefore = Proxy-Record
if ($Step -eq 'service') { Say "proxy record pid $proxyBefore, alive $([bool](Get-Process -Id $proxyBefore -ErrorAction SilentlyContinue))" }

$newOwner = "pools-shadow $(if ($Step -eq 'service') { $serviceCommand } else { $new.Command })*"
$oldOwner = if ($Step -eq 'service') { 'node pool-service.mjs' } else { "node $script" }
if (-not $Undo) {
  Set-TaskEnabled $oldTask $false
  if ((Owner-Name) -like $newOwner) { Say 'the port already holds it' } else {
  if ($Step -eq 'service') {
    Stop-Guardian { Get-NodeProcess $Root 'pool-guardian.mjs' }
  } else {
    if ($PSCmdlet.ShouldProcess($oldTask, 'stop')) { Stop-ScheduledTask -TaskName $oldTask -ErrorAction SilentlyContinue }
    Stop-Processes (Get-NodeProcess $Root $script) "node $script"
  }
  if ($WhatIfPreference) { return }
  Wait-Until { -not (Get-PortOwner $port) } 30 "port $port to be free"
  }
  Set-TaskEnabled $new.Name $true
  if ((Owner-Name) -notlike $newOwner) { Start-Task $new.Name }
  Wait-Until { (Owner-Name) -like $newOwner } 60 'the port to listen'
} else {
  Set-TaskEnabled $new.Name $false
  if ((Owner-Name) -eq $oldOwner) { Say 'Node already holds it' } else {
  if ($Step -eq 'service') {
    Stop-Guardian { Get-PoolRsProcess $Root 'guardian' }
  } else {
    if ($PSCmdlet.ShouldProcess($new.Name, 'stop')) { Stop-ScheduledTask -TaskName $new.Name -ErrorAction SilentlyContinue }
    Stop-Processes (Get-PoolRsProcess $Root $new.Command) "pools-shadow $($new.Command)"
  }
  if ($WhatIfPreference) { return }
  Wait-Until { -not (Get-PortOwner $port) } 30 "port $port to be free"
  }
  Set-TaskEnabled $oldTask $true
  if ((Owner-Name) -ne $oldOwner) { Start-Task $oldTask }
  Wait-Until { (Owner-Name) -eq $oldOwner } 60 'Node to listen'
}
Wait-Until { $h = Health; $h -and $h -ne '000' -and $h -ne 'down' } 30 'the health check'
Say "port $port held by $(Owner-Name); health $(Health)"
if ($Step -eq 'service') {
  Wait-Until { (Service-Proxy) -eq $proxyBefore } 90 "the service to adopt proxy $proxyBefore"
  Say "the service runs proxy $(Service-Proxy) (the record named $proxyBefore before: $(if ((Service-Proxy) -eq $proxyBefore) { 'adopted, not restarted' } else { 'a new process' }))"
  if ($Tailnet) {
    $to = if ($Undo) { 'http://127.0.0.1:8327' } else { "http://127.0.0.1:$port" }
    if ($Undo) { Set-TaskEnabled $tailnetTask $true; Start-Task $tailnetTask }
    if ($PSCmdlet.ShouldProcess("tailscale serve :8443 -> $to", 'set')) { & tailscale serve --bg --https=8443 $to | Out-Null; Say "tailscale serve :8443 -> $to" }
    if (-not $Undo) {
      Set-TaskEnabled $tailnetTask $false
      if ($PSCmdlet.ShouldProcess($tailnetTask, 'stop')) { Stop-ScheduledTask -TaskName $tailnetTask -ErrorAction SilentlyContinue }
      Stop-Processes (Get-NodeProcess $Root 'tailnet-dashboard-proxy.mjs') 'node tailnet-dashboard-proxy.mjs'
    }
  }
}
