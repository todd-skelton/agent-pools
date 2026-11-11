#Requires -Version 7.2
<#
.SYNOPSIS
Enables native discovery of already-trusted Git projects inside explicit local roots.
.DESCRIPTION
Provisioning only. The existing native Claude client task supervises servers. This does not
sign in, accept Remote Control consent, change workspace trust or weaken tool permissions.
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Exe,
  [Parameter(Mandatory)][string[]]$ProjectRoot,
  [string]$ClientRoot = (Join-Path $HOME '.agent-pools\claude-split'),
  [string]$ClaudeBinary,
  [string]$ClaudeProfile = $(if ($env:CLAUDE_CONFIG_DIR) { $env:CLAUDE_CONFIG_DIR } else { Join-Path $HOME '.claude' })
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Managed Claude project process ownership currently requires Windows.' }
$Exe = (Resolve-Path -LiteralPath $Exe).ProviderPath
$ClientRoot = [IO.Path]::GetFullPath($ClientRoot)
if (-not (Test-Path -LiteralPath (Join-Path $ClientRoot 'splitter.json'))) { throw 'Install the native Claude client before registering projects.' }
if (-not (Test-Path -LiteralPath (Join-Path $ClientRoot 'claude-projects.json'))) {
  if (-not $ClaudeBinary) { $ClaudeBinary = (Get-Command claude.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source }
  & $Exe claude-projects $ClientRoot init $ClaudeBinary $ClaudeProfile
  if ($LASTEXITCODE) { throw 'Claude project registry initialization failed.' }
}
foreach ($root in $ProjectRoot) {
  & $Exe claude-projects $ClientRoot discover-root ([IO.Path]::GetFullPath($root))
  if ($LASTEXITCODE) { throw 'Claude project discovery configuration failed.' }
}
'Registered discovery roots. Already-trusted main Git projects start automatically in the native client.'
'Check claude-projects status for first-party consent, trust or login requirements. None are accepted automatically.'
