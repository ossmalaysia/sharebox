# Install a release binary; do not launch it or modify the user's PATH.
param(
    [string]$Version = $(if ($env:VERSION) { $env:VERSION } else { 'latest' }),
    [string]$InstallDir = $(if ($env:SHAREBOX_INSTALL_DIR) { $env:SHAREBOX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Sharebox\bin' })
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ($Version -ne 'latest' -and $Version -notmatch '^v[0-9][A-Za-z0-9._-]*$') {
    throw 'Invalid Version. Use latest or a release tag such as v0.1.0.'
}
$architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($architecture -ne 'AMD64') { throw 'This installer supports Windows x86_64. Build from source for other architectures.' }
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$repo = 'https://github.com/ossmalaysia/sharebox/releases'
$release = if ($Version -eq 'latest') { "$repo/latest/download" } else { "$repo/download/$Version" }
$asset = 'sharebox-x86_64-pc-windows-msvc.zip'
$temp = Join-Path ([IO.Path]::GetTempPath()) ('sharebox-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    Write-Host "Downloading $asset ($Version)..."
    $archive = Join-Path $temp $asset
    $manifest = Join-Path $temp 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$release/$asset" -OutFile $archive
    Invoke-WebRequest -UseBasicParsing -Uri "$release/SHA256SUMS" -OutFile $manifest
    $pattern = '^([a-fA-F0-9]{64})\s+' + [Regex]::Escape($asset) + '$'
    $entries = @(Get-Content $manifest | Where-Object { $_ -match $pattern })
    if ($entries.Count -ne 1) { throw 'Release manifest has no unique checksum for this archive.' }
    if ($entries[0] -notmatch $pattern) { throw 'Invalid checksum entry.' }
    $expected = $Matches[1]
    $actual = (Get-FileHash -Algorithm SHA256 -Path $archive).Hash
    if ($actual -ne $expected) { throw 'Checksum mismatch. Nothing was installed.' }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        $entry = $zip.GetEntry('sharebox.exe')
        if ($null -eq $entry) { throw 'Archive does not contain sharebox.exe.' }
        $binary = Join-Path $temp 'sharebox.exe'
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $binary, $false)
    } finally { $zip.Dispose() }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -LiteralPath $binary -Destination (Join-Path $InstallDir 'sharebox.exe') -Force
    Write-Host "Installed: $(Join-Path $InstallDir 'sharebox.exe')"
    Write-Host "Add $InstallDir to PATH, or invoke the installed path directly."
    Write-Host 'Start locally with: sharebox master --bind 127.0.0.1:7331 --data .sharebox'
} finally {
    Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
}
