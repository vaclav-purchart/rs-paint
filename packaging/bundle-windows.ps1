# Build rs-paint on Windows and package the release .exe into a distribution zip.
# Usage (from a PowerShell prompt):  .\packaging\bundle-windows.ps1
$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Version from Cargo.toml (single source of truth).
$version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"(.*)"').Matches[0].Groups[1].Value

Write-Host "==> Building release binary"
cargo build --release

$exe = "target\release\rs-paint.exe"
if (-not (Test-Path $exe)) { throw "Build did not produce $exe" }

New-Item -ItemType Directory -Force -Path "bundles" | Out-Null
$zip = "bundles\rs-paint-v$version-windows-amd64.zip"
if (Test-Path $zip) { Remove-Item $zip }

Write-Host "==> Packaging distribution zip"
Compress-Archive -Path $exe -DestinationPath $zip

Write-Host "==> Bundle: $root\$zip"
