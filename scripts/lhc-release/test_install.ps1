Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$root = Join-Path $env:RUNNER_TEMP "codex-lhc-install-test-$([guid]::NewGuid())"
$release = Join-Path $root "release"
$payload = Join-Path $root "payload"
$prefix = Join-Path $root "prefix"
$store = Join-Path $root "store"
$version = (Get-Content (Join-Path $PSScriptRoot "..\..\lhc-release\VERSION") -Raw).Trim()
$asset = "codex-lhc-v$version-windows-x86_64.zip"

try {
    New-Item (Join-Path $payload "bin") -ItemType Directory -Force | Out-Null
    Copy-Item "$env:SystemRoot\System32\where.exe" (Join-Path $payload "bin\codex.exe")
    Copy-Item "$env:SystemRoot\System32\where.exe" (Join-Path $payload "bin\codex-code-mode-host.exe")
    Set-Content (Join-Path $payload "release-manifest.json") "{`"release`":`"$version`",`"lhcSdkCommit`":`"fixture`"}"
    New-Item $release -ItemType Directory -Force | Out-Null
    Compress-Archive (Join-Path $payload "*") (Join-Path $release $asset)
    $digest = (Get-FileHash (Join-Path $release $asset) -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content (Join-Path $release "SHA256SUMS") "$digest  $asset"

    & "$PSScriptRoot\install.ps1" -Version $version -Name codex-lhc-test -Prefix $prefix -InstallRoot $store -AssetDir $release
    $launcher = Join-Path $prefix "bin\codex-lhc-test.cmd"
    if (-not (Test-Path $launcher)) { throw "launcher was not installed" }
    & $launcher /?
    if ($LASTEXITCODE) { throw "installed executable failed: $LASTEXITCODE" }

    & "$PSScriptRoot\install.ps1" -Name codex-lhc-test -Prefix $prefix -InstallRoot $store -Uninstall
    if (Test-Path $launcher) { throw "launcher survived uninstall" }
    if (Test-Path $store) { throw "managed store survived uninstall" }
    Write-Host "Windows installer fixture: PASS"
} finally {
    if (Test-Path $root) { Remove-Item $root -Recurse -Force }
}
