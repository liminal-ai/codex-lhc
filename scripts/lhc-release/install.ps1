[CmdletBinding()]
param(
    [string]$Version = $env:CODEX_LHC_VERSION,
    [string]$Name = $env:CODEX_LHC_NAME,
    [string]$Prefix = $env:CODEX_LHC_PREFIX,
    [string]$InstallRoot = $env:CODEX_LHC_INSTALL_ROOT,
    [string]$AssetDir = $env:CODEX_LHC_ASSET_DIR,
    [switch]$Uninstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$Repository = if ($env:CODEX_LHC_REPOSITORY) { $env:CODEX_LHC_REPOSITORY } else { "liminal-ai/codex-lhc" }
$defaultPrefix = Join-Path $env:LOCALAPPDATA "CodexLHC"
if (-not $InstallRoot) {
    $InstallRoot = Join-Path $(if ($Prefix) { $Prefix } else { $defaultPrefix }) "packages"
}
# Defaults come from the store's own records, so a rerun that names only the
# store (as the CLI update path does) updates the existing command in place.
# An explicit parameter or environment value still wins.
if (-not $Prefix) {
    $recordedPrefix = Join-Path $InstallRoot "installed-prefix"
    $Prefix = if (Test-Path $recordedPrefix) { (Get-Content $recordedPrefix -Raw).Trim() } else { $defaultPrefix }
}
$BinDir = Join-Path $Prefix "bin"

function Fail([string]$Message) { throw "codex-lhc installer: $Message" }

# Default command name: the name recorded by an existing managed install,
# otherwise codex-lhc. An existing stock codex command is left alone.
if (-not $Name) {
    if (Test-Path (Join-Path $InstallRoot "installed-name")) {
        $Name = (Get-Content (Join-Path $InstallRoot "installed-name") -Raw).Trim()
    } else {
        $Name = "codex-lhc"
    }
}
if ($Name -notmatch '^[A-Za-z0-9._-]+$') { Fail "invalid command name: $Name" }
$Launcher = Join-Path $BinDir "$Name.cmd"

if ($Uninstall) {
    if (Test-Path $Launcher) {
        $text = Get-Content $Launcher -Raw
        if ($text -notmatch [regex]::Escape($InstallRoot)) { Fail "$Launcher is not managed by this installer" }
        Remove-Item $Launcher -Force
    }
    $marker = Join-Path $InstallRoot ".codex-lhc-managed"
    if ((Test-Path $InstallRoot) -and -not (Test-Path $marker)) { Fail "$InstallRoot is not installer-managed" }
    if (Test-Path $InstallRoot) { Remove-Item $InstallRoot -Recurse -Force }
    Write-Host "Removed Codex + LHC command '$Name'. User configuration and LHC archives were preserved."
    exit 0
}

if (-not $Version) {
    $metadata = Invoke-RestMethod "https://api.github.com/repos/$Repository/releases/latest"
    $Version = ([string]$metadata.tag_name).TrimStart('v')
}
$Version = $Version.TrimStart('v')
if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$') { Fail "invalid version: $Version" }

$platform = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "windows-aarch64" } else { "windows-x86_64" }
$asset = "codex-lhc-v$Version-$platform.zip"
$temp = Join-Path ([System.IO.Path]::GetTempPath()) "codex-lhc-$([guid]::NewGuid())"
New-Item $temp -ItemType Directory | Out-Null
try {
    $archive = Join-Path $temp $asset
    $checksums = Join-Path $temp "SHA256SUMS"
    if ($AssetDir) {
        Copy-Item (Join-Path $AssetDir $asset) $archive
        Copy-Item (Join-Path $AssetDir "SHA256SUMS") $checksums
    } else {
        $base = "https://github.com/$Repository/releases/download/v$Version"
        Invoke-WebRequest "$base/$asset" -OutFile $archive
        Invoke-WebRequest "$base/SHA256SUMS" -OutFile $checksums
    }
    $line = Get-Content $checksums | Where-Object { $_ -match "\s+$([regex]::Escape($asset))$" } | Select-Object -First 1
    if (-not $line) { Fail "SHA256SUMS does not list $asset" }
    $expected = ($line -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { Fail "checksum mismatch for $asset" }

    New-Item (Join-Path $InstallRoot "versions") -ItemType Directory -Force | Out-Null
    New-Item $BinDir -ItemType Directory -Force | Out-Null
    Set-Content (Join-Path $InstallRoot ".codex-lhc-managed") "managed by codex-lhc install.ps1"
    $destination = Join-Path (Join-Path $InstallRoot "versions") $Version
    $stage = "$destination.tmp.$PID"
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    Expand-Archive $archive $stage
    if (-not (Test-Path (Join-Path $stage "bin\codex.exe"))) { Fail "archive is missing bin\codex.exe" }
    if (-not (Test-Path (Join-Path $stage "bin\codex-code-mode-host.exe"))) { Fail "archive is missing bin\codex-code-mode-host.exe" }
    if (-not (Test-Path (Join-Path $stage "codex-package.json"))) { Fail "archive is missing codex-package.json" }
    if (Test-Path $destination) { Remove-Item $destination -Recurse -Force }
    Move-Item $stage $destination

    $escaped = $destination.Replace('%', '%%')
    Set-Content $Launcher "@echo off`r`n`"$escaped\bin\codex.exe`" %*" -Encoding Ascii
    Set-Content (Join-Path $InstallRoot "installed-version") $Version
    Set-Content (Join-Path $InstallRoot "installed-name") $Name
    Set-Content (Join-Path $InstallRoot "installed-prefix") $Prefix
    Write-Host "Installed Codex + LHC v$Version"
    Write-Host "Command: $Launcher"
} finally {
    if (Test-Path $temp) { Remove-Item $temp -Recurse -Force }
}
