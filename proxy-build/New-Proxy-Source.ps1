<#
.SYNOPSIS
Recreates the proxy's patched source from upstream and pool.patch, or writes pool.patch from it.

.DESCRIPTION
Upstream is CLIProxyAPI 5b27856, kept as <Root>\proxy-upstream-5b27856.tar.gz.

  -Target <dir>   extracts upstream into <dir> (which must not exist) and applies pool.patch.
                  Build-Pool-Proxy.ps1 builds <Root>\proxy-source.
  -WritePatch     compares <Root>\proxy-source with upstream and writes the difference to
                  -PatchPath (default: pool.patch beside this script), so the repository keeps
                  the source each proxy version was built from.

Line endings are kept as they are in both directions (core.autocrlf=false); the patch is binary
safe (git diff --binary --full-index).
#>
[CmdletBinding(DefaultParameterSetName = 'Target')]
param(
  [Parameter(ParameterSetName = 'Target', Mandatory)][string]$Target,
  [Parameter(ParameterSetName = 'WritePatch', Mandatory)][switch]$WritePatch,
  [Parameter(ParameterSetName = 'WritePatch')][string]$PatchPath = (Join-Path $PSScriptRoot 'pool.patch'),
  [string]$Root
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\deploy\common.ps1')
if (-not $Root) { $Root = $script:LiveRoot }
$Root = (Resolve-Path -LiteralPath $Root).ProviderPath
$tarball = Join-Path $Root 'proxy-upstream-5b27856.tar.gz'
if (-not (Test-Path -LiteralPath $tarball)) { throw "$tarball is missing" }

# Upstream, extracted into a new directory under the temp folder; returns its source directory.
function Expand-Upstream {
  $dir = Join-Path ([IO.Path]::GetTempPath()) ('pool-proxy-upstream-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
  New-Item -ItemType Directory -Path $dir | Out-Null
  & tar -xzf $tarball -C $dir
  if ($LASTEXITCODE -ne 0) { throw "tar could not extract $tarball" }
  $top = @(Get-ChildItem -LiteralPath $dir -Directory)
  if ($top.Count -ne 1) { throw "$tarball should hold one top-level directory" }
  [pscustomobject]@{ Temp = $dir; Source = $top[0].FullName }
}

if ($PSCmdlet.ParameterSetName -eq 'Target') {
  $patch = Join-Path $PSScriptRoot 'pool.patch'
  if (Test-Path -LiteralPath $Target) { throw "$Target exists; pick a new directory" }
  $up = Expand-Upstream
  try {
    & git -c core.autocrlf=false -C $up.Source apply --binary --whitespace=nowarn $patch
    if ($LASTEXITCODE -ne 0) { throw 'pool.patch did not apply to upstream' }
    Move-Item -LiteralPath $up.Source -Destination $Target
  } finally { Remove-Item -LiteralPath $up.Temp -Recurse -Force -ErrorAction SilentlyContinue }
  Write-Output "Wrote upstream 5b27856 with pool.patch to $Target"
  return
}

$source = Join-Path $Root 'proxy-source'
if (-not (Test-Path -LiteralPath $source)) { throw "$source is missing" }
$up = Expand-Upstream
try {
  $g = @('-c', 'core.autocrlf=false', '-c', 'core.safecrlf=false', '-C', $up.Source)
  & git @g init -q
  & git @g add -A
  & git @g -c user.name=upstream -c user.email=upstream@example.invalid commit -q -m upstream
  if ($LASTEXITCODE -ne 0) { throw 'could not record upstream' }
  & robocopy $source $up.Source /MIR /XD .git /XF .git /NFL /NDL /NJH /NJS /NP | Out-Null
  if ($LASTEXITCODE -ge 8) { throw "robocopy failed ($LASTEXITCODE)" }
  & git @g add -A
  # cmd's redirection keeps the bytes as git writes them.
  $full = [IO.Path]::GetFullPath($PatchPath)
  & cmd /c "git -c core.autocrlf=false -C `"$($up.Source)`" diff --cached --no-color --binary --full-index > `"$full`""
  if ($LASTEXITCODE -ne 0) { throw 'git diff failed' }
  $files = @(& git @g diff --cached --name-only).Count
  Write-Output "Wrote $full ($files files, $((Get-Item -LiteralPath $full).Length) bytes)"
} finally { Remove-Item -LiteralPath $up.Temp -Recurse -Force -ErrorAction SilentlyContinue }
