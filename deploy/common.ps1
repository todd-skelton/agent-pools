# Shared by Install-Pool-Rs.ps1 and Cutover-Pool-Rs.ps1.
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

# UTF-8 without a byte-order mark (the wrapper reads it as text).
function Write-Json([string]$Path, $Value) {
  [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 6) + "`n", [Text.UTF8Encoding]::new($false))
}

# The three tasks, as Agent Pools registers its own (Register-Pool-Tasks.ps1, Set-Inference-Relay.ps1,
# Set-Public-Gateway.ps1): logon plus a 5-minute keepalive, IgnoreNew, no time limit.
function Get-PoolRsTasks([string]$Prefix) {
  @(
    [pscustomobject]@{ Key = 'service';   Name = "$Prefix - subscriptions";   Command = 'guardian';        Restart = $false; Priority = $null; Description = 'Agent Pools on Corten: the guardian, the service and the proxy.' }
    [pscustomobject]@{ Key = 'inference'; Name = "$Prefix - inference relay"; Command = 'serve-inference'; Restart = $true;  Priority = $null; Description = 'Agent Pools on Corten: the inference relay.' }
    [pscustomobject]@{ Key = 'gateway';   Name = "$Prefix - public gateway";  Command = 'serve-gateway';   Restart = $true;  Priority = 4;     Description = 'Agent Pools on Corten: the public gateway.' }
  )
}

# The port's processes the wrapper started for $Root with $Command.
function Get-PoolRsProcess([string]$Root, [string]$Command) {
  $want = "$Command `"$Root`""
  Get-CimInstance Win32_Process -Filter "Name='pools-shadow.exe'" | Where-Object { $_.CommandLine -and $_.CommandLine.IndexOf($want, [StringComparison]::OrdinalIgnoreCase) -ge 0 }
}

# Agent Pools' node processes running $Root\$Script.
function Get-NodeProcess([string]$Root, [string]$Script) {
  $path = Join-Path $Root $Script
  Get-CimInstance Win32_Process -Filter "Name='node.exe'" | Where-Object { $_.CommandLine -and $_.CommandLine.IndexOf($path, [StringComparison]::OrdinalIgnoreCase) -ge 0 }
}

function Get-PortOwner([int]$Port) {
  $id = Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort $Port -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty OwningProcess
  if ($id) { Get-CimInstance Win32_Process -Filter "ProcessId=$id" }
}

function Wait-Until([scriptblock]$Test, [int]$Seconds, [string]$What) {
  for ($i = 0; $i -lt $Seconds; $i++) { if (& $Test) { return }; Start-Sleep 1 }
  throw "timed out after $Seconds s waiting for $What"
}
