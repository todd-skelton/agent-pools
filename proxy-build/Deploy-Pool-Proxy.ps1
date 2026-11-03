<#
.SYNOPSIS
Build, install and activate a pool proxy version, or roll back to the previous one.

.DESCRIPTION
-Version X builds through Build-Pool-Proxy.ps1, installs to proxy/X/cli-proxy-api.exe,
backs up state/install.json, points proxyBinary at the new build and restarts the service
(deploy\Restart-Pool-Rs.ps1 -Step service). The service's takeover plan retires the proxy that
is no longer install.json's and starts the new one; this script then checks the running proxy
executable. Once the new build is serving, proxy/ is pruned to the original 7.2.158, the build
just replaced, and the live one.

-Rollback restores the most recent install.json backup and restarts.

Run this when no turn is streaming through the pool: the old proxy is stopped, so in-flight
requests on it fail. The replacement is serving within a few seconds. The script refuses to
deploy while the service reports requests held open by the proxy's outage hold (a restart fails
every one of them); pass -Force to deploy anyway. The live install is refused without -Live.
#>
[CmdletBinding(DefaultParameterSetName = 'Deploy')]
param(
  [Parameter(ParameterSetName = 'Deploy', Mandatory)][ValidatePattern('^7\.2\.158-pool\.\d+$')][string]$Version,
  [Parameter(ParameterSetName = 'Deploy')][switch]$SkipBuild,
  [Parameter(ParameterSetName = 'Deploy')][switch]$Force,
  [Parameter(ParameterSetName = 'Rollback', Mandatory)][switch]$Rollback,
  [Parameter(Mandatory)][string]$Root,
  [switch]$Live
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\deploy\common.ps1')
$target = Resolve-PoolRoot $Root $Live.IsPresent
$poolRoot = $target.Root
$installPath = Join-Path $poolRoot 'state/install.json'
$backupDir = Join-Path $poolRoot 'state/install-history'
[IO.Directory]::CreateDirectory($backupDir) | Out-Null

function Wait-ProxyRunning([string]$expectedRelative) {
  $expected = [IO.Path]::GetFullPath((Join-Path $poolRoot $expectedRelative)).ToLowerInvariant()
  $deadline = [DateTime]::UtcNow.AddSeconds(90)
  do {
    Start-Sleep -Seconds 2
    $running = Get-CimInstance Win32_Process -Filter "Name='cli-proxy-api.exe'" | Where-Object { $_.ExecutablePath -and ([IO.Path]::GetFullPath($_.ExecutablePath).ToLowerInvariant() -eq $expected) }
    if ($running) {
      try {
        $r = Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:8317/' -TimeoutSec 5
        if ($r.StatusCode -eq 200) { return $running }
      } catch {}
    }
  } while ([DateTime]::UtcNow -lt $deadline)
  throw "Proxy did not come up from $expectedRelative within 90 seconds; check logs/main.log and logs/pool-events.jsonl"
}

function Assert-NoHeldRequests {
  $servicePort = 8318
  try { $servicePort = (Get-Content -LiteralPath $installPath -Raw | ConvertFrom-Json).supervisorPort } catch {}
  if (-not $servicePort) { $servicePort = 8318 }
  try {
    $status = Invoke-RestMethod -Uri "http://127.0.0.1:$servicePort/api/status" -TimeoutSec 5
  } catch {
    Write-Warning "Service status unavailable on port $servicePort; cannot check for held requests ($($_.Exception.Message))"
    return
  }
  $active = 0
  if ($status.holds -and $status.holds.active) { $active = [int]$status.holds.active }
  if ($active -gt 0) {
    if ($Force) { Write-Warning "Deploying with $active request(s) held by the outage hold; they will fail when the proxy restarts" }
    else { throw "Refusing to deploy: $active request(s) are held open by the proxy's outage hold and would fail on restart. Wait for the hold to clear or pass -Force." }
  }
}

if ($Rollback) {
  $previous = Get-ChildItem -LiteralPath $backupDir -Filter 'install-*.json' | Sort-Object Name -Descending | Select-Object -First 1
  if (-not $previous) { throw 'No install.json backup to roll back to' }
  $proxyBinary = (Get-Content -LiteralPath $previous.FullName -Raw | ConvertFrom-Json).proxyBinary
  Copy-Item -LiteralPath $previous.FullName -Destination $installPath -Force
  Write-Output "Restored $($previous.Name) (proxyBinary = $proxyBinary)"
} else {
  Assert-NoHeldRequests
  $proxyBinary = "proxy/$Version/cli-proxy-api.exe"
  $targetDir = Join-Path $poolRoot "proxy/$Version"
  $candidate = Join-Path $poolRoot 'build/candidate.exe'
  if (-not $SkipBuild) { & (Join-Path $PSScriptRoot 'Build-Pool-Proxy.ps1') -Version $Version -Root $poolRoot -Output $candidate }
  if (-not (Test-Path -LiteralPath $candidate)) { throw "Candidate $candidate is missing" }
  $reported = (& $candidate --version 2>&1 | Select-Object -First 1)
  if ($reported -notmatch [regex]::Escape("Version: $Version,")) { throw "Candidate reports '$reported', expected $Version" }
  [IO.Directory]::CreateDirectory($targetDir) | Out-Null
  if (-not (Test-Path -LiteralPath $targetDir)) { throw "Could not create $targetDir" }
  Copy-Item -LiteralPath $candidate -Destination (Join-Path $targetDir 'cli-proxy-api.exe') -Force
  Get-FileHash -LiteralPath (Join-Path $targetDir 'cli-proxy-api.exe') -Algorithm SHA256 |
    Select-Object @{n='binary';e={"proxy/$Version/cli-proxy-api.exe"}}, Hash, @{n='builtAt';e={[DateTime]::UtcNow.ToString('o')}} |
    ConvertTo-Json | Set-Content -LiteralPath (Join-Path $targetDir 'build-manifest.json') -Encoding utf8
  Copy-Item -LiteralPath $installPath -Destination (Join-Path $backupDir ("install-" + [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ') + '.json'))
  $install = Get-Content -LiteralPath $installPath -Raw | ConvertFrom-Json
  $replaced = ($install.proxyBinary -split '/')[-2]
  $install.proxyBinary = $proxyBinary
  $install | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $installPath -Encoding utf8
  Write-Output "install.json now selects $proxyBinary"
}

$restart = @{ Root = $poolRoot; Step = 'service'; ProxyCutover = $true }
if ($target.Live) { $restart.Live = $true }
& (Join-Path $PSScriptRoot '..\deploy\Restart-Pool-Rs.ps1') @restart
$proc = Wait-ProxyRunning $proxyBinary
Write-Output "Proxy running: pid $($proc.ProcessId) from $($proc.ExecutablePath)"
if (-not $Rollback) {
  $keep = @('7.2.158', $replaced, $Version)
  Get-ChildItem -LiteralPath (Join-Path $poolRoot 'proxy') -Directory | Where-Object { $keep -notcontains $_.Name } | ForEach-Object {
    Remove-Item -LiteralPath $_.FullName -Recurse -Force
    Write-Output "Removed superseded build $($_.Name)"
  }
}
