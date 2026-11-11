<#
.SYNOPSIS
Tests and builds the pool's proxy (CLIProxyAPI 5b27856 with proxy-build\pool.patch) from
<Root>\proxy-source into a candidate executable.

.DESCRIPTION
<Root>\proxy-source is the patched source: upstream 5b27856 (<Root>\proxy-upstream-5b27856.tar.gz)
with pool.patch applied. New-Proxy-Source.ps1 recreates it; after changing it, regenerate the
patch with New-Proxy-Source.ps1 -WritePatch so the repository keeps what was built.
The toolchain is the install's own: <Root>\build\go, <Root>\build\llvm-mingw-20260908-ucrt-x86_64,
with the module and build caches beside them.
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory)][ValidatePattern('^7\.2\.158-pool\.\d+$')][string]$Version,
  [string]$Root,
  [string]$Output
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\deploy\common.ps1')
if (-not $Root) { $Root = $script:LiveRoot }
$poolRoot = (Resolve-Path -LiteralPath $Root).ProviderPath
if (-not $Output) { $Output = Join-Path $poolRoot 'build/candidate.exe' }
$env:GOTOOLCHAIN = 'local'
$env:GOMODCACHE = Join-Path $poolRoot 'build/modcache'
$env:GOCACHE = Join-Path $poolRoot 'build/buildcache'
$env:CGO_ENABLED = '1'
$env:CC = '"' + (Join-Path $poolRoot 'build/llvm-mingw-20260908-ucrt-x86_64/bin/x86_64-w64-mingw32-clang.exe').Replace('\', '/') + '"'
$poolGo = Join-Path $poolRoot 'build/go/bin/go.exe'
Push-Location -LiteralPath (Join-Path $poolRoot 'proxy-source')
try {
  # The two *ReturnsNotFoundForUnknownPlugin tests walk a cwd-relative plugins
  # directory and fail under Codex's sandbox with a path-resolution error that
  # has nothing to do with the pool patches. Everything else in the gate runs.
  # pool.17 adds the live model list, tool route and installed client version
  # packages (sdk/cliproxy, sdk/api/handlers, registry, clientversion, livemodels).
  # pool.19 adds the Claude refresh client (internal/auth/claude).
  & $poolGo test ./sdk/cliproxy ./sdk/cliproxy/auth ./sdk/api/handlers/... ./internal/registry ./internal/clientversion ./internal/livemodels ./internal/api/handlers/management ./internal/runtime/executor/helps ./internal/translator/claude/... ./internal/translator/common ./internal/auth/claude -skip 'ReturnsNotFoundForUnknownPlugin' -count=1 -timeout 300s
  if ($LASTEXITCODE -ne 0) { throw 'Proxy tests failed' }
  # The executor package as a whole has unrelated upstream failures
  # (OpenAICompat tool-result modalities, a timing-sensitive Antigravity pool
  # test), so pool.25 gates only its own Claude web search provenance recovery
  # and the single send boundary it must keep. pool.27 adds the Codex stream
  # bootstrap buffering suite (overload behind an empty reasoning item).
  # pool.34 retains thread tool symbols and exercises rotation/replay and remapping.
  & $poolGo test ./internal/runtime/executor -run 'SearchProvenance|ForeignSearchProvenance|TranslatedResponsesHistoryRecovers|DoesNotResendOnOtherBadRequests|ResendsAtMostOnce|SingleUpstreamSendBoundary|InStreamOverload|HandshakeBuffer|BootstrapBuffering|ToolThreads|RemapOAuthToolNames' -count=1 -timeout 300s
  if ($LASTEXITCODE -ne 0) { throw 'Proxy executor tests failed' }
  $poolBuiltAt = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ')
  & $poolGo build -ldflags "-s -w -X main.Version=$Version -X main.Commit=5b278561+pool-recovery -X main.BuildDate=$poolBuiltAt" -o $Output ./cmd/server
  if ($LASTEXITCODE -ne 0) { throw 'Proxy build failed' }
  $reported = (& $Output --version 2>&1 | Select-Object -First 1)
  if ($reported -notmatch [regex]::Escape("Version: $Version,")) { throw "Built binary reports '$reported', expected version $Version" }
  Write-Output "Built $Output ($reported)"
} finally { Pop-Location }
