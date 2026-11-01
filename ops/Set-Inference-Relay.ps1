<#
.SYNOPSIS
Routes native openai inference through the pool, without changing desktop login
or the backend/remote-control relay. Disable restores only the prior endpoint.
Disable refuses while the default model_provider is openai (the desktop default
since 2026-09-23): set model_provider = "account_pool" first so new tasks keep
using the pool.

.DESCRIPTION
-Enable starts the port's inference relay task ("<TaskPrefix> - inference relay", registered by
deploy\Install-Pool-Rs.ps1 -RegisterTasks; enabled here when it is not), waits for
https://127.0.0.1:<inferenceRelayPort>/health, and only then sets openai_base_url in
~/.codex/config.toml to https://localhost:<port>/v1 (InferenceConfig.ps1). The value it
replaced is kept in state/inference-config.json; -Disable puts it back and deletes the record.
-Disable leaves the relay running: tasks started with the old endpoint keep their streams.
-Root is the install (default: the live one).
#>
[CmdletBinding()]
param(
  [Parameter(ParameterSetName='Enable', Mandatory)][switch]$Enable,
  [Parameter(ParameterSetName='Disable', Mandatory)][switch]$Disable,
  [string]$Root,
  [string]$ConfigPath = (Join-Path $HOME '.codex\config.toml'),
  [string]$TaskPrefix = 'Agent Pools (rs)'
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\deploy\common.ps1')
. (Join-Path $PSScriptRoot 'InferenceConfig.ps1')
if (-not $Root) { $Root = $script:LiveRoot }
$Root = (Resolve-Path -LiteralPath $Root).ProviderPath.TrimEnd('\')
$install = Get-Content -LiteralPath (Join-Path $Root 'state/install.json') -Raw | ConvertFrom-Json
$port = if ($install.inferenceRelayPort) { $install.inferenceRelayPort } else { 8319 }
$endpoint = "https://localhost:$port/v1"
$statePath = Join-Path $Root 'state/inference-config.json'
if ($Disable) {
  Set-InferenceEndpoint -ConfigPath $ConfigPath -StatePath $statePath -Endpoint $endpoint -Enable $false
  # Old tasks may still reference this endpoint. Leave the listener available
  # until those tasks have been closed; rollback must not sever their streams.
  Write-Output 'Restored the native OpenAI inference endpoint. Relay left running for existing tasks. Relaunch Codex to use the restored endpoint.'
  return
}
$authPath = Join-Path $HOME '.codex/auth.json'
$auth = Get-Content -LiteralPath $authPath -Raw | ConvertFrom-Json
if (-not $auth.tokens.access_token -and -not $auth.OPENAI_API_KEY) { throw 'No file-backed desktop credential found.' }
$task = Get-PoolRsTasks $TaskPrefix | Where-Object Key -eq 'inference'
$scheduled = Get-ScheduledTask -TaskName $task.Name -ErrorAction SilentlyContinue
if (-not $scheduled) { throw "Task '$($task.Name)' is not registered; run deploy\Install-Pool-Rs.ps1 -RegisterTasks first." }
if ($scheduled.State -eq 'Disabled') { Enable-ScheduledTask -TaskName $task.Name | Out-Null }
if ((Get-ScheduledTask -TaskName $task.Name).State -ne 'Running') { Start-ScheduledTask -TaskName $task.Name }
$deadline = [DateTime]::UtcNow.AddSeconds(45)
$healthy = $false
do {
  try {
    # Avoid IPv6-first localhost fallback eating the short startup deadline.
    # The existing certificate includes 127.0.0.1 as an IP SAN.
    $health = Invoke-RestMethod "https://127.0.0.1:$port/health" -TimeoutSec 5
    $healthy = $health.service -eq 'agent-pools-inference'
  } catch { }
  if (-not $healthy) { Start-Sleep -Milliseconds 500 }
} while (-not $healthy -and [DateTime]::UtcNow -lt $deadline)
if (-not $healthy) { throw 'Inference listener not healthy; endpoint configuration was not changed.' }
Set-InferenceEndpoint -ConfigPath $ConfigPath -StatePath $statePath -Endpoint $endpoint -Enable $true
Write-Output "Inference relay ready at https://localhost:$port (loopback only); openai_base_url points at it. Existing backend relay and login settings were not changed. Desktop reload/relaunch may be required."
