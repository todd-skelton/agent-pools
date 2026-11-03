# Exercise the restart script's real cutover predicate without restarting any service.
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$path = Join-Path $PSScriptRoot '..\deploy\Restart-Pool-Rs.ps1'
$ast = [Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
$wait = @($ast.FindAll({
  param($node)
  $node -is [Management.Automation.Language.CommandAst] -and
    $node.GetCommandName() -eq 'Wait-Until' -and
    $node.Extent.Text.Contains('the service to run selected proxy')
}, $true))
if ($wait.Count -ne 1) { throw 'Expected exactly one proxy cutover check' }
$predicate = $wait[0].CommandElements[1].ScriptBlock.GetScriptBlock()
$expectedProxy = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'test-proxy.exe'))

function Port-For { 8318 }
function Invoke-RestMethod {
  [pscustomobject]@{ processes = @([pscustomobject]@{ name = 'proxy'; pid = $script:testPid }) }
}
function Get-CimInstance {
  param($ClassName, $Filter)
  if ($Filter -ne "ProcessId=$script:testPid") { throw "Unexpected filter: $Filter" }
  if ($script:testMissing) { return $null }
  [pscustomobject]@{ ExecutablePath = $script:testPath }
}

foreach ($case in @(
  @{ Name = 'replacement PID with selected binary'; Pid = 2; Path = $expectedProxy; Missing = $false; Want = $true },
  @{ Name = 'existing PID with selected binary'; Pid = 1; Path = $expectedProxy; Missing = $false; Want = $true },
  @{ Name = 'wrong binary'; Pid = 2; Path = (Join-Path $PSScriptRoot 'old-proxy.exe'); Missing = $false; Want = $false },
  @{ Name = 'missing process'; Pid = 2; Path = $expectedProxy; Missing = $true; Want = $false }
)) {
  $script:testPid = $case.Pid
  $script:testPath = $case.Path
  $script:testMissing = $case.Missing
  $got = [bool](& $predicate)
  if ($got -ne $case.Want) { throw "$($case.Name): got $got, expected $($case.Want)" }
  Write-Output "PASS: $($case.Name)"
}
