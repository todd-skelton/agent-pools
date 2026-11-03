<#
.SYNOPSIS
Starts the port's commands again on pool-rs.json's current version (after Install-Pool-Rs.ps1).

.DESCRIPTION
  service    state\guardian-stop.request stops the guardian, which stops its service gracefully and
             leaves the proxy running; the task starts a new guardian, whose service adopts the same
             proxy (state\process-proxy.json). About 10-30 s without the dashboard and the API.
  inference  The task and its process are stopped and the task started: Codex turns in flight
  gateway    through that relay or the gateway are cut, so pick an idle moment.
  tunnel     The same for cloudflared; the public hostname is unreachable for a few seconds.
  all        inference, gateway, tunnel, then service.

Only tasks that are enabled are restarted. The live install is refused without -Live.
-ProxyCutover verifies the proxy selected by install.json instead of requiring the old PID.
#>
[CmdletBinding(SupportsShouldProcess)]
param(
  [Parameter(Mandatory)][string]$Root,
  [Parameter(Mandatory)][ValidateSet('service', 'inference', 'gateway', 'tunnel', 'all')][string]$Step,
  [string]$TaskPrefix = 'Agent Pools (rs)',
  [switch]$ProxyCutover,
  [switch]$Live
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
$target = Resolve-PoolRoot $Root $Live.IsPresent
$Root = $target.Root
$install = Read-Json (Join-Path $Root 'state\install.json')
$current = (Read-Json (Join-Path $Root 'pool-rs\pool-rs.json')).current
$tasks = Get-PoolRsTasks $TaskPrefix

function Say([string]$s) { "$((Get-Date).ToUniversalTime().ToString('HH:mm:ss'))Z $s" }
function Running([string]$command) { @(Get-PoolRsProcess $Root $command) }
function Version-Of($p) { if ($p.ExecutablePath -match '\\pool-rs\\([^\\]+)\\[^\\]+$') { $Matches[1] } else { $p.ExecutablePath } }
function Port-For([string]$key) {
  switch ($key) {
    'service' { if ($install.supervisorPort) { [int]$install.supervisorPort } else { 8318 } }
    'inference' { if ($install.inferenceRelayPort) { [int]$install.inferenceRelayPort } else { 8319 } }
    'gateway' { if ($install.publicGateway.port) { [int]$install.publicGateway.port } else { 8330 } }
  }
}
function Tunnel-Processes {
  $config = Join-Path $Root 'state\cloudflared\config.yml'
  @(Get-CimInstance Win32_Process -Filter "Name='cloudflared.exe'" | Where-Object { $_.CommandLine -and $_.CommandLine.IndexOf($config, [StringComparison]::OrdinalIgnoreCase) -ge 0 })
}

function Restart-One($t) {
  $task = Get-ScheduledTask -TaskName $t.Name -ErrorAction SilentlyContinue
  if (-not $task) { Say "no task '$($t.Name)'"; return }
  if (-not $task.Settings.Enabled) { Say "'$($t.Name)' is disabled; not started"; return }
  $was = if ($t.Key -eq 'tunnel') { Tunnel-Processes } else { Running $t.Command }
  Say "$($t.Key): running $(@($was | ForEach-Object { "$($_.ProcessId) ($(Version-Of $_))" }) -join ', '); starting $current"
  if (-not $PSCmdlet.ShouldProcess($t.Name, 'restart')) { return }
  if ($t.Key -eq 'service') {
    $proxy = [int](Read-Json (Join-Path $Root 'state\process-proxy.json')).pid
    $stop = Join-Path $Root 'state\guardian-stop.request'
    New-Item -ItemType File -Force -Path $stop | Out-Null
    Wait-Until { -not (Running 'guardian') -and -not (Running 'serve-service') -and -not (Get-PortOwner (Port-For 'service')) } 90 'the guardian and its service to stop'
    Say "stopped; stop file left: $(Test-Path -LiteralPath $stop)"
    Start-ScheduledTask -TaskName $t.Name
    Wait-Until { try { (Invoke-RestMethod "http://127.0.0.1:$(Port-For 'service')/api/health" -TimeoutSec 3).service -eq 'agent-pools' } catch { $false } } 90 'the service to answer'
    if ($ProxyCutover) {
      $expectedProxy = [IO.Path]::GetFullPath((Join-Path $Root $install.proxyBinary))
      Wait-Until {
        try {
          $selected = @((Invoke-RestMethod "http://127.0.0.1:$(Port-For 'service')/api/status" -TimeoutSec 10).processes | Where-Object name -eq 'proxy')[0].pid
          $running = Get-CimInstance Win32_Process -Filter "ProcessId=$selected"
          $running.ExecutablePath -and ([IO.Path]::GetFullPath($running.ExecutablePath) -ieq $expectedProxy)
        } catch { $false }
      } 90 "the service to run selected proxy $($install.proxyBinary)"
      Say "service answering; selected proxy $($install.proxyBinary) running"
    } else {
      Wait-Until { try { @((Invoke-RestMethod "http://127.0.0.1:$(Port-For 'service')/api/status" -TimeoutSec 10).processes | Where-Object name -eq 'proxy')[0].pid -eq $proxy } catch { $false } } 90 "the service to adopt proxy $proxy"
      Say "service answering; proxy $proxy adopted, not restarted"
    }
  } else {
    Stop-ScheduledTask -TaskName $t.Name -ErrorAction SilentlyContinue
    foreach ($p in $was) { if (Get-Process -Id $p.ProcessId -ErrorAction SilentlyContinue) { Stop-Process -Id $p.ProcessId -Force } }
    if ($t.Key -ne 'tunnel') { Wait-Until { -not (Get-PortOwner (Port-For $t.Key)) } 30 "port $(Port-For $t.Key) to be free" }
    else { Wait-Until { -not (Tunnel-Processes) } 30 'cloudflared to stop' }
    Start-ScheduledTask -TaskName $t.Name
    if ($t.Key -ne 'tunnel') { Wait-Until { Get-PortOwner (Port-For $t.Key) } 60 "port $(Port-For $t.Key) to listen" }
    else { Wait-Until { Tunnel-Processes } 60 'cloudflared to start' }
  }
  $now = if ($t.Key -eq 'tunnel') { Tunnel-Processes } else { Running $t.Command }
  Say "$($t.Key): now $(@($now | ForEach-Object { "$($_.ProcessId) $($_.Name) ($(Version-Of $_))" }) -join ', ')"
}

$order = if ($Step -eq 'all') { 'inference', 'gateway', 'tunnel', 'service' } else { @($Step) }
foreach ($k in $order) { Restart-One ($tasks | Where-Object Key -eq $k) }
