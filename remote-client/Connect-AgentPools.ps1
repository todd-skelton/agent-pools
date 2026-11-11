<#
.SYNOPSIS
Points Claude Desktop on this machine at the Agent Pools URL. No admin rights,
no service, nothing running in the background.

.DESCRIPTION
Run on the remote Windows machine:

  powershell -ExecutionPolicy Bypass -File .\Connect-AgentPools.ps1 -Url https://<the pool's public hostname>

1. Asks for this machine's pool key (hidden input; the pool host issues it with
   ops\Remote-Client.ps1 -Add <name>) and checks it against <Url>/v1/models.
2. Writes Claude Desktop's gateway entry 'Agent Pools' in
   %LOCALAPPDATA%\Claude-3p\configLibrary (gateway URL, x-api-key, static key,
   fixed model list, discovery off) and makes it the applied entry.
   An 'Agent Pools' entry from the earlier Tailscale forwarder is updated in
   place; any other existing gateway setup is left alone unless -Force.
3. Removes the old Tailscale forwarder (exe, Startup shortcut, device identity)
   if it is still installed.

The dashboard is at <Url>/ in any browser; sign in there with the same key.
-Uninstall removes the 'Agent Pools' gateway entry (Claude Desktop goes back
to its normal sign-in after a restart).
#>
[CmdletBinding()]
param(
  # The pool's public URL (install.json publicGateway.hostname on the pool host); not needed with -Uninstall.
  [string]$Url,
  [string[]]$Models = @('claude-sonnet-5-5', 'claude-opus-5-5', 'claude-fable-5-1'),
  # Keep first-party/mobile identity for local Code instead of Desktop's gateway mode.
  [switch]$SignedInCode,
  [string]$Exe,
  [string]$ClientRoot = (Join-Path $HOME '.agent-pools\claude-split'),
  [switch]$MigrateNode,
  [switch]$Force,
  [switch]$Uninstall
)
$ErrorActionPreference = 'Stop'
if ($SignedInCode) {
  $argsForCode = @{ ClientRoot = $ClientRoot; MigrateNode = $MigrateNode; Disable = $Uninstall }
  if ($Exe) { $argsForCode.Exe = $Exe }
  if ($Url) { $argsForCode.Url = $Url }
  & (Join-Path $PSScriptRoot 'Connect-ClaudeCode.ps1') @argsForCode
  return
}
if (-not $Uninstall -and -not $Url) { throw 'Pass -Url https://<the pool''s public hostname>.' }
# Windows PowerShell 5.1 may still offer TLS 1.0 first; Cloudflare needs 1.2+.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
if ($Url) { $Url = $Url.TrimEnd('/') }
$library = Join-Path $env:LOCALAPPDATA 'Claude-3p\configLibrary'
$meta = Join-Path $library '_meta.json'
$entryName = 'Agent Pools'
$utf8 = [Text.UTF8Encoding]::new($false)

function Read-Meta {
  if (-not (Test-Path -LiteralPath $meta)) { return $null }
  Get-Content -LiteralPath $meta -Raw | ConvertFrom-Json
}
function Write-Json([string]$path, $value) { [IO.File]::WriteAllText($path, ($value | ConvertTo-Json -Depth 6), $utf8) }

if ($Uninstall) {
  $m = Read-Meta
  $ours = @($m.entries | Where-Object { $_.name -eq $entryName })
  if (-not $ours.Count) { Write-Output "No '$entryName' gateway entry found."; return }
  foreach ($e in $ours) { Remove-Item -LiteralPath (Join-Path $library "$($e.id).json") -Force -ErrorAction SilentlyContinue }
  $rest = @($m.entries | Where-Object { $_.name -ne $entryName })
  if ($rest.Count) {
    $applied = if ($ours.id -contains $m.appliedId) { $rest[0].id } else { $m.appliedId }
    Write-Json $meta ([ordered]@{ appliedId = $applied; entries = $rest })
  } else { Remove-Item -LiteralPath $meta -Force }
  Write-Output "Removed the '$entryName' gateway entry. Quit Claude Desktop from the tray and start it again."
  return
}

