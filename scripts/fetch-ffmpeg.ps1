# Downloads a pinned FFmpeg "essentials" build from gyan.dev (includes NVENC, libx264 and
# AAC) and places it in bin/, where the app finds it during development and where
# scripts/package.ps1 picks it up for the release zip.
#
# The version is pinned so every build ships the FFmpeg the tests ran against. To upgrade:
# run with -Version <x.y.z> (or -Version latest), run `cargo test`, then update $PinnedVersion.
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts/fetch-ffmpeg.ps1 [-Force] [-Version 9.0.2|latest]

param([switch]$Force, [string]$Version = '')

$PinnedVersion = '9.0.2'
if (-not $Version) { $Version = $PinnedVersion }

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$root = Split-Path -Parent $PSScriptRoot
$binDir = Join-Path $root 'bin'
$target = Join-Path $binDir 'ffmpeg.exe'
$licenseTarget = Join-Path $binDir 'ffmpeg-LICENSE.txt'
$baseUrl = 'https://www.gyan.dev/ffmpeg/builds'

if ((Test-Path $target) -and -not $Force) {
    Write-Host "FFmpeg already present at $target (use -Force to re-download)."
    & $target -hide_banner -version | Select-Object -First 1
    exit 0
}

if ($Version -eq 'latest') {
    $Version = (Invoke-RestMethod "$baseUrl/release-version").Trim()
}
$version = $Version
$zipUrl = "$baseUrl/packages/ffmpeg-$version-essentials_build.zip"
$expectedHash = (Invoke-RestMethod "$zipUrl.sha256").Trim().Split()[0].ToLower()
Write-Host "Downloading FFmpeg $version essentials build..."

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("batchviproc-ffmpeg-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $work | Out-Null
try {
    $zip = Join-Path $work 'ffmpeg.zip'
    Invoke-WebRequest $zipUrl -OutFile $zip

    $actualHash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if ($actualHash -ne $expectedHash) {
        throw "Checksum mismatch: expected $expectedHash, got $actualHash"
    }

    $extract = Join-Path $work 'extract'
    Expand-Archive $zip -DestinationPath $extract
    $exe = Get-ChildItem $extract -Recurse -Filter 'ffmpeg.exe' | Select-Object -First 1
    $license = Get-ChildItem $extract -Recurse -Filter 'LICENSE*' | Select-Object -First 1
    if (-not $exe) { throw 'ffmpeg.exe not found in the archive' }

    New-Item -ItemType Directory -Force $binDir | Out-Null
    Copy-Item $exe.FullName $target -Force
    if ($license) { Copy-Item $license.FullName $licenseTarget -Force }
    else { Set-Content $licenseTarget "FFmpeg $version essentials build from $baseUrl (GPLv3)." }

    $encoders = & $target -hide_banner -encoders
    foreach ($name in 'av1_nvenc', 'hevc_nvenc', 'h264_nvenc', 'libx264', 'aac') {
        if (-not ($encoders | Select-String -Pattern "\s$name\s" -Quiet)) {
            throw "Downloaded FFmpeg is missing the $name encoder"
        }
    }

    Write-Host "Installed FFmpeg $version to $target"
}
finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
