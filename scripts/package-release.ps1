# Build a self-contained release folder: iptv-player.exe + libmpv-2.dll +
# config.example.toml (reference only: the app creates config.toml itself on
# first start and asks for the playlist URL). Usage (from anywhere):
#   powershell -ExecutionPolicy Bypass -File scripts\package-release.ps1 [-Out <dir>]
# Default output: <repo>\dist\iptv-player (gitignored), plus the distributable
# <repo>\dist\iptv-player.zip (exe + DLL + LICENSE; upload this to GitHub releases).
param(
    [string]$Out = ""
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
if ($Out -eq "") { $Out = Join-Path $repo "dist\iptv-player" }

$dll = Join-Path $repo "libmpv\libmpv-2.dll"
if (-not (Test-Path $dll)) {
    throw "libmpv\libmpv-2.dll not found (see README.md, 'libmpv')."
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
}
cargo build --release -p iptv-player --manifest-path (Join-Path $repo "Cargo.toml")
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

New-Item -ItemType Directory -Force $Out | Out-Null
Copy-Item (Join-Path $repo "target\release\iptv-player.exe") $Out -Force
Copy-Item $dll $Out -Force
Copy-Item (Join-Path $repo "config.example.toml") $Out -Force
Copy-Item (Join-Path $repo "LICENSE") $Out -Force
# The distributable: the exe, the DLL and the license, next to the release folder.
$zip = Join-Path (Split-Path -Parent $Out) "iptv-player.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $Out "iptv-player.exe"), (Join-Path $Out "libmpv-2.dll"), (Join-Path $Out "LICENSE") -DestinationPath $zip
Write-Host "Release folder: $Out"
Write-Host "Zip: $zip"
Write-Host "Next: start iptv-player.exe there; it creates config.toml and asks for the playlist URL."
