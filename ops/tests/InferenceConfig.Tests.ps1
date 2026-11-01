# Checks ops\InferenceConfig.ps1 on scratch files: pwsh -File ops\tests\InferenceConfig.Tests.ps1
# (exits 1 on the first failure). Never touches ~/.codex.
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\InferenceConfig.ps1')
$dir = Join-Path ([IO.Path]::GetTempPath()) ('inference-config-tests-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $dir | Out-Null
$failed = 0
function Check([string]$name, [scriptblock]$body) {
  try { & $body; Write-Output "ok   $name" } catch { $script:failed++; Write-Output "FAIL $name : $($_.Exception.Message)" }
}
function Expect-Throw([scriptblock]$body, [string]$like) {
  try { & $body } catch { if ($_.Exception.Message -like "*$like*") { return }; throw "threw '$($_.Exception.Message)', expected '*$like*'" }
  throw "did not throw (expected '*$like*')"
}
function Write-Bytes([string]$path, [string]$text, [bool]$bom = $false) {
  $b = [Text.UTF8Encoding]::new($false).GetBytes($text)
  if ($bom) { $b = [byte[]](0xEF, 0xBB, 0xBF) + $b }
  [IO.File]::WriteAllBytes($path, $b)
}
$ep = 'https://localhost:8319/v1'
try {
  $base = "model = `"gpt`"`n# a comment`nmodel_provider = `"account_pool`"`n`n[profiles.x]`nopenai_base_url = `"in-a-table`"`n"
  Check 'enable inserts the key first and keeps every other byte' {
    $c = Join-Path $dir 'a.toml'; $s = Join-Path $dir 'a.json'; Write-Bytes $c $base
    Set-InferenceEndpoint $c $s $ep $true
    $t = [IO.File]::ReadAllText($c)
    if ($t -ne ("openai_base_url = `"$ep`"`n" + $base)) { throw "got: $t" }
    $r = Get-Content $s -Raw | ConvertFrom-Json
    if ($r.configPath -ne $c -or $r.endpoint -ne $ep -or ($r.PSObject.Properties.Name -contains 'previous')) { throw 'state record' }
  }
  Check 'disable restores the file exactly and deletes the record' {
    $c = Join-Path $dir 'a.toml'; $s = Join-Path $dir 'a.json'
    Set-InferenceEndpoint $c $s $ep $false
    if ([IO.File]::ReadAllText($c) -ne $base) { throw 'not restored' }
    if (Test-Path $s) { throw 'record left' }
  }
  Check 'a previous value is kept and put back, CRLF and BOM preserved' {
    $c = Join-Path $dir 'b.toml'; $s = Join-Path $dir 'b.json'
    $orig = "model_provider = 'account_pool'`r`nopenai_base_url = `"https://old.example/v1`" # was`r`nmodel = `"x`"`r`n"
    Write-Bytes $c $orig $true
    Set-InferenceEndpoint $c $s $ep $true
    $b = [IO.File]::ReadAllBytes($c)
    if (-not ($b[0] -eq 0xEF -and $b[1] -eq 0xBB -and $b[2] -eq 0xBF)) { throw 'bom lost' }
    $t = [Text.Encoding]::UTF8.GetString($b, 3, $b.Length - 3)
    if ($t -ne "openai_base_url = `"$ep`"`r`nmodel_provider = 'account_pool'`r`nmodel = `"x`"`r`n") { throw "got: $t" }
    if ((Get-Content $s -Raw | ConvertFrom-Json).previous -ne 'https://old.example/v1') { throw 'previous not kept' }
    Set-InferenceEndpoint $c $s $ep $false
    $after = [IO.File]::ReadAllBytes($c)
    $t2 = [Text.Encoding]::UTF8.GetString($after, 3, $after.Length - 3)
    if ($t2 -ne "openai_base_url = `"https://old.example/v1`"`r`nmodel_provider = 'account_pool'`r`nmodel = `"x`"`r`n") { throw "got: $t2" }
  }
  Check 'enable again with the record in place is a no-op rewrite' {
    $c = Join-Path $dir 'c.toml'; $s = Join-Path $dir 'c.json'; Write-Bytes $c $base
    Set-InferenceEndpoint $c $s $ep $true; $first = [IO.File]::ReadAllText($c)
    Set-InferenceEndpoint $c $s $ep $true
    if ([IO.File]::ReadAllText($c) -ne $first) { throw 'changed' }
  }
  Check 'disable refuses while model_provider defaults to openai' {
    $c = Join-Path $dir 'd.toml'; $s = Join-Path $dir 'd.json'; Write-Bytes $c "model = `"gpt`"`n"
    Set-InferenceEndpoint $c $s $ep $true
    Expect-Throw { Set-InferenceEndpoint $c $s $ep $false } 'model_provider is openai'
  }
  Check 'disable without a record refuses' {
    $c = Join-Path $dir 'e.toml'; Write-Bytes $c $base
    Expect-Throw { Set-InferenceEndpoint $c (Join-Path $dir 'none.json') $ep $false } 'No rollback record'
  }
  Check 'an endpoint changed by hand is not overwritten' {
    $c = Join-Path $dir 'f.toml'; $s = Join-Path $dir 'f.json'; Write-Bytes $c $base
    Set-InferenceEndpoint $c $s $ep $true
    Write-Bytes $c ("openai_base_url = `"https://elsewhere/v1`"`n" + $base)
    Expect-Throw { Set-InferenceEndpoint $c $s $ep $false } 'changed externally'
  }
  Check 'non-file credential stores are refused' {
    $c = Join-Path $dir 'g.toml'; Write-Bytes $c "cli_auth_credentials_store = `"keyring`"`n"
    Expect-Throw { Set-InferenceEndpoint $c (Join-Path $dir 'g.json') $ep $true } 'file-backed'
  }
  Check 'nonstandard layouts are refused' {
    Expect-Throw { Get-RootString "openai_base_url = `"a\u0041`"`n" 'openai_base_url' } 'Nonstandard'
    Expect-Throw { Get-RootString "`"openai_base_url`" = `"a`"`n" 'openai_base_url' } 'Nonstandard'
    Expect-Throw { Get-RootString "openai_base_url = `"a`"`nopenai_base_url = `"b`"`n" 'openai_base_url' } 'more than once'
    Expect-Throw { Get-RootString "x = `"`"`"`nopenai_base_url = `"a`"`n`"`"`"`n" 'openai_base_url' } 'Multi-line'
    if ($null -ne (Get-RootString "[t]`nopenai_base_url = `"a`"`n" 'openai_base_url')) { throw 'a table key read as root' }
  }
} finally { Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue }
if ($failed) { Write-Output "$failed failed"; exit 1 }
Write-Output 'all passed'
