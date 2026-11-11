#Requires -Version 7.2
param([Parameter(Mandatory)][string]$Exe)
$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ('agent-pools-models-' + [Guid]::NewGuid().ToString('N'))
$client = Join-Path $root 'client'
$config = Join-Path $root 'claude profile'
$server = $null
try {
  [IO.Directory]::CreateDirectory($client) | Out-Null
  [IO.Directory]::CreateDirectory($config) | Out-Null
  '{}' | Set-Content -LiteralPath (Join-Path $client 'splitter.json')
  $settings = '{"env":{"HTTPS_PROXY":"fixture-only"},"permissions":{"deny":["Read(.env)"]},"model":"unchanged","effortLevel":"medium"}'
  $settings | Set-Content -LiteralPath (Join-Path $config 'settings.json')
  $catalog = Join-Path $root 'catalog.json'
  '{"schema":"agent-pools-model-catalog/v1","generatedAt":null,"models":[{"id":"gpt-fixture","provider":"openai","efforts":["low","high"],"admittedEfforts":["high"]}]}' | Set-Content -LiteralPath $catalog
  $helper = Join-Path $PSScriptRoot 'Register-ClaudeModels.ps1'
  $common = @{ Exe=$Exe;ClientRoot=$client;ClaudeProfile=$config }
  $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
  $listener.Start(); $port = $listener.LocalEndpoint.Port; $listener.Stop()
  @{schema='claude-splitter/v1';poolOrigin="http://127.0.0.1:$port";port=8326} | ConvertTo-Json | Set-Content (Join-Path $client 'splitter.json')
  $sealed = [Security.Cryptography.ProtectedData]::Protect([Text.Encoding]::UTF8.GetBytes('fixture-catalog-key'), $null, [Security.Cryptography.DataProtectionScope]::CurrentUser)
  [IO.File]::WriteAllBytes((Join-Path $client 'pool-key.dpapi'), $sealed)
  $server = Start-Job -ArgumentList $port,([IO.File]::ReadAllText($catalog)) -ScriptBlock {
    param($port,$catalog)
    $http = [Net.HttpListener]::new()
    $http.Prefixes.Add("http://127.0.0.1:$port/")
    try {
      $http.Start()
      foreach ($status in @(200,302)) {
        $pending = $http.GetContextAsync()
        if (-not $pending.Wait(30000)) { throw 'Catalog fixture timed out.' }
        $request = $pending.Result
        if ($request.Request.Url.AbsolutePath -ne '/v1/agent-pools/models' -or $request.Request.Headers['Authorization'] -ne 'Bearer fixture-catalog-key') { throw 'Native catalog fetch did not use the machine credential and exact endpoint.' }
        $request.Response.StatusCode = $status
        if ($status -eq 302) { $request.Response.RedirectLocation = "http://127.0.0.1:$port/not-followed" }
        $bytes = [Text.Encoding]::UTF8.GetBytes($catalog)
        $request.Response.ContentType = 'application/json'
        $request.Response.OutputStream.Write($bytes)
        $request.Response.Close()
      }
      'PASS: authenticated catalog endpoint and redirect refusal fixture.'
    } finally { $http.Close() }
  }
  Start-Sleep -Milliseconds 700
  $remote = (& $helper @common -Action preview | Out-String) | ConvertFrom-Json
  if ($remote.models -ne 1 -or $remote.changesApplied) { throw 'Authenticated catalog preview failed.' }
  $before = (Get-FileHash (Join-Path $config 'settings.json')).Hash
  $rejected = $false
  try { $null = & $helper @common -Action preview } catch { $rejected = $true }
  if (-not $rejected -or (Get-FileHash (Join-Path $config 'settings.json')).Hash -ne $before) { throw 'Catalog redirect was followed or changed settings.' }
  $null = Wait-Job $server -Timeout 5
  if ($server.State -ne 'Completed') { throw 'Catalog fixture failed.' }
  Receive-Job $server -ErrorAction Stop
  $preview = (& $helper @common -Catalog $catalog -Action preview | Out-String) | ConvertFrom-Json
  if ($preview.changesApplied -or $preview.models -ne 1 -or (Test-Path (Join-Path $client 'model-catalog.json'))) { throw 'Preview changed settings or returned an incorrect catalog.' }
  foreach ($round in 1..2) {
    $result = (& $helper @common -Catalog $catalog | Out-String) | ConvertFrom-Json
    if ($result.models -ne 1 -or $result.agents -ne 1 -or $result.permissionsChanged -or $result.loginChanged -or $result.trustChanged) { throw 'Unexpected provisioning result.' }
  }
  $actual = Get-Content (Join-Path $config 'settings.json') -Raw | ConvertFrom-Json
  if ($actual.modelPicker.options[0].model -ne 'pool--gpt-fixture--high' -or $actual.permissions.deny[0] -ne 'Read(.env)' -or $actual.env.HTTPS_PROXY -ne 'fixture-only' -or $actual.model -ne 'unchanged' -or $actual.effortLevel -ne 'medium') { throw 'Provisioning changed unrelated settings or installed the wrong selection.' }
  $null = & $helper @common -Action disable
  $actual = Get-Content (Join-Path $config 'settings.json') -Raw | ConvertFrom-Json -AsHashtable
  if ($actual.ContainsKey('modelPicker') -or (Test-Path (Join-Path $config 'agents\pool-gpt-fixture-high.md'))) { throw 'Disable did not remove owned settings/agents.' }
  'PASS: preview, native console/windowless provisioning, paths with spaces, idempotent configure, explicit model/effort choice, unchanged permissions/defaults/proxy and disable.'
} finally {
  if ($server) { Stop-Job $server; Remove-Job $server -Force }
  $full = [IO.Path]::GetFullPath($root)
  $parent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
  if (-not $full.StartsWith($parent, [StringComparison]::OrdinalIgnoreCase) -or [IO.Path]::GetFileName($full) -notmatch '^agent-pools-models-[a-f0-9]{32}$') { throw 'Unsafe fixture cleanup path.' }
  if (Test-Path -LiteralPath $full) { Remove-Item -LiteralPath $full -Recurse -Force }
}
