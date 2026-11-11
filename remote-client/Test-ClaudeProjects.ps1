#Requires -Version 7.2
param([Parameter(Mandatory)][string]$Exe)
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath((Join-Path ([IO.Path]::GetTempPath()) ('agent-pools-rc-fixture-' + [Guid]::NewGuid().ToString('N'))))
if (-not $root.StartsWith([IO.Path]::GetFullPath([IO.Path]::GetTempPath()),[StringComparison]::OrdinalIgnoreCase)) { throw 'Fixture must be inside the temporary directory.' }
$client=Join-Path $root 'client'; $claude=Join-Path $root 'claude'; $repos=Join-Path $root 'repos'; $repo=Join-Path $repos 'fixture'
try {
  foreach($path in @($client,$claude,(Join-Path $repo '.git'))) { [IO.Directory]::CreateDirectory($path)|Out-Null }
  '{"schema":"claude-splitter/v1","poolOrigin":"http://127.0.0.1:9","port":8326}'|Set-Content -LiteralPath (Join-Path $client 'splitter.json')
  @{projects=@{$repo=@{hasTrustDialogAccepted=$true}}}|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $claude '.claude.json')
  $trustHash=(Get-FileHash -LiteralPath (Join-Path $claude '.claude.json')).Hash
  & (Join-Path $PSScriptRoot 'Register-ClaudeProjects.ps1') -Exe $Exe -ClientRoot $client -ClaudeProfile $claude -ClaudeBinary $Exe -ProjectRoot $repos | Out-Null
  $doc=(& $Exe claude-projects $client list | Out-String)|ConvertFrom-Json -AsHashtable
  if($LASTEXITCODE -or $doc.projects.Count -ne 1){throw 'Trusted project not discovered.'}
  $id=@($doc.projects.Keys)[0]
  & $Exe claude-projects $client remove $id | Out-Null
  if($LASTEXITCODE){throw 'Project removal failed.'}
  & (Join-Path $PSScriptRoot 'Register-ClaudeProjects.ps1') -Exe $Exe -ClientRoot $client -ProjectRoot $repos | Out-Null
  $doc=(& $Exe claude-projects $client list | Out-String)|ConvertFrom-Json -AsHashtable
  if($LASTEXITCODE -or $doc.projects.Count -ne 0){throw 'Discovery re-enabled a removed project.'}
  $container=Join-Path $root 'controller-container'
  [IO.Directory]::CreateDirectory($container)|Out-Null
  & $Exe claude-projects $client add-dir $container 'Controller' | Out-Null
  if($LASTEXITCODE){throw 'Explicit non-Git container registration failed.'}
  $doc=(& $Exe claude-projects $client list | Out-String)|ConvertFrom-Json -AsHashtable
  $shared=@($doc.projects.Values)
  if($LASTEXITCODE -or $shared.Count -ne 1 -or $shared[0].spawn -ne 'same-dir' -or $shared[0].name -ne 'Controller') {throw 'Non-Git container did not retain shared-folder mode.'}
  if((Get-FileHash -LiteralPath (Join-Path $claude '.claude.json')).Hash -ne $trustHash){throw 'Registration changed Claude trust.'}
  'PASS: native Git and explicit non-Git registration, trusted-only discovery, idempotence, disabled tombstone and unchanged trust; no real Claude or service started.'
} finally {
  if((Test-Path -LiteralPath $root) -and $root.StartsWith([IO.Path]::GetFullPath([IO.Path]::GetTempPath()),[StringComparison]::OrdinalIgnoreCase) -and (Split-Path -Leaf $root) -like 'agent-pools-rc-fixture-*') { Remove-Item -LiteralPath $root -Recurse -Force }
}
