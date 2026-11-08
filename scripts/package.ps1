# Builds the release app and packages it as a portable zip with ffmpeg:
#   dist/batchviproc-<version>-win64.zip
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts/package.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$ffmpeg = Join-Path $root 'bin\ffmpeg.exe'
if (-not (Test-Path $ffmpeg)) {
    & (Join-Path $PSScriptRoot 'fetch-ffmpeg.ps1')
}

cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$name = "batchviproc-$version-win64"
$stage = Join-Path $root "dist\$name"
$zip = Join-Path $root "dist\$name.zip"

Remove-Item $stage, $zip -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item (Join-Path $root 'target\release\batchviproc.exe') $stage
Copy-Item $ffmpeg $stage
Copy-Item (Join-Path $root 'bin\ffmpeg-LICENSE.txt') $stage -ErrorAction SilentlyContinue
Copy-Item (Join-Path $root 'LICENSE') $stage

Compress-Archive -Path "$stage\*" -DestinationPath $zip
Remove-Item $stage -Recurse -Force
$sizeMb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host "Created $zip ($sizeMb MB)"
