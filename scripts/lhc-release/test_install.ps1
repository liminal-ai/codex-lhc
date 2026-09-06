Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$root = Join-Path $env:RUNNER_TEMP "codex-lhc-install-test-$([guid]::NewGuid())"
$release = Join-Path $root "release"
$payload = Join-Path $root "payload"
$prefix = Join-Path $root "prefix"
$store = Join-Path $root "store"
$version = (Get-Content (Join-Path $PSScriptRoot "..\..\lhc-release\VERSION") -Raw).Trim()
$platform = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "windows-aarch64" } else { "windows-x86_64" }
$asset = "codex-lhc-v$version-$platform.zip"

try {
    New-Item (Join-Path $payload "bin") -ItemType Directory -Force | Out-Null
    Copy-Item "$env:SystemRoot\System32\where.exe" (Join-Path $payload "bin\codex.exe")
    Copy-Item "$env:SystemRoot\System32\where.exe" (Join-Path $payload "bin\codex-code-mode-host.exe")
    Set-Content (Join-Path $payload "codex-package.json") "{`"version`":`"$version`",`"lhc`":{`"sdkCommit`":`"fixture`"}}"
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

    # A new install without -Name uses codex-lhc even when a codex command exists,
    # and a rerun without -Name keeps the recorded name.
    $stock = Join-Path $prefix "bin\codex.cmd"
    Set-Content $stock "@echo off`r`nexit /b 0" -Encoding Ascii
    & "$PSScriptRoot\install.ps1" -Version $version -Prefix $prefix -InstallRoot $store -AssetDir $release
    $default = Join-Path $prefix "bin\codex-lhc.cmd"
    if (-not (Test-Path $default)) { throw "default launcher codex-lhc.cmd was not installed" }
    if ((Get-Content $stock -Raw) -notmatch 'exit /b 0') { throw "stock codex.cmd was modified" }
    & "$PSScriptRoot\install.ps1" -Version $version -Prefix $prefix -InstallRoot $store -AssetDir $release
    if ((Get-Content (Join-Path $store "installed-name") -Raw).Trim() -ne "codex-lhc") { throw "rerun changed the recorded name" }
    & "$PSScriptRoot\install.ps1" -Prefix $prefix -InstallRoot $store -Uninstall
    if (Test-Path $default) { throw "default launcher survived uninstall" }
    if (Test-Path $store) { throw "managed store survived uninstall" }
    Write-Host "Windows installer fixture: PASS"
} finally {
    if (Test-Path $root) { Remove-Item $root -Recurse -Force }
}
