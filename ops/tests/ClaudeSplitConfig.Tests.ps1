#Requires -Version 7.2
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\ClaudeSplitConfig.ps1')
$root = Join-Path ([IO.Path]::GetTempPath()) ('claude-settings-test-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($root) | Out-Null
$settingsPath = Join-Path $root 'settings.json'
$statePath = Join-Path $root 'rollback.json'
$before = @{ model = 'sonnet'; permissions = @{ deny = @('Read(.env)') }; apiKeyHelper = 'fixture-helper'; env = @{ ANTHROPIC_BASE_URL = 'https://fixture.invalid'; ANTHROPIC_AUTH_TOKEN = 'fixture-token'; https_proxy = 'https://fixture.invalid'; API_TIMEOUT_MS = '30000' } }
$before | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $settingsPath
$desired = @{ HTTPS_PROXY = 'http://probe:fixture@127.0.0.1:18326'; HTTP_PROXY = 'http://probe:fixture@127.0.0.1:18326'; NO_PROXY = 'localhost,127.0.0.1,::1'; NODE_EXTRA_CA_CERTS = 'fixture-ca.pem' }
try {
  Set-ClaudeSplitSettings -SettingsPath $settingsPath -StatePath $statePath -Environment $desired -Enable $true
  $s = Get-Content -LiteralPath $settingsPath -Raw | ConvertFrom-Json -AsHashtable
  if ($s.env.Contains('ANTHROPIC_AUTH_TOKEN') -or $s.env.Contains('https_proxy') -or $s.Contains('apiKeyHelper')) { throw 'Provider/proxy conflict survived.' }
  if ($s.env.HTTPS_PROXY -ne $desired.HTTPS_PROXY -or $s.env.API_TIMEOUT_MS -ne '30000' -or $s.permissions.deny[0] -ne 'Read(.env)') { throw 'Unrelated settings/permissions changed.' }
  $saved = Get-Content -LiteralPath $statePath -Raw
  Set-ClaudeSplitSettings -SettingsPath $settingsPath -StatePath $statePath -Environment $desired -Enable $true
  if ((Get-Content -LiteralPath $statePath -Raw) -ne $saved) { throw 'Repeated enable changed rollback history.' }
  $s.model = 'fixture-edited-model'; $s | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $settingsPath
  Set-ClaudeSplitSettings -SettingsPath $settingsPath -StatePath $statePath -Enable $false
  $s = Get-Content -LiteralPath $settingsPath -Raw | ConvertFrom-Json -AsHashtable
  if ($s.env.ANTHROPIC_AUTH_TOKEN -ne 'fixture-token' -or $s.env.https_proxy -ne 'https://fixture.invalid' -or $s.apiKeyHelper -ne 'fixture-helper' -or $s.model -ne 'fixture-edited-model') { throw "Rollback mismatch: token=$($s.env.ANTHROPIC_AUTH_TOKEN -eq 'fixture-token'), lowercaseProxy=$($s.env.https_proxy -eq 'https://fixture.invalid'), helper=$($s.apiKeyHelper -eq 'fixture-helper'), model=$($s.model -eq 'fixture-edited-model')." }
  if (Test-Path -LiteralPath $statePath) { throw 'Rollback record survived successful restore.' }
  Set-ClaudeSplitSettings -SettingsPath $settingsPath -StatePath $statePath -Environment $desired -Enable $true
  $s = Get-Content -LiteralPath $settingsPath -Raw | ConvertFrom-Json -AsHashtable
  $s.env.HTTPS_PROXY = 'https://user-edited.invalid'; $s | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $settingsPath
  $refused = $false
  try { Set-ClaudeSplitSettings -SettingsPath $settingsPath -StatePath $statePath -Enable $false } catch { $refused = $true }
  if (-not $refused) { throw 'Rollback overwrote a user-edited managed key.' }
  'PASS: provider removal, Claude-only proxy/CA, preserved permissions/preferences, idempotence, rollback, concurrent-edit refusal.'
} finally {
  if ([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($root)) -ne [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')) { throw 'Unsafe fixture cleanup path.' }
  Remove-Item -LiteralPath $root -Recurse -Force
}
