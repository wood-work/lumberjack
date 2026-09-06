<#
.SYNOPSIS
Build the Windows release zip.

.DESCRIPTION
Builds the two programs, gathers them with the documents that go out beside
them, and writes a zip named for the version in Cargo.toml.

Assembled by hand for 0.1.0 and 0.1.1, which is how a file gets left out of the
next one. The checks below are each something that has already gone wrong or
came close to it: a version typed rather than read, a zip built from a tree
with uncommitted work in it, and an interface shipped as a console application
so that Windows gave it an empty black window.

.EXAMPLE
./packaging/build-release.ps1
#>

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
Push-Location $repo
try {
    # Read rather than passed in. A version given on the command line is one
    # that can disagree with the binaries it names.
    $manifest = Get-Content 'lumbergui/Cargo.toml'
    $version = ($manifest | Select-String '^version = "(.+)"').Matches[0].Groups[1].Value
    Write-Host "lumberjack $version"

    $dirty = git status --porcelain
    if ($dirty) {
        Write-Warning "The working tree has uncommitted changes. A zip built from one cannot be rebuilt from its tag."
        $dirty | ForEach-Object { Write-Host "  $_" }
    }

    Write-Host "`nBuilding..."
    cargo build --release -p lumbergui -p choptui
    if ($LASTEXITCODE -ne 0) { throw "the build failed" }

    # The PE optional header records which subsystem Windows should start the
    # program in: 2 is a windowed program, 3 is a console one. An interface
    # built as 3 comes up with an empty console beside it.
    function Get-Subsystem($path) {
        $bytes = [System.IO.File]::ReadAllBytes($path)[0..4095]
        $pe = [BitConverter]::ToInt32($bytes, 0x3c)
        return [BitConverter]::ToUInt16($bytes, $pe + 24 + 68)
    }

    $gui = Get-Subsystem "target/release/lumbergui.exe"
    if ($gui -ne 2) { throw "lumbergui is subsystem $gui; it should be 2, or it will open a console window" }
    $tui = Get-Subsystem "target/release/choptui.exe"
    if ($tui -ne 3) { throw "choptui is subsystem $tui; it should be 3, being a terminal interface" }
    Write-Host "  subsystems: lumbergui $gui (windowed), choptui $tui (console)"

    # Everything that goes out, named in one place so nothing is left behind by
    # being remembered rather than listed.
    $contents = @(
        'target/release/lumbergui.exe',
        'target/release/choptui.exe',
        'README.md',
        'LICENSE',
        'packaging/RUNNING.txt'
    )

    $staging = "target/package/lumberjack-$version"
    if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
    New-Item -ItemType Directory -Path $staging -Force | Out-Null
    foreach ($item in $contents) {
        if (-not (Test-Path $item)) { throw "missing from the package: $item" }
        Copy-Item $item $staging
    }

    $zip = "target/package/lumberjack-$version-windows-x86_64.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    Compress-Archive -Path "$staging/*" -DestinationPath $zip -CompressionLevel Optimal

    # Read back rather than trusted. The point of the list above is that what
    # ships is what it names.
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead((Resolve-Path $zip))
    $inside = $archive.Entries | ForEach-Object { $_.Name }
    $archive.Dispose()
    foreach ($item in $contents) {
        $name = Split-Path $item -Leaf
        if ($inside -notcontains $name) { throw "$name did not make it into the zip" }
    }

    $built = Get-Item $zip
    Write-Host "`n$($built.FullName)"
    Write-Host ("  {0:N1} MB, {1} files" -f ($built.Length / 1MB), $inside.Count)
    Write-Host "  sha256 $((Get-FileHash $zip -Algorithm SHA256).Hash)"
    Write-Host "`nAttach it to a release on the v$version tag."
}
finally {
    Pop-Location
}
