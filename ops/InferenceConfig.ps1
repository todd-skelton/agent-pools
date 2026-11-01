# The Codex config edit behind Set-Inference-Relay.ps1 (Agent Pools' inference-config.mjs): one
# root key of ~/.codex/config.toml, openai_base_url, is set or removed and the rest of the file is
# kept byte for byte. There is no TOML parser here, so only the plain layout is accepted: root keys
# are the lines before the first table header, and a key this edits or reads must be one
# conventional line (key = "value" or key = 'value', no escapes, an optional comment). Anything
# else (a quoted or dotted key, a multi-line string before the first table, a repeated key) is
# refused rather than guessed at.

# The file's text and how to write it back: byte-order mark and line ending as found.
function Read-ConfigText([string]$Path) {
  $bytes = [IO.File]::ReadAllBytes($Path)
  $bom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
  $offset = if ($bom) { 3 } else { 0 }
  [pscustomobject]@{ Text = [Text.UTF8Encoding]::new($false, $true).GetString($bytes, $offset, $bytes.Length - $offset); Bom = $bom; Bytes = $bytes }
}

# The root region: the lines before the first table header, each with its own line ending.
function Get-RootLines([string]$Text) {
  $lines = [regex]::Matches($Text, '[^\n]*\n|[^\n]+$') | ForEach-Object { $_.Value }
  $root = @()
  foreach ($l in $lines) {
    if ($l -match '^\s*\[') { break }
    $root += $l
  }
  $joined = -join $root
  if ($joined.Contains('"""') -or $joined.Contains("'''")) { throw 'Multi-line string before the first table; refusing to edit this configuration' }
  , $root
}

# The root key's string value, or $null when it is absent. A non-conventional definition throws.
function Get-RootString([string]$Text, [string]$Key) {
  $root = Get-RootLines $Text
  $mentions = @($root | Where-Object { $_ -match ('^\s*(' + [regex]::Escape($Key) + '|"' + [regex]::Escape($Key) + '"|''' + [regex]::Escape($Key) + ''')\s*[=.]') })
  if ($mentions.Count -eq 0) { return $null }
  if ($mentions.Count -gt 1) { throw "$Key is set more than once; refusing to edit this configuration" }
  $m = [regex]::Match($mentions[0], '^' + [regex]::Escape($Key) + '\s*=\s*(?:"([^"\\\r\n]*)"|''([^''\r\n]*)'')\s*(?:#[^\r\n]*)?\r?\n?$')
  if (-not $m.Success) { throw "Nonstandard $Key configuration; refusing to rewrite it" }
  if ($m.Groups[1].Success) { return $m.Groups[1].Value }
  $m.Groups[2].Value
}

# $Text with the root key removed and, when $Value is not $null, written as the file's first line
# (inference-config.mjs's changeEndpoint). Every other byte is unchanged.
function Set-RootString([string]$Text, [string]$Key, $Value) {
  if ($null -ne $Value -and $Value -notmatch '^[^"\\\r\n]*$') { throw "Refusing to write a $Key value with quotes, backslashes or line breaks" }
  $null = Get-RootString $Text $Key
  $root = Get-RootLines $Text
  $rootText = -join $root
  $rest = $Text.Substring($rootText.Length)
  $kept = -join @($root | Where-Object { $_ -notmatch ('^' + [regex]::Escape($Key) + '\s*=') })
  $newline = if ($Text.Contains("`r`n")) { "`r`n" } else { "`n" }
  $head = if ($null -ne $Value) { "$Key = " + '"' + $Value + '"' + $newline } else { '' }
  $next = $head + $kept + $rest
  if ((Get-RootString $next $Key) -ne $Value) { throw "Could not set $Key" }
  $next
}

# inference-config.mjs's setEndpoint: points openai_base_url at $Endpoint (-Enable) or back at the
# value it replaced, kept in $StatePath ({configPath, endpoint, previous}), which -Enable creates and
# the restore deletes.
function Set-InferenceEndpoint([string]$ConfigPath, [string]$StatePath, [string]$Endpoint, [bool]$Enable) {
  $read = Read-ConfigText $ConfigPath
  $text = $read.Text
  $current = Get-RootString $text 'openai_base_url'
  $store = Get-RootString $text 'cli_auth_credentials_store'
  $state = $null
  if (Test-Path -LiteralPath $StatePath) { $state = Get-Content -LiteralPath $StatePath -Raw | ConvertFrom-Json }
  if ($Enable -and @($null, 'file', 'auto') -notcontains $store) { throw 'Desktop must use file-backed credentials for this relay' }
  if ($state -and ($state.configPath -ne $ConfigPath -or $state.endpoint -ne $Endpoint)) { throw 'Rollback record belongs to different settings' }
  if (-not $Enable -and -not $state) { throw 'No rollback record' }
  if ($state -and $current -ne $Endpoint -and $current -ne $state.previous) { throw 'Endpoint changed externally; refusing to overwrite it' }
  # Codex's default provider is openai when model_provider is absent. Without the relay, that
  # default would send new desktop tasks straight to OpenAI instead of the pool (and Claude models
  # would fail), so the default must move first.
  $provider = Get-RootString $text 'model_provider'
  if (-not $provider) { $provider = 'openai' }
  if (-not $Enable -and $provider -eq 'openai') { throw 'Default model_provider is openai; set model_provider = "account_pool" before disabling the relay, then relaunch Codex' }
  if ($Enable -and -not $state) {
    $record = [ordered]@{ configPath = $ConfigPath; endpoint = $Endpoint }
    if ($null -ne $current) { $record.previous = $current }
    $stream = [IO.File]::Open($StatePath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $bytes = [Text.UTF8Encoding]::new($false).GetBytes(($record | ConvertTo-Json)); $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
    $state = [pscustomobject]$record
  }
  $value = if ($Enable) { $Endpoint } else { $state.previous }
  $next = Set-RootString $text 'openai_base_url' $value
  $out = [Text.UTF8Encoding]::new($false).GetBytes($next)
  if ($read.Bom) { $out = [byte[]](0xEF, 0xBB, 0xBF) + $out }
  $temporary = "$ConfigPath.inference-$([Guid]::NewGuid().ToString('N')).tmp"
  try {
    $stream = [IO.File]::Open($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $stream.Write($out, 0, $out.Length) } finally { $stream.Dispose() }
    $now = [IO.File]::ReadAllBytes($ConfigPath)
    if ([Convert]::ToBase64String($now) -ne [Convert]::ToBase64String($read.Bytes)) { throw 'Configuration changed concurrently; retry after reviewing it' }
    Move-Item -LiteralPath $temporary -Destination $ConfigPath -Force
  } finally { Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue }
  if (-not $Enable) { Remove-Item -LiteralPath $StatePath -Force }
}
