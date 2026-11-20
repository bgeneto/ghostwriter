<#
.SYNOPSIS
    Builds every Ghostwriter Windows artifact into dist/.

.DESCRIPTION
    Tauri 2 has no "portable" bundle target. What it has is the raw release
    binary, which is already fully self-contained apart from the system WebView2
    runtime — that *is* the portable build. So this script produces four things:

      Ghostwriter-<version>-x64-portable.exe   the bare binary, no installer
      Ghostwriter-<version>-x64-portable.zip   same binary + README, for sharing
      Ghostwriter-<version>-x64.msi            WiX installer
      Ghostwriter-<version>-x64-setup.exe      NSIS installer

    The portable exe still needs Microsoft Edge WebView2, which ships with
    Windows 11 and is installed on virtually every Windows 10 machine.

.PARAMETER SkipBundle
    Only build the portable exe, skipping the MSI and NSIS installers. Much
    faster and does not require the WiX/VBSCRIPT optional features.

.EXAMPLE
    pwsh -File scripts/dist.ps1
    pwsh -File scripts/dist.ps1 -SkipBundle
#>
[CmdletBinding()]
param(
    [switch]$SkipBundle
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# LLVM/LTO can exhaust the commit limit on machines with a small fixed pagefile.
# Serializing rustc keeps the peak below that limit. CI may override this with
# CARGO_BUILD_JOBS=2, which it sets globally.
if (-not $env:CARGO_BUILD_JOBS) {
    $env:CARGO_BUILD_JOBS = '1'
}

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $conf = Get-Content (Join-Path $root 'src-tauri/tauri.conf.json') -Raw | ConvertFrom-Json
    $version = $conf.version
    $name = $conf.productName
    $dist = Join-Path $root 'dist'
    $release = Join-Path $root 'src-tauri/target/release'
    $exe = Join-Path $release "$name.exe"

    Write-Host "Ghostwriter $version" -ForegroundColor Cyan
    Write-Host ""

    # --- build ------------------------------------------------------------
    # `--no-bundle` produces only the executable, which is what we want for the
    # portable artifact and is also faster than a full bundle run.
    if ($SkipBundle) {
        Write-Host "==> cargo build --release (portable only)" -ForegroundColor Yellow
        cargo build --release --manifest-path (Join-Path $root 'src-tauri/Cargo.toml')
    }
    else {
        Write-Host "==> cargo tauri build (msi + nsis)" -ForegroundColor Yellow
        cargo tauri build --config (Join-Path $root 'src-tauri/tauri.conf.json')
    }

    # A failed build must never be packaged: an old `Ghostwriter.exe` can still
    # be sitting in target/release, and shipping it silently is worse than
    # failing the script.
    if ($LASTEXITCODE -ne 0) {
        throw "the build failed (exit $LASTEXITCODE); refusing to package stale artifacts"
    }

    if (-not (Test-Path $exe)) {
        throw "Build finished but the binary is missing: $exe"
    }

    # --- stage ------------------------------------------------------------
    New-Item -ItemType Directory -Force -Path $dist | Out-Null

    $portableExe = Join-Path $dist "$name-$version-x64-portable.exe"
    Copy-Item $exe $portableExe -Force
    $sizeMb = [math]::Round((Get-Item $portableExe).Length / 1MB, 1)
    Write-Host "    portable exe  $([IO.Path]::GetFileName($portableExe)) ($sizeMb MB)"

    # A zip is friendlier to share than a lone exe, and gives us somewhere to
    # put the usage note.
    $staging = Join-Path $dist "_stage"
    if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $staging | Out-Null
    Copy-Item $portableExe (Join-Path $staging "$name.exe")

    $note = @"
$name $version — portable build
====================================

Double-click $name.exe. Nothing is installed and no registry keys are written.

Ghostwriter types text into whichever window has focus, at 60-300 words per
minute. Paste your text, pick a speed, press Start, then focus the window where
it should appear: typing starts as soon as that window has focus.

Pausar holds the run. Retomar continues from the same character and brings the
destination window back. Cancelar ends the run without clearing the text, so the
next start begins from the beginning.

Shortcuts (work with another app in the foreground):
  Pause / resume   Ctrl+Shift+P
  Cancel           Ctrl+Shift+S

Requirement: Microsoft Edge WebView2. It ships with Windows 11 and is present
on almost all Windows 10 machines. If the window stays blank, install the
runtime once from:
  https://developer.microsoft.com/microsoft-edge/webview2/
"@
    Set-Content -Path (Join-Path $staging 'README.txt') -Value $note -Encoding UTF8

    $zip = Join-Path $dist "$name-$version-x64-portable.zip"
    if (Test-Path $zip) { Remove-Item $zip -Force }
    Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip
    Remove-Item $staging -Recurse -Force
    Write-Host "    portable zip  $([IO.Path]::GetFileName($zip))"

    # --- installers -------------------------------------------------------
    if (-not $SkipBundle) {
        $bundleDir = Join-Path $release 'bundle'
        $targets = @(
            @{ Path = "$bundleDir/msi/*.msi";        Extension = 'msi' },
            @{ Path = "$bundleDir/nsis/*-setup.exe"; Extension = 'setup.exe' }
        )
        foreach ($t in $targets) {
            $found = Get-ChildItem -Path $t.Path -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($found) {
                # The bundler names files with its own convention
                # (Ghostwriter_0.1.0_x64_en-US.msi), so normalise to the release
                # naming used for the portable artifacts.
                $dest = Join-Path $dist "$name-$version-x64.$($t.Extension)"
                Copy-Item $found.FullName $dest -Force
                $mb = [math]::Round($found.Length / 1MB, 1)
                Write-Host "    installer     $([IO.Path]::GetFileName($dest)) ($mb MB)"
            }
            else {
                Write-Warning "no $($t.Extension) produced; check the bundler output above"
            }
        }
    }

    Write-Host ""
    Write-Host "Artifacts in $dist" -ForegroundColor Green
    Get-ChildItem $dist -File | ForEach-Object {
        Write-Host ("  {0,-52} {1,7:N1} MB" -f $_.Name, ($_.Length / 1MB))
    }
}
finally {
    Pop-Location
}
