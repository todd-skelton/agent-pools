<#
.SYNOPSIS
Registers, lists or removes machines allowed to use the public pool URL.

.DESCRIPTION
Each remote machine gets its own key. Only the key's SHA-256 hash is stored, in
state/remote-clients.json, which the public gateway (pools-rs serve-gateway) re-reads on change,
so adding or removing a machine takes effect immediately without a restart.
-Add prints nothing secret: the new key goes to the clipboard once; paste it
into the remote machine's installer (remote-client\Connect-AgentPools.ps1) or the
dashboard sign-in page there. Removing a machine also ends its dashboard sessions.
-Root is the install (default: the live one).

.EXAMPLE
./ops/Remote-Client.ps1 -Add work-laptop
./ops/Remote-Client.ps1 -List
./ops/Remote-Client.ps1 -Remove work-laptop
#>
[CmdletBinding(DefaultParameterSetName = 'List')]
param(
  [Parameter(ParameterSetName = 'Add', Mandatory)][ValidatePattern('^[A-Za-z0-9][A-Za-z0-9_.-]{0,47}$')][string]$Add,
  [Parameter(ParameterSetName = 'Add')][switch]$Replace,
  [Parameter(ParameterSetName = 'Remove', Mandatory)][string]$Remove,
  [Parameter(ParameterSetName = 'List')][switch]$List,
  [string]$Root
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\deploy\common.ps1')
if (-not $Root) { $Root = $script:LiveRoot }
$path = Join-Path $Root 'state/remote-clients.json'
$secretsPath = Join-Path $Root 'state/secrets.json'
$utf8 = [Text.UTF8Encoding]::new($false)

# Same <file>.lock convention as Set-Pool-Secret.ps1.
function Use-Lock([string]$file, [scriptblock]$body) {
  $lock = "$file.lock"
  $deadline = [DateTime]::UtcNow.AddSeconds(60)
  while ($true) {
    try {
      $handle = [IO.File]::Open($lock, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
      $bytes = $utf8.GetBytes((@{ pid = $PID; at = [DateTime]::UtcNow.ToString('o') } | ConvertTo-Json -Compress))
      $handle.Write($bytes, 0, $bytes.Length); $handle.Dispose()
      break
    } catch [IO.IOException] {
      $item = Get-Item -LiteralPath $lock -ErrorAction SilentlyContinue
      if ($item -and ([DateTime]::UtcNow - $item.LastWriteTimeUtc).TotalMinutes -gt 30) { Remove-Item -LiteralPath $lock -Force; continue }
      if ([DateTime]::UtcNow -gt $deadline) { throw "Lock busy: $lock" }
      Start-Sleep -Milliseconds 250
    }
  }
  try { & $body } finally { Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue }
}
function Write-Atomic([string]$file, [string]$text) {
  $tmp = "$file.$PID.$([Guid]::NewGuid().ToString('N').Substring(0, 8)).tmp"
  [IO.File]::WriteAllText($tmp, $text, $utf8)
  Move-Item -LiteralPath $tmp -Destination $file -Force
}
function Read-Clients {
  if (-not (Test-Path -LiteralPath $path)) { return @() }
  $doc = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
  @($doc.clients | Where-Object { $_ })
}
function Save-Clients($clients) {
  $doc = [ordered]@{ schema = 'remote-clients/v1'; clients = @($clients) }
  Write-Atomic $path ($doc | ConvertTo-Json -Depth 4)
}
function New-Secret([int]$bytes) {
  $buf = [byte[]]::new($bytes)
  [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($buf)
  [Convert]::ToBase64String($buf).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

if ($PSCmdlet.ParameterSetName -eq 'Add') {
  # The gateway signs dashboard cookies with this; create it once.
  Use-Lock $secretsPath {
    $secrets = [ordered]@{}
    (Get-Content -LiteralPath $secretsPath -Raw | ConvertFrom-Json).PSObject.Properties | ForEach-Object { $secrets[$_.Name] = $_.Value }
    if (-not $secrets['publicGatewaySessionSecret']) {
      $secrets['publicGatewaySessionSecret'] = New-Secret 32
      Write-Atomic $secretsPath ($secrets | ConvertTo-Json)
      Write-Output 'Created publicGatewaySessionSecret in state/secrets.json.'
    }
  }
  $key = 'ap-m-' + (New-Secret 32)
  $sha = [Security.Cryptography.SHA256]::Create()
  $hash = -join ($sha.ComputeHash($utf8.GetBytes($key)) | ForEach-Object { $_.ToString('x2') })
  Use-Lock $path {
    $clients = @(Read-Clients)
    $existing = @($clients | Where-Object { $_.name -eq $Add })
    if ($existing.Count -and -not $Replace) { throw "Machine '$Add' is already registered. Use -Replace to issue it a new key (the old key and its dashboard sessions stop working)." }
    $clients = @($clients | Where-Object { $_.name -ne $Add })
    $clients += [ordered]@{ name = $Add; keySha256 = $hash; addedAt = [DateTime]::UtcNow.ToString('o') }
    Save-Clients $clients
  }
  Set-Clipboard -Value $key
  Write-Output "Registered '$Add'. Its key is on the clipboard (shown nowhere else; run -Add $Add -Replace for a new one)."
  return
}
if ($PSCmdlet.ParameterSetName -eq 'Remove') {
  Use-Lock $path {
    $clients = @(Read-Clients)
    if (-not @($clients | Where-Object { $_.name -eq $Remove }).Count) { throw "No machine named '$Remove'." }
    Save-Clients @($clients | Where-Object { $_.name -ne $Remove })
  }
  Write-Output "Removed '$Remove'. Its key and dashboard sessions stop working now."
  return
}
$clients = @(Read-Clients)
if (-not $clients.Count) { Write-Output 'No machines registered.'; return }
$clients | ForEach-Object { [pscustomobject]@{ Name = $_.name; Added = $_.addedAt; Disabled = [bool]$_.disabled } } | Format-Table -AutoSize
