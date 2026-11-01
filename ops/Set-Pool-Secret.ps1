<#
.SYNOPSIS
Stores one named secret in state/secrets.json without echoing it.

.DESCRIPTION
Prompts with a masked input, so the value never reaches the console, shell
history, or a transcript. Existing keys (clientKey, managementKey, ...) are
preserved. state/ is ACL-restricted by Protect-Pool-Files.ps1 and included in
the backup (pools-rs backup), so the new key inherits both. -Root is the
install (default: the live one).

.EXAMPLE
./ops/Set-Pool-Secret.ps1 -Name artificialAnalysisKey
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory)]
  [ValidatePattern('^[A-Za-z][A-Za-z0-9]{0,63}$')]
  [string]$Name,
  [string]$Root
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\deploy\common.ps1')
if (-not $Root) { $Root = $script:LiveRoot }
$path = Join-Path $Root 'state/secrets.json'
$secure = Read-Host -AsSecureString -Prompt "Value for $Name (input hidden)"
$bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
try { $value = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr).Trim() }
finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
if (-not $value) { throw 'Empty value; nothing written.' }
# The pool's <file>.lock convention: exclusive create, 60 s wait, a lock older
# than 30 minutes is treated as abandoned.
$lock = "$path.lock"
$deadline = [DateTime]::UtcNow.AddSeconds(60)
while ($true) {
  try {
    $handle = [IO.File]::Open($lock, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    $bytes = [Text.Encoding]::UTF8.GetBytes((@{ pid = $PID; at = [DateTime]::UtcNow.ToString('o') } | ConvertTo-Json -Compress))
    $handle.Write($bytes, 0, $bytes.Length); $handle.Dispose()
    break
  } catch [IO.IOException] {
    $item = Get-Item -LiteralPath $lock -ErrorAction SilentlyContinue
    if ($item -and ([DateTime]::UtcNow - $item.LastWriteTimeUtc).TotalMinutes -gt 30) { Remove-Item -LiteralPath $lock -Force; continue }
    if ([DateTime]::UtcNow -gt $deadline) { throw "Lock busy: $lock" }
    Start-Sleep -Milliseconds 250
  }
}
try {
  $secrets = [ordered]@{}
  (Get-Content -LiteralPath $path -Raw | ConvertFrom-Json).PSObject.Properties | ForEach-Object { $secrets[$_.Name] = $_.Value }
  $secrets[$Name] = $value
  $tmp = "$path.$PID.$([Guid]::NewGuid().ToString('N').Substring(0, 8)).tmp"
  [IO.File]::WriteAllText($tmp, ($secrets | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
  Move-Item -LiteralPath $tmp -Destination $path -Force
} finally {
  Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
}
Write-Output "Stored '$Name' in state/secrets.json ($($value.Length) chars). Keys now: $($secrets.Keys -join ', ')"
