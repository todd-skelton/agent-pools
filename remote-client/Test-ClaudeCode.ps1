#Requires -Version 7.2
param([Parameter(Mandatory)][string]$Exe)
$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ('agent-pools-claude-test-' + [Guid]::NewGuid().ToString('N'))
$client = Join-Path $root 'client'; $claude = Join-Path $root 'claude'
[IO.Directory]::CreateDirectory($client) | Out-Null; [IO.Directory]::CreateDirectory($claude) | Out-Null
function Free-Port {
  $l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0); $l.Start(); $port = $l.LocalEndpoint.Port; $l.Stop(); return $port
}
$poolPort = Free-Port; $port = Free-Port
$job = Start-Job -ArgumentList $poolPort -ScriptBlock {
  param($port)
  $l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $port); $l.Start(); 'READY'
  try {
    while ($true) {
      $c = $l.AcceptTcpClient()
      try {
        $stream = $c.GetStream(); $reader = [IO.StreamReader]::new($stream, [Text.Encoding]::ASCII, $false, 1024, $true)
        $head = ''; do { $line = $reader.ReadLine(); $head += $line + "`r`n" } while ($line)
        if ($head -notmatch 'Authorization: Bearer fixture-pool-key') { throw 'Fixture expected pool credential, not control auth.' }
        $body = '{"data":[{"id":"fixture-model"}]}'
        $out = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 200 OK`r`nContent-Type: application/json`r`nContent-Length: $($body.Length)`r`nConnection: close`r`n`r`n$body")
        $stream.Write($out); $stream.Flush()
      } finally { $c.Dispose() }
    }
  } finally { $l.Stop() }
}
$process = $null
$taskName = 'Agent Pools test Claude ' + [Guid]::NewGuid().ToString('N')
try {
  for ($i=0; $i -lt 60; $i++) { if (@(Receive-Job $job -Keep) -contains 'READY') { break }; Start-Sleep -Milliseconds 100 }
  if (@(Receive-Job $job -Keep) -notcontains 'READY') { throw 'Fixture pool did not start.' }
  @{ schema='claude-splitter/v1'; poolOrigin="http://127.0.0.1:$poolPort"; port=$port; additionalTunnelHosts=@() } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $client 'splitter.json')
  [Management.Automation.PSCredential]::new('agent-pools', (ConvertTo-SecureString 'fixture-pool-key' -AsPlainText -Force)) | Export-Clixml -LiteralPath (Join-Path $client 'pool-key.clixml')
  # A legacy RSA CA, as the prototype wrote it. Native init must sign a new leaf under it.
  $certDir = Join-Path $client 'probe-certs'; [IO.Directory]::CreateDirectory($certDir) | Out-Null
  $rsa = [Security.Cryptography.RSA]::Create(2048)
  try {
    $request = [Security.Cryptography.X509Certificates.CertificateRequest]::new('CN=Legacy fixture CA', $rsa, [Security.Cryptography.HashAlgorithmName]::SHA256, [Security.Cryptography.RSASignaturePadding]::Pkcs1)
    $request.CertificateExtensions.Add([Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($true, $false, 0, $true))
    $request.CertificateExtensions.Add([Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new([Security.Cryptography.X509Certificates.X509KeyUsageFlags]::KeyCertSign, $true))
    $ca = $request.CreateSelfSigned([DateTimeOffset]::UtcNow.AddMinutes(-5), [DateTimeOffset]::UtcNow.AddYears(5))
    try { [IO.File]::WriteAllText((Join-Path $certDir 'ca.pem'), $ca.ExportCertificatePem()); [IO.File]::WriteAllText((Join-Path $certDir 'ca-key.pem'), $rsa.ExportPkcs8PrivateKeyPem()) } finally { $ca.Dispose() }
  } finally { $rsa.Dispose() }
  $caHash = (Get-FileHash -LiteralPath (Join-Path $certDir 'ca.pem')).Hash
  $settings = Join-Path $claude 'settings.json'; '{"env":{"ANTHROPIC_BASE_URL":"https://fixture.invalid"},"permissions":{"deny":["Read(.env)"]}}' | Set-Content -LiteralPath $settings
  $settingsHash = (Get-FileHash -LiteralPath $settings).Hash
  $installed = (& (Join-Path $PSScriptRoot 'Connect-ClaudeCode.ps1') -Exe $Exe -ClientRoot $client -ClaudeProfile $claude -StageOnly | Out-String) | ConvertFrom-Json
  if (-not $installed.staged -or (Get-FileHash -LiteralPath $settings).Hash -ne $settingsHash) { throw 'Stage-only changed Claude settings.' }
  if ((Get-FileHash -LiteralPath (Join-Path $certDir 'ca.pem')).Hash -ne $caHash) { throw 'Migration replaced legacy CA.' }
  $sealed = [IO.File]::ReadAllBytes((Join-Path $client 'pool-key.dpapi'))
  $plain = [Security.Cryptography.ProtectedData]::Unprotect($sealed, $null, [Security.Cryptography.DataProtectionScope]::CurrentUser)
  if ([Text.Encoding]::UTF8.GetString($plain) -ne 'fixture-pool-key') { throw 'Native DPAPI migration failed.' }
  if ([Text.Encoding]::UTF8.GetString($sealed).Contains('fixture-pool-key')) { throw 'Native credential stored in plaintext.' }
  $key = [IO.File]::ReadAllText((Join-Path $client 'proxy-key.txt'))
  $headers = @{ 'Proxy-Authorization' = 'Basic ' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes('probe:' + $key)) }
  $first = $null
  for ($round=0; $round -lt 2; $round++) {
    $process = Start-Process -FilePath $installed.nativeExe -ArgumentList ('serve-claude-split "' + $client + '"') -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $client "out-$round.log") -RedirectStandardError (Join-Path $client "error-$round.log")
    $health = $null
    for ($i=0; $i -lt 60; $i++) { try { $health = Invoke-RestMethod "http://127.0.0.1:$port/health" -Headers $headers -NoProxy -TimeoutSec 1 } catch { }; if ($health) { break }; Start-Sleep -Milliseconds 100 }
    if ($health.service -ne 'agent-pools-claude-split' -or -not $health.certificateValid) { throw 'Native proxy not healthy.' }
    $check = Start-Process -FilePath $installed.nativeExe -ArgumentList ('check-claude-split "' + $client + '"') -WindowStyle Hidden -Wait -PassThru
    if ($check.ExitCode -ne 0) { throw 'Native TLS/model discovery failed under legacy RSA CA.' }
    if ($round -eq 0) {
      $first = $health.instanceId
      $duplicate = Start-Process -FilePath $installed.nativeExe -ArgumentList ('serve-claude-split "' + $client + '"') -WindowStyle Hidden -Wait -PassThru
      if ($duplicate.ExitCode -eq 0) { throw 'Duplicate native server did not refuse occupied port.' }
      $state = Get-Content -LiteralPath (Join-Path $client 'probe-proxy.json') -Raw | ConvertFrom-Json
      if ($state.instanceId -ne $first) { throw 'Duplicate replaced active state.' }
    } elseif ($health.instanceId -eq $first) { throw 'Restart did not create a fresh instance.' }
    $headers['X-Splitter-Instance'] = $health.instanceId
    Invoke-RestMethod -Method Post "http://127.0.0.1:$port/shutdown" -Headers $headers -NoProxy -TimeoutSec 3 | Out-Null
    if (-not $process.WaitForExit(5000)) { throw 'Native authenticated stop timed out.' }
    $process = $null
    if ([IO.File]::ReadAllText((Join-Path $client 'proxy-key.txt')) -ne $key -or (Get-FileHash -LiteralPath (Join-Path $certDir 'ca.pem')).Hash -ne $caHash) { throw 'Restart changed proxy credential/trust.' }
  }
  if (Test-Path -LiteralPath (Join-Path $client 'probe-proxy.json')) { throw 'Stopped native proxy left active state.' }
  # Use only a unique fixture task/profile; never the installed pool or prototype task.
  & (Join-Path $PSScriptRoot 'Connect-ClaudeCode.ps1') -Exe $Exe -ClientRoot $client -ClaudeProfile $claude -TaskName $taskName | Out-Null
  $task = Get-ScheduledTask -TaskName $taskName
  if ($task.State -ne 'Running' -or $task.Actions.Execute -notlike "$client\bin\*\pools-rs.exe" -or $task.Actions.Arguments -ne ('serve-claude-split "' + $client + '"')) { throw 'Fixture task does not run native binary directly.' }
  $refused = $false
  try { & (Join-Path $PSScriptRoot 'Connect-ClaudeCode.ps1') -ClientRoot (Join-Path $root 'wrong-client') -ClaudeProfile $claude -TaskName $taskName -Disable | Out-Null } catch { $refused = $true }
  if (-not $refused -or (Get-ScheduledTask -TaskName $taskName).State -ne 'Running') { throw 'Disable did not protect a task belonging to another client.' }
  # Reinstall/upgrade must stop the old native instance gracefully, then pass TLS/models.
  & (Join-Path $PSScriptRoot 'Connect-ClaudeCode.ps1') -Exe $Exe -ClientRoot $client -ClaudeProfile $claude -TaskName $taskName | Out-Null
  & (Join-Path $PSScriptRoot 'Connect-ClaudeCode.ps1') -ClientRoot $client -ClaudeProfile $claude -TaskName $taskName -Disable | Out-Null
  $restored = Get-Content -LiteralPath $settings -Raw | ConvertFrom-Json
  if ($restored.env.ANTHROPIC_BASE_URL -ne 'https://fixture.invalid' -or $restored.permissions.deny[0] -ne 'Read(.env)') { throw 'Native disable did not restore fixture settings.' }
  if ((Get-ScheduledTask -TaskName $taskName).Settings.Enabled) { throw 'Native startup task not disabled.' }
  'PASS: staged installer, legacy RSA CA renewal, DPAPI migration, TLS/models, native singleton, authenticated stop, stable restart, fixture task install/upgrade/disable. No real Claude settings/tasks changed.'
} finally {
  if ($process -and -not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
  $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
  if ($task) {
    if ($task.Actions.Execute -notlike "$client\bin\*\pools-rs.exe") { throw 'Fixture task ownership mismatch; cleanup refused.' }
    Stop-ScheduledTask -TaskName $taskName
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
  }
  Stop-Job $job; Remove-Job $job
  if ([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($root)) -ne [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')) { throw 'Unsafe fixture cleanup path.' }
  Remove-Item -LiteralPath $root -Recurse -Force
}
