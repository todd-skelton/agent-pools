<#
.SYNOPSIS
Restricts the pool's credentials, state and logs to this Windows account.

.DESCRIPTION
auth/, profiles/, state/, logs/ and proxy.yaml inherit entries from the parent
folder that let every authenticated user read them (and Users modify them).
This stops inheritance on those paths, grants full control to the current user,
SYSTEM and Administrators only, and resets everything beneath the directories
so it inherits the new entries. Safe to rerun; -Report only prints the entries.
-Root is the install (default: the live one).
#>
[CmdletBinding()]
param([switch]$Report, [string]$Root)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\deploy\common.ps1')
if (-not $Root) { $Root = $script:LiveRoot }
$poolRoot = (Resolve-Path -LiteralPath $Root).ProviderPath.TrimEnd('\')
$targets = @('auth', 'profiles', 'state', 'logs' | ForEach-Object { Join-Path $poolRoot $_ } | Where-Object { Test-Path -LiteralPath $_ })
$targets += @(Get-ChildItem -LiteralPath $poolRoot -Filter 'proxy.yaml*' -File | ForEach-Object { $_.FullName })
# Well-known SIDs for SYSTEM and Administrators keep this locale-independent.
$principals = @([Security.Principal.WindowsIdentity]::GetCurrent().User.Value, 'S-1-5-18', 'S-1-5-32-544')
foreach ($target in $targets) {
  $isDirectory = Test-Path -LiteralPath $target -PathType Container
  if (-not $Report) {
    $flags = if ($isDirectory) { '(OI)(CI)F' } else { 'F' }
    $grants = $principals | ForEach-Object { "*$($_):$flags" }
    & icacls $target /inheritance:r /grant:r @grants /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls failed for $target" }
    if ($isDirectory) {
      & icacls (Join-Path $target '*') /reset /T /Q | Out-Null
      if ($LASTEXITCODE -ne 0) { throw "icacls reset failed under $target" }
    }
  }
  $entries = (Get-Acl -LiteralPath $target).Access | ForEach-Object { "$($_.IdentityReference)=$($_.FileSystemRights)" }
  Write-Output ("{0}: {1}" -f $target.Substring($poolRoot.Length + 1), ($entries -join '; '))
}
