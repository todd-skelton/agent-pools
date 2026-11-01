<#
.SYNOPSIS
Starts the native Claude CLI with its Chrome extension (claude --chrome), signed in as itself
rather than through the pool: the pool's ANTHROPIC_* variables and CLAUDE_CONFIG_DIR are unset
for the child. -CheckLogin prints claude auth status instead.
#>
[CmdletBinding()]
param(
  [string]$Worktree=(Get-Location).Path,
  [switch]$CheckLogin
)
$ErrorActionPreference='Stop'
$claudeNative=Join-Path $HOME '.local/bin/claude.exe'
if(-not (Test-Path -LiteralPath $Worktree -PathType Container)){throw 'Workspace does not exist.'}
# Unset the override: explicitly setting even the default directory changes
# where Claude looks for its top-level .claude.json login/extension metadata.
$changes=@{CLAUDE_CONFIG_DIR=$null}
foreach($key in @('ANTHROPIC_BASE_URL','ANTHROPIC_AUTH_TOKEN','ANTHROPIC_API_KEY','CLAUDE_CODE_OAUTH_TOKEN','CLAUDE_CODE_USE_BEDROCK','CLAUDE_CODE_USE_VERTEX','CLAUDE_CODE_USE_FOUNDRY')){$changes[$key]=$null}
$previous=@{}
foreach($key in $changes.Keys){$previous[$key]=[Environment]::GetEnvironmentVariable($key,'Process');if($null -eq $changes[$key]){Remove-Item -LiteralPath "Env:$key" -ErrorAction SilentlyContinue}else{[Environment]::SetEnvironmentVariable($key,$changes[$key],'Process')}}
Push-Location -LiteralPath $Worktree
try{
  if($CheckLogin){& $claudeNative auth status}else{& $claudeNative --chrome}
}finally{
  Pop-Location
  foreach($key in $previous.Keys){if($null -eq $previous[$key]){Remove-Item -LiteralPath "Env:$key" -ErrorAction SilentlyContinue}else{[Environment]::SetEnvironmentVariable($key,$previous[$key],'Process')}}
}
