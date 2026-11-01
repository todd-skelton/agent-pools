<#
.SYNOPSIS
Runs one chase-sets lane (.orchestrator\dispatch-lane.ps1, with these parameters unchanged)
through the pool: the lane's harness gets the pool's client key, the install's profiles\codex or
profiles\claude, and the native executable from install.json. The install is AGENT_POOLS_ROOT,
or the live one.
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory)][ValidateSet('codex','claude')][string]$Harness,
  [Parameter(Mandatory)][string]$Model,
  [Parameter(Mandatory)][string]$Effort,
  [Parameter(Mandatory)][ValidateSet('implementation','review','planning')][string]$LaneRole,
  [Parameter(Mandatory)][string]$PromptFile,
  [Parameter(Mandatory)][string]$Worktree,
  [Parameter(Mandatory)][string]$Label,
  [switch]$DryRun,
  [switch]$Force
)
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot '..\deploy\common.ps1')
$poolRoot=if($env:AGENT_POOLS_ROOT){$env:AGENT_POOLS_ROOT}else{$script:LiveRoot}
$container='D:/Users/ToddS/Source/Repos/chase-sets'
$canonical=[IO.Path]::GetFullPath($(if([IO.Path]::IsPathRooted($Worktree)){$Worktree}else{Join-Path $container $Worktree}))
if($canonical.TrimEnd('/','\') -ieq [IO.Path]::GetFullPath((Join-Path $container 'main')).TrimEnd('/','\')){throw 'The canonical main checkout is read-only.'}
$keys=Get-Content (Join-Path $poolRoot 'state/secrets.json') -Raw | ConvertFrom-Json
$install=Get-Content (Join-Path $poolRoot 'state/install.json') -Raw | ConvertFrom-Json
$nativeBinary=if($Harness -eq 'codex'){$install.codexBinary}else{$install.claudeBinary}
if(-not $nativeBinary.EndsWith('.exe',[StringComparison]::OrdinalIgnoreCase) -or -not (Test-Path -LiteralPath $nativeBinary -PathType Leaf)){throw 'Configured native harness executable is unavailable.'}
$envChanges=@{AGENT_POOL_API_KEY=$keys.clientKey}
# Resolve the tested native executable even after logon, when Codex desktop's
# private PATH additions are absent. The canonical launcher still resolves it.
$envChanges.PATH=(Split-Path -Parent $nativeBinary)+[IO.Path]::PathSeparator+$env:PATH
foreach($name in @('CODEX_APP_TOOLS_PIPE_PATH','CODEX_SESSION_ID','CODEX_THREAD_ID','CODEX_INTERNAL_ORIGINATOR_OVERRIDE','CODEX_CI','CODEX_PERMISSION_PROFILE')){$envChanges[$name]=$null}
if($Harness -eq 'codex'){
  $envChanges.CODEX_HOME=Join-Path $poolRoot 'profiles/codex'
  $envChanges.OPENAI_API_KEY=$null
  $envChanges.CODEX_ACCESS_TOKEN=$null
}else{
  $envChanges.CLAUDE_CONFIG_DIR=Join-Path $poolRoot 'profiles/claude'
  $envChanges.ANTHROPIC_BASE_URL='http://127.0.0.1:8317'
  $envChanges.ANTHROPIC_AUTH_TOKEN=$keys.clientKey
  $envChanges.ANTHROPIC_API_KEY=$null
  $envChanges.CLAUDE_CODE_OAUTH_TOKEN=$null
}
$original=@{}
foreach($name in $envChanges.Keys){$original[$name]=[Environment]::GetEnvironmentVariable($name,'Process');[Environment]::SetEnvironmentVariable($name,$envChanges[$name],'Process')}
try{
  # The existing reviewed launcher retains ownership, worktree and review isolation checks.
  & (Join-Path $container '.orchestrator/dispatch-lane.ps1') @PSBoundParameters
}finally{
  foreach($name in $original.Keys){[Environment]::SetEnvironmentVariable($name,$original[$name],'Process')}
}
