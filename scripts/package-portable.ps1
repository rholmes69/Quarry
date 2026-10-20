# Build a portable Quarry bundle for Windows (run from a "Developer PowerShell"
# or any shell with the MSVC build tools on PATH).
#
#   powershell -ExecutionPolicy Bypass -File scripts\package-portable.ps1
#
# Produces dist\Quarry-<version>-windows-x64-portable.zip:
#   Quarry.exe   one file; C runtime linked statically (.cargo/config.toml),
#                SQLite3MC, rustls TLS — runs on a clean Windows 10/11 box
#   QuarryData\  its presence enables portable mode (no %APPDATA% writes)
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
cargo build --release
$name = "Quarry-$version-windows-x64-portable"
$out = "dist\$name"
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force "$out\QuarryData" | Out-Null
Copy-Item target\release\quarry.exe "$out\Quarry.exe"
@"
Quarry $version - portable edition

Double-click Quarry.exe. Nothing to install: no Visual C++ runtime, no
OpenSSL, no ODBC/JDBC drivers. Settings, recent files, connection profiles
and custom collations live in the QuarryData folder next to Quarry.exe, so
the whole folder can travel on a USB stick. Passwords are never stored.
"@ | Set-Content "$out\README-PORTABLE.txt"
Compress-Archive -Force -Path $out -DestinationPath "dist\$name.zip"
Write-Host "built dist\$name.zip"
