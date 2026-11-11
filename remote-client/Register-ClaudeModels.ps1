#Requires -Version 7.2
<#
.SYNOPSIS
Provisions pooled provider/model/effort choices and explicitly named subagents.
.DESCRIPTION
Provisioning only. Does not restart services, copy logins, accept workspace trust or
change tool permissions. Restart the owning native client after configure or disable.
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Exe,
  [ValidateSet('configure','preview','disable')][string]$Action = 'configure',
  [string]$ClientRoot = (Join-Path $HOME '.agent-pools\claude-split'),
  [string]$ClaudeProfile = $(if ($env:CLAUDE_CONFIG_DIR) { $env:CLAUDE_CONFIG_DIR } else { Join-Path $HOME '.claude' }),
  [string]$Catalog
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'The native Claude client currently uses Windows DPAPI.' }
$Exe = (Resolve-Path -LiteralPath $Exe).ProviderPath
$ClientRoot = (Resolve-Path -LiteralPath $ClientRoot).ProviderPath
$ClaudeProfile = (Resolve-Path -LiteralPath $ClaudeProfile).ProviderPath
if (-not (Test-Path -LiteralPath (Join-Path $ClientRoot 'splitter.json'))) { throw 'Install the native Claude client first.' }
if ($Action -eq 'disable' -and $Catalog) { throw '-Catalog is not used when disabling.' }
$arguments = @('claude-models', $Action, ('"' + $ClientRoot + '"'), ('"' + $ClaudeProfile + '"'))
if ($Catalog) { $arguments += ('"' + (Resolve-Path -LiteralPath $Catalog).ProviderPath + '"') }
$id = [Guid]::NewGuid().ToString('N')
$output = Join-Path $ClientRoot "model-command-$id.json"
$errors = Join-Path $ClientRoot "model-command-$id.error"
try {
  $process = Start-Process -FilePath $Exe -ArgumentList $arguments -WindowStyle Hidden -Wait -PassThru -RedirectStandardOutput $output -RedirectStandardError $errors
  if ($process.ExitCode) { throw 'Native model provisioning failed. Existing user edits are never overwritten; inspect configuration ownership or preview the catalog.' }
  Get-Content -LiteralPath $output -Raw | ConvertFrom-Json | ConvertTo-Json -Depth 5
} finally {
  foreach ($path in @($output, $errors)) { if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force } }
}
