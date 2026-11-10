# Package river.exe as River-<version>-windows-x86_64.zip.
#
#   pwsh package-windows.ps1 -Exe <river.exe> -Version <version> -OutDir <dir> [-Suffix <text>]
#
# Checks that the exe carries the version resource build.rs embeds, with the version being
# released, then zips it with the licence and a README. Signing happens before this, in
# .github/workflows/release-desktop.yml, so the zip holds the signed exe.

param(
    [Parameter(Mandatory)] [string] $Exe,
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [string] $OutDir,
    [string] $Suffix = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..\..')

$info = (Get-Item $Exe).VersionInfo
Write-Host "ProductName=$($info.ProductName) ProductVersion=$($info.ProductVersion) FileVersion=$($info.FileVersion) CompanyName=$($info.CompanyName)"
if ($info.ProductName -ne 'River') { throw "$Exe has ProductName '$($info.ProductName)', expected 'River' (is the build.rs version resource missing?)" }
if ($info.ProductVersion -ne $Version) { throw "$Exe has ProductVersion '$($info.ProductVersion)', expected '$Version'" }

$name = "River-$Version-windows-x86_64$Suffix"
$stage = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
$dir = Join-Path $stage 'River'
New-Item -ItemType Directory -Path $dir | Out-Null
Copy-Item $Exe (Join-Path $dir 'river.exe')
Copy-Item (Join-Path $repoRoot 'LICENSE') (Join-Path $dir 'LICENSE.txt')
@"
River $Version for Windows (x86_64)

Run river.exe to start. River finds the Freenet node on this computer, or offers to
install it.

Before first use: this preview cannot set up River's storage on your Freenet node
by itself. If you have never used River on this computer, open River in your web
browser through Freenet once first, then start this app.

Source: https://github.com/freenet/river
"@ | Set-Content -Path (Join-Path $dir 'README.txt') -Encoding utf8

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$zip = Join-Path (Resolve-Path $OutDir) "$name.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $dir -DestinationPath $zip
Remove-Item -Recurse -Force $stage
Write-Host $zip
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
try {
    $archive.Entries | ForEach-Object { Write-Host ("{0,12}  {1}" -f $_.Length, $_.FullName) }
} finally {
    $archive.Dispose()
}
