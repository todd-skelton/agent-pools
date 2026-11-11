#Requires -Version 7.2
<#
.SYNOPSIS
Installs Agent Pools' native Claude Code routing with one first-party/mobile identity.
.DESCRIPTION
Normal CLI/Desktop local Code launches use a Claude-only authenticated loopback proxy.
The scheduled task runs pools-rs.exe directly: no Node or PowerShell runtime wrapper.
This does not configure Desktop Chat/Cowork, cloud Code environments or other apps.
The remote machine needs the built native binary, this script and ops/ClaudeSplitConfig.ps1.
-StageOnly prepares a protected client directory without changing Claude or tasks.
-MigrateNode explicitly permits stopping the prototype's own watchdog and authenticated
proxy; its CA, local key, settings and first-party credentials are retained.
-Disable restores only the settings this installer changed; the listener remains available
for existing sessions, but its startup task is disabled. Quit those sessions before stopping it.
#>
[CmdletBinding()]
param(
  [string]$Exe,
  [string]$Url,
  [string]$ClientRoot = (Join-Path $HOME '.agent-pools\claude-split'),
  [string]$ClaudeProfile = $(if ($env:CLAUDE_CONFIG_DIR) { $env:CLAUDE_CONFIG_DIR } else { Join-Path $HOME '.claude' }),
  [Security.SecureString]$PoolKey,
  [ValidateRange(1024,65535)][int]$Port = 8326,
  [string[]]$AllowHost = @(),
  [string]$TaskName = 'Agent Pools - Claude split',
  [switch]$StageOnly,
  [switch]$MigrateNode,
  [switch]$Disable
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This installer uses Windows DPAPI and Task Scheduler.' }
. (Join-Path $PSScriptRoot '..\ops\ClaudeSplitConfig.ps1')
$ClientRoot = [IO.Path]::GetFullPath($ClientRoot)
$ClaudeProfile = [IO.Path]::GetFullPath($ClaudeProfile)
$record = Join-Path $ClientRoot 'claude-settings-rollback.json'
$settingsPath = Join-Path $ClaudeProfile 'settings.json'
function Assert-NativeTask($Task) {
  if (-not $Task) { return }
  $path = [IO.Path]::GetFullPath($Task.Actions.Execute)
  $version = Split-Path -Leaf (Split-Path -Parent $path)
  $binRoot = Split-Path -Parent (Split-Path -Parent $path)
  if ([IO.Path]::GetFileName($path) -ine 'pools-rs.exe' -or $version -notmatch '^[a-f0-9]{64}$' -or $binRoot -ine (Join-Path $ClientRoot 'bin') -or $Task.Actions.Arguments -ne ('serve-claude-split "' + $ClientRoot + '"')) { throw 'Task does not belong to this native client; left untouched.' }
}
if ($Disable) {
  $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
  Assert-NativeTask $task
  Set-ClaudeSplitSettings -SettingsPath $settingsPath -StatePath $record -Enable $false
  if ($task) { Disable-ScheduledTask -TaskName $TaskName | Out-Null }
  'Restored Claude settings; native listener left available for existing sessions, startup task disabled.'
  return
}
if (-not $Exe -or -not (Test-Path -LiteralPath $Exe -PathType Leaf)) { throw '-Exe must name a built pools-rs.exe.' }
if ($StageOnly -and $MigrateNode) { throw '-StageOnly cannot stop or migrate a running proxy.' }
$oldConfig = if (Test-Path -LiteralPath (Join-Path $ClientRoot 'splitter.json')) { Get-Content -LiteralPath (Join-Path $ClientRoot 'splitter.json') -Raw | ConvertFrom-Json } else { $null }
if (-not $Url) { $Url = $oldConfig.poolOrigin }
if (-not $Url) { throw '-Url must name the HTTPS pool origin (loopback HTTP is also allowed).' }
$uri = [uri]$Url
if ($uri.UserInfo -or $uri.Query -or $uri.Fragment -or $uri.AbsolutePath -ne '/' -or ($uri.Scheme -ne 'https' -and -not ($uri.Scheme -eq 'http' -and $uri.IsLoopback))) { throw 'Use an HTTPS pool origin, or loopback HTTP, without credentials/path/query.' }
$Url = $uri.GetLeftPart([UriPartial]::Authority)
if ($oldConfig) {
  if ($PSBoundParameters.ContainsKey('Port') -and $Port -ne $oldConfig.port) { throw 'An existing client must retain its port; use a different client directory to reconfigure.' }
  if ($Url -ne $oldConfig.poolOrigin) { throw 'An existing client must retain its pool origin; use a different client directory to reconfigure.' }
  $Port = [int]$oldConfig.port
  if (-not $PSBoundParameters.ContainsKey('AllowHost')) { $AllowHost = @($oldConfig.additionalTunnelHosts) }
}
foreach ($hostName in $AllowHost) { if ($hostName -notmatch '^[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?$' -or $hostName.Contains('..')) { throw 'AllowHost entries must be exact lowercase DNS names.' } }
if (-not $PoolKey) {
  $legacy = Join-Path $ClientRoot 'pool-key.clixml'
  if (Test-Path -LiteralPath $legacy) { $PoolKey = (Import-Clixml -LiteralPath $legacy).Password }
  elseif (-not (Test-Path -LiteralPath (Join-Path $ClientRoot 'pool-key.dpapi'))) { $PoolKey = Read-Host 'Registered Agent Pools machine key' -AsSecureString }
}
# Protect the client directory before writing any key, backup, or native binary.
[IO.Directory]::CreateDirectory($ClientRoot) | Out-Null
if (-not [IO.Directory]::Exists($ClientRoot)) { throw 'Client directory creation failed.' }
$acl = [Security.AccessControl.DirectorySecurity]::new()
$acl.SetAccessRuleProtection($true, $false)
$owner = [Security.Principal.WindowsIdentity]::GetCurrent().User
foreach ($sid in @($owner, [Security.Principal.SecurityIdentifier]::new('S-1-5-18'))) {
  $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
}
[IO.FileSystemAclExtensions]::SetAccessControl([IO.DirectoryInfo]::new($ClientRoot), $acl)
$backup = Join-Path $ClientRoot ('backups\' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($backup) | Out-Null
foreach ($name in @('splitter.json','pool-key.dpapi','claude-settings-rollback.json')) { if (Test-Path -LiteralPath (Join-Path $ClientRoot $name)) { Copy-Item -LiteralPath (Join-Path $ClientRoot $name) -Destination (Join-Path $backup $name) } }
if (Test-Path -LiteralPath $settingsPath) { Copy-Item -LiteralPath $settingsPath -Destination (Join-Path $backup 'claude-settings.json') }
if ($PoolKey) {
  $credential = [Management.Automation.PSCredential]::new('agent-pools', $PoolKey)
  $bytes = [Text.Encoding]::UTF8.GetBytes($credential.GetNetworkCredential().Password)
  try { [IO.File]::WriteAllBytes((Join-Path $ClientRoot 'pool-key.dpapi'), [Security.Cryptography.ProtectedData]::Protect($bytes, $null, [Security.Cryptography.DataProtectionScope]::CurrentUser)) }
  finally { [Array]::Clear($bytes); $credential = $null }
}
@{ schema = 'claude-splitter/v1'; poolOrigin = $Url; port = $Port; additionalTunnelHosts = @($AllowHost) } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $ClientRoot 'splitter.json') -Encoding utf8
$hash = (Get-FileHash -LiteralPath $Exe).Hash.ToLowerInvariant()
$bin = Join-Path $ClientRoot "bin\$hash"
[IO.Directory]::CreateDirectory($bin) | Out-Null
$native = Join-Path $bin 'pools-rs.exe'
if (-not (Test-Path -LiteralPath $native)) { Copy-Item -LiteralPath $Exe -Destination $native }
if ((Get-FileHash -LiteralPath $native).Hash.ToLowerInvariant() -ne $hash) { throw 'Installed native binary hash mismatch.' }
# pools-rs.exe is intentionally windowless; wait for it instead of relying on GUI-app & semantics.
$p = Start-Process -FilePath $native -ArgumentList ('init-claude-split "' + $ClientRoot + '"') -WindowStyle Hidden -Wait -PassThru
if ($p.ExitCode -ne 0) { throw 'Native client initialization failed; no Claude settings or tasks changed.' }
if ($StageOnly) { [pscustomobject]@{ clientRoot = $ClientRoot; nativeExe = $native; staged = $true } | ConvertTo-Json; return }
$previousTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Assert-NativeTask $previousTask
if ($previousTask) { Export-ScheduledTask -TaskName $TaskName | Set-Content -LiteralPath (Join-Path $backup 'native-task.xml') }
$legacyTask = Get-ScheduledTask -TaskName 'Claude Inference Splitter' -ErrorAction SilentlyContinue
$statePath = Join-Path $ClientRoot 'probe-proxy.json'
$state = if (Test-Path -LiteralPath $statePath) { Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json } else { $null }
$localKey = [IO.File]::ReadAllText((Join-Path $ClientRoot 'proxy-key.txt'))
$headers = @{ 'Proxy-Authorization' = 'Basic ' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes('probe:' + $localKey)) }
$existingHealth = $null
try { $existingHealth = Invoke-RestMethod "http://127.0.0.1:$Port/health" -Headers $headers -NoProxy -TimeoutSec 2 } catch { }
if ($existingHealth -and $existingHealth.service -ne 'agent-pools-claude-split' -and -not $MigrateNode) { throw 'A prototype proxy is running. Use -MigrateNode to authorize the local cutover.' }
if ($MigrateNode -and $legacyTask -and ([IO.Path]::GetFileName($legacyTask.Actions.Execute) -ine 'pwsh.exe' -or $legacyTask.Actions.Arguments.IndexOf(('-File "' + (Join-Path $ClientRoot 'launcher\Watch-ClaudeSplit.ps1') + '"'), [StringComparison]::OrdinalIgnoreCase) -lt 0 -or -not $legacyTask.Actions.Arguments.EndsWith(('-Profile "' + $ClientRoot + '"'), [StringComparison]::OrdinalIgnoreCase))) { throw 'Prototype watchdog belongs to another client or has an unexpected action; left untouched.' }
$legacyWasRunning = $legacyTask -and $legacyTask.State -eq 'Running'
$nativeWasRunning = $previousTask -and $previousTask.State -eq 'Running'
try {
  if ($MigrateNode -and $legacyTask) { Export-ScheduledTask -TaskName $legacyTask.TaskName | Set-Content -LiteralPath (Join-Path $backup 'prototype-task.xml'); Disable-ScheduledTask -TaskName $legacyTask.TaskName | Out-Null; Stop-ScheduledTask -TaskName $legacyTask.TaskName }
  if ($existingHealth) {
    if (-not $state -or $state.instanceId -ne $existingHealth.instanceId) { throw 'Proxy instance/state mismatch; no shutdown sent.' }
    $headers['X-Splitter-Instance'] = $state.instanceId
    try { Invoke-RestMethod -Method Post "http://127.0.0.1:$Port/shutdown" -Headers $headers -NoProxy -TimeoutSec 3 | Out-Null }
    catch {
      # Task Scheduler may already have stopped a watchdog's child. Never kill a port owner.
      $socket = [Net.Sockets.TcpClient]::new()
      try { $socket.Connect('127.0.0.1', $Port); throw 'Existing listener did not accept authenticated shutdown.' }
      catch [Net.Sockets.SocketException] { }
      finally { $socket.Dispose() }
    }
    Start-Sleep -Milliseconds 500
  }
  if ($previousTask) { Stop-ScheduledTask -TaskName $TaskName }
  $action = New-ScheduledTaskAction -Execute $native -Argument ('serve-claude-split "' + $ClientRoot + '"') -WorkingDirectory $ClientRoot
  $user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
  $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
  $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
  $opts = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable
  Register-ScheduledTask -TaskName $TaskName -Action $action -Principal $principal -Trigger $trigger -Settings $opts -Description 'Agent Pools native Claude Code inference/control routing.' -Force | Out-Null
  Start-ScheduledTask -TaskName $TaskName
  $healthy = $false
  for ($i = 0; $i -lt 60; $i++) {
    try { $h = Invoke-RestMethod "http://127.0.0.1:$Port/health" -Headers $headers -NoProxy -TimeoutSec 1; $healthy = $h.service -eq 'agent-pools-claude-split' -and $h.certificateValid } catch { }
    if ($healthy) { break }; Start-Sleep -Milliseconds 250
  }
  if (-not $healthy) { throw 'Native proxy did not become healthy.' }
  $p = Start-Process -FilePath $native -ArgumentList ('check-claude-split "' + $ClientRoot + '"') -WindowStyle Hidden -Wait -PassThru
  if ($p.ExitCode -ne 0) { throw 'Native TLS/pool model discovery failed; Claude settings were not applied.' }
  $proxy = 'http://probe:' + [uri]::EscapeDataString($localKey) + "@127.0.0.1:$Port"
  $envSettings = @{ HTTP_PROXY = $proxy; HTTPS_PROXY = $proxy; NO_PROXY = 'localhost,127.0.0.1,::1'; NODE_EXTRA_CA_CERTS = (Join-Path $ClientRoot 'probe-certs\ca.pem') }
  Set-ClaudeSplitSettings -SettingsPath $settingsPath -StatePath $record -Environment $envSettings -Enable $true
} catch {
  if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) { Stop-ScheduledTask -TaskName $TaskName; Disable-ScheduledTask -TaskName $TaskName | Out-Null }
  foreach ($name in @('splitter.json','pool-key.dpapi','claude-settings-rollback.json')) { if (Test-Path -LiteralPath (Join-Path $backup $name)) { Copy-Item -LiteralPath (Join-Path $backup $name) -Destination (Join-Path $ClientRoot $name) -Force } }
  if (Test-Path -LiteralPath (Join-Path $backup 'claude-settings.json')) { Copy-Item -LiteralPath (Join-Path $backup 'claude-settings.json') -Destination $settingsPath -Force }
  if (Test-Path -LiteralPath (Join-Path $backup 'native-task.xml')) { Register-ScheduledTask -TaskName $TaskName -Xml (Get-Content -LiteralPath (Join-Path $backup 'native-task.xml') -Raw) -Force | Out-Null; if ($nativeWasRunning) { Start-ScheduledTask -TaskName $TaskName } }
  if (Test-Path -LiteralPath (Join-Path $backup 'prototype-task.xml')) { Register-ScheduledTask -TaskName 'Claude Inference Splitter' -Xml (Get-Content -LiteralPath (Join-Path $backup 'prototype-task.xml') -Raw) -Force | Out-Null; if ($legacyWasRunning) { Start-ScheduledTask -TaskName 'Claude Inference Splitter' } }
  throw 'Native cutover failed; backed-up settings/task definitions were restored. Check local task state before continuing.'
}
[pscustomobject]@{ clientRoot = $ClientRoot; nativeExe = $native; task = $TaskName; defaultClaudeProfile = $ClaudeProfile; backup = $backup; nodeRuntimeRequired = $false } | ConvertTo-Json
Write-Warning 'Desktop must use signed-in local Code mode, not an applied gateway entry or cloud environment. Sign into Claude.ai and enable Remote Control in Claude when needed; this installer does not change gateway-library entries or consent.'
