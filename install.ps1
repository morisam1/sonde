# Install Sonde on Windows:
#
#   irm https://raw.githubusercontent.com/morisam1/sonde/master/install.ps1 | iex
#
# Settings, as environment variables:
#   $env:SONDE_CHANNEL = "nightly"   install the nightly build instead of the latest release
#   $env:SONDE_VERSION = "0.2.0"     install this release instead of the latest
#
# Sonde goes in %LOCALAPPDATA%\Programs\Sonde, with a Start menu shortcut.
# Nothing needs administrator rights, and the download is checked against
# its published SHA-256 checksum before it is installed.

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$repo = "morisam1/sonde"
$base = if ($env:SONDE_DOWNLOAD_BASE) { $env:SONDE_DOWNLOAD_BASE } else { "https://github.com/$repo/releases" }
$channel = if ($env:SONDE_CHANNEL) { $env:SONDE_CHANNEL } else { "stable" }
$package = "sonde-x86_64-pc-windows-msvc.zip"

if ($env:SONDE_VERSION) {
    $url = "$base/download/v$($env:SONDE_VERSION.TrimStart('v'))"
} elseif ($channel -eq "nightly") {
    $url = "$base/download/nightly"
} elseif ($channel -eq "stable") {
    $url = "$base/latest/download"
} else {
    throw "SONDE_CHANNEL must be stable or nightly, not $channel"
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("sonde-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    Write-Host "sonde: downloading $package"
    $zip = Join-Path $work $package
    Invoke-WebRequest "$url/$package" -OutFile $zip -UseBasicParsing
    $expected = ((Invoke-WebRequest "$url/$package.sha256" -UseBasicParsing).Content -split '\s+')[0]
    if ($expected -is [byte[]]) { $expected = [Text.Encoding]::ASCII.GetString($expected) }
    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected.Trim().ToLower()) {
        throw "the download does not match its checksum; nothing was installed"
    }

    Expand-Archive $zip -DestinationPath $work -Force
    $target = Join-Path $env:LOCALAPPDATA "Programs\Sonde"
    New-Item -ItemType Directory -Path $target -Force | Out-Null
    Copy-Item (Join-Path $work "sonde\*") $target -Recurse -Force

    $exe = Join-Path $target "sonde.exe"
    $shortcut = Join-Path ([Environment]::GetFolderPath("Programs")) "Sonde.lnk"
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($shortcut)
    $link.TargetPath = $exe
    $link.WorkingDirectory = $target
    $link.Save()

    Write-Host "sonde: installed $exe"
    Write-Host "sonde: done. Start Sonde from the Start menu."
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