# 1. Key and reachability.
$secure = Read-Host -AsSecureString -Prompt 'Pool key for this machine (input hidden)'
$bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
try { $key = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr).Trim() }
finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
if (-not $key) { throw 'No key entered.' }
try {
  $resp = Invoke-RestMethod -Uri "$Url/v1/models" -Headers @{ 'x-api-key' = $key; 'anthropic-version' = '2023-06-01' } -TimeoutSec 30
} catch {
  $code = if ($_.Exception.Response) { [int]$_.Exception.Response.StatusCode } else { 0 }
  if ($code -eq 401) { throw 'The pool did not accept that key. Issue one on the pool host with ops\Remote-Client.ps1 -Add <name>.' }
  if ($code -eq 0) { throw "Could not reach $Url ($($_.Exception.Message)). If a browser here also cannot open $Url/login, the network is blocking it." }
  throw "The pool answered HTTP $($code): $($_.Exception.Message)"
}
$available = @($resp.data.id)
$missing = @($Models | Where-Object { $available -notcontains $_ })
Write-Output "Pool reachable at $($Url): $($available.Count) models listed."
if ($missing) { Write-Warning "Not offered by the pool right now: $($missing -join ', ')" }

# 2. Claude Desktop gateway entry.
$entry = [ordered]@{
  inferenceGatewayBaseUrl    = $Url
  inferenceGatewayApiKey     = $key
  inferenceGatewayAuthScheme = 'x-api-key'
  inferenceProvider          = 'gateway'
  inferenceCredentialKind    = 'static'
  inferenceModels            = @($Models)
  modelDiscoveryEnabled      = $false
}
New-Item -ItemType Directory -Force -Path $library | Out-Null
$m = Read-Meta
$ours = @($m.entries | Where-Object { $_.name -eq $entryName }) | Select-Object -First 1
if ($m -and -not $ours -and -not $Force) {
  Write-Warning "Claude Desktop already has another gateway configuration ($library); not changing it. Rerun with -Force to add '$entryName' and make it the active one."
  Write-Output "Or enter these in Claude Desktop's gateway setup: URL $Url, auth scheme x-api-key, the key you just entered, models $($Models -join ', ')."
  return
}
$id = if ($ours) { $ours.id } else { [guid]::NewGuid().ToString() }
Write-Json (Join-Path $library "$id.json") $entry
$entries = @($m.entries | Where-Object { $_ -and $_.id -ne $id }) + @([ordered]@{ id = $id; name = $entryName })
Write-Json $meta ([ordered]@{ appliedId = $id; entries = $entries })
Write-Output "Claude Desktop's '$entryName' gateway entry now points at $Url."

# 3. Old Tailscale forwarder, if any.
$fwdDir = Join-Path $env:LOCALAPPDATA 'AgentPoolsForwarder'
$fwdLink = Join-Path ([Environment]::GetFolderPath('Startup')) 'Agent Pools forwarder.lnk'
if ((Test-Path -LiteralPath $fwdDir) -or (Test-Path -LiteralPath $fwdLink)) {
  Get-Process pool-forwarder -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$fwdDir\*" } | Stop-Process -Force
  Start-Sleep -Milliseconds 500
  if (Test-Path -LiteralPath $fwdLink) { Remove-Item -LiteralPath $fwdLink -Force }
  if (Test-Path -LiteralPath $fwdDir) { Remove-Item -LiteralPath $fwdDir -Recurse -Force }
  Write-Output 'Removed the old Tailscale forwarder.'
}
Write-Output "Done. Quit Claude Desktop completely (tray icon > Quit) and start it again. Dashboard: $Url/ (sign in with the same key)."
