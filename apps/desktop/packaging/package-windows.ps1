# Package river.exe as River-<version>-windows-x86_64.zip.
#
#   pwsh package-windows.ps1 -Exe <river.exe> -Version <version> -OutDir <dir> [-Suffix <text>]
#
# Checks that the exe carries the version resource build.rs embeds, with the version being
# released, and that it reports that version through a pipe (as the self-update reads it), then
# zips it with the licence and a README. Signing happens before this, in
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

# The self-update runs a new river.exe with --version and reads its stdout through a pipe
# before installing it (apps/desktop/src/update/unpack.rs). A release exe uses the GUI
# subsystem, so it prints nothing to a console; it must still answer through a pipe, or every
# Windows install would refuse the update.
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = (Resolve-Path $Exe).Path
$psi.Arguments = '--version'
$psi.RedirectStandardOutput = $true
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$proc = [System.Diagnostics.Process]::Start($psi)
$said = $proc.StandardOutput.ReadToEnd().Trim()
if (-not $proc.WaitForExit(20000)) { $proc.Kill(); throw "$Exe --version did not finish" }
if ($said -ne "river $Version") { throw "$Exe --version said '$said' through a pipe, expected 'river $Version'" }
Write-Host "--version through a pipe: $said"

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

New users do not need River in the browser first: this preview sets up River's
storage on your Freenet node itself. The first time, the app asks Freenet for
permission: press the button in the app to open a page in your browser, on your
Freenet node's own address (not River), and choose Allow there.
If you already used River in your web browser, rooms from an earlier version show
up here after you open River in the browser once.

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
