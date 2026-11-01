<#
.SYNOPSIS
Points the Codex desktop at the pool's backend relay, or back at chatgpt.com.

.DESCRIPTION
-Enable sets both readers the desktop uses (see README, "Desktop usage banner")
to https://localhost:<desktopRelayPort>/backend-api: the app-server's
chatgpt_base_url in ~/.codex/config.toml and the Electron process's
CODEX_API_BASE_URL user environment variable. Codex 0.155 accepts only an https
origin for chatgpt_base_url (workspace routing discovery) and Electron then
routes its own backend calls to that origin, attaching the ChatGPT token only
for host localhost:8000, so the relay must speak TLS on that port. -Enable
therefore also provisions the certificate: a throwaway local CA and a leaf for
localhost / 127.0.0.1 / ::1 in state/certs (5 years), with the CA imported into
the current user's Trusted Root store (Windows asks once, in a dialog). The
service reads the files at start; when they were (re)created this script
restarts it (deploy\Restart-Pool-Rs.ps1 -Step service).
-Disable removes both settings and leaves the certificate in place.
Either way the desktop must be restarted; both values are read at launch.
Ports come from state/install.json (desktopRelayPort). -Root is the install
(default: the live one).
#>
[CmdletBinding()]
param(
  [Parameter(ParameterSetName = 'Enable', Mandatory)][switch]$Enable,
  [Parameter(ParameterSetName = 'Disable', Mandatory)][switch]$Disable,
  [string]$ConfigPath = (Join-Path $HOME '.codex\config.toml'),
  # -Enable only: skip the service restart after (re)creating the certificate.
  [switch]$NoRestart,
  [string]$Root
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\deploy\common.ps1')
if (-not $Root) { $Root = $script:LiveRoot }
$Root = (Resolve-Path -LiteralPath $Root).ProviderPath.TrimEnd('\')
$install = Get-Content -LiteralPath (Join-Path $Root 'state/install.json') -Raw | ConvertFrom-Json
$relayPort = if ($install.desktopRelayPort) { $install.desktopRelayPort } else { 8000 }
$relayBase = "https://localhost:$relayPort/backend-api"
$certDir = Join-Path $Root 'state\certs'
$caSubject = 'CN=Agent Pools desktop relay CA'
$leafSubject = 'CN=localhost'

function ConvertTo-Pem([string]$label, [byte[]]$der) {
  $body = [Convert]::ToBase64String($der, [Base64FormattingOptions]::InsertLineBreaks) -replace "`r`n", "`n"
  return "-----BEGIN $label-----`n$body`n-----END $label-----`n"
}
function Read-PemCertificate([string]$path) {
  $base64 = ((Get-Content -LiteralPath $path -Raw) -replace '-----[^-]+-----', '') -replace '\s', ''
  return New-Object Security.Cryptography.X509Certificates.X509Certificate2 -ArgumentList (, [Convert]::FromBase64String($base64))
}
function Test-RelayCertificateFiles {
  $leafPath = Join-Path $certDir 'desktop-relay.crt'
  foreach ($name in 'desktop-relay.crt', 'desktop-relay.key', 'desktop-relay-ca.cer') {
    if (-not (Test-Path -LiteralPath (Join-Path $certDir $name))) { return $false }
  }
  try { $leaf = Read-PemCertificate $leafPath } catch { return $false }
  # Renew well before expiry so the desktop never meets an expired leaf.
  return $leaf.NotAfter -gt (Get-Date).AddDays(30)
}
# Creates the CA and leaf in the user's personal store (New-SelfSignedCertificate
# has no file output), exports them as PEM files the service's relay reads, then removes
# both from the store including their keys: the files are the only copy.
function New-RelayCertificateFiles {
  New-Item -ItemType Directory -Force -Path $certDir | Out-Null
  $notAfter = (Get-Date).AddYears(5)
  $ca = New-SelfSignedCertificate -Subject $caSubject -FriendlyName 'Agent Pools desktop relay CA' -Type Custom `
    -KeyAlgorithm RSA -KeyLength 2048 -HashAlgorithm SHA256 -KeyUsage CertSign, CRLSign, DigitalSignature `
    -TextExtension @('2.5.29.19={critical}{text}ca=true&pathlength=0') -NotAfter $notAfter -CertStoreLocation Cert:\CurrentUser\My
  try {
    $leaf = New-SelfSignedCertificate -Subject $leafSubject -FriendlyName 'Agent Pools desktop relay' -Signer $ca -Type Custom `
      -KeyAlgorithm RSA -KeyLength 2048 -HashAlgorithm SHA256 -KeyExportPolicy Exportable -KeyUsage DigitalSignature, KeyEncipherment `
      -TextExtension @('2.5.29.17={text}DNS=localhost&IPAddress=127.0.0.1&IPAddress=::1', '2.5.29.37={text}1.3.6.1.5.5.7.3.1', '2.5.29.19={text}ca=false') `
      -NotAfter $notAfter -CertStoreLocation Cert:\CurrentUser\My
    try {
      $rsa = [Security.Cryptography.X509Certificates.RSACertificateExtensions]::GetRSAPrivateKey($leaf)
      $pkcs8 = $rsa.Key.Export([Security.Cryptography.CngKeyBlobFormat]::Pkcs8PrivateBlob)
      [IO.File]::WriteAllText((Join-Path $certDir 'desktop-relay.key'), (ConvertTo-Pem 'PRIVATE KEY' $pkcs8))
      [IO.File]::WriteAllText((Join-Path $certDir 'desktop-relay.crt'), (ConvertTo-Pem 'CERTIFICATE' $leaf.RawData))
      [IO.File]::WriteAllBytes((Join-Path $certDir 'desktop-relay-ca.cer'), $ca.RawData)
      [IO.File]::WriteAllText((Join-Path $certDir 'desktop-relay-ca.pem'), (ConvertTo-Pem 'CERTIFICATE' $ca.RawData))
    } finally {
      Remove-Item -LiteralPath "Cert:\CurrentUser\My\$($leaf.Thumbprint)" -DeleteKey
    }
  } finally {
    Remove-Item -LiteralPath "Cert:\CurrentUser\My\$($ca.Thumbprint)" -DeleteKey
  }
}
# Trusts the CA from the file for the current user (Windows shows a confirmation
# dialog the first time) and drops stale CAs of the same name from that store.
function Confirm-RelayCaTrusted {
  $caPath = Join-Path $certDir 'desktop-relay-ca.cer'
  $ca = New-Object Security.Cryptography.X509Certificates.X509Certificate2 -ArgumentList $caPath
  $stale = @(Get-ChildItem Cert:\CurrentUser\Root | Where-Object { $_.Subject -eq $caSubject -and $_.Thumbprint -ne $ca.Thumbprint })
  foreach ($old in $stale) { Remove-Item -LiteralPath "Cert:\CurrentUser\Root\$($old.Thumbprint)" }
  if (Get-ChildItem Cert:\CurrentUser\Root | Where-Object Thumbprint -eq $ca.Thumbprint) { return $false }
  Write-Host "Trusting the relay CA for the current user (thumbprint $($ca.Thumbprint)); accept the Windows security warning."
  Import-Certificate -FilePath $caPath -CertStoreLocation Cert:\CurrentUser\Root | Out-Null
  return $true
}

$certCreated = $false
if ($Enable) {
  if (-not (Test-RelayCertificateFiles)) {
    New-RelayCertificateFiles
    $certCreated = $true
    Write-Output "Created the relay certificate in $certDir (leaf for localhost, valid 5 years)."
  }
  # Only trust a CA we can pair with a leaf: an untrusted CA is harmless, an
  # untrusted-but-used one just fails the desktop, but never the other way round.
  Confirm-RelayCaTrusted | Out-Null
}

$block = @(
  '# Agent Pools desktop relay (Set-Desktop-Relay.ps1); only /wham/usage is rewritten. https: Codex 0.155 rejects http here.',
  "chatgpt_base_url = `"$relayBase/`""
)
$text = Get-Content -LiteralPath $ConfigPath -Raw
$newline = if ($text.Contains("`r`n")) { "`r`n" } else { "`n" }
$lines = @($text -split "`r?`n" | Where-Object { $_ -notmatch '^\s*chatgpt_base_url\s*=' -and $_ -notmatch '^# Agent Pools desktop relay' })
if ($Enable) {
  # Top-level keys must precede the first table header.
  $firstTable = [Array]::FindIndex($lines, [Predicate[string]] { param($l) $l -match '^\s*\[' })
  if ($firstTable -lt 0) { $firstTable = $lines.Count }
  $head = if ($firstTable -gt 0) { $lines[0..($firstTable - 1)] } else { @() }
  $tail = if ($firstTable -lt $lines.Count) { $lines[$firstTable..($lines.Count - 1)] } else { @() }
  $lines = @($head) + $block + @($tail)
}
[IO.File]::WriteAllText($ConfigPath, ($lines -join $newline))
[Environment]::SetEnvironmentVariable('CODEX_API_BASE_URL', $(if ($Enable) { $relayBase } else { $null }), 'User')
if ($Enable) {
  Write-Output "Desktop relay enabled: config.toml chatgpt_base_url and CODEX_API_BASE_URL -> $relayBase. Restart the Codex desktop."
  if ($certCreated) {
    if ($NoRestart) {
      Write-Output 'The service must be restarted to serve the new certificate (deploy\Restart-Pool-Rs.ps1 -Step service).'
    } else {
      Write-Output 'Restarting the service so the relay serves the new certificate.'
      $restart = @{ Root = $Root; Step = 'service' }
      if ($Root -ieq $script:LiveRoot.TrimEnd('\')) { $restart.Live = $true }
      & (Join-Path $PSScriptRoot '..\deploy\Restart-Pool-Rs.ps1') @restart
    }
  }
} else {
  Write-Output 'Desktop relay disabled: chatgpt_base_url removed, CODEX_API_BASE_URL cleared. Restart the Codex desktop.'
  Write-Output "The relay certificate stays in $certDir and the CA in Cert:\CurrentUser\Root ($caSubject); remove them by hand if no longer wanted."
}
