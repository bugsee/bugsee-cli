# Bugsee CLI installer (Windows / PowerShell).
#
#   powershell -ExecutionPolicy ByPass -c "irm https://download.bugsee.com/cli/install.ps1 | iex"
#
# Downloads the published bugsee-cli binary for this host from
# download.bugsee.com, SHA-256-verifies it against the published checksum, and
# installs it. No GitHub dependency.
#
# Environment overrides:
#   BUGSEE_CLI_VERSION       pin an exact X.Y.Z (default: the latest release)
#   BUGSEE_CLI_INSTALL_DIR   install directory (default: %LOCALAPPDATA%\Bugsee\bin)
#   BUGSEE_CLI_BASE_URL      download root (default: https://download.bugsee.com/cli)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Base = if ($env:BUGSEE_CLI_BASE_URL) { $env:BUGSEE_CLI_BASE_URL } else { 'https://download.bugsee.com/cli' }
$Version = $env:BUGSEE_CLI_VERSION
$InstallDir = $env:BUGSEE_CLI_INSTALL_DIR

# Only x86_64 Windows is published.
$procArch = $env:PROCESSOR_ARCHITECTURE
if ($procArch -ne 'AMD64') {
    throw "bugsee-cli install error: unsupported Windows architecture '$procArch' (only x86_64/AMD64 is published)"
}
$triple = 'x86_64-pc-windows-msvc'

if (-not $Version) {
    $Version = (Invoke-RestMethod -Uri "$Base/latest/version.txt").Trim()
}
if (-not $Version) { throw "bugsee-cli install error: could not determine a version to install" }
# Path-traversal guard: the version is interpolated into the download URL.
if ($Version -notmatch '^[0-9A-Za-z.+-]+$') {
    throw "bugsee-cli install error: unexpected version string '$Version'"
}

$art = "bugsee-cli-$triple.zip"
$url = "$Base/v$Version/$art"
$tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP ("bugsee-cli-" + [System.Guid]::NewGuid().ToString()))
try {
    Write-Host "Downloading bugsee-cli $Version ($triple)..."
    $zip = Join-Path $tmp $art
    Invoke-WebRequest -Uri $url -OutFile $zip

    $expected = (((Invoke-RestMethod -Uri "$url.sha256") -split '\s+')[0]).ToLower()
    $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) {
        throw "bugsee-cli install error: checksum mismatch (expected $expected, got $actual)"
    }

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $exe = Get-ChildItem -Path $tmp -Recurse -Filter 'bugsee-cli.exe' | Select-Object -First 1
    if (-not $exe) { throw "bugsee-cli install error: bugsee-cli.exe not found after extraction" }

    if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'Bugsee\bin' }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Path $exe.FullName -Destination (Join-Path $InstallDir 'bugsee-cli.exe') -Force

    Write-Host "Installed bugsee-cli $Version -> $InstallDir\bugsee-cli.exe"
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $InstallDir) {
        Write-Host ""
        Write-Host "  $InstallDir is not on your PATH. Add it for new shells with:"
        Write-Host "    setx PATH `"$InstallDir;`$env:PATH`""
    }
    & (Join-Path $InstallDir 'bugsee-cli.exe') --version
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
