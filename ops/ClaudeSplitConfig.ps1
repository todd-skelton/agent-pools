#Requires -Version 7.2
# Claude-only settings. Provisioning never changes the Windows proxy or certificate store.
function Set-ClaudeSplitSettings {
  param([string]$SettingsPath, [string]$StatePath, [hashtable]$Environment, [bool]$Enable)
  $settings = if (Test-Path -LiteralPath $SettingsPath) { Get-Content -LiteralPath $SettingsPath -Raw | ConvertFrom-Json -AsHashtable } else { @{} }
  if (-not $settings.env) { $settings.env = @{} }
  $cleared = @('ANTHROPIC_BASE_URL','ANTHROPIC_AUTH_TOKEN','ANTHROPIC_API_KEY','CLAUDE_CODE_OAUTH_TOKEN',
    'CLAUDE_CODE_USE_BEDROCK','CLAUDE_CODE_USE_VERTEX','CLAUDE_CODE_USE_FOUNDRY','ANTHROPIC_CUSTOM_HEADERS',
    'CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST','AGENT_POOLS_API_KEY','CLAUDE_SPLIT_PROXY_KEY','NODE_TLS_REJECT_UNAUTHORIZED',
    'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC','DISABLE_GROWTHBOOK','ALL_PROXY','http_proxy','https_proxy','all_proxy','no_proxy','CLAUDE_CONFIG_DIR')
  $record = if (Test-Path -LiteralPath $StatePath) { Get-Content -LiteralPath $StatePath -Raw | ConvertFrom-Json -AsHashtable } else { $null }
  if ($record -and $record.settingsPath -ne [IO.Path]::GetFullPath($SettingsPath)) { throw 'Settings rollback record belongs to another profile.' }
  if ($Enable) {
    if (-not $record) {
      $record = @{ settingsPath = [IO.Path]::GetFullPath($SettingsPath); previous = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal); applied = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal); apiKeyHelper = @{ present = $settings.Contains('apiKeyHelper'); value = $settings.apiKeyHelper } }
      $names = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
      foreach ($name in (@($Environment.Keys) + $cleared)) {
        if ($names.Add($name)) { $record.previous[$name] = @{ present = $settings.env.Contains($name); value = $settings.env[$name] } }
      }
    }
    foreach ($name in $cleared) { $settings.env.Remove($name) }
    $settings.Remove('apiKeyHelper')
    foreach ($entry in $Environment.GetEnumerator()) { $settings.env[$entry.Key] = $entry.Value }
    foreach ($name in $record.previous.Keys) { $record.applied[$name] = @{ present = $settings.env.Contains($name); value = $settings.env[$name] } }
    $record | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $StatePath -Encoding utf8
  } else {
    if (-not $record) { return }
    foreach ($name in $record.applied.Keys) {
      $expected = $record.applied[$name]
      if ($settings.env.Contains($name) -ne $expected.present -or ($expected.present -and $settings.env[$name] -cne $expected.value)) { throw "Claude setting $name changed after installation; rollback left it untouched." }
    }
    if ($settings.Contains('apiKeyHelper')) { throw 'apiKeyHelper changed after installation; rollback left it untouched.' }
    foreach ($name in $record.previous.Keys) {
      $old = $record.previous[$name]
      if ($old.present) { $settings.env[$name] = $old.value } else { $settings.env.Remove($name) }
    }
    if ($record.apiKeyHelper.present) { $settings.apiKeyHelper = $record.apiKeyHelper.value }
  }
  $parent = Split-Path -Parent $SettingsPath
  [IO.Directory]::CreateDirectory($parent) | Out-Null
  $settings | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $SettingsPath -Encoding utf8
  if (-not $Enable) { Remove-Item -LiteralPath $StatePath }
}
